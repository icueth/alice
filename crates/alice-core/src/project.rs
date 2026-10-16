//! Detect which project a working directory belongs to.
//!
//! A git checkout is keyed by its normalized `origin` remote (after `insteadOf` rewrites), so a
//! worktree and its main checkout are one project. A repository without a remote is keyed by
//! the path of its main checkout (`path:<main>`, or the repository directory when it is bare),
//! which its worktrees share too; `root` is still the checkout actually in use. A plain
//! directory is keyed by its own path. `$HOME`, the filesystem root and directories that no
//! longer exist map to `global`.

use crate::normalize::slugify;
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectRef {
    /// Stable identity: the normalized remote; for a repository without a remote,
    /// `path:<main checkout>` (or `path:<git dir>` when bare or not named `.git`); for a plain
    /// directory, `path:<dir>`; or `global`.
    pub key: String,
    pub slug: String,
    pub name: String,
    pub root: Option<PathBuf>,
    pub remote: Option<String>,
}

impl ProjectRef {
    pub fn global() -> Self {
        Self {
            key: "global".into(),
            slug: "global".into(),
            name: "Global".into(),
            root: None,
            remote: None,
        }
    }

    /// The working tree in use, keyed by the project (its rules are the project's, whatever
    /// tree they were read in); `None` without one.
    pub fn tree(&self) -> Option<crate::settings::ProjectTree> {
        self.root
            .as_ref()
            .map(|r| crate::settings::ProjectTree::new(self.key.clone(), r.clone()))
    }
}

/// `git@github.com:Me/Repo.git` -> `github.com/me/repo`. Credentials and ports are dropped.
/// Local paths (`/srv/repo.git`, `C:\repo`, `file://…`) are not remotes and give `None`.
pub fn normalize_remote(url: &str) -> Option<String> {
    let url = url.trim().trim_end_matches('/');
    let url = url.strip_suffix(".git").unwrap_or(url);
    if url.is_empty() {
        return None;
    }
    let (host, path) = if let Some(rest) = url.split_once("://").map(|(_, r)| r) {
        // Credentials end at the last `@` of the authority; the path may contain `@` too.
        let (authority, path) = rest.split_once('/')?;
        let host = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
        (host.split(':').next()?.to_string(), path.to_string())
    } else if let Some((user_host, path)) = url.split_once(':') {
        // scp-like `[user@]host:path`. Like git, a slash before the colon or a one-letter
        // "host" (a Windows drive) means a local path.
        if user_host.contains(['/', '\\']) || user_host.len() == 1 {
            return None;
        }
        let host = user_host.rsplit_once('@').map_or(user_host, |(_, h)| h);
        (host.to_string(), path.to_string())
    } else {
        return None;
    };
    let path = path.trim_matches('/');
    if host.is_empty() || path.is_empty() {
        return None;
    }
    Some(format!("{host}/{path}").to_lowercase())
}

/// Variables that point git at a specific repository (`git rev-parse --local-env-vars`).
/// A long-lived daemon may inherit them from whatever started it, e.g. a git hook.
const GIT_REPO_ENV: [&str; 15] = [
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_CONFIG",
    "GIT_CONFIG_PARAMETERS",
    "GIT_CONFIG_COUNT",
    "GIT_OBJECT_DIRECTORY",
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_IMPLICIT_WORK_TREE",
    "GIT_GRAFT_FILE",
    "GIT_INDEX_FILE",
    "GIT_NO_REPLACE_OBJECTS",
    "GIT_REPLACE_REF_BASE",
    "GIT_PREFIX",
    "GIT_SHALLOW_FILE",
    "GIT_COMMON_DIR",
];

/// `git -C <dir>` that discovers the repository from `dir` alone.
fn git_command(dir: &Path) -> Command {
    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(dir);
    for var in GIT_REPO_ENV {
        cmd.env_remove(var);
    }
    cmd
}

fn git_output(dir: &Path, args: &[&str]) -> Option<String> {
    let out = git_command(dir).args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8(out.stdout).ok()?.trim().to_string();
    (!s.is_empty()).then_some(s)
}

