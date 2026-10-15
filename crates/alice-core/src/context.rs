//! Token-budgeted briefing for a project: what an agent should know at the start of a session.

use crate::error::Result;
use crate::model::Memory;
use crate::normalize::{estimate_tokens, truncate_chars};
use crate::store::memories::get_by_seqs;
use crate::store::projects::{GLOBAL_ID, ResolvedProject};
use chrono::{DateTime, Duration, SecondsFormat, Utc};
use rusqlite::{Connection, ToSql};

/// Memories an agent should see. New memories stay `pending` until the embedding worker
/// activates them, so they count as live.
const LIVE: &str = "status IN ('active','pending')";
/// Longest detail, and longest headline taken from the body, of a one-line entry in characters.
/// A title is never cut.
const LINE_CHARS: usize = 120;
/// Longest handoff text in characters.
const HANDOFF_CHARS: usize = 600;
/// Decisions created within this many days are recent.
const RECENT_DAYS: i64 = 14;

/// What a section selects and how it is ordered; see `section_sql`.
type Select = (&'static str, &'static str);
const BY_IMPORTANCE: &str = "importance DESC, updated_at DESC";
const LAST_HANDOFF: Select = ("kind = 'handoff'", "created_at DESC");
const WARNINGS: Select = ("kind = 'warning'", BY_IMPORTANCE);
/// Keep `importance >= 4` literal: the partial index `memories_project_importance` applies only then.
const KEY_MEMORIES: Select = (
    "importance >= 4 AND kind NOT IN ('handoff','diary')",
    BY_IMPORTANCE,
);
const RECENT_DECISIONS: Select = ("kind = 'decision' AND created_at >= ?2", "created_at DESC");
const PREFERENCES: Select = ("kind = 'preference'", BY_IMPORTANCE);
/// Live diary entries except auto-save checkpoints (a title ending `: checkpoint`, or the topic).
const RECENT_DIARY: Select = (
    "kind = 'diary' AND title NOT LIKE '%: checkpoint' AND NOT EXISTS \
     (SELECT 1 FROM memory_topics t WHERE t.memory_id = memories.id AND t.topic = 'checkpoint')",
    "created_at DESC",
);
/// More open review items than this are not worth asking the user about at session start.
const REVIEW_ASK_MAX: u64 = 10;

/// A section's query: `seq` of live memories in display order, `?1` the project, the row limit
/// bound last. `seq DESC` breaks ties, so the newest of equals wins and the output is stable.
fn section_sql((filter, order): Select) -> String {
    format!(
        "SELECT seq FROM memories WHERE project_id = ?1 AND {filter} AND {LIVE} ORDER BY {order}, seq DESC LIMIT ?"
    )
}

fn any_live_sql() -> String {
    format!("SELECT EXISTS(SELECT 1 FROM memories WHERE project_id = ?1 AND {LIVE})")
}

/// The body's first line and the text after it, both trimmed.
fn split_first_line(body: &str) -> (&str, &str) {
    let body = body.trim();
    let (first, rest) = body.split_once('\n').unwrap_or((body, ""));
    (first.trim(), rest.trim())
}

/// `- [kind] headline — detail (id)`.
///
/// When the body's first line starts with the title (a title derived from that line, cut at 80
/// characters and maybe mid-word, or a short label for it), the line itself is the headline and
/// the next line is the detail, so nothing is cut unmarked or shown twice. Otherwise the title
/// is the headline and the body's first line is the detail.
fn line(m: &Memory) -> String {
    let (first, rest) = split_first_line(&m.body);
    let (head, detail) = if first.starts_with(m.title.as_str()) {
        // Cut the line, never the title within it.
        let cap = LINE_CHARS.max(m.title.chars().count());
        (
            truncate_chars(first, cap),
            rest.lines().next().unwrap_or(""),
        )
    } else {
        (m.title.clone(), first)
    };
    let detail = match detail.trim() {
        "" => String::new(),
        d => format!(" — {}", truncate_chars(d, LINE_CHARS)),
    };
    format!("- [{}] {head}{detail} ({})\n", m.kind.as_str(), m.id)
}

/// The latest handoff in full (up to 600 characters): date, author and id on one line, then
/// the text, preceded by the title unless the text already starts with it.
fn handoff(m: &Memory) -> String {
    // Imported memories keep their original timestamp, which need not be RFC 3339.
    let date: String = m.created_at.chars().take(10).collect();
    let by = match m.author.agent.as_deref() {
        Some(agent) if !agent.is_empty() => format!(" · {agent}"),
        _ => String::new(),
    };
    let body = m.body.trim();
    let text = if split_first_line(body).0.starts_with(m.title.as_str()) {
        truncate_chars(body, HANDOFF_CHARS)
    } else {
        truncate_chars(&format!("{}\n{body}", m.title), HANDOFF_CHARS)
    };
    format!("{date}{by} ({})\n{text}\n", m.id)
}

