//! A person's uploaded MemPalace (`POST /api/v1/import/upload`, spec addendum 6): the safe
//! unpacking of the gzip tar, the search for the palace inside it, and the sweep of what a crash
//! left behind. Nothing here trusts the archive: every size is counted from the bytes actually
//! read, and only regular files and folders are ever created.

use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use alice_core::error::{AliceError, Result};
use alice_core::import::palace::detect_backend;

/// Entries (files and folders) an upload may hold. A palace is a handful of database files.
pub const MAX_ENTRIES: u64 = 50_000;
/// The unpacked size may be this many times the archive's size. A palace's sqlite files pack 3 to
/// 6 times (more when they are mostly text), so 12 leaves room; what truly bounds the unpacked
/// total is `import.max_upload_bytes`, which it never exceeds.
pub const UNPACK_RATIO: u64 = 12;
/// Name every upload's archive and unpacked folder begin with, inside `import-inbox/`.
pub const PREFIX: &str = "upload-";
const KG_FILE: &str = "knowledge_graph.sqlite3";

/// Removes what an earlier run of the daemon left of uploads: archives and unpacked folders.
/// Nothing else in the inbox (the admin's folders) is touched. Returns how many were removed.
pub fn sweep(inbox: &Path) -> usize {
    let Ok(rd) = std::fs::read_dir(inbox) else {
        return 0;
    };
    let mut removed = 0;
    for e in rd.flatten() {
        if !e.file_name().to_string_lossy().starts_with(PREFIX) {
            continue;
        }
        let p = e.path();
        let is_dir = std::fs::symlink_metadata(&p).is_ok_and(|m| m.is_dir());
        let ok = if is_dir {
            std::fs::remove_dir_all(&p).is_ok()
        } else {
            std::fs::remove_file(&p).is_ok()
        };
        removed += usize::from(ok);
    }
    removed
}

fn invalid(msg: impl Into<String>) -> AliceError {
    AliceError::invalid(msg)
}

/// A reader that fails once `left` bytes have been read, so a decompression bomb ends at a
/// bound that follows from the budget, whatever the archive's headers claim.
struct Limited<R> {
    inner: R,
    left: u64,
    hit: std::sync::Arc<AtomicBool>,
}

impl<R: Read> Read for Limited<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.left == 0 {
            self.hit.store(true, Ordering::SeqCst);
            return Err(std::io::Error::other("limit"));
        }
        let max = buf
            .len()
            .min(usize::try_from(self.left).unwrap_or(usize::MAX));
        let n = self.inner.read(&mut buf[..max])?;
        self.left -= n as u64;
        Ok(n)
    }
}

/// Creates `path` and the folders above it, private to the daemon's user.
fn make_dirs(path: &Path) -> std::io::Result<()> {
    let mut b = std::fs::DirBuilder::new();
    b.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        b.mode(0o700);
    }
    b.create(path)
}

/// The entry's path below the unpack folder, or why it must be refused. `.` parts are dropped.
fn relative(path: &Path) -> Result<PathBuf> {
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::Normal(p) => out.push(p),
            Component::CurDir => {}
            Component::ParentDir => {
                return Err(invalid(format!(
                    "the archive holds a path with `..` ({}); only paths inside the archive are accepted",
                    path.display()
                )));
            }
            Component::RootDir | Component::Prefix(_) => {
                return Err(invalid(format!(
                    "the archive holds an absolute path ({}); only relative paths are accepted",
                    path.display()
                )));
            }
        }
    }
    Ok(out)
}

fn kind_name(t: tar::EntryType) -> &'static str {
    match t {
        tar::EntryType::Symlink => "symbolic link",
        tar::EntryType::Link => "hard link",
        tar::EntryType::Char | tar::EntryType::Block => "device",
        tar::EntryType::Fifo => "named pipe",
        _ => "special",
    }
}

