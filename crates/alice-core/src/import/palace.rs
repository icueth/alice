//! Reading a MemPalace palace: the one audited code path that opens the owner's files.
//!
//! `snapshot()` copies the palace's SQLite files with SQLite's backup API (the source is opened
//! read-only and is never modified); everything after that reads the copy.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use rusqlite::backup::{Backup, StepResult};
use rusqlite::{Connection, OpenFlags, params_from_iter};

use crate::error::{AliceError, Result};

/// Collection that holds the drawers in a MemPalace database.
pub const DEFAULT_COLLECTION: &str = "mempalace_drawers";

const MAIN_FILE: &str = "sqlite_exact.sqlite3";
const CHROMA_FILE: &str = "chroma.sqlite3";
const KG_FILE: &str = "knowledge_graph.sqlite3";
const TUNNELS_FILE: &str = "tunnels.json";
const TUNNELS_MAX_BYTES: u64 = 16 * 1024 * 1024;
const SNAPSHOT_TIMEBOX: Duration = Duration::from_secs(600);
const STEP_PAGES: i32 = 500;
/// Restarts tolerated before the copy switches to one single step.
const RESTARTS_BEFORE_SINGLE_STEP: u32 = 3;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct DrawerMeta {
    pub wing: String,
    pub room: String,
    pub hall: Option<String>,
    pub source_file: Option<String>,
    pub added_by: Option<String>,
    pub ingest_mode: Option<String>,
    pub filed_at: Option<String>,
    pub authored_at: Option<String>,
    pub chunk_index: Option<i64>,
    pub agent: Option<String>,
    pub date: Option<String>,
    pub topic: Option<String>,
    pub entry_type: Option<String>,
    pub parent_drawer_id: Option<String>,
    /// True when the metadata JSON could not be parsed at all (the drawer is then skipped, never guessed at).
    pub unreadable: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Drawer {
    pub id: String,
    pub text: String,
    pub meta: DrawerMeta,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    SqliteExact,
    Chroma,
}

#[derive(Debug, Clone)]
pub struct PalaceInfo {
    pub backend: Backend,
    pub path: PathBuf,
    pub drawers: u64,
}

#[derive(Debug, Clone)]
pub struct Snapshot {
    pub dir: PathBuf,
    pub backend: Backend,
    pub source: PathBuf,
    /// Copies of the knowledge graph and the tunnels file made into `dir`, when the palace has them.
    pub kg: Option<PathBuf>,
    pub tunnels: Option<PathBuf>,
    /// Facts the caller may show, e.g. "palace is in WAL mode; snapshot read through SQLite's WAL".
    pub notes: Vec<String>,
}

pub trait PalaceReader: Send {
    fn info(&self) -> &PalaceInfo;
    /// Drawers with id greater than `after` (byte order), at most `limit`, optionally only those filed at or after `since`.
    fn page(&self, after: Option<&str>, limit: usize, since: Option<&str>) -> Result<Vec<Drawer>>;
    /// Drawers by id (missing ids are skipped).
    fn get(&self, ids: &[String]) -> Result<Vec<Drawer>>;
    /// (wing, drawer count) over the whole palace.
    fn wing_counts(&self) -> Result<Vec<(String, u64)>>;
}

pub fn detect_backend(palace: &Path) -> Result<Backend> {
    if !palace.exists() {
        return Err(AliceError::invalid(format!(
            "{} does not exist; pass --palace <folder> or run `mempalace status` to see where your palace is",
            palace.display()
        )));
    }
    if !palace.is_dir() {
        return Err(AliceError::invalid(format!(
            "{} is a file, not a palace folder; pass the folder that holds {MAIN_FILE} with --palace",
            palace.display()
        )));
    }
    if palace.join(MAIN_FILE).is_file() {
        Ok(Backend::SqliteExact)
    } else if palace.join(CHROMA_FILE).is_file() {
        Ok(Backend::Chroma)
    } else {
        Err(AliceError::invalid(format!(
            "{} is not a MemPalace palace (no {MAIN_FILE} or {CHROMA_FILE}); pass the folder that holds it with --palace",
            palace.display()
        )))
    }
}

/// Copies the palace (and its knowledge graph and tunnels, when present) into `dest_dir`.
///
/// Never overwrites or deletes an existing file: `dest_dir` must not be the palace folder (or
/// `mempalace_home`, also through symlinks) and every file written is created new. On any error
/// the files this call created are removed.
pub fn snapshot(palace: &Path, dest_dir: &Path, mempalace_home: Option<&Path>) -> Result<Snapshot> {
    let backend = detect_backend(palace)?;
    refuse_shared_destination(palace, dest_dir, mempalace_home)?;
    let mut written: Vec<PathBuf> = Vec::new();
    let r = snapshot_inner(palace, dest_dir, mempalace_home, backend, &mut written);
    if r.is_err() {
        for p in &written {
            remove_db_files(p);
        }
    }
    r
}

/// `path` made absolute and symlink-free as far as it exists: the nearest existing ancestor is
/// canonicalized and the part that does not exist yet is appended. Nothing is created.
fn resolve_lenient(path: &Path) -> PathBuf {
    use std::path::Component;
    let start = if path.is_absolute() {
        PathBuf::new()
    } else {
        std::env::current_dir().unwrap_or_default()
    };
    // `..` is applied to the real folder when there is one (so `link/..` follows the link),
    // else lexically.
    let mut abs = start;
    for c in path.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                if let Ok(real) = abs.canonicalize() {
                    abs = real;
                }
                abs.pop();
            }
            other => abs.push(other.as_os_str()),
        }
    }
    let mut rest: Vec<std::ffi::OsString> = Vec::new();
    loop {
        if let Ok(c) = abs.canonicalize() {
            abs = c;
            break;
        }
        match (abs.file_name().map(|n| n.to_os_string()), abs.parent()) {
            (Some(n), Some(parent)) => {
                if n != "." {
                    rest.push(n);
                }
                abs = parent.to_path_buf();
            }
            _ => break,
        }
    }
    for n in rest.into_iter().rev() {
        abs.push(n);
    }
    abs
}