struct Briefing<'c> {
    conn: &'c Connection,
    max_tokens: usize,
    out: String,
    /// Tokens in `out`, counted piece by piece; never less than `estimate_tokens(&out)`.
    used: usize,
    shown: Vec<i64>,
}

impl Briefing<'_> {
    /// Append a section with up to `cap` memories that are not shown yet, as many as fit.
    /// `args` bind the query's parameters before the row limit.
    fn section(
        &mut self,
        title: &str,
        select: Select,
        args: &[&dyn ToSql],
        cap: usize,
        render: fn(&Memory) -> String,
    ) -> Result<()> {
        let heading = format!("\n## {title}\n");
        let mut cost = estimate_tokens(&heading);
        if self.used + cost > self.max_tokens {
            return Ok(());
        }
        // Rows already shown in an earlier section are skipped, so fetch that many more.
        let limit = i64::try_from(cap + self.shown.len()).unwrap_or(i64::MAX);
        let mut params = args.to_vec();
        params.push(&limit);
        let mut stmt = self.conn.prepare_cached(&section_sql(select))?;
        let found: Vec<i64> = stmt
            .query_map(params.as_slice(), |r| r.get(0))?
            .collect::<std::result::Result<_, _>>()?;
        let fresh: Vec<i64> = found
            .into_iter()
            .filter(|s| !self.shown.contains(s))
            .take(cap)
            .collect();
        let mut body = String::new();
        for (seq, m) in get_by_seqs(self.conn, &fresh)? {
            let entry = render(&m);
            let entry_cost = estimate_tokens(&entry);
            if self.used + cost + entry_cost > self.max_tokens {
                break;
            }
            cost += entry_cost;
            body.push_str(&entry);
            self.shown.push(seq);
        }
        if !body.is_empty() {
            self.out.push_str(&heading);
            self.out.push_str(&body);
            self.used += cost;
        }
        Ok(())
    }

    /// Append a section of plain text, if it fits the budget.
    fn text_section(&mut self, title: &str, text: &str) {
        let section = format!("\n## {title}\n{text}\n");
        let cost = estimate_tokens(&section);
        if self.used + cost <= self.max_tokens {
            self.out.push_str(&section);
            self.used += cost;
        }
    }
}

