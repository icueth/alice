//! `POST /api/v1/import/upload`: a person uploads their own MemPalace as a gzip tar (spec
//! addendum 6). A tenant daemon on a free port, a throwaway home, real HTTP.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use alice_core::embed::{Embedder, FakeEmbedder};
use alice_core::import::{ImportRequest, ImportState};
use alice_daemon::importer::ImportHook;
use alice_daemon::paths::AliceHome;
use alice_daemon::service::{Service, ServiceOptions};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

// ------------------------------------------------------------------------------------ harness

struct Srv {
    _d: tempfile::TempDir,
    home: AliceHome,
    port: u16,
    token: String,
    shutdown: CancellationToken,
    http: reqwest::Client,
}

type Before = Box<dyn Fn(&AliceHome)>;

#[derive(Default)]
struct Opts {
    tenant: bool,
    /// `import.max_upload_bytes`, written to config.toml before the start.
    cap: Option<u64>,
    free: Option<alice_daemon::maintenance::FreeSpace>,
    hook: Option<Arc<ImportHook>>,
    /// Written under the home before the start.
    before: Option<Before>,
}

impl Srv {
    async fn tenant() -> Srv {
        Self::start(Opts {
            tenant: true,
            ..Default::default()
        })
        .await
    }

    async fn start(o: Opts) -> Srv {
        alice_daemon::install_tls();
        let d = tempfile::tempdir().unwrap();
        let home = AliceHome::new(d.path().to_path_buf());
        home.ensure_dirs().unwrap();
        if let Some(cap) = o.cap {
            std::fs::write(
                home.config_file(),
                format!("[import]\nmax_upload_bytes = {cap}\n"),
            )
            .unwrap();
        }
        if let Some(f) = &o.before {
            f(&home);
        }
        let svc = Service::start(
            home.clone(),
            &|| Ok(Arc::new(FakeEmbedder::new(384)) as Arc<dyn Embedder>),
            ServiceOptions {
                tenant: o.tenant,
                free_space: o.free,
                import_hook: o.hook,
                ..Default::default()
            },
        )
        .unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let token = "t".repeat(64);
        let shutdown = CancellationToken::new();
        let app = alice_daemon::http::router(svc.clone(), token.clone(), port, shutdown.clone());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Srv {
            _d: d,
            home,
            port,
            token,
            shutdown,
            http: reqwest::Client::new(),
        }
    }

    fn url(&self, p: &str) -> String {
        format!("http://127.0.0.1:{}{p}", self.port)
    }

    fn upload_req(&self) -> reqwest::RequestBuilder {
        self.http
            .post(self.url("/api/v1/import/upload"))
            .bearer_auth(&self.token)
            .header("content-type", "application/gzip")
    }

    async fn upload(&self, body: Vec<u8>) -> reqwest::Response {
        self.upload_req().body(body).send().await.unwrap()
    }

