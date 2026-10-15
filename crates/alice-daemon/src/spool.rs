//! Requests written to disk while the daemon is unavailable or read-only; drained in order.
//! One file per request (`<ulid>.json`), written atomically, so concurrent writers never collide.

use crate::service::Service;
use alice_core::api::{IngestRequest, RememberRequest};
use alice_core::error::AliceError;
use alice_core::ids::new_id;
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum SpoolEntry {
    Remember(RememberRequest),
    Ingest(IngestRequest),
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

/// Apply spooled requests oldest first. Invalid requests move to `spool/rejected/`;
/// a transient failure stops the drain so the order is kept for the next attempt.
///
/// Delivery is at-least-once: an entry applied just before a crash is applied again on the
/// next drain, which is harmless because `remember` and `ingest` both skip content they
/// already hold.
pub async fn drain(dir: &Path, svc: &Service) -> DrainReport {
    let mut report = DrainReport::default();
    let files = entries(dir);
    // Forget entries that are gone, so a new one with the same name would be reported again.
    REPORTED
        .lock()
        .retain(|p| p.parent() != Some(dir) || files.contains(p));
    for (i, file) in files.iter().enumerate() {
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
        let (project, cwd, client_project) = match &entry {
            SpoolEntry::Remember(r) => (r.project.clone(), r.cwd.clone(), r.client_project.clone()),
            SpoolEntry::Ingest(r) => (r.project.clone(), r.cwd.clone(), r.client_project.clone()),
        };
        let result = match entry {
            SpoolEntry::Remember(r) => svc.remember(r).await.map(|_| ()),
            SpoolEntry::Ingest(r) => svc.ingest(r).await.map(|_| ()),
        };
        match result {
            Ok(()) => {
                let _ = std::fs::remove_file(file);
                report.processed += 1;
            }
            // A request that block mode refuses held a secret: only the reason is kept.
            Err(e) if alice_core::redact::is_refusal(&e) => {
                // Refused only because the project's rules are in a fail-closed state (a
                // half-written `.alice.toml`, say): keep the entry for the next drain.
                if svc
                    .rules_fail_closed(crate::service::Scope {
                        project: project.as_deref(),
                        cwd: cwd.as_deref(),
                        client_project: client_project.as_ref(),
                    })
                    .await
                {
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
                tracing::warn!(error = %e, "spool drain paused");
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
