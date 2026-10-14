//! A value that is loaded on demand, dropped after an idle time, and reloaded in the
//! background when a caller that must not wait asks for it.
//!
//! Loads are serialised (`load_lock`), so a background reload and a caller that waits for the
//! value never build two copies at once (the embedding model is the daemon's largest
//! allocation), and no caller waits for a load while holding the value's lock: a query that
//! finds the value missing returns at once while a load is in progress.

use parking_lot::{Mutex, MutexGuard};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

pub(crate) type Loader<T> = Box<dyn Fn() -> alice_core::Result<T> + Send + Sync>;
/// Starts a background job (a named thread in production; tests inject failures).
pub(crate) type Spawner =
    Box<dyn Fn(Box<dyn FnOnce() + Send>) -> std::io::Result<()> + Send + Sync>;

pub(crate) struct Lazy<T: Send + 'static> {
    slot: Mutex<Option<T>>,
    /// Held for the whole of every load.
    load_lock: Mutex<()>,
    /// A background load is queued or running.
    loading: AtomicBool,
    started: Instant,
    last_used_ms: AtomicU64,
    /// `None`: never unloaded (a zero duration counts as `None`).
    idle: Option<Duration>,
    load: Loader<T>,
    spawn: Spawner,
    /// Why the last background load did not happen (cleared by the next successful load).
    last_error: Mutex<Option<String>>,
}

fn thread_spawner() -> Spawner {
    Box::new(|job| {
        std::thread::Builder::new()
            .name("alice-model-load".into())
            .spawn(job)
            .map(|_| ())
    })
}