/// The destination must be none of: the palace folder, the MemPalace home (given, or the palace's
/// parent when that is a `.mempalace` folder), a folder inside any of them, or a folder that holds
/// the palace. Symlinks are followed, and a destination that does not exist yet is judged by
/// its nearest existing ancestor.
fn refuse_shared_destination(palace: &Path, dest_dir: &Path, home: Option<&Path>) -> Result<()> {
    let dest = resolve_lenient(dest_dir);
    let palace_r = resolve_lenient(palace);
    let mut protected = vec![("the palace folder", palace_r.clone())];
    if let Some(h) = home {
        protected.push(("the MemPalace home", resolve_lenient(h)));
    }
    if let Some(parent) = palace_r.parent()
        && parent.file_name().is_some_and(|n| n == ".mempalace")
    {
        protected.push(("the MemPalace home", parent.to_path_buf()));
    }
    for (what, dir) in &protected {
        if dest == *dir {
            return Err(AliceError::invalid(format!(
                "{} is {what}; the snapshot must go to a different, empty folder. Nothing was copied.",
                dest_dir.display()
            )));
        }
        if dest.starts_with(dir) {
            return Err(AliceError::invalid(format!(
                "{} is inside {what} ({}); the snapshot must go outside it. Nothing was copied.",
                dest_dir.display(),
                dir.display()
            )));
        }
    }
    if palace_r.starts_with(&dest) {
        return Err(AliceError::invalid(format!(
            "{} holds the palace folder; the snapshot must go to a folder of its own. Nothing was copied.",
            dest_dir.display()
        )));
    }
    Ok(())
}

fn remove_db_files(p: &Path) {
    let _ = std::fs::remove_file(p);
    for suffix in ["-journal", "-wal", "-shm"] {
        let mut s = p.as_os_str().to_owned();
        s.push(suffix);
        let _ = std::fs::remove_file(PathBuf::from(s));
    }
}

fn snapshot_inner(
    palace: &Path,
    dest_dir: &Path,
    home: Option<&Path>,
    backend: Backend,
    written: &mut Vec<PathBuf>,
) -> Result<Snapshot> {
    let created = !dest_dir.exists();
    std::fs::create_dir_all(dest_dir)?;
    #[cfg(unix)]
    if created {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dest_dir, std::fs::Permissions::from_mode(0o700))?;
    }
    let limits = Limits::production();
    let mut notes = Vec::new();

    let main_name = match backend {
        Backend::SqliteExact => MAIN_FILE,
        Backend::Chroma => CHROMA_FILE,
    };
    let source = palace.join(main_name);
    let dst = dest_dir.join(main_name);
    // copy_db removes its own partial output on error and refuses to touch an existing file,
    // so a path joins `written` only once this call has fully created it.
    let stats = copy_db(&source, &dst, &limits, &mut || {})?;
    written.push(dst.clone());
    note_stats(&mut notes, main_name, &stats);

    // The knowledge graph: the palace folder first, then ~/.mempalace. A failure here must not
    // lose the drawers, so it becomes a note (and does not fall through to the next candidate).
    let mut kg = None;
    let kg_candidates = [Some(palace.to_path_buf()), home.map(Path::to_path_buf)];
    if let Some(src) = kg_candidates
        .iter()
        .flatten()
        .map(|d| d.join(KG_FILE))
        .find(|p| p.is_file())
    {
        let dst = dest_dir.join(KG_FILE);
        match copy_db(&src, &dst, &limits, &mut || {}) {
            Ok(st) => {
                note_stats(&mut notes, KG_FILE, &st);
                written.push(dst.clone());
                kg = Some(dst);
            }
            Err(e) => notes.push(format!("the knowledge graph was not copied: {e}")),
        }
    }

    // Tunnels: MemPalace keeps this next to the palace folder; older layouts put it inside.
    let mut tunnels = None;
    let parent = palace
        .canonicalize()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf));
    let t_candidates = [
        parent.map(|p| p.join(TUNNELS_FILE)),
        Some(palace.join(TUNNELS_FILE)),
        home.map(|h| h.join(TUNNELS_FILE)),
    ];
    if let Some(src) = t_candidates.into_iter().flatten().find(|p| p.is_file()) {
        let dst = dest_dir.join(TUNNELS_FILE);
        match copy_small_file(&src, &dst) {
            Ok(()) => {
                written.push(dst.clone());
                tunnels = Some(dst);
            }
            Err(e) => notes.push(format!("{TUNNELS_FILE} was not copied: {e}")),
        }
    }

    Ok(Snapshot {
        dir: dest_dir.to_path_buf(),
        backend,
        source,
        kg,
        tunnels,
        notes,
    })
}

fn note_stats(notes: &mut Vec<String>, name: &str, st: &CopyStats) {
    if st.wal {
        notes.push(format!(
            "{name}: palace is in WAL mode; snapshot read through SQLite's WAL"
        ));
    }
    if st.restarts > 0 {
        notes.push(format!("{name}: copy restarted {} times", st.restarts));
    }
}

/// Creates `dst` new (never overwrites). A failed copy leaves nothing behind.
fn copy_small_file(src: &Path, dst: &Path) -> Result<()> {
    let too_big = || AliceError::invalid(format!("{} is larger than 16 MB", src.display()));
    if std::fs::metadata(src)?.len() > TUNNELS_MAX_BYTES {
        return Err(too_big());
    }
    let mut buf = Vec::new();
    std::fs::File::open(src)?
        .take(TUNNELS_MAX_BYTES + 1)
        .read_to_end(&mut buf)?;
    if buf.len() as u64 > TUNNELS_MAX_BYTES {
        return Err(too_big());
    }
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(dst)
        .map_err(|e| exists_error(dst, e))?;
    if let Err(e) = std::io::Write::write_all(&mut f, &buf) {
        drop(f);
        let _ = std::fs::remove_file(dst);
        return Err(e.into());
    }
    Ok(())
}

