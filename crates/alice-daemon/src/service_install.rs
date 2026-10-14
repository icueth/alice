//! `alice service install|uninstall` (spec §2.2 rule 5): a launchd agent on macOS, a systemd
//! user unit on Linux, a logon scheduled task on Windows. Plans are data, so they are tested
//! without touching the user's service manager; `apply` writes files and runs commands only
//! when given a runner.
//!
//! `alice restore` also uses this module: it stops the installed unit before the swap and
//! starts it again afterwards ([`suspend`], [`resume`]), so a unit that restarts its daemon
//! cannot start one in the middle of a restore.

use crate::paths::AliceHome;
use alice_core::error::{AliceError, Result};
use std::path::{Path, PathBuf};

pub const LABEL: &str = "dev.alice.daemon";
/// The systemd user unit's name.
pub const UNIT: &str = "alice.service";
/// The scheduled task's name on Windows: the launchd label, which no unrelated task is likely to
/// carry (`/F` replaces a task of the same name).
pub const TASK: &str = LABEL;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
    MacOs,
    Linux,
    Windows,
}

impl Platform {
    pub fn current() -> Option<Platform> {
        if cfg!(target_os = "macos") {
            Some(Platform::MacOs)
        } else if cfg!(target_os = "linux") {
            Some(Platform::Linux)
        } else if cfg!(windows) {
            Some(Platform::Windows)
        } else {
            None
        }
    }
}

#[derive(Debug, Clone, PartialEq, Default, serde::Serialize)]
pub struct InstallPlan {
    pub files: Vec<(PathBuf, String)>,
    pub commands: Vec<Vec<String>>,
    pub notes: Vec<String>,
}

pub trait Runner {
    /// Run a command; `Ok` is exit status 0. Nothing reads the error text (it is localised).
    fn run(&self, cmd: &[String]) -> Result<()>;
    /// Run a command and return what it printed on stdout (`Err`: it failed).
    fn output(&self, cmd: &[String]) -> Result<String> {
        self.run(cmd).map(|()| String::new())
    }
}

pub struct SystemRunner;

impl SystemRunner {
    fn exec(cmd: &[String]) -> Result<std::process::Output> {
        let Some((program, args)) = cmd.split_first() else {
            return Err(AliceError::Other("an empty command".into()));
        };
        let out = std::process::Command::new(program).args(args).output()?;
        if !out.status.success() {
            let why = String::from_utf8_lossy(&out.stderr).trim().to_string();
            return Err(AliceError::Other(format!(
                "`{}` exited with {}: {why}",
                cmd.join(" "),
                out.status
            )));
        }
        Ok(out)
    }
}

impl Runner for SystemRunner {
    fn run(&self, cmd: &[String]) -> Result<()> {
        Self::exec(cmd).map(|_| ())
    }
    fn output(&self, cmd: &[String]) -> Result<String> {
        Self::exec(cmd).map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
    }
}

/// The user's home directory, where the unit files live.
pub fn user_home() -> Option<PathBuf> {
    dirs::home_dir()
}

fn unit_path(p: Platform, root: &Path) -> Option<PathBuf> {
    match p {
        Platform::MacOs => Some(
            root.join("Library/LaunchAgents")
                .join(format!("{LABEL}.plist")),
        ),
        Platform::Linux => Some(
            linux_unit_dir(
                root,
                std::env::var_os("XDG_CONFIG_HOME").as_deref(),
                dirs::home_dir().as_deref(),
            )
            .join(UNIT),
        ),
        Platform::Windows => None,
    }
}

/// Where systemd reads user units: `$XDG_CONFIG_HOME/systemd/user` when that is set and `root`
/// is the user's real home (a test or `--target-dir` root never follows the real environment).
fn linux_unit_dir(root: &Path, xdg: Option<&std::ffi::OsStr>, real_home: Option<&Path>) -> PathBuf {
    match xdg.filter(|x| !x.is_empty()) {
        Some(x) if real_home == Some(root) => PathBuf::from(x).join("systemd/user"),
        _ => root.join(".config/systemd/user"),
    }
}

