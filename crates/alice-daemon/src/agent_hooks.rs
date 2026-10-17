//! `alice hooks install | uninstall | status <claude|codex>` (SP2 spec §4.3): Alice's capture
//! hooks in Claude Code's `settings.json` and Codex's `hooks.json`.
//!
//! - **What is written.** For each of [`CLAUDE_EVENTS`] one group,
//!   `{"hooks":[{"type":"command","command":"'<alice>' hook <event> --agent <agent> || true","timeout":10}]}`
//!   (Codex's SessionEnd gets 3 s), the command after `ALICE_HOME='<home>' ` when the home is
//!   not the default. ` || true`: a missing binary never fails the agent's turn.
//! - **Alice's entries** are commands whose program's file name is `alice` and whose words are
//!   `hook <event> --agent <agent>`, with or without that prefix and that suffix, whatever the
//!   path. Install removes them all, then adds fresh ones: it is idempotent and repairs hooks of
//!   a moved binary. Uninstall removes them, and the groups, lists and `hooks` object they leave
//!   empty (spec: "and arrays left empty"). An empty list the person had before the install is
//!   one of those: Alice cannot tell it from one it made.
//! - **The file** is edited through [`json`], which keeps key order; the rest of it stays as it
//!   is. A missing file starts from `{}`; one that is not strict JSON is refused untouched. A
//!   file that would not change is not written. Otherwise the new text is checked against
//!   `serde_json`, the old file is copied to `<file>.alice-backup-<stamp>` (0600; the newest
//!   five are kept) and the new one replaces it atomically.
//! - **Codex** takes hooks from one format per layer: an install is refused while its
//!   `config.toml` holds hooks of its own.

pub mod json;

use self::json::JValue;
use alice_core::Result;
use alice_core::error::AliceError;
use serde::Serialize;
use std::ffi::OsString;
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};

/// The agents' hook events and the `alice hook` event each one runs. Codex names them the same.
pub const CLAUDE_EVENTS: &[(&str, &str)] = &[
    ("SessionStart", "session-start"),
    ("Stop", "stop"),
    ("SubagentStop", "subagent-stop"),
    ("PreCompact", "pre-compact"),
    ("SessionEnd", "session-end"),
];

/// The last line of a Codex install: Codex runs a hook only once the person trusts it (see
/// [`install_notes`]).
pub const CODEX_TRUST: &str = "Codex runs a hook only after you trust it: open /hooks in Codex once and trust the Alice entries.";

/// A hook's time limit in seconds: a guard, as the hook itself ends within 100 ms.
const TIMEOUT_SECS: u32 = 10;
/// Codex gives SessionEnd at most 3 s.
const CODEX_SESSION_END_SECS: u32 = 3;
/// Backups kept next to each file.
const BACKUPS_KEPT: usize = 5;
const BACKUP_MARK: &str = ".alice-backup-";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum HookAgent {
    Claude,
    Codex,
}

impl HookAgent {
    /// The word on the command line: `claude` or `codex`.
    pub fn name(self) -> &'static str {
        match self {
            HookAgent::Claude => "claude",
            HookAgent::Codex => "codex",
        }
    }

    /// For people: `Claude Code` or `Codex`.
    pub fn label(self) -> &'static str {
        match self {
            HookAgent::Claude => "Claude Code",
            HookAgent::Codex => "Codex",
        }
    }

    /// The `--agent` of `alice hook`.
    fn hook_agent(self) -> &'static str {
        match self {
            HookAgent::Claude => "claude-code",
            HookAgent::Codex => "codex",
        }
    }

    fn timeout(self, event: &str) -> u32 {
        if self == HookAgent::Codex && event == "SessionEnd" {
            CODEX_SESSION_END_SECS
        } else {
            TIMEOUT_SECS
        }
    }
}

/// Where an agent's hooks go and what they run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookTarget {
    pub agent: HookAgent,
    /// Claude Code's `settings.json` or Codex's `hooks.json`.
    pub file: PathBuf,
    /// The `alice` the hooks run (the canonical path of this program).
    pub alice: PathBuf,
    /// `ALICE_HOME` for the hooks, when it is not the default `~/.alice`.
    pub alice_home: Option<PathBuf>,
    /// Codex's `config.toml`, which must not hold hooks of its own (`None` for Claude Code).
    pub codex_config: Option<PathBuf>,
}

impl HookTarget {
    /// The file `agent`'s hooks go in: `settings_override` (`--settings`), else Claude Code's
    /// `$CLAUDE_CONFIG_DIR/settings.json` (`~/.claude/settings.json`) or Codex's
    /// `$CODEX_HOME/hooks.json` (`~/.codex/hooks.json`). Codex's `config.toml` is always the one
    /// in its home. `env` reads the environment.
    pub fn locate(
        agent: HookAgent,
        settings_override: Option<&Path>,
        env: &dyn Fn(&str) -> Option<OsString>,
    ) -> Result<Self> {
        let var = |k: &str| {
            env(k)
                .filter(|v| !v.is_empty())
                .map(|v| absolute(Path::new(&v)))
        };
        let home = var("HOME").or_else(|| var("USERPROFILE"));
        let in_home = |dir: &str| {
            home.as_ref().map(|h| h.join(dir)).ok_or_else(|| {
                AliceError::invalid(format!(
                    "HOME is not set, so {}'s settings cannot be found; set HOME or pass --settings <file>",
                    agent.label()
                ))
            })
        };
        let codex_home = || var("CODEX_HOME").map_or_else(|| in_home(".codex"), Ok);
        let file = match (settings_override, agent) {
            (Some(p), _) => absolute(p),
            (None, HookAgent::Claude) => match var("CLAUDE_CONFIG_DIR") {
                Some(dir) => dir,
                None => in_home(".claude")?,
            }
            .join("settings.json"),
            (None, HookAgent::Codex) => codex_home()?.join("hooks.json"),
        };
        let codex_config = match agent {
            HookAgent::Codex => Some(codex_home()?.join("config.toml")),
            HookAgent::Claude => None,
        };
        let exe = std::env::current_exe().map_err(|e| {
            AliceError::Other(format!(
                "cannot find this program's path ({e}); run alice by its full path"
            ))
        })?;
        let alice = std::fs::canonicalize(&exe).unwrap_or(exe);
        let default_home = home.as_ref().map(|h| h.join(".alice"));
        let alice_home =
            var("ALICE_HOME").filter(|h| !default_home.as_ref().is_some_and(|d| same_path(h, d)));
        Ok(Self {
            agent,
            file,
            alice,
            alice_home,
            codex_config,
        })
    }
}

fn absolute(p: &Path) -> PathBuf {
    std::path::absolute(p).unwrap_or_else(|_| p.to_path_buf())
}

fn same_path(a: &Path, b: &Path) -> bool {
    a == b
        || matches!(
            (std::fs::canonicalize(a), std::fs::canonicalize(b)),
            (Ok(x), Ok(y)) if x == y
        )
}

/// What an install or uninstall did, or with `dry_run` would do.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HookChange {
    pub file: PathBuf,
    /// The copy of the previous file, when one was made.
    pub backup: Option<PathBuf>,
    /// The commands added.
    pub added: Vec<String>,
    /// Alice's entries removed (an install replaces them).
    pub removed: usize,
    /// Other hooks that mention `mempalace` or `alice`, for the person to look at, each as
    /// `<event>: <word> …` ([`other_label`]), never as its whole command.
    pub others: Vec<String>,
    /// Whether the file changes.
    pub changed: bool,
    /// The file's text after the command (with `dry_run`, as it would be). Never printed: it
    /// may hold the person's keys.
    #[serde(skip)]
    pub text: String,
}

/// Whether Alice's hooks are in an agent's file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HookStatus {
    pub file: PathBuf,
    /// All five events hold an Alice entry.
    pub installed: bool,
    /// Every program Alice's entries run exists (false when there are none).
    pub binary_exists: bool,
    /// The events that hold an Alice entry.
    pub events: Vec<String>,
    /// The programs Alice's entries run, each once.
    pub binaries: Vec<String>,
    /// Those of them that do not exist.
    pub missing: Vec<String>,
}