fn exists_error(dst: &Path, e: std::io::Error) -> AliceError {
    if e.kind() == std::io::ErrorKind::AlreadyExists {
        AliceError::invalid(format!(
            "{} already exists; the snapshot never overwrites a file. Use an empty folder.",
            dst.display()
        ))
    } else {
        e.into()
    }
}

#[derive(Debug)]
struct CopyStats {
    wal: bool,
    restarts: u32,
}

/// Knobs of the copy loop; tests shrink them.
struct Limits {
    deadline: Instant,
    /// Pages per normal step.
    step_pages: i32,
    /// After this many restarts the copy switches to one single step.
    single_step_after: u32,
    /// A non-WAL source that restarts more often than this is being written to.
    max_restarts_non_wal: u32,
    /// Consecutive Busy/Locked answers tolerated (non-WAL) before giving up.
    busy_retries: u32,
    busy_sleep: Duration,
}

impl Limits {
    fn production() -> Self {
        Self {
            deadline: Instant::now() + SNAPSHOT_TIMEBOX,
            step_pages: STEP_PAGES,
            single_step_after: RESTARTS_BEFORE_SINGLE_STEP,
            max_restarts_non_wal: RESTARTS_BEFORE_SINGLE_STEP,
            busy_retries: 100,
            busy_sleep: Duration::from_millis(50),
        }
    }
}

/// Header bytes 18 and 19 are 2 when the database is in WAL mode.
fn is_wal(path: &Path) -> bool {
    let mut h = [0u8; 20];
    std::fs::File::open(path)
        .and_then(|mut f| f.read_exact(&mut h))
        .map(|()| h[18] == 2 && h[19] == 2)
        .unwrap_or(false)
}

/// The "cannot be read as a database" sentence for corruption, a plain sentence for anything else.
fn copy_error(src: &Path, e: rusqlite::Error) -> AliceError {
    use rusqlite::ErrorCode;
    if matches!(
        e.sqlite_error_code(),
        Some(ErrorCode::NotADatabase | ErrorCode::DatabaseCorrupt)
    ) {
        AliceError::invalid(format!(
            "{} cannot be read as a SQLite database ({e}); the palace may be damaged. Nothing was copied.",
            src.display()
        ))
    } else {
        AliceError::invalid(format!(
            "copying {} failed ({e}); nothing was copied. Retry, and check free disk space.",
            src.display()
        ))
    }
}

/// Copies `src` to the NEW file `dst` with the backup API. Refuses to touch an existing `dst`;
/// removes its own partial output on any error. `between` runs after every non-final step (tests
/// use it to play the part of a writer).
fn copy_db(
    src: &Path,
    dst: &Path,
    limits: &Limits,
    between: &mut dyn FnMut(),
) -> Result<CopyStats> {
    let wal = is_wal(src);
    let source = Connection::open_with_flags(
        src,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|e| copy_error(src, e))?;
    // Reserve the destination atomically; an existing file is never replaced.
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(dst)
        .map_err(|e| exists_error(dst, e))?;
    let r = run_copy(&source, src, dst, wal, limits, between);
    if r.is_err() {
        remove_db_files(dst);
    }
    r
}

fn run_copy(
    source: &Connection,
    src: &Path,
    dst: &Path,
    wal: bool,
    limits: &Limits,
    between: &mut dyn FnMut(),
) -> Result<CopyStats> {
    let mut dest = Connection::open(dst)?;
    let mut restarts = 0u32;
    {
        let backup = Backup::new(source, &mut dest).map_err(|e| copy_error(src, e))?;
        // (pagecount, remaining) after the previous step.
        let mut prev: Option<(i32, i32)> = None;
        let mut busy = 0u32;
        loop {
            if Instant::now() > limits.deadline {
                return Err(AliceError::invalid(format!(
                    "the snapshot did not finish in 10 minutes; MemPalace may be busy writing {}. Stop it (or retry later) and run the import again.",
                    src.display()
                )));
            }
            let pages = if restarts >= limits.single_step_after {
                -1
            } else {
                limits.step_pages
            };
            match backup.step(pages).map_err(|e| copy_error(src, e))? {
                StepResult::Done => break,
                StepResult::More => {
                    busy = 0;
                    let p = backup.progress();
                    // A step that did not advance by the pages just copied means SQLite restarted
                    // the copy because a writer changed the source (even an in-place update that
                    // does not grow the file); a grown file is a restart too.
                    if let Some((pc, rem)) = prev
                        && (p.pagecount > pc || p.remaining > rem - pages)
                    {
                        restarts += 1;
                        if !wal && restarts > limits.max_restarts_non_wal {
                            return Err(writing_error(src));
                        }
                    }
                    prev = Some((p.pagecount, p.remaining));
                    between();
                }
                // Busy: a writer holds the source; Locked: contention inside the copy.
                _ => {
                    busy += 1;
                    if !wal && busy > limits.busy_retries {
                        return Err(writing_error(src));
                    }
                    std::thread::sleep(limits.busy_sleep);
                }
            }
        }
    }
    // The copy inherits the WAL flag from the header; it is ours alone, so make it a plain file.
    let _: String = dest.query_row("PRAGMA journal_mode=DELETE", [], |r| r.get(0))?;
    drop(dest);
    Ok(CopyStats { wal, restarts })
}

fn writing_error(src: &Path) -> AliceError {
    AliceError::invalid(format!(
        "MemPalace is writing to {}; stop it (or retry in a minute) and run the import again.",
        src.display()
    ))
}

/// Opens the copy made by `snapshot()`. `collection` defaults to [`DEFAULT_COLLECTION`].
pub fn open_snapshot(s: &Snapshot, collection: Option<&str>) -> Result<Box<dyn PalaceReader>> {
    match s.backend {
        Backend::SqliteExact => {
            let path = s.dir.join(MAIN_FILE);
            Ok(Box::new(SqliteExactReader::open(
                &path,
                collection.unwrap_or(DEFAULT_COLLECTION),
            )?))
        }
        Backend::Chroma => Ok(Box::new(super::chroma::ChromaReader::open(
            &s.dir.join(CHROMA_FILE),
            collection.unwrap_or(DEFAULT_COLLECTION),
        )?)),
    }
}

