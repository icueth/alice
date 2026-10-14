//! Secret redaction on every write path (spec §5.3): what a client sends is redacted before
//! the writer, the history, the event stream or the disk see it, and `block` mode refuses.
//!
//! Every secret-shaped string here is built with `concat!`, split inside the provider prefix.

use alice_core::api::*;
use alice_core::embed::{Embedder, FakeEmbedder};
use alice_core::kg_types::AssertFact;
use alice_core::store::memories::{MemoryUpdate, UpdateAction};
use alice_core::store::sources::{ChunkInput, SourceInput};
use alice_daemon::paths::AliceHome;
use alice_daemon::service::{Service, ServiceOptions};
use std::path::Path;
use std::sync::Arc;

const GITHUB: &str = concat!("gh", "p_aBcDeFgHiJkLmNoPqRsTuVwXyZ0123456789");
const ANTHROPIC: &str = concat!("sk-", "ant-api03-abcdefghijklmnopqrstuvwxyz012345");
const PEM: &str = concat!(
    "-----BEGIN RSA PRIVATE ",
    "KEY-----\nMIIEpAIBAAKCAQEA1234\n-----END RSA PRIVATE KEY-----"
);
const URL_WITH_PASSWORD: &str = "postgres://admin:s3cr3tPass@db.internal:5432/app";

/// Pieces of the secrets above that carry no provider prefix: what a scan looks for.
const CORES: &[&str] = &[
    "aBcDeFgHiJkLmNoPqRsTuVwXyZ0123456789",
    "abcdefghijklmnopqrstuvwxyz012345",
    "MIIEpAIBAAKCAQEA1234",
    "s3cr3tPass",
];

async fn start() -> (tempfile::TempDir, Arc<Service>) {
    let d = tempfile::tempdir().unwrap();
    // Default options: no `ALICE_*` or key variable of the shell reaches the service.
    let s = Service::start(
        AliceHome::new(d.path().to_path_buf()),
        &|| Ok(Arc::new(FakeEmbedder::new(384)) as Arc<dyn Embedder>),
        ServiceOptions::default(),
    )
    .unwrap();
    (d, s)
}

fn repo(d: &tempfile::TempDir, name: &str) -> Option<String> {
    let repo = d.path().join(name);
    std::fs::create_dir(&repo).unwrap();
    Some(repo.to_string_lossy().into_owned())
}

fn leaks(text: &str) -> Vec<&'static str> {
    CORES
        .iter()
        .copied()
        .filter(|c| text.contains(c))
        .chain([GITHUB, ANTHROPIC].into_iter().filter(|c| text.contains(c)))
        .collect()
}

/// Every file under `dir` (the database, its WAL, the spool, the logs, the vector files) that
/// holds a secret, byte for byte.
fn files_holding_a_secret(dir: &Path) -> Vec<String> {
    let mut bad = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).into_iter().flatten().flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
                continue;
            }
            let bytes = std::fs::read(&p).unwrap_or_default();
            let hit = CORES
                .iter()
                .chain(&[GITHUB, ANTHROPIC])
                .any(|s| bytes.windows(s.len()).any(|w| w == s.as_bytes()));
            if hit {
                bad.push(p.display().to_string());
            }
        }
    }
    bad
}