/// Adds Alice's hooks for the five events, after removing those already there.
pub fn install(t: &HookTarget, dry_run: bool) -> Result<HookChange> {
    if let Some(config) = &t.codex_config {
        check_codex_config(config, &t.file)?;
    }
    let (old, before) = load(&t.file)?;
    let mut after = before.clone();
    // The five events keep their place in the file when their only entry was Alice's.
    let keep: Vec<&str> = CLAUDE_EVENTS.iter().map(|(e, _)| *e).collect();
    let removed = remove_alice(&mut after, t.agent, &keep);
    let added = add_alice(&mut after, t)?;
    let change = HookChange {
        file: t.file.clone(),
        backup: None,
        added,
        removed,
        others: others(&before, t.agent),
        changed: old.is_none() || after != before,
        text: String::new(),
    };
    finish(change, old, &after, dry_run)
}

/// Removes exactly Alice's hooks, and the groups, lists and `hooks` object they leave empty.
pub fn uninstall(t: &HookTarget, dry_run: bool) -> Result<HookChange> {
    let (old, before) = load(&t.file)?;
    let mut after = before.clone();
    let removed = remove_alice(&mut after, t.agent, &[]);
    let change = HookChange {
        file: t.file.clone(),
        backup: None,
        added: Vec::new(),
        removed,
        others: others(&before, t.agent),
        changed: after != before,
        text: String::new(),
    };
    finish(change, old, &after, dry_run)
}

/// Which events hold Alice's hooks and whether the programs they run exist. Reads only.
pub fn status(t: &HookTarget) -> Result<HookStatus> {
    let (_, root) = load(&t.file)?;
    let mut events: Vec<String> = Vec::new();
    let mut binaries: Vec<String> = Vec::new();
    for (event, command) in commands(&root) {
        let Some(program) = alice_program(command, t.agent) else {
            continue;
        };
        if !events.iter().any(|e| e == event) {
            events.push(event.to_string());
        }
        if !binaries.contains(&program) {
            binaries.push(program);
        }
    }
    let missing: Vec<String> = binaries
        .iter()
        .filter(|b| !program_exists(b))
        .cloned()
        .collect();
    Ok(HookStatus {
        file: t.file.clone(),
        installed: CLAUDE_EVENTS
            .iter()
            .all(|(e, _)| events.iter().any(|x| x == e)),
        binary_exists: !binaries.is_empty() && missing.is_empty(),
        events,
        binaries,
        missing,
    })
}

/// The program an Alice entry for `agent` runs, if `command` is one: `<alice> hook <event>
/// --agent <agent>`, maybe after `ALICE_HOME=…` and before `|| true`. Nothing is run or
/// expanded; quotes are read as a POSIX shell reads them.
pub fn alice_program(command: &str, agent: HookAgent) -> Option<String> {
    let mut tokens = tokens(command)?;
    if tokens.ends_with(&[Token::Op("||".into()), Token::Word("true".into())]) {
        tokens.truncate(tokens.len() - 2);
    }
    let mut words = tokens
        .into_iter()
        .map(|t| match t {
            Token::Word(w) => Some(w),
            Token::Op(_) => None,
        })
        .collect::<Option<Vec<String>>>()?;
    if words.first().is_some_and(|w| w.starts_with("ALICE_HOME=")) {
        words.remove(0);
    }
    match words.as_slice() {
        [program, hook, _event, flag, name]
            if hook == "hook"
                && flag == "--agent"
                && name == agent.hook_agent()
                && Path::new(program).file_name().is_some_and(|n| n == "alice") =>
        {
            Some(program.clone())
        }
        _ => None,
    }
}

/// What `alice hooks install` prints. `undo` is the command that removes the hooks again.
pub fn install_report(t: &HookTarget, c: &HookChange, dry_run: bool, undo: &str) -> String {
    let (label, file) = (t.agent.label(), c.file.display());
    let mut s = String::new();
    if !c.changed {
        s.push_str(&format!(
            "{label}: Alice's capture hooks are already in {file}; nothing changed.\n"
        ));
    } else {
        let verb = if dry_run { "would add" } else { "added" };
        s.push_str(&format!(
            "{label}: {verb} Alice's capture hooks to {file}:\n"
        ));
        for a in &c.added {
            s.push_str(&format!("  {a}\n"));
        }
        if c.removed > 0 {
            s.push_str(&format!(
                "They replace {} earlier Alice {}.\n",
                c.removed,
                entries(c.removed)
            ));
        }
        if let Some(b) = &c.backup {
            s.push_str(&format!("The previous file is saved as {}.\n", b.display()));
        }
    }
    if !c.others.is_empty() {
        s.push_str(
            "Other hooks mention alice or mempalace; remove them yourself if Alice's capture replaces them:\n",
        );
        for o in &c.others {
            s.push_str(&format!("  {o}\n"));
        }
    }
    if dry_run {
        s.push_str("Dry run: nothing was written.\n");
    } else {
        s.push_str(&format!(
            "Hooks load in new sessions. To remove them: {undo}\n"
        ));
    }
    for note in install_notes(t.agent) {
        s.push_str(note);
        s.push('\n');
    }
    s
}

/// What the person still has to do after an install: for Codex, trust the hooks.
pub fn install_notes(agent: HookAgent) -> Vec<&'static str> {
    match agent {
        HookAgent::Claude => Vec::new(),
        HookAgent::Codex => vec![CODEX_TRUST],
    }
}

/// What `alice hooks uninstall` prints.
pub fn uninstall_report(t: &HookTarget, c: &HookChange, dry_run: bool) -> String {
    let (label, file) = (t.agent.label(), c.file.display());
    let mut s = if c.changed {
        let verb = if dry_run { "would remove" } else { "removed" };
        format!(
            "{label}: {verb} {} Alice {} from {file}.\n",
            c.removed,
            entries(c.removed)
        )
    } else {
        format!("{label}: {file} holds no Alice capture hooks; nothing changed.\n")
    };
    if let Some(b) = &c.backup {
        s.push_str(&format!("The previous file is saved as {}.\n", b.display()));
    }
    if dry_run {
        s.push_str("Dry run: nothing was written.\n");
    } else if c.changed {
        s.push_str("Sessions started from now on run without them.\n");
    }
    s
}

fn entries(n: usize) -> &'static str {
    if n == 1 { "entry" } else { "entries" }
}

/// `s` in single quotes for a POSIX shell.
pub fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// The command of Alice's hook for `event` (`stop`, …).
fn command(t: &HookTarget, event: &str) -> Result<String> {
    let quoted = |p: &Path| {
        p.to_str().map(shell_quote).ok_or_else(|| {
            AliceError::invalid(format!(
                "{} is not valid UTF-8, so no hook can name it; move it to a path that is",
                p.display()
            ))
        })
    };
    let home = match &t.alice_home {
        Some(h) => format!("ALICE_HOME={} ", quoted(h)?),
        None => String::new(),
    };
    Ok(format!(
        "{home}{} hook {event} --agent {} || true",
        quoted(&t.alice)?,
        t.agent.hook_agent()
    ))
}

/// A word of a shell command, or an operator (`||`, `;`, `|`, …) outside quotes.
#[derive(Debug, PartialEq)]
enum Token {
    Word(String),
    Op(String),
}

/// The command split as a POSIX shell splits it: quotes and backslashes removed, nothing
/// expanded. `None` for an unfinished quote or escape.
fn tokens(command: &str) -> Option<Vec<Token>> {
    let mut out = Vec::new();
    let mut word: Option<String> = None;
    let mut chars = command.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            ' ' | '\t' | '\n' => out.extend(word.take().map(Token::Word)),
            '\'' => {
                let w = word.get_or_insert_with(String::new);
                loop {
                    match chars.next()? {
                        '\'' => break,
                        c => w.push(c),
                    }
                }
            }
            '"' => {
                let w = word.get_or_insert_with(String::new);
                loop {
                    match chars.next()? {
                        '"' => break,
                        '\\' => match chars.next()? {
                            c @ ('"' | '\\' | '$' | '`') => w.push(c),
                            '\n' => {}
                            c => {
                                w.push('\\');
                                w.push(c);
                            }
                        },
                        c => w.push(c),
                    }
                }
            }
            '\\' => match chars.next()? {
                '\n' => {}
                c => word.get_or_insert_with(String::new).push(c),
            },
            '|' | '&' | ';' | '<' | '>' | '(' | ')' => {
                out.extend(word.take().map(Token::Word));
                let mut op = c.to_string();
                while let Some(&n) = chars.peek()
                    && "|&;<>()".contains(n)
                {
                    op.push(n);
                    chars.next();
                }
                out.push(Token::Op(op));
            }
            c => word.get_or_insert_with(String::new).push(c),
        }
    }
    out.extend(word.map(Token::Word));
    Some(out)
}

