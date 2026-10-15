//! Mapping MemPalace wings (and transcript folders) to Alice projects.
//!
//! The rules are data-driven: they use the palace's own wing list, so they work on any palace.
//!
//! A diary picks its project by these rules (`WingMap::decide_diary`), first match wins, and
//! none reads more of the drawer text than the existing 4,000-character project-name scan:
//! 1. `--map-wing` always wins.
//! 2. Its topic equals a known project slug or starts with `<slug>-` (longest slug), unless the wing's
//!    own project already contains that slug as whole tokens (`alice` in `project-alice`).
//! 3. A project wing names its project.
//! 4. An agent wing whose name ends in a known project slug (`wing_claude_code_pgai`) names it.
//! 5. Otherwise as for any drawer: transcript folder, project named in the text, `global`.
//!
//! Rules 2 and 4 only trust an established project: its wings hold at least `ESTABLISHED_DRAWERS`
//! drawers in total, or one of its wings is named by `--map-wing`. Rule 2 knows a project by its
//! slug and by the spellings of its wings; rule 4 needs a slug of at least 4 characters with an
//! ASCII letter or digit that is not made only of generic agent-name words (`code`, `claude`...).
//!
//! `test` is never a project inferred for a diary (topic, agent wing or text); only a diary
//! filed in the `test` wing itself follows it. Auto-save diaries (topic `checkpoint`) follow
//! the same rules; their weight is set in `plan.rs`.

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
    /// A diary whose own topic names a known project.
    Topic,
    /// A diary in an agent wing whose name ends in a known project.
    AgentWing,
    Global,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectDecision {
    pub slug: String,
    pub via: Via,
    pub agent: Option<String>,
}

