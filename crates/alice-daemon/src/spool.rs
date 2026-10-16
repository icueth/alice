//! Requests written to disk while the daemon is unavailable or read-only; drained in order.
//! One file per request (`<ulid>.json`), written atomically, so concurrent writers never collide.

use crate::service::Service;
use alice_core::api::{CaptureNotify, IngestRequest, RememberRequest};
use alice_core::error::AliceError;
use alice_core::ids::new_id;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum SpoolEntry {
    Remember(RememberRequest),
    Ingest(IngestRequest),
    /// A capture notify written by `alice hook` while the daemon was down (SP2 spec §4.1):
    /// drained, it is an ordinary notify, and the pass reads from the source's cursor.
    CaptureNotify(CaptureNotify),
}

/// What a reply says about a write queued because the database is read-only (`reason`).
pub fn read_only_note(reason: &str) -> String {
    format!(
        "memory is read-only ({reason}); the memory is saved once the database is repaired and the daemon restarted. See \"alice status\" and \"alice daemon logs\""
    )
}

/// Make a rename inside `dir` durable (Unix; Windows has no directory handles to sync).
fn sync_dir(dir: &Path) {
    #[cfg(unix)]
    if let Ok(d) = std::fs::File::open(dir) {
        let _ = d.sync_all();
    }
    #[cfg(not(unix))]
    let _ = dir;
}

/// Spooled requests hold memory text: directories this creates are 0700 and files 0600 (Unix).
pub fn append(dir: &Path, entry: &SpoolEntry) -> std::io::Result<PathBuf> {
    let mut mkdir = std::fs::DirBuilder::new();
    mkdir.recursive(true);
    let mut open = std::fs::OpenOptions::new();
    open.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
        mkdir.mode(0o700);
        open.mode(0o600);
    }
    mkdir.create(dir)?;
    let id = new_id();
    let tmp = dir.join(format!("{id}.tmp"));
    let dest = dir.join(format!("{id}.json"));
    let mut f = open.open(&tmp)?;
    f.write_all(&serde_json::to_vec(entry)?)?;
    f.sync_all()?;
    drop(f);
    std::fs::rename(&tmp, &dest)?;
    sync_dir(dir);
    Ok(dest)
}

/// Complete entries, oldest first (ULID names sort by creation time).
fn entries(dir: &Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .map(|rd| {
            rd.filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| p.extension().is_some_and(|x| x == "json"))
                .collect()
        })
        .unwrap_or_default();
    files.sort();
    files
}