/// The file's text (`None`: there is none) and its value: `{}` for no file or an empty one.
fn load(file: &Path) -> Result<(Option<String>, JValue)> {
    let text = match std::fs::read_to_string(file) {
        Ok(t) => Some(t),
        Err(e) if e.kind() == ErrorKind::NotFound => None,
        Err(e) => {
            return Err(AliceError::Other(format!(
                "cannot read {}: {e}; nothing was changed",
                file.display()
            )));
        }
    };
    let value = match text.as_deref() {
        Some(t) if !t.trim().is_empty() => json::parse(t).map_err(|e| {
            AliceError::invalid(format!(
                "{} is not valid JSON ({e}); fix it, then run the command again. Nothing was changed.",
                file.display()
            ))
        })?,
        _ => JValue::Obj(Vec::new()),
    };
    if !matches!(value, JValue::Obj(_)) {
        return Err(shape(file, "it does not hold a JSON object"));
    }
    Ok((text, value))
}

fn shape(file: &Path, what: &str) -> AliceError {
    AliceError::invalid(format!(
        "{} cannot take Alice's hooks: {what}. Fix the file, then run the command again; nothing was changed.",
        file.display()
    ))
}

/// Each hook command of the file, with its event, in file order.
fn commands(root: &JValue) -> Vec<(&str, &str)> {
    let mut out = Vec::new();
    let Some(JValue::Obj(events)) = root.get("hooks") else {
        return out;
    };
    for (event, groups) in events {
        let JValue::Arr(groups) = groups else {
            continue;
        };
        for group in groups {
            let Some(JValue::Arr(hooks)) = group.get("hooks") else {
                continue;
            };
            for hook in hooks {
                if let Some(c) = hook.get("command").and_then(JValue::as_str) {
                    out.push((event.as_str(), c));
                }
            }
        }
    }
    out
}

/// The other hooks whose commands mention `alice` or `mempalace`, each once, in file order, as
/// [`other_label`]s.
fn others(root: &JValue, agent: HookAgent) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for (event, c) in commands(root) {
        if mentions_memory(c) && alice_program(c, agent).is_none() {
            let label = other_label(event, c);
            if !out.contains(&label) {
                out.push(label);
            }
        }
    }
    out
}

/// How a report names another hook (final review A-M6): its event and the file name of the
/// word that mentions Alice or MemPalace, a variable by its name only, redacted, and `…` when
/// the command has more: `SessionStart: alice-session-start …`. Never the whole command: it may
/// hold a key, and an agent that runs the install reads the report into its transcript.
fn other_label(event: &str, command: &str) -> String {
    let words = words(command);
    let Some(word) = words.iter().find(|w| names_memory(w)) else {
        return format!("{event}: …");
    };
    let name = match word.split_once('=') {
        Some((var, _)) => format!("{var}=…"),
        None => word.rsplit('/').next().unwrap_or_default().to_owned(),
    };
    let name = alice_core::redact::Redactor::builtin().apply(&name).text;
    let more = if words.len() > 1 { " …" } else { "" };
    format!("{event}: {name}{more}")
}

/// The words of a command (a shell's split, else its whitespace's).
fn words(command: &str) -> Vec<String> {
    match tokens(command) {
        Some(tokens) => tokens
            .into_iter()
            .filter_map(|t| match t {
                Token::Word(w) => Some(w),
                Token::Op(_) => None,
            })
            .collect(),
        None => command.split_whitespace().map(String::from).collect(),
    }
}

/// Whether a word names something of Alice's or MemPalace's: its last path component starts
/// with `alice` or `mempalace`, whatever the case (`alice-session-start`, `mempalace.hooks`).
/// A folder on the way (`/home/alice/…`) does not count.
fn names_memory(word: &str) -> bool {
    let last = word.rsplit('/').next().unwrap_or_default().to_lowercase();
    last.starts_with("alice") || last.starts_with("mempalace")
}

/// Whether a word of `command` names something of Alice's or MemPalace's ([`names_memory`]).
fn mentions_memory(command: &str) -> bool {
    words(command).iter().any(|w| names_memory(w))
}

/// Removes Alice's entries for `agent`, and the groups they leave empty; returns how many. An
/// event list they leave empty goes too, unless it is one of `keep`, and so does a `hooks`
/// object left empty.
fn remove_alice(root: &mut JValue, agent: HookAgent, keep: &[&str]) -> usize {
    let JValue::Obj(fields) = root else {
        return 0;
    };
    let Some(at) = fields.iter().position(|(k, _)| k == "hooks") else {
        return 0;
    };
    let JValue::Obj(events) = &mut fields[at].1 else {
        return 0;
    };
    let had = events.len();
    let mut removed = 0;
    events.retain_mut(|(event, groups)| {
        let JValue::Arr(groups) = groups else {
            return true;
        };
        let before = removed;
        groups.retain_mut(|group| {
            let Some(JValue::Arr(hooks)) = group.get_mut("hooks") else {
                return true;
            };
            let n = hooks.len();
            hooks.retain(|h| {
                !h.get("command")
                    .and_then(JValue::as_str)
                    .is_some_and(|c| alice_program(c, agent).is_some())
            });
            removed += n - hooks.len();
            !(hooks.is_empty() && n > 0)
        });
        !(groups.is_empty() && removed > before && !keep.contains(&event.as_str()))
    });
    if had > 0 && events.is_empty() {
        fields.remove(at);
    }
    removed
}

/// Appends one group to each event's list, running Alice's hook; returns the commands.
fn add_alice(root: &mut JValue, t: &HookTarget) -> Result<Vec<String>> {
    let JValue::Obj(fields) = root else {
        return Err(shape(&t.file, "it does not hold a JSON object"));
    };
    if !fields.iter().any(|(k, _)| k == "hooks") {
        fields.push(("hooks".into(), JValue::Obj(Vec::new())));
    }
    let Some(JValue::Obj(events)) = root.get_mut("hooks") else {
        return Err(shape(&t.file, "\"hooks\" is not an object"));
    };
    let mut added = Vec::new();
    for &(event, arg) in CLAUDE_EVENTS {
        let command = command(t, arg)?;
        let hook = JValue::Obj(vec![
            ("type".into(), JValue::Str("command".into())),
            ("command".into(), JValue::Str(command.clone())),
            (
                "timeout".into(),
                JValue::Num(t.agent.timeout(event).to_string()),
            ),
        ]);
        let group = JValue::Obj(vec![("hooks".into(), JValue::Arr(vec![hook]))]);
        match events.iter_mut().find(|(k, _)| k == event) {
            Some((_, JValue::Arr(groups))) => groups.push(group),
            Some(_) => {
                return Err(shape(
                    &t.file,
                    &format!("\"hooks\".\"{event}\" is not a list"),
                ));
            }
            None => events.push((event.into(), JValue::Arr(vec![group]))),
        }
        added.push(command);
    }
    Ok(added)
}

/// The change with its text (`after`, or the old text when nothing changes), written unless
/// `dry_run` or unchanged.
fn finish(
    mut change: HookChange,
    old: Option<String>,
    after: &JValue,
    dry_run: bool,
) -> Result<HookChange> {
    if !change.changed {
        change.text = old.unwrap_or_default();
        return Ok(change);
    }
    change.text = json::print(after);
    // Plan decision P5: what is written must read back as the value meant.
    let meant = after.to_serde();
    let read = serde_json::from_str::<serde_json::Value>(&change.text);
    if !matches!((meant, read), (Ok(m), Ok(r)) if m == r) {
        return Err(AliceError::Other(format!(
            "Alice could not rewrite {} safely; it is unchanged",
            change.file.display()
        )));
    }
    if !dry_run {
        change.backup = write(&change.file, old.as_deref(), &change.text)?;
    }
    Ok(change)
}

