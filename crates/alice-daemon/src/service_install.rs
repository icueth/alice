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
/// The scheduled task's name on Windows.
pub const TASK: &str = "Alice";

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
    fn run(&self, cmd: &[String]) -> Result<()>;
}

pub struct SystemRunner;

impl Runner for SystemRunner {
    fn run(&self, cmd: &[String]) -> Result<()> {
        let Some((program, args)) = cmd.split_first() else {
            return Ok(());
        };
        let out = std::process::Command::new(program).args(args).output()?;
        if !out.status.success() {
            // stderr carries the reason ("No such process", "Unit … not loaded", …); `uninstall`
            // tells "not installed" from real failures by it.
            let why = String::from_utf8_lossy(&out.stderr).trim().to_string();
            return Err(AliceError::Other(format!(
                "`{}` exited with {}: {why}",
                cmd.join(" "),
                out.status
            )));
        }
        Ok(())
    }
}

/// Whether a failed stop/unregister command only means the service was not loaded.
fn not_installed(e: &AliceError) -> bool {
    let m = e.to_string().to_ascii_lowercase();
    [
        "no such process",
        "does not exist",
        "cannot find",
        "not loaded",
        "not found",
        "could not find",
    ]
    .iter()
    .any(|p| m.contains(p))
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
        Platform::Linux => Some(root.join(".config/systemd/user").join(UNIT)),
        Platform::Windows => None,
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
  <key>ProcessType</key><string>Background</string>
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
                "[Unit]\nDescription=Alice memory daemon\n\n[Service]\nExecStart={} daemon run\nEnvironment={}\nStandardOutput=append:{}\nStandardError=append:{}\nRestart=on-failure\nRestartSec=5\n\n[Install]\nWantedBy=default.target\n",
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

/// Run `commands`, treating a service that is not loaded as done. Returns whether every command
/// ran (`false`: the service was not there, and nothing more was run).
fn run_tolerant(commands: &[Vec<String>], r: &dyn Runner) -> Result<bool> {
    for c in commands {
        match r.run(c) {
            Ok(()) => {}
            Err(e) if not_installed(&e) => {
                tracing::info!(error = %e, "service was not loaded");
                return Ok(false);
            }
            Err(e) => return Err(e),
        }
    }
    Ok(true)
}

/// Uninstall: stop and unregister (when a runner is given), then remove the unit file. A
/// service that is not loaded is not an error; any other command failure is returned (the unit
/// file stays, so `alice service status` still shows it and a retry can finish the job).
pub fn uninstall(p: Platform, root: &Path, uid: u32, runner: Option<&dyn Runner>) -> Result<()> {
    if let Some(r) = runner {
        run_tolerant(&plan_uninstall(p, root, uid).commands, r)?;
    }
    if let Some(f) = installed(p, root) {
        std::fs::remove_file(f)?;
    }
    Ok(())
}

/// Unload a unit that is already registered, so installing again replaces it (launchd refuses
/// to bootstrap a job that is loaded). A unit that is not loaded is fine.
pub fn unload(p: Platform, root: &Path, uid: u32, runner: &dyn Runner) -> Result<()> {
    // Only launchd needs this: systemd's `enable --now` and the task's `/F` replace the old one.
    if p == Platform::MacOs {
        run_tolerant(&plan_uninstall(p, root, uid).commands, runner)?;
    }
    Ok(())
}

/// Stop the installed unit's daemon for the duration of a restore. `Ok(true)`: it was running
/// and [`resume`] should start it again; `Ok(false)`: no unit (or not loaded), nothing to
/// resume. A command that fails for another reason is an error: the restore must not go on
/// with a unit that may start a daemon in the middle of the swap.
pub fn suspend(p: Platform, root: &Path, uid: u32, runner: &dyn Runner) -> Result<bool> {
    // Where a unit is a file, its absence means there is nothing to stop; on Windows the task
    // lives in the Task Scheduler, and `/End` says whether it exists.
    if unit_path(p, root).is_some() && installed(p, root).is_none() {
        return Ok(false);
    }
    run_tolerant(&plan_suspend(p, uid).commands, runner)
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

    #[test]
    fn uninstall_ignores_not_loaded_but_returns_other_failures() {
        struct Fails(&'static str);
        impl Runner for Fails {
            fn run(&self, cmd: &[String]) -> alice_core::Result<()> {
                Err(AliceError::Other(format!(
                    "`{}` exited with exit status: 1: {}",
                    cmd.join(" "),
                    self.0
                )))
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
        let e = uninstall(
            Platform::Linux,
            d.path(),
            1000,
            Some(&Fails("Failed to connect to bus: Permission denied")),
        )
        .unwrap_err();
        assert!(e.to_string().contains("Permission denied"), "{e}");
        assert!(
            installed(Platform::Linux, d.path()).is_some(),
            "the unit file stays after a real failure"
        );
        uninstall(
            Platform::Linux,
            d.path(),
            1000,
            Some(&Fails("Unit alice.service not loaded.")),
        )
        .unwrap();
        assert!(
            installed(Platform::Linux, d.path()).is_none(),
            "not loaded is fine: the file goes"
        );
    }

    /// `alice restore` stops the unit before the swap and starts it afterwards, with the
    /// commands of each platform; no unit, no commands.
    #[test]
    fn suspend_and_resume_stop_and_start_the_installed_unit_only() {
        struct Rec(parking_lot::Mutex<Vec<Vec<String>>>);
        impl Runner for Rec {
            fn run(&self, cmd: &[String]) -> alice_core::Result<()> {
                self.0.lock().push(cmd.to_vec());
                Ok(())
            }
        }
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
            let rec = Rec(Default::default());
            assert!(
                !suspend(platform, d.path(), 501, &rec).unwrap(),
                "no unit file, nothing to stop"
            );
            assert!(rec.0.lock().is_empty(), "{platform:?}");
            apply(
                &plan_install(platform, d.path(), Path::new("/x/alice"), &home(), 501),
                None,
            )
            .unwrap();
            assert!(suspend(platform, d.path(), 501, &rec).unwrap());
            resume(platform, d.path(), 501, &rec).unwrap();
            let seen = rec.0.lock().clone();
            assert_eq!(seen[0], stop, "{platform:?}");
            assert!(seen[1].contains(&start.to_string()), "{seen:?}");
            assert!(
                installed(platform, d.path()).is_some(),
                "the unit stays installed"
            );
        }
        // Windows: the task is not a file; `/End` on a task that does not exist is "nothing to do".
        struct NoTask;
        impl Runner for NoTask {
            fn run(&self, _: &[String]) -> alice_core::Result<()> {
                Err(AliceError::Other(
                    "`schtasks /End /TN Alice` exited with exit code: 1: ERROR: The system cannot find the file specified.".into(),
                ))
            }
        }
        assert!(!suspend(Platform::Windows, Path::new("/x"), 0, &NoTask).unwrap());
        // A stop that fails for another reason refuses: the restore must not go on.
        struct Denied;
        impl Runner for Denied {
            fn run(&self, _: &[String]) -> alice_core::Result<()> {
                Err(AliceError::Other("Access is denied".into()))
            }
        }
        assert!(suspend(Platform::Windows, Path::new("/x"), 0, &Denied).is_err());
    }
}
