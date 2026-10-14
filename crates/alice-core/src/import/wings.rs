//! Mapping MemPalace wings (and transcript folders) to Alice projects.
//!
//! The rules are data-driven: they use the palace's own wing list, so they work on any palace.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;

use super::palace::DrawerMeta;
use crate::normalize::slugify;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WingTarget {
    /// A project wing (or an alias of one).
    Project { slug: String },
    /// An agent's own wing (`wing_claude_opus_4_7`): the project is inferred per drawer; `agent` is the wing suffix as a slug.
    Agent { agent: String },
    /// The `sessions` wing: transcripts, project from the transcript path.
    Sessions,
    /// The wing slugifies to nothing.
    Unnamed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Via {
    Wing,
    Alias,
    Override,
    TranscriptPath,
    Inferred,
    Global,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectDecision {
    pub slug: String,
    pub via: Via,
    pub agent: Option<String>,
}

const GLOBAL: &str = "global";
const INFER_WINDOW_CHARS: usize = 4_000;
/// Slugs shorter than this are never matched by content or by transcript-folder suffix.
const MIN_MATCH_SLUG_CHARS: usize = 4;

#[derive(Debug, Clone)]
pub struct WingMap {
    /// Raw wing string -> (target, how a project target was reached).
    table: HashMap<String, (WingTarget, Via)>,
    /// slug -> wing spellings (sorted, no duplicates).
    projects: BTreeMap<String, Vec<String>>,
    /// Canon of every non-prefixed wing other than `sessions`.
    plain_canons: BTreeSet<String>,
    /// (raw key, canon of key, project slug) from `--map-wing`.
    overrides: Vec<(String, String, String)>,
}

fn canon(w: &str) -> String {
    slugify(w)
}

fn is_prefixed(canon: &str) -> bool {
    canon.starts_with("wing-")
}

fn add_wing(projects: &mut BTreeMap<String, Vec<String>>, slug: &str, wing: &str) {
    let v = projects.entry(slug.to_string()).or_default();
    if !v.iter().any(|w| w == wing) {
        v.push(wing.to_string());
        v.sort();
    }
}

impl WingMap {
    pub fn build(wings: &[(String, u64)], overrides: &BTreeMap<String, String>) -> WingMap {
        let overrides: Vec<(String, String, String)> = overrides
            .iter()
            .filter_map(|(k, v)| {
                let slug = slugify(v);
                (!slug.is_empty()).then(|| (k.clone(), canon(k), slug))
            })
            .collect();
        let plain_canons: BTreeSet<String> = wings
            .iter()
            .map(|(w, _)| canon(w))
            .filter(|c| !c.is_empty() && c != "sessions" && !is_prefixed(c))
            .collect();
        let mut map = WingMap {
            table: HashMap::new(),
            projects: BTreeMap::new(),
            plain_canons,
            overrides,
        };
        for (w, _) in wings {
            let (t, via) = map.classify(w);
            if let WingTarget::Project { slug } = &t {
                add_wing(&mut map.projects, slug, w);
            }
            map.table.insert(w.clone(), (t, via));
        }
        map
    }

    /// The rules for one wing spelling (used at build time and for wings first seen later).
    fn classify(&self, wing: &str) -> (WingTarget, Via) {
        let c = canon(wing);
        if let Some((_, _, slug)) = self
            .overrides
            .iter()
            .find(|(k, kc, _)| k == wing || (!c.is_empty() && *kc == c))
        {
            return (WingTarget::Project { slug: slug.clone() }, Via::Override);
        }
        if c.is_empty() {
            return (WingTarget::Unnamed, Via::Global);
        }
        if c == "sessions" {
            return (WingTarget::Sessions, Via::Global);
        }
        if let Some(stem) = c.strip_prefix("wing-") {
            return if self.plain_canons.contains(stem) {
                (
                    WingTarget::Project {
                        slug: stem.to_string(),
                    },
                    Via::Alias,
                )
            } else {
                (
                    WingTarget::Agent {
                        agent: stem.to_string(),
                    },
                    Via::Global,
                )
            };
        }
        (WingTarget::Project { slug: c }, Via::Wing)
    }

    pub fn target(&self, wing: &str) -> WingTarget {
        match self.table.get(wing) {
            Some((t, _)) => t.clone(),
            None => self.classify(wing).0,
        }
    }

    /// Project slugs (not agents) with the wing spellings that map to each: slug → wings.
    pub fn projects(&self) -> BTreeMap<String, Vec<String>> {
        self.projects.clone()
    }

    /// alias (as `slugify` gives it) → project slug, for every spelling that differs from the slug.
    pub fn aliases(&self) -> Vec<(String, String)> {
        let mut out = BTreeSet::new();
        for (slug, wings) in &self.projects {
            for w in wings {
                let c = canon(w);
                if !c.is_empty() && c != *slug {
                    out.insert((c, slug.clone()));
                }
            }
        }
        out.into_iter().collect()
    }

    pub fn decide(&self, meta: &DrawerMeta, text: &str) -> ProjectDecision {
        let (target, via) = match self.table.get(&meta.wing) {
            Some(x) => x.clone(),
            None => self.classify(&meta.wing),
        };
        let agent = match &target {
            WingTarget::Agent { agent } => Some(agent.clone()),
            _ => None,
        };
        let decision = |slug: String, via: Via| ProjectDecision {
            slug,
            via,
            agent: agent.clone(),
        };
        // A project wing (including an override) names the project outright.
        if let WingTarget::Project { slug } = target {
            return decision(slug, via);
        }
        // Sessions, and agent or unnamed wings that hold transcripts: the transcript's folder first.
        let transcript = meta
            .source_file
            .as_deref()
            .filter(|f| matches!(target, WingTarget::Sessions) || is_transcript(f));
        if let Some(slug) = transcript.and_then(|f| self.project_for_transcript(f)) {
            return decision(slug, Via::TranscriptPath);
        }
        if !matches!(target, WingTarget::Unnamed)
            && let Some(slug) = self.infer(text)
        {
            return decision(slug, Via::Inferred);
        }
        decision(GLOBAL.to_string(), Via::Global)
    }

    /// Known project (≥ 4 characters) whose spellings occur most often as whole tokens in the
    /// first 4,000 characters; a tie or no hit gives `None`.
    fn infer(&self, text: &str) -> Option<String> {
        let head: String = text.chars().take(INFER_WINDOW_CHARS).collect();
        let head = head.to_lowercase();
        let mut best: Option<(&str, usize)> = None;
        let mut tied = false;
        for slug in self.projects.keys() {
            if slug.chars().count() < MIN_MATCH_SLUG_CHARS {
                continue;
            }
            let spellings: BTreeSet<String> =
                [slug.clone(), slug.replace('-', "_"), slug.replace('-', "")]
                    .into_iter()
                    .collect();
            let n: usize = spellings.iter().map(|s| count_tokens(&head, s)).sum();
            if n == 0 {
                continue;
            }
            match best {
                Some((_, b)) if n < b => {}
                Some((_, b)) if n == b => tied = true,
                _ => {
                    best = Some((slug, n));
                    tied = false;
                }
            }
        }
        best.filter(|_| !tied).map(|(s, _)| s.to_string())
    }

    pub fn project_for_transcript(&self, source_file: &str) -> Option<String> {
        let folder = transcript_folder(source_file)?;
        let tokens = folder_tokens(&folder);
        let mut known: Vec<&String> = self
            .projects
            .keys()
            .filter(|s| s.chars().count() >= MIN_MATCH_SLUG_CHARS)
            .collect();
        known.sort_by(|a, b| b.len().cmp(&a.len()).then_with(|| a.cmp(b)));
        for slug in known {
            let st: Vec<&str> = slug.split('-').collect();
            if tokens.len() >= st.len() && tokens[tokens.len() - st.len()..] == st[..] {
                return Some(slug.clone());
            }
        }
        derived_slug(&tokens)
    }

    /// Called by the survey for each transcript path: a folder that decodes to a project nobody
    /// named adds that project (wing spelling `transcript:<folder>`).
    pub fn note_transcript_dir(&mut self, source_file: &str) {
        let (Some(folder), Some(slug)) = (
            transcript_folder(source_file),
            self.project_for_transcript(source_file),
        ) else {
            return;
        };
        if !self.projects.contains_key(&slug) {
            add_wing(&mut self.projects, &slug, &format!("transcript:{folder}"));
        }
    }
}

fn is_transcript(source_file: &str) -> bool {
    source_file.to_ascii_lowercase().ends_with(".jsonl")
}

fn transcript_folder(source_file: &str) -> Option<String> {
    let name = Path::new(source_file).parent()?.file_name()?;
    let name = name.to_string_lossy().into_owned();
    (!name.is_empty()).then_some(name)
}

fn folder_tokens(folder: &str) -> Vec<String> {
    folder
        .to_lowercase()
        .split('-')
        .filter(|t| !t.is_empty())
        .map(str::to_string)
        .collect()
}

/// The tokens after the first `code`, a leading `ai` dropped when something follows.
fn derived_slug(tokens: &[String]) -> Option<String> {
    let i = tokens.iter().position(|t| t == "code")?;
    let mut rest = &tokens[i + 1..];
    if rest.len() > 1 && rest[0] == "ai" {
        rest = &rest[1..];
    }
    let slug = slugify(&rest.join("-"));
    (!slug.is_empty()).then_some(slug)
}

/// Occurrences of `needle` in `hay` whose neighbours are not ASCII alphanumeric.
fn count_tokens(hay: &str, needle: &str) -> usize {
    let b = hay.as_bytes();
    hay.match_indices(needle)
        .filter(|(i, _)| {
            let before_ok = *i == 0 || !b[*i - 1].is_ascii_alphanumeric();
            let end = i + needle.len();
            let after_ok = end >= b.len() || !b[end].is_ascii_alphanumeric();
            before_ok && after_ok
        })
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn counts(ws: &[&str]) -> Vec<(String, u64)> {
        ws.iter().map(|w| (w.to_string(), 1)).collect()
    }

    #[test]
    fn spellings_of_one_project_collapse_and_agent_wings_stay_out() {
        let m = WingMap::build(
            &counts(&[
                "pg-ai",
                "pg_ai",
                "wing_pg_ai",
                "wing_claude_opus_4.7",
                "wing_impl_t5",
                "sessions",
                "devspace",
                "wing_devspace",
            ]),
            &Default::default(),
        );
        assert_eq!(
            m.target("pg_ai"),
            WingTarget::Project {
                slug: "pg-ai".into()
            }
        );
        assert_eq!(
            m.target("wing_pg_ai"),
            WingTarget::Project {
                slug: "pg-ai".into()
            }
        );
        assert_eq!(
            m.target("wing_devspace"),
            WingTarget::Project {
                slug: "devspace".into()
            }
        );
        assert_eq!(
            m.target("wing_claude_opus_4.7"),
            WingTarget::Agent {
                agent: "claude-opus-4-7".into()
            }
        );
        assert_eq!(
            m.target("wing_impl_t5"),
            WingTarget::Agent {
                agent: "impl-t5".into()
            }
        );
        assert_eq!(m.target("sessions"), WingTarget::Sessions);
        let slugs: Vec<_> = m.projects().keys().cloned().collect();
        assert_eq!(slugs, vec!["devspace", "pg-ai"]);
        assert!(m.aliases().contains(&("wing-pg-ai".into(), "pg-ai".into())));
    }

    #[test]
    fn aliases_never_repeat_the_slug_and_have_no_duplicates() {
        let m = WingMap::build(
            &counts(&["pg-ai", "pg_ai", "PG.AI", "wing_pg_ai", "wing-pg-ai"]),
            &Default::default(),
        );
        let a = m.aliases();
        assert_eq!(a, vec![("wing-pg-ai".to_string(), "pg-ai".to_string())]);
        assert!(a.iter().all(|(alias, slug)| alias != slug));
        let p = m.projects();
        assert_eq!(p.len(), 1);
        assert_eq!(p["pg-ai"].len(), 5);
    }

    #[test]
    fn an_override_beats_the_rules_and_can_name_a_new_project() {
        let mut o = BTreeMap::new();
        o.insert("wing_project_alice".to_string(), "alice".to_string());
        let m = WingMap::build(&counts(&["alice", "wing_project_alice"]), &o);
        assert_eq!(
            m.target("wing_project_alice"),
            WingTarget::Project {
                slug: "alice".into()
            }
        );
        let d = m.decide(
            &DrawerMeta {
                wing: "wing_project_alice".into(),
                ..Default::default()
            },
            "x",
        );
        assert_eq!((d.slug.as_str(), d.via), ("alice", Via::Override));
        // An override naming a project nobody else names creates it; the key may be given by canon.
        let mut o = BTreeMap::new();
        o.insert("Wing Scratch".to_string(), "Brand New".to_string());
        let m = WingMap::build(&counts(&["wing_scratch", "pg-ai"]), &o);
        assert_eq!(
            m.target("wing_scratch"),
            WingTarget::Project {
                slug: "brand-new".into()
            }
        );
        assert_eq!(
            m.projects().keys().cloned().collect::<Vec<_>>(),
            vec!["brand-new", "pg-ai"]
        );
    }

    #[test]
    fn an_agent_wing_takes_the_project_its_text_mentions_most() {
        let m = WingMap::build(
            &counts(&["pg-ai", "devspace", "wing_claude_code"]),
            &Default::default(),
        );
        let meta = DrawerMeta {
            wing: "wing_claude_code".into(),
            ..Default::default()
        };
        let d = m.decide(
            &meta,
            "Fixed the pg_ai v4 frame bug; pgai tests pass. (devspace unaffected)",
        );
        assert_eq!(
            (d.slug.as_str(), d.via, d.agent.as_deref()),
            ("pg-ai", Via::Inferred, Some("claude-code"))
        );
        let tie = m.decide(&meta, "pg-ai and devspace once each");
        assert_eq!((tie.slug.as_str(), tie.via), ("global", Via::Global));
        assert_eq!(tie.agent.as_deref(), Some("claude-code"));
        let none = m.decide(&meta, "nothing relevant, mentions spg-aix only");
        assert_eq!(none.slug, "global"); // "spg-aix" is not a whole token
    }

    #[test]
    fn transcript_folders_decode_to_projects() {
        let m = WingMap::build(
            &counts(&["pg-ai", "games-platform", "alice", "sessions"]),
            &Default::default(),
        );
        let p = |f: &str| m.project_for_transcript(f);
        assert_eq!(
            p("/Users/x/.claude/projects/-Users-icue-Code-payments-quickpay-pg-ai/a.jsonl")
                .as_deref(),
            Some("pg-ai")
        );
        assert_eq!(
            p("/Users/x/.claude/projects/-Users-icue-Code-games-platform/a.jsonl").as_deref(),
            Some("games-platform")
        );
        assert_eq!(
            p("/Users/x/.claude/projects/-Users-icue-Code-AI-project-alice/a.jsonl").as_deref(),
            Some("alice")
        );
        // unknown project: taken from the folder name after `code`, `ai` dropped
        assert_eq!(
            p("/Users/x/.claude/projects/-Users-icue-Code-AI-gsb-app-verify/a.jsonl").as_deref(),
            Some("gsb-app-verify")
        );
        assert_eq!(p("/tmp/loose/a.jsonl"), None);
    }

    #[test]
    fn the_longest_known_slug_wins_and_a_short_slug_never_matches_by_suffix() {
        let m = WingMap::build(
            &counts(&["alice", "project-alice", "app"]),
            &Default::default(),
        );
        assert_eq!(
            m.project_for_transcript("/p/-Users-icue-Code-AI-project-alice/a.jsonl")
                .as_deref(),
            Some("project-alice")
        );
        // `app` has 3 characters: it must not swallow a folder that merely ends in `-app`.
        assert_eq!(
            m.project_for_transcript("/p/-Users-icue-Code-AI-gsb-app/a.jsonl")
                .as_deref(),
            Some("gsb-app")
        );
        // Without `code` in the folder and no suffix match there is nothing to say.
        assert_eq!(m.project_for_transcript("/p/-Users-icue-app/a.jsonl"), None);
        // `ai` alone after `code` is kept (nothing follows it to prefer).
        assert_eq!(
            m.project_for_transcript("/p/-Users-icue-Code-AI/a.jsonl")
                .as_deref(),
            Some("ai")
        );
        // Nothing after `code`: no project.
        assert_eq!(
            m.project_for_transcript("/p/-Users-icue-Code/a.jsonl"),
            None
        );
    }

    #[test]
    fn an_unknown_transcript_folder_becomes_a_project_once_noted() {
        let mut m = WingMap::build(&counts(&["pg-ai", "sessions"]), &Default::default());
        let f = "/Users/x/.claude/projects/-Users-me-Code-AI-gsb-app-verify/a.jsonl";
        assert!(!m.projects().contains_key("gsb-app-verify"));
        m.note_transcript_dir(f);
        assert_eq!(
            m.projects().get("gsb-app-verify"),
            Some(&vec![
                "transcript:-Users-me-Code-AI-gsb-app-verify".to_string()
            ])
        );
        // Noting again, or noting a known project's folder, changes nothing.
        m.note_transcript_dir(f);
        m.note_transcript_dir("/p/-Users-me-Code-payments-pg-ai/b.jsonl");
        m.note_transcript_dir("/tmp/loose/a.jsonl");
        let p = m.projects();
        assert_eq!(p["gsb-app-verify"].len(), 1);
        assert_eq!(p["pg-ai"], vec!["pg-ai".to_string()]);
        assert_eq!(p.len(), 2);
        // The new project is now known to the suffix rule and to decide().
        let meta = DrawerMeta {
            wing: "sessions".into(),
            source_file: Some(f.into()),
            ..Default::default()
        };
        assert_eq!(m.decide(&meta, "x").slug, "gsb-app-verify");
    }

    #[test]
    fn a_sessions_drawer_with_a_decodable_path_uses_the_path() {
        let m = WingMap::build(
            &counts(&["pg-ai", "devspace", "sessions"]),
            &Default::default(),
        );
        let meta = DrawerMeta {
            wing: "sessions".into(),
            source_file: Some("/u/.claude/projects/-Users-icue-Code-AI-devspace/a.jsonl".into()),
            ..Default::default()
        };
        // The text names another project; the path still wins.
        let d = m.decide(&meta, "pg-ai pg-ai pg-ai");
        assert_eq!(
            (d.slug.as_str(), d.via, d.agent),
            ("devspace", Via::TranscriptPath, None)
        );
    }

    #[test]
    fn a_sessions_drawer_without_a_decodable_path_falls_back_to_its_text_then_global() {
        let m = WingMap::build(&counts(&["pg-ai", "sessions"]), &Default::default());
        let meta = DrawerMeta {
            wing: "sessions".into(),
            source_file: Some("/tmp/loose/a.jsonl".into()),
            ..Default::default()
        };
        assert_eq!(m.decide(&meta, "talking about pg-ai here").slug, "pg-ai");
        assert_eq!(m.decide(&meta, "talking about nothing").slug, "global");
    }

    #[test]
    fn odd_wing_names_do_not_panic() {
        let m = WingMap::build(
            &counts(&["", "   ", "---", "wing_", "ไทย", "Wing-Mixed_Case"]),
            &Default::default(),
        );
        assert_eq!(m.target(""), WingTarget::Unnamed);
        assert_eq!(m.target("---"), WingTarget::Unnamed);
        assert!(matches!(m.target("ไทย"), WingTarget::Project { .. }));
        // `wing_` slugifies to `wing`: an ordinary project wing, not an unnamed one.
        assert_eq!(
            m.target("wing_"),
            WingTarget::Project {
                slug: "wing".into()
            }
        );
        assert_eq!(
            m.target("Wing-Mixed_Case"),
            WingTarget::Agent {
                agent: "mixed-case".into()
            }
        );
        let d = m.decide(
            &DrawerMeta {
                wing: "   ".into(),
                ..Default::default()
            },
            "x",
        );
        assert_eq!(d.slug, "global");
        assert_eq!(d.via, Via::Global);
    }
}
