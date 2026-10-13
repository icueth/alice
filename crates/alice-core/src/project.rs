//! Detect which project a working directory belongs to.
//!
//! A git checkout is keyed by its normalized `origin` remote, so a worktree and its main checkout
//! are one project. A repository without a remote, or a plain directory, is keyed by its path
//! (`path:<root>`); a worktree of a repository without a remote therefore gets its own key.
//! `$HOME`, the filesystem root and directories that no longer exist map to `global`.

use crate::normalize::slugify;
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectRef {
    /// Stable identity: normalized remote, `path:<root>`, or `global`.
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

/// Both paths must be canonical (on Windows, `canonicalize` adds a `\\?\` prefix).
fn is_home_or_root(dir: &Path, home: Option<&Path>) -> bool {
    dir.parent().is_none() || home == Some(dir)
}

/// Resolve the project for `cwd`. Never fails: unknown or unusable directories map to `global`.
pub fn detect_project(cwd: &Path) -> ProjectRef {
    detect_with_home(cwd, std::env::home_dir().as_deref())
}

fn detect_with_home(cwd: &Path, home: Option<&Path>) -> ProjectRef {
    let Ok(dir) = cwd.canonicalize() else {
        return ProjectRef::global();
    };
    let home = home.map(|h| h.canonicalize().unwrap_or_else(|_| h.to_path_buf()));
    let home = home.as_deref();
    if is_home_or_root(&dir, home) {
        return ProjectRef::global();
    }
    if let Some(root) = git_output(&dir, &["rev-parse", "--show-toplevel"]).map(PathBuf::from)
        // A repository rooted at $HOME (dotfiles) does not claim every directory below it.
        && !is_home_or_root(&root.canonicalize().unwrap_or_else(|_| root.clone()), home)
    {
        let remote_raw = git_output(&root, &["config", "--get", "remote.origin.url"]);
        let remote = remote_raw.as_deref().and_then(normalize_remote);
        let name = match &remote {
            Some(r) => r.rsplit('/').next().unwrap_or(r).to_string(),
            None => root
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default(),
        };
        let key = remote
            .clone()
            .unwrap_or_else(|| format!("path:{}", root.display()));
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
        key: format!("path:{}", dir.display()),
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
