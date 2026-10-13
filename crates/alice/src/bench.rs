//! `alice bench`: synthetic benchmark in a throw-away ALICE_HOME (never the user's data).
//!
//! 1. Load: ingest raw chunks, remember memories, wait until every item is embedded and indexed.
//! 2. Latency, one call at a time: `remember`, `recall` (knowledge only; with `include_raw`,
//!    also under a date filter; with `include_raw` for a rare Thai word and for a rare identifier
//!    made of common words; and a query only raw chunks answer, which takes the raw fallback)
//!    and `context`.
//! 3. `memories::get_by_seqs` with as many seqs as one recall's candidate lists can produce.
//! 4. Start-up parts, timed one by one on the saved data: open + migrate, newest HLC scan,
//!    embedding count, vector index open, vector index rebuild.
//! 5. A real `alice daemon run` child on that data: time until it answers, RSS after start,
//!    bursts of 64 concurrent recalls over HTTP, RSS after them and peak RSS.
//!
//! The corpus mixes a small, dense vocabulary (every common word is in a large share of chunks,
//! the worst case for ranked full-text search) with a long tail of made-up words, Thai words and
//! code identifiers; each chunk also carries one word that never appears in a memory.

use crate::cmd::BenchArgs;
use alice_core::api::*;
use alice_core::embed::{Embedder, FakeEmbedder};
use alice_core::model::Kind;
use alice_core::store::sources::{ChunkInput, SourceInput};
use alice_core::store::{embeddings, memories, open_reader, open_writer};
use alice_core::vector::VectorIndex;
use alice_daemon::client::DaemonClient;
use alice_daemon::paths::AliceHome;
use alice_daemon::service::{Service, ServiceOptions};
use anyhow::{Context as _, Result, bail};
use serde::Serialize;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

const WORDS_EN: &[&str] = &[
    "writer", "lease", "timeout", "sqlite", "deploy", "proxy", "socks5", "protobuf", "frame",
    "index", "vector", "token", "daemon", "hook", "session", "agent", "memory", "decision", "bug",
    "fix",
];
const WORDS_TH: &[&str] = &[
    "ระบบ",
    "ความจำ",
    "เอเจนต์",
    "ค้นหา",
    "ภาษาไทย",
    "บันทึก",
    "โปรเจกต์",
    "แก้บั๊ก",
    "ตัดสินใจ",
    "ข้อมูล",
];
const CODE: &[&str] = &[
    "HostLinkReconnect",
    "writer_lease",
    "internal/teacher/jev/client.go",
    "VectorIndex::upsert",
    "parse_hlc",
];
/// Thai words found only in raw chunks, each in about 0.2% of them (2,000 chunks at 1M): rare,
/// but in many more chunks than one list returns.
const RARE_TH: &[&str] = &[
    "สัญญาเช่าบ้าน",
    "จองคิวร้าน",
    "ใบแจ้งหนี้",
    "ตารางเวรพยาบาล",
    "งบประมาณรายปี",
    "คลังสินค้า",
    "บัญชีรายรับ",
    "ประกันสุขภาพ",
    "สมัครสมาชิก",
    "ตรวจสอบสลิป",
];
/// Identifiers found only in raw chunks, each in about 0.2% of them, made of common words: as
/// a query term each is a phrase whose words are all common.
const RARE_IDENT: &[&str] = &[
    "lease_token_frame",
    "proxy_hook_index",
    "daemon_session_fix",
    "vector_agent_bug",
    "timeout_memory_decision",
    "writer_deploy_sqlite",
    "protobuf_socks5_frame",
    "index_token_hook",
    "session_bug_lease",
    "memory_proxy_daemon",
];
const SYLLABLES: &[&str] = &[
    "ka", "lo", "mi", "ra", "ten", "vo", "shi", "pu", "ne", "dar", "gu", "fe", "zo", "bin", "hal",
    "qui",
];
/// Made-up words in memories and chunks (each in ~0.4% of chunks).
const TAIL_WORDS: usize = 2_000;
/// Made-up words only chunks contain: one per chunk.
const RAW_WORDS: usize = 500;
/// Items ingested per request (the API maximum is 1,000).
const INGEST_BATCH: usize = 1_000;
/// Concurrent `remember` calls while loading.
const REMEMBER_PARALLEL: usize = 64;
/// Concurrent recalls per burst against the daemon.
const BURST: usize = 64;
/// Texts embedded to measure the embedder alone.
const EMBED_SAMPLE: usize = 1_024;
/// Calls per `get_by_seqs` measurement.
const GET_BY_SEQS_CALLS: usize = 50;
/// How long the daemon child may take to answer, and to exit after a stop.
const DAEMON_WAIT: Duration = Duration::from_secs(600);

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> usize {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0 as usize
    }

    fn below(&mut self, n: usize) -> usize {
        self.next() % n.max(1)
    }
}

