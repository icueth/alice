//! `alice adopt` (server spec §2.12, addendum 4): a person's own database becomes a fresh tenant's,
//! without their machine's paths and redacted with the tenant's rules.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use alice_core::api::{IngestRequest, OutputFormat, RecallMode, RecallRequest, RememberRequest};
use alice_core::embed::{Embedder, FakeEmbedder};
use alice_core::model::SourceType;
use alice_core::store::sources::{ChunkInput, SourceInput};
use alice_daemon::adopt;
use alice_daemon::paths::AliceHome;
use alice_daemon::service::{Service, ServiceOptions};
use rusqlite::Connection;

/// A key the built-in detectors redact; built in two halves so no file holds it whole.
const SECRET: &str = concat!("AKIA", "Q7ZX3P9LMN2BV4KD");
const OTHER_SECRET: &str = concat!("AKIA", "J4WQ8RT2ZX6NB1LM");
const TRANSCRIPT: &str = "/Users/someone/.claude/projects/-Users-someone-Code-shop/s1.jsonl";

fn start(home: &AliceHome, tenant: bool) -> Arc<Service> {
    Service::start(
        home.clone(),
        &|| Ok(Arc::new(FakeEmbedder::new(384)) as Arc<dyn Embedder>),
        ServiceOptions {
            tenant,
            ..ServiceOptions::default()
        },
    )
    .unwrap()
}