/// The checkout's top-level directory and the repository's common git directory, which a
/// worktree shares with its main checkout. One git spawn.
fn git_toplevel_and_common_dir(dir: &Path) -> Option<(PathBuf, Option<PathBuf>)> {
    let out = git_output(
        dir,
        &[
            "rev-parse",
            "--show-toplevel",
            "--path-format=absolute",
            "--git-common-dir",
        ],
    )?;
    parse_toplevel_and_common_dir(&out)
}

/// The first line is the top-level directory. The common dir is trusted only when the output is
/// exactly two lines and the second is absolute: `rev-parse` echoes options it does not know and
/// still exits 0, so git older than 2.31 prints `--path-format=absolute` as a line of its own.
/// The caller then falls back to the top-level directory.
fn parse_toplevel_and_common_dir(stdout: &str) -> Option<(PathBuf, Option<PathBuf>)> {
    let mut lines = stdout.lines();
    let top = lines.next().filter(|l| !l.is_empty())?;
    let common_dir = match (lines.next(), lines.next()) {
        (Some(c), None) if !c.is_empty() && Path::new(c).is_absolute() => Some(PathBuf::from(c)),
        _ => None,
    };
    Some((PathBuf::from(top), common_dir))
}

/// The main checkout every worktree belongs to: the parent of `<main>/.git`, or the
/// repository directory itself when it is bare.
fn main_checkout(common_dir: &Path) -> PathBuf {
    match (common_dir.file_name(), common_dir.parent()) {
        (Some(name), Some(parent)) if name == ".git" => parent.to_path_buf(),
        _ => common_dir.to_path_buf(),
    }
}

/// `canonicalize` on Windows returns `\\?\C:\…` (and `\\?\UNC\server\share\…`), which git and
/// people do not use; strip it so keys and `git -C` see the ordinary form. Identity elsewhere,
/// and on Windows whenever the ordinary form would not name the same file (see
/// [`ordinary_form`]).
pub fn strip_verbatim(p: PathBuf) -> PathBuf {
    #[cfg(windows)]
    if let Some(ordinary) = p.to_str().and_then(ordinary_form) {
        return PathBuf::from(ordinary);
    }
    p
}

