//! What an import tells its owner: counts for every place a drawer can end up, and the text
//! layout the CLI prints. No drawer text and no detected secret ever appears here, only names
//! and numbers.

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct PalaceSummary {
    pub path: String,
    /// `sqlite_exact` or `chroma`.
    pub backend: String,
    pub collection: String,
    pub drawers: u64,
}

/// What this run found to import, before duplicates collapse; drawers an earlier run stored are
/// not counted again (they are under `already_imported`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct TierReport {
    pub knowledge: u64,
    pub raw_session_chunks: u64,
    pub raw_session_transcripts: u64,
    pub raw_file_chunks: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ProjectCounts {
    pub memories: u64,
    pub chunks: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct WingMapEntry {
    /// The project slug, or a short phrase when the project is decided per drawer.
    pub to: String,
    /// `wing`, `alias`, `override`, `inferred`, `transcript` or `global`.
    pub via: String,
    /// For an agent's own wing: the agent.
    pub agent: Option<String>,
    pub drawers: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct DuplicateCounts {
    pub knowledge: u64,
    pub raw: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct FactCounts {
    pub added: u64,
    pub ended: u64,
    pub skipped: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct TunnelCounts {
    pub linked: u64,
    pub dropped: u64,
    pub ignored_topic: u64,
}

/// Drawers cut into several parts because they were over the size limits.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SplitCounts {
    pub memories: u64,
    pub chunks: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct PhaseTiming {
    pub phase: String,
    pub seconds: f64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ImportReport {
    pub job_id: String,
    pub dry_run: bool,
    pub started_at: String,
    pub finished_at: Option<String>,
    pub palace: PalaceSummary,
    /// The offset naive MemPalace times were read in, such as `+07:00`.
    pub assumed_utc_offset: String,
    pub tiers: TierReport,
    pub projects: BTreeMap<String, ProjectCounts>,
    pub wing_map: BTreeMap<String, WingMapEntry>,
    pub duplicates: DuplicateCounts,
    pub facts: FactCounts,
    pub tunnels: TunnelCounts,
    pub drawers_seen: u64,
    pub drawers_accounted: u64,
    pub closets_dropped: u64,
    pub isolated_entities: u64,
    pub review_items: u64,
    pub already_imported: u64,
    pub skipped: BTreeMap<String, u64>,
    pub split: SplitCounts,
    /// Detector name -> drawers (and knowledge graph triples) held back because they held that kind of secret.
    pub blocked_by_redaction: BTreeMap<String, u64>,
    pub redacted_fields: u64,
    pub redaction_note: Option<String>,
    pub phases: Vec<PhaseTiming>,
    pub peak_rss_mb: Option<u64>,
    pub warnings: Vec<String>,
    pub errors: Vec<String>,
}

/// `1234567` -> `1,234,567`.
fn num(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// `  Label ...... value`: the value starts at one column for every line.
fn line(out: &mut String, label: &str, value: &str) {
    const WIDTH: usize = 24;
    let head = format!("{label} ");
    let dots = WIDTH.saturating_sub(head.chars().count()).max(2);
    out.push_str(&format!("  {head}{} {value}\n", ".".repeat(dots)));
}

fn map_phrase(e: &WingMapEntry) -> String {
    match e.via.as_str() {
        "alias" => format!("{} (alias)", e.to),
        "override" => format!("{} (override)", e.to),
        "inferred" => match &e.agent {
            Some(a) => format!("{}, agent {a}", e.to),
            None => e.to.clone(),
        },
        _ => e.to.clone(),
    }
}

/// Projects listed on the summary line before "… (N total)".
const PROJECTS_SHOWN: usize = 8;
/// Wings listed in the map before "…".
const WINGS_SHOWN: usize = 12;

impl ImportReport {
    pub fn render_text(&self) -> String {
        let mut o = String::new();
        let dry = if self.dry_run { " [dry run]" } else { "" };
        o.push_str(&format!(
            "MemPalace import \u{2014} {} ({}, {} drawers){dry}\n",
            self.palace.path,
            self.palace.backend,
            num(self.palace.drawers)
        ));
        line(&mut o, "Knowledge memories", &num(self.tiers.knowledge));
        line(
            &mut o,
            "Raw session chunks",
            &format!(
                "{} in {} transcripts",
                num(self.tiers.raw_session_chunks),
                num(self.tiers.raw_session_transcripts)
            ),
        );
        line(&mut o, "Raw file chunks", &num(self.tiers.raw_file_chunks));
        line(
            &mut o,
            "Duplicates collapsed",
            &format!(
                "knowledge {}, raw {}",
                num(self.duplicates.knowledge),
                num(self.duplicates.raw)
            ),
        );
        line(&mut o, "Already imported", &num(self.already_imported));
        line(&mut o, "Closets dropped", &num(self.closets_dropped));
        line(
            &mut o,
            "Knowledge graph",
            &format!(
                "{} facts ({} ended), {} predicate reviews, {} entities without facts left out",
                num(self.facts.added),
                num(self.facts.ended),
                num(self.review_items),
                num(self.isolated_entities)
            ),
        );
        line(
            &mut o,
            "Tunnels",
            &format!(
                "{} linked, {} dropped (no drawer ids), {} topic tunnels ignored",
                num(self.tunnels.linked),
                num(self.tunnels.dropped),
                num(self.tunnels.ignored_topic)
            ),
        );
        let skipped: u64 = self.skipped.values().sum();
        let why: Vec<String> = self
            .skipped
            .iter()
            .map(|(k, v)| format!("{k}: {}", num(*v)))
            .collect();
        line(
            &mut o,
            "Skipped",
            &if why.is_empty() {
                num(skipped)
            } else {
                format!("{} ({})", num(skipped), why.join(", "))
            },
        );
        line(
            &mut o,
            "Split",
            &format!(
                "{} memories, {} chunks (over the size limits)",
                num(self.split.memories),
                num(self.split.chunks)
            ),
        );
        let blocked: u64 = self.blocked_by_redaction.values().sum();
        let mut red = format!("{} fields redacted", num(self.redacted_fields));
        if blocked > 0 {
            let by: Vec<String> = self
                .blocked_by_redaction
                .iter()
                .map(|(k, v)| format!("{k}: {}", num(*v)))
                .collect();
            red.push_str(&format!(
                ", {} drawers blocked ({})",
                num(blocked),
                by.join(", ")
            ));
        }
        if let Some(note) = &self.redaction_note {
            red.push_str(&format!("   [{note}]"));
        }
        line(&mut o, "Redaction", &red);

        let mut projects: Vec<(&String, u64)> = self
            .projects
            .iter()
            .map(|(k, v)| (k, v.memories + v.chunks))
            .collect();
        projects.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));
        let shown: Vec<String> = projects
            .iter()
            .take(PROJECTS_SHOWN)
            .map(|(k, n)| format!("{k} {}", num(*n)))
            .collect();
        let more = if projects.len() > PROJECTS_SHOWN {
            format!(" \u{b7} \u{2026} ({} total)", projects.len())
        } else {
            String::new()
        };
        o.push_str(&format!(
            "  Projects: {}{more}\n",
            if shown.is_empty() {
                "none".to_string()
            } else {
                shown.join(" \u{b7} ")
            }
        ));

        let mut wings: Vec<(&String, &WingMapEntry)> = self.wing_map.iter().collect();
        wings.sort_by(|a, b| b.1.drawers.cmp(&a.1.drawers).then_with(|| a.0.cmp(b.0)));
        o.push_str(&format!(
            "  Wing \u{2192} project map ({} wings):\n",
            wings.len()
        ));
        let mut parts: Vec<String> = wings
            .iter()
            .take(WINGS_SHOWN)
            .map(|(w, e)| format!("{w} \u{2192} {}", map_phrase(e)))
            .collect();
        if wings.len() > WINGS_SHOWN {
            parts.push("\u{2026}".to_string());
        }
        if !parts.is_empty() {
            o.push_str(&format!("    {}\n", parts.join(" \u{b7} ")));
        }

        let phases: Vec<String> = self
            .phases
            .iter()
            .map(|p| format!("{} {} s", p.phase, p.seconds.round() as u64))
            .collect();
        let peak = self
            .peak_rss_mb
            .map(|m| format!("{m} MB"))
            .unwrap_or_else(|| "unknown".to_string());
        o.push_str(&format!(
            "  Phases: {}    Peak memory {peak}\n",
            if phases.is_empty() {
                "none".to_string()
            } else {
                phases.join(" \u{b7} ")
            }
        ));
        o.push_str(&format!(
            "  Times assumed in UTC{}. Embeddings are computed in the background; search works now.\n",
            self.assumed_utc_offset
        ));
        if self.warnings.is_empty() {
            o.push_str("  Warnings: none\n");
        } else {
            o.push_str("  Warnings:\n");
            for w in &self.warnings {
                o.push_str(&format!("    - {w}\n"));
            }
        }
        if self.errors.is_empty() {
            o.push_str("  Errors: none\n");
        } else {
            o.push_str("  Errors:\n");
            for e in &self.errors {
                o.push_str(&format!("    - {e}\n"));
            }
        }
        o
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) fn sample_report() -> ImportReport {
        let mut r = ImportReport {
            job_id: "job".into(),
            dry_run: true,
            started_at: "2026-10-14T00:00:00.000Z".into(),
            palace: PalaceSummary {
                path: "/p/palace".into(),
                backend: "sqlite_exact".into(),
                collection: "mempalace_drawers".into(),
                drawers: 119_179,
            },
            assumed_utc_offset: "+07:00".into(),
            tiers: TierReport {
                knowledge: 5058,
                raw_session_chunks: 76_126,
                raw_session_transcripts: 130,
                raw_file_chunks: 38_000,
            },
            duplicates: DuplicateCounts {
                knowledge: 5,
                raw: 27,
            },
            closets_dropped: 4416,
            redacted_fields: 311,
            redaction_note: Some("privacy.redact is off: Alice redacted anyway".into()),
            peak_rss_mb: Some(410),
            phases: vec![PhaseTiming {
                phase: "snapshot".into(),
                seconds: 12.0,
            }],
            ..Default::default()
        };
        r.skipped.insert("MemPalace registry entry".into(), 24);
        r.blocked_by_redaction.insert("github_token".into(), 2);
        r.projects.insert(
            "pg-ai".into(),
            ProjectCounts {
                memories: 1,
                chunks: 35_200,
            },
        );
        r.wing_map.insert(
            "pg_ai".into(),
            WingMapEntry {
                to: "pg-ai".into(),
                via: "alias".into(),
                agent: None,
                drawers: 3,
            },
        );
        r
    }

    #[test]
    fn the_text_report_names_every_count_and_never_prints_blocked_text() {
        let r = sample_report();
        let t = r.render_text();
        for needle in [
            "Knowledge memories",
            "Raw session chunks",
            "Duplicates collapsed",
            "Already imported",
            "Closets dropped",
            "Wing \u{2192} project map",
            "pg_ai \u{2192} pg-ai",
            "Skipped",
            "Redaction",
            "Phases",
            "Peak memory",
            "[dry run]",
            "119,179",
            "github_token: 2",
        ] {
            assert!(t.contains(needle), "{needle}\n{t}");
        }
        assert!(!t.contains("ghp_"));
    }

    #[test]
    fn numbers_get_thousands_separators_and_long_lists_are_cut() {
        assert_eq!(num(0), "0");
        assert_eq!(num(999), "999");
        assert_eq!(num(1000), "1,000");
        assert_eq!(num(1_234_567), "1,234,567");
        let mut r = sample_report();
        for i in 0..30 {
            r.projects.insert(
                format!("p{i:02}"),
                ProjectCounts {
                    memories: i,
                    chunks: 0,
                },
            );
            r.wing_map.insert(
                format!("w{i:02}"),
                WingMapEntry {
                    to: format!("p{i:02}"),
                    via: "wing".into(),
                    agent: None,
                    drawers: i,
                },
            );
        }
        let t = r.render_text();
        assert!(t.contains("(31 total)"), "{t}");
        assert!(t.contains("(31 wings)") && t.contains('\u{2026}'), "{t}");
    }

    #[test]
    fn a_report_survives_json() {
        let r = sample_report();
        let back: ImportReport = serde_json::from_str(&serde_json::to_string(&r).unwrap()).unwrap();
        assert_eq!(back, r);
    }
}