fn event_rows(s: &Service) -> String {
    // All of them, as the API lists them.
    let rt = tokio::runtime::Handle::current();
    let rows = tokio::task::block_in_place(|| {
        rt.block_on(s.events(EventsRequest {
            limit: Some(500),
            ..Default::default()
        }))
    })
    .unwrap();
    assert!(!rows.is_empty());
    serde_json::to_string(&rows).unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn every_write_path_redacts_and_nothing_reaches_the_history_the_stream_or_the_disk() {
    let (d, s) = start().await;
    let mut stream = s.subscribe();
    let cwd = repo(&d, "proj");

    // remember: content, title, topics
    let r = s
        .remember(RememberRequest {
            content: format!("deploy key is {GITHUB}"),
            title: Some(format!("key {GITHUB}")),
            topics: vec![GITHUB.to_string(), "deploy".into()],
            cwd: cwd.clone(),
            ..Default::default()
        })
        .await
        .unwrap();
    let id = r.id.clone().unwrap();
    let got = s
        .get(GetRequest {
            ids: vec![id.clone()],
            include_provenance: false,
        })
        .await
        .unwrap();
    let m = &got.memories[0].memory;
    assert!(
        m.body.contains("[REDACTED:github_token]")
            && leaks(&serde_json::to_string(m).unwrap()).is_empty(),
        "{m:?}"
    );

    // update: every patched text field
    let edited = s
        .update(UpdateRequest {
            id: id.clone(),
            update: MemoryUpdate {
                action: UpdateAction::Edit,
                title: Some(format!("rotated {ANTHROPIC}")),
                body: Some(format!("now {ANTHROPIC} and {URL_WITH_PASSWORD}")),
                topics: Some(vec![ANTHROPIC.to_string()]),
                ..Default::default()
            },
            ..Default::default()
        })
        .await
        .unwrap();
    assert!(
        leaks(&serde_json::to_string(&edited).unwrap()).is_empty(),
        "{edited:?}"
    );
    assert!(edited.memory.body.contains("[REDACTED:anthropic_key]"));
    assert!(edited.memory.body.contains("[REDACTED:url_credentials]"));

    // ingest: raw chunks and the source uri
    s.ingest(IngestRequest {
        source: SourceInput {
            uri: format!("{URL_WITH_PASSWORD}/export"),
            ..Default::default()
        },
        chunks: vec![
            ChunkInput {
                ord: 0,
                text: format!("config\n{PEM}\nend {GITHUB}"),
                ..Default::default()
            },
            ChunkInput {
                ord: 1,
                text: "nothing to hide here".into(),
                ..Default::default()
            },
        ],
        cwd: cwd.clone(),
        ..Default::default()
    })
    .await
    .unwrap();

    // assert_fact: subject, predicate, object
    s.assert_fact(AssertFact {
        subject: "ci".into(),
        predicate: "uses".into(),
        object: format!("token {GITHUB}"),
        ..Default::default()
    })
    .await
    .unwrap();

    // Stored rows.
    let c = alice_core::store::open_reader(&s.home().db()).unwrap();
    let dump = |sql: &str| -> String {
        let mut stmt = c.prepare(sql).unwrap();
        let rows: Vec<String> = stmt
            .query_map([], |r| r.get::<_, Option<String>>(0))
            .unwrap()
            .map(|r| r.unwrap().unwrap_or_default())
            .collect();
        rows.join("\n")
    };
    let chunks = dump("SELECT text FROM chunks");
    assert!(chunks.contains("[REDACTED:pem_private_key]") && leaks(&chunks).is_empty());
    let uris = dump("SELECT uri FROM sources");
    assert!(
        uris.contains("[REDACTED:url_credentials]") && leaks(&uris).is_empty(),
        "{uris}"
    );
    let facts = dump(
        "SELECT COALESCE(object_literal, '') || predicate FROM relations
         UNION ALL SELECT name FROM entities",
    );
    assert!(leaks(&facts).is_empty(), "{facts}");
    let topics = dump("SELECT topic FROM memory_topics");
    assert!(leaks(&topics).is_empty(), "{topics}");
    let titles = dump("SELECT title || body FROM memories");
    assert!(leaks(&titles).is_empty(), "{titles}");

    // The history (before and after of every event) and the stream carry none of it either.
    let history = event_rows(&s);
    assert!(leaks(&history).is_empty(), "{history}");
    let mut seen = 0;
    while let Ok(item) = stream.try_recv() {
        seen += 1;
        let json = serde_json::to_string(&item).unwrap();
        assert!(leaks(&json).is_empty(), "{json}");
    }
    assert!(seen >= 3, "the stream carried the events ({seen})");

    // Raw bytes of every file in the home, WAL included.
    s.shutdown();
    assert_eq!(files_holding_a_secret(d.path()), Vec::<String>::new());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_project_file_can_block_and_a_broken_one_fails_closed_keeping_its_patterns() {
    let (d, s) = start().await;
    let cwd = repo(&d, "proj");
    let root = Path::new(cwd.as_deref().unwrap()).to_path_buf();
    let remember = |content: String| {
        let (s, cwd) = (s.clone(), cwd.clone());
        async move {
            s.remember(RememberRequest {
                content,
                cwd,
                ..Default::default()
            })
            .await
        }
    };
    // No file: redact.
    assert!(remember(format!("one {GITHUB}")).await.is_ok());

    // `block_secrets`: refused, naming the detector and not the token; nothing is written.
    std::fs::write(
        root.join(".alice.toml"),
        "[privacy]\nblock_secrets = true\n",
    )
    .unwrap();
    let err = remember(format!("two {GITHUB}"))
        .await
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("github_token") && !err.contains(GITHUB),
        "{err}"
    );
    assert!(remember("plain text is fine".into()).await.is_ok());
    {
        let c = alice_core::store::open_reader(&s.home().db()).unwrap();
        let refused: i64 = c
            .query_row(
                "SELECT count(*) FROM memories WHERE body LIKE 'two%'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(refused, 0, "a refused write stores nothing");
    }

    // A custom pattern in the file applies, and survives a typo in the file.
    std::fs::write(
        root.join(".alice.toml"),
        "[privacy]\nblock_secrets = true\ncustom_patterns = [{ name = \"ticket\", regex = \"TCK-[0-9]{4,}\" }]\n",
    )
    .unwrap();
    let err = remember("see TCK-12345".into())
        .await
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("custom:ticket") && !err.contains("TCK-12345"),
        "{err}"
    );
    std::fs::write(root.join(".alice.toml"), "[privacy\nblock_secrets = tru").unwrap();
    let err = remember("see TCK-12345".into())
        .await
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("custom:ticket"),
        "the last good pattern stays: {err}"
    );
    let err = remember(format!("three {GITHUB}"))
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("github_token"), "a broken file blocks: {err}");
    // The same rules for an edit of a memory of that project, and for a raw ingest.
    std::fs::write(
        root.join(".alice.toml"),
        "[privacy]\nblock_secrets = true\n",
    )
    .unwrap();
    let ok = remember("a clean memory".into()).await.unwrap();
    let err = s
        .update(UpdateRequest {
            id: ok.id.clone().unwrap(),
            update: MemoryUpdate {
                action: UpdateAction::Edit,
                body: Some(format!("now {GITHUB}")),
                ..Default::default()
            },
            ..Default::default()
        })
        .await
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("github_token") && err.contains("body"),
        "{err}"
    );
    let err = s
        .ingest(IngestRequest {
            source: SourceInput {
                uri: "x".into(),
                ..Default::default()
            },
            chunks: vec![
                ChunkInput {
                    ord: 0,
                    text: "fine".into(),
                    ..Default::default()
                },
                ChunkInput {
                    ord: 1,
                    text: format!("late {GITHUB}"),
                    ..Default::default()
                },
            ],
            cwd: cwd.clone(),
            ..Default::default()
        })
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("github_token"), "{err}");
    let c = alice_core::store::open_reader(&s.home().db()).unwrap();
    let chunks: i64 = c
        .query_row("SELECT count(*) FROM chunks", [], |r| r.get(0))
        .unwrap();
    assert_eq!(chunks, 0, "a refused ingest writes none of its chunks");
    s.shutdown();
    assert_eq!(files_holding_a_secret(d.path()), Vec::<String>::new());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_global_block_mode_set_through_the_live_settings_applies_to_the_next_write() {
    let (d, s) = start().await;
    let cwd = repo(&d, "proj");
    let write = || {
        s.remember(RememberRequest {
            content: format!("key {GITHUB}"),
            cwd: cwd.clone(),
            ..Default::default()
        })
    };
    assert!(write().await.is_ok(), "redact mode is the default");
    let mut change = serde_json::Map::new();
    change.insert("privacy.redact_mode".into(), serde_json::json!("block"));
    s.settings_handle().patch(&change).unwrap();
    let err = write().await.unwrap_err().to_string();
    assert!(err.contains("github_token"), "{err}");
    change.insert("privacy.redact_mode".into(), serde_json::json!("redact"));
    s.settings_handle().patch(&change).unwrap();
    assert!(write().await.is_ok(), "and back");
    s.shutdown();
    assert_eq!(files_holding_a_secret(d.path()), Vec::<String>::new());
}

