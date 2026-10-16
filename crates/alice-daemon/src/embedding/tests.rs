use super::testkit::*;
use super::*;
use crate::service::Service;
use alice_core::api::{IngestRequest, MatchVia, OutputFormat, RecallRequest, RememberRequest};
use alice_core::embed::FakeEmbedder;
use alice_core::store::memories::{MemoryUpdate, UpdateAction};
use alice_core::store::sources::{ChunkInput, SourceInput};
use std::sync::atomic::AtomicUsize;

fn remote_toml(mock: &Mock, model: &str, extra: &str) -> String {
    format!(
        "[embedding]\nprovider = \"openai\"\nmodel = \"{model}\"\nendpoint = \"{}\"\n{extra}",
        mock.base
    )
}

async fn state_of(s: &Service, content_word: &str) -> String {
    let like = format!("%{content_word}%");
    s.read(move |c| {
        Ok(c.query_row(
            "SELECT embed_state FROM memories WHERE body LIKE ?1",
            [&like],
            |r| r.get(0),
        )?)
    })
    .await
    .unwrap()
}

async fn chunk_states(s: &Service) -> Vec<String> {
    s.read(|c| {
        let mut stmt = c.prepare("SELECT embed_state FROM chunks ORDER BY seq")?;
        Ok(stmt
            .query_map([], |r| r.get::<_, String>(0))?
            .collect::<std::result::Result<_, _>>()?)
    })
    .await
    .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_denied_projects_text_never_reaches_a_remote_provider() {
    let mock = Mock::start();
    let d = tempfile::tempdir().unwrap();
    let toml = remote_toml(
        &mock,
        "m",
        "[privacy]\ncloud_denied_projects = [\"secret-proj\"]\n",
    );
    let s = start(d.path(), options(http_factory(true, Arc::default())), &toml);
    until("the provider's embedder to be built", 15, || async {
        s.model() == "openai:m"
    })
    .await;
    let (open, secret) = (repo(&d, "open-proj"), repo(&d, "secret-proj"));
    for (cwd, text) in [
        (&open, "the pelican nests by the harbour"),
        (&secret, "the narwhal tusk is in the vault"),
    ] {
        s.remember(RememberRequest {
            content: text.into(),
            cwd: cwd.clone(),
            ..Default::default()
        })
        .await
        .unwrap();
    }
    for (cwd, text) in [
        (&open, "a transcript line about a hamster wheel"),
        (&secret, "a transcript line about a walrus colony"),
    ] {
        s.ingest(IngestRequest {
            source: SourceInput {
                uri: format!("s-{text}"),
                ..Default::default()
            },
            chunks: vec![ChunkInput {
                ord: 0,
                text: text.into(),
                ..Default::default()
            }],
            cwd: cwd.clone(),
            ..Default::default()
        })
        .await
        .unwrap();
    }
    settled(&s).await;
    // The open project's text went; the denied project's did not, anywhere.
    assert!(
        mock.saw("pelican") && mock.saw("hamster"),
        "{:?}",
        mock.texts()
    );
    for secret_word in ["narwhal", "walrus", "vault"] {
        assert!(!mock.saw(secret_word), "{secret_word} left the machine");
    }
    assert_eq!(state_of(&s, "narwhal").await, "skipped_cloud");
    assert_eq!(state_of(&s, "pelican").await, "done");
    let chunks = chunk_states(&s).await;
    assert!(
        chunks.contains(&"skipped_cloud".to_string()) && chunks.contains(&"done".to_string()),
        "{chunks:?}"
    );
    let h = s.health().await.unwrap();
    assert_eq!((h.embed_skipped_cloud, h.embed_backlog), (2, 0));

    // A recall in the denied project is lexical only, says so, and its query is not sent.
    let sent = mock.texts().len();
    let r = s
        .recall(RecallRequest {
            query: "narwhal tusk".into(),
            cwd: secret.clone(),
            format: OutputFormat::Json,
            ..Default::default()
        })
        .await
        .unwrap();
    assert!(
        r.results.iter().any(|h| h.title.contains("narwhal")),
        "words still find it"
    );
    assert!(
        r.results
            .iter()
            .all(|h| !h.matched.contains(&MatchVia::Vector)),
        "no vector search"
    );
    assert!(
        r.notes.iter().any(|n| n.contains("word matches only")),
        "{:?}",
        r.notes
    );
    assert_eq!(mock.texts().len(), sent, "the query never left");
    assert!(!mock.saw("narwhal tusk"));
    let text = s
        .recall(RecallRequest {
            query: "narwhal tusk".into(),
            cwd: secret.clone(),
            format: OutputFormat::Text,
            ..Default::default()
        })
        .await
        .unwrap();
    assert!(text.text.unwrap().contains("word matches only"));

    // A recall in the open project sends its query, with secrets in it redacted.
    let token = concat!("gh", "p_abcdefghijklmnopqrstuvwxyz0123456789");
    let r = s
        .recall(RecallRequest {
            query: format!("where does the pelican nest {token}"),
            cwd: open.clone(),
            format: OutputFormat::Json,
            ..Default::default()
        })
        .await
        .unwrap();
    assert!(r.notes.is_empty(), "{:?}", r.notes);
    assert!(
        mock.saw("where does the pelican nest"),
        "the query was sent"
    );
    assert!(
        !mock.saw(token),
        "the secret in the query was redacted before it left"
    );

    // The policy changes: the denied project is let through and its rows are embedded.
    let mut m = serde_json::Map::new();
    m.insert(
        "privacy.cloud_denied_projects".into(),
        serde_json::json!([]),
    );
    s.patch_settings(m).unwrap();
    until("the held-back rows to be embedded", 15, || async {
        s.health().await.unwrap().embed_skipped_cloud == 0 && mock.saw("narwhal")
    })
    .await;
    settled(&s).await;
    assert_eq!(state_of(&s, "narwhal").await, "done");
    assert!(mock.saw("walrus"));
    // And the key reached the provider only as a bearer header.
    let heads = mock.heads().join("\n").to_ascii_lowercase();
    assert!(
        heads.contains(&format!(
            "authorization: bearer {}",
            TEST_KEY.to_ascii_lowercase()
        )) && heads.contains(&format!("user-agent: alice/{}", env!("CARGO_PKG_VERSION")))
    );
    s.shutdown();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_loopback_ollama_endpoint_is_local_and_needs_no_cloud_policy() {
    let mock = Mock::start();
    let d = tempfile::tempdir().unwrap();
    let toml = format!(
        "[embedding]\nprovider = \"ollama\"\nmodel = \"nomic\"\nendpoint = \"{}\"\n[privacy]\ncloud_denied_projects = [\"secret-proj\"]\n",
        mock.base
    );
    // No forced "remote": the real rule decides.
    let s = start(
        d.path(),
        options(http_factory(false, Arc::default())),
        &toml,
    );
    until("the provider's embedder to be built", 15, || async {
        s.model() == "ollama:nomic"
    })
    .await;
    assert!(!s.embedder().is_remote());
    s.remember(RememberRequest {
        content: "the narwhal tusk is in the vault".into(),
        cwd: repo(&d, "secret-proj"),
        ..Default::default()
    })
    .await
    .unwrap();
    settled(&s).await;
    assert!(
        mock.saw("narwhal"),
        "a local server gets every project's text"
    );
    assert_eq!(state_of(&s, "narwhal").await, "done");
    let r = s
        .recall(RecallRequest {
            query: "narwhal".into(),
            cwd: repo(&d, "secret-proj"),
            format: OutputFormat::Json,
            ..Default::default()
        })
        .await
        .unwrap();
    assert!(r.notes.is_empty(), "{:?}", r.notes);
    s.shutdown();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_degraded_configuration_sends_no_text_to_a_remote_provider() {
    let mock = Mock::start();
    let d = tempfile::tempdir().unwrap();
    // A wrong type inside the privacy section: the file is salvaged, but it may have lost
    // a privacy setting, so the configuration counts as degraded (and no cloud is used).
    let toml = remote_toml(&mock, "m", "[privacy]\ncloud_denied_projects = 5\n");
    let s = start(d.path(), options(http_factory(true, Arc::default())), &toml);
    assert!(s.settings_handle().config_degraded());
    s.remember(RememberRequest {
        content: "the heron stands in the reeds".into(),
        ..Default::default()
    })
    .await
    .unwrap();
    settled(&s).await;
    assert!(!mock.saw("heron"), "{:?}", mock.texts());
    assert_eq!(state_of(&s, "heron").await, "skipped_cloud");
    let r = s
        .recall(RecallRequest {
            query: "heron".into(),
            format: OutputFormat::Json,
            ..Default::default()
        })
        .await
        .unwrap();
    assert!(r.notes.iter().any(|n| n.contains("word matches only")));
    assert!(!mock.saw("heron"));
    s.shutdown();
}

/// A factory whose remote embedders are `ForceRemote` fakes of 64 dimensions that count
/// the texts they are given.
fn remote_fake(texts: Arc<AtomicUsize>) -> alice_embed::EmbedderFactory {
    counting_factory(texts, Duration::ZERO, true)
}

#[tokio::test(flavor = "multi_thread")]
async fn a_change_to_a_remote_provider_states_what_will_be_sent_and_honours_the_policy() {
    let d = tempfile::tempdir().unwrap();
    let texts = Arc::new(AtomicUsize::new(0));
    let s = start(
        d.path(),
        options(remote_fake(texts.clone())),
        "[privacy]\ncloud_denied_projects = [\"secret-proj\"]\n",
    );
    let (open, secret) = (repo(&d, "open-proj"), repo(&d, "secret-proj"));
    for i in 0..3 {
        s.remember(RememberRequest {
            content: format!("open memory {i}"),
            cwd: open.clone(),
            ..Default::default()
        })
        .await
        .unwrap();
    }
    for i in 0..2 {
        s.remember(RememberRequest {
            content: format!("secret memory {i}"),
            cwd: secret.clone(),
            ..Default::default()
        })
        .await
        .unwrap();
    }
    s.ingest(IngestRequest {
        source: SourceInput {
            uri: "s".into(),
            ..Default::default()
        },
        chunks: (0..4)
            .map(|i| ChunkInput {
                ord: i,
                text: format!("open chunk {i}"),
                ..Default::default()
            })
            .collect(),
        cwd: open.clone(),
        ..Default::default()
    })
    .await
    .unwrap();
    settled(&s).await;
    let mut m = serde_json::Map::new();
    m.insert("embedding.provider".into(), serde_json::json!("openai"));
    m.insert(
        "embedding.model".into(),
        serde_json::json!("text-embedding-3-small"),
    );
    let out = s.patch_settings(m).unwrap();
    let note = out
        .notes
        .iter()
        .find(|n| n.contains("will be sent to api.openai.com"))
        .unwrap_or_else(|| panic!("{:?}", out.notes));
    assert!(
        note.contains("about 3 memories and 4 chunks")
            && note.contains("2 memories and 0 chunks")
            && note.contains("cloud policy")
            && note.contains("answers searches until the new index is complete"),
        "{note}"
    );
    // The same change is also announced as a loosened protection.
    assert!(
        out.notes
            .iter()
            .any(|n| n.contains("embedding.provider") && n.contains("loosened")),
        "{:?}",
        out.notes
    );
    until("the switch", 15, || async {
        s.health().await.unwrap().embedding_model == "openai:text-embedding-3-small"
    })
    .await;
    let model = "openai:text-embedding-3-small";
    let count = |sql: &'static str| {
        let s = s.clone();
        async move {
            s.read(move |c| Ok(c.query_row(sql, [], |r| r.get::<_, i64>(0))?))
                .await
                .unwrap()
        }
    };
    assert_eq!(
        count("SELECT count(*) FROM embeddings WHERE model = 'openai:text-embedding-3-small'")
            .await,
        7,
        "3 memories + 4 chunks of the open project"
    );
    assert_eq!(
        texts.load(Ordering::SeqCst),
        7,
        "nothing of the denied project was embedded"
    );
    assert_eq!(state_of(&s, "secret memory 0").await, "skipped_cloud");
    assert_eq!(state_of(&s, "open memory 0").await, "done");
    assert_eq!(s.model(), model);
    s.shutdown();
}

#[tokio::test(flavor = "multi_thread")]
async fn going_back_to_a_local_model_queues_the_held_back_rows_again() {
    let d = tempfile::tempdir().unwrap();
    let texts = Arc::new(AtomicUsize::new(0));
    let toml = "[embedding]\nprovider = \"openai\"\nmodel = \"m\"\n[privacy]\ncloud_denied_projects = [\"secret-proj\"]\n";
    let s = start(d.path(), options(remote_fake(texts)), toml);
    until("the provider's embedder to be built", 15, || async {
        s.model() == "openai:m"
    })
    .await;
    s.remember(RememberRequest {
        content: "secret memory".into(),
        cwd: repo(&d, "secret-proj"),
        ..Default::default()
    })
    .await
    .unwrap();
    settled(&s).await;
    assert_eq!(state_of(&s, "secret memory").await, "skipped_cloud");
    let mut m = serde_json::Map::new();
    m.insert("embedding.provider".into(), serde_json::json!("local"));
    m.insert("embedding.model".into(), serde_json::json!(MODEL_ID));
    s.patch_settings(m).unwrap();
    until("the switch back", 15, || async {
        s.health().await.unwrap().embedding_model == MODEL_ID
    })
    .await;
    until("the row to be embedded locally", 15, || async {
        state_of(&s, "secret memory").await == "done"
    })
    .await;
    s.shutdown();
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unreachable_provider_at_start_keeps_the_daemon_up_and_heals() {
    let d = tempfile::tempdir().unwrap();
    let up = Arc::new(AtomicBool::new(true));
    let factory: alice_embed::EmbedderFactory = {
        let up = up.clone();
        Arc::new(move |s: &EmbeddingSettings, _key| {
            let id = expected_model_id(s);
            if id != MODEL_ID && !up.load(Ordering::SeqCst) {
                return Err(alice_core::AliceError::Other(
                    "cannot reach the provider".into(),
                ));
            }
            Ok(Arc::new(FakeEmbedder::named(
                id.clone(),
                if id == MODEL_ID { 384 } else { 64 },
            )) as Arc<dyn Embedder>)
        })
    };
    let toml = "[embedding]\nprovider = \"ollama\"\nmodel = \"m\"\n";
    let s = start(d.path(), options(factory.clone()), toml);
    until("the provider's embedder to be built", 15, || async {
        s.model() == "ollama:m"
    })
    .await;
    s.remember(RememberRequest {
        content: "the otter slides down the bank".into(),
        ..Default::default()
    })
    .await
    .unwrap();
    settled(&s).await;
    s.shutdown();
    drop(s);

    up.store(false, Ordering::SeqCst);
    let s = start(d.path(), options(factory), "");
    assert_eq!(s.model(), "ollama:m", "its vectors stay in use");
    until("health to name the cause", 15, || async {
        s.health()
            .await
            .unwrap()
            .embedding_error
            .is_some_and(|e| e.contains("cannot"))
    })
    .await;
    // Searching still works (words), and a new memory waits instead of failing.
    let r = s
        .recall(RecallRequest {
            query: "otter".into(),
            format: OutputFormat::Json,
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(r.results.len(), 1);
    s.remember(RememberRequest {
        content: "the beaver builds a lodge".into(),
        ..Default::default()
    })
    .await
    .unwrap();
    until("the new memory to be queued for a retry", 10, || async {
        state_of(&s, "beaver").await == "retry"
    })
    .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        state_of(&s, "beaver").await,
        "retry",
        "queued, not given up"
    );
    // The provider comes back: the stand-in is replaced and the queue drains.
    up.store(true, Ordering::SeqCst);
    until("the provider to be used again", 15, || async {
        let h = s.health().await.unwrap();
        h.embedding_error.is_none() && h.embed_backlog == 0
    })
    .await;
    assert_eq!(state_of(&s, "beaver").await, "done");
    s.shutdown();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_key_set_later_switches_to_the_provider_and_a_rotated_key_is_used_without_leaking() {
    let d = tempfile::tempdir().unwrap();
    let keys: Arc<Mutex<Vec<Option<String>>>> = Arc::default();
    let factory: alice_embed::EmbedderFactory = {
        let keys = keys.clone();
        Arc::new(move |s: &EmbeddingSettings, key| {
            keys.lock()
                .push(key.as_ref().map(|k| k.expose().to_string()));
            let id = expected_model_id(s);
            if id != MODEL_ID && key.is_none() {
                return Err(alice_core::AliceError::invalid(
                    "the openai embedding provider needs an API key; set it with `alice config set-secret embedding_api_key`",
                ));
            }
            Ok(Arc::new(FakeEmbedder::named(
                id.clone(),
                if id == MODEL_ID { 384 } else { 64 },
            )) as Arc<dyn Embedder>)
        })
    };
    let mut opts = options(factory);
    opts.secret_env = Some(Arc::new(|_: &str| None)); // no key anywhere, until one is set
    let toml = "[embedding]\nprovider = \"openai\"\nmodel = \"m\"\n";
    let s = start(d.path(), opts, toml);
    // No key: nothing answers yet (a stand-in holds the place), and health says what to do.
    until("health to name the missing key", 15, || async {
        s.health()
            .await
            .unwrap()
            .embedding_error
            .is_some_and(|e| e.contains("embedding_api_key"))
    })
    .await;
    assert_eq!(s.model(), crate::embedding::PLACEHOLDER_MODEL);
    s.remember(RememberRequest {
        content: "a memory before the key".into(),
        ..Default::default()
    })
    .await
    .unwrap();
    let key1 = concat!("sk-", "first-key-0123456789abcdef");
    s.set_secret_async(crate::secrets::SecretName::EmbeddingApiKey, key1.into())
        .await
        .unwrap();
    until("the switch to the provider", 15, || async {
        s.health().await.unwrap().embedding_model == "openai:m"
    })
    .await;
    assert!(keys.lock().iter().any(|k| k.as_deref() == Some(key1)));
    // Rotating the key builds the embedder again with the new one.
    let key2 = concat!("sk-", "second-key-0123456789abcdef");
    let before = keys.lock().len();
    s.set_secret_async(crate::secrets::SecretName::EmbeddingApiKey, key2.into())
        .await
        .unwrap();
    until("the rotated key to be used", 15, || async {
        keys.lock()[before..]
            .iter()
            .any(|k| k.as_deref() == Some(key2))
    })
    .await;
    // Deleting it stops anything being sent: the stand-in answers "unavailable".
    s.delete_secret_async(crate::secrets::SecretName::EmbeddingApiKey)
        .await
        .unwrap();
    until("the stand-in after the key was deleted", 15, || async {
        s.health()
            .await
            .unwrap()
            .embedding_error
            .is_some_and(|e| e.contains("embedding_api_key"))
    })
    .await;
    assert!(s.embedder().embed_passages(&["x".into()]).is_err());
    // The keys are in no answer the daemon gives.
    let health = serde_json::to_string(&s.health().await.unwrap()).unwrap();
    let view = serde_json::to_string(&s.settings_view()).unwrap();
    for out in [health, view] {
        assert!(
            !out.contains(key1) && !out.contains(key2),
            "a key leaked: {out}"
        );
    }
    s.shutdown();
}
/// H2: a switch to a remote provider typed into config.toml is a loosening, so it waits to
/// be read the same on three polls in a row (a half-written save never sends text away).
#[tokio::test(flavor = "multi_thread")]
async fn editing_the_file_to_a_remote_provider_waits_for_the_loosening_ratchet() {
    let d = tempfile::tempdir().unwrap();
    let mut opts = options(fake_factory());
    // Slow enough that the wait for the first read cannot overshoot the two reads before the
    // switch (a loaded runner may stall a thread for a few hundred milliseconds).
    opts.settings_poll = Duration::from_millis(500);
    let s = start(d.path(), opts, "");
    let home = AliceHome::new(d.path().to_path_buf());
    std::fs::write(
        home.config_file(),
        "[embedding]\nprovider = \"voyage\"\nmodel = \"voyage-3-lite\"\n",
    )
    .unwrap();
    // Read once: not in force yet, and the settings view says why. Waited for, not slept for.
    until("the first read of the edit", 15, || async {
        s.settings_handle().loosening_waiting()
    })
    .await;
    assert_eq!(s.settings().embedding.provider, EmbeddingProvider::Local);
    assert_eq!(s.model(), MODEL_ID);
    until("the third identical poll", 10, || async {
        s.settings().embedding.provider == EmbeddingProvider::Voyage
    })
    .await;
    until("the switch", 15, || async {
        s.health().await.unwrap().embedding_model == "voyage:voyage-3-lite"
    })
    .await;
    s.shutdown();
}

fn forget_or_restore(id: &str, action: UpdateAction) -> alice_core::api::UpdateRequest {
    alice_core::api::UpdateRequest {
        id: id.into(),
        update: MemoryUpdate {
            action,
            ..Default::default()
        },
        ..Default::default()
    }
}

/// C1: the cloud policy is asked again before every attempt. A project denied while the
/// provider was down is not sent when it is back, whichever of the two workers had its text.
#[tokio::test(flavor = "multi_thread")]
async fn a_project_denied_while_the_provider_is_down_is_never_sent_after_it() {
    let d = tempfile::tempdir().unwrap();
    let up = Arc::new(AtomicBool::new(true));
    let seen: Arc<Mutex<Vec<String>>> = Arc::default();
    let s = start(
        d.path(),
        options(gate_factory(up.clone(), seen.clone())),
        "",
    );
    let (open, secret) = (repo(&d, "open-proj"), repo(&d, "secret-proj"));
    for (cwd, text) in [
        (&open, "the pelican nests by the harbour"),
        (&secret, "the narwhal tusk is in the vault"),
    ] {
        s.remember(RememberRequest {
            content: text.into(),
            cwd: cwd.clone(),
            ..Default::default()
        })
        .await
        .unwrap();
    }
    settled(&s).await;
    // The provider goes down, then the switch to it starts: its first attempts fail.
    up.store(false, Ordering::SeqCst);
    let mut m = serde_json::Map::new();
    m.insert("embedding.provider".into(), serde_json::json!("openai"));
    m.insert("embedding.model".into(), serde_json::json!("m"));
    s.patch_settings(m).unwrap();
    until("a first attempt while it is down", 15, || async {
        seen.lock().iter().any(|t| t.contains("narwhal"))
    })
    .await;
    // The project is denied during the outage; the provider then comes back.
    let mut m = serde_json::Map::new();
    m.insert(
        "privacy.cloud_denied_projects".into(),
        serde_json::json!(["secret-proj"]),
    );
    s.patch_settings(m).unwrap();
    let sent_before = seen.lock().len();
    up.store(true, Ordering::SeqCst);
    until("the switch", 30, || async { s.model() == "openai:m" }).await;
    assert!(
        !seen.lock()[sent_before..]
            .iter()
            .any(|t| t.contains("narwhal")),
        "text of a project denied meanwhile was sent"
    );
    assert_eq!(state_of(&s, "narwhal").await, "skipped_cloud");
    s.shutdown();
}

/// I3: a config.toml that does not load must not turn the settings into defaults and start a
/// re-embed to the local model: the recorded model stays in force, and health says why.
#[tokio::test(flavor = "multi_thread")]
async fn a_broken_config_keeps_the_recorded_model_and_starts_no_re_embed() {
    let d = tempfile::tempdir().unwrap();
    let s = start(
        d.path(),
        options(fake_factory()),
        "[embedding]\nprovider = \"ollama\"\nmodel = \"m\"\n",
    );
    until("the model", 15, || async { s.model() == "ollama:m" }).await;
    s.remember(RememberRequest {
        content: "a memory".into(),
        ..Default::default()
    })
    .await
    .unwrap();
    settled(&s).await;
    s.shutdown();
    drop(s);
    let home = AliceHome::new(d.path().to_path_buf());
    std::fs::write(home.config_file(), "[embedding\nthis is not toml").unwrap();
    let s = start(d.path(), options(fake_factory()), "");
    assert!(s.settings_handle().config_degraded());
    tokio::time::sleep(Duration::from_millis(1200)).await; // a few reconciles
    let h = s.health().await.unwrap();
    assert_eq!(h.embedding_model, "ollama:m");
    assert!(h.reembed.is_none(), "{h:?}");
    assert!(
        h.embedding_error
            .as_deref()
            .is_some_and(|e| e.contains("config.toml")),
        "{h:?}"
    );
    let local: i64 = s
        .read(|c| {
            Ok(c.query_row(
                "SELECT count(*) FROM embeddings WHERE model = ?1",
                [MODEL_ID],
                |r| r.get(0),
            )?)
        })
        .await
        .unwrap();
    assert_eq!(local, 0, "nothing was re-embedded to the local model");
    // And the connection test refuses to send anything while the configuration is degraded.
    let t = s.test_provider("embedding").await.unwrap();
    assert!(!t.ok || !s.embedder().is_remote());
    s.shutdown();
}

/// I4: the daemon starts at once even on a new database whose provider is down, answers by
/// words, queues what it is told, and takes the provider into use when it answers.
#[tokio::test(flavor = "multi_thread")]
async fn a_new_database_starts_at_once_with_the_provider_down_and_heals() {
    let d = tempfile::tempdir().unwrap();
    let up = Arc::new(AtomicBool::new(false));
    let factory: alice_embed::EmbedderFactory = {
        let up = up.clone();
        Arc::new(move |s: &EmbeddingSettings, _key| {
            let id = expected_model_id(s);
            if id != MODEL_ID && !up.load(Ordering::SeqCst) {
                return Err(alice_core::AliceError::Unavailable(
                    "cannot reach the provider".into(),
                ));
            }
            Ok(Arc::new(FakeEmbedder::named(
                id.clone(),
                if id == MODEL_ID { 384 } else { 64 },
            )) as Arc<dyn Embedder>)
        })
    };
    let started = std::time::Instant::now();
    let s = start(
        d.path(),
        options(factory),
        "[embedding]\nprovider = \"ollama\"\nmodel = \"m\"\n",
    );
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "start waited for the provider"
    );
    s.remember(RememberRequest {
        content: "the otter slides down the bank".into(),
        ..Default::default()
    })
    .await
    .unwrap();
    let r = s
        .recall(RecallRequest {
            query: "otter".into(),
            format: OutputFormat::Json,
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(r.results.len(), 1, "words still find it");
    until("health to say what is wrong", 15, || async {
        s.health().await.unwrap().embedding_error.is_some()
    })
    .await;
    up.store(true, Ordering::SeqCst);
    until("the provider to be used", 30, || async {
        let h = s.health().await.unwrap();
        h.embedding_model == "ollama:m" && h.embedding_error.is_none() && h.embed_backlog == 0
    })
    .await;
    assert_eq!(state_of(&s, "otter").await, "done");
    s.shutdown();
    // The placeholder of the new database left no index files behind.
    let base = AliceHome::new(d.path().to_path_buf()).vector_file(PLACEHOLDER_MODEL);
    for ext in ["memories.usearch", "chunks.bq"] {
        assert!(!base.with_extension(ext).exists(), "{ext}");
    }
}

/// I5: a memory in the trash when the model switched, and restored afterwards, has a vector
/// for the new model.
#[tokio::test(flavor = "multi_thread")]
async fn a_memory_restored_after_a_model_switch_has_a_vector_for_the_new_model() {
    let d = tempfile::tempdir().unwrap();
    let s = start(d.path(), options(fake_factory()), "");
    let id = s
        .remember(RememberRequest {
            content: "a memory that goes to the trash".into(),
            ..Default::default()
        })
        .await
        .unwrap()
        .id
        .unwrap();
    s.remember(RememberRequest {
        content: "another memory that stays".into(),
        ..Default::default()
    })
    .await
    .unwrap();
    settled(&s).await;
    s.update(forget_or_restore(&id, UpdateAction::Forget))
        .await
        .unwrap();
    let mut m = serde_json::Map::new();
    m.insert("embedding.provider".into(), serde_json::json!("ollama"));
    m.insert("embedding.model".into(), serde_json::json!("nomic"));
    s.patch_settings(m).unwrap();
    until("the switch", 20, || async { s.model() == "ollama:nomic" }).await;
    s.update(forget_or_restore(&id, UpdateAction::Restore))
        .await
        .unwrap();
    let n: i64 = s
        .read(move |c| {
            Ok(c.query_row(
                "SELECT count(*) FROM embeddings e JOIN memories m ON m.seq = e.owner_seq
                 WHERE e.owner_type = 'memory' AND e.model = 'ollama:nomic' AND m.id = ?1",
                [&id],
                |r| r.get(0),
            )?)
        })
        .await
        .unwrap();
    assert_eq!(n, 1);
    s.shutdown();
}

/// M8: a daemon that is never quiet still finishes the re-embed (one batch per FORCE_AFTER).
#[tokio::test(flavor = "multi_thread")]
async fn a_busy_daemon_does_not_starve_the_re_embed() {
    let d = tempfile::tempdir().unwrap();
    let s = start(d.path(), options(fake_factory()), "");
    s.remember(RememberRequest {
        content: "a memory".into(),
        ..Default::default()
    })
    .await
    .unwrap();
    settled(&s).await;
    let _busy = s.activity().enter(); // a request that never ends
    let mut m = serde_json::Map::new();
    m.insert("embedding.provider".into(), serde_json::json!("ollama"));
    m.insert("embedding.model".into(), serde_json::json!("nomic"));
    s.patch_settings(m).unwrap();
    until("the switch under constant activity", 30, || async {
        s.model() == "ollama:nomic"
    })
    .await;
    s.shutdown();
}

mod plan_tests {
    use super::*;
    use crate::secrets::FileSecrets;
    use alice_core::store::embeddings;

    fn builder(dir: &std::path::Path, calls: Arc<AtomicUsize>, fail_local: bool) -> Builder {
        Builder {
            factory: Arc::new(move |s: &EmbeddingSettings, _| {
                calls.fetch_add(1, Ordering::SeqCst);
                let id = expected_model_id(s);
                if fail_local && id == MODEL_ID {
                    return Err(alice_core::AliceError::Other("no model".into()));
                }
                Ok(Arc::new(FakeEmbedder::named(id, 8)) as Arc<dyn Embedder>)
            }),
            secrets: Arc::new(FileSecrets::new(dir.join("secrets.toml"))),
            env: Arc::new(|_: &str| None),
        }
    }

    fn settings(provider: EmbeddingProvider, model: &str) -> EmbeddingSettings {
        EmbeddingSettings {
            provider,
            model: model.into(),
            ..Default::default()
        }
    }

    fn conn(dir: &std::path::Path) -> Connection {
        alice_core::store::open_writer(&dir.join("a.db")).unwrap()
    }

    fn put(c: &Connection, model: &str, dim: usize) {
        let p = alice_core::store::projects::upsert_project(
            c,
            &alice_core::project::ProjectRef::global(),
        )
        .unwrap();
        let m = alice_core::store::memories::prepare(
            alice_core::store::memories::NewMemoryInput {
                content: "x".into(),
                ..Default::default()
            },
            &p,
            "h".into(),
        )
        .unwrap();
        let alice_core::store::memories::InsertOutcome::New { seq, .. } =
            alice_core::store::memories::insert(c, &m).unwrap()
        else {
            panic!()
        };
        embeddings::put(c, embeddings::VecKey::Memory(seq), model, &vec![0.5; dim]).unwrap();
    }

    #[test]
    fn a_new_database_with_a_remote_provider_calls_nothing_and_starts_with_a_placeholder() {
        let d = tempfile::tempdir().unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let b = builder(d.path(), calls.clone(), false);
        let p = plan(
            &b,
            &conn(d.path()),
            &settings(EmbeddingProvider::Openai, "m"),
        )
        .unwrap();
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "no provider is called on the start path"
        );
        assert_eq!(p.active.model_id(), PLACEHOLDER_MODEL);
        assert!(p.active.is_remote() && p.record.is_none());
        assert!(p.active.problem().is_some());
    }

    #[test]
    fn a_recorded_remote_model_stays_searchable_through_a_stand_in_of_its_dimension() {
        let d = tempfile::tempdir().unwrap();
        let c = conn(d.path());
        put(&c, "openai:m", 12);
        let recorded = serde_json::to_string(&settings(EmbeddingProvider::Openai, "m")).unwrap();
        c.execute(
            "INSERT INTO meta (key, value) VALUES (?1, ?2)",
            rusqlite::params![ACTIVE_KEY, recorded],
        )
        .unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let b = builder(d.path(), calls.clone(), false);
        // The settings ask for another model: the recorded one stays in force.
        let p = plan(&b, &c, &settings(EmbeddingProvider::Voyage, "v")).unwrap();
        assert_eq!((p.active.model_id(), p.active.dim()), ("openai:m", 12));
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn a_database_with_local_vectors_and_nothing_recorded_is_on_the_local_model() {
        let d = tempfile::tempdir().unwrap();
        let c = conn(d.path());
        put(&c, MODEL_ID, 8);
        let b = builder(d.path(), Arc::default(), false);
        let p = plan(&b, &c, &settings(EmbeddingProvider::Ollama, "m")).unwrap();
        assert_eq!(p.active.model_id(), MODEL_ID);
        assert!(
            p.record.is_some(),
            "recorded now, so a later change is a change"
        );
    }

    #[test]
    fn a_local_model_that_cannot_be_built_leaves_a_stand_in_and_an_error() {
        let d = tempfile::tempdir().unwrap();
        let c = conn(d.path());
        let b = builder(d.path(), Arc::default(), true);
        let p = plan(&b, &c, &EmbeddingSettings::default()).unwrap();
        assert_eq!(p.active.model_id(), PLACEHOLDER_MODEL);
        assert!(p.error.as_deref().is_some_and(|e| e.contains("no model")));
    }
}

/// N1: one call of the embedder is one request, so callers cut their batches to its size and ask
/// the cloud policy before each. A project denied after the first request is not sent in the
/// second, even though the settings' batch size (8) is larger than the request (3).
#[tokio::test(flavor = "multi_thread")]
async fn a_batch_larger_than_a_request_is_cut_and_the_policy_asked_before_each_piece() {
    let d = tempfile::tempdir().unwrap();
    let seen: Arc<Mutex<Vec<String>>> = Arc::default();
    let service: Arc<std::sync::OnceLock<Arc<Service>>> = Arc::default();
    let factory: alice_embed::EmbedderFactory = {
        let (seen, service) = (seen.clone(), service.clone());
        Arc::new(move |s: &EmbeddingSettings, _| {
            let id = expected_model_id(s);
            if id == MODEL_ID {
                return Ok(Arc::new(FakeEmbedder::named(id, 384)) as Arc<dyn Embedder>);
            }
            let service = service.clone();
            // After the first request: the project is denied.
            let deny: Box<dyn FnOnce() + Send> = Box::new(move || {
                if let Some(s) = service.get() {
                    let mut m = serde_json::Map::new();
                    m.insert(
                        "privacy.cloud_denied_projects".into(),
                        serde_json::json!(["secret-proj"]),
                    );
                    s.patch_settings(m).unwrap();
                }
            });
            Ok(Arc::new(Capped {
                inner: FakeEmbedder::named(id, 64),
                cap: 3,
                seen: seen.clone(),
                after_first: Mutex::new(Some(deny)),
            }) as Arc<dyn Embedder>)
        })
    };
    let s = start(d.path(), options(factory), "[embedding]\nbatch_size = 8\n");
    service.set(s.clone()).ok();
    let (open, secret) = (repo(&d, "open-proj"), repo(&d, "secret-proj"));
    for i in 0..3 {
        s.remember(RememberRequest {
            content: format!("open memory {i}"),
            cwd: open.clone(),
            ..Default::default()
        })
        .await
        .unwrap();
    }
    for i in 0..3 {
        s.remember(RememberRequest {
            content: format!("secret memory {i}"),
            cwd: secret.clone(),
            ..Default::default()
        })
        .await
        .unwrap();
    }
    settled(&s).await;
    let mut m = serde_json::Map::new();
    m.insert("embedding.provider".into(), serde_json::json!("openai"));
    m.insert("embedding.model".into(), serde_json::json!("m"));
    s.patch_settings(m).unwrap();
    until("the switch", 30, || async { s.model() == "openai:m" }).await;
    let seen = seen.lock().clone();
    assert!(seen.iter().any(|t| t.contains("open memory")), "{seen:?}");
    assert!(
        !seen.iter().any(|t| t.contains("secret memory")),
        "a project denied after the first request was sent in the second: {seen:?}"
    );
    s.shutdown();
}

/// Final review A-I3: a remote provider gets text redacted with today's always-on rules, also
/// when the person stores raw text (`privacy.redact = false`), and for a pattern added after the
/// write (the re-embed of older rows); what is stored stays as the person chose.
#[tokio::test(flavor = "multi_thread")]
async fn a_remote_provider_gets_text_redacted_with_todays_rules_whatever_was_stored() {
    let mock = Mock::start();
    let d = tempfile::tempdir().unwrap();
    let s = start(
        d.path(),
        options(http_factory(true, Arc::default())),
        "[privacy]\nredact = false\n",
    );
    let open = repo(&d, "open-proj");
    let token = concat!("gh", "p_aBcDeFgHiJkLmNoPqRsTuVwXyZ0123456789");
    let core = "aBcDeFgHiJkLmNoPqRsTuVwXyZ0123456789";
    let first = s
        .remember(RememberRequest {
            content: format!("deploy key {token} for the pelican service"),
            cwd: open.clone(),
            ..Default::default()
        })
        .await
        .unwrap();
    s.ingest(IngestRequest {
        source: SourceInput {
            uri: "s-1".into(),
            ..Default::default()
        },
        chunks: vec![ChunkInput {
            ord: 0,
            text: "ticket TCK-12345 about the hamster wheel".into(),
            ..Default::default()
        }],
        cwd: open.clone(),
        ..Default::default()
    })
    .await
    .unwrap();
    settled(&s).await;
    // A pattern added after the writes, then a remote provider: the re-embed sends old rows.
    let mut m = serde_json::Map::new();
    m.insert(
        "privacy.custom_patterns".into(),
        serde_json::json!([{ "name": "ticket", "regex": "TCK-[0-9]{4,}" }]),
    );
    m.insert("embedding.provider".into(), serde_json::json!("openai"));
    m.insert("embedding.model".into(), serde_json::json!("m"));
    m.insert("embedding.endpoint".into(), serde_json::json!(mock.base));
    s.patch_settings(m).unwrap();
    until("the switch to the provider", 15, || async {
        s.health().await.unwrap().embedding_model == "openai:m"
    })
    .await;
    assert!(
        mock.saw("pelican") && mock.saw("hamster"),
        "{:?}",
        mock.texts()
    );
    for secret in [token, core, "TCK-12345"] {
        assert!(
            !mock.saw(secret),
            "{secret} left the machine: {:?}",
            mock.texts()
        );
    }
    assert!(mock.saw("[REDACTED:github_token]") && mock.saw("[REDACTED:custom:ticket]"));
    // A new write is embedded by the worker on the provider: redacted the same way.
    s.remember(RememberRequest {
        content: format!("another key {token} for the walrus colony"),
        cwd: open.clone(),
        ..Default::default()
    })
    .await
    .unwrap();
    settled(&s).await;
    until("the new memory to be sent", 15, || async {
        mock.saw("walrus")
    })
    .await;
    assert!(!mock.saw(token) && !mock.saw(core));
    // What is stored is what the person chose to store.
    let id = first.id.unwrap();
    let body: String = s
        .read(move |c| {
            Ok(
                c.query_row("SELECT body FROM memories WHERE id = ?1", [&id], |r| {
                    r.get(0)
                })?,
            )
        })
        .await
        .unwrap();
    assert!(body.contains(token));
    s.shutdown();
}

/// Final re-review N-I1: a recall from a stored project's tree whose `.alice.toml` denies the
/// cloud sends no query to a remote embedder, although the project was last written from a
/// worktree without the file; and the project's later writes stay at home.
#[tokio::test(flavor = "multi_thread")]
async fn a_query_from_a_tree_whose_file_denies_the_cloud_is_not_sent_to_a_remote_embedder() {
    let mock = Mock::start();
    let d = tempfile::tempdir().unwrap();
    let s = start(
        d.path(),
        options(http_factory(true, Arc::default())),
        &remote_toml(&mock, "m", ""),
    );
    until("the provider's embedder to be built", 15, || async {
        s.model() == "openai:m"
    })
    .await;
    let [main, feat] = ["client-x", "client-x-feat"].map(|n| {
        let dir = d.path().join(n);
        std::fs::create_dir(&dir).unwrap();
        for args in [
            &["init", "-q"][..],
            &[
                "remote",
                "add",
                "origin",
                "https://example.com/me/client-x.git",
            ],
        ] {
            let ok = std::process::Command::new("git")
                .args(args)
                .current_dir(&dir)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_SYSTEM", "/dev/null")
                .status()
                .is_ok_and(|s| s.success());
            assert!(ok, "git {args:?}");
        }
        Some(dir.to_string_lossy().into_owned())
    });
    std::fs::write(
        std::path::Path::new(main.as_deref().unwrap()).join(".alice.toml"),
        "[privacy]\ncloud_denied = true\n",
    )
    .unwrap();
    let remember = |cwd: &Option<String>, text: &str| {
        let (s, cwd, text) = (s.clone(), cwd.clone(), text.to_string());
        async move {
            s.remember(RememberRequest {
                content: text,
                cwd,
                ..Default::default()
            })
            .await
            .unwrap()
        }
    };
    remember(&feat, "the pelican nests by the harbour").await;
    settled(&s).await;
    assert!(mock.saw("pelican"), "nothing said otherwise yet");
    let sent = mock.texts().len();
    let r = s
        .recall(RecallRequest {
            query: "where does the pelican nest".into(),
            cwd: main.clone(),
            format: OutputFormat::Json,
            ..Default::default()
        })
        .await
        .unwrap();
    assert!(
        r.notes.iter().any(|n| n.contains("word matches only")),
        "{:?}",
        r.notes
    );
    assert_eq!(mock.texts().len(), sent, "the query never left");
    remember(&feat, "the walrus colony is on the north shore").await;
    settled(&s).await;
    assert!(
        !mock.saw("walrus"),
        "the project stays at home from every tree"
    );
    assert_eq!(state_of(&s, "walrus").await, "skipped_cloud");
    s.shutdown();
}