/// Briefing for `project`: the last handoff in full, then warnings, key memories (importance 4
/// and up), decisions from the last 14 days and the global preferences, one line each, no
/// memory twice, then the five newest diary entries that are not auto-save checkpoints. The whole text stays within `max_tokens` (as `estimate_tokens` counts), except
/// that the heading line, and for a project without live memories a hint, are always included.
/// When review items wait for a decision, a last section says how many (and, up to ten, to ask
/// the user).
pub fn build(
    conn: &Connection,
    project: &ResolvedProject,
    max_tokens: usize,
    now: DateTime<Utc>,
) -> Result<String> {
    let pid = project.id.as_str();
    let is_global = pid == GLOBAL_ID;
    let mut out = format!("# Context · {}\n", project.slug);
    if !is_global {
        let any_live: bool = conn
            .prepare_cached(&any_live_sql())?
            .query_row([pid], |r| r.get(0))?;
        if !any_live {
            out.push_str(&format!(
                "No memories yet for {}. Save decisions, fixes and lessons with remember; use recall to search everything.\n",
                project.slug
            ));
        }
    }
    let mut b = Briefing {
        conn,
        max_tokens,
        used: estimate_tokens(&out),
        out,
        shown: Vec::new(),
    };
    let since = (now - Duration::days(RECENT_DAYS)).to_rfc3339_opts(SecondsFormat::Millis, true);
    b.section("Last handoff", LAST_HANDOFF, &[&pid], 1, handoff)?;
    b.section("Warnings", WARNINGS, &[&pid], 5, line)?;
    b.section("Key memories", KEY_MEMORIES, &[&pid], 8, line)?;
    b.section(
        "Recent decisions",
        RECENT_DECISIONS,
        &[&pid, &since],
        5,
        line,
    )?;
    if !is_global {
        b.section("Your preferences", PREFERENCES, &[&GLOBAL_ID], 8, line)?;
    }
    b.section("Recent diary", RECENT_DIARY, &[&pid], 5, line)?;
    let open = crate::store::reviews::count_open(conn)?;
    if open > REVIEW_ASK_MAX {
        b.text_section(
            "Review",
            &format!("{open} decisions wait in `alice review` (handle them when convenient)."),
        );
    } else if open > 0 {
        b.text_section(
            "Review",
            &format!(
                "{open} item(s) wait for a decision (duplicates, updates or contradictions Alice was not sure about). Run `alice review`, or ask the user."
            ),
        );
    }
    Ok(b.out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Author, Kind, Status};
    use crate::normalize::estimate_tokens;
    use crate::project::ProjectRef;
    use crate::store::{memories, open_writer, projects};

    fn add(
        c: &rusqlite::Connection,
        p: &projects::ResolvedProject,
        kind: Kind,
        imp: u8,
        text: &str,
    ) {
        let m = memories::prepare(
            memories::NewMemoryInput {
                content: text.into(),
                kind: Some(kind),
                importance: Some(imp),
                ..Default::default()
            },
            p,
            "h".into(),
        )
        .unwrap();
        memories::insert(c, &m).unwrap();
    }

    #[test]
    fn briefing_has_sections_in_order_and_respects_budget() {
        let d = tempfile::tempdir().unwrap();
        let c = open_writer(&d.path().join("a.db")).unwrap();
        let app = projects::upsert_project(
            &c,
            &ProjectRef {
                key: "k".into(),
                slug: "app".into(),
                name: "app".into(),
                root: None,
                remote: None,
            },
        )
        .unwrap();
        let global = projects::upsert_project(&c, &ProjectRef::global()).unwrap();
        add(
            &c,
            &app,
            Kind::Handoff,
            3,
            "Handoff: M1 tasks 1-5 done\nnext: vector index",
        );
        add(&c, &app, Kind::Warning, 4, "Never commit alice.db to git");
        add(&c, &app, Kind::Decision, 5, "Rust single binary");
        add(&c, &app, Kind::Decision, 3, "FTS trigram for Thai");
        add(&c, &global, Kind::Preference, 4, "Reply in Thai");
        add(&c, &app, Kind::Note, 1, "trivial note");
        let text = build(&c, &app, 1_200, chrono::Utc::now()).unwrap();
        let pos = |s: &str| {
            text.find(s)
                .unwrap_or_else(|| panic!("missing {s}:\n{text}"))
        };
        assert!(pos("Last handoff") < pos("Warnings"));
        assert!(pos("Warnings") < pos("Key memories"));
        assert!(pos("Key memories") < pos("Recent decisions"));
        assert!(pos("Recent decisions") < pos("Your preferences"));
        assert!(text.contains("Reply in Thai"));
        assert!(!text.contains("trivial note"));
        let small = build(&c, &app, 60, chrono::Utc::now()).unwrap();
        assert!(estimate_tokens(&small) <= 75, "{small}");
    }

    #[test]
    fn empty_project_gets_a_hint() {
        let d = tempfile::tempdir().unwrap();
        let c = open_writer(&d.path().join("a.db")).unwrap();
        let p = projects::upsert_project(
            &c,
            &ProjectRef {
                key: "k".into(),
                slug: "new".into(),
                name: "new".into(),
                root: None,
                remote: None,
            },
        )
        .unwrap();
        assert!(
            build(&c, &p, 1_200, chrono::Utc::now())
                .unwrap()
                .contains("No memories yet")
        );
    }

    // ---- Beyond the brief: statuses, windows, layout, caps and budget edges. ----

    fn setup(
        slug: &str,
    ) -> (
        tempfile::TempDir,
        rusqlite::Connection,
        projects::ResolvedProject,
    ) {
        let d = tempfile::tempdir().unwrap();
        let c = open_writer(&d.path().join("a.db")).unwrap();
        let p = project(&c, slug);
        (d, c, p)
    }

    fn project(c: &rusqlite::Connection, slug: &str) -> projects::ResolvedProject {
        projects::upsert_project(
            c,
            &ProjectRef {
                key: format!("k/{slug}"),
                slug: slug.into(),
                name: slug.into(),
                root: None,
                remote: None,
            },
        )
        .unwrap()
    }

    /// Insert and return (id, seq).
    fn put(
        c: &rusqlite::Connection,
        p: &projects::ResolvedProject,
        input: memories::NewMemoryInput,
    ) -> (String, i64) {
        let m = memories::prepare(input, p, "h".into()).unwrap();
        match memories::insert(c, &m).unwrap() {
            memories::InsertOutcome::New { id, seq } => (id, seq),
            other => panic!("expected a new memory, got {other:?}"),
        }
    }

    fn input(kind: Kind, imp: u8, text: &str) -> memories::NewMemoryInput {
        memories::NewMemoryInput {
            content: text.into(),
            kind: Some(kind),
            importance: Some(imp),
            ..Default::default()
        }
    }

    fn at(rfc3339: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(rfc3339)
            .unwrap()
            .with_timezone(&Utc)
    }

    const NOW: &str = "2026-10-13T12:00:00.000Z";

    #[test]
    fn only_live_memories_appear_and_pending_counts_as_live() {
        let (_d, c, app) = setup("app");
        put(&c, &app, input(Kind::Warning, 3, "pending warning"));
        for (text, status) in [
            ("active warning", Status::Active),
            ("superseded warning", Status::Superseded),
            ("archived warning", Status::Archived),
            ("trashed warning", Status::Trashed),
        ] {
            let (_, seq) = put(&c, &app, input(Kind::Warning, 3, text));
            memories::set_status_by_seq(&c, seq, status).unwrap();
        }
        let text = build(&c, &app, 1_200, at(NOW)).unwrap();
        for live in ["pending warning", "active warning"] {
            assert!(text.contains(live), "missing {live}:\n{text}");
        }
        for gone in ["superseded warning", "archived warning", "trashed warning"] {
            assert!(!text.contains(gone), "{gone} shown:\n{text}");
        }
        assert!(!text.contains("No memories yet"), "{text}");

        // A project whose only memory was forgotten is empty again.
        let other = project(&c, "other");
        let (_, seq) = put(&c, &other, input(Kind::Note, 5, "forgotten"));
        memories::set_status_by_seq(&c, seq, Status::Trashed).unwrap();
        let text = build(&c, &other, 1_200, at(NOW)).unwrap();
        assert!(text.contains("No memories yet for other"), "{text}");
        assert!(!text.contains("forgotten"), "{text}");
    }

    #[test]
    fn recent_decisions_cover_fourteen_days_newest_first() {
        let (_d, c, app) = setup("app");
        for (text, created) in [
            // Half a day either side of the cut-off, so a window of 13 or 15 days fails.
            ("decision from 14.5 days ago", "2026-09-29T00:00:00.000Z"),
            ("decision from 13.5 days ago", "2026-09-30T00:00:00.000Z"),
            ("decision from yesterday", "2026-10-12T12:00:00.000Z"),
        ] {
            let mut i = input(Kind::Decision, 3, text);
            i.created_at = Some(created.into());
            put(&c, &app, i);
        }
        let text = build(&c, &app, 1_200, at(NOW)).unwrap();
        let pos = |s: &str| {
            text.find(s)
                .unwrap_or_else(|| panic!("missing {s}:\n{text}"))
        };
        assert!(pos("## Recent decisions") < pos("decision from yesterday"));
        assert!(pos("decision from yesterday") < pos("decision from 13.5 days ago"));
        assert!(!text.contains("decision from 14.5 days ago"), "{text}");
    }

    #[test]
    fn latest_handoff_is_shown_in_full_with_its_id_and_without_a_repeated_title() {
        let (_d, c, app) = setup("app");
        let mut old = input(Kind::Handoff, 3, "Handoff: old state\nold next step");
        old.created_at = Some("2026-10-01T09:00:00.000Z".into());
        put(&c, &app, old);
        let mut new = input(
            Kind::Handoff,
            3,
            "Handoff: wired the daemon\nnext: CLI\nthen: docs",
        );
        new.created_at = Some("2026-10-10T09:00:00.000Z".into());
        new.author = Author {
            agent: Some("codex".into()),
            ..Default::default()
        };
        let (id, _) = put(&c, &app, new);
        let text = build(&c, &app, 1_200, at(NOW)).unwrap();
        let block = format!(
            "\n## Last handoff\n2026-10-10 · codex ({id})\nHandoff: wired the daemon\nnext: CLI\nthen: docs\n"
        );
        assert!(text.contains(&block), "{text}");
        assert_eq!(
            text.matches("Handoff: wired the daemon").count(),
            1,
            "{text}"
        );
        assert!(!text.contains("old state"), "{text}");

        // An explicit title the text does not start with comes first; no author, no separator.
        let other = project(&c, "other");
        let mut titled = input(Kind::Handoff, 3, "Merged Task 8\nnext: Task 9");
        titled.title = Some("Session end".into());
        titled.created_at = Some("2026-10-11T09:00:00.000Z".into());
        let (id, _) = put(&c, &other, titled);
        let text = build(&c, &other, 1_200, at(NOW)).unwrap();
        let block = format!(
            "\n## Last handoff\n2026-10-11 ({id})\nSession end\nMerged Task 8\nnext: Task 9\n"
        );
        assert!(text.contains(&block), "{text}");

        // Long handoffs are cut at 600 characters, marked.
        let third = project(&c, "third");
        put(&c, &third, input(Kind::Handoff, 3, &"step ".repeat(300)));
        let text = build(&c, &third, 1_200, at(NOW)).unwrap();
        let shown = text.split('\n').find(|l| l.starts_with("step")).unwrap();
        assert_eq!(shown.chars().count(), 601, "{shown}");
        assert!(shown.ends_with('…'));
    }

    #[test]
    fn odd_creation_dates_do_not_panic() {
        let (_d, c, app) = setup("app");
        // Imports keep the original creation time as given; it may be short or non-ASCII.
        let mut short = input(Kind::Handoff, 3, "short date handoff");
        short.created_at = Some("2026".into());
        put(&c, &app, short);
        let text = build(&c, &app, 1_200, at(NOW)).unwrap();
        assert!(text.contains("\n2026 ("), "{text}");

        let other = project(&c, "other");
        let mut thai = input(Kind::Handoff, 3, "thai date handoff");
        thai.created_at = Some("๒๕๖๙-๑๐-๑๓T00:00:00Z".into());
        put(&c, &other, thai);
        let text = build(&c, &other, 1_200, at(NOW)).unwrap();
        assert!(text.contains("\n๒๕๖๙-๑๐-๑๓ ("), "{text}");
    }

    #[test]
    fn entries_show_the_text_after_the_title() {
        let (_d, c, app) = setup("app");
        // A title derived from a short first line: the next line is the detail.
        let (wal, _) = put(
            &c,
            &app,
            input(
                Kind::Decision,
                4,
                "Use WAL mode\nreaders must not block the writer",
            ),
        );
        // An explicit title: the body's first line is the detail, not dropped.
        let mut port = input(
            Kind::Fact,
            4,
            "listens on 8080\nset ALICE_PORT to change it",
        );
        port.title = Some("API port".into());
        let (port, _) = put(&c, &app, port);
        // A title cut mid-word at 80 characters: the whole first line is shown, once.
        let long_first = format!("{}bcdef more words after the cut", "a".repeat(78));
        let (cut, _) = put(
            &c,
            &app,
            input(Kind::Warning, 4, &format!("{long_first}\nsecond line")),
        );
        // A first line over 120 characters is cut there, marked.
        let (huge, _) = put(&c, &app, input(Kind::Note, 4, &"z".repeat(300)));
        let text = build(&c, &app, 5_000, at(NOW)).unwrap();
        for want in [
            format!("- [decision] Use WAL mode — readers must not block the writer ({wal})\n"),
            format!("- [fact] API port — listens on 8080 ({port})\n"),
            format!("- [warning] {long_first} — second line ({cut})\n"),
            format!("- [note] {}… ({huge})\n", "z".repeat(120)),
        ] {
            assert!(text.contains(&want), "missing {want:?}:\n{text}");
        }
        assert_eq!(text.matches(&"a".repeat(78)).count(), 1, "{text}");
    }

    #[test]
    fn section_caps_count_only_memories_not_shown_yet() {
        let (_d, c, app) = setup("app");
        for i in 0..5 {
            put(&c, &app, input(Kind::Warning, 5, &format!("warning {i}")));
        }
        // Newest, but least important: the five places go to the important ones.
        put(&c, &app, input(Kind::Warning, 2, "minor warning"));
        for i in 0..9 {
            put(&c, &app, input(Kind::Note, 4, &format!("key note {i}")));
        }
        let text = build(&c, &app, 5_000, at(NOW)).unwrap();
        for i in 0..5 {
            assert_eq!(text.matches(&format!("warning {i} (")).count(), 1, "{text}");
        }
        assert!(!text.contains("minor warning"), "{text}");
        // The five warnings rank first for Key memories too; they must not use up its eight places.
        assert_eq!(text.matches("key note ").count(), 8, "{text}");

        // Shown memories that a later section does not select must not widen its cap.
        let other = project(&c, "other");
        for i in 0..5 {
            put(
                &c,
                &other,
                input(Kind::Warning, 3, &format!("low warning {i}")),
            );
        }
        for i in 0..14 {
            put(&c, &other, input(Kind::Note, 4, &format!("key note {i}")));
        }
        let text = build(&c, &other, 5_000, at(NOW)).unwrap();
        assert_eq!(text.matches("low warning ").count(), 5, "{text}");
        assert_eq!(text.matches("key note ").count(), 8, "{text}");
    }

    #[test]
    fn key_memories_leave_out_handoffs_and_diaries() {
        let (_d, c, app) = setup("app");
        let mut older = input(Kind::Handoff, 5, "older handoff");
        older.created_at = Some("2026-10-01T09:00:00.000Z".into());
        put(&c, &app, older);
        put(&c, &app, input(Kind::Handoff, 3, "latest handoff"));
        put(&c, &app, input(Kind::Diary, 5, "diary entry"));
        put(&c, &app, input(Kind::Fact, 4, "key fact"));
        let text = build(&c, &app, 1_200, at(NOW)).unwrap();
        assert!(
            text.contains("latest handoff") && text.contains("key fact"),
            "{text}"
        );
        assert!(!text.contains("older handoff"), "{text}");
        // The diary has its own section, after the key memories.
        let (before_diary, in_diary) = text.split_once("## Recent diary").unwrap();
        assert!(!before_diary.contains("diary entry"), "{text}");
        assert!(in_diary.contains("diary entry"), "{text}");
    }

    #[test]
    fn budget_is_a_hard_cap_beyond_the_heading() {
        let (_d, c, app) = setup("app");
        let global = projects::upsert_project(&c, &ProjectRef::global()).unwrap();
        put(
            &c,
            &app,
            input(
                Kind::Handoff,
                3,
                &format!("Handoff: state\n{}", "detail ".repeat(60)),
            ),
        );
        for i in 0..6 {
            put(
                &c,
                &app,
                input(Kind::Warning, 4, &format!("warning number {i}\nwhy {i}")),
            );
            put(&c, &app, input(Kind::Fact, 5, &format!("key fact {i}")));
            put(&c, &app, input(Kind::Decision, 3, &format!("decision {i}")));
            put(
                &c,
                &global,
                input(Kind::Preference, 3, &format!("preference {i}")),
            );
        }
        // The decisions were created just now, so judge "recent" from the real clock.
        let now = chrono::Utc::now();
        let heading = "# Context · app\n";
        assert_eq!(build(&c, &app, 0, now).unwrap(), heading);
        for budget in (0..=600).step_by(7) {
            let text = build(&c, &app, budget, now).unwrap();
            assert!(text.starts_with(heading), "{text}");
            let cost = estimate_tokens(&text);
            assert!(
                cost <= budget.max(estimate_tokens(heading)),
                "{budget}: {cost}\n{text}"
            );
        }
        let full = build(&c, &app, 1_200, now).unwrap();
        for section in [
            "Last handoff",
            "Warnings",
            "Key memories",
            "Recent decisions",
            "Your preferences",
        ] {
            assert!(full.contains(&format!("\n## {section}\n")), "{full}");
        }
        assert!(estimate_tokens(&full) <= 1_200);
    }

    #[test]
    fn global_briefing_has_no_hint_and_no_preferences_section() {
        let d = tempfile::tempdir().unwrap();
        let c = open_writer(&d.path().join("a.db")).unwrap();
        let global = projects::upsert_project(&c, &ProjectRef::global()).unwrap();
        assert_eq!(
            build(&c, &global, 1_200, at(NOW)).unwrap(),
            "# Context · global\n"
        );
        put(&c, &global, input(Kind::Preference, 4, "Reply in Thai"));
        // Below Key memories' cut-off: only a preferences section could show it.
        put(&c, &global, input(Kind::Preference, 3, "Short answers"));
        let text = build(&c, &global, 1_200, at(NOW)).unwrap();
        assert!(
            text.contains("## Key memories\n- [preference] Reply in Thai"),
            "{text}"
        );
        assert!(!text.contains("Your preferences"), "{text}");
        assert!(!text.contains("Short answers"), "{text}");
    }

    fn query_plan(c: &rusqlite::Connection, sql: &str) -> String {
        let mut st = c.prepare(&format!("EXPLAIN QUERY PLAN {sql}")).unwrap();
        let unbound = std::iter::repeat_n("x", st.parameter_count());
        st.query_map(rusqlite::params_from_iter(unbound), |r| {
            r.get::<_, String>(3)
        })
        .unwrap()
        .map(|step| step.unwrap())
        .collect::<Vec<_>>()
        .join("; ")
    }

    #[test]
    fn context_queries_read_an_index_in_order() {
        // Session-start latency must not grow with the project: each query walks an index and
        // stops at its limit. Ordered by importance, the index order is the display order, so
        // nothing is sorted. The handoff and recent-decision plans keep a sort step: `status IN
        // (2 values)` gives two `created_at`-ordered index ranges, which SQLite merges and leaves
        // once the limit is reached (see `context_query_work_does_not_grow_with_the_project`).
        let (_d, c, _) = setup("app");
        let mut wrong = Vec::new();
        for (name, sql, index, merges) in [
            ("any live", any_live_sql(), "memories_project_kind", false),
            (
                "last handoff",
                section_sql(LAST_HANDOFF),
                "memories_project_kind",
                true,
            ),
            (
                "warnings",
                section_sql(WARNINGS),
                "memories_project_kind_importance",
                false,
            ),
            (
                "key memories",
                section_sql(KEY_MEMORIES),
                "memories_project_importance",
                false,
            ),
            (
                "recent decisions",
                section_sql(RECENT_DECISIONS),
                "memories_project_kind",
                true,
            ),
            (
                "preferences",
                section_sql(PREFERENCES),
                "memories_project_kind_importance",
                false,
            ),
        ] {
            let plan = query_plan(&c, &sql);
            if !plan.contains(&format!("INDEX {index} ("))
                || (!merges && plan.contains("TEMP B-TREE"))
            {
                wrong.push(format!("{name}: {plan}"));
            }
        }
        assert!(wrong.is_empty(), "{}", wrong.join("\n"));
    }

    /// Virtual-machine steps for one run of `sql`, which must return rows.
    fn vm_steps(c: &rusqlite::Connection, sql: &str, args: &[&dyn ToSql]) -> i32 {
        let mut st = c.prepare(sql).unwrap();
        let rows = st.query_map(args, |r| r.get::<_, i64>(0)).unwrap().count();
        assert!(rows > 0, "no rows: {sql}");
        st.get_status(rusqlite::StatementStatus::VmStep)
    }

    #[test]
    fn context_query_work_does_not_grow_with_the_project() {
        let (_d, c, small) = setup("small");
        let large = project(&c, "large");
        c.execute_batch("BEGIN").unwrap();
        for (p, per_kind) in [(&small, 10), (&large, 300)] {
            for i in 0..per_kind {
                for kind in [
                    Kind::Handoff,
                    Kind::Warning,
                    Kind::Decision,
                    Kind::Preference,
                    Kind::Note,
                ] {
                    // Every importance and both live statuses, in the same mix in both projects.
                    let imp = u8::try_from(i % 5 + 1).unwrap();
                    let (_, seq) = put(&c, p, input(kind, imp, &format!("{} {i}", kind.as_str())));
                    if i % 2 == 0 {
                        memories::set_status_by_seq(&c, seq, Status::Active).unwrap();
                    }
                }
            }
        }
        c.execute_batch("COMMIT").unwrap();
        let (since, limit) = ("2000-01-01T00:00:00.000Z", 10_i64);
        let mut counts = Vec::new();
        let mut grows = false;
        for (name, sql) in [
            ("any live", any_live_sql()),
            ("last handoff", section_sql(LAST_HANDOFF)),
            ("warnings", section_sql(WARNINGS)),
            ("key memories", section_sql(KEY_MEMORIES)),
            ("recent decisions", section_sql(RECENT_DECISIONS)),
            ("preferences", section_sql(PREFERENCES)),
        ] {
            let steps = |p: &projects::ResolvedProject| {
                let pid = p.id.as_str();
                match (sql.contains("?2"), sql.contains("LIMIT")) {
                    (true, _) => vm_steps(&c, &sql, &[&pid, &since, &limit]),
                    (false, true) => vm_steps(&c, &sql, &[&pid, &limit]),
                    (false, false) => vm_steps(&c, &sql, &[&pid]),
                }
            };
            let (few, many) = (steps(&small), steps(&large));
            // 30 times the rows: reading or sorting all of them would take about 30 times the steps.
            grows |= many > 2 * few;
            counts.push(format!(
                "{name}: {few} steps for 50 memories, {many} for 1,500"
            ));
        }
        assert!(!grows, "{}", counts.join("\n"));
    }

    #[test]
    fn open_review_items_add_a_last_section_and_none_adds_nothing() {
        let (_d, c, app) = setup("app");
        put(&c, &app, input(Kind::Decision, 5, "Rust single binary"));
        let none = build(&c, &app, 1_200, at(NOW)).unwrap();
        assert!(!none.contains("## Review"), "{none}");
        for key in ["a", "b"] {
            crate::store::reviews::create(
                &c,
                &crate::store::reviews::NewReview {
                    kind: crate::store::reviews::ReviewType::Merge,
                    payload: serde_json::json!({}),
                    confidence: None,
                    dedupe_key: Some(key.into()),
                },
            )
            .unwrap();
        }
        let text = build(&c, &app, 1_200, at(NOW)).unwrap();
        assert!(text.contains("2 item(s) wait for a decision"), "{text}");
        assert!(
            text.trim_end()
                .lines()
                .rev()
                .nth(1)
                .unwrap()
                .starts_with("## Review"),
            "the Review section comes last:\n{text}"
        );
        // A budget too small for it leaves it out rather than overrun.
        let tiny = build(&c, &app, 30, at(NOW)).unwrap();
        assert!(!tiny.contains("## Review"), "{tiny}");
    }

    fn diary(c: &rusqlite::Connection, p: &projects::ResolvedProject, title: &str, created: &str) {
        let mut i = input(Kind::Diary, 3, &format!("{title}\nwhat happened that day"));
        i.title = Some(title.into());
        i.created_at = Some(created.into());
        put(c, p, i);
    }

    fn review_items(c: &rusqlite::Connection, prefix: &str, n: usize) {
        for key in 0..n {
            crate::store::reviews::create(
                c,
                &crate::store::reviews::NewReview {
                    kind: crate::store::reviews::ReviewType::Merge,
                    payload: serde_json::json!({}),
                    confidence: None,
                    dedupe_key: Some(format!("{prefix}{key}")),
                },
            )
            .unwrap();
        }
    }

    #[test]
    fn recent_diary_lists_the_newest_non_checkpoint_entries() {
        let (_d, c, app) = setup("app");
        for day in 1..=7 {
            diary(
                &c,
                &app,
                &format!("Diary 2026-10-0{day}: topic{day}"),
                &format!("2026-10-0{day}T09:00:00.000Z"),
            );
        }
        diary(
            &c,
            &app,
            "Diary 2026-10-09: checkpoint",
            "2026-10-09T09:00:00.000Z",
        );
        let mut tagged = input(Kind::Diary, 3, "auto-save body");
        tagged.title = Some("Diary 2026-10-10: autosave".into());
        tagged.topics = vec!["checkpoint".into()];
        tagged.created_at = Some("2026-10-10T09:00:00.000Z".into());
        put(&c, &app, tagged);

        let text = build(&c, &app, 1_200, at(NOW)).unwrap();
        let pos = |s: &str| {
            text.find(s)
                .unwrap_or_else(|| panic!("missing {s}:\n{text}"))
        };
        assert!(pos("## Recent diary") < pos("topic7"), "{text}");
        assert!(pos("topic7") < pos("topic6"), "newest first:\n{text}");
        assert!(pos("topic3") > pos("topic4"), "{text}");
        assert!(
            !text.contains("topic2") && !text.contains("topic1"),
            "at most 5:\n{text}"
        );
        assert!(!text.contains("checkpoint"), "{text}");
        assert!(!text.contains("autosave"), "{text}");
        assert!(text.contains("what happened that day"), "{text}");
        // Inside the budget it is the first to shrink: a tight budget drops entries, never overruns.
        let tight = build(&c, &app, 40, at(NOW)).unwrap();
        assert!(estimate_tokens(&tight) <= 55, "{tight}");
    }

    #[test]
    fn recent_diary_comes_after_preferences_and_before_review_and_is_absent_without_diaries() {
        let (_d, c, app) = setup("app");
        put(&c, &app, input(Kind::Decision, 5, "Rust single binary"));
        let none = build(&c, &app, 1_200, at(NOW)).unwrap();
        assert!(!none.contains("Recent diary"), "{none}");
        let global = projects::upsert_project(&c, &ProjectRef::global()).unwrap();
        put(&c, &global, input(Kind::Preference, 4, "Reply in Thai"));
        diary(
            &c,
            &app,
            "Diary 2026-10-05: wiring",
            "2026-10-05T09:00:00.000Z",
        );
        review_items(&c, "a", 1);
        let text = build(&c, &app, 1_200, at(NOW)).unwrap();
        let pos = |s: &str| {
            text.find(s)
                .unwrap_or_else(|| panic!("missing {s}:\n{text}"))
        };
        assert!(
            pos("## Your preferences") < pos("## Recent diary"),
            "{text}"
        );
        assert!(pos("## Recent diary") < pos("## Review"), "{text}");
    }

    #[test]
    fn only_checkpoint_diaries_give_no_diary_section() {
        let (_d, c, app) = setup("app");
        diary(
            &c,
            &app,
            "Diary 2026-10-05: checkpoint",
            "2026-10-05T09:00:00.000Z",
        );
        let text = build(&c, &app, 1_200, at(NOW)).unwrap();
        assert!(!text.contains("Recent diary"), "{text}");
    }

    #[test]
    fn review_line_asks_the_user_only_for_a_handful_of_items() {
        let (_d, c, app) = setup("app");
        review_items(&c, "a", 3);
        let few = build(&c, &app, 1_200, at(NOW)).unwrap();
        assert!(few.contains("3 item(s) wait for a decision"), "{few}");
        assert!(few.contains("ask the user"), "{few}");
        review_items(&c, "b", 11);
        let many = build(&c, &app, 1_200, at(NOW)).unwrap();
        assert!(
            many.contains("14 decisions wait in `alice review` (handle them when convenient)"),
            "{many}"
        );
        assert!(!many.contains("ask the user"), "{many}");
    }
}