fn xml(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// A word of a systemd unit file: `%` doubled (it starts a specifier), and quoted when it holds
/// whitespace, quotes or backslashes. `prefix` stays outside the quotes' meaning (`KEY=value`
/// assignments are quoted as a whole).
fn systemd_word(s: &str) -> String {
    let s = s.replace('%', "%%");
    if s.chars()
        .any(|c| c.is_whitespace() || c == '"' || c == '\'' || c == '\\')
    {
        format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
    } else {
        s
    }
}

/// The plan to run `exe daemon run` for `home` at login. `root` is the user's home directory
/// (where `Library/LaunchAgents` or `.config/systemd/user` live; tests pass a temporary one).
/// `uid` is the numeric user id launchd needs (`gui/<uid>`); it is not used elsewhere.
pub fn plan_install(
    p: Platform,
    root: &Path,
    exe: &Path,
    home: &AliceHome,
    uid: u32,
) -> InstallPlan {
    let exe_s = exe.display().to_string();
    let home_s = home.root().display().to_string();
    let log = home.logs_dir().join("daemon.log").display().to_string();
    match p {
        Platform::MacOs => {
            let path = unit_path(p, root).expect("macOS has a plist");
            let plist = format!(
                r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>{LABEL}</string>
  <key>ProgramArguments</key>
  <array><string>{}</string><string>daemon</string><string>run</string></array>
  <key>EnvironmentVariables</key>
  <dict><key>ALICE_HOME</key><string>{}</string></dict>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><dict><key>SuccessfulExit</key><false/></dict>
  <key>ThrottleInterval</key><integer>30</integer>
  <key>StandardOutPath</key><string>{}</string>
  <key>StandardErrorPath</key><string>{}</string>
</dict>
</plist>
"#,
                xml(&exe_s),
                xml(&home_s),
                xml(&log),
                xml(&log)
            );
            InstallPlan {
                commands: vec![vec![
                    "launchctl".into(),
                    "bootstrap".into(),
                    format!("gui/{uid}"),
                    path.display().to_string(),
                ]],
                files: vec![(path, plist)],
                notes: vec![
                    "`alice daemon stop` stops it until the next login (it exits cleanly, so launchd does not restart it).".into(),
                ],
            }
        }
        Platform::Linux => {
            let path = unit_path(p, root).expect("Linux has a unit");
            let unit = format!(
                "[Unit]\nDescription=Alice memory daemon\nStartLimitIntervalSec=600\nStartLimitBurst=5\n\n[Service]\nExecStart={} daemon run\nEnvironment={}\nStandardOutput=append:{}\nStandardError=append:{}\nRestart=on-failure\nRestartSec=30\n\n[Install]\nWantedBy=default.target\n",
                systemd_word(&exe_s),
                systemd_word(&format!("ALICE_HOME={home_s}")),
                systemd_word(&log),
                systemd_word(&log),
            );
            InstallPlan {
                files: vec![(path, unit)],
                commands: vec![
                    vec!["systemctl".into(), "--user".into(), "daemon-reload".into()],
                    vec![
                        "systemctl".into(),
                        "--user".into(),
                        "enable".into(),
                        "--now".into(),
                        UNIT.into(),
                    ],
                ],
                notes: vec![
                    "User services stop at logout unless lingering is on: `loginctl enable-linger $USER`.".into(),
                ],
            }
        }
        Platform::Windows => InstallPlan {
            files: vec![],
            commands: vec![vec![
                "schtasks".into(),
                "/Create".into(),
                "/SC".into(),
                "ONLOGON".into(),
                "/TN".into(),
                TASK.into(),
                "/TR".into(),
                format!("cmd /c set \"ALICE_HOME={home_s}\"&& \"{exe_s}\" daemon run"),
                "/RL".into(),
                "LIMITED".into(),
                "/F".into(),
            ]],
            notes: vec![
                "Windows support is not yet verified on a real machine; the scheduled task starts the daemon at logon.".into(),
            ],
        },
    }
}

pub fn plan_uninstall(p: Platform, root: &Path, uid: u32) -> InstallPlan {
    let _ = root;
    match p {
        Platform::MacOs => InstallPlan {
            commands: vec![vec![
                "launchctl".into(),
                "bootout".into(),
                format!("gui/{uid}/{LABEL}"),
            ]],
            files: vec![],
            notes: vec![],
        },
        Platform::Linux => InstallPlan {
            commands: vec![
                vec![
                    "systemctl".into(),
                    "--user".into(),
                    "disable".into(),
                    "--now".into(),
                    UNIT.into(),
                ],
                vec!["systemctl".into(), "--user".into(), "daemon-reload".into()],
            ],
            files: vec![],
            notes: vec![],
        },
        Platform::Windows => InstallPlan {
            commands: vec![vec![
                "schtasks".into(),
                "/Delete".into(),
                "/TN".into(),
                TASK.into(),
                "/F".into(),
            ]],
            files: vec![],
            notes: vec![],
        },
    }
}

/// The commands that stop the installed unit's daemon without removing the unit (the unit stays
/// registered, or is loaded again by [`plan_resume`]).
pub fn plan_suspend(p: Platform, uid: u32) -> InstallPlan {
    let command = match p {
        Platform::MacOs => vec![
            "launchctl".into(),
            "bootout".into(),
            format!("gui/{uid}/{LABEL}"),
        ],
        Platform::Linux => vec![
            "systemctl".into(),
            "--user".into(),
            "stop".into(),
            UNIT.into(),
        ],
        Platform::Windows => vec!["schtasks".into(), "/End".into(), "/TN".into(), TASK.into()],
    };
    InstallPlan {
        commands: vec![command],
        ..Default::default()
    }
}

/// The commands that start the unit's daemon again after [`plan_suspend`].
pub fn plan_resume(p: Platform, root: &Path, uid: u32) -> InstallPlan {
    let command = match p {
        Platform::MacOs => vec![
            "launchctl".into(),
            "bootstrap".into(),
            format!("gui/{uid}"),
            unit_path(p, root).unwrap_or_default().display().to_string(),
        ],
        Platform::Linux => vec![
            "systemctl".into(),
            "--user".into(),
            "start".into(),
            UNIT.into(),
        ],
        Platform::Windows => vec!["schtasks".into(), "/Run".into(), "/TN".into(), TASK.into()],
    };
    InstallPlan {
        commands: vec![command],
        ..Default::default()
    }
}

/// The unit file of an installed service (macOS, Linux). Windows keeps the task in the Task
/// Scheduler, not in a file Alice could look at: `None` there.
pub fn installed(p: Platform, root: &Path) -> Option<PathBuf> {
    unit_path(p, root).filter(|f| f.exists())
}

/// Install: write the files, then run the commands (when a runner is given).
pub fn apply(plan: &InstallPlan, runner: Option<&dyn Runner>) -> Result<()> {
    for (path, content) in &plan.files {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(path, content)?;
    }
    if let Some(r) = runner {
        for c in &plan.commands {
            r.run(c)?;
        }
    }
    Ok(())
}

/// Whether the service manager has the unit loaded (launchd: `print`; systemd: `is-active`;
/// Windows: the task exists). By exit status only.
fn is_loaded(p: Platform, uid: u32, r: &dyn Runner) -> bool {
    let probe: Vec<String> = match p {
        Platform::MacOs => vec![
            "launchctl".into(),
            "print".into(),
            format!("gui/{uid}/{LABEL}"),
        ],
        Platform::Linux => vec![
            "systemctl".into(),
            "--user".into(),
            "is-active".into(),
            UNIT.into(),
        ],
        Platform::Windows => vec![
            "schtasks".into(),
            "/Query".into(),
            "/TN".into(),
            TASK.into(),
        ],
    };
    r.run(&probe).is_ok()
}

/// Uninstall: stop and unregister what the manager has (when a runner is given), then remove the
/// unit file. What is there is found by exit status (a unit file, a loaded job, an existing
/// task), never by error text; a command that fails on something that is there is returned (the
/// unit file stays, so `alice service status` still shows it and a retry can finish the job).
pub fn uninstall(p: Platform, root: &Path, uid: u32, runner: Option<&dyn Runner>) -> Result<()> {
    if let Some(r) = runner {
        let present = match p {
            Platform::Linux => installed(p, root).is_some(),
            _ => is_loaded(p, uid, r),
        };
        if present {
            for c in &plan_uninstall(p, root, uid).commands {
                r.run(c)?;
            }
        }
    }
    if let Some(f) = installed(p, root) {
        std::fs::remove_file(f)?;
    }
    Ok(())
}

/// Unload a unit that is already loaded, so installing again replaces it (launchd refuses to
/// bootstrap a job that is loaded). Not loaded is fine.
pub fn unload(p: Platform, root: &Path, uid: u32, runner: &dyn Runner) -> Result<()> {
    // Only launchd needs this: systemd's `enable --now` and the task's `/F` replace the old one.
    if p == Platform::MacOs && is_loaded(p, uid, runner) {
        for c in &plan_uninstall(p, root, uid).commands {
            runner.run(c)?;
        }
    }
    Ok(())
}

/// The `ALICE_HOME` the installed unit file sets (`None`: no unit file, or none that names one).
pub fn unit_home(p: Platform, root: &Path) -> Option<PathBuf> {
    let text = std::fs::read_to_string(installed(p, root)?).ok()?;
    match p {
        Platform::MacOs => {
            let key = "<key>ALICE_HOME</key><string>";
            let rest = &text[text.find(key)? + key.len()..];
            let value = &rest[..rest.find("</string>")?];
            Some(PathBuf::from(
                value
                    .replace("&lt;", "<")
                    .replace("&gt;", ">")
                    .replace("&amp;", "&"),
            ))
        }
        Platform::Linux => {
            let line = text.lines().find(|l| l.starts_with("Environment="))?;
            let mut v = line.trim_start_matches("Environment=").to_string();
            if v.starts_with('"') && v.ends_with('"') && v.len() >= 2 {
                v = v[1..v.len() - 1]
                    .replace("\\\"", "\"")
                    .replace("\\\\", "\\");
            }
            let v = v.replace("%%", "%");
            v.strip_prefix("ALICE_HOME=").map(PathBuf::from)
        }
        Platform::Windows => None,
    }
}

/// Whose unit this is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UnitOwner {
    /// No unit is installed.
    None,
    /// The unit serves the data directory asked about.
    This,
    /// The unit serves another data directory.
    Other(PathBuf),
    /// A unit exists but its data directory cannot be read.
    Unreadable,
}