/// A secret recorded in the history before the rules existed does not come back with an undo.
#[tokio::test(flavor = "multi_thread")]
async fn undo_restores_old_text_through_the_current_rules() {
    let (d, s) = start().await;
    let cwd = repo(&d, "proj");
    let root = Path::new(cwd.as_deref().unwrap()).to_path_buf();
    let plant = |c: &rusqlite::Connection, event: &str| {
        // As if the event had been recorded by an older Alice: the secret sits in its before-state.
        let n = c
            .execute(
                "UPDATE events SET before_json = replace(before_json, 'PLANTED-HERE', ?1) WHERE id = ?2",
                rusqlite::params![GITHUB, event],
            )
            .unwrap();
        assert_eq!(n, 1);
    };
    let edit = |id: String, body: &str| {
        let s = s.clone();
        let body = body.to_string();
        async move {
            s.update(UpdateRequest {
                id,
                update: MemoryUpdate {
                    action: UpdateAction::Edit,
                    body: Some(body),
                    ..Default::default()
                },
                ..Default::default()
            })
            .await
            .unwrap()
        }
    };
    let db = alice_core::store::open_writer(&s.home().db()).unwrap();

    // Redact mode: restored with the token replaced.
    let a = s
        .remember(RememberRequest {
            content: "deploy with PLANTED-HERE".into(),
            cwd: cwd.clone(),
            ..Default::default()
        })
        .await
        .unwrap();
    let ev = edit(a.id.clone().unwrap(), "deploy by hand")
        .await
        .event_id
        .unwrap();
    plant(&db, &ev);
    let undone = s
        .undo(UndoRequest {
            event_id: ev,
            ..Default::default()
        })
        .await
        .unwrap();
    assert!(undone.undone, "{undone:?}");
    let body = |id: String| {
        let s = s.clone();
        async move {
            s.get(GetRequest {
                ids: vec![id],
                include_provenance: false,
            })
            .await
            .unwrap()
            .memories[0]
                .memory
                .body
                .clone()
        }
    };
    assert_eq!(
        body(a.id.clone().unwrap()).await,
        "deploy with [REDACTED:github_token]"
    );

    // Block mode in the project: refused, the memory stays as it is.
    let b = s
        .remember(RememberRequest {
            content: "rollout with PLANTED-HERE".into(),
            cwd: cwd.clone(),
            ..Default::default()
        })
        .await
        .unwrap();
    let ev = edit(b.id.clone().unwrap(), "rollout by hand")
        .await
        .event_id
        .unwrap();
    plant(&db, &ev);
    std::fs::write(
        root.join(".alice.toml"),
        "[privacy]\nblock_secrets = true\n",
    )
    .unwrap();
    let err = s
        .undo(UndoRequest {
            event_id: ev,
            ..Default::default()
        })
        .await
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("github_token") && !err.contains(GITHUB),
        "{err}"
    );
    assert_eq!(body(b.id.unwrap()).await, "rollout by hand");
    drop(db);
    s.shutdown();
    let history = {
        let c = alice_core::store::open_reader(&s.home().db()).unwrap();
        let mut stmt = c
            .prepare("SELECT COALESCE(before_json,'') || COALESCE(after_json,'') FROM events WHERE action LIKE 'undo:%'")
            .unwrap();
        stmt.query_map([], |r| r.get::<_, String>(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect::<Vec<_>>()
            .join("\n")
    };
    assert!(
        !history.is_empty() && leaks(&history).is_empty(),
        "{history}"
    );
}

/// A row stored before the rules existed still holds its secret; the history of a later change
/// to it and the stream carry the snapshot redacted.
#[tokio::test(flavor = "multi_thread")]
async fn snapshots_of_an_old_row_are_redacted_in_the_history_and_the_stream() {
    let (d, s) = start().await;
    let cwd = repo(&d, "proj");
    let a = s
        .remember(RememberRequest {
            content: "deploy with PLANTED-HERE".into(),
            cwd,
            ..Default::default()
        })
        .await
        .unwrap();
    {
        let db = alice_core::store::open_writer(&s.home().db()).unwrap();
        db.execute(
            "UPDATE memories SET body = replace(body, 'PLANTED-HERE', ?1)",
            [GITHUB],
        )
        .unwrap();
    }
    let mut stream = s.subscribe();
    s.update(UpdateRequest {
        id: a.id.unwrap(),
        update: MemoryUpdate {
            action: UpdateAction::Edit,
            body: Some("deploy by hand".into()),
            ..Default::default()
        },
        ..Default::default()
    })
    .await
    .unwrap();
    let history = event_rows(&s);
    assert!(
        history.contains("[REDACTED:github_token]") && leaks(&history).is_empty(),
        "{history}"
    );
    let item = serde_json::to_string(&stream.try_recv().unwrap()).unwrap();
    assert!(
        item.contains("[REDACTED:github_token]") && leaks(&item).is_empty(),
        "{item}"
    );
    s.shutdown();
    // Only the row the test planted itself still holds the token; nothing else does.
    let bad = files_holding_a_secret(d.path());
    assert!(
        bad.iter()
            .all(|f| f.ends_with("alice.db") || f.ends_with("alice.db-wal")),
        "{bad:?}"
    );
}

/// A fact that cites a memory of a blocking project is judged by that project's rules.
#[tokio::test(flavor = "multi_thread")]
async fn assert_fact_follows_the_rules_of_the_project_of_its_source_memory() {
    let (d, s) = start().await;
    let cwd = repo(&d, "proj");
    let root = Path::new(cwd.as_deref().unwrap()).to_path_buf();
    let m = s
        .remember(RememberRequest {
            content: "the ci setup".into(),
            cwd: cwd.clone(),
            ..Default::default()
        })
        .await
        .unwrap();
    std::fs::write(
        root.join(".alice.toml"),
        "[privacy]\ncustom_patterns = [{ name = \"ticket\", regex = \"TCK-[0-9]{4,}\" }]\nblock_secrets = true\n",
    )
    .unwrap();
    let fact = |object: &str, source: Option<String>| AssertFact {
        subject: "ci".into(),
        predicate: "tracks".into(),
        object: object.into(),
        source_memory_id: source,
        ..Default::default()
    };
    let err = s
        .assert_fact(fact("TCK-12345", m.id.clone()))
        .await
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("custom:ticket") && err.contains("object"),
        "{err}"
    );
    // Without a source memory the global rules apply (redact mode): the fact is stored cleaned.
    let ok = s.assert_fact(fact("TCK-12345", None)).await;
    assert!(ok.is_ok(), "{ok:?}");
    s.shutdown();
}