/// Puts `text` in place of `file` atomically, after a backup of `old` (when there is a file). A
/// link stays a link: the file it points to is replaced, and the backup sits next to the link.
fn write(file: &Path, old: Option<&str>, text: &str) -> Result<Option<PathBuf>> {
    let fail = |e: std::io::Error| {
        AliceError::Other(format!(
            "cannot write {}: {e}; it is unchanged",
            file.display()
        ))
    };
    let target = match std::fs::symlink_metadata(file) {
        Ok(m) if m.file_type().is_symlink() => std::fs::canonicalize(file).map_err(fail)?,
        _ => file.to_path_buf(),
    };
    if let Some(dir) = target.parent() {
        std::fs::create_dir_all(dir).map_err(fail)?;
    }
    let tmp = target.with_file_name(format!(
        ".{}.alice-{}.tmp",
        file_name(&target),
        alice_core::ids::new_id()
    ));
    let perms = std::fs::metadata(&target).ok().map(|m| m.permissions());
    let done = (|| {
        let mut f = create_private(&tmp).map_err(fail)?;
        f.write_all(text.as_bytes()).map_err(fail)?;
        if let Some(p) = perms {
            f.set_permissions(p).map_err(fail)?;
        }
        f.sync_all().map_err(fail)?;
        drop(f);
        // An edit made since the file was read is never written over.
        let now = match std::fs::read_to_string(&target) {
            Ok(t) => Some(t),
            Err(e) if e.kind() == ErrorKind::NotFound => None,
            Err(e) => return Err(fail(e)),
        };
        if now.as_deref() != old {
            return Err(AliceError::Other(format!(
                "{} changed while Alice was editing it; it is unchanged. Run the command again",
                file.display()
            )));
        }
        let backup = old.map(|o| backup(file, o)).transpose()?;
        std::fs::rename(&tmp, &target).map_err(fail)?;
        #[cfg(unix)]
        if let Some(dir) = target.parent().and_then(|d| std::fs::File::open(d).ok()) {
            let _ = dir.sync_all();
        }
        Ok(backup)
    })();
    if done.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    let backup = done?;
    prune_backups(file);
    Ok(backup)
}

fn file_name(p: &Path) -> String {
    p.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn create_private(path: &Path) -> std::io::Result<std::fs::File> {
    let mut o = std::fs::OpenOptions::new();
    o.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.mode(0o600);
    }
    o.open(path)
}

/// Copies `old` to `<file>.alice-backup-<yyyymmddThhmmssZ>` (private). A second that already
/// has backups gives `-<n>`, counting on from the highest: a name freed by pruning is never
/// taken again, or the newest backup would sort first and be the next one pruned.
fn backup(file: &Path, old: &str) -> Result<PathBuf> {
    let stamp = chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
    let base = format!("{}{BACKUP_MARK}{stamp}", file_name(file));
    let fail = |p: &Path, e: std::io::Error| {
        AliceError::Other(format!(
            "cannot back up {} to {}: {e}; it is unchanged",
            file.display(),
            p.display()
        ))
    };
    let first = backups(file)
        .into_iter()
        .filter(|((s, _), _)| *s == stamp)
        .map(|((_, n), _)| n + 1)
        .max()
        .unwrap_or(0);
    for n in first..first + 1000 {
        let path = file.with_file_name(if n == 0 {
            base.clone()
        } else {
            format!("{base}-{n}")
        });
        match create_private(&path) {
            Ok(mut f) => {
                if let Err(e) = f.write_all(old.as_bytes()).and_then(|()| f.sync_all()) {
                    let _ = std::fs::remove_file(&path);
                    return Err(fail(&path, e));
                }
                return Ok(path);
            }
            Err(e) if e.kind() == ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(fail(&path, e)),
        }
    }
    Err(AliceError::Other(format!(
        "{} has too many backups made this second; it is unchanged. Run the command again",
        file.display()
    )))
}

/// The backups of `file` with their keys, oldest first. Other files of the folder are never
/// counted.
fn backups(file: &Path) -> Vec<((String, u32), PathBuf)> {
    let prefix = format!("{}{BACKUP_MARK}", file_name(file));
    let Some(Ok(entries)) = file.parent().map(std::fs::read_dir) else {
        return Vec::new();
    };
    let mut found: Vec<((String, u32), PathBuf)> = entries
        .filter_map(|e| {
            let e = e.ok()?;
            let name = e.file_name().into_string().ok()?;
            Some((backup_key(name.strip_prefix(&prefix)?)?, e.path()))
        })
        .collect();
    found.sort();
    found
}

/// `<yyyymmddThhmmssZ>[-<n>]` as a key that sorts by age.
fn backup_key(rest: &str) -> Option<(String, u32)> {
    let (stamp, n) = match rest.split_once('-') {
        Some((stamp, n)) if n.bytes().all(|b| b.is_ascii_digit()) => (stamp, n.parse().ok()?),
        Some(_) => return None,
        None => (rest, 0),
    };
    let b = stamp.as_bytes();
    let ok = b.len() == 16
        && b[8] == b'T'
        && b[15] == b'Z'
        && b.iter()
            .enumerate()
            .all(|(i, c)| i == 8 || i == 15 || c.is_ascii_digit());
    ok.then(|| (stamp.to_string(), n))
}

/// Deletes all but the newest [`BACKUPS_KEPT`] backups of `file`.
fn prune_backups(file: &Path) {
    let all = backups(file);
    for (_, old) in &all[..all.len().saturating_sub(BACKUPS_KEPT)] {
        let _ = std::fs::remove_file(old);
    }
}

/// Whether a hook's program is there: a path is looked at, a bare name looked for on `PATH`.
fn program_exists(program: &str) -> bool {
    let p = Path::new(program);
    if p.components().count() > 1 {
        return p.is_file();
    }
    std::env::var_os("PATH")
        .is_some_and(|path| std::env::split_paths(&path).any(|d| d.join(p).is_file()))
}

