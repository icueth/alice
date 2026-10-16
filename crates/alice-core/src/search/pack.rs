//! Fit search results into a token budget, as compact text (default) or JSON.

use super::SearchOutput;
use crate::api::{Hit, MatchVia, RecallResponse, Tier};
use crate::normalize::{estimate_tokens, truncate_chars};

/// Longest body shown for one result.
const FULL_BODY_CHARS: usize = 2_000;
/// Rough JSON overhead per result (keys, id, dates, score), in tokens.
const JSON_HIT_OVERHEAD: usize = 30;
const RAW_NOTE: &str = "(Few saved memories matched, so raw session excerpts are included.)\n";

fn omitted_note(n: usize) -> String {
    format!("({n} more results omitted; raise max_tokens or narrow the query.)\n")
}

/// Lowercase one character to one character, so positions in the folded text match the original.
fn fold(c: char) -> char {
    c.to_lowercase().next().unwrap_or(c)
}

/// A one-line window of at most `max_chars` (plus `…` marks) that starts a little before the
/// first occurrence of any term; the start of the text when no term occurs.
pub fn make_snippet(text: &str, terms: &[String], max_chars: usize) -> String {
    let flat: Vec<char> = text
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .collect();
    let folded: String = flat.iter().map(|c| fold(*c)).collect();
    let first_match = terms
        .iter()
        .filter(|t| !t.is_empty())
        .filter_map(|t| {
            let t: String = t.chars().map(fold).collect();
            folded.find(&t).map(|b| folded[..b].chars().count())
        })
        .min();
    let start = first_match.map_or(0, |p| p.saturating_sub(max_chars / 4));
    let rest: String = flat[start..].iter().collect();
    let mut s = truncate_chars(&rest, max_chars);
    if start > 0 {
        s.insert(0, '…');
    }
    s
}

fn via_label(v: &[MatchVia]) -> String {
    v.iter()
        .map(|m| match m {
            MatchVia::Vector => "vector",
            MatchVia::Words => "words",
            MatchVia::Code => "code",
        })
        .collect::<Vec<_>>()
        .join("+")
}

fn header(i: usize, h: &Hit) -> String {
    let kind = match (h.tier, h.kind) {
        (Tier::Raw, _) => "raw",
        (_, Some(k)) => k.as_str(),
        _ => "note",
    };
    let status = h
        .status
        .map(|s| format!(" · {}", s.as_str()))
        .unwrap_or_default();
    let date: String = h.created_at.chars().take(10).collect();
    format!(
        "[{}] {} · {} · {} · {}{} · matched: {}\n",
        i + 1,
        h.id,
        kind,
        h.project,
        date,
        status,
        via_label(&h.matched)
    )
}

/// One result: header, title, then the text without a leading copy of the title (a title
/// derived from the first line would otherwise be shown twice). The copy is removed only when
/// it ends at a word boundary: a derived title is cut at 80 characters, possibly mid-word, and
/// an explicit title may be a prefix of the first word ("Use" / "User sessions").
fn block(i: usize, h: &Hit, text: &str) -> String {
    let rest = match text.strip_prefix(h.title.as_str()) {
        Some(r) if r.is_empty() || r.starts_with(char::is_whitespace) => r.trim_start(),
        _ => text,
    };
    if rest.is_empty() {
        format!("{}{}\n\n", header(i, h), h.title)
    } else {
        format!("{}{}\n{}\n\n", header(i, h), h.title, rest)
    }
}

/// Snippet blocks that fit in `budget` tokens; the first result is always kept.
fn fit_blocks(hits: &[Hit], budget: usize) -> (Vec<String>, usize) {
    let mut blocks = Vec::new();
    let mut used = 0;
    for (i, h) in hits.iter().enumerate() {
        let b = block(i, h, &h.snippet);
        let cost = estimate_tokens(&b);
        if used + cost > budget && !blocks.is_empty() {
            break;
        }
        used += cost;
        blocks.push(b);
    }
    (blocks, used)
}