/// Documents in the palace's `mempalace_closets` collection (MemPalace's index of drawers; it is
/// not imported). 0 when the collection is missing or unreadable: the number is only reported.
pub fn count_closets(s: &Snapshot) -> u64 {
    const NAME: &str = "mempalace_closets";
    let (file, sql) = match s.backend {
        Backend::SqliteExact => (
            MAIN_FILE,
            "SELECT count(*) FROM documents d JOIN collections c ON c.id = d.collection_id WHERE c.name = ?1",
        ),
        Backend::Chroma => (
            CHROMA_FILE,
            "SELECT count(*) FROM embeddings e
             JOIN segments sg ON sg.id = e.segment_id AND sg.scope = 'METADATA'
             JOIN collections c ON c.id = sg.collection
             WHERE c.name = ?1",
        ),
    };
    let Ok(conn) = Connection::open_with_flags(
        s.dir.join(file),
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    ) else {
        return 0;
    };
    conn.query_row(sql, [NAME], |r| r.get::<_, i64>(0))
        .map(|n| n.max(0) as u64)
        .unwrap_or(0)
}

struct SqliteExactReader {
    info: PalaceInfo,
    conn: Connection,
    collection_id: i64,
}

impl SqliteExactReader {
    fn open(path: &Path, collection: &str) -> Result<Self> {
        let conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        let collection_id: i64 = conn
            .query_row(
                "SELECT id FROM collections WHERE name = ?1",
                [collection],
                |r| r.get(0),
            )
            .map_err(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => AliceError::invalid(format!(
                    "the palace has no collection named {collection}; pass the right name or check that this is a MemPalace palace"
                )),
                other => other.into(),
            })?;
        let drawers: i64 = conn.query_row(
            "SELECT count(*) FROM documents WHERE collection_id = ?1",
            [collection_id],
            |r| r.get(0),
        )?;
        Ok(Self {
            info: PalaceInfo {
                backend: Backend::SqliteExact,
                path: path.to_path_buf(),
                drawers: drawers.max(0) as u64,
            },
            conn,
            collection_id,
        })
    }
}

fn lossy(r: &rusqlite::Row<'_>, i: usize) -> rusqlite::Result<String> {
    use rusqlite::types::ValueRef;
    Ok(match r.get_ref(i)? {
        ValueRef::Text(b) | ValueRef::Blob(b) => String::from_utf8_lossy(b).into_owned(),
        ValueRef::Integer(n) => n.to_string(),
        ValueRef::Real(f) => f.to_string(),
        ValueRef::Null => String::new(),
    })
}

fn drawer_from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Drawer> {
    Ok(Drawer {
        id: lossy(r, 0)?,
        text: lossy(r, 1)?,
        meta: parse_meta(&lossy(r, 2)?),
    })
}

/// One metadata value as a backend stores it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum MetaValue {
    Str(String),
    Int(i64),
    Float(f64),
    Bool(bool),
}

impl MetaValue {
    fn scalar(&self) -> String {
        match self {
            MetaValue::Str(s) => s.clone(),
            MetaValue::Int(n) => n.to_string(),
            MetaValue::Float(f) => {
                serde_json::Number::from_f64(*f).map_or_else(|| f.to_string(), |n| n.to_string())
            }
            MetaValue::Bool(b) => b.to_string(),
        }
    }

    fn int(&self) -> Option<i64> {
        let from_f = |f: f64| f.is_finite().then_some(f as i64);
        match self {
            MetaValue::Int(n) => Some(*n),
            MetaValue::Float(f) => from_f(*f),
            MetaValue::Str(s) => {
                let t = s.trim();
                t.parse::<i64>()
                    .ok()
                    .or_else(|| t.parse::<f64>().ok().and_then(from_f))
            }
            MetaValue::Bool(_) => None,
        }
    }
}

fn from_json(v: &serde_json::Value) -> Option<MetaValue> {
    match v {
        serde_json::Value::String(s) => Some(MetaValue::Str(s.clone())),
        serde_json::Value::Bool(b) => Some(MetaValue::Bool(*b)),
        serde_json::Value::Number(n) => Some(match n.as_i64() {
            Some(i) => MetaValue::Int(i),
            // Integers beyond i64 keep their digits; `int()` parses them like a string.
            None if n.is_u64() => MetaValue::Str(n.to_string()),
            None => MetaValue::Float(n.as_f64()?),
        }),
        _ => None,
    }
}

/// The one place metadata is typed, shared by every reader so they cannot drift: missing keys give
/// `None`/empty, wrong scalar types are stringified, unknown keys are ignored.
pub(crate) fn meta_from_pairs(pairs: impl Iterator<Item = (String, MetaValue)>) -> DrawerMeta {
    let mut m = DrawerMeta::default();
    for (k, v) in pairs {
        let s = v.scalar();
        match k.as_str() {
            "wing" => m.wing = s,
            "room" => m.room = s,
            "hall" => m.hall = Some(s),
            "source_file" => m.source_file = Some(s),
            "added_by" => m.added_by = Some(s),
            "ingest_mode" => m.ingest_mode = Some(s),
            "filed_at" => m.filed_at = Some(s),
            "authored_at" => m.authored_at = Some(s),
            "chunk_index" => m.chunk_index = v.int(),
            "agent" => m.agent = Some(s),
            "date" => m.date = Some(s),
            "topic" => m.topic = Some(s),
            "entry_type" => m.entry_type = Some(s),
            "parent_drawer_id" => m.parent_drawer_id = Some(s),
            _ => {}
        }
    }
    m
}

/// Bad JSON sets `unreadable`; otherwise see [`meta_from_pairs`].
fn parse_meta(json: &str) -> DrawerMeta {
    let Ok(serde_json::Value::Object(o)) = serde_json::from_str::<serde_json::Value>(json) else {
        return DrawerMeta {
            unreadable: true,
            ..Default::default()
        };
    };
    meta_from_pairs(
        o.iter()
            .filter_map(|(k, v)| from_json(v).map(|m| (k.clone(), m))),
    )
}

