//! Alice's capture hooks in Codex's `config.toml`, for a `config.toml` that holds hooks of its own
//! (`[[hooks.<Event>]]` tables, such as OpenRig's): Codex reads hooks from one format per layer,
//! so they go there, not into a `hooks.json` beside it.
//!
//! - **The block.** The five hooks are `[[hooks.<Event>]]` / `[[hooks.<Event>.hooks]]` tables
//!   between two marker lines, [`BEGIN`] and [`END`], appended at the end of the file. Nothing
//!   outside the block is ever changed, apart from a missing last newline, which is added.
//! - **Idempotent.** A block whose own lines are already what Alice writes is left as it is, byte
//!   for byte (the person's trust in it stays). Another one (an older version, a moved binary) is
//!   replaced in place, so the indices of the person's hooks, which Codex keys its trust by, stay.
//! - **Other lines inside the block** (Codex may put its `[hooks.state."…"]` trust records
//!   there) are not Alice's: a replace moves them just below the block, a removal leaves them.
//! - **Checked before it is written.** The result must read back as TOML (`toml` and
//!   `toml_edit`), mean exactly what the file meant apart from Alice's hooks, and hold exactly
//!   Alice's five (none after a removal). Otherwise the edit is refused with a reason that never
//!   quotes a line of the file.
//!
//! The tenant's `hook.sh` (`codex_config_py`) keeps the same rules for its own block.

use std::collections::BTreeMap;

/// The first line of Alice's block.
pub const BEGIN: &str = "# >>> alice capture (Codex) >>>";
/// The last line of Alice's block.
pub const END: &str = "# <<< alice capture (Codex) <<<";

/// The events a block's tables may be for.
const EVENTS: &[&str] = &[
    "SessionStart",
    "Stop",
    "SubagentStop",
    "PreCompact",
    "SessionEnd",
];

/// One hook of the block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub event: &'static str,
    pub command: String,
    pub timeout: u32,
}

/// What an edit comes to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Edit {
    /// The file's new text; `removed` counts the Alice hooks of the block it had.
    Changed { text: String, removed: usize },
    /// The block is already what Alice writes; `removed` counts its hooks, as a reinstall
    /// replaces them by the same ones.
    Unchanged { removed: usize },
    /// The file has no block of Alice's.
    Absent,
    /// Alice's entries are in the file outside a block (added by hand); `same` when they are
    /// exactly the five the block would hold.
    Outside { same: bool },
}

/// Alice's block: a comment line (`hint`), then one group per entry, between the markers.
pub fn block(hint: &str, entries: &[Entry]) -> String {
    let mut s = format!("{BEGIN}\n# {hint}\n");
    for (i, e) in entries.iter().enumerate() {
        if i > 0 {
            s.push('\n');
        }
        s.push_str(&format!(
            "[[hooks.{ev}]]\n[[hooks.{ev}.hooks]]\ntype = \"command\"\ncommand = {}\ntimeout = {}\n",
            toml_string(&e.command),
            e.timeout,
            ev = e.event
        ));
    }
    s.push_str(END);
    s.push('\n');
    s
}

