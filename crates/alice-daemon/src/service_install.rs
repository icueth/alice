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

/// What a command printed and how it ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Exec {
    /// The exit code (`None`: ended by a signal).
    pub code: Option<i32>,
    /// Raw stdout (a Windows tool may print UTF-16).
    pub stdout: Vec<u8>,
}

impl Exec {
    pub fn ok(stdout: &str) -> Self {
        Exec {
            code: Some(0),
            stdout: stdout.as_bytes().to_vec(),
        }
    }
    pub fn code(code: i32) -> Self {
        Exec {
            code: Some(code),
            stdout: Vec::new(),
        }
    }
}

pub trait Runner {
    /// Run a command to its end. `Err` only when it could not be run at all.
    fn exec(&self, cmd: &[String]) -> Result<Exec>;
    /// Run a command; `Ok` is exit status 0. Nothing reads the error text (it is localised).
    fn run(&self, cmd: &[String]) -> Result<()> {
        let e = self.exec(cmd)?;
        if e.code == Some(0) {
            Ok(())
        } else {
            Err(AliceError::Other(format!(
                "`{}` exited with {}",
                cmd.join(" "),
                e.code.map_or("a signal".to_string(), |c| c.to_string())
            )))
        }
    }
}

pub struct SystemRunner;

impl Runner for SystemRunner {
    fn exec(&self, cmd: &[String]) -> Result<Exec> {
        let Some((program, args)) = cmd.split_first() else {
            return Err(AliceError::Other("an empty command".into()));
        };
        let out = std::process::Command::new(program).args(args).output()?;
        Ok(Exec {
            code: out.status.code(),
            stdout: out.stdout,
        })
    }
}