/// Word `i` of a made-up vocabulary: three syllables picked by the digits of `i` in base 16.
fn made_up(prefix: &str, i: usize) -> String {
    let n = SYLLABLES.len();
    format!(
        "{prefix}{}{}{}",
        SYLLABLES[i % n],
        SYLLABLES[(i / n) % n],
        SYLLABLES[(i / (n * n)) % n]
    )
}

struct Corpus {
    rng: Rng,
    tail: Vec<String>,
    raw_only: Vec<String>,
}

impl Corpus {
    fn new() -> Self {
        Self {
            rng: Rng(0x9e37_79b9_7f4a_7c15),
            tail: (0..TAIL_WORDS).map(|i| made_up("", i)).collect(),
            raw_only: (0..RAW_WORDS).map(|i| made_up("zx", i)).collect(),
        }
    }

    fn word(&mut self) -> String {
        match self.rng.below(7) {
            0 => WORDS_TH[self.rng.below(WORDS_TH.len())].to_string(),
            1 => CODE[self.rng.below(CODE.len())].to_string(),
            2 | 3 => WORDS_EN[self.rng.below(WORDS_EN.len())].to_string(),
            _ => self.tail[self.rng.below(self.tail.len())].clone(),
        }
    }

    /// 8 to 27 words.
    fn sentence(&mut self) -> String {
        let n = 8 + self.rng.below(20);
        (0..n).map(|_| self.word()).collect::<Vec<_>>().join(" ")
    }

    /// A sentence with one raw-only word in it; one chunk in 50 also holds a rare Thai word,
    /// one in 50 a rare identifier.
    fn chunk(&mut self) -> String {
        let raw = self.raw_only[self.rng.below(self.raw_only.len())].clone();
        let mut text = format!("{} {raw} {}", self.sentence(), self.sentence());
        if self.rng.below(50) == 0 {
            text.push(' ');
            text.push_str(RARE_TH[self.rng.below(RARE_TH.len())]);
        }
        if self.rng.below(50) == 0 {
            text.push(' ');
            text.push_str(RARE_IDENT[self.rng.below(RARE_IDENT.len())]);
        }
        text
    }

    /// Three words, as an agent would search.
    fn query(&mut self) -> String {
        (0..3).map(|_| self.word()).collect::<Vec<_>>().join(" ")
    }

    /// A rare Thai word (in ~0.2% of chunks) beside a common one.
    fn thai_query(&mut self) -> String {
        let rare = RARE_TH[self.rng.below(RARE_TH.len())];
        let common = WORDS_TH[self.rng.below(WORDS_TH.len())];
        format!("{common} {rare}")
    }

    /// A rare identifier made of common words (a phrase query) beside a common word.
    fn ident_query(&mut self) -> String {
        let rare = RARE_IDENT[self.rng.below(RARE_IDENT.len())];
        let common = WORDS_EN[self.rng.below(WORDS_EN.len())];
        format!("{rare} {common}")
    }

    /// Two raw-only words: no memory matches, so recall falls back to raw chunks.
    fn fallback_query(&mut self) -> String {
        let a = self.raw_only[self.rng.below(self.raw_only.len())].clone();
        let b = self.raw_only[self.rng.below(self.raw_only.len())].clone();
        format!("{a} {b}")
    }
}

#[derive(Serialize, Default)]
struct Latency {
    samples: usize,
    remember_p50_ms: f64,
    remember_p95_ms: f64,
    recall_p50_ms: f64,
    recall_p95_ms: f64,
    recall_raw_p50_ms: f64,
    recall_raw_p95_ms: f64,
    /// `include_raw` with a rare Thai word beside a common one (the trigram list's shares).
    recall_raw_thai_p50_ms: f64,
    recall_raw_thai_p95_ms: f64,
    /// `include_raw` with a rare identifier made of common words.
    recall_raw_ident_p50_ms: f64,
    recall_raw_ident_p95_ms: f64,
    /// `include_raw` with a date filter that keeps every item: filtered lists read 400
    /// candidates instead of 100, so each term's share is at least 400 rows.
    recall_raw_filtered_p50_ms: f64,
    recall_raw_filtered_p95_ms: f64,
    recall_fallback_p50_ms: f64,
    recall_fallback_p95_ms: f64,
    /// Fallback queries whose answer came from the raw fallback.
    fallback_fired: usize,
    context_p50_ms: f64,
    context_p95_ms: f64,
}

#[derive(Serialize, Default)]
struct GetBySeqs {
    memories: usize,
    seqs_500_p50_ms: f64,
    seqs_500_p95_ms: f64,
    seqs_2000_p50_ms: f64,
    seqs_2000_p95_ms: f64,
}

#[derive(Serialize, Default)]
struct Startup {
    open_migrate_ms: f64,
    newest_hlc_ms: f64,
    embeddings_count_ms: f64,
    index_open_ms: f64,
    index_rebuild_secs: f64,
    indexed_vectors: usize,
}