/// The live project's `.alice.toml` applies after the working tree moved: a re-clone keeps the
/// project (same remote) but has another root.
#[tokio::test(flavor = "multi_thread")]
async fn a_moved_working_tree_brings_its_project_rules_to_update_and_undo() {
    fn git(dir: &Path, args: &[&str]) {
        let ok = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        assert!(ok, "git {args:?}");
    }
    let (d, s) = start().await;
    let (a, b) = (d.path().join("clone-a"), d.path().join("clone-b"));
    for dir in [&a, &b] {
        std::fs::create_dir(dir).unwrap();
        git(dir, &["init", "-q"]);
        git(
            dir,
            &["remote", "add", "origin", "https://example.com/me/app.git"],
        );
    }
    let remember = |dir: &Path, content: &str| {
        let (s, cwd) = (s.clone(), dir.to_string_lossy().into_owned());
        let content = content.to_string();
        async move {
            s.remember(RememberRequest {
                content,
                cwd: Some(cwd),
                ..Default::default()
            })
            .await
            .unwrap()
        }
    };
    let first = remember(&a, "first, from the old clone").await;
    std::fs::write(
        b.join(".alice.toml"),
        "[privacy]\ncustom_patterns = [{ name = \"ticket\", regex = \"TCK-[0-9]{4,}\" }]\n",
    )
    .unwrap();
    let second = remember(&b, "second, from the new clone").await;
    assert_eq!(first.project, second.project, "one project, two roots");
    let edited = s
        .update(UpdateRequest {
            id: first.id.unwrap(),
            update: MemoryUpdate {
                action: UpdateAction::Edit,
                body: Some("see TCK-12345".into()),
                ..Default::default()
            },
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(edited.memory.body, "see [REDACTED:custom:ticket]");
    s.shutdown();
}

/// A review item the way the judge will leave it (Task 12): written straight into the inbox.
fn add_review(
    s: &Service,
    kind: alice_core::store::reviews::ReviewType,
    payload: serde_json::Value,
    key: &str,
) -> String {
    let c = alice_core::store::open_writer(&s.home().db()).unwrap();
    alice_core::store::reviews::create(
        &c,
        &alice_core::store::reviews::NewReview {
            kind,
            payload,
            confidence: Some(0.8),
            dedupe_key: Some(key.into()),
        },
    )
    .unwrap()
    .unwrap()
}

fn resolve(s: &Service, id: &str, choice: &str) -> alice_core::error::Result<ResolveResponse> {
    let rt = tokio::runtime::Handle::current();
    tokio::task::block_in_place(|| {
        rt.block_on(s.resolve_review(ResolveRequest {
            id: id.into(),
            choice: choice.into(),
            ..Default::default()
        }))
    })
}

/// R2: "undo anyway" is a restore path like any other: it skips the conflict check, never the
/// privacy rules. Redact mode restores the redacted form; block mode refuses and names the
/// detector, and the item stays open.
#[tokio::test(flavor = "multi_thread")]
async fn applying_an_undo_item_goes_through_the_privacy_rules() {
    let (_d, s) = start().await;
    let r = s
        .remember(RememberRequest {
            content: "deploy with PLANTED-HERE".into(),
            ..Default::default()
        })
        .await
        .unwrap();
    let id = r.id.unwrap();
    let edit = |body: &str| UpdateRequest {
        id: id.clone(),
        update: MemoryUpdate {
            action: UpdateAction::Edit,
            body: Some(body.into()),
            ..Default::default()
        },
        ..Default::default()
    };
    let first = s.update(edit("deploy by hand")).await.unwrap();
    let first_event = first.event_id.unwrap();
    // As if an older Alice had recorded the event: the secret sits in its before-state.
    {
        let c = alice_core::store::open_writer(&s.home().db()).unwrap();
        let n = c
            .execute(
                "UPDATE events SET before_json = replace(before_json, 'PLANTED-HERE', ?1) WHERE id = ?2",
                rusqlite::params![GITHUB, first_event],
            )
            .unwrap();
        assert_eq!(n, 1);
    }
    s.update(edit("deploy by pipeline")).await.unwrap();
    let u = s
        .undo(UndoRequest {
            event_id: first_event,
            ..Default::default()
        })
        .await
        .unwrap();
    let review_id = u.review_id.expect("the target changed since: an item asks");

    let mut block = serde_json::Map::new();
    block.insert("privacy.redact_mode".into(), serde_json::json!("block"));
    s.settings_handle().patch(&block).unwrap();
    let err = resolve(&s, &review_id, "apply").unwrap_err().to_string();
    assert!(
        err.contains("github_token") && err.contains("cannot undo") && !err.contains(GITHUB),
        "{err}"
    );
    let item = s.review(review_id.clone()).await.unwrap();
    assert_eq!(item.status, alice_core::store::reviews::ReviewStatus::Open);
    let body = |s: &Service| {
        let c = alice_core::store::open_reader(&s.home().db()).unwrap();
        c.query_row("SELECT body FROM memories WHERE id = ?1", [&id], |r| {
            r.get::<_, String>(0)
        })
        .unwrap()
    };
    assert_eq!(body(&s), "deploy by pipeline", "nothing was restored");

    let mut redact = serde_json::Map::new();
    redact.insert("privacy.redact_mode".into(), serde_json::json!("redact"));
    s.settings_handle().patch(&redact).unwrap();
    resolve(&s, &review_id, "apply").unwrap();
    assert_eq!(body(&s), "deploy with [REDACTED:github_token]");
    // The planted row is the only one that holds the secret: what the resolution recorded is clean.
    let c = alice_core::store::open_reader(&s.home().db()).unwrap();
    let recorded: Vec<String> = c
        .prepare("SELECT COALESCE(before_json, '') || COALESCE(after_json, '') FROM events WHERE action LIKE 'undo%' OR action = 'review.resolve'")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .map(|r| r.unwrap())
        .collect();
    assert!(recorded.len() >= 3, "{recorded:?}");
    assert!(
        recorded.iter().all(|r| leaks(r).is_empty()),
        "no secret reaches the history the resolution wrote"
    );
}

/// R2: the names a mapped predicate writes go through the privacy rules before the writer; a
/// memory moved into a project whose rules would change its text is refused, not moved.
#[tokio::test(flavor = "multi_thread")]
async fn resolutions_never_write_text_the_privacy_rules_would_change() {
    use alice_core::store::reviews::ReviewType;
    let (d, s) = start().await;
    s.assert_fact(AssertFact {
        subject: "alice".into(),
        predicate: "use".into(),
        object: "Rust".into(),
        ..Default::default()
    })
    .await
    .unwrap();
    let item = add_review(
        &s,
        ReviewType::Predicate,
        serde_json::json!({"predicate": format!("uses {GITHUB}"), "canonical": "use", "p": 0.9, "provider": "heuristic"}),
        "p1",
    );
    let predicates = |s: &Service| -> String {
        let c = alice_core::store::open_reader(&s.home().db()).unwrap();
        let mut stmt = c
            .prepare("SELECT name || ' ' || COALESCE(canonical, '') FROM predicates UNION ALL SELECT COALESCE(before_json, '') || COALESCE(after_json, '') || target_id FROM events")
            .unwrap();
        stmt.query_map([], |r| r.get::<_, String>(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect::<Vec<_>>()
            .join("\n")
            .to_lowercase()
    };
    // Block mode: refused naming the detector; the item stays open and nothing is stored.
    let mut block = serde_json::Map::new();
    block.insert("privacy.redact_mode".into(), serde_json::json!("block"));
    s.settings_handle().patch(&block).unwrap();
    let err = resolve(&s, &item, "map").unwrap_err().to_string();
    assert!(
        err.contains("github_token") && !err.contains(GITHUB),
        "{err}"
    );
    assert!(!predicates(&s).contains("abcdefghijklmnopqrstuvwxyz0123456789"));
    // Redact mode: the redacted name is what becomes vocabulary.
    let mut redact = serde_json::Map::new();
    redact.insert("privacy.redact_mode".into(), serde_json::json!("redact"));
    s.settings_handle().patch(&redact).unwrap();
    resolve(&s, &item, "map").unwrap();
    let stored = predicates(&s);
    assert!(
        !stored.contains("abcdefghijklmnopqrstuvwxyz0123456789") && stored.contains("redacted"),
        "{stored}"
    );

    // A move into a project with a stricter rule.
    let loose = repo(&d, "loose");
    let strict_dir = d.path().join("strict");
    std::fs::create_dir(&strict_dir).unwrap();
    std::fs::write(
        strict_dir.join(".alice.toml"),
        "[privacy]\ncustom_patterns = [{ name = \"ticket\", regex = \"TCK-[0-9]{4,}\" }]\n",
    )
    .unwrap();
    // Written before the rule existed: see TCK-12345.
    let m = s
        .remember(RememberRequest {
            content: "see TCK-12345 for details".into(),
            cwd: loose,
            ..Default::default()
        })
        .await
        .unwrap();
    let _ = s
        .remember(RememberRequest {
            content: "something in the strict project".into(),
            cwd: Some(strict_dir.to_string_lossy().into_owned()),
            ..Default::default()
        })
        .await
        .unwrap();
    let slugs: Vec<String> = s
        .projects()
        .await
        .unwrap()
        .into_iter()
        .map(|p| p.slug)
        .collect();
    let strict = slugs
        .iter()
        .find(|p| p.starts_with("strict"))
        .expect("a strict project")
        .clone();
    let mv = add_review(
        &s,
        ReviewType::Project,
        serde_json::json!({"memory_id": m.id.clone().unwrap(), "from": "loose", "to": strict, "p": 0.8, "provider": "heuristic"}),
        "mv",
    );
    // Block mode: refused naming the detector, and the memory stays where it was.
    let mut block = serde_json::Map::new();
    block.insert("privacy.redact_mode".into(), serde_json::json!("block"));
    s.settings_handle().patch(&block).unwrap();
    let err = resolve(&s, &mv, "move").unwrap_err().to_string();
    assert!(
        err.contains("custom:ticket") && err.contains("cannot move") && !err.contains("TCK-12345"),
        "{err}"
    );
    let item = s.review(mv.clone()).await.unwrap();
    assert_eq!(item.status, alice_core::store::reviews::ReviewStatus::Open);
    let where_is = |s: &Service| -> (String, String) {
        let c = alice_core::store::open_reader(&s.home().db()).unwrap();
        c.query_row(
            "SELECT p.slug, m.body FROM memories m JOIN projects p ON p.id = m.project_id WHERE m.id = ?1",
            [m.id.as_ref().unwrap()],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap()
    };
    assert_ne!(where_is(&s).0, strict, "the memory stayed where it was");
    // Redact mode: moved, and re-redacted with the destination's rules in the same group.
    s.settings_handle().patch(&redact).unwrap();
    resolve(&s, &mv, "move").unwrap();
    let (slug, body) = where_is(&s);
    assert_eq!(slug, strict);
    assert_eq!(body, "see [REDACTED:custom:ticket] for details");
    let c = alice_core::store::open_reader(&s.home().db()).unwrap();
    let actions: String = c
        .query_row(
            "SELECT group_concat(action, ',') FROM (SELECT action FROM events WHERE group_id = (SELECT group_id FROM events WHERE action = 'memory.move') ORDER BY rowid)",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(actions, "memory.move,memory.edit");
    // Undo of the move restores the original text and the project.
    let ev: String = c
        .query_row(
            "SELECT id FROM events WHERE action = 'memory.move'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let u = s
        .undo(UndoRequest {
            event_id: ev,
            ..Default::default()
        })
        .await
        .unwrap();
    assert!(u.undone, "{}", u.message);
    let (slug, body) = where_is(&s);
    assert_ne!(slug, strict);
    assert_eq!(body, "see TCK-12345 for details");
    // A move into a project with no such rule goes through.
    let other = add_review(
        &s,
        ReviewType::Project,
        serde_json::json!({"memory_id": m.id.clone().unwrap(), "from": &where_is(&s).0, "to": "global", "p": 0.8, "provider": "heuristic"}),
        "mv2",
    );
    resolve(&s, &other, "move").unwrap();
}