impl PalaceReader for SqliteExactReader {
    fn info(&self) -> &PalaceInfo {
        &self.info
    }

    fn page(&self, after: Option<&str>, limit: usize, since: Option<&str>) -> Result<Vec<Drawer>> {
        // `since` compares ISO text, so a bare date works against naive and Z strings; a zoned
        // `--since` is compared as text too (decision 20). A drawer with no `filed_at`, or whose
        // metadata cannot be parsed, cannot be shown older and is kept.
        // json_valid guards json_extract, which raises on malformed JSON.
        let mut stmt = self.conn.prepare_cached(
            "SELECT id, document, metadata_json FROM documents
             WHERE collection_id = ?1 AND id > ?2
               AND (?3 IS NULL
                    OR CASE WHEN json_valid(metadata_json)
                            THEN CAST(json_extract(metadata_json, '$.filed_at') AS TEXT) END IS NULL
                    OR CAST(json_extract(metadata_json, '$.filed_at') AS TEXT) >= ?3)
             ORDER BY id LIMIT ?4",
        )?;
        let rows = stmt.query_map(
            rusqlite::params![self.collection_id, after.unwrap_or(""), since, limit as i64],
            drawer_from_row,
        )?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    fn get(&self, ids: &[String]) -> Result<Vec<Drawer>> {
        let mut out = Vec::new();
        for chunk in ids.chunks(500) {
            let marks = vec!["?"; chunk.len()].join(",");
            let sql = format!(
                "SELECT id, document, metadata_json FROM documents WHERE collection_id = ? AND id IN ({marks}) ORDER BY id"
            );
            let mut stmt = self.conn.prepare(&sql)?;
            let params = std::iter::once(rusqlite::types::Value::Integer(self.collection_id))
                .chain(
                    chunk
                        .iter()
                        .map(|s| rusqlite::types::Value::Text(s.clone())),
                );
            let rows = stmt.query_map(params_from_iter(params), drawer_from_row)?;
            for r in rows {
                out.push(r?);
            }
        }
        Ok(out)
    }

    fn wing_counts(&self) -> Result<Vec<(String, u64)>> {
        let mut stmt = self.conn.prepare(
            "SELECT COALESCE(CASE WHEN json_valid(metadata_json)
                                  THEN CAST(json_extract(metadata_json, '$.wing') AS TEXT) END, ''),
                    count(*)
             FROM documents WHERE collection_id = ?1 GROUP BY 1 ORDER BY 1",
        )?;
        let rows = stmt.query_map([self.collection_id], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?.max(0) as u64))
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    use sha2::{Digest, Sha256};

    use super::*;
    use crate::import::fixture::{DrawerSpec, build_sqlite_exact, build_sqlite_exact_wal};