/// Win32 device names: in the ordinary (non-verbatim) form, a component named like these
/// (with or without an extension) means the device, not a file.
#[cfg_attr(not(windows), allow(dead_code))]
const RESERVED_NAMES: [&str; 22] = [
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

/// The ordinary form of a Windows verbatim path (`\\?\C:\a` -> `C:\a`, `\\?\UNC\srv\share\a` ->
/// `\\srv\share\a`), or `None` when there is none that names the same file: another verbatim
/// kind (`\\?\Volume{…}\`), a component that ends in `.` or a space (the ordinary form trims
/// them), a reserved device name (`CON`, `nul.txt`, `LPT1`…), or 260 UTF-16 units or more
/// (`MAX_PATH`). Pure string work, so it is tested on every OS.
#[cfg_attr(not(windows), allow(dead_code))]
fn ordinary_form(verbatim: &str) -> Option<String> {
    let (ordinary, names) = if let Some(rest) = verbatim.strip_prefix(r"\\?\UNC\") {
        // A server and a share at least.
        if rest.split('\\').filter(|n| !n.is_empty()).count() < 2 {
            return None;
        }
        (format!(r"\\{rest}"), rest)
    } else {
        // `X:\` exactly: `\\?\X:` alone would become the drive-relative `X:`.
        let rest = verbatim.strip_prefix(r"\\?\")?;
        let b = rest.as_bytes();
        if !(b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && b[2] == b'\\') {
            return None;
        }
        (rest.to_string(), &rest[3..])
    };
    let safe = |name: &str| {
        let stem = name.split('.').next().unwrap_or_default().trim_end();
        !name.ends_with(['.', ' ']) && !RESERVED_NAMES.iter().any(|r| r.eq_ignore_ascii_case(stem))
    };
    let fits = ordinary.encode_utf16().count() < 260;
    (fits && names.split('\\').filter(|n| !n.is_empty()).all(safe)).then_some(ordinary)
}

/// `path:<dir>`. On Windows with `/` separators and an upper-case drive letter, the form git
/// prints, so a directory keeps its key when it becomes a repository without a remote.
fn path_key(p: &Path) -> String {
    let shown = p.display().to_string();
    #[cfg(windows)]
    let shown = windows_key_form(&shown);
    format!("path:{shown}")
}

/// `c:\work\repo` and `C:/work/repo` -> `C:/work/repo`; `\\srv\share` -> `//srv/share`.
#[cfg_attr(not(windows), allow(dead_code))]
fn windows_key_form(path: &str) -> String {
    let mut s = path.replace('\\', "/");
    if s.as_bytes().get(1) == Some(&b':') {
        s[..1].make_ascii_uppercase();
    }
    s
}

/// `canonicalize` without the Windows verbatim prefix (see [`strip_verbatim`]).
fn canonical(p: &Path) -> std::io::Result<PathBuf> {
    p.canonicalize().map(strip_verbatim)
}

/// Both paths must come from [`canonical`].
fn is_home_or_root(dir: &Path, home: Option<&Path>) -> bool {
    dir.parent().is_none() || home == Some(dir)
}

/// Resolve the project for `cwd`. Never fails: unknown or unusable directories map to `global`.
pub fn detect_project(cwd: &Path) -> ProjectRef {
    detect_with_home(cwd, std::env::home_dir().as_deref())
}

fn detect_with_home(cwd: &Path, home: Option<&Path>) -> ProjectRef {
    let Ok(dir) = canonical(cwd) else {
        return ProjectRef::global();
    };
    let home = home.map(|h| canonical(h).unwrap_or_else(|_| h.to_path_buf()));
    let home = home.as_deref();
    if is_home_or_root(&dir, home) {
        return ProjectRef::global();
    }
    if let Some((root, common_dir)) = git_toplevel_and_common_dir(&dir)
        // A repository rooted at $HOME (dotfiles) does not claim every directory below it.
        && !is_home_or_root(&canonical(&root).unwrap_or_else(|_| root.clone()), home)
    {
        // `get-url` applies `url.<base>.insteadOf` rewrites; the raw config value does not.
        let remote_raw = git_output(&root, &["remote", "get-url", "origin"]);
        let remote = remote_raw.as_deref().and_then(normalize_remote);
        let main = common_dir
            .as_deref()
            .map_or_else(|| root.clone(), main_checkout);
        let name = match &remote {
            Some(r) => r.rsplit('/').next().unwrap_or(r).to_string(),
            None => main
                .file_name()
                .map(|n| n.to_string_lossy())
                .map(|n| n.strip_suffix(".git").unwrap_or(&n).to_string())
                .unwrap_or_default(),
        };
        let key = remote.clone().unwrap_or_else(|| path_key(&main));
        let slug = slugify(&name);
        if slug.is_empty() {
            return ProjectRef::global();
        }
        return ProjectRef {
            key,
            slug,
            name,
            root: Some(root),
            remote,
        };
    }
    let name = dir
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let slug = slugify(&name);
    if slug.is_empty() {
        return ProjectRef::global();
    }
    ProjectRef {
        key: path_key(&dir),
        slug,
        name,
        root: Some(dir),
        remote: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remotes_normalise_to_host_and_path() {
        assert_eq!(
            normalize_remote("git@github.com:icueth/alice.git").as_deref(),
            Some("github.com/icueth/alice")
        );
        assert_eq!(
            normalize_remote("https://github.com/icueth/Alice.git/").as_deref(),
            Some("github.com/icueth/alice")
        );
        assert_eq!(
            normalize_remote("ssh://git@gitlab.example.com:2222/a/b.git").as_deref(),
            Some("gitlab.example.com/a/b")
        );
        assert_eq!(
            normalize_remote("https://user:secret@host.io/x/y").as_deref(),
            Some("host.io/x/y")
        );
        assert_eq!(normalize_remote(""), None);
        assert_eq!(
            normalize_remote("https://ci:t@ken@host.io/team/repo@v2.git").as_deref(),
            Some("host.io/team/repo@v2")
        );
    }

    #[test]
    fn local_path_remotes_are_not_remote_keys() {
        // Git treats these as local paths, so the project keeps its `path:` key.
        for url in [
            "/srv/git/repo.git",
            "../sibling/repo",
            "file:///srv/git/repo.git",
            "C:\\repos\\tool.git",
            "C:/repos/tool.git",
            "./odd:name/repo",
        ] {
            assert_eq!(normalize_remote(url), None, "{url}");
        }
    }

    fn git(dir: &Path, args: &[&str]) {
        let ok = git_command(dir)
            .args(args)
            .output()
            .unwrap()
            .status
            .success();
        assert!(ok, "git {args:?} failed");
    }

    #[test]
    fn repo_with_remote_uses_remote_key_and_worktree_matches() {
        let tmp = tempfile::tempdir().unwrap();
        let main = tmp.path().join("my_app");
        std::fs::create_dir(&main).unwrap();
        git(&main, &["init", "-q", "-b", "main"]);
        git(
            &main,
            &["remote", "add", "origin", "git@github.com:me/My_App.git"],
        );
        git(
            &main,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "-q",
                "--allow-empty",
                "-m",
                "x",
            ],
        );
        let p = detect_project(&main);
        assert_eq!(p.key, "github.com/me/my_app");
        assert_eq!(p.slug, "my-app");

        let wt = tmp.path().join("my_app-wt");
        git(
            &main,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "feature",
                wt.to_str().unwrap(),
            ],
        );
        assert_eq!(detect_project(&wt).key, p.key);
        assert_eq!(detect_project(&wt.join(".")).slug, "my-app");

        // A subdirectory belongs to the repository it sits in.
        let sub = main.join("src").join("deep");
        std::fs::create_dir_all(&sub).unwrap();
        assert_eq!(detect_project(&sub), p);
    }

    #[test]
    fn repo_without_remote_uses_path_key() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("Local Tool");
        std::fs::create_dir(&repo).unwrap();
        git(&repo, &["init", "-q"]);
        let p = detect_project(&repo.join("."));
        assert!(p.key.starts_with("path:"), "{}", p.key);
        assert_eq!(p.slug, "local-tool");
        assert!(p.remote.is_none());
    }

    fn commit_empty(dir: &Path) {
        git(
            dir,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "-q",
                "--allow-empty",
                "-m",
                "x",
            ],
        );
    }

    fn same_dir(a: &Path, b: &Path) -> bool {
        a.canonicalize().unwrap() == b.canonicalize().unwrap()
    }

    #[test]
    fn worktree_of_repo_without_remote_shares_the_main_checkout_key() {
        let tmp = tempfile::tempdir().unwrap();
        let main = tmp.path().join("Local Tool");
        std::fs::create_dir(&main).unwrap();
        git(&main, &["init", "-q"]);
        commit_empty(&main);
        let wt = tmp.path().join(".worktrees").join("feature-x");
        git(
            &main,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "feature-x",
                wt.to_str().unwrap(),
            ],
        );
        let p = detect_project(&main);
        let w = detect_project(&wt.join("."));
        assert!(p.key.starts_with("path:"), "{}", p.key);
        assert_eq!(w.key, p.key);
        assert_eq!(w.slug, "local-tool");
        assert_eq!(w.name, p.name);
        assert!(w.remote.is_none());
        // `root` stays the checkout actually in use.
        assert!(same_dir(w.root.as_deref().unwrap(), &wt), "{:?}", w.root);
        assert!(same_dir(p.root.as_deref().unwrap(), &main), "{:?}", p.root);
    }

    #[test]
    fn worktree_of_bare_repo_is_keyed_by_the_repository() {
        let tmp = tempfile::tempdir().unwrap();
        let seed = tmp.path().join("seed");
        std::fs::create_dir(&seed).unwrap();
        git(&seed, &["init", "-q", "-b", "main"]);
        commit_empty(&seed);
        let bare = tmp.path().join("tool.git");
        let bare_s = bare.to_str().unwrap();
        git(tmp.path(), &["init", "-q", "--bare", bare_s]);
        git(&seed, &["push", "-q", bare_s, "main"]);
        let wt = tmp.path().join("tool-wt");
        git(
            &bare,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "b",
                wt.to_str().unwrap(),
                "main",
            ],
        );
        let p = detect_project(&wt);
        assert!(p.key.starts_with("path:"), "{}", p.key);
        assert!(p.key.ends_with("tool.git"), "{}", p.key);
        assert_eq!(p.slug, "tool");
        assert!(same_dir(p.root.as_deref().unwrap(), &wt), "{:?}", p.root);
    }

    #[test]
    fn rev_parse_output_must_be_two_lines_with_an_absolute_common_dir() {
        // Built from temp_dir so the paths are absolute on every OS.
        let top = std::env::temp_dir().join("app");
        let common = top.join(".git");
        let (t, c) = (top.display(), common.display());
        assert_eq!(
            parse_toplevel_and_common_dir(&format!("{t}\n{c}\n")),
            Some((top.clone(), Some(common.clone())))
        );
        assert_eq!(
            parse_toplevel_and_common_dir(&format!("{t}\r\n{c}")),
            Some((top.clone(), Some(common.clone())))
        );
        // Git older than 2.31 echoes the unknown `--path-format=absolute` and exits 0; using
        // that line as the common dir would key every repository without a remote alike.
        assert_eq!(
            parse_toplevel_and_common_dir(&format!("{t}\n--path-format=absolute\n.git\n")),
            Some((top.clone(), None))
        );
        assert_eq!(
            parse_toplevel_and_common_dir(&format!("{t}\n--path-format=absolute\n{c}\n")),
            Some((top.clone(), None))
        );
        // A relative common dir cannot be placed without knowing git's working directory.
        assert_eq!(
            parse_toplevel_and_common_dir(&format!("{t}\n.git\n")),
            Some((top.clone(), None))
        );
        assert_eq!(
            parse_toplevel_and_common_dir(&format!("{t}\n")),
            Some((top.clone(), None))
        );
        assert_eq!(parse_toplevel_and_common_dir(""), None);
    }

    #[test]
    fn remote_insteadof_rewrites_are_expanded() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("short");
        std::fs::create_dir(&repo).unwrap();
        git(&repo, &["init", "-q"]);
        git(&repo, &["config", "url.git@github.com:.insteadOf", "gh:"]);
        git(&repo, &["remote", "add", "origin", "gh:me/Repo.git"]);
        let p = detect_project(&repo);
        assert_eq!(p.key, "github.com/me/repo");
        assert_eq!(p.remote.as_deref(), Some("github.com/me/repo"));
        assert_eq!(p.slug, "repo");
    }

    #[test]
    fn plain_directory_home_and_missing_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("notes");
        std::fs::create_dir(&dir).unwrap();
        assert_eq!(detect_project(&dir).slug, "notes");
        if let Some(home) = std::env::home_dir() {
            assert_eq!(detect_project(&home).key, "global");
        }
        assert_eq!(
            detect_project(Path::new("/definitely/not/here/xyz")).key,
            "global"
        );
        assert_eq!(detect_project(Path::new("/")).key, "global");
    }

    #[test]
    fn deleted_directory_falls_back_to_global() {
        let tmp = tempfile::tempdir().unwrap();
        let gone = tmp.path().join("was-here");
        std::fs::create_dir(&gone).unwrap();
        assert_eq!(detect_project(&gone).slug, "was-here");
        std::fs::remove_dir(&gone).unwrap();
        assert_eq!(detect_project(&gone), ProjectRef::global());
    }

    #[test]
    fn home_that_is_a_git_repo_does_not_swallow_its_subdirectories() {
        // Some people keep dotfiles in a repository rooted at $HOME.
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let docs = home.join("Documents");
        std::fs::create_dir_all(&docs).unwrap();
        git(&home, &["init", "-q"]);
        assert_eq!(detect_with_home(&home, Some(&home)), ProjectRef::global());
        let p = detect_with_home(&docs, Some(&home));
        assert_eq!(p.slug, "documents");
        assert!(p.key.ends_with("Documents"), "{}", p.key);
        // Without the home rule the same directory is part of the repository.
        assert_eq!(detect_with_home(&docs, None).slug, "home");
    }

    #[test]
    fn strip_verbatim_keeps_ordinary_paths() {
        let p = PathBuf::from("/tmp/x");
        assert_eq!(strip_verbatim(p.clone()), p);
        #[cfg(windows)]
        {
            assert_eq!(
                strip_verbatim(PathBuf::from(r"\\?\C:\work\repo")),
                PathBuf::from(r"C:\work\repo")
            );
            assert_eq!(
                strip_verbatim(PathBuf::from(r"\\?\UNC\srv\share\r")),
                PathBuf::from(r"\\srv\share\r")
            );
            let volume = PathBuf::from(r"\\?\Volume{b75e2c83-0000-0000-0000-602f00000000}\r");
            assert_eq!(strip_verbatim(volume.clone()), volume);
            // What detection sees: the ordinary form of the current directory.
            let cwd = canonical(Path::new(".")).unwrap();
            assert!(
                !cwd.to_string_lossy().starts_with(r"\\?\"),
                "{}",
                cwd.display()
            );
        }
    }

    /// Pure string checks, so every OS runs them.
    #[test]
    fn the_verbatim_prefix_is_kept_where_the_ordinary_form_names_another_file() {
        let o = |s: &str| ordinary_form(s);
        assert_eq!(o(r"\\?\C:\work\repo").as_deref(), Some(r"C:\work\repo"));
        assert_eq!(o(r"\\?\d:\").as_deref(), Some(r"d:\"));
        assert_eq!(
            o(r"\\?\C:\ข้อมูล ทดสอบ\x").as_deref(),
            Some(r"C:\ข้อมูล ทดสอบ\x")
        );
        assert_eq!(o(r"\\?\UNC\srv\share\r").as_deref(), Some(r"\\srv\share\r"));
        // Names that merely contain a device name, or a dot inside, are ordinary.
        assert!(o(r"\\?\C:\CONSOLE\con-tools\COM10\lpt\a.b").is_some());
        for kept in [
            // Not a drive or UNC path.
            r"\\?\Volume{b75e2c83-0000-0000-0000-602f00000000}\r",
            r"\\?\C:",
            r"\\?\UNC\srv",
            r"C:\work",
            r"/tmp/x",
            // The ordinary form trims a trailing dot or space.
            r"\\?\C:\work.\repo",
            r"\\?\C:\work\repo ",
            r"\\?\UNC\srv\share.\r",
            // Device names, any case, with or without an extension.
            r"\\?\C:\work\CON",
            r"\\?\C:\work\nul.txt",
            r"\\?\C:\Aux\x",
            r"\\?\C:\work\prn .log",
            r"\\?\C:\work\com1",
            r"\\?\C:\work\LPT9.tar.gz",
        ] {
            assert_eq!(o(kept), None, "{kept}");
        }
        // MAX_PATH: 259 UTF-16 units still fit, 260 do not; a Thai letter is one unit.
        let long = |n: usize| format!(r"\\?\C:\{}", "ก".repeat(n - 3));
        assert_eq!(o(&long(259)).map(|s| s.encode_utf16().count()), Some(259));
        assert_eq!(o(&long(260)), None);
    }

    #[test]
    fn path_keys_use_the_form_git_prints() {
        assert_eq!(windows_key_form(r"c:\work\repo"), "C:/work/repo");
        assert_eq!(windows_key_form("C:/work/repo"), "C:/work/repo");
        assert_eq!(windows_key_form(r"\\srv\share\r"), "//srv/share/r");
        // Only Windows rewrites: a backslash is an ordinary character in a Unix name.
        let odd = Path::new("/tmp/a\\b");
        #[cfg(not(windows))]
        assert_eq!(path_key(odd), "path:/tmp/a\\b");
        #[cfg(windows)]
        assert_eq!(path_key(odd), "path:/tmp/a/b");
    }

    /// The key of a plain directory comes from its canonical path, that of a repository
    /// without a remote from git's output (`C:/…` on Windows): both name it the same way.
    #[test]
    fn a_directory_keeps_its_key_when_it_becomes_a_repository_without_a_remote() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("Plain Tool");
        std::fs::create_dir(&dir).unwrap();
        let plain = detect_project(&dir);
        git(&dir, &["init", "-q"]);
        let repo = detect_project(&dir);
        assert!(plain.key.starts_with("path:"), "{}", plain.key);
        assert_eq!(repo.key, plain.key);
        assert_eq!(repo.slug, "plain-tool");
        #[cfg(windows)]
        assert!(!plain.key.contains('\\'), "{}", plain.key);
    }

    #[test]
    fn git_runs_without_inherited_repository_variables() {
        // A daemon first started from a git hook inherits GIT_DIR and friends; they must not
        // redirect detection for every later working directory.
        let cmd = git_command(Path::new("."));
        let removed: Vec<_> = cmd
            .get_envs()
            .filter(|(_, v)| v.is_none())
            .map(|(k, _)| k.to_string_lossy().into_owned())
            .collect();
        for var in [
            "GIT_DIR",
            "GIT_WORK_TREE",
            "GIT_COMMON_DIR",
            "GIT_CONFIG_PARAMETERS",
        ] {
            assert!(removed.iter().any(|r| r == var), "{var} not removed");
        }
    }
}