/// RSS counts resident pages only: under memory pressure macOS compresses idle pages and RSS
/// drops. The physical footprint (macOS only; what Activity Monitor shows) includes them.
#[derive(Serialize, Default)]
struct DaemonRun {
    pid: u32,
    /// Spawn until it answers a verified ping (process start, open, index load, model load).
    ready_ms: f64,
    /// Spawn until the vector index is loaded (`index_ready`).
    index_ready_ms: f64,
    rss_after_start_mb: Option<f64>,
    footprint_after_start_mb: Option<f64>,
    bursts: usize,
    burst_p50_ms: f64,
    burst_p95_ms: f64,
    burst_max_ms: f64,
    burst_errors: usize,
    rss_after_bursts_mb: Option<f64>,
    footprint_after_bursts_mb: Option<f64>,
    /// Highest RSS of the daemon over its whole life (from the OS when it exits).
    peak_rss_mb: Option<f64>,
    /// Highest physical footprint over its whole life, read just before it stops.
    peak_footprint_mb: Option<f64>,
}

#[derive(Serialize, Default)]
struct Report {
    version: String,
    embedder: String,
    memories: usize,
    chunks: usize,
    /// The data came from an earlier run with the same --dir (load figures are then zero).
    reused: bool,
    ingest_secs: f64,
    ingest_chunks_per_sec: f64,
    remember_load_secs: f64,
    /// After loading, until the embedding backlog is empty and the index is ready.
    drain_secs: f64,
    /// Recalls made while the worker embedded the backlog (one every 250 ms): queries share
    /// the model with the worker and wait for at most one of its batches.
    recalls_during_drain: usize,
    recall_during_drain_p50_ms: f64,
    recall_during_drain_p95_ms: f64,
    /// Items made vector-searchable per second, end to end (load start to drained).
    indexed_per_sec: f64,
    /// The embedder alone, batches of 32 bench sentences.
    embed_per_sec: f64,
    latency: Latency,
    get_by_seqs: GetBySeqs,
    startup: Startup,
    daemon: Option<DaemonRun>,
    db_mb: f64,
    index_mb: f64,
    /// This process (it loaded the data and ran the service in-process) at the end of phase 2.
    bench_rss_mb: Option<f64>,
    bench_footprint_mb: Option<f64>,
}