/// Text for agents: a heading line, one block per result (header, title, snippet or full body),
/// related memories, then notes. Returns the text and how many results did not fit.
///
/// The whole text, heading and notes included, stays within `max_tokens`, except that the best
/// result is always shown (with its snippet) even when it alone is over a very small budget.
pub fn pack_text(out: &SearchOutput, project: Option<&str>, max_tokens: usize) -> (String, usize) {
    if out.hits.is_empty() {
        let p = project
            .map(|p| format!(" in {p} or elsewhere"))
            .unwrap_or_default();
        return (
            format!(
                "No memories matched{p}. Try other words, include_raw=true, or remember this once you learn it.\n"
            ),
            0,
        );
    }
    let heading = match project {
        Some(p) => format!("{} results (project {p}), best first:\n", out.hits.len()),
        None => format!("{} results, best first:\n", out.hits.len()),
    };
    let fixed = estimate_tokens(&heading)
        + if out.raw_fallback {
            estimate_tokens(RAW_NOTE)
        } else {
            0
        };
    let mut budget = max_tokens.saturating_sub(fixed);
    let (mut blocks, mut used) = fit_blocks(&out.hits, budget);
    if blocks.len() < out.hits.len() {
        // Some results will be left out: keep room for the note that says so.
        budget = budget.saturating_sub(estimate_tokens(&omitted_note(out.hits.len())));
        (blocks, used) = fit_blocks(&out.hits, budget);
    }
    let omitted = out.hits.len() - blocks.len();
    // Upgrade the best results to full bodies while the budget allows.
    for (i, h) in out.hits.iter().enumerate().take(blocks.len()) {
        let Some(body) = &h.body else { continue };
        let full = block(i, h, &truncate_chars(body, FULL_BODY_CHARS));
        let delta = estimate_tokens(&full).saturating_sub(estimate_tokens(&blocks[i]));
        if used + delta <= budget {
            used += delta;
            blocks[i] = full;
        }
    }
    let mut text = heading;
    text.push_str(&blocks.concat());
    if !out.related.is_empty() {
        let mut rel = String::from("related:\n");
        for r in &out.related {
            rel.push_str(&format!(
                "- {} ({}) {} [{}]\n",
                r.id,
                r.kind.as_str(),
                r.title,
                r.via
            ));
        }
        if used + estimate_tokens(&rel) <= budget {
            text.push_str(&rel);
        }
    }
    if out.raw_fallback {
        text.push_str(RAW_NOTE);
    }
    if omitted > 0 {
        text.push_str(&omitted_note(omitted));
    }
    (text, omitted)
}