/// Unpacks the gzip tar at `archive` into the new folder `dest`.
///
/// Regular files and folders only; any link, device, absolute path or `..` refuses the whole
/// archive (the caller removes `dest`). The unpacked bytes, counted as they are written, stay
/// within `min(UNPACK_RATIO x archive_len, cap)`, and the entries within [`MAX_ENTRIES`].
pub fn unpack(
    archive: &Path,
    dest: &Path,
    archive_len: u64,
    cap: u64,
    stop: &AtomicBool,
) -> Result<u64> {
    let budget = archive_len.saturating_mul(UNPACK_RATIO).min(cap);
    let too_large = || {
        invalid(format!(
            "the archive is too large once unpacked (over {budget} bytes: at most {UNPACK_RATIO} times the upload, and never over the {cap}-byte limit)"
        ))
    };
    let not_gzip = |why: String| {
        invalid(format!(
            "the upload is not a readable gzip tar archive ({why}); pack the palace with `tar -czf`"
        ))
    };
    let mut file = std::io::BufReader::new(std::fs::File::open(archive)?);
    {
        use std::io::BufRead;
        let head = file.fill_buf()?;
        if head.len() < 2 || head[0] != 0x1f || head[1] != 0x8b {
            return Err(not_gzip("it does not start like a gzip file".into()));
        }
    }
    make_dirs(dest)?;
    // Headers and padding ride on top of the file bytes: allow them, but no more.
    let hit = std::sync::Arc::new(AtomicBool::new(false));
    let reader = Limited {
        inner: flate2::read::GzDecoder::new(file),
        left: budget.saturating_add(MAX_ENTRIES * 1536 + 65_536),
        hit: hit.clone(),
    };
    let read_err = |e: std::io::Error| {
        if hit.load(Ordering::SeqCst) {
            too_large()
        } else {
            not_gzip(e.to_string())
        }
    };
    let mut tar = tar::Archive::new(reader);
    let (mut entries, mut total) = (0u64, 0u64);
    let mut buf = vec![0u8; 64 * 1024];
    for entry in tar.entries().map_err(read_err)? {
        if stop.load(Ordering::SeqCst) {
            return Err(AliceError::Other(crate::importer::STOPPED.to_string()));
        }
        let mut entry = entry.map_err(read_err)?;
        entries += 1;
        if entries > MAX_ENTRIES {
            return Err(invalid(format!(
                "the archive holds more than {MAX_ENTRIES} entries; a palace is a few database files"
            )));
        }
        let kind = entry.header().entry_type();
        if kind == tar::EntryType::XGlobalHeader {
            continue;
        }
        let path = entry
            .path()
            .map_err(|e| invalid(format!("the archive holds an unreadable path ({e})")))?
            .into_owned();
        if !(kind.is_file() || kind.is_dir()) {
            return Err(invalid(format!(
                "the archive holds a {} ({}); only files and folders are accepted",
                kind_name(kind),
                path.display()
            )));
        }
        let rel = relative(&path)?;
        let target = dest.join(&rel);
        if kind.is_dir() {
            make_dirs(&target)?;
            continue;
        }
        if rel.as_os_str().is_empty() {
            return Err(invalid("the archive holds a file without a name"));
        }
        if let Some(parent) = target.parent() {
            make_dirs(parent).map_err(|e| {
                invalid(format!(
                    "cannot unpack {}: it is both a file and a folder in the archive ({e})",
                    path.display()
                ))
            })?;
        }
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut out = opts.open(&target).map_err(|e| {
            if e.kind() == std::io::ErrorKind::AlreadyExists {
                invalid(format!(
                    "the archive holds {} twice; each file may appear once",
                    path.display()
                ))
            } else {
                AliceError::Io(e)
            }
        })?;
        loop {
            let n = entry.read(&mut buf).map_err(read_err)?;
            if n == 0 {
                break;
            }
            total += n as u64;
            if total > budget {
                return Err(too_large());
            }
            out.write_all(&buf[..n])?;
            if stop.load(Ordering::SeqCst) {
                return Err(AliceError::Other(crate::importer::STOPPED.to_string()));
            }
        }
        out.flush()?;
    }
    Ok(total)
}

/// Where the palace and its knowledge graph are inside an unpacked upload.
#[derive(Debug)]
pub struct Located {
    /// The shallowest folder that is a MemPalace palace.
    pub palace: PathBuf,
    /// The folder of the knowledge graph when it is not the palace folder itself (`None`: the
    /// palace holds it, or the upload has none).
    pub kg_dir: Option<PathBuf>,
}

/// Finds the palace root the way `alice import mempalace --palace` validates one (a folder
/// that holds `sqlite_exact.sqlite3` or `chroma.sqlite3`), shallowest first, and the knowledge
/// graph anywhere. More than one candidate at the shallowest depth is an error, never a guess.
pub fn locate(root: &Path) -> Result<Located> {
    let mut palaces: Vec<(usize, PathBuf)> = Vec::new();
    let mut graphs: Vec<(usize, PathBuf)> = Vec::new();
    let mut todo = vec![(0usize, root.to_path_buf())];
    while let Some((depth, dir)) = todo.pop() {
        if detect_backend(&dir).is_ok() {
            palaces.push((depth, dir.clone()));
        }
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in rd.flatten() {
            let Ok(m) = std::fs::symlink_metadata(e.path()) else {
                continue;
            };
            if m.is_dir() {
                todo.push((depth + 1, e.path()));
            } else if m.is_file() && e.file_name() == KG_FILE {
                graphs.push((depth, dir.clone()));
            }
        }
    }
    let shallowest = |mut v: Vec<(usize, PathBuf)>| -> Vec<PathBuf> {
        v.sort();
        let Some(&(d, _)) = v.first() else {
            return Vec::new();
        };
        v.into_iter()
            .take_while(|(x, _)| *x == d)
            .map(|(_, p)| p)
            .collect()
    };
    let shown = |paths: &[PathBuf]| -> String {
        let mut names: Vec<String> = paths
            .iter()
            .map(|p| {
                p.strip_prefix(root)
                    .map(|r| r.display().to_string())
                    .unwrap_or_default()
            })
            .map(|n| {
                if n.is_empty() {
                    "(the top folder)".into()
                } else {
                    n
                }
            })
            .collect();
        names.sort();
        names.join(", ")
    };
    let found = shallowest(palaces);
    let palace = match found.as_slice() {
        [] => {
            return Err(invalid(
                "no MemPalace palace found in the upload: it must hold a folder with sqlite_exact.sqlite3 or chroma.sqlite3 (pack your palace folder with `tar -czf`)",
            ));
        }
        [one] => one.clone(),
        many => {
            return Err(invalid(format!(
                "the upload holds more than one MemPalace palace ({}); upload one palace at a time",
                shown(many)
            )));
        }
    };
    // The palace folder's own graph is found by the importer. Otherwise the shallowest one.
    let kg_dir = if palace.join(KG_FILE).is_file() {
        None
    } else {
        let found = shallowest(graphs);
        match found.as_slice() {
            [] => None,
            [one] => Some(one.clone()),
            many => {
                return Err(invalid(format!(
                    "the upload holds more than one {KG_FILE} ({}); upload one",
                    shown(many)
                )));
            }
        }
    };
    Ok(Located { palace, kg_dir })
}