impl<T: Send + 'static> Lazy<T> {
    /// Loads now; with `idle` set, a reaper thread drops the value after that long unused.
    pub(crate) fn new(load: Loader<T>, idle: Option<Duration>) -> alice_core::Result<Arc<Self>> {
        Self::with_spawner(load, idle, thread_spawner())
    }

    pub(crate) fn with_spawner(
        load: Loader<T>,
        idle: Option<Duration>,
        spawn: Spawner,
    ) -> alice_core::Result<Arc<Self>> {
        let idle = idle.filter(|d| !d.is_zero());
        let first = load()?;
        let me = Arc::new(Self {
            slot: Mutex::new(Some(first)),
            load_lock: Mutex::new(()),
            loading: AtomicBool::new(false),
            started: Instant::now(),
            last_used_ms: AtomicU64::new(0),
            idle,
            load,
            spawn,
            last_error: Mutex::new(None),
        });
        if let Some(d) = idle {
            let weak = Arc::downgrade(&me);
            std::thread::Builder::new()
                .name("alice-model-idle".into())
                .spawn(move || reaper(weak, d))
                .map_err(alice_core::AliceError::Io)?;
        }
        Ok(me)
    }

    fn touch(&self) {
        self.last_used_ms
            .store(self.started.elapsed().as_millis() as u64, Ordering::Relaxed);
    }

    pub(crate) fn is_loaded(&self) -> bool {
        self.slot.lock().is_some()
    }

    /// Why the model is not loaded although a query asked for it, if a load failed.
    pub(crate) fn last_error(&self) -> Option<String> {
        self.last_error.lock().clone()
    }

    #[cfg(test)]
    fn unload(&self) {
        *self.slot.lock() = None;
    }

    /// Load the value unless it is there (or another load put it there meanwhile). Holds the
    /// load lock, never the slot lock, while loading. The value is marked used before it is
    /// stored, so the reaper cannot drop it before the caller gets to it.
    fn load_now(&self) -> alice_core::Result<()> {
        let _serial = self.load_lock.lock();
        if self.slot.lock().is_some() {
            return Ok(());
        }
        let v = (self.load)()?;
        self.touch();
        let mut g = self.slot.lock();
        if g.is_none() {
            *g = Some(v);
        }
        drop(g);
        *self.last_error.lock() = None;
        Ok(())
    }

    /// Run `f` on the value, loading it first if needed (blocks). The lock is handed over
    /// fairly afterwards, so a waiting caller goes next.
    pub(crate) fn with_blocking<R>(&self, f: impl FnOnce(&mut T) -> R) -> alice_core::Result<R> {
        loop {
            let mut guard = self.slot.lock();
            if let Some(v) = guard.as_mut() {
                let r = f(v);
                self.touch();
                MutexGuard::unlock_fair(guard);
                return Ok(r);
            }
            drop(guard);
            self.load_now()?;
        }
    }

    /// Run `f` if the value is loaded; otherwise start loading it in the background and
    /// return `None` at once.
    pub(crate) fn with_loaded<R>(self: &Arc<Self>, f: impl FnOnce(&mut T) -> R) -> Option<R> {
        let mut guard = self.slot.lock();
        if let Some(v) = guard.as_mut() {
            let r = f(v);
            self.touch();
            MutexGuard::unlock_fair(guard);
            return Some(r);
        }
        drop(guard);
        if !self.loading.swap(true, Ordering::AcqRel) {
            let me = self.clone();
            let job: Box<dyn FnOnce() + Send> = Box::new(move || {
                if let Err(e) = me.load_now() {
                    tracing::warn!(error = %e, "reloading the embedding model failed");
                    *me.last_error.lock() =
                        Some(format!("reloading the embedding model failed: {e}"));
                }
                me.loading.store(false, Ordering::Release);
            });
            if let Err(e) = (self.spawn)(job) {
                // No thread, no load: say why, and let the next query try again.
                tracing::warn!(error = %e, "cannot start the model loader thread");
                *self.last_error.lock() =
                    Some(format!("cannot start the model loader thread: {e}"));
                self.loading.store(false, Ordering::Release);
            }
        }
        None
    }

    /// Drop the value if it has not been used for `idle`; returns whether it was dropped.
    /// A value in use (its lock held) is left alone.
    pub(crate) fn reap(&self) -> bool {
        let Some(idle) = self.idle else { return false };
        let now = self.started.elapsed().as_millis() as u64;
        let last = self.last_used_ms.load(Ordering::Relaxed);
        if now.saturating_sub(last) < idle.as_millis() as u64 {
            return false;
        }
        let dropped = match self.slot.try_lock() {
            Some(mut g) if g.is_some() => g.take(),
            _ => None,
        };
        // Freed outside the lock: a query arriving now is not held up by the deallocation.
        let reaped = dropped.is_some();
        drop(dropped);
        if reaped {
            tracing::info!(
                idle_secs = idle.as_secs(),
                "unloaded the embedding model after idle time"
            );
        }
        reaped
    }
}