pub fn pending_count(dir: &Path) -> usize {
    entries(dir).len()
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct DrainReport {
    pub processed: usize,
    pub rejected: usize,
    pub remaining: usize,
}

/// Move an entry the service will never accept to `rejected/`, with the reason next to it.
fn reject(dir: &Path, file: &Path, why: &str) {
    let rejected = dir.join("rejected");
    let _ = std::fs::create_dir_all(&rejected);
    if let Some(name) = file.file_name() {
        let _ = std::fs::rename(file, rejected.join(name));
        let _ = std::fs::write(rejected.join(name).with_extension("err"), why);
    }
}

/// Entries already reported as kept because of a fail-closed state: the drain runs every few
/// seconds and a state can last for hours, so each entry is reported once, not on every drain.
static REPORTED: std::sync::LazyLock<parking_lot::Mutex<std::collections::HashSet<PathBuf>>> =
    std::sync::LazyLock::new(Default::default);

/// Drop an entry that `block` mode refuses and keep the reason next to the rejected ones. The
/// entry itself is not kept: its text holds what the rules forbid on disk. Failures are logged,
/// not swallowed.
fn withhold(dir: &Path, file: &Path, why: &str) {
    let rejected = dir.join("rejected");
    if let Err(e) = std::fs::create_dir_all(&rejected) {
        tracing::warn!(error = %e, "cannot create the spool's rejected directory");
    }
    let Some(name) = file.file_name() else { return };
    let note =
        format!("{why}\nThe request was removed instead of kept, because its text holds a secret.");
    if let Err(e) = std::fs::write(rejected.join(name).with_extension("err"), note) {
        tracing::warn!(error = %e, "cannot write the reason a spooled request was removed");
    }
    if let Err(e) = std::fs::remove_file(file) {
        tracing::warn!(error = %e, "cannot remove a refused spooled request");
    }
}

/// What the drain asks of the service: the daemon's [`Service`] (tests script it).
pub(crate) trait Applier {
    /// Apply one queued request.
    async fn apply(&self, entry: SpoolEntry) -> Result<(), AliceError>;
    /// Whether a write to this project would be judged under fail-closed rules right now.
    async fn fail_closed(&self, scope: crate::service::Scope<'_>) -> bool;
}

impl Applier for Service {
    async fn apply(&self, entry: SpoolEntry) -> Result<(), AliceError> {
        match entry {
            SpoolEntry::Remember(r) => self.remember_spooled(r).await.map(|_| ()),
            SpoolEntry::Ingest(r) => self.ingest_spooled(r).await.map(|_| ()),
            SpoolEntry::CaptureNotify(n) => self.capture_notify_spooled(n),
        }
    }

    async fn fail_closed(&self, scope: crate::service::Scope<'_>) -> bool {
        self.rules_fail_closed(scope).await
    }
}

/// The first wait before an entry that could not be applied is tried again; it doubles with
/// every failure up to [`MAX_RETRY`].
const FIRST_RETRY: Duration = if cfg!(test) {
    Duration::ZERO
} else {
    Duration::from_secs(15)
};
const MAX_RETRY: Duration = Duration::from_secs(600);
/// An entry that fails this many times in a row with the same error (not a refusal, not a
/// transient state the drain can see) is moved to `rejected/`, so it cannot hold back the
/// entries queued after it for ever.
const MAX_SAME_FAILURES: u32 = 6;

/// An entry that could not be applied: how often in a row, with what error, when to try again,
/// and whether the entries after it wait for it (a failure keeps the order; an entry kept under
/// fail-closed rules does not hold the others back).
struct Retry {
    failures: u32,
    last: String,
    next: Instant,
    holds: bool,
}

static RETRY: std::sync::LazyLock<parking_lot::Mutex<HashMap<PathBuf, Retry>>> =
    std::sync::LazyLock::new(Default::default);

/// Count a failure of `file` with `error`; the number of failures in a row with that error.
fn failed(file: &Path, error: &str, holds: bool, now: Instant) -> u32 {
    let mut all = RETRY.lock();
    let r = all.entry(file.to_path_buf()).or_insert(Retry {
        failures: 0,
        last: String::new(),
        next: now,
        holds,
    });
    if r.last != error || r.holds != holds {
        r.failures = 0;
        r.last = error.to_string();
        r.holds = holds;
    }
    r.failures += 1;
    r.next = now + retry_wait(FIRST_RETRY, r.failures);
    r.failures
}

/// The wait after the `failures`-th failure in a row: `first`, doubling, at most [`MAX_RETRY`].
fn retry_wait(first: Duration, failures: u32) -> Duration {
    first
        .saturating_mul(2u32.saturating_pow(failures.saturating_sub(1).min(16)))
        .min(MAX_RETRY)
}

/// Apply spooled requests oldest first. Invalid requests move to `spool/rejected/`; a failure
/// stops the drain so the order is kept for the next attempt, and is tried again after a
/// growing wait (an entry that keeps failing the same way is moved to `rejected/` after
/// [`MAX_SAME_FAILURES`] tries). An entry refused only because its rules are in a fail-closed
/// state stays queued and is tried again after a growing wait, without holding the others back.
/// The drain is not activity: it never keeps maintenance from starting.
///
/// Delivery is at-least-once: an entry applied just before a crash is applied again on the
/// next drain, which is harmless because `remember` and `ingest` both skip content they
/// already hold, and a capture notify reads from the source's cursor.
pub async fn drain(dir: &Path, svc: &Service) -> DrainReport {
    drain_with(dir, svc).await
}

pub(crate) async fn drain_with<A: Applier>(dir: &Path, svc: &A) -> DrainReport {
    let mut report = DrainReport::default();
    let files = entries(dir);
    let now = Instant::now();
    // Forget entries that are gone, so a new one with the same name would be reported again.
    REPORTED
        .lock()
        .retain(|p| p.parent() != Some(dir) || files.contains(p));
    RETRY
        .lock()
        .retain(|p, _| p.parent() != Some(dir) || files.contains(p));
    for (i, file) in files.iter().enumerate() {
        let waiting = RETRY
            .lock()
            .get(file)
            .filter(|r| r.next > now)
            .map(|r| r.holds);
        match waiting {
            Some(true) => {
                report.remaining = files.len() - i;
                return report;
            }
            Some(false) => {
                report.remaining += 1;
                continue;
            }
            None => {}
        }
        let parsed: Result<SpoolEntry, String> = std::fs::read(file)
            .map_err(|e| e.to_string())
            .and_then(|b| serde_json::from_slice(&b).map_err(|e| e.to_string()));
        let entry = match parsed {
            Ok(e) => e,
            Err(why) => {
                reject(dir, file, &why);
                report.rejected += 1;
                continue;
            }
        };
        let (project, cwd, client_project, hint) = match &entry {
            SpoolEntry::Remember(r) => (
                r.project.clone(),
                r.cwd.clone(),
                r.client_project.clone(),
                r.project_hint.clone(),
            ),
            SpoolEntry::Ingest(r) => (
                r.project.clone(),
                r.cwd.clone(),
                r.client_project.clone(),
                r.project_hint.clone(),
            ),
            SpoolEntry::CaptureNotify(n) => (None, n.cwd.clone(), None, None),
        };
        let result = svc.apply(entry).await;
        match result {
            Ok(()) => {
                let _ = std::fs::remove_file(file);
                RETRY.lock().remove(file);
                report.processed += 1;
            }
            // A request that block mode refuses held a secret: only the reason is kept.
            Err(e) if alice_core::redact::is_refusal(&e) => {
                // Refused only because the project's rules are in a fail-closed state (a
                // half-written `.alice.toml`, say): keep the entry for the next drain.
                if svc
                    .fail_closed(crate::service::Scope {
                        project: project.as_deref(),
                        cwd: cwd.as_deref(),
                        client_project: client_project.as_ref(),
                        project_hint: hint.as_deref(),
                    })
                    .await
                {
                    failed(file, "fail-closed", false, now);
                    if REPORTED.lock().insert(file.clone()) {
                        tracing::warn!(
                            entry = %file.display(),
                            "a spooled request stays queued: the privacy rules of its project are in a fail-closed state (check .alice.toml and config.toml)"
                        );
                    }
                    report.remaining += 1;
                    continue;
                }
                withhold(dir, file, &e.to_string());
                report.rejected += 1;
            }
            Err(e @ (AliceError::Invalid(_) | AliceError::NotFound(_))) => {
                reject(dir, file, &e.to_string());
                report.rejected += 1;
            }
            Err(e) => {
                let why = e.to_string();
                let n = failed(file, &why, true, now);
                if n >= MAX_SAME_FAILURES {
                    tracing::warn!(error = %e, entry = %file.display(), failures = n, "a spooled request keeps failing the same way; moved to spool/rejected so the requests after it go ahead");
                    reject(
                        dir,
                        file,
                        &format!(
                            "{why}\nIt failed {n} times in a row the same way and was moved aside so the requests after it could be applied; queue it again once the cause is fixed."
                        ),
                    );
                    RETRY.lock().remove(file);
                    report.rejected += 1;
                    continue;
                }
                tracing::warn!(error = %e, failures = n, "spool drain paused; the request is tried again later");
                report.remaining = files.len() - i;
                return report;
            }
        }
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paths::AliceHome;
    use crate::service::ServiceOptions;
    use alice_core::embed::{Embedder, FakeEmbedder};
    use alice_core::store::sources::{ChunkInput, SourceInput};
    use std::sync::Arc;

    fn start(home: &AliceHome) -> Arc<Service> {
        Service::start(
            home.clone(),
            &|| Ok(Arc::new(FakeEmbedder::new(384)) as Arc<dyn Embedder>),
            ServiceOptions::default(),
        )
        .unwrap()
    }

    fn remember(content: &str) -> SpoolEntry {
        SpoolEntry::Remember(RememberRequest {
            content: content.into(),
            ..Default::default()
        })
    }

    /// Memory bodies in insertion order, read straight from the database.
    fn bodies(home: &AliceHome) -> Vec<String> {
        let c = alice_core::store::open_reader(&home.db()).unwrap();
        let mut stmt = c.prepare("SELECT body FROM memories ORDER BY seq").unwrap();
        stmt.query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn drain_applies_in_order_and_rejects_bad_entries() {
        let d = tempfile::tempdir().unwrap();
        let home = AliceHome::new(d.path().to_path_buf());
        let svc = start(&home);
        let dir = home.spool_dir();
        append(&dir, &remember("spooled one")).unwrap();
        append(&dir, &remember("   ")).unwrap();
        std::fs::write(dir.join("zzzz.json"), b"not json").unwrap();
        append(&dir, &remember("spooled two")).unwrap();
        let r = drain(&dir, &svc).await;
        assert_eq!(
            r,
            DrainReport {
                processed: 2,
                rejected: 2,
                remaining: 0
            }
        );
        assert_eq!(pending_count(&dir), 0);
        assert_eq!(
            std::fs::read_dir(dir.join("rejected")).unwrap().count(),
            4,
            "two entries plus two .err notes"
        );
        assert_eq!(bodies(&home), ["spooled one", "spooled two"]);
    }

    /// Review focus #3 (lose no writes): when the writer is gone, nothing is dropped or reordered.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_transient_failure_keeps_every_entry_for_the_next_drain() {
        let d = tempfile::tempdir().unwrap();
        let home = AliceHome::new(d.path().to_path_buf());
        let dir = home.spool_dir();
        for c in ["first", "second", "third"] {
            append(&dir, &remember(c)).unwrap();
        }
        let svc = start(&home);
        let stopped = svc.clone();
        tokio::task::spawn_blocking(move || stopped.shutdown())
            .await
            .unwrap();
        let r = drain(&dir, &svc).await;
        assert_eq!(
            r,
            DrainReport {
                processed: 0,
                rejected: 0,
                remaining: 3
            }
        );
        assert_eq!(pending_count(&dir), 3);
        assert!(!dir.join("rejected").exists());
        drop(svc);

        let svc = start(&home);
        let r = drain(&dir, &svc).await;
        assert_eq!(
            r,
            DrainReport {
                processed: 3,
                rejected: 0,
                remaining: 0
            }
        );
        assert_eq!(bodies(&home), ["first", "second", "third"]);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn spooled_ingest_is_applied() {
        let d = tempfile::tempdir().unwrap();
        let home = AliceHome::new(d.path().to_path_buf());
        let dir = home.spool_dir();
        let path = append(
            &dir,
            &SpoolEntry::Ingest(IngestRequest {
                source: SourceInput {
                    uri: "session-1".into(),
                    ..Default::default()
                },
                chunks: vec![ChunkInput {
                    ord: 0,
                    text: "captured while the daemon was down".into(),
                    ..Default::default()
                }],
                ..Default::default()
            }),
        )
        .unwrap();
        let raw: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(raw["op"], "ingest");
        let svc = start(&home);
        assert_eq!(drain(&dir, &svc).await.processed, 1);
        assert_eq!(svc.health().await.unwrap().chunks, 1);
    }

    #[test]
    fn append_leaves_only_complete_files() {
        let d = tempfile::tempdir().unwrap();
        let dir = d.path().join("spool");
        let a = append(&dir, &remember("one")).unwrap();
        let b = append(&dir, &remember("two")).unwrap();
        assert!(a < b, "names sort in append order");
        assert_eq!(pending_count(&dir), 2);
        let names: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert!(names.iter().all(|n| n.ends_with(".json")), "{names:?}");
        let raw: serde_json::Value = serde_json::from_slice(&std::fs::read(&a).unwrap()).unwrap();
        assert_eq!(raw["op"], "remember");
        assert_eq!(raw["content"], "one");
        assert_eq!(pending_count(&d.path().join("missing")), 0);
    }

    // Unix file modes (0600 / 0700); Windows has no mode bits.
    #[cfg(unix)]
    #[test]
    fn spool_files_and_a_new_spool_dir_are_private() {
        use std::os::unix::fs::PermissionsExt;
        let d = tempfile::tempdir().unwrap();
        let dir = d.path().join("fresh").join("spool");
        let file = append(&dir, &remember("secret-ish")).unwrap();
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&file), 0o600);
        assert_eq!(mode(&dir), 0o700);
        // An existing directory keeps its mode; files in it are still private.
        let open = d.path().join("open");
        std::fs::create_dir(&open).unwrap();
        std::fs::set_permissions(&open, std::fs::Permissions::from_mode(0o755)).unwrap();
        let file = append(&open, &remember("two")).unwrap();
        assert_eq!(mode(&file), 0o600);
        assert_eq!(mode(&open), 0o755);
    }

    const GITHUB: &str = concat!("gh", "p_aBcDeFgHiJkLmNoPqRsTuVwXyZ0123456789");

    /// An entry queued by an older Alice (secret and all) is stored redacted when drained, and one
    /// that block mode refuses is not kept in `rejected/`: only the reason is.
    #[tokio::test(flavor = "multi_thread")]
    async fn drain_redacts_old_entries_and_keeps_no_text_that_block_mode_refuses() {
        let d = tempfile::tempdir().unwrap();
        let home = AliceHome::new(d.path().to_path_buf());
        let svc = start(&home);
        let dir = home.spool_dir();
        append(&dir, &remember(&format!("old entry with {GITHUB}"))).unwrap();
        let blocked = d.path().join("blocked");
        std::fs::create_dir(&blocked).unwrap();
        std::fs::write(
            blocked.join(".alice.toml"),
            "[privacy]\nblock_secrets = true\n",
        )
        .unwrap();
        append(
            &dir,
            &SpoolEntry::Remember(RememberRequest {
                content: format!("refused with {GITHUB}"),
                cwd: Some(blocked.to_string_lossy().into_owned()),
                ..Default::default()
            }),
        )
        .unwrap();
        let r = drain(&dir, &svc).await;
        assert_eq!((r.processed, r.rejected, r.remaining), (1, 1, 0));
        assert_eq!(bodies(&home), ["old entry with [REDACTED:github_token]"]);
        assert_eq!(pending_count(&dir), 0);
        let mut all = String::new();
        for e in std::fs::read_dir(dir.join("rejected")).unwrap().flatten() {
            all.push_str(&std::fs::read_to_string(e.path()).unwrap());
        }
        assert!(
            all.contains("github_token") && !all.contains(GITHUB),
            "{all}"
        );
        assert!(
            !std::fs::read_dir(dir.join("rejected"))
                .unwrap()
                .flatten()
                .any(|e| e.path().extension().is_some_and(|x| x == "json")),
            "the refused request itself is gone"
        );
    }

    /// Final review D-I1: a request the store keeps refusing the same way holds the queue only
    /// for a few tries: then it is moved to `rejected/` (with the reason) and the requests after
    /// it are applied, in order.
    #[tokio::test(flavor = "multi_thread")]
    async fn an_entry_that_keeps_failing_the_same_way_is_moved_aside_and_the_rest_go_ahead() {
        struct Script {
            applied: parking_lot::Mutex<Vec<String>>,
        }
        impl Applier for Script {
            async fn apply(&self, entry: SpoolEntry) -> Result<(), AliceError> {
                let SpoolEntry::Remember(r) = entry else {
                    unreachable!()
                };
                if r.content == "poison" {
                    return Err(AliceError::Other("the store said no".into()));
                }
                self.applied.lock().push(r.content);
                Ok(())
            }
            async fn fail_closed(&self, _: crate::service::Scope<'_>) -> bool {
                false
            }
        }
        let d = tempfile::tempdir().unwrap();
        let dir = d.path().join("spool");
        for c in ["first", "poison", "second", "third"] {
            append(&dir, &remember(c)).unwrap();
        }
        let svc = Script {
            applied: Default::default(),
        };
        for n in 1..MAX_SAME_FAILURES {
            let r = drain_with(&dir, &svc).await;
            assert_eq!((r.rejected, r.remaining), (0, 3), "drain {n}: {r:?}");
        }
        assert_eq!(
            *svc.applied.lock(),
            ["first"],
            "the order is kept meanwhile"
        );
        let r = drain_with(&dir, &svc).await;
        assert_eq!((r.processed, r.rejected, r.remaining), (2, 1, 0), "{r:?}");
        assert_eq!(*svc.applied.lock(), ["first", "second", "third"]);
        let why: String = std::fs::read_dir(dir.join("rejected"))
            .unwrap()
            .flatten()
            .filter(|e| e.path().extension().is_some_and(|x| x == "err"))
            .map(|e| std::fs::read_to_string(e.path()).unwrap())
            .collect();
        assert!(
            why.contains("the store said no") && why.contains("times in a row"),
            "{why}"
        );
        assert!(
            std::fs::read_dir(dir.join("rejected"))
                .unwrap()
                .flatten()
                .any(|e| e.path().extension().is_some_and(|x| x == "json")),
            "the request itself is kept"
        );
    }

    /// Final review D-I1: retrying a request that is kept or failing waits longer each time.
    #[test]
    fn a_failing_entry_waits_longer_each_time_up_to_ten_minutes() {
        let first = Duration::from_secs(15);
        let waits: Vec<u64> = (1..=8).map(|n| retry_wait(first, n).as_secs()).collect();
        assert_eq!(waits, [15, 30, 60, 120, 240, 480, 600, 600]);
        assert_eq!(retry_wait(first, 1_000), MAX_RETRY);
    }

    /// Final review D-I1: a spooled request kept under fail-closed rules is not activity: it
    /// never keeps maintenance (the daily backup included) from starting.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_kept_spool_entry_does_not_count_as_activity() {
        let d = tempfile::tempdir().unwrap();
        let home = AliceHome::new(d.path().to_path_buf());
        let svc = start(&home);
        let dir = home.spool_dir();
        let work = d.path().join("work");
        std::fs::create_dir(&work).unwrap();
        std::fs::write(work.join(".alice.toml"), "[privacy\nblock_secrets = tru").unwrap();
        append(
            &dir,
            &SpoolEntry::Remember(RememberRequest {
                content: format!("kept with {GITHUB}"),
                cwd: Some(work.to_string_lossy().into_owned()),
                ..Default::default()
            }),
        )
        .unwrap();
        tokio::time::sleep(Duration::from_millis(150)).await;
        for _ in 0..3 {
            let before = svc.activity().idle_for();
            assert_eq!(drain(&dir, &svc).await.remaining, 1);
            assert!(
                svc.activity().idle_for() >= before,
                "the drain counted as a request"
            );
        }
        assert!(svc.activity().idle_for() >= Duration::from_millis(150));
        // An applied entry is not activity either.
        append(&dir, &remember("plain")).unwrap();
        let before = svc.activity().idle_for();
        assert_eq!(drain(&dir, &svc).await.processed, 1);
        assert!(svc.activity().idle_for() >= before);
        svc.shutdown();
    }

    /// A refusal that comes from a fail-closed state (a half-written `.alice.toml`) keeps the
    /// entry for the next drain; once the file is fixed the entry is stored.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_refusal_under_a_broken_project_file_keeps_the_entry_for_the_next_drain() {
        let d = tempfile::tempdir().unwrap();
        let home = AliceHome::new(d.path().to_path_buf());
        let svc = start(&home);
        let dir = home.spool_dir();
        let work = d.path().join("work");
        std::fs::create_dir(&work).unwrap();
        std::fs::write(work.join(".alice.toml"), "[privacy\nblock_secrets = tru").unwrap();
        append(
            &dir,
            &SpoolEntry::Remember(RememberRequest {
                content: format!("kept with {GITHUB}"),
                cwd: Some(work.to_string_lossy().into_owned()),
                ..Default::default()
            }),
        )
        .unwrap();
        let r = drain(&dir, &svc).await;
        assert_eq!((r.processed, r.rejected, r.remaining), (0, 0, 1));
        assert_eq!(pending_count(&dir), 1, "still queued");
        // A second drain keeps it too, and reports it only once.
        let again = drain(&dir, &svc).await;
        assert_eq!(again.remaining, 1);
        assert!(
            REPORTED
                .lock()
                .iter()
                .filter(|p| p.starts_with(&dir))
                .count()
                == 1,
            "one report per entry"
        );
        assert!(!dir.join("rejected").exists());
        std::fs::write(
            work.join(".alice.toml"),
            "[privacy]\nblock_secrets = false\n",
        )
        .unwrap();
        // Tightening or equal applies at once; this file is valid and does not block.
        let r = drain(&dir, &svc).await;
        assert_eq!((r.processed, r.rejected), (1, 0), "{r:?}");
        assert_eq!(bodies(&home), ["kept with [REDACTED:github_token]"]);
    }
}