/// Codex takes hooks from one format per layer: while its `config.toml` holds hooks of its own,
/// a top-level `hooks` key (`[hooks]` tables, `[[hooks.<Event>]]` arrays, `hooks.<Event> = …`
/// before the first table), no `hooks.json` is written beside them. `[features] hooks = true`
/// is no such key. The tenant's `setup.sh` (`codex_inline_hooks`) keeps the same rule.
fn check_codex_config(config: &Path, file: &Path) -> Result<()> {
    let text = match std::fs::read_to_string(config) {
        Ok(t) => t,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(()),
        Err(e) => {
            return Err(AliceError::Other(format!(
                "cannot read {}: {e}; nothing was changed",
                config.display()
            )));
        }
    };
    let table: toml::Table = toml::from_str(&text).map_err(|e| {
        // The line number only: a line of config.toml may hold a key.
        let line = e
            .span()
            .map(|s| {
                format!(
                    " (line {})",
                    alice_core::settings::toml_line(&text, s.start)
                )
            })
            .unwrap_or_default();
        AliceError::invalid(format!(
            "{} is not valid TOML{line}; fix it, then run the command again. Nothing was changed.",
            config.display()
        ))
    })?;
    if table.contains_key("hooks") {
        return Err(AliceError::invalid(format!(
            "{} has hooks of its own ([hooks] tables), and Codex takes hooks from one format per layer, so Alice did not write {}. Move those hooks into {} or remove them, then run `alice hooks install codex` again; nothing was changed.",
            config.display(),
            file.display(),
            file.display()
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A throwaway machine: `HOME`, `CLAUDE_CONFIG_DIR`, `CODEX_HOME` and `ALICE_HOME` all point
    /// into a temporary directory, and an `alice` program exists there.
    struct Machine {
        _dir: tempfile::TempDir,
        root: PathBuf,
    }

    impl Machine {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let root = std::fs::canonicalize(dir.path()).unwrap();
            std::fs::create_dir_all(root.join("bin")).unwrap();
            std::fs::write(root.join("bin/alice"), "").unwrap();
            Machine { _dir: dir, root }
        }

        fn home(&self) -> PathBuf {
            self.root.join("home")
        }

        fn alice_home(&self) -> PathBuf {
            self.root.join("alice home")
        }

        fn alice(&self) -> PathBuf {
            self.root.join("bin/alice")
        }

        /// The environment with `unset` left out.
        fn env_without(&self, unset: &'static [&'static str]) -> impl Fn(&str) -> Option<OsString> {
            let (home, alice_home) = (self.home(), self.alice_home());
            move |k: &str| {
                if unset.contains(&k) {
                    return None;
                }
                match k {
                    "HOME" => Some(home.clone().into()),
                    "CLAUDE_CONFIG_DIR" => Some(home.join("claude-config").into()),
                    "CODEX_HOME" => Some(home.join("codex-home").into()),
                    "ALICE_HOME" => Some(alice_home.clone().into()),
                    _ => None,
                }
            }
        }

        /// `agent`'s target, running this machine's `alice`.
        fn target(&self, agent: HookAgent) -> HookTarget {
            let mut t = HookTarget::locate(agent, None, &self.env_without(&[])).unwrap();
            t.alice = self.alice();
            t
        }

        /// The command Alice writes for `event` (`stop`, …).
        fn command(&self, event: &str, agent: &str) -> String {
            format!(
                "ALICE_HOME='{}' '{}' hook {event} --agent {agent} || true",
                self.alice_home().display(),
                self.alice().display()
            )
        }
    }

    fn read(p: &Path) -> String {
        std::fs::read_to_string(p).unwrap()
    }

    fn value(p: &Path) -> serde_json::Value {
        serde_json::from_str(&read(p)).unwrap()
    }

    fn write(p: &Path, text: &str) {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, text).unwrap();
    }

    /// The backups next to `file`, by name.
    fn backups_of(file: &Path) -> Vec<PathBuf> {
        let prefix = format!(
            "{}.alice-backup-",
            file.file_name().unwrap().to_string_lossy()
        );
        let mut out: Vec<PathBuf> = std::fs::read_dir(file.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|p| {
                p.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with(&prefix)
            })
            .collect();
        out.sort();
        out
    }

    fn mtime(p: &Path) -> std::time::SystemTime {
        std::fs::metadata(p).unwrap().modified().unwrap()
    }

    /// Sets `p`'s mtime an hour back, so a rewrite would show.
    fn age(p: &Path) {
        let f = std::fs::File::options().write(true).open(p).unwrap();
        f.set_modified(std::time::SystemTime::now() - std::time::Duration::from_secs(3600))
            .unwrap();
    }

    /// The `{ … }` at `indent` spaces around `needle` in `text`, as written.
    fn enclosing<'a>(text: &'a str, needle: &str, indent: usize) -> &'a str {
        let pad = " ".repeat(indent);
        let at = text.find(needle).unwrap();
        let start = text[..at].rfind(&format!("\n{pad}{{")).unwrap() + 1;
        let end = at + text[at..].find(&format!("\n{pad}}}")).unwrap() + 1 + indent + 1;
        &text[start..end]
    }

    /// A Claude Code settings file of the person's own, written as Claude Code writes it.
    const OWN: &str = r#"{
  "model": "opus",
  "permissions": {
    "allow": [
      "Bash(ls:*)"
    ]
  },
  "hooks": {
    "Stop": [
      {
        "hooks": [
          {
            "type": "command",
            "command": "afplay x.aiff"
          }
        ]
      }
    ],
    "PreToolUse": [
      {
        "matcher": "Bash",
        "hooks": [
          {
            "type": "command",
            "command": "check-bash",
            "timeout": 5
          }
        ]
      }
    ]
  },
  "theme": "dark"
}
"#;

    #[test]
    fn locate_finds_each_agent_s_file_and_the_home_its_hooks_name() {
        let m = Machine::new();
        let home = m.home();
        let t = HookTarget::locate(HookAgent::Claude, None, &m.env_without(&[])).unwrap();
        assert_eq!(t.file, home.join("claude-config/settings.json"));
        assert_eq!(t.codex_config, None);
        assert_eq!(t.alice_home, Some(m.alice_home()));
        assert!(t.alice.is_absolute(), "{t:?}");
        let t = HookTarget::locate(
            HookAgent::Claude,
            None,
            &m.env_without(&["CLAUDE_CONFIG_DIR", "ALICE_HOME"]),
        )
        .unwrap();
        assert_eq!(t.file, home.join(".claude/settings.json"));
        assert_eq!(t.alice_home, None, "the default home is not named");
        let t = HookTarget::locate(HookAgent::Codex, None, &m.env_without(&[])).unwrap();
        assert_eq!(t.file, home.join("codex-home/hooks.json"));
        assert_eq!(t.codex_config, Some(home.join("codex-home/config.toml")));
        let t =
            HookTarget::locate(HookAgent::Codex, None, &m.env_without(&["CODEX_HOME"])).unwrap();
        assert_eq!(t.file, home.join(".codex/hooks.json"));
        assert_eq!(t.codex_config, Some(home.join(".codex/config.toml")));
        // `--settings` names the file; Codex's config.toml stays in its home.
        let other = m.root.join("other.json");
        let t = HookTarget::locate(HookAgent::Codex, Some(&other), &m.env_without(&[])).unwrap();
        assert_eq!(t.file, other);
        assert_eq!(t.codex_config, Some(home.join("codex-home/config.toml")));
        let t = HookTarget::locate(HookAgent::Claude, Some(&other), &m.env_without(&[])).unwrap();
        assert_eq!(t.file, other);
        // ALICE_HOME naming the default home is the default.
        let default_home = home.join(".alice");
        let env = move |k: &str| match k {
            "HOME" => Some(m.home().into()),
            "ALICE_HOME" => Some(default_home.clone().into()),
            _ => None,
        };
        assert_eq!(
            HookTarget::locate(HookAgent::Claude, None, &env)
                .unwrap()
                .alice_home,
            None
        );
        let e = HookTarget::locate(HookAgent::Claude, None, &|_| None).unwrap_err();
        assert!(e.to_string().contains("--settings"), "{e}");
    }

    #[test]
    fn install_creates_a_missing_settings_file() {
        let m = Machine::new();
        let t = m.target(HookAgent::Claude);
        assert!(!t.file.exists());
        let c = install(&t, false).unwrap();
        assert!(c.changed && c.backup.is_none() && c.removed == 0, "{c:?}");
        assert_eq!(read(&t.file), c.text);
        let v = value(&t.file);
        assert_eq!(v.as_object().unwrap().len(), 1, "{v}");
        assert_eq!(v["hooks"].as_object().unwrap().len(), 5, "{v}");
        let mut want = Vec::new();
        for (event, arg) in CLAUDE_EVENTS {
            let groups = v["hooks"][event].as_array().unwrap();
            assert_eq!(groups.len(), 1, "{event}: {v}");
            assert!(groups[0].get("matcher").is_none());
            assert_eq!(
                groups[0]["hooks"],
                serde_json::json!([{
                    "type": "command",
                    "command": m.command(arg, "claude-code"),
                    "timeout": 10
                }])
            );
            want.push(m.command(arg, "claude-code"));
        }
        assert_eq!(c.added, want);
        // The events in the order of the list, and the command before its timeout.
        let text = read(&t.file);
        let at = |k: &str| text.find(k).unwrap();
        assert!(at("SessionStart") < at("\"Stop\"") && at("PreCompact") < at("SessionEnd"));
        assert!(at("\"type\"") < at("\"command\"") && at("\"command\"") < at("\"timeout\""));
    }

    #[test]
    fn install_merges_beside_other_hooks_and_keeps_them() {
        let m = Machine::new();
        let t = m.target(HookAgent::Claude);
        assert_eq!(
            json::print(&json::parse(OWN).unwrap()),
            OWN,
            "written as Alice prints"
        );
        write(&t.file, OWN);
        let c = install(&t, false).unwrap();
        assert!(c.changed && c.removed == 0, "{c:?}");
        let text = read(&t.file);
        // The person's hooks, byte for byte.
        let afplay = enclosing(OWN, "afplay x.aiff", 6);
        let pre_tool_use = enclosing(OWN, "check-bash", 6);
        assert!(text.contains(afplay), "{text}");
        assert!(text.contains(pre_tool_use), "{text}");
        assert!(text.contains("    \"PreToolUse\": [\n      {\n        \"matcher\": \"Bash\""));
        // The top-level keys in their order, `hooks` where it was.
        let at = |k: &str| text.find(&format!("\n  \"{k}\"")).unwrap();
        assert!(at("model") < at("permissions") && at("permissions") < at("hooks"));
        assert!(at("hooks") < at("theme"));
        // Alice's Stop entry after the person's, in the same list.
        let v = value(&t.file);
        let stop = v["hooks"]["Stop"].as_array().unwrap();
        assert_eq!(stop.len(), 2);
        assert_eq!(stop[0]["hooks"][0]["command"], "afplay x.aiff");
        assert_eq!(
            stop[1]["hooks"][0]["command"],
            m.command("stop", "claude-code")
        );
        assert_eq!(v["hooks"]["PreToolUse"].as_array().unwrap().len(), 1);
        assert_eq!(v["permissions"]["allow"][0], "Bash(ls:*)");
        let backup = c.backup.unwrap();
        assert_eq!(read(&backup), OWN);
    }

    #[test]
    fn install_is_idempotent() {
        let m = Machine::new();
        let t = m.target(HookAgent::Claude);
        write(&t.file, OWN);
        install(&t, false).unwrap();
        let first = std::fs::read(&t.file).unwrap();
        assert_eq!(backups_of(&t.file).len(), 1);
        age(&t.file);
        let before = mtime(&t.file);
        let again = install(&t, false).unwrap();
        assert!(!again.changed && again.backup.is_none(), "{again:?}");
        assert_eq!(again.removed, 5, "replaced by the same five");
        assert_eq!(std::fs::read(&t.file).unwrap(), first);
        assert_eq!(mtime(&t.file), before, "not rewritten");
        assert_eq!(backups_of(&t.file).len(), 1, "no second backup");
    }

    #[test]
    fn install_repairs_hooks_of_a_moved_binary() {
        let m = Machine::new();
        let t = m.target(HookAgent::Claude);
        write(
            &t.file,
            r#"{"hooks": {
  "Stop": [{"hooks": [{"type": "command", "command": "/old/path/alice hook stop --agent claude-code"}]}],
  "SessionStart": [{"hooks": [
    {"type": "command", "command": "ALICE_HOME='/old/home' '/old/path/alice' hook session-start --agent claude-code || true", "timeout": 10},
    {"type": "command", "command": "echo hello"}
  ]}]
}}"#,
        );
        let c = install(&t, false).unwrap();
        assert_eq!(c.removed, 2, "{c:?}");
        let text = read(&t.file);
        assert!(!text.contains("/old/"), "{text}");
        let v = value(&t.file);
        let stop = v["hooks"]["Stop"].as_array().unwrap();
        assert_eq!(stop.len(), 1, "replaced, not added beside: {v}");
        assert_eq!(
            stop[0]["hooks"][0]["command"],
            m.command("stop", "claude-code")
        );
        // A group that keeps another hook stays where it was, without Alice's.
        let start = v["hooks"]["SessionStart"].as_array().unwrap();
        assert_eq!(start.len(), 2, "{v}");
        assert_eq!(
            start[0]["hooks"],
            serde_json::json!([{"type": "command", "command": "echo hello"}])
        );
        assert_eq!(
            start[1]["hooks"][0]["command"],
            m.command("session-start", "claude-code")
        );
        for (event, _) in CLAUDE_EVENTS {
            let alice = v["hooks"][event]
                .as_array()
                .unwrap()
                .iter()
                .flat_map(|g| g["hooks"].as_array().unwrap())
                .filter(|h| {
                    alice_program(h["command"].as_str().unwrap(), HookAgent::Claude).is_some()
                })
                .count();
            assert_eq!(alice, 1, "{event}: {v}");
        }
    }

    #[test]
    fn entries_with_or_without_the_home_and_the_or_true_are_alices() {
        use HookAgent::{Claude, Codex};
        for (command, agent, program) in [
            (
                "/usr/local/bin/alice hook stop --agent claude-code",
                Claude,
                "/usr/local/bin/alice",
            ),
            ("alice hook stop --agent claude-code", Claude, "alice"),
            (
                "'/opt/my alice/alice' hook stop --agent claude-code || true",
                Claude,
                "/opt/my alice/alice",
            ),
            (
                "\"/x y/alice\" hook pre-compact --agent claude-code||true",
                Claude,
                "/x y/alice",
            ),
            (
                "ALICE_HOME=/h /x/alice hook session-end --agent claude-code",
                Claude,
                "/x/alice",
            ),
            (
                "ALICE_HOME='/tmp/it'\\''s home' '/it'\\''s/alice' hook stop --agent codex || true",
                Codex,
                "/it's/alice",
            ),
        ] {
            assert_eq!(
                alice_program(command, agent).as_deref(),
                Some(program),
                "{command}"
            );
        }
        for (command, agent) in [
            ("/x/alice hook stop --agent codex", Claude),
            ("/x/alice hook stop --agent claude-code", Codex),
            ("/x/alice hook stop", Claude),
            ("~/.local/bin/alice-session-start", Claude),
            ("mempalace hook stop", Claude),
            ("/x/notalice hook stop --agent claude-code", Claude),
            (
                "/x/alice hook stop --agent claude-code; rm -rf /tmp/x",
                Claude,
            ),
            (
                "/x/alice hook stop --agent claude-code || echo failed",
                Claude,
            ),
            ("/x/alice hook stop --agent claude-code extra", Claude),
            ("FOO=1 /x/alice hook stop --agent claude-code", Claude),
            ("'/x/alice hook stop --agent claude-code", Claude),
            ("", Claude),
        ] {
            assert_eq!(alice_program(command, agent), None, "{command}");
        }
        // Both forms are replaced by an install, so a reinstall never doubles them.
        let m = Machine::new();
        let t = m.target(Claude);
        write(
            &t.file,
            r#"{"hooks": {"Stop": [
  {"hooks": [{"type": "command", "command": "/x/alice hook stop --agent claude-code || true"}]},
  {"hooks": [{"type": "command", "command": "ALICE_HOME='/h' /x/alice hook stop --agent claude-code"}]}
]}}"#,
        );
        assert_eq!(install(&t, false).unwrap().removed, 2);
        assert_eq!(value(&t.file)["hooks"]["Stop"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn uninstall_removes_only_alices_entries() {
        let m = Machine::new();
        let t = m.target(HookAgent::Claude);
        write(&t.file, OWN);
        install(&t, false).unwrap();
        let c = uninstall(&t, false).unwrap();
        assert!(c.changed && c.removed == 5 && c.added.is_empty(), "{c:?}");
        assert_eq!(read(&t.file), OWN, "exactly what install added is gone");
        assert!(c.backup.is_some());
        // Nothing left to remove: nothing changes and no backup is made.
        let n = backups_of(&t.file).len();
        let again = uninstall(&t, false).unwrap();
        assert!(
            !again.changed && again.removed == 0 && again.backup.is_none(),
            "{again:?}"
        );
        assert_eq!(backups_of(&t.file).len(), n);
        // A file with no hooks before: the `hooks` object Alice made goes too.
        let plain = "{\n  \"model\": \"opus\"\n}\n";
        write(&t.file, plain);
        install(&t, false).unwrap();
        uninstall(&t, false).unwrap();
        assert_eq!(read(&t.file), plain);
        // A missing file stays missing.
        std::fs::remove_file(&t.file).unwrap();
        let c = uninstall(&t, false).unwrap();
        assert!(!c.changed && c.removed == 0, "{c:?}");
        assert!(!t.file.exists());
    }

    #[test]
    fn a_malformed_settings_file_is_refused_untouched() {
        let m = Machine::new();
        let t = m.target(HookAgent::Claude);
        let bad = "{\n  \"model\": \"opus\",\n}\n";
        write(&t.file, bad);
        age(&t.file);
        let before = mtime(&t.file);
        for (what, e) in [
            ("install", install(&t, false).unwrap_err()),
            ("dry run", install(&t, true).unwrap_err()),
            ("uninstall", uninstall(&t, false).unwrap_err()),
            ("status", status(&t).unwrap_err()),
        ] {
            let e = e.to_string();
            assert!(e.contains(&t.file.display().to_string()), "{what}: {e}");
            assert!(e.contains("line 3"), "{what}: {e}");
        }
        assert_eq!(read(&t.file), bad);
        assert_eq!(mtime(&t.file), before);
        assert!(backups_of(&t.file).is_empty());
        // Valid JSON that is not an object, or hooks of the wrong shape, are refused too.
        for other in ["[]\n", "{\"hooks\": []}", "{\"hooks\": {\"Stop\": {}}}"] {
            write(&t.file, other);
            let e = install(&t, false).unwrap_err().to_string();
            assert!(e.contains(&t.file.display().to_string()), "{other}: {e}");
            assert_eq!(read(&t.file), other);
        }
        assert!(backups_of(&t.file).is_empty());
        // A file that is not UTF-8: refused the same way, and the error says so.
        let latin1 = b"{\"model\": \"caf\xe9\"}\n";
        std::fs::write(&t.file, latin1).unwrap();
        for e in [install(&t, false).unwrap_err(), status(&t).unwrap_err()] {
            let e = e.to_string();
            assert!(
                e.contains(&t.file.display().to_string()) && e.contains("nothing was changed"),
                "{e}"
            );
        }
        assert_eq!(std::fs::read(&t.file).unwrap(), latin1);
        assert!(backups_of(&t.file).is_empty());
    }

    /// Spec §4.3: uninstall removes Alice's entries "and arrays left empty". It cannot tell an
    /// empty list (or `hooks` object) the person had before the install from one Alice made,
    /// so that one goes too; an empty one Alice never filled stays.
    #[test]
    fn uninstall_drops_the_empty_lists_it_leaves_even_ones_the_person_made() {
        let m = Machine::new();
        let t = m.target(HookAgent::Claude);
        write(
            &t.file,
            "{\n  \"hooks\": {\n    \"Stop\": [],\n    \"Notification\": []\n  }\n}\n",
        );
        install(&t, false).unwrap();
        uninstall(&t, false).unwrap();
        assert_eq!(
            read(&t.file),
            "{\n  \"hooks\": {\n    \"Notification\": []\n  }\n}\n"
        );
        write(&t.file, "{\n  \"model\": \"opus\",\n  \"hooks\": {}\n}\n");
        install(&t, false).unwrap();
        uninstall(&t, false).unwrap();
        assert_eq!(read(&t.file), "{\n  \"model\": \"opus\"\n}\n");
    }

    /// Plan decision P5: a text `serde_json` would not read back as the value meant is never
    /// written.
    #[test]
    fn a_file_serde_json_cannot_read_back_is_refused_untouched() {
        let m = Machine::new();
        let t = m.target(HookAgent::Claude);
        let big = "{\n  \"limit\": 1e400\n}\n";
        write(&t.file, big);
        let e = install(&t, false).unwrap_err().to_string();
        assert_eq!(
            e,
            format!(
                "Alice could not rewrite {} safely; it is unchanged",
                t.file.display()
            )
        );
        assert_eq!(read(&t.file), big);
        assert!(backups_of(&t.file).is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn backups_are_private_and_capped_at_five() {
        use std::os::unix::fs::PermissionsExt;
        let m = Machine::new();
        let t = m.target(HookAgent::Claude);
        write(&t.file, OWN);
        let mut made = Vec::new();
        let mut last_before = String::new();
        for i in 0..7 {
            last_before = read(&t.file);
            let c = if i % 2 == 0 {
                install(&t, false)
            } else {
                uninstall(&t, false)
            }
            .unwrap();
            made.push(c.backup.expect("every change makes a backup"));
        }
        let kept = backups_of(&t.file);
        assert_eq!(kept.len(), 5, "{kept:?}");
        for b in &kept {
            let mode = std::fs::metadata(b).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "{}", b.display());
        }
        // The newest five: the first two are gone.
        assert!(!made[0].exists() && !made[1].exists(), "{made:?}");
        assert!(made[2..].iter().all(|b| b.exists()), "{made:?}");
        assert_eq!(read(made.last().unwrap()), last_before);
        // `<file>.alice-backup-<yyyymmddThhmmssZ>`, with a counter when the second repeats.
        let name = made[0].file_name().unwrap().to_string_lossy().into_owned();
        let stamp = name.strip_prefix("settings.json.alice-backup-").unwrap();
        assert_eq!(stamp.len(), 16, "{name}");
        assert!(
            stamp.ends_with('Z') && stamp.as_bytes()[8] == b'T',
            "{name}"
        );
        // A file of another name in the folder is never pruned.
        let own = t.file.with_file_name("settings.json.alice-backup-mine");
        write(&own, "x");
        install(&t, false).unwrap();
        assert!(own.exists());
    }

    #[test]
    fn install_lists_other_memory_hooks() {
        let m = Machine::new();
        let t = m.target(HookAgent::Claude);
        write(
            &t.file,
            r#"{"hooks": {
  "SessionStart": [{"hooks": [{"type": "command", "command": "~/.local/bin/alice-session-start"}]}],
  "Stop": [
    {"hooks": [{"type": "command", "command": "afplay x.aiff"}]},
    {"hooks": [{"type": "command", "command": "/Users/alice/bin/notify.sh done"}]},
    {"hooks": [{"type": "command", "command": "cd /home/alice/src && make"}]},
    {"hooks": [{"type": "command", "command": "mempalace hook stop"}]}
  ],
  "PreCompact": [{"hooks": [{"type": "command", "command": "/x/alice hook pre-compact --agent claude-code"}]}],
  "SubagentStop": [{"hooks": [
    {"type": "command", "command": "python3 -m mempalace.hooks stop"},
    {"type": "command", "command": "/opt/Alice-tools/run.sh"},
    {"type": "command", "command": "\"/opt/x/ALICE Session.sh\" --quiet"}
  ]}]
}}"#,
        );
        let c = install(&t, false).unwrap();
        // A word whose last path component starts with alice or mempalace, whatever the case;
        // not a home directory named alice. Alice's own entry is replaced, not listed.
        // Each is named by its event and that word's file name, never by its whole command.
        assert_eq!(
            c.others,
            [
                "SessionStart: alice-session-start",
                "Stop: mempalace …",
                "SubagentStop: mempalace.hooks …",
                "SubagentStop: ALICE Session.sh …"
            ],
            "{c:?}"
        );
        assert_eq!(c.removed, 1);
        // They stay; the report names them.
        let text = read(&t.file);
        assert!(text.contains("alice-session-start") && text.contains("mempalace hook stop"));
        let report = install_report(&t, &c, false, "alice hooks uninstall claude");
        assert!(
            report.contains("  SessionStart: alice-session-start\n")
                && report.contains("  Stop: mempalace …\n"),
            "{report}"
        );
        assert!(report.contains("alice hooks uninstall claude"), "{report}");
        assert!(
            report.contains(&c.backup.as_ref().unwrap().display().to_string()),
            "{report}"
        );
        assert!(report.contains("new sessions"), "{report}");
    }

    /// Final review A-M6: the install's report (and its `--json`), which an agent running the
    /// install reads into its transcript, never holds another hook's whole command: a command
    /// with a key inline would put the key there.
    #[test]
    fn other_hooks_are_named_without_their_commands() {
        let m = Machine::new();
        let t = m.target(HookAgent::Claude);
        let token = concat!("gh", "p_aBcDeFgHiJkLmNoPqRsTuVwXyZ0123456789");
        write(
            &t.file,
            &format!(
                r#"{{"hooks": {{
  "SessionStart": [{{"hooks": [
    {{"type": "command", "command": "TOKEN={token} ~/bin/alice-session-start --url https://h/x"}},
    {{"type": "command", "command": "ALICE_TOKEN={token} /opt/notify.sh"}},
    {{"type": "command", "command": "/opt/mempalace-{token}/run"}}
  ]}}]
}}}}"#
            ),
        );
        let c = install(&t, true).unwrap();
        assert_eq!(
            c.others,
            [
                "SessionStart: alice-session-start …",
                "SessionStart: ALICE_TOKEN=… …",
            ],
            "{c:?}"
        );
        let report = install_report(&t, &c, true, "alice hooks uninstall claude");
        let json = serde_json::to_string(&c).unwrap();
        for out in [&report, &json] {
            assert!(
                !out.contains("aBcDeFgHiJ") && !out.contains("https://h/x"),
                "{out}"
            );
        }
        // A word that holds a secret itself is redacted.
        assert_eq!(
            other_label("Stop", &format!("alice-{token}")),
            "Stop: alice-[REDACTED:github_token]"
        );
    }

    #[test]
    fn dry_run_changes_nothing() {
        let m = Machine::new();
        let t = m.target(HookAgent::Claude);
        // A missing file is not made, nor its folder.
        let c = install(&t, true).unwrap();
        assert!(
            c.changed && c.added.len() == 5 && c.backup.is_none(),
            "{c:?}"
        );
        assert!(!t.file.parent().unwrap().exists());
        let report = install_report(&t, &c, true, "alice hooks uninstall claude");
        assert!(report.contains("nothing was written"), "{report}");
        // An existing file keeps its bytes and its time, and no backup is made; the text is
        // what a real install then writes.
        write(&t.file, OWN);
        age(&t.file);
        let before = mtime(&t.file);
        let dry = install(&t, true).unwrap();
        assert!(dry.changed && dry.backup.is_none(), "{dry:?}");
        assert_eq!(read(&t.file), OWN);
        assert_eq!(mtime(&t.file), before);
        assert!(backups_of(&t.file).is_empty());
        let real = install(&t, false).unwrap();
        assert_eq!((&dry.text, &dry.added), (&real.text, &real.added));
        // Uninstall too.
        age(&t.file);
        let before = mtime(&t.file);
        let c = uninstall(&t, true).unwrap();
        assert!(c.changed && c.removed == 5 && c.backup.is_none(), "{c:?}");
        assert_eq!(c.text, OWN);
        assert_eq!(read(&t.file), real.text);
        assert_eq!(mtime(&t.file), before);
        assert_eq!(backups_of(&t.file).len(), 1, "only the real install's");
        let report = uninstall_report(&t, &c, true);
        assert!(report.contains("nothing was written"), "{report}");
    }

    #[test]
    fn status_says_which_events_hold_alice_and_whether_its_program_exists() {
        let m = Machine::new();
        let t = m.target(HookAgent::Claude);
        let s = status(&t).unwrap();
        assert!(
            !s.installed && !s.binary_exists && s.events.is_empty(),
            "{s:?}"
        );
        install(&t, false).unwrap();
        let s = status(&t).unwrap();
        assert!(s.installed && s.binary_exists, "{s:?}");
        assert_eq!(
            s.events,
            CLAUDE_EVENTS.iter().map(|(e, _)| *e).collect::<Vec<_>>()
        );
        assert_eq!(s.binaries, [m.alice().display().to_string()]);
        assert!(s.missing.is_empty());
        let moved = HookTarget {
            alice: m.root.join("gone/alice"),
            ..t.clone()
        };
        install(&moved, false).unwrap();
        let s = status(&t).unwrap();
        assert!(s.installed && !s.binary_exists, "{s:?}");
        assert_eq!(s.missing, [m.root.join("gone/alice").display().to_string()]);
    }

    #[test]
    fn install_codex_writes_hooks_json_and_reminds_to_trust() {
        let m = Machine::new();
        let t = m.target(HookAgent::Codex);
        assert_eq!(t.file, m.home().join("codex-home/hooks.json"));
        let c = install(&t, false).unwrap();
        let v = value(&t.file);
        for (event, arg) in CLAUDE_EVENTS {
            let timeout = if *event == "SessionEnd" { 3 } else { 10 };
            assert_eq!(
                v["hooks"][event],
                serde_json::json!([{"hooks": [{
                    "type": "command",
                    "command": m.command(arg, "codex"),
                    "timeout": timeout
                }]}]),
                "{event}"
            );
        }
        let report = install_report(&t, &c, false, "alice hooks uninstall codex");
        assert!(report.ends_with(&format!("{CODEX_TRUST}\n")), "{report}");
        assert!(report.contains("open /hooks in Codex once and trust the Alice entries"));
        // A Claude Code install says nothing of it.
        let t = m.target(HookAgent::Claude);
        let c = install(&t, false).unwrap();
        assert!(!install_report(&t, &c, false, "x").contains("/hooks"));
    }

    #[test]
    fn install_codex_refuses_inline_config_hooks() {
        let m = Machine::new();
        let t = m.target(HookAgent::Codex);
        let config = t.codex_config.clone().unwrap();
        for inline in [
            "model = \"o3\"\n\n[hooks]\nfoo = 1\n",
            "[[hooks.Stop]]\ncommand = \"echo\"\n",
            "hooks.Stop = []\n",
        ] {
            write(&config, inline);
            // No hooks.json: none is made.
            let e = install(&t, false).unwrap_err().to_string();
            assert!(e.contains(&config.display().to_string()), "{inline}: {e}");
            assert!(e.contains("one format"), "{inline}: {e}");
            assert!(!t.file.exists());
            // An existing one is left as it is, with no backup.
            write(&t.file, "{}\n");
            age(&t.file);
            let before = mtime(&t.file);
            assert!(install(&t, false).is_err());
            assert!(install(&t, true).is_err());
            assert_eq!(read(&t.file), "{}\n");
            assert_eq!(mtime(&t.file), before);
            assert!(backups_of(&t.file).is_empty());
            std::fs::remove_file(&t.file).unwrap();
        }
        // A config.toml that is not TOML is refused by its line, never its text.
        write(&config, "model = \"o3\"\nkey = \"unfinished\n");
        let e = install(&t, false).unwrap_err().to_string();
        assert!(e.contains("line 2") && !e.contains("unfinished"), "{e}");
        // Hooks of a table inside another one are not Codex's hooks layer, nor is the
        // `[features] hooks = true` switch (the tenant's setup.sh has the same rule).
        write(
            &config,
            "model = \"o3\"\n[features]\nhooks = true\n[mcp_servers.alice]\ncommand = \"alice\"\nhooks = 1\n",
        );
        install(&t, false).unwrap();
        assert!(status(&t).unwrap().installed);
        // Uninstall never needs the check.
        write(&config, "[hooks]\n");
        assert_eq!(uninstall(&t, false).unwrap().removed, 5);
    }

    #[test]
    fn install_codex_is_idempotent_and_uninstall_removes_exactly_its_entries() {
        let m = Machine::new();
        let t = m.target(HookAgent::Codex);
        let own = "{\n  \"hooks\": {\n    \"Stop\": [\n      {\n        \"hooks\": [\n          {\n            \"type\": \"command\",\n            \"command\": \"notify-send done\"\n          }\n        ]\n      }\n    ]\n  }\n}\n";
        write(&t.file, own);
        install(&t, false).unwrap();
        let first = read(&t.file);
        let again = install(&t, false).unwrap();
        assert!(!again.changed && again.backup.is_none(), "{again:?}");
        assert_eq!(read(&t.file), first);
        let c = uninstall(&t, false).unwrap();
        assert_eq!(c.removed, 5);
        assert_eq!(read(&t.file), own);
        let report = uninstall_report(&t, &c, false);
        assert!(
            report.contains("5") && report.contains(&t.file.display().to_string()),
            "{report}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_settings_file_stays_a_link() {
        let m = Machine::new();
        let t = m.target(HookAgent::Claude);
        let real = m.root.join("dotfiles/settings.json");
        write(&real, OWN);
        std::fs::create_dir_all(t.file.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&real, &t.file).unwrap();
        let c = install(&t, false).unwrap();
        assert!(
            std::fs::symlink_metadata(&t.file)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(read(&real), c.text);
        // The backup is next to the link, not in the folder the link points into.
        assert_eq!(c.backup.unwrap().parent(), t.file.parent());
    }

    #[test]
    fn shell_quote_survives_a_single_quote() {
        assert_eq!(shell_quote("/a b/c"), "'/a b/c'");
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
        assert_eq!(
            alice_program(
                &format!(
                    "{} hook stop --agent claude-code",
                    shell_quote("/it's/alice")
                ),
                HookAgent::Claude
            )
            .as_deref(),
            Some("/it's/alice")
        );
    }
}