fn reaper<T: Send + 'static>(weak: Weak<Lazy<T>>, idle: Duration) {
    let tick = (idle / 4).clamp(Duration::from_millis(10), Duration::from_secs(30));
    loop {
        std::thread::sleep(tick);
        let Some(me) = weak.upgrade() else { return };
        me.reap();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    fn counting(idle: Option<Duration>) -> (Arc<Lazy<usize>>, Arc<AtomicUsize>) {
        let loads = Arc::new(AtomicUsize::new(0));
        let l = loads.clone();
        let lazy = Lazy::new(
            Box::new(move || Ok(l.fetch_add(1, Ordering::SeqCst) + 1)),
            idle,
        )
        .unwrap();
        (lazy, loads)
    }

    #[test]
    fn without_idle_time_the_value_stays_loaded() {
        let (lazy, loads) = counting(None);
        assert_eq!(lazy.with_loaded(|v| *v), Some(1));
        assert!(!lazy.reap());
        assert_eq!(loads.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn idle_value_is_dropped_then_reloaded_in_the_background_for_queries() {
        let (lazy, loads) = counting(Some(Duration::from_millis(60)));
        std::thread::sleep(Duration::from_millis(300));
        assert!(!lazy.is_loaded(), "the reaper dropped it");
        // A query does not wait: it gets None and triggers one background load.
        assert_eq!(lazy.with_loaded(|v| *v), None);
        let deadline = Instant::now() + Duration::from_secs(5);
        while !lazy.is_loaded() {
            assert!(Instant::now() < deadline, "background reload");
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(loads.load(Ordering::SeqCst), 2);
        // The worker path waits for a load instead.
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(lazy.with_blocking(|v| *v).unwrap(), 3);
    }

    /// A loader that takes `delay` and records how many loads ran at the same time.
    fn slow(delay: Duration) -> (Arc<Lazy<usize>>, Arc<AtomicUsize>, Arc<AtomicUsize>) {
        let (loads, running, most) = (
            Arc::new(AtomicUsize::new(0)),
            Arc::new(AtomicUsize::new(0)),
            Arc::new(AtomicUsize::new(0)),
        );
        let (l, r, m) = (loads.clone(), running.clone(), most.clone());
        let lazy = Lazy::new(
            Box::new(move || {
                let now = r.fetch_add(1, Ordering::SeqCst) + 1;
                m.fetch_max(now, Ordering::SeqCst);
                std::thread::sleep(delay);
                r.fetch_sub(1, Ordering::SeqCst);
                Ok(l.fetch_add(1, Ordering::SeqCst) + 1)
            }),
            None,
        )
        .unwrap();
        (lazy, loads, most)
    }

    #[test]
    fn a_waiting_caller_reuses_a_background_load_instead_of_loading_a_second_copy() {
        let (lazy, loads, most) = slow(Duration::from_millis(200));
        lazy.unload();
        assert_eq!(lazy.with_loaded(|v| *v), None, "a query starts the reload");
        // The worker arrives while that load runs: it waits for it rather than loading too.
        std::thread::sleep(Duration::from_millis(20));
        assert_eq!(lazy.with_blocking(|v| *v).unwrap(), 2);
        assert_eq!(
            loads.load(Ordering::SeqCst),
            2,
            "one load at start, one reload"
        );
        assert_eq!(most.load(Ordering::SeqCst), 1, "never two loads at once");
    }

    #[test]
    fn a_query_does_not_wait_while_the_worker_loads() {
        let (lazy, _loads, _) = slow(Duration::from_millis(400));
        lazy.unload();
        let worker = {
            let lazy = lazy.clone();
            std::thread::spawn(move || lazy.with_blocking(|v| *v).unwrap())
        };
        std::thread::sleep(Duration::from_millis(50));
        let t = Instant::now();
        assert_eq!(lazy.with_loaded(|v| *v), None);
        assert!(
            t.elapsed() < Duration::from_millis(200),
            "the query waited {:?} for the worker's load",
            t.elapsed()
        );
        assert_eq!(worker.join().unwrap(), 2);
    }

    #[test]
    fn a_loader_thread_that_cannot_start_is_reported_and_retried_by_the_next_query() {
        let loads = Arc::new(AtomicUsize::new(0));
        let l = loads.clone();
        let fail_next = Arc::new(AtomicBool::new(true));
        let f = fail_next.clone();
        let lazy = Lazy::with_spawner(
            Box::new(move || Ok(l.fetch_add(1, Ordering::SeqCst) + 1)),
            None,
            // The first spawn fails; later ones run the job inline (deterministic).
            Box::new(move |job| {
                if f.swap(false, Ordering::SeqCst) {
                    return Err(std::io::Error::other("no threads left"));
                }
                job();
                Ok(())
            }),
        )
        .unwrap();
        lazy.unload();
        assert_eq!(lazy.with_loaded(|v| *v), None);
        assert!(
            lazy.last_error()
                .unwrap()
                .contains("cannot start the model loader thread"),
            "{:?}",
            lazy.last_error()
        );
        assert_eq!(
            loads.load(Ordering::SeqCst),
            1,
            "nothing loaded, and `loading` was reset"
        );
        assert_eq!(
            lazy.with_loaded(|v| *v),
            None,
            "the next query starts the load again"
        );
        assert_eq!(lazy.with_loaded(|v| *v), Some(2));
        assert!(
            lazy.last_error().is_none(),
            "a successful load clears the error"
        );
    }
}