fn canon(x: &Path) -> PathBuf {
    std::fs::canonicalize(x).unwrap_or_else(|_| x.to_path_buf())
}

fn owner_of(found: Option<PathBuf>, home: &Path) -> UnitOwner {
    match found {
        Some(h) if canon(&h) == canon(home) => UnitOwner::This,
        Some(h) => UnitOwner::Other(h),
        None => UnitOwner::Unreadable,
    }
}

/// The data directory the Windows task runs for, from its definition (`schtasks /Query /XML`;
/// the element names are not localised): the `ALICE_HOME=` Alice wrote into the action.
pub fn task_home(xml_text: &str) -> Option<PathBuf> {
    let args = xml_text
        .split("<Arguments>")
        .nth(1)?
        .split("</Arguments>")
        .next()?;
    let args = args
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&");
    let rest = &args[args.find("ALICE_HOME=")? + "ALICE_HOME=".len()..];
    Some(PathBuf::from(&rest[..rest.find('"')?]))
}

/// Whose unit is installed on this machine, for `home`.
pub fn unit_owner(p: Platform, root: &Path, home: &Path, runner: &dyn Runner) -> UnitOwner {
    if p == Platform::Windows {
        let query = ["schtasks", "/Query", "/TN", TASK, "/XML"].map(String::from);
        return match runner.output(&query) {
            Err(_) => UnitOwner::None,
            Ok(xml_text) => owner_of(task_home(&xml_text), home),
        };
    }
    if installed(p, root).is_none() {
        return UnitOwner::None;
    }
    owner_of(unit_home(p, root), home)
}