const GLOBAL: &str = "global";
/// A common word that is also a wing name: never a project inferred for a diary.
const NEVER_FROM_A_DIARY: &str = "test";
/// A project is established (trusted by the diary topic and agent-wing rules) when its wings
/// hold at least this many drawers in all, or a wing of it is named by `--map-wing`.
pub const ESTABLISHED_DRAWERS: u64 = 15;
/// Words of common agent wing names (`wing_claude_code`, `wing_general_purpose`...): a project
/// named only of these is never taken from the end of an agent wing's name.
const GENERIC_AGENT_WORDS: [&str; 9] = [
    "code", "claude", "codex", "opus", "fable", "general", "purpose", "lead", "reviewer",
];
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
    /// slug -> drawers in all its wings (transcript-derived projects have none).
    weight: BTreeMap<String, u64>,
    /// Projects with a wing named by `--map-wing`.
    explicit: BTreeSet<String>,
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
            weight: BTreeMap::new(),
            explicit: BTreeSet::new(),
        };
        for (w, n) in wings {
            let (t, via) = map.classify(w);
            if let WingTarget::Project { slug } = &t {
                add_wing(&mut map.projects, slug, w);
                *map.weight.entry(slug.clone()).or_default() += n;
                if via == Via::Override {
                    map.explicit.insert(slug.clone());
                }
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

    /// How `wing` reaches its target (for the report's wing map).
    pub fn via(&self, wing: &str) -> Via {
        match self.table.get(wing) {
            Some((_, v)) => *v,
            None => self.classify(wing).1,
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
        self.decide_with(meta, text, false)
    }

    /// `decide` for a diary, whose topic and agent wing carry project signals a plain drawer lacks.
    /// Rules, first match wins (`--map-wing` always wins; `test` is never inferred):
    /// 1. the diary's topic equals a known project slug, or starts with `<slug>-`: that project (longest slug),
    ///    unless the wing's own project already contains that slug as whole tokens (`alice` in `project-alice`);
    /// 2. otherwise a project wing names the project (as for any drawer);
    /// 3. otherwise an agent wing whose name ends in a known project slug (`wing_claude_code_pgai`) names it;
    /// 4. otherwise the transcript folder, then the drawer text, then `global` (as for any drawer), never `test`.
    pub fn decide_diary(&self, meta: &DrawerMeta, text: &str) -> ProjectDecision {
        let (target, via) = match self.table.get(&meta.wing) {
            Some(x) => x.clone(),
            None => self.classify(&meta.wing),
        };
        if via == Via::Override {
            return self.decide(meta, text);
        }
        let agent = match &target {
            WingTarget::Agent { agent } => Some(agent.clone()),
            _ => None,
        };
        let wing_project = match &target {
            WingTarget::Project { slug } => Some(slug.as_str()),
            _ => None,
        };
        if let Some(slug) = meta
            .topic
            .as_deref()
            .and_then(|t| self.project_for_topic(t))
            && !wing_project.is_some_and(|w| contains_tokens(w, &slug))
        {
            return ProjectDecision {
                slug,
                via: Via::Topic,
                agent,
            };
        }
        if wing_project.is_none()
            && let WingTarget::Agent { agent: a } = &target
            && let Some(slug) = self.project_for_agent_wing(a)
        {
            return ProjectDecision {
                slug,
                via: Via::AgentWing,
                agent: Some(a.clone()),
            };
        }
        self.decide_with(meta, text, true)
    }

    /// The established project (not `test`) with the longest name (≥ 4 characters: its slug or
    /// the spelling of one of its wings) that the topic equals or starts with.
    fn project_for_topic(&self, topic: &str) -> Option<String> {
        let t = slugify(topic);
        self.known_slugs()
            .filter_map(|slug| {
                let best = self
                    .names_of(slug)
                    .into_iter()
                    .filter(|n| t == *n || t.starts_with(&format!("{n}-")))
                    .map(|n| n.len())
                    .max()?;
                Some((best, slug))
            })
            .max_by_key(|(len, _)| *len)
            .map(|(_, slug)| slug.clone())
    }

    /// The slug and the canon of every wing spelling of a project, each ≥ 4 characters.
    fn names_of(&self, slug: &str) -> BTreeSet<String> {
        let mut names = BTreeSet::from([slug.to_string()]);
        for w in self.projects.get(slug).into_iter().flatten() {
            if !w.starts_with("transcript:") {
                names.insert(canon(w));
            }
        }
        names
            .into_iter()
            .filter(|n| n.chars().count() >= MIN_MATCH_SLUG_CHARS && n != NEVER_FROM_A_DIARY)
            .collect()
    }

    /// Whether the diary rules may trust `slug`: see [`ESTABLISHED_DRAWERS`].
    fn is_established(&self, slug: &str) -> bool {
        self.explicit.contains(slug)
            || self.weight.get(slug).copied().unwrap_or(0) >= ESTABLISHED_DRAWERS
    }

    /// Longest established project slug that ends the agent-wing name at a token boundary (`pgai` = `pg-ai`).
    fn project_for_agent_wing(&self, agent: &str) -> Option<String> {
        let toks: Vec<&str> = agent.split(['-', '_']).filter(|t| !t.is_empty()).collect();
        self.known_slugs()
            .filter(|s| {
                let want = compact(s);
                !want.is_empty()
                    && !s.split('-').all(|t| GENERIC_AGENT_WORDS.contains(&t))
                    && (0..toks.len()).any(|i| compact(&toks[i..].join("")) == want)
            })
            .max_by_key(|s| s.len())
            .cloned()
    }

    fn known_slugs(&self) -> impl Iterator<Item = &String> {
        self.projects.keys().filter(|s| {
            s.chars().count() >= MIN_MATCH_SLUG_CHARS
                && s.as_str() != NEVER_FROM_A_DIARY
                && self.is_established(s)
        })
    }

    fn decide_with(&self, meta: &DrawerMeta, text: &str, diary: bool) -> ProjectDecision {
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
            && let Some(slug) = self.infer(text, diary)
        {
            return decision(slug, Via::Inferred);
        }
        decision(GLOBAL.to_string(), Via::Global)
    }

    /// Known project (≥ 4 characters) whose spellings occur most often as whole tokens in the
    /// first 4,000 characters; a tie or no hit gives `None`.
    fn infer(&self, text: &str, diary: bool) -> Option<String> {
        let head: String = text.chars().take(INFER_WINDOW_CHARS).collect();
        let head = head.to_lowercase();
        let mut best: Option<(&str, usize)> = None;
        let mut tied = false;
        for slug in self.projects.keys() {
            if slug.chars().count() < MIN_MATCH_SLUG_CHARS || (diary && slug == NEVER_FROM_A_DIARY)
            {
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

    /// Called by the survey for each transcript chunk: a folder that decodes to a project nobody
    /// named adds that project (wing spelling `transcript:<folder>`), and every chunk adds one
    /// to the weight of the project (see [`ESTABLISHED_DRAWERS`]).
    pub fn note_transcript_dir(&mut self, source_file: &str) {
        let (Some(folder), Some(slug)) = (
            transcript_folder(source_file),
            self.project_for_transcript(source_file),
        ) else {
            return;
        };
        *self.weight.entry(slug.clone()).or_default() += 1;
        if !self.projects.contains_key(&slug) {
            add_wing(&mut self.projects, &slug, &format!("transcript:{folder}"));
        }
    }
}

fn compact(s: &str) -> String {
    s.chars().filter(char::is_ascii_alphanumeric).collect()
}

/// Whether `needle`'s hyphen-separated tokens occur as a contiguous run inside `hay`'s.
fn contains_tokens(hay: &str, needle: &str) -> bool {
    let h: Vec<&str> = hay.split('-').collect();
    let n: Vec<&str> = needle.split('-').collect();
    h.windows(n.len()).any(|w| w == n.as_slice())
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

    // ---- diary inference ----

    fn diary(wing: &str, topic: &str) -> DrawerMeta {
        DrawerMeta {
            wing: wing.into(),
            room: "diary".into(),
            topic: Some(topic.into()),
            ..Default::default()
        }
    }

    /// Wings with `n` drawers each.
    fn sized(ws: &[&str], n: u64) -> Vec<(String, u64)> {
        ws.iter().map(|w| (w.to_string(), n)).collect()
    }

    fn diary_map() -> WingMap {
        let mut wings = sized(
            &[
                "tara_valley",
                "tara-stock-market",
                "tara_stock",
                "alice",
                "project-alice",
                "pg-ai",
                "devspace",
                "sessions",
                "wing_claude_code",
                "wing_claude_code_pgai",
                "wing_project_alice_m3",
            ],
            ESTABLISHED_DRAWERS,
        );
        wings.push(("test".into(), 1));
        let mut m = WingMap::build(&wings, &Default::default());
        m.note_transcript_dir("/h/.claude/projects/-Users-me-Code-AI-project-alice-m3/s.jsonl");
        m
    }

    #[test]
    fn a_diary_topic_naming_a_known_project_beats_the_wing_default() {
        let m = diary_map();
        // `tara_valley` is a project wing, but this diary is about tara-stock-market.
        let d = m.decide_diary(&diary("tara_valley", "tara-stock-market"), "x");
        assert_eq!((d.slug.as_str(), d.via), ("tara-stock-market", Via::Topic));
        // Longest slug wins and a topic that merely starts with the slug counts.
        let d = m.decide_diary(&diary("wing_claude_code", "tara-stock-market-m2"), "x");
        assert_eq!(d.slug, "tara-stock-market");
        let d = m.decide_diary(&diary("wing_claude_code", "devspace-chat-tabs"), "x");
        assert_eq!((d.slug.as_str(), d.via), ("devspace", Via::Topic));
        // The topic must be the slug or start with `<slug>-`: a longer word is no match.
        let d = m.decide_diary(&diary("wing_claude_code", "devspaces-notes"), "x");
        assert_eq!(d.slug, "global");
        // The wing's own topic about itself changes nothing.
        let d = m.decide_diary(&diary("tara_valley", "tara-valley-sales"), "x");
        assert_eq!(d.slug, "tara-valley");
    }

    #[test]
    fn a_topic_naming_part_of_the_wings_own_project_does_not_move_the_diary() {
        let m = diary_map();
        let d = m.decide_diary(&diary("wing_project_alice", "alice-m1-task14"), "x");
        assert_eq!(d.slug, "project-alice");
        // Without that wing the same topic does name `alice`.
        let d = m.decide_diary(&diary("wing_claude_code", "alice-design"), "x");
        assert_eq!((d.slug.as_str(), d.via), ("alice", Via::Topic));
        let d = m.decide_diary(&diary("memory_vault", "alice-design"), "x");
        assert_eq!(d.slug, "alice");
    }

    #[test]
    fn an_agent_wing_ending_in_a_known_project_names_it() {
        let m = diary_map();
        let d = m.decide_diary(&diary("wing_claude_code_pgai", "checkpoint"), "x");
        assert_eq!((d.slug.as_str(), d.via), ("pg-ai", Via::AgentWing));
        assert_eq!(d.agent.as_deref(), Some("claude-code-pgai"));
        // A project known only from a few transcript chunks has no weight: not trusted here...
        let d = m.decide_diary(&diary("wing_project_alice_m3", "checkpoint"), "x");
        assert_eq!(d.slug, "global");
        // ...and with enough chunks it is.
        let mut m = m;
        for _ in 0..ESTABLISHED_DRAWERS {
            m.note_transcript_dir("/h/.claude/projects/-Users-me-Code-AI-project-alice-m3/s.jsonl");
        }
        let d = m.decide_diary(&diary("wing_project_alice_m3", "checkpoint"), "x");
        assert_eq!(d.slug, "project-alice-m3");
        // Not at a token boundary: no match.
        let d = m.decide_diary(&diary("wing_claude_code", "checkpoint"), "x");
        assert_eq!(d.slug, "global");
    }

    #[test]
    fn test_is_never_a_project_inferred_for_a_diary() {
        let m = diary_map();
        let text = "ran the test suite, then another test, test test";
        // Text inference would pick `test` for an ordinary drawer...
        let plain = DrawerMeta {
            wing: "wing_claude_code".into(),
            ..Default::default()
        };
        assert_eq!(m.decide(&plain, text).slug, "test");
        // ...but not for a diary, by text or by topic.
        let d = m.decide_diary(&diary("wing_claude_code", "deployment"), text);
        assert_eq!(d.slug, "global");
        let d = m.decide_diary(&diary("wing_claude_code", "test-run"), "x");
        assert_eq!(d.slug, "global");
        // A diary filed in the `test` wing itself still follows that wing.
        assert_eq!(
            m.decide_diary(&diary("test", "deployment"), text).slug,
            "test"
        );
    }

    #[test]
    fn map_wing_still_beats_every_diary_rule() {
        let ov: BTreeMap<String, String> = [("tara_valley".to_string(), "farm".to_string())].into();
        let m = WingMap::build(&counts(&["tara_valley", "tara-stock-market"]), &ov);
        let d = m.decide_diary(&diary("tara_valley", "tara-stock-market"), "x");
        assert_eq!((d.slug.as_str(), d.via), ("farm", Via::Override));
    }

    #[test]
    fn a_tiny_project_does_not_capture_diaries_by_topic() {
        let mut wings = sized(&["pg-ai", "wing_claude_code"], 500);
        wings.extend([
            ("docs".into(), 3),
            ("demo".into(), 1),
            ("notes".into(), ESTABLISHED_DRAWERS - 1),
        ]);
        let m = WingMap::build(&wings, &Default::default());
        for topic in ["docs", "docs-update", "demo-run", "notes-x"] {
            let d = m.decide_diary(&diary("wing_claude_code", topic), "x");
            assert_eq!(d.slug, "global", "{topic}");
        }
        // The same project with enough drawers is trusted; so is an explicit `--map-wing`.
        wings[4].1 = ESTABLISHED_DRAWERS;
        let m = WingMap::build(&wings, &Default::default());
        let d = m.decide_diary(&diary("wing_claude_code", "demo-run"), "x");
        assert_eq!(d.slug, "global", "demo is still tiny");
        let d = m.decide_diary(&diary("wing_claude_code", "notes-x"), "x");
        assert_eq!((d.slug.as_str(), d.via), ("notes", Via::Topic));
        let ov: BTreeMap<String, String> = [("demo".to_string(), "demo".to_string())].into();
        let m = WingMap::build(&wings, &ov);
        let d = m.decide_diary(&diary("wing_claude_code", "demo-run"), "x");
        assert_eq!((d.slug.as_str(), d.via), ("demo", Via::Topic));
    }

    #[test]
    fn a_topic_may_name_a_project_by_the_spelling_of_one_of_its_wings() {
        let mut wings = sized(&["games-platform", "wing_claude_code"], 300);
        wings.push(("slotgames".into(), 5));
        let ov: BTreeMap<String, String> =
            [("slotgames".to_string(), "games-platform".to_string())].into();
        let m = WingMap::build(&wings, &ov);
        let d = m.decide_diary(&diary("wing_claude_code", "slotgames-reels"), "x");
        assert_eq!((d.slug.as_str(), d.via), ("games-platform", Via::Topic));
        // Without the mapping the spelling is its own tiny project: no capture.
        let m = WingMap::build(&wings, &Default::default());
        let d = m.decide_diary(&diary("wing_claude_code", "slotgames-reels"), "x");
        assert_eq!(d.slug, "global");
    }

    #[test]
    fn the_agent_wing_suffix_rule_skips_generic_junk_and_unspellable_slugs() {
        let wings = sized(
            &[
                "code",
                "lead",
                "reviewer",
                "purpose",
                "opus",
                "ไทย",
                "app",
                "pg-ai",
                "wing_claude_code",
                "wing_claude_code_lead",
                "wing_general_purpose",
                "wing_the_reviewer",
                "wing_x_opus",
                "wing_a_lead_ไทย",
                "wing_x_app",
                "wing_claude_code_pgai",
            ],
            500,
        );
        let m = WingMap::build(&wings, &Default::default());
        for w in [
            "wing_claude_code",
            "wing_claude_code_lead",
            "wing_general_purpose",
            "wing_the_reviewer",
            "wing_x_opus",
            "wing_a_lead_ไทย",
            "wing_x_app",
        ] {
            let d = m.decide_diary(&diary(w, "checkpoint"), "x");
            assert_eq!(d.slug, "global", "{w}");
        }
        // A real project still works on the same map.
        let d = m.decide_diary(&diary("wing_claude_code_pgai", "checkpoint"), "x");
        assert_eq!((d.slug.as_str(), d.via), ("pg-ai", Via::AgentWing));
        // A tiny project is not trusted by the suffix rule either.
        let m = WingMap::build(
            &[("zebra".into(), 2), ("wing_claude_zebra".into(), 9)],
            &Default::default(),
        );
        let d = m.decide_diary(&diary("wing_claude_zebra", "checkpoint"), "x");
        assert_eq!(d.slug, "global");
    }
}