    #[test]
    fn a_snapshot_destination_in_or_around_the_palace_or_home_is_refused() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("home");
        let palace = home.join("palace");
        std::fs::create_dir_all(&palace).unwrap();
        build_sqlite_exact(&palace, &[spec("a", "text", "w", None)]);
        let link = root.path().join("link");
        std::os::unix::fs::symlink(&palace, &link).unwrap();
        for dest in [
            palace.clone(),
            palace.join("new/deeper"),
            link.join("via"),
            home.clone(),
            home.join("snap"),
            root.path().join("home/palace/x/../snap"),
            root.path().to_path_buf(),
        ] {
            let e = snapshot(&palace, &dest, Some(&home))
                .unwrap_err()
                .to_string();
            assert!(e.contains("Nothing was copied"), "{}: {e}", dest.display());
        }
        assert!(!palace.join("new").exists() && !home.join("snap").exists());
        let ok = root.path().join("elsewhere/snap");
        assert!(snapshot(&palace, &ok, Some(&home)).is_ok());
    }

    fn spec(id: &str, text: &str, wing: &str, filed_at: Option<&str>) -> DrawerSpec {
        DrawerSpec {
            id: id.into(),
            text: text.into(),
            meta: DrawerMeta {
                wing: wing.into(),
                room: "room".into(),
                filed_at: filed_at.map(Into::into),
                ..Default::default()
            },
        }
    }

    fn sample(n: usize) -> Vec<DrawerSpec> {
        (0..n)
            .map(|i| {
                // Shuffled-looking ids so id order differs from insertion order.
                let id = format!("drawer_{:04}", (i * 7919) % 10007);
                spec(
                    &id,
                    &format!("drawer text number {i} {}", "x".repeat(400)),
                    "pg-ai",
                    Some("2026-02-01T00:00:00"),
                )
            })
            .collect()
    }

    fn sha256_file(p: &Path) -> String {
        hex::encode(Sha256::digest(std::fs::read(p).unwrap()))
    }

    fn list_names(dir: &Path) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        v.sort();
        v
    }

    fn hash_dir(dir: &Path) -> Vec<(String, String)> {
        list_names(dir)
            .into_iter()
            .map(|n| {
                let h = sha256_file(&dir.join(&n));
                (n, h)
            })
            .collect()
    }

    fn all_ids(r: &dyn PalaceReader) -> Vec<String> {
        let mut seen = Vec::new();
        let mut after: Option<String> = None;
        loop {
            let p = r.page(after.as_deref(), 100, None).unwrap();
            if p.is_empty() {
                break;
            }
            after = Some(p.last().unwrap().id.clone());
            seen.extend(p.into_iter().map(|d| d.id));
        }
        seen
    }

    #[test]
    fn closets_are_counted_from_the_snapshot_and_a_missing_collection_counts_zero() {
        let src = tempfile::tempdir().unwrap();
        build_sqlite_exact(src.path(), &sample(3));
        let dst = tempfile::tempdir().unwrap();
        let snap = snapshot(src.path(), dst.path(), None).unwrap();
        assert_eq!(count_closets(&snap), 1);
        let chroma_src = tempfile::tempdir().unwrap();
        crate::import::fixture::build_chroma(chroma_src.path(), &sample(3));
        let chroma_dst = tempfile::tempdir().unwrap();
        let snap = snapshot(chroma_src.path(), chroma_dst.path(), None).unwrap();
        assert_eq!(count_closets(&snap), 0, "the chroma fixture has no closets");
    }

    #[test]
    fn snapshot_leaves_a_delete_mode_palace_byte_identical_and_reads_every_drawer() {
        let src = tempfile::tempdir().unwrap();
        build_sqlite_exact(src.path(), &sample(250));
        let before = hash_dir(src.path());
        let dst = tempfile::tempdir().unwrap();
        let snap = snapshot(src.path(), dst.path(), None).unwrap();
        assert_eq!(hash_dir(src.path()), before, "the palace must not change");
        assert_eq!(snap.backend, Backend::SqliteExact);
        let r = open_snapshot(&snap, None).unwrap();
        assert_eq!(r.info().drawers, 250); // closets are not counted
        let seen = all_ids(r.as_ref());
        assert_eq!(seen.len(), 250);
        let mut sorted = seen.clone();
        sorted.sort();
        assert_eq!(seen, sorted, "pages are in id order");
        assert_eq!(r.wing_counts().unwrap(), vec![("pg-ai".to_string(), 250)]);
        let got = r.get(&[seen[3].clone(), "missing".into()]).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].id, seen[3]);
    }

    #[test]
    fn a_live_wal_palace_with_an_open_writer_snapshots_without_changing_its_main_file() {
        let src = tempfile::tempdir().unwrap();
        let wal = build_sqlite_exact_wal(src.path(), &sample(120));
        let main = src.path().join("sqlite_exact.sqlite3");
        let before = sha256_file(&main);
        let dst = tempfile::tempdir().unwrap();
        let snap = snapshot(src.path(), dst.path(), None).unwrap();
        assert_eq!(sha256_file(&main), before, "main database file untouched");
        let new: Vec<_> = list_names(src.path())
            .into_iter()
            .filter(|n| n != "sqlite_exact.sqlite3")
            .collect();
        assert!(
            new.iter()
                .all(|n| n.ends_with("-shm") || n.ends_with("-wal")),
            "only SQLite's own WAL files may exist: {new:?}"
        );
        assert_eq!(
            open_snapshot(&snap, None).unwrap().info().drawers,
            120,
            "committed rows only; the writer's open transaction is not in the copy"
        );
        assert!(snap.notes.iter().any(|n| n.contains("WAL")));
        drop(wal);
    }

    #[test]
    fn a_writer_that_keeps_committing_cannot_stall_the_snapshot() {
        // Smoke test with a bounded duration: no hang, a consistent copy. Restart behaviour is
        // proven deterministically by the copy_db tests below.
        let src = tempfile::tempdir().unwrap();
        let mut wal = build_sqlite_exact_wal(src.path(), &sample(200));
        wal.release_writer();
        let stop = Arc::new(AtomicBool::new(false));
        let path = wal.path.clone();
        let flag = stop.clone();
        let t = std::thread::spawn(move || {
            let c = Connection::open(&path).unwrap();
            c.busy_timeout(Duration::from_millis(500)).unwrap();
            // Hard bound: this thread ends by itself after 30 s whatever the test does.
            let end = Instant::now() + Duration::from_secs(30);
            let mut n = 0i64;
            while !flag.load(Ordering::Relaxed) && Instant::now() < end {
                let _ = c.execute("INSERT INTO alice_fixture_scratch VALUES (?1)", [n]);
                n += 1;
                std::thread::sleep(Duration::from_millis(2));
            }
            n
        });
        let dst = tempfile::tempdir().unwrap();
        let started = Instant::now();
        let snap = snapshot(src.path(), dst.path(), None);
        stop.store(true, Ordering::Relaxed);
        let commits = t.join().unwrap();
        let snap = snap.unwrap();
        assert!(started.elapsed() < Duration::from_secs(60));
        assert!(commits > 0, "the background writer never ran");
        let c = Connection::open(snap.dir.join("sqlite_exact.sqlite3")).unwrap();
        let q: String = c.query_row("PRAGMA quick_check", [], |r| r.get(0)).unwrap();
        assert_eq!(q, "ok");
        assert_eq!(open_snapshot(&snap, None).unwrap().info().drawers, 200);
    }

    fn big_source(
        wal: bool,
    ) -> (
        tempfile::TempDir,
        PathBuf,
        Option<crate::import::fixture::WalPalace>,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let specs: Vec<DrawerSpec> = (0..300)
            .map(|i| spec(&format!("d{i:04}"), &"y".repeat(2000), "w", None))
            .collect();
        if wal {
            let mut w = build_sqlite_exact_wal(dir.path(), &specs);
            w.release_writer();
            let p = w.path.clone();
            (dir, p, Some(w))
        } else {
            let p = build_sqlite_exact(dir.path(), &specs);
            (dir, p, None)
        }
    }

    fn small_steps() -> Limits {
        Limits {
            step_pages: 10,
            // A broken detector must fail the test, not hang it.
            deadline: Instant::now() + Duration::from_secs(20),
            ..Limits::production()
        }
    }

    fn page_count(p: &Path) -> i64 {
        Connection::open(p)
            .unwrap()
            .query_row("PRAGMA page_count", [], |r| r.get(0))
            .unwrap()
    }

    #[test]
    fn in_place_updates_between_steps_are_counted_as_restarts_then_the_copy_finishes() {
        for wal in [false, true] {
            let (dir, path, _keep) = big_source(wal);
            assert!(page_count(&path) > 30, "fixture must span many steps");
            let writer = Connection::open(&path).unwrap();
            let mut n = 0i64;
            let dst_dir = tempfile::tempdir().unwrap();
            let dst = dst_dir.path().join("copy.sqlite3");
            let st = copy_db(&path, &dst, &small_steps(), &mut || {
                n += 1;
                writer
                    .execute(
                        "UPDATE documents SET updated_at = ?1 WHERE id = 'd0000' AND collection_id = 1",
                        [format!("v{n}")],
                    )
                    .unwrap();
            })
            .unwrap();
            assert!(st.restarts >= 3, "wal={wal} restarts={}", st.restarts);
            assert_eq!(st.wal, wal);
            let got: String = Connection::open(&dst)
                .unwrap()
                .query_row(
                    "SELECT updated_at FROM documents WHERE id = 'd0000' AND collection_id = 1",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(got, format!("v{n}"), "the copy holds the latest value");
            let mut notes = Vec::new();
            note_stats(&mut notes, "x", &st);
            assert!(
                notes
                    .iter()
                    .any(|m| m.contains(&format!("restarted {} times", st.restarts)))
            );
            drop(dir);
        }
    }

    #[test]
    fn a_delete_mode_source_that_keeps_changing_fails_with_a_clear_sentence_and_leaves_nothing() {
        let (_dir, path, _) = big_source(false);
        let writer = Connection::open(&path).unwrap();
        let dst_dir = tempfile::tempdir().unwrap();
        let dst = dst_dir.path().join("copy.sqlite3");
        let mut n = 0;
        let lim = Limits {
            single_step_after: u32::MAX, // the one-shot step is not available
            ..small_steps()
        };
        let e = copy_db(&path, &dst, &lim, &mut || {
            n += 1;
            writer
                .execute(
                    "UPDATE documents SET updated_at = ?1 WHERE id = 'd0000'",
                    [n],
                )
                .unwrap();
        })
        .unwrap_err()
        .to_string();
        assert!(e.contains("MemPalace is writing"), "{e}");
        assert!(std::fs::read_dir(dst_dir.path()).unwrap().next().is_none());
    }

    #[test]
    fn a_source_locked_by_a_writer_gives_up_with_the_writing_sentence() {
        let (_dir, path, _) = big_source(false);
        let writer = Connection::open(&path).unwrap();
        writer.execute_batch("BEGIN EXCLUSIVE").unwrap();
        let dst_dir = tempfile::tempdir().unwrap();
        let dst = dst_dir.path().join("copy.sqlite3");
        let lim = Limits {
            busy_retries: 3,
            busy_sleep: Duration::from_millis(1),
            ..Limits::production()
        };
        let e = copy_db(&path, &dst, &lim, &mut || {})
            .unwrap_err()
            .to_string();
        assert!(e.contains("MemPalace is writing"), "{e}");
        assert!(std::fs::read_dir(dst_dir.path()).unwrap().next().is_none());
        writer.execute_batch("ROLLBACK").unwrap();
    }

    #[test]
    fn snapshot_never_overwrites_or_deletes_an_existing_file() {
        let src = tempfile::tempdir().unwrap();
        build_sqlite_exact(src.path(), &sample(20));
        let main = src.path().join("sqlite_exact.sqlite3");
        let before = hash_dir(src.path());
        // Destination is the palace folder itself.
        let e = snapshot(src.path(), src.path(), None)
            .unwrap_err()
            .to_string();
        assert!(e.contains("palace folder"), "{e}");
        assert_eq!(hash_dir(src.path()), before);
        // ... through a symlink.
        #[cfg(unix)]
        {
            let links = tempfile::tempdir().unwrap();
            let l = links.path().join("l");
            std::os::unix::fs::symlink(src.path(), &l).unwrap();
            assert!(snapshot(src.path(), &l, None).is_err());
            assert!(snapshot(&l, src.path(), None).is_err());
            assert_eq!(hash_dir(src.path()), before);
        }
        // ... or mempalace_home.
        let home = tempfile::tempdir().unwrap();
        let dst = home.path().to_path_buf();
        let e = snapshot(src.path(), &dst, Some(home.path()))
            .unwrap_err()
            .to_string();
        assert!(e.contains("MemPalace home"), "{e}");
        // A destination that already holds the file: refused, file untouched, nothing else added.
        let dst = tempfile::tempdir().unwrap();
        std::fs::write(dst.path().join("sqlite_exact.sqlite3"), b"precious").unwrap();
        let e = snapshot(src.path(), dst.path(), None)
            .unwrap_err()
            .to_string();
        assert!(e.contains("already exists"), "{e}");
        assert_eq!(
            std::fs::read(dst.path().join("sqlite_exact.sqlite3")).unwrap(),
            b"precious"
        );
        assert_eq!(list_names(dst.path()), ["sqlite_exact.sqlite3"]);
        assert_eq!(sha256_file(&main), before[0].1);
    }

    #[test]
    fn since_keeps_only_drawers_filed_at_or_after_the_date() {
        let src = tempfile::tempdir().unwrap();
        build_sqlite_exact(
            src.path(),
            &[
                spec("a", "old", "w", Some("2026-01-01T10:00:00Z")),
                spec("b", "newer", "w", Some("2026-06-01T10:00:00")),
                spec("c", "undated", "w", None),
            ],
        );
        let dst = tempfile::tempdir().unwrap();
        let snap = snapshot(src.path(), dst.path(), None).unwrap();
        let r = open_snapshot(&snap, None).unwrap();
        let ids: Vec<_> = r
            .page(None, 10, Some("2026-03-01"))
            .unwrap()
            .into_iter()
            .map(|d| d.id)
            .collect();
        assert_eq!(ids, ["b", "c"]);
        assert_eq!(r.page(None, 10, None).unwrap().len(), 3);
        // The limit applies after the filter.
        let one = r.page(None, 1, Some("2026-03-01")).unwrap();
        assert_eq!(one[0].id, "b");
        let next = r.page(Some("b"), 5, Some("2026-03-01")).unwrap();
        assert_eq!(next[0].id, "c");
    }

    #[test]
    fn metadata_is_parsed_with_missing_keys_and_wrong_types() {
        let src = tempfile::tempdir().unwrap();
        let path = build_sqlite_exact(
            src.path(),
            &[
                spec("d1", "one", "w", None),
                spec("d2", "two", "w", None),
                spec("d3", "three", "w", None),
                spec("d4", "four", "w", None),
                spec("d5", "five", "w", None),
                spec("d6", "bad json", "w", None),
                spec("d7", "nul\0inside", "w", None),
            ],
        );
        {
            let c = Connection::open(&path).unwrap();
            let set = |id: &str, json: &str| {
                c.execute(
                    "UPDATE documents SET metadata_json = ?2 WHERE id = ?1",
                    [id, json],
                )
                .unwrap();
            };
            set("d1", r#"{"wing":"w","room":"r","chunk_index":"7"}"#);
            set("d2", r#"{"wing":"w","room":"r","chunk_index":7.0}"#);
            set("d3", r#"{"wing":"w","room":"r","chunk_index":7}"#);
            set("d4", r#"{"wing":"w","room":"r","chunk_index":-3}"#);
            set("d5", r#"{"room":"r","hall":null,"agent":42}"#);
            set("d6", "not json");
        }
        let dst = tempfile::tempdir().unwrap();
        let snap = snapshot(src.path(), dst.path(), None).unwrap();
        let r = open_snapshot(&snap, None).unwrap();
        let by = |id: &str| r.get(&[id.to_string()]).unwrap().remove(0);
        assert_eq!(by("d1").meta.chunk_index, Some(7));
        assert_eq!(by("d2").meta.chunk_index, Some(7));
        assert_eq!(by("d3").meta.chunk_index, Some(7));
        assert_eq!(by("d4").meta.chunk_index, Some(-3));
        let d5 = by("d5");
        assert_eq!(d5.meta.wing, "");
        assert_eq!(d5.meta.hall, None);
        assert_eq!(d5.meta.agent.as_deref(), Some("42"));
        assert!(!d5.meta.unreadable);
        let d6 = by("d6");
        assert!(d6.meta.unreadable);
        assert_eq!(d6.id, "d6");
        assert_eq!(by("d7").text, "nul\0inside");
        // A malformed row must not break paging with `since` or the wing counts.
        assert_eq!(r.page(None, 10, Some("2026-01-01")).unwrap().len(), 7);
        assert!(r.wing_counts().unwrap().iter().any(|(w, _)| w.is_empty()));
    }

    #[test]
    fn bad_paths_say_what_to_do() {
        let d = tempfile::tempdir().unwrap();
        let e = snapshot(&d.path().join("missing"), d.path(), None)
            .unwrap_err()
            .to_string();
        assert!(
            e.contains("does not exist") && e.contains("--palace"),
            "{e}"
        );
        std::fs::write(d.path().join("sqlite_exact.sqlite3"), b"not sqlite").unwrap();
        let dst = tempfile::tempdir().unwrap();
        let e = snapshot(d.path(), dst.path(), None)
            .unwrap_err()
            .to_string();
        assert!(
            e.contains("sqlite_exact.sqlite3") && e.contains("damaged"),
            "{e}"
        );
        assert!(
            std::fs::read_dir(dst.path()).unwrap().next().is_none(),
            "nothing left behind on failure"
        );
        let empty = tempfile::tempdir().unwrap();
        assert!(
            snapshot(empty.path(), dst.path(), None)
                .unwrap_err()
                .to_string()
                .contains("not a MemPalace palace")
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_palace_directory_is_followed_and_still_not_modified() {
        let real = tempfile::tempdir().unwrap();
        build_sqlite_exact(real.path(), &sample(30));
        let links = tempfile::tempdir().unwrap();
        let link = links.path().join("palace-link");
        std::os::unix::fs::symlink(real.path(), &link).unwrap();
        let before = hash_dir(real.path());
        let dst = tempfile::tempdir().unwrap();
        let snap = snapshot(&link, dst.path(), None).unwrap();
        assert_eq!(hash_dir(real.path()), before);
        assert_eq!(open_snapshot(&snap, None).unwrap().info().drawers, 30);
    }

    #[test]
    fn kg_and_tunnels_are_copied_from_mempalaces_own_locations() {
        // Palace folder with its own KG, tunnels.json in the PARENT folder (MemPalace's location).
        let root = tempfile::tempdir().unwrap();
        let palace = root.path().join("palace");
        std::fs::create_dir(&palace).unwrap();
        build_sqlite_exact(&palace, &sample(5));
        {
            let c = Connection::open(palace.join("knowledge_graph.sqlite3")).unwrap();
            c.execute_batch("CREATE TABLE triples (s TEXT); INSERT INTO triples VALUES ('x');")
                .unwrap();
        }
        std::fs::write(root.path().join("tunnels.json"), br#"{"tunnels":[]}"#).unwrap();
        let dst = tempfile::tempdir().unwrap();
        let snap = snapshot(&palace, dst.path(), None).unwrap();
        let kg = snap.kg.expect("kg copied");
        let tunnels = snap.tunnels.expect("tunnels copied");
        assert!(kg.starts_with(dst.path()) && tunnels.starts_with(dst.path()));
        let n: i64 = Connection::open(&kg)
            .unwrap()
            .query_row("SELECT count(*) FROM triples", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1);
        assert_eq!(std::fs::read(&tunnels).unwrap(), br#"{"tunnels":[]}"#);

        // Only mempalace_home has a KG: it is the fallback.
        let root2 = tempfile::tempdir().unwrap();
        let palace2 = root2.path().join("palace");
        std::fs::create_dir(&palace2).unwrap();
        build_sqlite_exact(&palace2, &sample(5));
        let home = tempfile::tempdir().unwrap();
        Connection::open(home.path().join("knowledge_graph.sqlite3"))
            .unwrap()
            .execute_batch("CREATE TABLE triples (s TEXT);")
            .unwrap();
        let dst2 = tempfile::tempdir().unwrap();
        let snap2 = snapshot(&palace2, dst2.path(), Some(home.path())).unwrap();
        assert!(snap2.kg.is_some());
        assert!(snap2.tunnels.is_none());

        // Neither.
        let dst3 = tempfile::tempdir().unwrap();
        let snap3 = snapshot(&palace2, dst3.path(), None).unwrap();
        assert!(snap3.kg.is_none() && snap3.tunnels.is_none());
    }
}