/// Whether the installed unit file serves `home` (compared after canonicalisation). `false` when
/// there is no unit file, or it serves another data directory.
pub fn unit_serves(p: Platform, root: &Path, home: &Path) -> bool {
    unit_home(p, root).is_some_and(|h| canon(&h) == canon(home))
}

/// What [`suspend`] did.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Suspension {
    /// The unit's daemon was stopped; [`resume`] should start it again.
    pub ended: bool,
    /// A line for the person: a unit that was left alone, and why.
    pub notice: Option<String>,
}

/// Stop the daemon of the service unit that serves `home`, for the duration of a restore. One
/// rule on every OS, by exit status and never by error text (it is localised):
///
/// 1. Find the unit. One that serves another data directory, or whose data directory cannot be
///    read, is left alone, with a notice.
/// 2. Ask the manager whether it is loaded (launchd `print`, systemd `is-active`; Windows: the
///    task exists). Not loaded: nothing to suspend.
/// 3. Loaded under launchd or systemd, which restart what exits: stop it. If the stop fails the
///    restore must not go on (it could restart the daemon in the middle of the swap): `Err`.
/// 4. A Windows task has no restart policy: `/End` is best effort, a failure never refuses (the
///    restore stops the daemon process itself), and `/Run` follows only after a successful `/End`.
pub fn suspend(
    p: Platform,
    root: &Path,
    uid: u32,
    home: &Path,
    runner: &dyn Runner,
) -> Result<Suspension> {
    let left_alone = |why: String| {
        Ok(Suspension {
            ended: false,
            notice: Some(format!(
                "a service unit is installed but {why}; it was left alone"
            )),
        })
    };
    match unit_owner(p, root, home, runner) {
        UnitOwner::None => return Ok(Suspension::default()),
        UnitOwner::Other(h) => return left_alone(format!("serves {}", h.display())),
        UnitOwner::Unreadable => {
            return left_alone("the data directory it serves could not be read".into());
        }
        UnitOwner::This => {}
    }
    if !is_loaded(p, uid, runner) {
        return Ok(Suspension::default());
    }
    for c in &plan_suspend(p, uid).commands {
        if let Err(e) = runner.run(c) {
            if p == Platform::Windows {
                tracing::info!(error = %e, "the scheduled task could not be ended");
                return Ok(Suspension::default());
            }
            return Err(e);
        }
    }
    Ok(Suspension {
        ended: true,
        notice: None,
    })
}