/// Text printed by a tool: UTF-16LE when it starts with a byte order mark or holds NUL bytes (the
/// Task Scheduler's XML may), UTF-8 otherwise.
pub fn decode_text(bytes: &[u8]) -> String {
    if bytes.starts_with(&[0xFF, 0xFE]) || bytes.contains(&0) {
        let body = bytes.strip_prefix(&[0xFF, 0xFE]).unwrap_or(bytes);
        let units: Vec<u16> = body
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| u16::from_le_bytes(*c))
            .collect();
        String::from_utf16_lossy(&units)
    } else {
        String::from_utf8_lossy(bytes).into_owned()
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
    // A relative value is ignored, as the XDG specification says.
    match xdg.filter(|x| Path::new(x).is_absolute()) {
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

/// What the service manager says about the unit. Three answers: asking can fail, and a failure
/// to ask is not "not running".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UnitState {
    /// Loaded or active, or waiting to be restarted: the manager may start the daemon.
    Running,
    /// The manager has it stopped (or does not have it) and will not start it.
    NotRunning,
    /// The manager could not be asked, or the answer was not understood (why).
    Unknown(String),
}

/// launchd's exit code for "Could not find specified service" (`launchctl error 113`).
const LAUNCHD_NOT_FOUND: i32 = 113;

/// Ask the manager about the unit, by exit codes and the manager's fixed state identifiers
/// (never its messages, which are localised). Not used for Windows, whose task has no restart
/// policy.
///
/// - launchd `print`: exit 0 is loaded (and `KeepAlive` can restart it), 113 is not found,
///   anything else is unknown.
/// - systemd `show -p ActiveState -p SubState --value`: a non-zero exit or output that names no
///   state is unknown; `active`, `activating`, `reloading`, `deactivating` or a `SubState` of
///   `auto-restart` is running; `inactive` or `failed` otherwise is not.
pub fn unit_state(p: Platform, uid: u32, r: &dyn Runner) -> UnitState {
    let unknown = |why: String| UnitState::Unknown(why);
    match p {
        Platform::MacOs => {
            let cmd = [
                "launchctl".to_string(),
                "print".into(),
                format!("gui/{uid}/{LABEL}"),
            ];
            match r.exec(&cmd) {
                Err(e) => unknown(e.to_string()),
                Ok(e) => match e.code {
                    Some(0) => UnitState::Running,
                    Some(LAUNCHD_NOT_FOUND) => UnitState::NotRunning,
                    other => unknown(format!(
                        "`launchctl print` exited with {}",
                        other.map_or("a signal".to_string(), |c| c.to_string())
                    )),
                },
            }
        }
        Platform::Linux => {
            let cmd = [
                "systemctl",
                "--user",
                "show",
                "-p",
                "ActiveState",
                "-p",
                "SubState",
                "--value",
                UNIT,
            ]
            .map(String::from);
            let e = match r.exec(&cmd) {
                Err(e) => return unknown(e.to_string()),
                Ok(e) => e,
            };
            if e.code != Some(0) {
                return unknown(format!(
                    "`systemctl --user show` exited with {}",
                    e.code.map_or("a signal".to_string(), |c| c.to_string())
                ));
            }
            let text = decode_text(&e.stdout);
            let words: Vec<&str> = text.split_whitespace().collect();
            let active = words.iter().find(|w| {
                [
                    "active",
                    "activating",
                    "reloading",
                    "deactivating",
                    "inactive",
                    "failed",
                ]
                .contains(*w)
            });
            match active {
                None => unknown("systemctl named no state".into()),
                Some(&state) => {
                    if words.contains(&"auto-restart")
                        || ["active", "activating", "reloading", "deactivating"].contains(&state)
                    {
                        UnitState::Running
                    } else {
                        UnitState::NotRunning
                    }
                }
            }
        }
        Platform::Windows => {
            let cmd = [
                "schtasks".to_string(),
                "/Query".into(),
                "/TN".into(),
                TASK.into(),
            ];
            match r.exec(&cmd) {
                Ok(e) if e.code == Some(0) => UnitState::Running,
                _ => UnitState::NotRunning,
            }
        }
    }
}

/// The command that stops the unit by hand, for a message.
fn stop_by_hand(p: Platform, uid: u32) -> String {
    plan_suspend(p, uid).commands[0].join(" ")
}

fn cannot_ask(p: Platform, uid: u32, why: &str) -> AliceError {
    AliceError::Other(format!(
        "the service manager could not be asked whether the unit runs ({why}); stop the unit by hand with `{}` and retry",
        stop_by_hand(p, uid)
    ))
}

/// Uninstall: stop and unregister what the manager has (when a runner is given), then remove the
/// unit file. What the manager says is asked by exit codes ([`unit_state`]): a running unit is
/// stopped (a failure is returned), a stopped one only has its file removed, and when the manager
/// cannot be asked nothing is removed and the error says how to stop the unit by hand. The unit
/// file stays after any failure, so `alice service status` still shows it and a retry can finish.
pub fn uninstall(p: Platform, root: &Path, uid: u32, runner: Option<&dyn Runner>) -> Result<()> {
    if let Some(r) = runner {
        match p {
            Platform::Windows => {
                if unit_state(p, uid, r) == UnitState::Running {
                    for c in &plan_uninstall(p, root, uid).commands {
                        r.run(c)?;
                    }
                }
            }
            _ => match unit_state(p, uid, r) {
                UnitState::Unknown(why) => return Err(cannot_ask(p, uid, &why)),
                UnitState::Running => {
                    for c in &plan_uninstall(p, root, uid).commands {
                        r.run(c)?;
                    }
                }
                UnitState::NotRunning => {
                    // Not running: only the registration is undone (best effort).
                    if p == Platform::Linux && installed(p, root).is_some() {
                        for c in [
                            ["systemctl", "--user", "disable", UNIT]
                                .map(String::from)
                                .to_vec(),
                            ["systemctl", "--user", "daemon-reload"]
                                .map(String::from)
                                .to_vec(),
                        ] {
                            let _ = r.run(&c);
                        }
                    }
                }
            },
        }
    }
    if let Some(f) = installed(p, root) {
        std::fs::remove_file(f)?;
    }
    Ok(())
}

/// Unload a unit that is already loaded, so installing again replaces it (launchd refuses to
/// bootstrap a job that is loaded). Not running is fine; a manager that cannot be asked is an
/// error.
pub fn unload(p: Platform, root: &Path, uid: u32, runner: &dyn Runner) -> Result<()> {
    // Only launchd needs this: systemd's `enable --now` and the task's `/F` replace the old one.
    if p == Platform::MacOs {
        match unit_state(p, uid, runner) {
            UnitState::Unknown(why) => return Err(cannot_ask(p, uid, &why)),
            UnitState::Running => {
                for c in &plan_uninstall(p, root, uid).commands {
                    runner.run(c)?;
                }
            }
            UnitState::NotRunning => {}
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
        return match runner.exec(&query) {
            Ok(e) if e.code == Some(0) => owner_of(task_home(&decode_text(&e.stdout)), home),
            _ => UnitOwner::None,
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
/// 2. Ask the manager ([`unit_state`]): launchd and systemd restart what exits, so a unit that
///    is running, or waiting to restart, is stopped; if the stop fails, or the manager cannot be
///    asked, the restore must not go on (it could restart the daemon in the middle of the
///    swap): `Err`. A unit the manager has stopped is left stopped: nothing to suspend, and
///    [`resume`] never starts it.
/// 3. (Stopped, above.)
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
    if p == Platform::Windows {
        // Best effort: a failure never refuses (the restore stops the daemon process itself).
        for c in &plan_suspend(p, uid).commands {
            if let Err(e) = runner.run(c) {
                tracing::info!(error = %e, "the scheduled task could not be ended");
                return Ok(Suspension::default());
            }
        }
        return Ok(Suspension {
            ended: true,
            notice: None,
        });
    }
    match unit_state(p, uid, runner) {
        UnitState::NotRunning => Ok(Suspension::default()),
        UnitState::Unknown(why) => Err(cannot_ask(p, uid, &why)),
        UnitState::Running => {
            for c in &plan_suspend(p, uid).commands {
                runner.run(c)?;
            }
            Ok(Suspension {
                ended: true,
                notice: None,
            })
        }
    }
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
        let rec = Script::new(&[]).with("show -p", Exec::ok("active\nrunning\n"));
        uninstall(Platform::Linux, d.path(), 1000, Some(&rec)).unwrap();
        assert!(installed(Platform::Linux, d.path()).is_none());
        assert!(rec.actions().iter().any(|c| c.contains("disable")));
        // With a runner, `apply` runs the plan's commands in order.
        let rec = Script::new(&[]);
        apply(&p, Some(&rec)).unwrap();
        assert_eq!(rec.actions().len(), 2);
    }

    /// A runner that records what it is given and answers from `rules`: the first whose text is
    /// contained in the command gives the exit code and stdout (default: exit 0, nothing).
    /// Failure text would be localised; here there is none, only codes.
    struct Script {
        seen: parking_lot::Mutex<Vec<Vec<String>>>,
        rules: Vec<(&'static str, Exec)>,
    }

    impl Script {
        fn new(fail: &[&'static str]) -> Self {
            Script {
                seen: Default::default(),
                rules: fail.iter().map(|f| (*f, Exec::code(1))).collect(),
            }
        }
        fn with(mut self, text: &'static str, answer: Exec) -> Self {
            self.rules.insert(0, (text, answer));
            self
        }
        /// The commands run, without the probes that only ask a question.
        fn actions(&self) -> Vec<String> {
            self.seen
                .lock()
                .iter()
                .map(|c| c.join(" "))
                .filter(|l| {
                    !["launchctl print", "show -p", "/Query"]
                        .iter()
                        .any(|p| l.contains(p))
                })
                .collect()
        }
    }

    impl Runner for Script {
        fn exec(&self, cmd: &[String]) -> alice_core::Result<Exec> {
            self.seen.lock().push(cmd.to_vec());
            let line = cmd.join(" ");
            Ok(self
                .rules
                .iter()
                .find(|(t, _)| line.contains(t))
                .map(|(_, a)| a.clone())
                .unwrap_or_else(|| Exec::code(0)))
        }
    }

    /// The probe answers of a unit the manager has running / stopped, per platform.
    fn running(p: Platform) -> (&'static str, Exec) {
        match p {
            Platform::MacOs => ("launchctl print", Exec::ok("service = dev.alice.daemon")),
            _ => ("show -p", Exec::ok("active\nrunning\n")),
        }
    }
    fn stopped(p: Platform) -> (&'static str, Exec) {
        match p {
            Platform::MacOs => ("launchctl print", Exec::code(113)),
            _ => ("show -p", Exec::ok("inactive\ndead\n")),
        }
    }

    fn task_xml(home: &str) -> String {
        format!(
            "<Task><Actions><Exec><Command>cmd</Command><Arguments>/c set &quot;ALICE_HOME={home}&quot;&amp;&amp; &quot;C:\\alice.exe&quot; daemon run</Arguments></Exec></Actions></Task>"
        )
    }

    fn install_unit(p: Platform, root: &Path, h: &AliceHome) {
        apply(&plan_install(p, root, Path::new("/x/alice"), h, 501), None).unwrap();
    }

    /// The three answers, read from exit codes and the manager's fixed state words.
    #[test]
    fn the_probe_has_three_answers_from_codes_and_state_identifiers() {
        let linux = |code: i32, out: &str| {
            let r = Script::new(&[]).with(
                "show -p",
                Exec {
                    code: Some(code),
                    stdout: out.as_bytes().to_vec(),
                },
            );
            unit_state(Platform::Linux, 501, &r)
        };
        assert_eq!(linux(0, "active\nrunning\n"), UnitState::Running);
        assert_eq!(
            linux(0, "activating\nauto-restart\n"),
            UnitState::Running,
            "restart backoff"
        );
        assert_eq!(
            linux(0, "auto-restart\nactivating\n"),
            UnitState::Running,
            "either order"
        );
        assert_eq!(linux(0, "deactivating\nstop-sigterm\n"), UnitState::Running);
        assert_eq!(linux(0, "reloading\nreload\n"), UnitState::Running);
        assert_eq!(linux(0, "failed\nfailed\n"), UnitState::NotRunning);
        assert_eq!(linux(0, "inactive\ndead\n"), UnitState::NotRunning);
        assert!(matches!(linux(1, ""), UnitState::Unknown(_)), "no user bus");
        assert!(
            matches!(linux(3, "inactive\ndead\n"), UnitState::Unknown(_)),
            "non-zero is not an answer"
        );
        assert!(
            matches!(linux(0, ""), UnitState::Unknown(_)),
            "no state named"
        );
        assert!(matches!(linux(0, "สถานะ\n"), UnitState::Unknown(_)));
        let mac = |code: Option<i32>| {
            let r = Script::new(&[]).with(
                "launchctl print",
                Exec {
                    code,
                    stdout: Vec::new(),
                },
            );
            unit_state(Platform::MacOs, 501, &r)
        };
        assert_eq!(mac(Some(0)), UnitState::Running);
        assert_eq!(mac(Some(113)), UnitState::NotRunning, "service not found");
        assert!(
            matches!(mac(Some(5)), UnitState::Unknown(_)),
            "no gui domain, say"
        );
        assert!(matches!(mac(None), UnitState::Unknown(_)));
    }

    #[test]
    fn uninstall_follows_the_probe_and_removes_nothing_when_it_cannot_ask() {
        for platform in [Platform::Linux, Platform::MacOs] {
            let (probe, run_answer) = running(platform);
            let d = tempfile::tempdir().unwrap();
            install_unit(platform, d.path(), &home());
            // The manager cannot be asked: refused, the file stays, no command but the probe.
            let cannot = Script::new(&[]).with(probe, Exec::code(1));
            let e = uninstall(platform, d.path(), 501, Some(&cannot)).unwrap_err();
            assert!(e.to_string().contains("by hand"), "{e}");
            assert!(installed(platform, d.path()).is_some());
            assert!(cannot.actions().is_empty());
            // Running: stopped (a failing stop is returned and the file stays), then removed.
            let stuck = Script::new(&[match platform {
                Platform::MacOs => "bootout",
                _ => "disable --now",
            }])
            .with(probe, run_answer.clone());
            assert!(uninstall(platform, d.path(), 501, Some(&stuck)).is_err());
            assert!(
                installed(platform, d.path()).is_some(),
                "the file stays after a failed stop"
            );
            let ok = Script::new(&[]).with(probe, run_answer);
            uninstall(platform, d.path(), 501, Some(&ok)).unwrap();
            assert!(installed(platform, d.path()).is_none());
            assert!(!ok.actions().is_empty());
            // Stopped (or not found): the file goes, nothing is stopped.
            install_unit(platform, d.path(), &home());
            let (probe, off) = stopped(platform);
            let idle = Script::new(&[]).with(probe, off);
            uninstall(platform, d.path(), 501, Some(&idle)).unwrap();
            assert!(installed(platform, d.path()).is_none());
            assert!(
                !idle
                    .actions()
                    .iter()
                    .any(|a| a.contains("bootout") || a.contains("--now")),
                "{:?}",
                idle.actions()
            );
        }
        // Linux: a unit that is running but has no file is still disabled.
        let d = tempfile::tempdir().unwrap();
        let (probe, on) = running(Platform::Linux);
        let r = Script::new(&[]).with(probe, on);
        uninstall(Platform::Linux, d.path(), 501, Some(&r)).unwrap();
        assert!(r.actions().iter().any(|a| a.contains("disable --now")));
        // Windows: the task is deleted when it exists, and nothing runs when it does not.
        let none = Script::new(&["/Query"]);
        uninstall(Platform::Windows, d.path(), 0, Some(&none)).unwrap();
        assert!(none.actions().is_empty());
        let there = Script::new(&[]);
        uninstall(Platform::Windows, d.path(), 0, Some(&there)).unwrap();
        assert!(there.actions()[0].contains("/Delete"));
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
            let (probe, on) = running(platform);
            let rec = Script::new(&[]).with(probe, on);
            assert_eq!(
                suspend(platform, d.path(), 501, home().root(), &rec).unwrap(),
                Suspension::default(),
                "no unit file, nothing to stop"
            );
            assert!(rec.actions().is_empty(), "{platform:?}");
            install_unit(platform, d.path(), &home());
            let s = suspend(platform, d.path(), 501, home().root(), &rec).unwrap();
            assert!(s.ended && s.notice.is_none());
            resume(platform, d.path(), 501, &rec).unwrap();
            let acts = rec.actions();
            assert_eq!(acts[0], stop.join(" "), "{platform:?}");
            assert!(acts[1].contains(start), "{acts:?}");
            assert!(
                installed(platform, d.path()).is_some(),
                "the unit stays installed"
            );
        }
    }

    /// systemd's restart backoff is running; a stopped unit is never stopped, and never started
    /// afterwards; a manager that cannot be asked refuses, with nothing run.
    #[test]
    fn suspend_follows_the_three_answers_on_systemd_and_launchd() {
        // auto-restart: stopped, and resumed.
        let d = tempfile::tempdir().unwrap();
        install_unit(Platform::Linux, d.path(), &home());
        let r = Script::new(&[]).with("show -p", Exec::ok("activating\nauto-restart\n"));
        assert!(
            suspend(Platform::Linux, d.path(), 501, home().root(), &r)
                .unwrap()
                .ended
        );
        assert!(r.actions()[0].contains("--user stop"));
        // inactive / not found: no stop, nothing to start afterwards.
        for platform in [Platform::Linux, Platform::MacOs] {
            let d = tempfile::tempdir().unwrap();
            install_unit(platform, d.path(), &home());
            let (probe, off) = stopped(platform);
            let r = Script::new(&[]).with(probe, off);
            assert_eq!(
                suspend(platform, d.path(), 501, home().root(), &r).unwrap(),
                Suspension::default(),
                "{platform:?}"
            );
            assert!(r.actions().is_empty(), "{platform:?}");
        }
        // The manager cannot be asked (no user bus, no gui domain, another launchctl error): refused.
        for (platform, probe, answer) in [
            (Platform::Linux, "show -p", Exec::code(1)),
            (Platform::MacOs, "launchctl print", Exec::code(5)),
            (Platform::MacOs, "launchctl print", Exec::code(1)),
        ] {
            let d = tempfile::tempdir().unwrap();
            install_unit(platform, d.path(), &home());
            let r = Script::new(&[]).with(probe, answer);
            let e = suspend(platform, d.path(), 501, home().root(), &r).unwrap_err();
            assert!(
                e.to_string().contains("by hand") && e.to_string().contains("stop"),
                "{e}"
            );
            assert!(r.actions().is_empty(), "nothing ran: {:?}", r.actions());
        }
        // Running and the stop fails: refused.
        for platform in [Platform::Linux, Platform::MacOs] {
            let d = tempfile::tempdir().unwrap();
            install_unit(platform, d.path(), &home());
            let (probe, on) = running(platform);
            let stop = if platform == Platform::MacOs {
                "bootout"
            } else {
                "--user stop"
            };
            let r = Script::new(&[stop]).with(probe, on);
            assert!(suspend(platform, d.path(), 501, home().root(), &r).is_err());
        }
    }

    /// A unit for another home, or one whose home cannot be read, is left alone with a notice.
    #[test]
    fn a_unit_for_another_home_or_an_unreadable_one_is_left_alone_with_a_notice() {
        for platform in [Platform::MacOs, Platform::Linux] {
            let (probe, on) = running(platform);
            let d = tempfile::tempdir().unwrap();
            let (a, b) = (d.path().join("home-a"), d.path().join("home-b"));
            std::fs::create_dir_all(&a).unwrap();
            std::fs::create_dir_all(&b).unwrap();
            install_unit(platform, d.path(), &AliceHome::new(a.clone()));
            assert_eq!(
                unit_home(platform, d.path()),
                Some(a.clone()),
                "{platform:?}"
            );
            let rec = Script::new(&[]).with(probe, on.clone());
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
            let rec = Script::new(&[]).with(probe, on);
            let s = suspend(platform, d.path(), 501, &a, &rec).unwrap();
            assert!(!s.ended && s.notice.unwrap().contains("could not be read"));
            assert!(rec.actions().is_empty());
        }
        // A path with a space, a `%` and a `&` survives the unit file (quoting and escaping).
        let d = tempfile::tempdir().unwrap();
        let odd = PathBuf::from("/tmp/a b%c&d");
        for platform in [Platform::MacOs, Platform::Linux] {
            install_unit(platform, d.path(), &AliceHome::new(odd.clone()));
            assert_eq!(
                unit_home(platform, d.path()),
                Some(odd.clone()),
                "{platform:?}"
            );
        }
    }

    /// Windows: the home is read from the task definition (UTF-8 or the UTF-16LE the Task
    /// Scheduler may print, with a Thai path); another home's task is left alone; a definition
    /// that names no home is left alone; `/End` is best effort and never refuses.
    #[test]
    fn the_windows_task_is_found_by_its_definition_and_ended_on_a_best_effort() {
        let root = Path::new("/x");
        let mine = "C:\\Users\\u\\.alice";
        let h = Path::new(mine);
        let xml = |home: &str| Exec::ok(&task_xml(home));
        assert_eq!(task_home(&task_xml(mine)), Some(PathBuf::from(mine)));
        // Absent: `/Query` fails, nothing else runs.
        let rec = Script::new(&["/Query"]);
        assert_eq!(
            suspend(Platform::Windows, root, 0, h, &rec).unwrap(),
            Suspension::default()
        );
        assert!(rec.actions().is_empty());
        // Another home's task: nothing ended, a notice.
        let rec = Script::new(&[]).with("/XML", xml("D:\\other"));
        let s = suspend(Platform::Windows, root, 0, h, &rec).unwrap();
        assert!(!s.ended && s.notice.unwrap().contains("D:\\other"));
        assert!(rec.actions().is_empty());
        // A definition that names no home: left alone, with a notice.
        let rec = Script::new(&[]).with("/XML", Exec::ok("<Task/>"));
        let s = suspend(Platform::Windows, root, 0, h, &rec).unwrap();
        assert!(!s.ended && s.notice.unwrap().contains("could not be read"));
        // This home's task: ended.
        let rec = Script::new(&[]).with("/XML", xml(mine));
        let s = suspend(Platform::Windows, root, 0, h, &rec).unwrap();
        assert!(s.ended);
        assert!(rec.actions()[0].contains("/End"));
        // `/End` fails (not running, any language): proceeds, nothing to resume.
        let rec = Script::new(&["/End"]).with("/XML", xml(mine));
        assert_eq!(
            suspend(Platform::Windows, root, 0, h, &rec).unwrap(),
            Suspension::default()
        );
        assert_eq!(TASK, "dev.alice.daemon");
        // UTF-16LE with a byte order mark and a Thai user name.
        let thai = "C:\\Users\\ผู้ใช้\\.alice";
        let mut bytes = vec![0xFF, 0xFE];
        bytes.extend(task_xml(thai).encode_utf16().flat_map(|u| u.to_le_bytes()));
        assert_eq!(decode_text(&bytes), task_xml(thai));
        let rec = Script::new(&[]).with(
            "/XML",
            Exec {
                code: Some(0),
                stdout: bytes,
            },
        );
        let s = suspend(Platform::Windows, root, 0, Path::new(thai), &rec).unwrap();
        assert!(s.ended, "{s:?}");
        // Without a byte order mark the NUL bytes still say UTF-16.
        let bare: Vec<u8> = task_xml(thai)
            .encode_utf16()
            .flat_map(|u| u.to_le_bytes())
            .collect();
        assert_eq!(decode_text(&bare), task_xml(thai));
        assert_eq!(decode_text("สวัสดี".as_bytes()), "สวัสดี");
    }

    /// Where systemd reads user units: `$XDG_CONFIG_HOME`, only for the user's real home.
    #[test]
    fn linux_units_follow_xdg_config_home_for_the_real_home_only() {
        // Absolute on every OS (a path with no drive is relative on Windows).
        let base = std::env::temp_dir();
        let home = base.join("u");
        let cfg = home.join("cfg");
        let xdg = cfg.clone().into_os_string();
        let dot = home.join(".config/systemd/user");
        assert_eq!(
            linux_unit_dir(&home, Some(&xdg), Some(&home)),
            cfg.join("systemd/user")
        );
        assert_eq!(linux_unit_dir(&home, None, Some(&home)), dot);
        let relative = std::ffi::OsString::from("cfg");
        assert_eq!(
            linux_unit_dir(&home, Some(&relative), Some(&home)),
            dot,
            "a relative XDG_CONFIG_HOME is ignored"
        );
        let empty = std::ffi::OsString::new();
        assert_eq!(linux_unit_dir(&home, Some(&empty), Some(&home)), dot);
        // A sandbox or `--target-dir` root never follows the real environment.
        let sandbox = base.join("t");
        assert_eq!(
            linux_unit_dir(&sandbox, Some(&xdg), Some(&home)),
            sandbox.join(".config/systemd/user")
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