/// `s` as a TOML basic string.
fn toml_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            c if c.is_control() => out.push_str(&format!("\\u{:04X}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// `text` read as TOML, or why not: `what` "is not valid TOML", with the line, never its text.
pub fn parse(text: &str, what: &str) -> Result<toml::Table, String> {
    toml::from_str::<toml::Table>(text).map_err(|e| {
        let line = e
            .span()
            .map(|s| format!(" (line {})", alice_core::settings::toml_line(text, s.start)))
            .unwrap_or_default();
        format!("{what} is not valid TOML{line}")
    })
}

/// Whether a TOML file holds hooks of its own: a top-level `hooks` key (`[hooks]` tables,
/// `[[hooks.<Event>]]` arrays, `hooks.<Event> = …` before the first table). `[features] hooks =
/// true` is none.
pub fn has_hooks(table: &toml::Table) -> bool {
    table.contains_key("hooks")
}

/// Every hook command of the file, with its event, in file order.
pub fn commands(table: &toml::Table) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let Some(toml::Value::Table(events)) = table.get("hooks") else {
        return out;
    };
    for (event, groups) in events {
        let toml::Value::Array(groups) = groups else {
            continue;
        };
        for group in groups {
            let Some(toml::Value::Array(hooks)) = group.get("hooks") else {
                continue;
            };
            for hook in hooks {
                if let Some(c) = hook.get("command").and_then(toml::Value::as_str) {
                    out.push((event.clone(), c.to_owned()));
                }
            }
        }
    }
    out
}

/// Puts `block` in the file `text`: appended when there is none, replaced when it differs.
pub fn install(text: &str, block: &str, is_alice: &dyn Fn(&str) -> bool) -> Result<Edit, String> {
    let old = parse(text, "it")?;
    let want = split(&parse(block, "Alice's block")?, is_alice).1;
    match rebuild(text, block, is_alice)? {
        None => {
            let (_, mine) = split(&old, is_alice);
            if !mine.is_empty() {
                return Ok(Edit::Outside { same: mine == want });
            }
            let sep = if text.is_empty() || text.ends_with('\n') {
                ""
            } else {
                "\n"
            };
            let new = format!("{text}{sep}{block}");
            verify(&old, &new, &want, is_alice)?;
            Ok(Edit::Changed {
                text: new,
                removed: 0,
            })
        }
        Some(r) if r.intact => Ok(Edit::Unchanged { removed: r.alice }),
        Some(r) => {
            verify(&old, &r.text, &want, is_alice)?;
            Ok(Edit::Changed {
                text: r.text,
                removed: r.alice,
            })
        }
    }
}

/// Takes Alice's block out of the file `text`. A file without one is not read as TOML.
pub fn uninstall(text: &str, is_alice: &dyn Fn(&str) -> bool) -> Result<Edit, String> {
    let Some(r) = rebuild(text, "", is_alice)? else {
        return Ok(Edit::Absent);
    };
    let old = parse(text, "it")?;
    verify(&old, &r.text, &BTreeMap::new(), is_alice)?;
    Ok(Edit::Changed {
        text: r.text,
        removed: r.alice,
    })
}

/// A file's text with its block replaced.
struct Rebuilt {
    /// The text with `block` in place of the old block, and the block's other lines below it.
    text: String,
    /// The Alice hooks the old block held.
    alice: usize,
    /// The old block's own lines are `block`'s, blank lines aside.
    intact: bool,
}

/// The lines of `text`, each with its `\n`.
fn lines_of(text: &str) -> Vec<&str> {
    text.split_inclusive('\n').collect()
}

fn blank_or_comment(line: &str) -> bool {
    let s = line.trim();
    s.is_empty() || s.starts_with('#')
}

/// `[[hooks.<Event>]]` (`Some((event, false))`) or `[[hooks.<Event>.hooks]]` (`Some((event,
/// true))`) for one of the five events, spaces and a trailing comment allowed.
fn header(line: &str) -> Option<(&str, bool)> {
    let inner = line.trim().strip_prefix("[[")?;
    let (keys, rest) = inner.split_once("]]")?;
    let rest = rest.trim();
    if !(rest.is_empty() || rest.starts_with('#')) {
        return None;
    }
    let keys: Vec<&str> = keys.split('.').map(str::trim).collect();
    match keys.as_slice() {
        ["hooks", event] if EVENTS.contains(event) => Some((event, false)),
        ["hooks", event, "hooks"] if EVENTS.contains(event) => Some((event, true)),
        _ => None,
    }
}

/// A hook of Alice's: only `type`, `command` and `timeout`, and a command of Alice's.
fn alice_hook(t: &toml::Table, is_alice: &dyn Fn(&str) -> bool) -> bool {
    t.keys()
        .all(|k| matches!(k.as_str(), "type" | "command" | "timeout"))
        && t.get("command")
            .and_then(toml::Value::as_str)
            .is_some_and(is_alice)
}

/// A group of Alice's: nothing but `hooks`, holding one hook of Alice's.
fn alice_group(g: &toml::Value, is_alice: &dyn Fn(&str) -> bool) -> bool {
    let Some(g) = g.as_table() else {
        return false;
    };
    match g.get("hooks") {
        Some(toml::Value::Array(hooks)) if g.len() == 1 && hooks.len() == 1 => {
            hooks[0].as_table().is_some_and(|h| alice_hook(h, is_alice))
        }
        _ => false,
    }
}

/// A table of the block (its header line and the lines up to the next header) that is Alice's:
/// a group header with no keys, or a hook table of Alice's.
fn alice_chunk(chunk: &[&str], is_alice: &dyn Fn(&str) -> bool) -> bool {
    let Some((_, hook)) = chunk.first().and_then(|l| header(l)) else {
        return false;
    };
    let body = &chunk[1..];
    if !hook {
        return body.iter().all(|l| blank_or_comment(l));
    }
    toml::from_str::<toml::Table>(&body.concat()).is_ok_and(|t| alice_hook(&t, is_alice))
}

/// `None` when `text` has no block; otherwise the text with `block` (empty: none) in its place.
fn rebuild(
    text: &str,
    block: &str,
    is_alice: &dyn Fn(&str) -> bool,
) -> Result<Option<Rebuilt>, String> {
    let lines = lines_of(text);
    let at = |m: &str| -> Vec<usize> {
        lines
            .iter()
            .enumerate()
            .filter(|(_, l)| l.trim_end() == m)
            .map(|(i, _)| i)
            .collect()
    };
    let (begins, ends) = (at(BEGIN), at(END));
    let (b, e) = match (begins.as_slice(), ends.as_slice()) {
        ([], []) => return Ok(None),
        ([b], [e]) if b < e => (*b, *e),
        _ => {
            return Err(
                "its Alice block is broken (a marker line is missing, doubled or out of order)"
                    .into(),
            );
        }
    };
    let mut parts: Vec<Vec<&str>> = vec![Vec::new()];
    for l in &lines[b + 1..e] {
        if l.trim_start().starts_with('[') {
            parts.push(vec![l]);
        } else if let Some(last) = parts.last_mut() {
            last.push(l);
        }
    }
    let lead = &parts[0];
    if !lead.iter().all(|l| blank_or_comment(l)) {
        return Err("a key of another table sits inside its Alice block".into());
    }
    let (mut mine, mut other, mut alice) = (String::new(), String::new(), 0);
    for c in &parts[1..] {
        if alice_chunk(c, is_alice) {
            mine.push_str(&c.concat());
            if header(c[0]).is_some_and(|(_, hook)| hook) {
                alice += 1;
            }
        } else {
            other.push_str(&c.concat());
        }
    }
    let current = format!("{}{}{mine}{}", lines[b], lead.concat(), lines[e]);
    // Blank lines mean nothing: a block that differs only in them is intact.
    let solid = |s: &str| -> Vec<String> {
        s.lines()
            .filter(|l| !l.trim().is_empty())
            .map(String::from)
            .collect()
    };
    Ok(Some(Rebuilt {
        text: format!(
            "{}{block}{other}{}",
            lines[..b].concat(),
            lines[e + 1..].concat()
        ),
        alice,
        intact: !block.is_empty() && solid(&current) == solid(block),
    }))
}

/// The file's value without Alice's groups (and without the hook lists and `hooks` table left
/// empty, on both sides of a comparison alike), and Alice's groups by event.
fn split(
    v: &toml::Table,
    is_alice: &dyn Fn(&str) -> bool,
) -> (toml::Table, BTreeMap<String, Vec<toml::Value>>) {
    let mut rest = v.clone();
    let mut mine = BTreeMap::new();
    if let Some(toml::Value::Table(hooks)) = v.get("hooks") {
        let mut kept = toml::Table::new();
        for (event, groups) in hooks {
            let toml::Value::Array(groups) = groups else {
                kept.insert(event.clone(), groups.clone());
                continue;
            };
            let (own, other): (Vec<_>, Vec<_>) = groups
                .iter()
                .cloned()
                .partition(|g| alice_group(g, is_alice));
            if !own.is_empty() {
                mine.insert(event.clone(), own);
            }
            if !other.is_empty() {
                kept.insert(event.clone(), toml::Value::Array(other));
            }
        }
        if kept.is_empty() {
            rest.remove("hooks");
        } else {
            rest.insert("hooks".into(), toml::Value::Table(kept));
        }
    }
    (rest, mine)
}

/// The new text reads back as TOML (as Codex reads it, and as it edits it), means what `old`
/// meant apart from Alice's groups, and holds exactly `want` of them.
fn verify(
    old: &toml::Table,
    new_text: &str,
    want: &BTreeMap<String, Vec<toml::Value>>,
    is_alice: &dyn Fn(&str) -> bool,
) -> Result<(), String> {
    let new = parse(new_text, "the result")?;
    new_text
        .parse::<toml_edit::DocumentMut>()
        .map_err(|_| "the result is not valid TOML".to_string())?;
    let (old_rest, _) = split(old, is_alice);
    let (new_rest, mine) = split(&new, is_alice);
    if new_rest != old_rest {
        return Err("the edit would change more than Alice's hooks".into());
    }
    if mine != *want {
        return Err(
            "Alice's hooks would not come out as written (are some of Alice's entries outside its marked block?)"
                .into(),
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const CMD: &str = "'/bin/alice' hook stop --agent codex || true";

    fn is_alice(c: &str) -> bool {
        c.contains("/bin/alice' hook") && c.ends_with("--agent codex || true")
    }

    fn entries(program: &str) -> Vec<Entry> {
        [
            ("SessionStart", "session-start", 10),
            ("Stop", "stop", 10),
            ("SubagentStop", "subagent-stop", 10),
            ("PreCompact", "pre-compact", 10),
            ("SessionEnd", "session-end", 3),
        ]
        .into_iter()
        .map(|(event, arg, timeout)| Entry {
            event,
            command: format!("'{program}' hook {arg} --agent codex || true"),
            timeout,
        })
        .collect()
    }

    fn the_block() -> String {
        block("Alice capture hooks.", &entries("/bin/alice"))
    }

    /// A config.toml as OpenRig leaves it: its marked block of `[[hooks.<Event>]]` tables, the
    /// features switch, an MCP server and a trust record.
    const OPENRIG: &str = r#"model = "gpt-5"

[features]
hooks = true

[mcp_servers.exa]
command = "npx"
args = ["-y", "exa-mcp"]

# BEGIN OPENRIG MANAGED ACTIVITY HOOKS
[[hooks.SessionStart]]
[[hooks.SessionStart.hooks]]
type = "command"
command = 'node "/opt/openrig/relay.js"'
timeout = 5

[[hooks.Stop]]
[[hooks.Stop.hooks]]
type = "command"
command = 'node "/opt/openrig/relay.js"'
timeout = 5
# END OPENRIG MANAGED ACTIVITY HOOKS

[hooks.state."/home/u/.codex/config.toml:session_start:0:0"]
trusted_hash = "sha256:abc"
"#;

    fn changed(e: Edit) -> String {
        match e {
            Edit::Changed { text, .. } => text,
            other => panic!("not changed: {other:?}"),
        }
    }

    #[test]
    fn the_block_is_valid_toml_holding_the_five_hooks() {
        let b = the_block();
        assert!(b.starts_with(&format!("{BEGIN}\n# Alice capture hooks.\n")));
        assert!(b.ends_with(&format!("timeout = 3\n{END}\n")), "{b}");
        let t = parse(&b, "it").unwrap();
        let (rest, mine) = split(&t, &is_alice);
        assert!(rest.is_empty(), "{rest:?}");
        assert_eq!(mine.len(), 5);
        assert_eq!(
            mine["Stop"][0]["hooks"][0]["command"].as_str(),
            Some("'/bin/alice' hook stop --agent codex || true")
        );
        assert_eq!(
            mine["SessionEnd"][0]["hooks"][0]["timeout"].as_integer(),
            Some(3)
        );
        // A command with quotes and backslashes survives the string's escapes.
        let odd = r#"ALICE_HOME='/a "b"\c' '/bin/alice' hook stop --agent codex || true"#;
        let b = block(
            "x",
            &[Entry {
                event: "Stop",
                command: odd.into(),
                timeout: 10,
            }],
        );
        let t = parse(&b, "it").unwrap();
        assert_eq!(commands(&t), [("Stop".to_string(), odd.to_string())]);
    }

    #[test]
    fn install_appends_the_block_and_leaves_every_other_byte() {
        let new = changed(install(OPENRIG, &the_block(), &is_alice).unwrap());
        assert_eq!(new, format!("{OPENRIG}{}", the_block()));
        // The person's hooks keep their place: Alice's are the second group of an event they
        // share (Codex keys its trust by those indices).
        let t = parse(&new, "it").unwrap();
        let stop = t["hooks"]["Stop"].as_array().unwrap();
        assert_eq!(stop.len(), 2);
        assert_eq!(
            stop[0]["hooks"][0]["command"].as_str(),
            Some(r#"node "/opt/openrig/relay.js""#)
        );
        assert!(is_alice(stop[1]["hooks"][0]["command"].as_str().unwrap()));
        assert!(t["hooks"]["state"].is_table());
        // A last line without its newline gets one.
        let bare = "[[hooks.Stop]]\nhooks = []";
        let new = changed(install(bare, &the_block(), &is_alice).unwrap());
        assert_eq!(new, format!("{bare}\n{}", the_block()));
    }

    #[test]
    fn install_is_idempotent_even_with_codex_s_trust_records_inside_the_block() {
        let once = changed(install(OPENRIG, &the_block(), &is_alice).unwrap());
        assert_eq!(
            install(&once, &the_block(), &is_alice).unwrap(),
            Edit::Unchanged { removed: 5 }
        );
        // Codex writes its trust records where its TOML editor puts them, maybe before END.
        let trusted = once.replace(
            &format!("{END}\n"),
            &format!(
                "\n[hooks.state.\"/home/u/.codex/config.toml:session_end:0:0\"]\ntrusted_hash = \"sha256:def\"\n{END}\n"
            ),
        );
        assert_eq!(
            install(&trusted, &the_block(), &is_alice).unwrap(),
            Edit::Unchanged { removed: 5 }
        );
        // A removal leaves the records where they are (the blank line above one goes with the
        // Alice table it follows).
        let gone = changed(uninstall(&trusted, &is_alice).unwrap());
        assert_eq!(
            gone,
            format!(
                "{OPENRIG}[hooks.state.\"/home/u/.codex/config.toml:session_end:0:0\"]\ntrusted_hash = \"sha256:def\"\n"
            )
        );
        // A block that differs from Alice's only in blank lines is Alice's as it is.
        let airy = once.replace("timeout = 10\n\n", "timeout = 10\n\n\n");
        assert_ne!(airy, once);
        assert_eq!(
            install(&airy, &the_block(), &is_alice).unwrap(),
            Edit::Unchanged { removed: 5 }
        );
    }

    #[test]
    fn uninstall_restores_the_original_bytes() {
        let once = changed(install(OPENRIG, &the_block(), &is_alice).unwrap());
        assert_eq!(
            uninstall(&once, &is_alice).unwrap(),
            Edit::Changed {
                text: OPENRIG.into(),
                removed: 5
            }
        );
        assert_eq!(uninstall(OPENRIG, &is_alice).unwrap(), Edit::Absent);
        // A file without a block is never read as TOML.
        assert_eq!(uninstall("not = [toml", &is_alice).unwrap(), Edit::Absent);
    }

    #[test]
    fn a_changed_block_is_replaced_in_place() {
        let old = block("Alice capture hooks.", &entries("/old/bin/alice"));
        let old_is_alice = |c: &str| c.contains("bin/alice' hook");
        let mut text = format!("{OPENRIG}{old}");
        text.push_str(
            "[[hooks.Stop]]\n[[hooks.Stop.hooks]]\ntype = \"command\"\ncommand = \"later\"\n",
        );
        let r = install(&text, &the_block(), &old_is_alice).unwrap();
        let Edit::Changed { text: new, removed } = r else {
            panic!("{r:?}");
        };
        assert_eq!(removed, 5);
        // In place: the hook added after the block keeps its place after Alice's.
        assert_eq!(
            new,
            format!(
                "{OPENRIG}{}[[hooks.Stop]]\n[[hooks.Stop.hooks]]\ntype = \"command\"\ncommand = \"later\"\n",
                the_block()
            )
        );
    }

    #[test]
    fn an_edit_that_would_not_read_back_as_meant_is_refused() {
        for (text, why) in [
            // A static array cannot take [[hooks.Stop]].
            ("hooks.Stop = []\n", "not valid TOML"),
            ("[hooks]\nStop = [{ hooks = [] }]\n", "not valid TOML"),
            // [hooks.Stop] is a table, not a list.
            ("[hooks.Stop]\nx = 1\n", "not valid TOML"),
            ("model = \"x\n", "it is not valid TOML (line 1)"),
        ] {
            let e = install(text, &the_block(), &is_alice).unwrap_err();
            assert!(e.contains(why), "{text:?}: {e}");
            assert!(!e.contains("Stop = "), "never a line of the file: {e}");
        }
        // A key of the table before the block, moved inside it, would change tables.
        let once = changed(install(OPENRIG, &the_block(), &is_alice).unwrap());
        let moved = once.replace(&format!("{BEGIN}\n"), &format!("{BEGIN}\nx = 1\n"));
        assert!(install(&moved, &the_block(), &is_alice).is_err());
        assert!(uninstall(&moved, &is_alice).is_err());
        // A key Codex (or the person) added to one of Alice's tables: that table is not
        // Alice's any more, and taking the rest away would hand it to another group.
        let edited = once.replacen("timeout = 3\n", "timeout = 3\nstatusMessage = \"x\"\n", 1);
        let e = uninstall(&edited, &is_alice).unwrap_err();
        assert!(e.contains("more than Alice's hooks"), "{e}");
        // A broken block.
        let half = once.replace(&format!("{END}\n"), "");
        let e = uninstall(&half, &is_alice).unwrap_err();
        assert!(e.contains("broken"), "{e}");
    }

    #[test]
    fn entries_outside_a_block_are_left_to_the_person() {
        let mut by_hand = OPENRIG.to_string();
        by_hand.push_str(
            &the_block()
                .lines()
                .filter(|l| !l.starts_with('#'))
                .map(|l| format!("{l}\n"))
                .collect::<String>(),
        );
        assert_eq!(
            install(&by_hand, &the_block(), &is_alice).unwrap(),
            Edit::Outside { same: true }
        );
        let one = format!(
            "{OPENRIG}[[hooks.Stop]]\n[[hooks.Stop.hooks]]\ntype = \"command\"\ncommand = {}\n",
            toml_string(CMD)
        );
        assert_eq!(
            install(&one, &the_block(), &is_alice).unwrap(),
            Edit::Outside { same: false }
        );
        // With a block too, a reinstall cannot leave exactly five: refused.
        let both = format!("{by_hand}{}", block("old", &entries("/old/bin/alice")));
        let e = install(&both, &the_block(), &|c: &str| {
            c.contains("bin/alice' hook")
        })
        .unwrap_err();
        assert!(e.contains("outside its marked block"), "{e}");
    }

    #[test]
    fn header_reads_every_spacing_of_the_five_events_only() {
        assert_eq!(header("[[hooks.Stop]]\n"), Some(("Stop", false)));
        assert_eq!(
            header("  [[ hooks . Stop . hooks ]] # x\n"),
            Some(("Stop", true))
        );
        assert_eq!(header("[[hooks.UserPromptSubmit]]"), None);
        assert_eq!(header("[hooks.Stop]"), None);
        assert_eq!(header("[[hooks.Stop]] x = 1"), None);
        assert_eq!(header("[[hooks.state]]"), None);
    }
}