/// Start the unit's daemon again after [`suspend`].
pub fn resume(p: Platform, root: &Path, uid: u32, runner: &dyn Runner) -> Result<()> {
    for c in &plan_resume(p, root, uid).commands {
        runner.run(c)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn home() -> AliceHome {
        AliceHome::new(PathBuf::from("/Users/u/.alice"))
    }

    #[test]
    fn the_macos_plan_is_a_launch_agent_for_this_binary_and_home() {
        let root = PathBuf::from("/tmp/root");
        let p = plan_install(
            Platform::MacOs,
            &root,
            Path::new("/opt/alice/bin/alice"),
            &home(),
            501,
        );
        let (path, plist) = &p.files[0];
        assert_eq!(
            path,
            &root.join("Library/LaunchAgents/dev.alice.daemon.plist")
        );
        for want in [
            "<string>dev.alice.daemon</string>",
            "<string>/opt/alice/bin/alice</string>",
            "<string>daemon</string>",
            "<string>run</string>",
            "<key>ALICE_HOME</key>",
            "<string>/Users/u/.alice</string>",
        ] {
            assert!(plist.contains(want), "{want}\n{plist}");
        }
        // The log file, spelled as this platform spells a path (backslashes on Windows).
        let log = home().logs_dir().join("daemon.log").display().to_string();
        assert!(plist.contains(&log), "{log}\n{plist}");
        assert_eq!(
            p.commands,
            vec![vec![
                "launchctl".to_string(),
                "bootstrap".into(),
                "gui/501".into(),
                path.display().to_string()
            ]]
        );
        let u = plan_uninstall(Platform::MacOs, &root, 501);
        assert_eq!(
            u.commands[0],
            vec![
                "launchctl".to_string(),
                "bootout".into(),
                "gui/501/dev.alice.daemon".into()
            ]
        );
    }

    #[test]
    fn the_linux_plan_is_a_systemd_user_unit_and_windows_a_logon_task() {
        let root = PathBuf::from("/tmp/root");
        let p = plan_install(
            Platform::Linux,
            &root,
            Path::new("/usr/local/bin/alice"),
            &home(),
            1000,
        );
        assert_eq!(
            p.files[0].0,
            root.join(".config/systemd/user/alice.service")
        );
        assert!(
            p.files[0]
                .1
                .contains("ExecStart=/usr/local/bin/alice daemon run")
        );
        assert!(
            p.files[0]
                .1
                .contains("Environment=ALICE_HOME=/Users/u/.alice")
        );
        assert_eq!(
            p.commands.last().unwrap(),
            &vec![
                "systemctl".to_string(),
                "--user".into(),
                "enable".into(),
                "--now".into(),
                "alice.service".into()
            ]
        );
        assert!(p.notes.iter().any(|n| n.contains("loginctl enable-linger")));
        let w = plan_install(
            Platform::Windows,
            &root,
            Path::new(r"C:\alice\alice.exe"),
            &home(),
            0,
        );
        assert!(w.files.is_empty());
        assert_eq!(
            &w.commands[0][..3],
            &["schtasks".to_string(), "/Create".into(), "/SC".into()]
        );
        assert!(w.notes.iter().any(|n| n.contains("not yet verified")));
    }

    #[test]
    fn a_plan_carries_no_secret_and_quotes_paths_with_spaces() {
        let root = PathBuf::from("/tmp/root");
        let spaced = AliceHome::new(PathBuf::from("/Users/a b/.alice"));
        let p = plan_install(
            Platform::Linux,
            &root,
            Path::new("/opt/my tools/alice"),
            &spaced,
            1000,
        );
        let unit = &p.files[0].1;
        assert!(
            unit.contains("ExecStart=\"/opt/my tools/alice\" daemon run")
                && unit.contains("Environment=\"ALICE_HOME=/Users/a b/.alice\""),
            "{unit}"
        );
        // A `%` would start a systemd specifier.
        assert_eq!(systemd_word("/a%b"), "/a%%b");
        // The plan names the program and the home, and nothing else of the environment.
        for platform in [Platform::MacOs, Platform::Linux, Platform::Windows] {
            let p = plan_install(platform, &root, Path::new("/x/alice"), &home(), 1);
            let all = format!("{p:?}").to_ascii_lowercase();
            for word in ["token", "secret", "password", "@", "api_key"] {
                assert!(!all.contains(word), "{platform:?} mentions {word}: {all}");
            }
        }
    }

    #[test]
    fn apply_writes_files_and_runs_commands_only_with_a_runner() {
        struct Rec(parking_lot::Mutex<Vec<Vec<String>>>);
        impl Runner for Rec {
            fn run(&self, cmd: &[String]) -> alice_core::Result<()> {
                self.0.lock().push(cmd.to_vec());
                Ok(())
            }
        }
        let d = tempfile::tempdir().unwrap();
        let p = plan_install(
            Platform::Linux,
            d.path(),
            Path::new("/x/alice"),
            &home(),
            1000,
        );
        apply(&p, None).unwrap();
        assert!(installed(Platform::Linux, d.path()).is_some());
        let rec = Rec(Default::default());
        uninstall(Platform::Linux, d.path(), 1000, Some(&rec)).unwrap();
        assert!(installed(Platform::Linux, d.path()).is_none());
        assert!(
            rec.0
                .lock()
                .iter()
                .any(|c| c.contains(&"disable".to_string()))
        );
    }

    /// A runner that records what it is given and fails the commands whose text contains one of
    /// `fail`. Failures are in Thai on purpose: nothing may depend on the words. `xml` is what
    /// `schtasks /Query /XML` prints.
    struct Script {
        seen: parking_lot::Mutex<Vec<Vec<String>>>,
        fail: Vec<&'static str>,
        xml: String,
    }

    impl Script {
        fn new(fail: &[&'static str]) -> Self {
            Script {
                seen: Default::default(),
                fail: fail.to_vec(),
                xml: String::new(),
            }
        }
        fn with_xml(mut self, xml: &str) -> Self {
            self.xml = xml.into();
            self
        }
        /// The commands run, without the probes that only ask a question.
        fn actions(&self) -> Vec<String> {
            self.seen
                .lock()
                .iter()
                .map(|c| c.join(" "))
                .filter(|l| {
                    !["launchctl print", "is-active", "/Query"]
                        .iter()
                        .any(|p| l.contains(p))
                })
                .collect()
        }
    }

    impl Runner for Script {
        fn run(&self, cmd: &[String]) -> alice_core::Result<()> {
            self.seen.lock().push(cmd.to_vec());
            let line = cmd.join(" ");
            if self.fail.iter().any(|f| line.contains(f)) {
                return Err(AliceError::Other(format!("`{line}` ล้มเหลว")));
            }
            Ok(())
        }
        fn output(&self, cmd: &[String]) -> alice_core::Result<String> {
            self.run(cmd).map(|()| self.xml.clone())
        }
    }

    fn task_xml(home: &str) -> String {
        format!(
            "<Task><Actions><Exec><Command>cmd</Command><Arguments>/c set &quot;ALICE_HOME={home}&quot;&amp;&amp; &quot;C:\\alice.exe&quot; daemon run</Arguments></Exec></Actions></Task>"
        )
    }

    #[test]
    fn uninstall_returns_failures_of_what_is_there_and_skips_what_is_not() {
        let d = tempfile::tempdir().unwrap();
        let p = plan_install(
            Platform::Linux,
            d.path(),
            Path::new("/x/alice"),
            &home(),
            1000,
        );
        apply(&p, None).unwrap();
        let e = uninstall(
            Platform::Linux,
            d.path(),
            1000,
            Some(&Script::new(&["disable"])),
        )
        .unwrap_err();
        assert!(e.to_string().contains("ล้มเหลว"), "{e}");
        assert!(
            installed(Platform::Linux, d.path()).is_some(),
            "the unit file stays after a failure"
        );
        uninstall(Platform::Linux, d.path(), 1000, Some(&Script::new(&[]))).unwrap();
        assert!(installed(Platform::Linux, d.path()).is_none());
        // Nothing installed and a manager whose every command fails (in Thai): succeeds, runs nothing.
        let none = Script::new(&["systemctl", "launchctl", "schtasks"]);
        for platform in [Platform::Linux, Platform::MacOs, Platform::Windows] {
            uninstall(platform, d.path(), 1000, Some(&none)).unwrap();
            unload(platform, d.path(), 1000, &none).unwrap();
        }
        assert!(none.actions().is_empty(), "{:?}", none.actions());
    }

    /// `alice restore` stops the unit before the swap and starts it afterwards, with the commands
    /// of each platform; no unit, no commands.
    #[test]
    fn suspend_and_resume_stop_and_start_the_installed_unit_only() {
        let words = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        for (platform, stop, start) in [
            (
                Platform::MacOs,
                words(&["launchctl", "bootout", "gui/501/dev.alice.daemon"]),
                "bootstrap",
            ),
            (
                Platform::Linux,
                words(&["systemctl", "--user", "stop", "alice.service"]),
                "start",
            ),
        ] {
            let d = tempfile::tempdir().unwrap();
            let rec = Script::new(&[]);
            assert_eq!(
                suspend(platform, d.path(), 501, home().root(), &rec).unwrap(),
                Suspension::default(),
                "no unit file, nothing to stop"
            );
            assert!(rec.actions().is_empty(), "{platform:?}");
            apply(
                &plan_install(platform, d.path(), Path::new("/x/alice"), &home(), 501),
                None,
            )
            .unwrap();
            let s = suspend(platform, d.path(), 501, home().root(), &rec).unwrap();
            assert!(s.ended && s.notice.is_none());
            resume(platform, d.path(), 501, &rec).unwrap();
            let seen = rec.seen.lock().clone();
            let acts: Vec<_> = seen
                .iter()
                .filter(|c| !c.join(" ").contains("print") && !c.join(" ").contains("is-active"))
                .cloned()
                .collect();
            assert_eq!(acts[0], stop, "{platform:?}");
            assert!(acts[1].contains(&start.to_string()), "{acts:?}");
            assert!(
                installed(platform, d.path()).is_some(),
                "the unit stays installed"
            );
        }
    }

    /// A unit for another home, or one whose home cannot be read, is left alone with a notice.
    #[test]
    fn a_unit_for_another_home_or_an_unreadable_one_is_left_alone_with_a_notice() {
        for platform in [Platform::MacOs, Platform::Linux] {
            let d = tempfile::tempdir().unwrap();
            let (a, b) = (d.path().join("home-a"), d.path().join("home-b"));
            std::fs::create_dir_all(&a).unwrap();
            std::fs::create_dir_all(&b).unwrap();
            apply(
                &plan_install(
                    platform,
                    d.path(),
                    Path::new("/x/alice"),
                    &AliceHome::new(a.clone()),
                    501,
                ),
                None,
            )
            .unwrap();
            assert_eq!(
                unit_home(platform, d.path()),
                Some(a.clone()),
                "{platform:?}"
            );
            let rec = Script::new(&[]);
            let s = suspend(platform, d.path(), 501, &b, &rec).unwrap();
            assert!(!s.ended);
            let n = s.notice.unwrap();
            assert!(
                n.contains("serves") && n.contains("left alone") && n.contains("home-a"),
                "{n}"
            );
            assert!(rec.actions().is_empty(), "{platform:?}: nothing stopped");
            // The same home spelled through a link still matches.
            #[cfg(unix)]
            {
                let link = d.path().join("link-a");
                std::os::unix::fs::symlink(&a, &link).unwrap();
                assert!(unit_serves(platform, d.path(), &link));
            }
            assert!(suspend(platform, d.path(), 501, &a, &rec).unwrap().ended);
            // A unit file that names no data directory.
            let path = installed(platform, d.path()).unwrap();
            std::fs::write(&path, "hand edited\n").unwrap();
            let rec = Script::new(&[]);
            let s = suspend(platform, d.path(), 501, &a, &rec).unwrap();
            assert!(!s.ended && s.notice.unwrap().contains("could not be read"));
            assert!(rec.actions().is_empty());
        }
        // A path with a space, a `%` and a `&` survives the unit file (quoting and escaping).
        let d = tempfile::tempdir().unwrap();
        let odd = PathBuf::from("/tmp/a b%c&d");
        for platform in [Platform::MacOs, Platform::Linux] {
            apply(
                &plan_install(
                    platform,
                    d.path(),
                    Path::new("/x/alice"),
                    &AliceHome::new(odd.clone()),
                    1,
                ),
                None,
            )
            .unwrap();
            assert_eq!(
                unit_home(platform, d.path()),
                Some(odd.clone()),
                "{platform:?}"
            );
        }
    }

    /// One rule on macOS and Linux: not loaded proceeds without a stop; loaded and a stop that
    /// fails (in Thai) refuses.
    #[test]
    fn a_loaded_unit_that_cannot_be_stopped_refuses_and_an_unloaded_one_proceeds() {
        for (platform, probe, stop) in [
            (Platform::MacOs, "launchctl print", "bootout"),
            (Platform::Linux, "is-active", "--user stop"),
        ] {
            let d = tempfile::tempdir().unwrap();
            apply(
                &plan_install(platform, d.path(), Path::new("/x/alice"), &home(), 501),
                None,
            )
            .unwrap();
            let not_loaded = Script::new(&[probe]);
            assert_eq!(
                suspend(platform, d.path(), 501, home().root(), &not_loaded).unwrap(),
                Suspension::default()
            );
            assert!(
                not_loaded.actions().is_empty(),
                "no stop for an unloaded unit"
            );
            let stuck = Script::new(&[stop]);
            let e = suspend(platform, d.path(), 501, home().root(), &stuck).unwrap_err();
            assert!(e.to_string().contains("ล้มเหลว"), "{e}");
        }
    }

    /// Windows: the home is read from the task definition; another home's task is left alone; a
    /// task that cannot be read is left alone; `/End` is best effort and never refuses.
    #[test]
    fn the_windows_task_is_found_by_its_definition_and_ended_on_a_best_effort() {
        let root = Path::new("/x");
        let mine = "C:\\Users\\u\\.alice";
        let h = Path::new(mine);
        assert_eq!(task_home(&task_xml(mine)), Some(PathBuf::from(mine)));
        // Absent: `/Query` fails, nothing else runs.
        let rec = Script::new(&["/Query"]);
        assert_eq!(
            suspend(Platform::Windows, root, 0, h, &rec).unwrap(),
            Suspension::default()
        );
        assert!(rec.actions().is_empty());
        // Another home's task: nothing ended, a notice.
        let rec = Script::new(&[]).with_xml(&task_xml("D:\\other"));
        let s = suspend(Platform::Windows, root, 0, h, &rec).unwrap();
        assert!(!s.ended && s.notice.unwrap().contains("D:\\other"));
        assert!(rec.actions().is_empty());
        // A definition that names no home: left alone, with a notice.
        let rec = Script::new(&[]).with_xml("<Task/>");
        let s = suspend(Platform::Windows, root, 0, h, &rec).unwrap();
        assert!(!s.ended && s.notice.unwrap().contains("could not be read"));
        // This home's task: ended.
        let rec = Script::new(&[]).with_xml(&task_xml(mine));
        let s = suspend(Platform::Windows, root, 0, h, &rec).unwrap();
        assert!(s.ended);
        assert!(rec.actions()[0].contains("/End"));
        // `/End` fails (not running, any language): proceeds, nothing to resume.
        let rec = Script::new(&["/End"]).with_xml(&task_xml(mine));
        assert_eq!(
            suspend(Platform::Windows, root, 0, h, &rec).unwrap(),
            Suspension::default()
        );
        assert_eq!(TASK, "dev.alice.daemon");
    }

    /// Where systemd reads user units: `$XDG_CONFIG_HOME`, only for the user's real home.
    #[test]
    fn linux_units_follow_xdg_config_home_for_the_real_home_only() {
        let home = Path::new("/home/u");
        let xdg = std::ffi::OsString::from("/home/u/cfg");
        assert_eq!(
            linux_unit_dir(home, Some(&xdg), Some(home)),
            PathBuf::from("/home/u/cfg/systemd/user")
        );
        assert_eq!(
            linux_unit_dir(home, None, Some(home)),
            PathBuf::from("/home/u/.config/systemd/user")
        );
        let empty = std::ffi::OsString::new();
        assert_eq!(
            linux_unit_dir(home, Some(&empty), Some(home)),
            PathBuf::from("/home/u/.config/systemd/user")
        );
        // A sandbox or `--target-dir` root never follows the real environment.
        assert_eq!(
            linux_unit_dir(Path::new("/tmp/t"), Some(&xdg), Some(home)),
            PathBuf::from("/tmp/t/.config/systemd/user")
        );
    }

    /// The unit options that keep a daemon that cannot start from looping every few seconds,
    /// and keep launchd from throttling it.
    #[test]
    fn the_units_restart_slowly_and_launchd_does_not_throttle_the_daemon() {
        let root = PathBuf::from("/tmp/root");
        let mac = plan_install(Platform::MacOs, &root, Path::new("/x/alice"), &home(), 1);
        let plist = &mac.files[0].1;
        assert!(!plist.contains("ProcessType"), "{plist}");
        assert!(plist.contains("<key>ThrottleInterval</key><integer>30</integer>"));
        assert!(plist.contains("<key>SuccessfulExit</key><false/>"));
        let linux = plan_install(Platform::Linux, &root, Path::new("/x/alice"), &home(), 1);
        let unit = &linux.files[0].1;
        for want in [
            "Restart=on-failure",
            "RestartSec=30",
            "StartLimitIntervalSec=600",
            "StartLimitBurst=5",
        ] {
            assert!(unit.contains(want), "{want}\n{unit}");
        }
    }
}