async fn settled(s: &Service) {
    for _ in 0..500 {
        let h = s.health().await.unwrap();
        if h.embed_backlog == 0 {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("the embedding backlog did not drain");
}

/// A person's machine: Alice with redaction switched off (a weaker setup than any tenant), a
/// git-less project (its key and root are paths), a transcript under the home directory, and the
/// same note twice, differing only by a key. Returns the source home, the project's checkout and
/// a backup made with `backup now`.
async fn person(dir: &Path) -> (AliceHome, PathBuf, PathBuf) {
    let home = AliceHome::new(dir.join("person-home"));
    std::fs::create_dir_all(home.root()).unwrap();
    std::fs::write(
        home.root().join("config.toml"),
        "[privacy]\nredact = false\n",
    )
    .unwrap();
    let checkout = dir.join("Code").join("shop");
    std::fs::create_dir_all(&checkout).unwrap();
    let cwd = Some(checkout.display().to_string());
    let s = start(&home, false);
    for (content, key) in [
        (
            "Shop deploy: the staging key is {} for the S3 bucket",
            SECRET,
        ),
        (
            "Shop deploy: the staging key is {} for the S3 bucket",
            OTHER_SECRET,
        ),
        ("Shop uses Postgres 16 with pgvector for search", ""),
    ] {
        s.remember(RememberRequest {
            content: content.replace("{}", key),
            cwd: cwd.clone(),
            ..Default::default()
        })
        .await
        .unwrap();
    }
    s.ingest(IngestRequest {
        source: SourceInput {
            source_type: SourceType::Session,
            uri: TRANSCRIPT.into(),
            ..Default::default()
        },
        chunks: vec![
            ChunkInput {
                ord: 0,
                text: format!("user: put {SECRET} into the bucket policy"),
                ..Default::default()
            },
            ChunkInput {
                ord: 1,
                text: "assistant: the shop's search runs on pgvector".into(),
                ..Default::default()
            },
        ],
        cwd: cwd.clone(),
        ..Default::default()
    })
    .await
    .unwrap();
    settled(&s).await;
    let backup = s.backup_now().await.unwrap().path;
    s.shutdown();
    (home, checkout, backup)
}

/// A tenant as `alice-server add` leaves it: started once, empty.
fn fresh_tenant(dir: &Path, name: &str) -> AliceHome {
    let home = AliceHome::new(dir.join(name));
    let s = start(&home, true);
    s.shutdown();
    home
}

fn db_text(home: &AliceHome) -> Vec<u8> {
    let mut all = std::fs::read(home.db()).unwrap();
    for ext in ["-wal", "-shm"] {
        let mut p = home.db().into_os_string();
        p.push(ext);
        all.extend(std::fs::read(PathBuf::from(p)).unwrap_or_default());
    }
    all
}

fn contains(hay: &[u8], needle: &str) -> bool {
    hay.windows(needle.len()).any(|w| w == needle.as_bytes())
}

/// Also the lower-case form: the full-text tokenizers fold case.
fn contains_any_case(hay: &[u8], needle: &str) -> bool {
    contains(hay, needle) || contains(hay, &needle.to_lowercase())
}

/// Text the person deleted on their machine: in free pages of the backup file.
const DELETED_ON_THE_MAC: &str = "deleted-on-the-mac-zebra-falcon-0042";

#[tokio::test(flavor = "multi_thread")]
async fn an_adopted_database_loses_the_machines_paths_and_is_redacted_with_the_tenants_rules() {
    let d = tempfile::tempdir().unwrap();
    let (source, checkout, backup) = person(d.path()).await;
    {
        let c = Connection::open(&backup).unwrap();
        c.execute_batch(&format!(
            "PRAGMA secure_delete = OFF; CREATE TABLE gone (t TEXT); INSERT INTO gone VALUES ('{}'); DROP TABLE gone;",
            DELETED_ON_THE_MAC.repeat(40)
        ))
        .unwrap();
    }
    let src = std::fs::read(&backup).unwrap();
    assert!(contains(&src, SECRET), "the person's machine kept the key");
    assert!(
        contains(&src, DELETED_ON_THE_MAC),
        "the backup carries free pages"
    );
    assert!(contains(&src, &checkout.display().to_string()));
    let tenant = fresh_tenant(d.path(), "tenant");

    let r = adopt::adopt_now(&tenant, &backup).unwrap();
    assert_eq!((r.memories, r.chunks), (3, 2), "{r:?}");
    assert_eq!(r.redacted_memories, 2, "{r:?}");
    assert_eq!(
        r.duplicates_trashed, 1,
        "the two notes become one text: {r:?}"
    );
    assert_eq!(r.redacted_chunks, 1, "{r:?}");
    assert!(r.paths_cleared >= 3, "root, key and transcript: {r:?}");
    assert!(r.saved_tenant_db_as.is_some(), "{r:?}");

    let c = Connection::open(tenant.db()).unwrap();
    let n = |sql: &str| -> i64 { c.query_row(sql, [], |row| row.get(0)).unwrap() };
    assert_eq!(
        n("SELECT count(*) FROM projects WHERE root_path IS NOT NULL"),
        0
    );
    assert_eq!(
        n("SELECT count(*) FROM projects WHERE key LIKE 'path:%'"),
        0
    );
    assert_eq!(
        n("SELECT count(*) FROM project_key_aliases WHERE key LIKE 'path:%'"),
        0
    );
    let uri: String = c
        .query_row("SELECT uri FROM sources", [], |row| row.get(0))
        .unwrap();
    assert!(
        uri.starts_with("adopted:") && uri.ends_with("/s1.jsonl"),
        "{uri}"
    );
    assert_eq!(n("SELECT count(*) FROM judge_cache"), 0);
    // Embedded again: the vectors of the old text are gone or pending.
    assert_eq!(
        n("SELECT count(*) FROM chunks WHERE embed_state = 'pending'"),
        1
    );
    assert_eq!(
        n("SELECT count(*) FROM memories WHERE embed_state = 'pending'"),
        2
    );
    assert!(n("SELECT count(*) FROM events WHERE action = 'chunk.redact'") == 1);
    drop(c);
    let bytes = db_text(&tenant);
    for gone in [
        SECRET,
        OTHER_SECRET,
        &checkout.display().to_string()[..],
        TRANSCRIPT,
        "/Users/someone",
        DELETED_ON_THE_MAC,
    ] {
        assert!(
            !contains_any_case(&bytes, gone),
            "{gone} is still in the tenant's database"
        );
    }

    // Served as a tenant: a project hint (`?project=`) finds the person's project and its memory.
    let slug: String = Connection::open(tenant.db())
        .unwrap()
        .query_row(
            "SELECT slug FROM projects WHERE slug <> 'global' ORDER BY created_at LIMIT 1",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let s = start(&tenant, true);
    settled(&s).await;
    let found = s
        .recall(RecallRequest {
            query: "which database does the shop use for search".into(),
            mode: Some(RecallMode::Fast),
            format: OutputFormat::Json,
            project_hint: Some(slug.clone()),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(found.project.as_deref(), Some(slug.as_str()));
    assert!(
        found
            .results
            .iter()
            .any(|h| h.snippet.contains("Postgres 16")),
        "{:?}",
        found.results
    );
    let keyed = s
        .recall(RecallRequest {
            query: "shop staging key S3 bucket".into(),
            mode: Some(RecallMode::Fast),
            format: OutputFormat::Json,
            project_hint: Some(slug),
            include_raw: true,
            ..Default::default()
        })
        .await
        .unwrap();
    assert!(!keyed.results.is_empty());
    assert!(
        keyed.results.iter().all(|h| !h.snippet.contains("AKIA")),
        "{:?}",
        keyed.results
    );
    s.shutdown();
    drop(source);
}

#[tokio::test(flavor = "multi_thread")]
async fn adopt_refuses_a_tenant_that_holds_anything_and_leaves_it_as_it_was() {
    let d = tempfile::tempdir().unwrap();
    let (_, _, backup) = person(d.path()).await;
    let tenant = fresh_tenant(d.path(), "tenant");
    adopt::adopt_now(&tenant, &backup).unwrap();
    let before = std::fs::read(tenant.db()).unwrap();
    let e = adopt::adopt_now(&tenant, &backup).unwrap_err().to_string();
    assert!(
        e.contains("already holds 3 memories") && e.contains("fresh tenant"),
        "{e}"
    );
    assert_eq!(std::fs::read(tenant.db()).unwrap(), before);
}

/// A schema older or newer than this build's, or a damaged file, is refused before anything
/// changes.
#[tokio::test(flavor = "multi_thread")]
async fn adopt_refuses_another_schema_and_a_damaged_file() {
    let d = tempfile::tempdir().unwrap();
    let (_, _, backup) = person(d.path()).await;
    let tenant = fresh_tenant(d.path(), "tenant");
    let before = std::fs::read(tenant.db()).unwrap();
    let copy = |name: &str| -> PathBuf {
        let p = d.path().join(name);
        std::fs::copy(&backup, &p).unwrap();
        p
    };
    let current = alice_core::store::migrations::CURRENT_VERSION;
    let older = copy("older.db");
    Connection::open(&older)
        .unwrap()
        .execute(
            "DELETE FROM schema_migrations WHERE version = ?1",
            [current],
        )
        .unwrap();
    let e = adopt::adopt_now(&tenant, &older).unwrap_err().to_string();
    assert!(
        e.contains("older Alice") && e.contains("upgrade the source Alice first"),
        "{e}"
    );
    let newer = copy("newer.db");
    Connection::open(&newer)
        .unwrap()
        .execute(
            "INSERT INTO schema_migrations (version, applied_at) VALUES (?1, '2026-10-16T00:00:00Z')",
            [current + 1],
        )
        .unwrap();
    let e = adopt::adopt_now(&tenant, &newer).unwrap_err().to_string();
    assert!(
        e.contains("newer Alice") && e.contains("upgrade the server first"),
        "{e}"
    );
    let damaged = copy("damaged.db");
    let mut bytes = std::fs::read(&damaged).unwrap();
    let mid = bytes.len() / 2;
    for b in &mut bytes[mid..mid + 4096] {
        *b = 0xA5;
    }
    std::fs::write(&damaged, bytes).unwrap();
    assert!(adopt::adopt_now(&tenant, &damaged).is_err());
    assert_eq!(
        std::fs::read(tenant.db()).unwrap(),
        before,
        "the tenant is untouched"
    );
}