/// Structured output with the same budget rule: results that do not fit are omitted and
/// bodies are dropped from results that fit only without them. The best result is always kept.
pub fn pack_json(out: SearchOutput, project: Option<String>, max_tokens: usize) -> RecallResponse {
    let mut used = 0usize;
    let mut results = Vec::new();
    let total = out.hits.len();
    for mut h in out.hits {
        let base = estimate_tokens(&h.title) + estimate_tokens(&h.snippet) + JSON_HIT_OVERHEAD;
        if used + base > max_tokens && !results.is_empty() {
            break;
        }
        used += base;
        if let Some(b) = h.body.take() {
            let b = truncate_chars(&b, FULL_BODY_CHARS);
            let cost = estimate_tokens(&b);
            if used + cost <= max_tokens {
                used += cost;
                h.body = Some(b);
            }
        }
        results.push(h);
    }
    RecallResponse {
        project,
        omitted: total - results.len(),
        results,
        related: out.related,
        raw_fallback: out.raw_fallback,
        reranked: false,
        low_confidence: false,
        text: None,
        notes: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{Hit, MatchVia, RelatedRef, Tier};
    use crate::model::{Kind, Status};
    use crate::normalize::estimate_tokens;

    fn hit(i: usize, body: &str) -> Hit {
        Hit {
            id: format!("ID{i}"),
            tier: Tier::Knowledge,
            kind: Some(Kind::Decision),
            project: "app".into(),
            title: format!("title {i}"),
            snippet: body.chars().take(50).collect(),
            body: Some(body.into()),
            created_at: "2026-10-13T00:00:00.000Z".into(),
            score: 1.0,
            relevance: None,
            matched: vec![MatchVia::Words],
            status: None,
        }
    }

    #[test]
    fn budget_limits_output_and_reports_omitted() {
        let out = SearchOutput {
            hits: (0..10).map(|i| hit(i, &"word ".repeat(200))).collect(),
            ..Default::default()
        };
        let (text, omitted) = pack_text(&out, Some("app"), 300);
        assert!(estimate_tokens(&text) <= 330, "{}", text.len());
        assert!(omitted > 0);
        assert!(text.contains("[1] ID0"));
        assert!(text.contains("omitted"));
    }

    #[test]
    fn budget_is_a_hard_cap_including_notes() {
        let mut out = SearchOutput {
            hits: (0..10).map(|i| hit(i, &"word ".repeat(200))).collect(),
            raw_fallback: true,
            ..Default::default()
        };
        out.related = vec![RelatedRef {
            id: "R1".into(),
            kind: Kind::Fact,
            title: "related title".into(),
            via: "supersedes".into(),
        }];
        for budget in [120, 300, 777, 1500] {
            let (text, omitted) = pack_text(&out, Some("app"), budget);
            assert!(
                estimate_tokens(&text) <= budget,
                "{budget}: {} tokens",
                estimate_tokens(&text)
            );
            assert!(text.contains("[1] ID0"), "{budget}");
            assert_eq!(omitted > 0, text.contains("omitted"), "{budget}");
            assert!(text.contains("raw session excerpts"), "{budget}");
        }
    }

    #[test]
    fn a_tiny_budget_still_returns_the_best_result() {
        let out = SearchOutput {
            hits: vec![hit(0, "short body"), hit(1, "other")],
            ..Default::default()
        };
        let (text, omitted) = pack_text(&out, Some("app"), 5);
        assert!(text.contains("[1] ID0"));
        assert_eq!(omitted, 1);
        let json = pack_json(out, Some("app".into()), 5);
        assert_eq!(json.results.len(), 1);
        assert_eq!(json.omitted, 1);
    }

    #[test]
    fn small_results_include_full_bodies() {
        let out = SearchOutput {
            hits: vec![hit(0, "short body")],
            ..Default::default()
        };
        let (text, omitted) = pack_text(&out, Some("app"), 1500);
        assert_eq!(omitted, 0);
        assert!(text.contains("short body"));
        assert!(!text.contains("omitted"));
    }

    #[test]
    fn full_bodies_go_to_the_best_results_first() {
        let long = format!("start {} end-of-body", "word ".repeat(150));
        let out = SearchOutput {
            hits: (0..3).map(|i| hit(i, &long)).collect(),
            ..Default::default()
        };
        // Room for three snippets and roughly one full body.
        let (text, omitted) = pack_text(&out, None, 320);
        assert_eq!(omitted, 0);
        assert_eq!(text.matches("end-of-body").count(), 1, "{text}");
        let first = text.find("[1] ID0").unwrap();
        let second = text.find("[2] ID1").unwrap();
        let body_at = text.find("end-of-body").unwrap();
        assert!(first < body_at && body_at < second, "{text}");
    }

    #[test]
    fn header_shows_kind_status_date_and_match() {
        let mut h = hit(0, "body");
        h.status = Some(Status::Superseded);
        h.matched = vec![MatchVia::Vector, MatchVia::Code];
        let mut raw = hit(1, "raw text");
        raw.tier = Tier::Raw;
        raw.kind = None;
        raw.created_at = "สองพันยี่สิบหก".into();
        let out = SearchOutput {
            hits: vec![h, raw],
            ..Default::default()
        };
        let (text, _) = pack_text(&out, Some("app"), 1500);
        assert!(
            text.contains(
                "[1] ID0 · decision · app · 2026-10-13 · superseded · matched: vector+code"
            ),
            "{text}"
        );
        assert!(text.contains("[2] ID1 · raw · app · สองพันยี่ส"), "{text}");
    }

    #[test]
    fn a_title_taken_from_the_first_line_is_not_repeated() {
        let mut one_line = hit(0, "port is 9090");
        one_line.title = "port is 9090".into();
        one_line.snippet = "port is 9090".into();
        let mut two_lines = hit(1, "Use WAL mode\nbecause readers must not block");
        two_lines.title = "Use WAL mode".into();
        two_lines.snippet = "Use WAL mode because readers must not block".into();
        let out = SearchOutput {
            hits: vec![one_line, two_lines],
            ..Default::default()
        };
        for budget in [60, 1500] {
            let (text, _) = pack_text(&out, None, budget);
            assert_eq!(text.matches("port is 9090").count(), 1, "{text}");
            assert_eq!(text.matches("Use WAL mode").count(), 1, "{text}");
            assert!(text.contains("because readers must not block"), "{text}");
        }
    }

    #[test]
    fn a_title_is_only_removed_at_a_word_boundary() {
        // A derived title is the first 80 characters: it can end inside a word.
        let long = "Hub writer lease held during transcript mining caused 120 s diary timeouts; mining now runs off the writer";
        let mut derived = hit(0, long);
        derived.title = crate::normalize::derive_title(long);
        derived.snippet = long.into();
        assert!(!derived.title.ends_with(' ') && long.len() > derived.title.len());
        // An explicit title can be a prefix of the first word.
        let mut prefix = hit(1, "User sessions expire after an hour");
        prefix.title = "Use".into();
        prefix.snippet = "User sessions expire after an hour".into();
        let out = SearchOutput {
            hits: vec![derived, prefix],
            ..Default::default()
        };
        for budget in [200, 1500] {
            let (text, _) = pack_text(&out, None, budget);
            assert!(text.contains(long), "{budget}: {text}");
            assert!(!text.contains("\nng now"), "{budget}: {text}");
            assert!(
                text.contains("User sessions expire after an hour"),
                "{budget}: {text}"
            );
            assert!(!text.contains("\nr sessions"), "{budget}: {text}");
        }
    }

    #[test]
    fn empty_results_say_so() {
        let (text, omitted) = pack_text(&SearchOutput::default(), Some("app"), 1500);
        assert!(text.contains("No memories matched"));
        assert!(text.contains("app"));
        assert_eq!(omitted, 0);
    }

    #[test]
    fn json_keeps_bodies_within_the_budget() {
        let out = SearchOutput {
            hits: (0..10).map(|i| hit(i, &"word ".repeat(200))).collect(),
            raw_fallback: true,
            ..Default::default()
        };
        let json = pack_json(out.clone(), Some("app".into()), 400);
        assert!(json.omitted > 0);
        assert_eq!(json.results.len() + json.omitted, 10);
        assert!(json.raw_fallback);
        assert_eq!(json.project.as_deref(), Some("app"));
        assert!(
            json.results[0].body.is_some(),
            "the best result keeps its body"
        );
        assert!(
            json.results.iter().any(|h| h.body.is_none()),
            "later bodies are dropped"
        );
        let roomy = pack_json(out, None, 100_000);
        assert_eq!(roomy.omitted, 0);
        assert!(roomy.results.iter().all(|h| h.body.is_some()));
    }

    #[test]
    fn snippet_centres_on_the_first_match() {
        let text = format!("{} needle {}", "a".repeat(500), "b".repeat(500));
        let s = make_snippet(&text, &["needle".into()], 100);
        assert!(s.contains("needle"));
        assert!(s.starts_with('…'));
        assert!(s.chars().count() <= 102);
    }

    #[test]
    fn snippet_is_one_line_and_handles_case_changes_and_thai() {
        let s = make_snippet("line one\n\nNEEDLE  here", &["needle".into()], 100);
        assert_eq!(s, "line one NEEDLE here");
        // Lowercasing that changes byte length before the match must not shift or panic.
        let text = format!("{}İ needle tail", "x ".repeat(100));
        let s = make_snippet(&text, &["needle".into()], 30);
        assert!(s.contains("needle"), "{s}");
        let thai = format!("{}ระบบความจำของเอเจนต์", "ก".repeat(300));
        let s = make_snippet(&thai, &["ความจำ".into()], 80);
        assert!(s.contains("ความจำ"), "{s}");
        assert_eq!(make_snippet("no match", &["zzz".into()], 3), "no …");
    }
}