    async fn status(&self) -> Value {
        // The server closes a connection it cut off mid-upload; a pooled one can be stale for
        // the next request, so a failed send is retried on a fresh connection.
        let mut last = None;
        for _ in 0..4 {
            match self
                .http
                .get(self.url("/api/v1/import/status"))
                .bearer_auth(&self.token)
                .send()
                .await
            {
                Ok(r) => return r.json().await.unwrap(),
                Err(e) => last = Some(e),
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("status request failed: {:?}", last);
    }

    /// Waits until the job is `done` or `failed`; returns that status.
    async fn finished(&self) -> Value {
        let t = Instant::now();
        loop {
            let s = self.status().await;
            if matches!(s["state"].as_str(), Some("done" | "failed")) {
                return s;
            }
            assert!(t.elapsed() < Duration::from_secs(300), "never ended: {s}");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// Waits until the status shows `state`.
    async fn until(&self, state: &str) -> Value {
        let t = Instant::now();
        loop {
            let s = self.status().await;
            if s["state"] == state {
                return s;
            }
            assert!(t.elapsed() < Duration::from_secs(30), "never {state}: {s}");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    fn inbox(&self) -> PathBuf {
        self.home.import_inbox()
    }

    /// Everything in the inbox whose name starts with `upload-`.
    fn leftovers(&self) -> Vec<String> {
        let Ok(rd) = std::fs::read_dir(self.inbox()) else {
            return Vec::new();
        };
        rd.flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with("upload-"))
            .collect()
    }

    fn memory_count(&self) -> i64 {
        let db = rusqlite::Connection::open_with_flags(
            self.home.db(),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap();
        db.query_row("SELECT count(*) FROM memories", [], |r| r.get(0))
            .unwrap()
    }
}

impl Drop for Srv {
    fn drop(&mut self) {
        self.shutdown.cancel();
    }
}

// ------------------------------------------------------------------------------------ archives

enum E<'a> {
    File(&'a str, Vec<u8>),
    Dir(&'a str),
    Sym(&'a str, &'a str),
    Hard(&'a str, &'a str),
    Char(&'a str),
    Fifo(&'a str),
}

/// A tar written by hand, so the names the `tar` crate's builder refuses (`..`, absolute) and the
/// entry kinds an honest client never sends can be tested.
fn raw_tar(entries: &[E]) -> Vec<u8> {
    let mut out = Vec::new();
    for e in entries {
        let mut h = tar::Header::new_gnu();
        let (name, kind, link, body): (&str, tar::EntryType, &str, &[u8]) = match e {
            E::File(n, b) => (n, tar::EntryType::Regular, "", b),
            E::Dir(n) => (n, tar::EntryType::Directory, "", &[]),
            E::Sym(n, t) => (n, tar::EntryType::Symlink, t, &[]),
            E::Hard(n, t) => (n, tar::EntryType::Link, t, &[]),
            E::Char(n) => (n, tar::EntryType::Char, "", &[]),
            E::Fifo(n) => (n, tar::EntryType::Fifo, "", &[]),
        };
        h.set_entry_type(kind);
        h.set_mode(0o644);
        h.set_size(body.len() as u64);
        let old = h.as_old_mut();
        old.name[..name.len()].copy_from_slice(name.as_bytes());
        old.linkname[..link.len()].copy_from_slice(link.as_bytes());
        h.set_cksum();
        out.extend_from_slice(h.as_bytes());
        out.extend_from_slice(body);
        out.resize(out.len().div_ceil(512) * 512, 0);
    }
    out.extend_from_slice(&[0u8; 1024]);
    out
}

fn gz(raw: &[u8]) -> Vec<u8> {
    use std::io::Write;
    let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    e.write_all(raw).unwrap();
    e.finish().unwrap()
}

/// Bytes that do not compress, like the vectors of a real palace (a fixture palace is plain
/// text, which packs far better than the 12x the unpack limit allows when it repeats).
fn noise(n: usize) -> Vec<u8> {
    let mut x = 0x9E37_79B9_7F4A_7C15u64;
    (0..n)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            (x >> 24) as u8
        })
        .collect()
}

/// A gzip tar of the folder `dir`, stored under `prefix/`, with `vectors.bin` of noise as big as
/// the folder beside its files.
fn tar_gz_of(dir: &Path, prefix: &str) -> Vec<u8> {
    let mut b = tar::Builder::new(Vec::new());
    b.append_dir_all(prefix, dir).unwrap();
    append_noise(&mut b, prefix, dir_size(dir));
    gz(&b.into_inner().unwrap())
}

fn dir_size(dir: &Path) -> usize {
    std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().metadata().unwrap().len() as usize)
        .sum()
}

fn append_noise(b: &mut tar::Builder<Vec<u8>>, prefix: &str, n: usize) {
    let mut h = tar::Header::new_gnu();
    h.set_size(n as u64);
    h.set_mode(0o644);
    b.append_data(&mut h, format!("{prefix}/vectors.bin"), noise(n).as_slice())
        .unwrap();
}

/// A palace of one real drawer table, small enough to be quick.
fn small_palace(root: &Path) -> PathBuf {
    alice_core::import::fixture::synthetic_palace(&root.join("palace"), 30).dir
}

async fn assert_failed_clean(s: &Srv, needle: &str) {
    let st = s.finished().await;
    assert_eq!(st["state"], "failed", "{st}");
    let err = st["error"].as_str().unwrap_or_default().to_lowercase();
    assert!(err.contains(&needle.to_lowercase()), "{err} lacks {needle}");
    assert_eq!(s.memory_count(), 0, "nothing was imported");
    assert!(s.leftovers().is_empty(), "inbox: {:?}", s.leftovers());
}

// ----------------------------------------------------------------------------------- the route

#[tokio::test(flavor = "multi_thread")]
async fn no_token_is_401_and_nothing_is_stored() {
    let s = Srv::tenant().await;
    let r = s
        .http
        .post(s.url("/api/v1/import/upload"))
        .header("content-type", "application/gzip")
        .body(vec![1u8; 10])
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 401);
    assert!(s.leftovers().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_local_daemon_answers_404() {
    let s = Srv::start(Opts::default()).await;
    let r = s.upload(vec![1u8; 10]).await;
    assert_eq!(r.status(), 404);
    assert!(s.leftovers().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn an_uploaded_palace_imports_with_the_counts_of_the_json_path() {
    let src = tempfile::tempdir().unwrap();
    let palace = alice_core::import::fixture::synthetic_palace(&src.path().join("palace"), 600);
    let s = Srv::tenant().await;

    // The same palace through the local JSON path of a local daemon.
    let other = tempfile::tempdir().unwrap();
    let local = Service::start(
        AliceHome::new(other.path().join("alice")),
        &|| Ok(Arc::new(FakeEmbedder::new(384)) as Arc<dyn Embedder>),
        ServiceOptions::default(),
    )
    .unwrap();
    local
        .start_import(ImportRequest {
            palace: Some(palace.dir.display().to_string()),
            ..Default::default()
        })
        .unwrap();
    let l = loop {
        let st = local.import_status();
        if st.state != ImportState::Running {
            break st;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    assert_eq!(l.state, ImportState::Done, "{:?}", l.error);
    let l = serde_json::to_value(l.report.unwrap()).unwrap();

    let r = s.upload(tar_gz_of(&palace.dir, "palace")).await;
    assert_eq!(r.status(), 202, "{}", r.text().await.unwrap());
    let first: Value = r.json().await.unwrap();
    assert!(first["job_id"].as_str().is_some_and(|j| !j.is_empty()));
    assert!(
        matches!(
            first["state"].as_str(),
            Some("unpacking" | "importing" | "done")
        ),
        "{first}"
    );
    let t = s.finished().await;
    assert_eq!(t["state"], "done", "{t}");
    assert_eq!(t["job_id"], first["job_id"]);
    assert!(t["bytes_received"].as_u64().unwrap() > 0, "{t}");
    assert!(t["embedding_backlog"].is_u64(), "{t}");
    let t = &t["report"];
    for k in [
        "tiers",
        "projects",
        "duplicates",
        "facts",
        "skipped",
        "drawers_seen",
        "drawers_accounted",
    ] {
        assert_eq!(l[k], t[k], "report.{k}");
    }
    assert!(t["drawers_seen"].as_u64().unwrap() >= 600);
    assert!(s.leftovers().is_empty(), "{:?}", s.leftovers());
    assert!(s.memory_count() > 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_knowledge_graph_may_sit_anywhere_in_the_upload() {
    let src = tempfile::tempdir().unwrap();
    let palace = small_palace(src.path());
    let kgdir = src.path().join("kg");
    std::fs::create_dir_all(&kgdir).unwrap();
    alice_core::import::fixture::build_kg(
        &kgdir,
        &[("alice", "alice"), ("bangkok", "bangkok")],
        &[alice_core::import::fixture::TripleSpec {
            id: "t1".into(),
            subject: "alice".into(),
            predicate: "located_at".into(),
            object: "bangkok".into(),
            valid_from: Some("2026-01-01".into()),
            ..Default::default()
        }],
        false,
    );
    let mut b = tar::Builder::new(Vec::new());
    b.append_dir_all("wrap/palace", &palace).unwrap();
    b.append_dir_all("wrap/extra", &kgdir).unwrap();
    append_noise(&mut b, "wrap/palace", dir_size(&palace));
    let body = gz(&b.into_inner().unwrap());
    let s = Srv::tenant().await;
    assert_eq!(s.upload(body).await.status(), 202);
    let st = s.finished().await;
    assert_eq!(st["state"], "done", "{st}");
    assert_eq!(st["report"]["facts"]["added"].as_u64(), Some(1), "{st}");
    assert!(s.leftovers().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn the_status_keeps_its_old_fields_and_adds_the_new_ones() {
    let s = Srv::tenant().await;
    let st = s.status().await;
    assert_eq!(st["state"], "idle");
    for k in ["job_id", "phase", "done", "total", "report", "error"] {
        assert!(st.get(k).is_some(), "{k} missing in {st}");
    }
    assert_eq!(st["bytes_received"], 0);
    assert_eq!(st["embedding_backlog"], 0);
}

// ---------------------------------------------------------------------------------- one at a time

fn channel_body() -> (tokio::sync::mpsc::Sender<Vec<u8>>, reqwest::Body) {
    let (tx, rx) = tokio::sync::mpsc::channel::<Vec<u8>>(4);
    let stream = futures::stream::unfold(rx, |mut rx| async move {
        rx.recv().await.map(|c| (Ok::<_, std::io::Error>(c), rx))
    });
    (tx, reqwest::Body::wrap_stream(stream))
}

#[tokio::test(flavor = "multi_thread")]
async fn a_second_upload_gets_409_while_one_is_being_received() {
    let s = Srv::tenant().await;
    let (tx, body) = channel_body();
    let req = s.upload_req().body(body);
    let first = tokio::spawn(async move { req.send().await });
    tx.send(vec![0u8; 4096]).await.unwrap();
    let st = s.until("receiving").await;
    assert!(st["job_id"].is_string());
    let r = s.upload(vec![1u8; 10]).await;
    assert_eq!(r.status(), 409);
    let msg = r.text().await.unwrap();
    assert!(msg.contains("already"), "{msg}");
    // The refused one left nothing; the running one is intact.
    assert_eq!(s.leftovers().len(), 1);
    // Ending the first body (not a gzip) fails that job only.
    drop(tx);
    let r = first.await.unwrap().unwrap();
    assert_eq!(r.status(), 202);
    assert_failed_clean(&s, "gzip").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_second_upload_gets_409_while_the_first_imports() {
    let hook = Arc::new(ImportHook::default());
    hook.hold.store(true, Ordering::SeqCst);
    let s = Srv::start(Opts {
        tenant: true,
        hook: Some(hook.clone()),
        ..Default::default()
    })
    .await;
    let src = tempfile::tempdir().unwrap();
    let body = tar_gz_of(&small_palace(src.path()), "palace");
    assert_eq!(s.upload(body.clone()).await.status(), 202);
    s.until("importing").await;
    let r = s.upload(body.clone()).await;
    assert_eq!(r.status(), 409);
    hook.hold.store(false, Ordering::SeqCst);
    let st = s.finished().await;
    assert_eq!(st["state"], "done", "{st}");
    // The slot is free again once the job ended.
    assert_eq!(s.upload(body).await.status(), 202);
    s.finished().await;
}

// -------------------------------------------------------------------------------------- limits

#[tokio::test(flavor = "multi_thread")]
async fn content_length_over_the_cap_is_413_before_anything_is_stored() {
    let s = Srv::start(Opts {
        tenant: true,
        cap: Some(1000),
        ..Default::default()
    })
    .await;
    let r = s.upload(vec![7u8; 5000]).await;
    assert_eq!(r.status(), 413);
    let msg = r.text().await.unwrap();
    assert!(msg.contains("1000"), "{msg}");
    assert!(s.leftovers().is_empty());
    assert_eq!(
        s.status().await["state"],
        "idle",
        "an early refusal reserves nothing"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_stream_over_the_cap_without_a_length_is_cut_and_its_file_deleted() {
    let s = Srv::start(Opts {
        tenant: true,
        cap: Some(1000),
        ..Default::default()
    })
    .await;
    let chunks: Vec<Result<Vec<u8>, std::io::Error>> =
        (0..50).map(|_| Ok(vec![7u8; 100])).collect();
    let body = reqwest::Body::wrap_stream(futures::stream::iter(chunks));
    // chunked: no Content-Length. The server may answer 413 or close the connection first.
    if let Ok(r) = s.upload_req().body(body).send().await {
        assert_eq!(r.status(), 413);
    }
    let st = s.status().await;
    assert_eq!(st["state"], "failed", "{st}");
    assert!(
        st["error"].as_str().unwrap().contains("1000"),
        "{}",
        st["error"]
    );
    assert!(s.leftovers().is_empty(), "{:?}", s.leftovers());
    // The slot is free again.
    assert_ne!(s.upload(vec![1u8; 10]).await.status(), 409);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_disk_guard_refuses_from_the_content_length() {
    let s = Srv::start(Opts {
        tenant: true,
        // 4 GB free: not enough for 3 x 1 GB plus the 2 GB reserve.
        free: Some(Arc::new(|_| Some(4_000_000_000))),
        ..Default::default()
    })
    .await;
    let (tx, body) = channel_body();
    drop(tx);
    let r = s
        .upload_req()
        .header("content-length", "1000000000")
        .body(body)
        .send()
        .await;
    // The body is shorter than the length it declares; the refusal comes first.
    if let Ok(r) = r {
        assert_eq!(r.status(), 400);
        let msg = r.text().await.unwrap();
        assert!(msg.contains("free"), "{msg}");
    }
    assert!(s.leftovers().is_empty());
    assert_eq!(s.status().await["state"], "idle");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_disk_guard_is_checked_again_while_streaming() {
    let free = Arc::new(AtomicU64::new(u64::MAX / 4));
    let f = free.clone();
    let s = Srv::start(Opts {
        tenant: true,
        free: Some(Arc::new(move |_| Some(f.load(Ordering::SeqCst)))),
        ..Default::default()
    })
    .await;
    let (tx, body) = channel_body();
    let req = s.upload_req().body(body);
    let call = tokio::spawn(async move { req.send().await });
    tx.send(vec![0u8; 4096]).await.unwrap();
    s.until("receiving").await;
    free.store(1_000, Ordering::SeqCst);
    // Feed until the server gives up on us.
    for _ in 0..200 {
        if tx.send(vec![0u8; 4096]).await.is_err() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
        if s.status().await["state"] == "failed" {
            break;
        }
    }
    drop(tx);
    let _ = call.await;
    let st = s.status().await;
    assert_eq!(st["state"], "failed", "{st}");
    assert!(st["error"].as_str().unwrap().contains("free"), "{st}");
    assert!(s.leftovers().is_empty(), "{:?}", s.leftovers());
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unpacked_size_over_twelve_times_the_archive_is_refused() {
    let s = Srv::tenant().await;
    // 20 MB of zeros packs to a few KB: far over 12x.
    let body = gz(&raw_tar(&[E::File("palace/zeros", vec![0u8; 20_000_000])]));
    assert!(body.len() * 12 < 20_000_000);
    assert_eq!(s.upload(body).await.status(), 202);
    assert_failed_clean(&s, "too large").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_text_heavy_palace_that_packs_five_to_ten_times_is_not_refused_for_its_size() {
    let s = Srv::tenant().await;
    // One random byte in every ten, the rest zeros: packs well over 3 times, under 12.
    let raw: Vec<u8> = noise(300_000)
        .into_iter()
        .flat_map(|b| [b, 0, 0, 0, 0, 0, 0, 0, 0, 0])
        .collect();
    let body = gz(&raw_tar(&[E::File("palace/sparse", raw.clone())]));
    let ratio = raw.len() / body.len();
    assert!((5..12).contains(&ratio), "fixture packs {ratio} times");
    assert_eq!(s.upload(body).await.status(), 202);
    let st = s.finished().await;
    // It fails (there is no palace in it), but not for its size.
    assert_eq!(st["state"], "failed", "{st}");
    assert!(!st["error"].as_str().unwrap().contains("too large"), "{st}");
}

#[tokio::test(flavor = "multi_thread")]
async fn upload_sh_is_public_static_text_in_tenant_mode_only() {
    let s = Srv::tenant().await;
    // No token, a good Host.
    let r = s
        .http
        .get(s.url("/api/v1/import/upload.sh"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(
        r.headers()["content-type"],
        "text/x-shellscript; charset=utf-8"
    );
    let body = r.text().await.unwrap();
    assert!(
        body.starts_with("#!/bin/sh"),
        "{}",
        &body[..40.min(body.len())]
    );
    // No address inside: the only URL-looking words are the documented placeholder.
    let stripped = body.replace("<tenant-base-url>", "");
    for (i, _) in stripped.match_indices("http") {
        let rest = &stripped[i..];
        assert!(
            !(rest.starts_with("http://") || rest.starts_with("https://"))
                || rest.starts_with("https://alice.example"),
            "an address in the script: {}",
            &rest[..rest.len().min(40)]
        );
    }
    assert!(!body.contains(&s.token));
    // A foreign Host is still refused by the guard.
    let r = s
        .http
        .get(s.url("/api/v1/import/upload.sh"))
        .header("host", "evil.example")
        .send()
        .await
        .unwrap();
    assert!(r.status().is_client_error(), "{}", r.status());
    // A local daemon answers 404 like the rest.
    let l = Srv::start(Opts::default()).await;
    let r = l
        .http
        .get(l.url("/api/v1/import/upload.sh"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 404);
}

// -------------------------------------------------------------------------- unsafe archives

#[tokio::test(flavor = "multi_thread")]
async fn unsafe_entries_fail_the_whole_upload_and_leave_nothing() {
    let src = tempfile::tempdir().unwrap();
    let palace = small_palace(src.path());
    let outside = tempfile::tempdir().unwrap();
    let abs = format!("{}/escaped", outside.path().display());
    let cases: Vec<(&str, &str, Vec<E>)> = vec![
        (
            "symlink",
            "symbolic link",
            vec![E::Sym("palace/link", "/etc")],
        ),
        (
            "hard link",
            "hard link",
            vec![E::Hard("palace/h", "palace/sqlite_exact.sqlite3")],
        ),
        ("device", "device", vec![E::Char("palace/null")]),
        ("fifo", "named pipe", vec![E::Fifo("palace/pipe")]),
        ("absolute", "absolute", vec![E::File(&abs, b"x".to_vec())]),
        (
            "parent",
            "..",
            vec![E::File("palace/../../escaped", b"x".to_vec())],
        ),
        (
            "parent first",
            "..",
            vec![E::File("../escaped", b"x".to_vec())],
        ),
    ];
    for (name, needle, bad) in cases {
        let s = Srv::tenant().await;
        // A valid palace first (with noise, so the size limit is not what refuses it), so a
        // lax unpacker would have something to import.
        let good = std::fs::read(palace.join("sqlite_exact.sqlite3")).unwrap();
        let mut entries = vec![
            E::Dir("palace/"),
            E::File("palace/vectors.bin", noise(good.len())),
            E::File("palace/sqlite_exact.sqlite3", good),
        ];
        entries.extend(bad);
        let r = s.upload(gz(&raw_tar(&entries))).await;
        assert_eq!(r.status(), 202, "{name}");
        assert_failed_clean(&s, needle).await;
        assert!(!outside.path().join("escaped").exists(), "{name}");
        let parent_escape = s.home.import_inbox().join("escaped");
        assert!(!parent_escape.exists(), "{name}");
        assert!(!s.home.root().join("escaped").exists(), "{name}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn too_many_entries_are_refused() {
    let s = Srv::tenant().await;
    let many: Vec<E> = (0..60_000).map(|_| E::Dir("palace/d")).collect();
    assert_eq!(s.upload(gz(&raw_tar(&many))).await.status(), 202);
    assert_failed_clean(&s, "entries").await;
}

// ---------------------------------------------------------------------- failing jobs clean up

#[tokio::test(flavor = "multi_thread")]
async fn a_body_that_is_not_gzip_fails_and_cleans() {
    let s = Srv::tenant().await;
    assert_eq!(
        s.upload(b"hello, not an archive".to_vec()).await.status(),
        202
    );
    assert_failed_clean(&s, "gzip").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn an_empty_body_is_refused() {
    let s = Srv::tenant().await;
    let r = s.upload(Vec::new()).await;
    assert_eq!(r.status(), 400);
    assert!(s.leftovers().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn an_archive_without_a_palace_fails_with_a_plain_error() {
    let s = Srv::tenant().await;
    let body = gz(&raw_tar(&[E::File("notes/readme.txt", b"hi".to_vec())]));
    assert!(!body.is_empty());
    assert_eq!(s.upload(body).await.status(), 202);
    assert_failed_clean(&s, "palace").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn two_palaces_at_the_same_depth_are_ambiguous() {
    let src = tempfile::tempdir().unwrap();
    let palace = small_palace(src.path());
    let db = std::fs::read(palace.join("sqlite_exact.sqlite3")).unwrap();
    let s = Srv::tenant().await;
    let body = gz(&raw_tar(&[
        E::File("one/sqlite_exact.sqlite3", db.clone()),
        E::File("two/sqlite_exact.sqlite3", db.clone()),
        E::File("pad.bin", noise(2 * db.len())),
    ]));
    assert_eq!(s.upload(body).await.status(), 202);
    assert_failed_clean(&s, "more than one").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn the_shallowest_palace_wins() {
    let src = tempfile::tempdir().unwrap();
    let palace = small_palace(src.path());
    let db = std::fs::read(palace.join("sqlite_exact.sqlite3")).unwrap();
    let s = Srv::tenant().await;
    let body = gz(&raw_tar(&[
        E::File("deep/er/sqlite_exact.sqlite3", b"not a database".to_vec()),
        E::File("pad.bin", noise(db.len())),
        E::File("palace/sqlite_exact.sqlite3", db),
    ]));
    assert_eq!(s.upload(body).await.status(), 202);
    let st = s.finished().await;
    assert_eq!(st["state"], "done", "{st}");
}

#[tokio::test(flavor = "multi_thread")]
async fn an_importer_panic_fails_the_job_and_cleans() {
    let hook = Arc::new(ImportHook::default());
    hook.panic.store(true, Ordering::SeqCst);
    let s = Srv::start(Opts {
        tenant: true,
        hook: Some(hook),
        ..Default::default()
    })
    .await;
    let src = tempfile::tempdir().unwrap();
    assert_eq!(
        s.upload(tar_gz_of(&small_palace(src.path()), "palace"))
            .await
            .status(),
        202
    );
    assert_failed_clean(&s, "stopped unexpectedly").await;
    assert!(
        std::fs::read_dir(s.home.import_tmp())
            .map(|mut r| r.next().is_none())
            .unwrap_or(true)
    );
}

// ------------------------------------------------------------------------------ restart sweep

#[tokio::test(flavor = "multi_thread")]
async fn a_start_removes_stale_uploads_and_only_those() {
    let s = Srv::start(Opts {
        tenant: true,
        before: Some(Box::new(|home| {
            let inbox = home.import_inbox();
            std::fs::create_dir_all(inbox.join("upload-01ABC/palace")).unwrap();
            std::fs::write(inbox.join("upload-01ABC/palace/x"), b"x").unwrap();
            std::fs::write(inbox.join("upload-01ABC.tar.gz"), b"x").unwrap();
            std::fs::create_dir_all(inbox.join("j1/palace")).unwrap();
        })),
        ..Default::default()
    })
    .await;
    assert!(s.leftovers().is_empty(), "{:?}", s.leftovers());
    assert!(
        s.inbox().join("j1/palace").is_dir(),
        "the admin's folder stays"
    );
    assert_eq!(s.status().await["state"], "idle");
}

// ------------------------------------------------------------------------- the real script

/// The shipped script, run by the real sh, curl and tar against the real route: it must finish
/// with exit 0 and leave the palace imported and its temporary folder gone.
#[tokio::test(flavor = "multi_thread")]
async fn upload_sh_runs_end_to_end_against_the_route() {
    let have = |t: &str| {
        std::process::Command::new("sh")
            .args(["-c", &format!("command -v {t} >/dev/null")])
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    };
    if !have("curl") || !have("tar") || !have("gzip") {
        eprintln!("skipped: curl, tar or gzip is missing");
        return;
    }
    let s = Srv::tenant().await;
    let src = tempfile::tempdir().unwrap();
    let palace = small_palace(src.path());
    let home = tempfile::tempdir().unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let cfg = home.path().join(".config/alice");
    std::fs::create_dir_all(&cfg).unwrap();
    let tok = cfg.join("token");
    std::fs::write(&tok, format!("{}\n", s.token)).unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tok, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    let script = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("assets/upload.sh");
    let (base, palace_arg, home_p, tmp_p) = (
        format!("http://127.0.0.1:{}", s.port),
        palace.clone(),
        home.path().to_path_buf(),
        tmp.path().to_path_buf(),
    );
    let out = tokio::task::spawn_blocking(move || {
        std::process::Command::new("sh")
            .arg(script)
            .arg(base)
            .arg("--palace")
            .arg(palace_arg)
            .env("HOME", home_p)
            .env("TMPDIR", tmp_p)
            .env("ALICE_UPLOAD_POLL_SECS", "1")
            .output()
            .unwrap()
    })
    .await
    .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    println!("{stdout}\n{stderr}");
    assert!(out.status.success(), "{stdout}\n{stderr}");
    assert!(stdout.contains("Your MemPalace was imported"), "{stdout}");
    assert!(!stdout.contains(&s.token) && !stderr.contains(&s.token));
    assert_eq!(s.status().await["state"], "done");
    assert!(s.memory_count() > 0);
    assert!(std::fs::read_dir(tmp.path()).unwrap().next().is_none());
    assert!(s.leftovers().is_empty());
}