fn pct(v: &mut [Duration], p: f64) -> f64 {
    if v.is_empty() {
        return 0.0;
    }
    v.sort();
    v[((v.len() as f64 * p) as usize).min(v.len() - 1)].as_secs_f64() * 1000.0
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

/// Resident set size of `pid` from `ps`; `None` where `ps` is missing (Windows).
fn rss_mb(pid: u32) -> Option<f64> {
    let out = Command::new("ps")
        .args(["-o", "rss=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    let kb: f64 = String::from_utf8(out.stdout).ok()?.trim().parse().ok()?;
    Some(kb / 1024.0)
}

/// Physical footprint of `pid` now and its lifetime maximum, in MB (macOS only).
#[cfg(target_os = "macos")]
fn footprint_mb(pid: u32) -> Option<(f64, f64)> {
    // SAFETY: proc_pid_rusage fills the zeroed rusage_info_v4 it is given, of the flavor named.
    let mut info: libc::rusage_info_v4 = unsafe { std::mem::zeroed() };
    let rc = unsafe {
        libc::proc_pid_rusage(
            pid as libc::c_int,
            libc::RUSAGE_INFO_V4,
            (&mut info as *mut libc::rusage_info_v4).cast::<libc::rusage_info_t>(),
        )
    };
    let mb = |b: u64| b as f64 / (1024.0 * 1024.0);
    (rc == 0).then(|| {
        (
            mb(info.ri_phys_footprint),
            mb(info.ri_lifetime_max_phys_footprint),
        )
    })
}

#[cfg(not(target_os = "macos"))]
fn footprint_mb(_pid: u32) -> Option<(f64, f64)> {
    None
}

/// Peak RSS of the largest child this process has waited for.
#[cfg(unix)]
fn children_peak_rss_mb() -> Option<f64> {
    // SAFETY: getrusage only writes into the zeroed struct it is given.
    let mut u: libc::rusage = unsafe { std::mem::zeroed() };
    if unsafe { libc::getrusage(libc::RUSAGE_CHILDREN, &mut u) } != 0 {
        return None;
    }
    // ru_maxrss is in bytes on macOS and in kilobytes elsewhere.
    let raw = u.ru_maxrss as f64;
    Some(if cfg!(target_os = "macos") {
        raw / (1024.0 * 1024.0)
    } else {
        raw / 1024.0
    })
}

#[cfg(not(unix))]
fn children_peak_rss_mb() -> Option<f64> {
    None
}

/// Names the embedder a bench directory was loaded with; its presence marks bench data.
const MARKER: &str = "bench-embedder";

/// The daemon lock of `home`, or an error when a daemon (or another bench) holds it.
fn take_lock(home: &AliceHome) -> Result<std::fs::File> {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(home.lock_file())?;
    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(std::fs::TryLockError::WouldBlock) => bail!(
            "{} is in use by a running daemon or bench; stop it (ALICE_HOME={} alice daemon stop) and retry",
            home.root().display(),
            home.root().display()
        ),
        Err(std::fs::TryLockError::Error(e)) => Err(e.into()),
    }
}

fn same_dir(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

fn file_mb(path: &Path) -> f64 {
    std::fs::metadata(path)
        .map(|m| m.len() as f64 / (1024.0 * 1024.0))
        .unwrap_or(0.0)
}

/// `user_home` is the ALICE_HOME in use: only its model is read, and only with `--real`.
pub fn bench(user_home: AliceHome, args: BenchArgs, json: bool) -> Result<()> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let user_models = user_home.models_dir();
    // Removed on return unless --dir names a directory to keep.
    let mut tmp = None;
    let root = match &args.dir {
        Some(dir) => {
            let dir = std::path::absolute(dir)?;
            if same_dir(&dir, user_home.root()) {
                bail!(
                    "--dir {} is the ALICE_HOME in use; choose a directory for bench data only",
                    dir.display()
                );
            }
            // Only bench data is ever written to: anything else (another Alice home, say) is
            // refused before a single file changes.
            let has_files = std::fs::read_dir(&dir).is_ok_and(|mut d| d.next().is_some());
            if has_files && !dir.join(MARKER).exists() {
                bail!(
                    "--dir {} is not bench data (no {MARKER} file in it); choose a new or empty directory",
                    dir.display()
                );
            }
            dir
        }
        None => tmp
            .insert(tempfile::Builder::new().prefix("alice-bench-").tempdir()?)
            .path()
            .to_path_buf(),
    };
    let home = AliceHome::new(root);
    std::fs::create_dir_all(home.root())?;
    // The daemon's single-instance lock, held while this process writes the data itself: a
    // daemon serving the directory would be a second writer. Released before the bench starts
    // its own daemon there.
    let lock = take_lock(&home)?;
    home.ensure_dirs()?;
    // Data loaded with one embedder is meaningless with another.
    let marker = home.root().join(MARKER);
    let embedder = if args.real {
        alice_embed::MODEL_ID
    } else {
        "fake"
    };
    match std::fs::read_to_string(&marker) {
        Ok(m) if m.trim() != embedder => bail!(
            "{} holds bench data for the {} embedder; use another --dir",
            home.root().display(),
            m.trim()
        ),
        Ok(_) => {}
        Err(_) => std::fs::write(&marker, embedder)?,
    }
    let real = args.real;
    let models = user_models.clone();
    let make = move || -> alice_core::Result<Arc<dyn Embedder>> {
        if real {
            Ok(Arc::new(alice_embed::FastEmbedder::new(&models)?))
        } else {
            Ok(Arc::new(FakeEmbedder::new(384)))
        }
    };
    let mut report = Report {
        version: env!("CARGO_PKG_VERSION").into(),
        embedder: embedder.into(),
        memories: args.memories,
        chunks: args.chunks,
        ..Default::default()
    };
    let mut corpus = Corpus::new();

    eprintln!("bench: measuring the embedder alone");
    let probe = make()?;
    let model = (probe.model_id().to_string(), probe.dim());
    report.embed_per_sec = embed_throughput(probe.as_ref(), &mut corpus)?;
    drop(probe);

    let svc = Service::start(home.clone(), &make, ServiceOptions::default())?;
    rt.block_on(load(&svc, &args, &mut corpus, &mut report))?;
    eprintln!("bench: measuring latency");
    report.latency = rt.block_on(latency(&svc, args.samples, &mut corpus))?;
    eprintln!("bench: measuring get_by_seqs");
    report.get_by_seqs = get_by_seqs(&home)?;
    report.bench_rss_mb = rss_mb(std::process::id());
    report.bench_footprint_mb = footprint_mb(std::process::id()).map(|f| f.0);
    svc.shutdown();
    drop(svc);
    report.db_mb = file_mb(&home.db());
    let base = home.vector_file(&model.0);
    report.index_mb = file_mb(&base.with_extension("memories.usearch"))
        + file_mb(&base.with_extension("chunks.usearch"));

    eprintln!("bench: timing the start-up parts");
    report.startup = startup_parts(&home, &model.0, model.1)?;

    drop(lock);
    eprintln!("bench: starting a daemon on the data");
    report.daemon = daemon_run(&rt, &home, real, &user_models, &mut corpus)?;

    drop(tmp);
    let text = render(&report);
    if let Some(path) = &args.out {
        std::fs::write(path, serde_json::to_vec_pretty(&report)?)
            .with_context(|| format!("cannot write {}", path.display()))?;
    }
    if json {
        crate::cmd::out(&format!("{}\n", serde_json::to_string_pretty(&report)?))
    } else {
        crate::cmd::out(&text)
    }
}

fn embed_throughput(e: &dyn Embedder, corpus: &mut Corpus) -> Result<f64> {
    let texts: Vec<String> = (0..EMBED_SAMPLE).map(|_| corpus.sentence()).collect();
    let t = Instant::now();
    for batch in texts.chunks(32) {
        e.embed_passages(batch)?;
    }
    Ok(EMBED_SAMPLE as f64 / t.elapsed().as_secs_f64().max(1e-9))
}

async fn load(
    svc: &Arc<Service>,
    args: &BenchArgs,
    corpus: &mut Corpus,
    report: &mut Report,
) -> Result<()> {
    let h = svc.health().await?;
    if h.chunks as usize >= args.chunks && h.memories as usize >= args.memories {
        eprintln!(
            "bench: reusing {} chunks and {} memories in {}",
            h.chunks,
            h.memories,
            svc.home().root().display()
        );
        report.reused = true;
        // A rebuilt index, or memories the last run left unembedded.
        loop {
            let h = svc.health().await?;
            if h.embed_backlog == 0 && h.index_ready {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    }
    if h.chunks > 0 || h.memories > 0 {
        bail!(
            "{} already holds other data ({} chunks, {} memories); use an empty directory",
            svc.home().root().display(),
            h.chunks,
            h.memories
        );
    }
    let started = Instant::now();
    let mut last_note = Instant::now();
    for b in 0..args.chunks.div_ceil(INGEST_BATCH) {
        let n = (args.chunks - b * INGEST_BATCH).min(INGEST_BATCH);
        let chunks = (0..n)
            .map(|i| ChunkInput {
                ord: (b * INGEST_BATCH + i) as i64,
                text: corpus.chunk(),
                ..Default::default()
            })
            .collect();
        svc.ingest(IngestRequest {
            source: SourceInput {
                uri: format!("bench-session-{}", b / 50),
                ..Default::default()
            },
            chunks,
            ..Default::default()
        })
        .await?;
        if last_note.elapsed() > Duration::from_secs(30) {
            eprintln!(
                "bench: ingested {} of {} chunks",
                (b + 1) * INGEST_BATCH,
                args.chunks
            );
            last_note = Instant::now();
        }
    }
    report.ingest_secs = started.elapsed().as_secs_f64();
    report.ingest_chunks_per_sec = args.chunks as f64 / report.ingest_secs.max(1e-9);

    let t = Instant::now();
    let mut i = 0;
    while i < args.memories {
        let mut set = tokio::task::JoinSet::new();
        for j in i..(i + REMEMBER_PARALLEL).min(args.memories) {
            let svc = svc.clone();
            let req = RememberRequest {
                content: format!("{} #{j}", corpus.sentence()),
                kind: Some(Kind::ALL[j % Kind::ALL.len()]),
                importance: Some((1 + j % 5) as u8),
                ..Default::default()
            };
            set.spawn(async move { svc.remember(req).await });
        }
        while let Some(r) = set.join_next().await {
            r??;
        }
        i += REMEMBER_PARALLEL;
    }
    report.remember_load_secs = t.elapsed().as_secs_f64();

    eprintln!("bench: waiting for embeddings and the index");
    let t = Instant::now();
    let mut last_note = Instant::now();
    let mut during = Vec::new();
    loop {
        let h = svc.health().await?;
        if h.embed_backlog == 0 && h.index_ready {
            break;
        }
        let (res, d) = timed(svc.recall(RecallRequest {
            query: corpus.query(),
            ..Default::default()
        }))
        .await;
        res?;
        during.push(d);
        if last_note.elapsed() > Duration::from_secs(30) {
            eprintln!("bench: embedding backlog {}", h.embed_backlog);
            last_note = Instant::now();
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    report.drain_secs = t.elapsed().as_secs_f64();
    report.recalls_during_drain = during.len();
    report.recall_during_drain_p50_ms = pct(&mut during, 0.50);
    report.recall_during_drain_p95_ms = pct(&mut during, 0.95);
    report.indexed_per_sec =
        (args.chunks + args.memories) as f64 / started.elapsed().as_secs_f64().max(1e-9);
    Ok(())
}

async fn timed<T>(f: impl std::future::Future<Output = T>) -> (T, Duration) {
    let t = Instant::now();
    let out = f.await;
    (out, t.elapsed())
}

async fn latency(svc: &Arc<Service>, samples: usize, corpus: &mut Corpus) -> Result<Latency> {
    let (mut w, mut r, mut rr, mut rf, mut c) =
        (Vec::new(), Vec::new(), Vec::new(), Vec::new(), Vec::new());
    let (mut rt, mut ri, mut rfl) = (Vec::new(), Vec::new(), Vec::new());
    let mut fallback_fired = 0;
    for i in 0..samples {
        let (res, d) = timed(svc.remember(RememberRequest {
            content: format!("bench write {i} {}", corpus.sentence()),
            ..Default::default()
        }))
        .await;
        res?;
        w.push(d);

        let q = corpus.query();
        let (res, d) = timed(svc.recall(RecallRequest {
            query: q.clone(),
            ..Default::default()
        }))
        .await;
        res?;
        r.push(d);

        let (res, d) = timed(svc.recall(RecallRequest {
            query: q.clone(),
            include_raw: true,
            ..Default::default()
        }))
        .await;
        res?;
        rr.push(d);

        let (res, d) = timed(svc.recall(RecallRequest {
            query: q,
            include_raw: true,
            since: Some("2000-01-01".into()),
            ..Default::default()
        }))
        .await;
        res?;
        rfl.push(d);

        for (query, times) in [
            (corpus.thai_query(), &mut rt),
            (corpus.ident_query(), &mut ri),
        ] {
            let (res, d) = timed(svc.recall(RecallRequest {
                query,
                include_raw: true,
                ..Default::default()
            }))
            .await;
            res?;
            times.push(d);
        }

        let (res, d) = timed(svc.recall(RecallRequest {
            query: corpus.fallback_query(),
            ..Default::default()
        }))
        .await;
        if res?.raw_fallback {
            fallback_fired += 1;
        }
        rf.push(d);

        let (res, d) = timed(svc.context(ContextRequest::default())).await;
        res?;
        c.push(d);
    }
    Ok(Latency {
        samples,
        remember_p50_ms: pct(&mut w, 0.50),
        remember_p95_ms: pct(&mut w, 0.95),
        recall_p50_ms: pct(&mut r, 0.50),
        recall_p95_ms: pct(&mut r, 0.95),
        recall_raw_p50_ms: pct(&mut rr, 0.50),
        recall_raw_p95_ms: pct(&mut rr, 0.95),
        recall_raw_thai_p50_ms: pct(&mut rt, 0.50),
        recall_raw_thai_p95_ms: pct(&mut rt, 0.95),
        recall_raw_ident_p50_ms: pct(&mut ri, 0.50),
        recall_raw_ident_p95_ms: pct(&mut ri, 0.95),
        recall_raw_filtered_p50_ms: pct(&mut rfl, 0.50),
        recall_raw_filtered_p95_ms: pct(&mut rfl, 0.95),
        recall_fallback_p50_ms: pct(&mut rf, 0.50),
        recall_fallback_p95_ms: pct(&mut rf, 0.95),
        fallback_fired,
        context_p50_ms: pct(&mut c, 0.50),
        context_p95_ms: pct(&mut c, 0.95),
    })
}

/// `get_by_seqs` with as many seqs as one recall's memory candidates (500 unfiltered, 2,000
/// with filters: four lexical lists and the vector list, 100 or 400 each).
fn get_by_seqs(home: &AliceHome) -> Result<GetBySeqs> {
    let conn = open_reader(&home.db())?;
    let (count, max_seq): (i64, Option<i64>) =
        conn.query_row("SELECT count(*), max(seq) FROM memories", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })?;
    let mut out = GetBySeqs {
        memories: count as usize,
        ..Default::default()
    };
    let Some(max_seq) = max_seq else {
        return Ok(out);
    };
    let mut rng = Rng(0x2545_f491_4f6c_dd1d);
    let mut measure = |n: usize| -> Result<(f64, f64)> {
        let mut times = Vec::with_capacity(GET_BY_SEQS_CALLS);
        for _ in 0..GET_BY_SEQS_CALLS {
            let seqs: Vec<i64> = (0..n)
                .map(|_| 1 + rng.below(max_seq as usize) as i64)
                .collect();
            let t = Instant::now();
            memories::get_by_seqs(&conn, &seqs)?;
            times.push(t.elapsed());
        }
        Ok((pct(&mut times, 0.50), pct(&mut times, 0.95)))
    };
    (out.seqs_500_p50_ms, out.seqs_500_p95_ms) = measure(500)?;
    (out.seqs_2000_p50_ms, out.seqs_2000_p95_ms) = measure(2_000)?;
    Ok(out)
}

/// What `Service::start` and the writer do before serving, one part at a time.
fn startup_parts(home: &AliceHome, model: &str, dim: usize) -> Result<Startup> {
    let mut s = Startup::default();
    let t = Instant::now();
    let conn = open_writer(&home.db())?;
    s.open_migrate_ms = ms(t.elapsed());
    let t = Instant::now();
    alice_daemon::writer::newest_hlc(&conn)?;
    s.newest_hlc_ms = ms(t.elapsed());
    let t = Instant::now();
    let stored = embeddings::count(&conn, model)?;
    s.embeddings_count_ms = ms(t.elapsed());
    drop(conn);
    let t = Instant::now();
    let idx = VectorIndex::open(&home.vector_file(model), dim)?;
    s.index_open_ms = ms(t.elapsed());
    s.indexed_vectors = idx.len();
    if idx.len() != stored {
        bail!(
            "the saved index holds {} vectors but {stored} are stored",
            idx.len()
        );
    }
    drop(idx);
    let reader = open_reader(&home.db())?;
    let t = Instant::now();
    let rebuilt = VectorIndex::rebuild_from(&reader, model, dim)?;
    s.index_rebuild_secs = t.elapsed().as_secs_f64();
    if rebuilt.len() != stored {
        bail!(
            "the rebuilt index holds {} of {stored} vectors",
            rebuilt.len()
        );
    }
    Ok(s)
}

/// Make the user's downloaded model visible in the bench home without copying it.
#[cfg(unix)]
fn link_model(home: &AliceHome, user_models: &Path) -> Result<bool> {
    let src = alice_embed::model_dir(user_models);
    let dest = alice_embed::model_dir(&home.models_dir());
    if dest.exists() {
        return Ok(true);
    }
    std::os::unix::fs::symlink(&src, &dest)
        .with_context(|| format!("cannot link {} into the bench home", src.display()))?;
    Ok(true)
}

#[cfg(not(unix))]
fn link_model(_home: &AliceHome, _user_models: &Path) -> Result<bool> {
    Ok(false)
}

/// Waits for `child` to exit; kills it after `DAEMON_WAIT`.
fn reap(child: &mut Child) -> Result<()> {
    let deadline = Instant::now() + DAEMON_WAIT;
    while child.try_wait()?.is_none() {
        if Instant::now() >= deadline {
            let _ = child.kill();
            child.wait()?;
            bail!("the bench daemon did not stop in time and was killed");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    Ok(())
}

fn daemon_run(
    rt: &tokio::runtime::Runtime,
    home: &AliceHome,
    real: bool,
    user_models: &Path,
    corpus: &mut Corpus,
) -> Result<Option<DaemonRun>> {
    if real && !link_model(home, user_models)? {
        eprintln!("bench: skipping the daemon run (the model cannot be linked on this OS)");
        return Ok(None);
    }
    let log = std::fs::File::create(home.logs_dir().join("daemon.log"))?;
    let mut cmd = Command::new(std::env::current_exe()?);
    cmd.args(["daemon", "run"])
        .env("ALICE_HOME", home.root())
        .env("ALICE_LOG", "info")
        .stdin(Stdio::null())
        .stdout(Stdio::from(log.try_clone()?))
        .stderr(Stdio::from(log));
    if real {
        cmd.env_remove("ALICE_EMBEDDER");
    } else {
        cmd.env("ALICE_EMBEDDER", "fake");
    }
    let queries: Vec<RecallRequest> = (0..BURST)
        .map(|i| match i % 3 {
            0 => RecallRequest {
                query: corpus.query(),
                ..Default::default()
            },
            1 => RecallRequest {
                query: corpus.query(),
                include_raw: true,
                ..Default::default()
            },
            _ => RecallRequest {
                query: corpus.fallback_query(),
                ..Default::default()
            },
        })
        .collect();
    let started = Instant::now();
    let mut child = cmd.spawn().context("cannot start the bench daemon")?;
    let pid = child.id();
    let run = rt.block_on(async {
        let client = loop {
            if let Some(c) = DaemonClient::try_existing(home).await {
                break c;
            }
            if child.try_wait()?.is_some() {
                bail!(
                    "the bench daemon exited; see {}",
                    home.logs_dir().join("daemon.log").display()
                );
            }
            if started.elapsed() > DAEMON_WAIT {
                bail!("the bench daemon did not answer in time");
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        };
        let mut run = DaemonRun {
            pid,
            ready_ms: ms(started.elapsed()),
            ..Default::default()
        };
        while !client.health().await?.index_ready {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        run.index_ready_ms = ms(started.elapsed());
        run.rss_after_start_mb = rss_mb(pid);
        run.footprint_after_start_mb = footprint_mb(pid).map(|f| f.0);
        let mut times = Vec::new();
        for _ in 0..4 {
            let mut set = tokio::task::JoinSet::new();
            for q in &queries {
                let (c, q) = (client.clone(), q.clone());
                set.spawn(async move { timed(c.recall(&q)).await });
            }
            while let Some(r) = set.join_next().await {
                let (res, d) = r?;
                match res {
                    Ok(_) => times.push(d),
                    Err(e) => {
                        run.burst_errors += 1;
                        eprintln!("bench: burst recall failed: {e}");
                    }
                }
            }
            run.bursts += 1;
        }
        run.burst_p50_ms = pct(&mut times, 0.50);
        run.burst_p95_ms = pct(&mut times, 0.95);
        run.burst_max_ms = pct(&mut times, 1.0);
        run.rss_after_bursts_mb = rss_mb(pid);
        if let Some((now, max)) = footprint_mb(pid) {
            run.footprint_after_bursts_mb = Some(now);
            run.peak_footprint_mb = Some(max);
        }
        client.shutdown().await?;
        anyhow::Ok(run)
    });
    if run.is_err() {
        // It may not have been asked to stop: do not wait for it.
        let _ = child.kill();
    }
    let reaped = reap(&mut child);
    let mut run = run?;
    reaped?;
    run.peak_rss_mb = children_peak_rss_mb();
    Ok(Some(run))
}

fn opt_mb(v: Option<f64>) -> String {
    v.map(|v| format!("{v:.0} MB"))
        .unwrap_or_else(|| "n/a".into())
}

fn render(r: &Report) -> String {
    let l = &r.latency;
    let s = &r.startup;
    let g = &r.get_by_seqs;
    let mut out = format!(
        "embedder        {}\n\
         data            {} memories, {} chunks (db {:.0} MB, index {:.0} MB)\n\
         ingest          {:.1} s ({:.0} chunks/s); memories {:.1} s; drain {:.1} s\n\
         recall in drain p50 {:.2} ms · p95 {:.2} ms ({} calls while the backlog was embedded)\n\
         embedding       {:.0} items/s (embedder alone); {:.0} items/s end to end\n\
         remember        p50 {:.2} ms · p95 {:.2} ms (budget 50)\n\
         recall          p50 {:.2} ms · p95 {:.2} ms (budget 200)\n\
         recall raw      p50 {:.2} ms · p95 {:.2} ms (include_raw)\n\
         raw, rare Thai  p50 {:.2} ms · p95 {:.2} ms (include_raw)\n\
         raw, identifier p50 {:.2} ms · p95 {:.2} ms (include_raw)\n\
         raw, filtered   p50 {:.2} ms · p95 {:.2} ms (include_raw, since 2000-01-01)\n\
         recall fallback p50 {:.2} ms · p95 {:.2} ms ({} of {} fell back)\n\
         context         p50 {:.2} ms · p95 {:.2} ms (budget 150)\n\
         get_by_seqs     500: p50 {:.2} ms · 2000: p50 {:.2} ms ({} memories)\n\
         start-up parts  open+migrate {:.1} ms · newest hlc {:.1} ms · count {:.1} ms · index open {:.1} ms · rebuild {:.1} s\n\
         bench process   RSS {} · footprint {}\n",
        r.embedder,
        r.memories,
        r.chunks,
        r.db_mb,
        r.index_mb,
        r.ingest_secs,
        r.ingest_chunks_per_sec,
        r.remember_load_secs,
        r.drain_secs,
        r.recall_during_drain_p50_ms,
        r.recall_during_drain_p95_ms,
        r.recalls_during_drain,
        r.embed_per_sec,
        r.indexed_per_sec,
        l.remember_p50_ms,
        l.remember_p95_ms,
        l.recall_p50_ms,
        l.recall_p95_ms,
        l.recall_raw_p50_ms,
        l.recall_raw_p95_ms,
        l.recall_raw_thai_p50_ms,
        l.recall_raw_thai_p95_ms,
        l.recall_raw_ident_p50_ms,
        l.recall_raw_ident_p95_ms,
        l.recall_raw_filtered_p50_ms,
        l.recall_raw_filtered_p95_ms,
        l.recall_fallback_p50_ms,
        l.recall_fallback_p95_ms,
        l.fallback_fired,
        l.samples,
        l.context_p50_ms,
        l.context_p95_ms,
        g.seqs_500_p50_ms,
        g.seqs_2000_p50_ms,
        g.memories,
        s.open_migrate_ms,
        s.newest_hlc_ms,
        s.embeddings_count_ms,
        s.index_open_ms,
        s.index_rebuild_secs,
        opt_mb(r.bench_rss_mb),
        opt_mb(r.bench_footprint_mb),
    );
    if let Some(d) = &r.daemon {
        out.push_str(&format!(
            "daemon          ready {:.0} ms · index ready {:.0} ms · after start RSS {} · footprint {}\n\
             daemon bursts   {}x{} recalls: p50 {:.1} ms · p95 {:.1} ms · max {:.1} ms · {} errors\n\
             daemon memory   after bursts RSS {} · footprint {}; peak RSS {} · footprint {} (budget 800 MB at 1M chunks)\n",
            d.ready_ms,
            d.index_ready_ms,
            opt_mb(d.rss_after_start_mb),
            opt_mb(d.footprint_after_start_mb),
            d.bursts,
            BURST,
            d.burst_p50_ms,
            d.burst_p95_ms,
            d.burst_max_ms,
            d.burst_errors,
            opt_mb(d.rss_after_bursts_mb),
            opt_mb(d.footprint_after_bursts_mb),
            opt_mb(d.peak_rss_mb),
            opt_mb(d.peak_footprint_mb),
        ));
    }
    out
}
