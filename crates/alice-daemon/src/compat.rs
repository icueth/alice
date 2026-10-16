//! The MemPalace compatibility layer: `mempalace_*` tool calls answered from Alice's own data, in
//! the shapes MemPalace's tools return, so prompts and hooks written for it keep working.
//!
//! This is a plain function of the [`Service`] (`compat_call`): the HTTP route, and later the
//! MCP adapter, only carry a call to it. A call never fails at the protocol level; every
//! failure is a normal result `{"error": "..."}`, like MemPalace's own. Every text returned was
//! redacted when it was stored, and a source is named by its file name, not its path.
//!
//! `mempalace_search` goes through [`Service::recall`], so, like a normal recall, it records
//! that the memories it returns were accessed (the access data that feeds ranking); nothing
//! else is written. `similarity` is relative: the best hit of a search is 1.0 and the others
//! are their score divided by it, so a threshold on it keeps the best hit always. The keys of
//! `status.wing_project_map` are slugified aliases (`wing-pg-ai`); lookups accept the old
//! spelling (`wing_pg_ai`) too.
//!
//! The write tools (`add_drawer`, `update_drawer`, `delete_drawer`, `diary_write`, `kg_add`,
//! `kg_invalidate`, `kg_supersede`) go through the normal service paths (`remember`, `update`,
//! `assert_fact`), so they are redacted, judged, recorded as events and undoable like any agent
//! write. The caller's agent is the author (an `added_by` or `agent_name` argument wins); with no
//! agent the write is the user's. Agents never create containers: a wing that is not an Alice
//! project files the memory into the caller's current project and the answer says so.
//!
//! Actor mapping: the author of a write is the `added_by` / `agent_name` argument when given,
//! else the call's `agent` (the MCP adapter fills it with the client's name). An author of
//! `cli`, an empty one, or none is recorded as the user; any other string is a named agent, so a
//! caller chooses its own attribution (as MemPalace's own `added_by` allowed). `update_drawer`,
//! `delete_drawer` and the graph tools always act as the call's agent. A `source_file` is stored
//! by file name only (`Source: <name>`), like everything else shown. Everything a write answers
//! with is built from redacted values. `kg_timeline` needs an `entity` (Alice lists one entity's
//! facts); `kg_add` with `valid_to` is two events (add, then end), reported honestly when the
//! second step fails.

use crate::service::Service;
use alice_core::api::{
    ContextRequest, FactsRequest, GetRequest, OutputFormat, RecallRequest, RememberRequest,
    RememberStatus, Tier, UpdateRequest,
};
use alice_core::error::Result;
use alice_core::import::plan::{chunk_suffix, kind_for_room};
use alice_core::model::{Author, Kind, Memory, Status};
use alice_core::normalize::normalize_topic;
use alice_core::store::compat::{self as store, CompatDrawer, Overview};
use alice_core::store::kg::{self, AssertFact, FactAction};
use alice_core::store::memories::{MemoryUpdate, UpdateAction};
use alice_core::store::projects;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// The MemPalace tools Alice answers (the others get an explanation, see [`unsupported`]).
pub const SUPPORTED: [&str; 15] = [
    "mempalace_status",
    "mempalace_search",
    "mempalace_add_drawer",
    "mempalace_diary_write",
    "mempalace_diary_read",
    "mempalace_get_drawer",
    "mempalace_update_drawer",
    "mempalace_delete_drawer",
    "mempalace_kg_query",
    "mempalace_kg_add",
    "mempalace_kg_invalidate",
    "mempalace_kg_supersede",
    "mempalace_kg_timeline",
    "mempalace_list_wings",
    "mempalace_list_rooms",
];

/// What the HTTP route and the MCP adapter answer while `compat.mempalace` is off.
pub const OFF_MESSAGE: &str =
    "MemPalace compatibility is off; turn it on with `alice config set compat.mempalace true`";

pub fn is_compat_name(name: &str) -> bool {
    name.starts_with("mempalace_")
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct CompatCall {
    pub tool: String,
    #[serde(default)]
    pub arguments: Value,
    /// The caller's working directory (decides the project when no wing is given).
    #[serde(default)]
    pub cwd: Option<String>,
    /// The calling agent's name.
    #[serde(default)]
    pub agent: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct CompatResult {
    /// Always a JSON object; a failure is `{"error": "..."}`.
    pub value: Value,
}

/// How long the `status` overview is reused (old prompts call `status` first in every session).
const OVERVIEW_TTL: Duration = Duration::from_secs(5);
/// `status` stays under 6,000 bytes: it is read into every session.
const STATUS_BUDGET: usize = 5_800;
const STATUS_WINGS: usize = 40;
const STATUS_ROOMS: usize = 40;
const STATUS_ALIASES: usize = 30;
const STATUS_CONTEXT_TOKENS: usize = 400;
const SEARCH_DEFAULT_LIMIT: u64 = 5;
const SEARCH_MAX_LIMIT: u64 = 50;
const SEARCH_MAX_TOKENS: usize = 6_000;

/// The tools that change something; their failures also carry `"success": false` (MemPalace's
/// write tools answer so).
const WRITE_TOOLS: [&str; 7] = [
    "mempalace_add_drawer",
    "mempalace_update_drawer",
    "mempalace_delete_drawer",
    "mempalace_diary_write",
    "mempalace_kg_add",
    "mempalace_kg_invalidate",
    "mempalace_kg_supersede",
];
const DIARY_DEFAULT_ENTRIES: u64 = 10;
const DIARY_MAX_ENTRIES: u64 = 100;
/// One diary entry is shown up to this many characters, and an answer up to this many bytes.
const DIARY_ENTRY_CHARS: usize = 4_000;
const DIARY_READ_BYTES: usize = 64_000;
/// `kg_query` shows at most this many facts (a hub entity must not make a huge answer).
const KG_QUERY_MAX_FACTS: usize = 200;
/// Up to this many facts `kg_query` also repeats in the flat `facts` list.
const KG_QUERY_ALL_ROWS: usize = 50;
const TRASHED_REFUSAL: &str =
    "this memory was already forgotten (it is in the trash); `alice undo` brings it back";
const TIMELINE_DEFAULT_LIMIT: u64 = 100;
const TIMELINE_MAX_LIMIT: u64 = 1_000;
const RAW_CHUNK_REFUSAL: &str = "raw transcript chunks cannot be changed one by one; they are the recorded history of a session. Save a corrected memory with mempalace_add_drawer instead";

const PROTOCOL: &str = "This memory is Alice. Search with mempalace_search before answering about past work; save with mempalace_add_drawer; the same data is reachable through Alice's own tools (recall, remember, context).";

/// The overview and the old wing names, as of one moment.
struct Snapshot {
    overview: Overview,
    aliases: BTreeMap<String, String>,
}

/// The short-lived cache behind `status` and `list_wings`.
#[derive(Default)]
pub struct CompatState {
    cached: parking_lot::Mutex<Option<(Instant, std::sync::Arc<Snapshot>)>>,
    runs: AtomicU64,
}

// ---------------------------------------------------------------------------------- arguments

/// A call's arguments with the coercions old clients rely on: numbers and booleans sent as
/// strings are accepted, and a JSON string holding an object is parsed once.
struct Args(Map<String, Value>);

impl Args {
    fn parse(v: Value) -> std::result::Result<Args, String> {
        match v {
            Value::Null => Ok(Args(Map::new())),
            Value::Object(m) => Ok(Args(m)),
            Value::String(s) => match serde_json::from_str::<Value>(&s) {
                Ok(Value::Object(m)) => Ok(Args(m)),
                _ => Err("arguments must be a JSON object".into()),
            },
            _ => Err("arguments must be a JSON object".into()),
        }
    }

    fn get(&self, name: &str) -> Option<&Value> {
        self.0.get(name).filter(|v| !v.is_null())
    }

    fn required_str(&self, name: &str) -> std::result::Result<String, String> {
        match self.get(name) {
            None => Err(format!("missing required argument: {name}")),
            Some(Value::String(s)) if s.trim().is_empty() => {
                Err(format!("missing required argument: {name}"))
            }
            Some(Value::String(s)) => Ok(s.clone()),
            Some(_) => Err(format!("argument {name} must be a string")),
        }
    }

    /// An optional string; an empty one counts as not given.
    fn opt_str(&self, name: &str) -> std::result::Result<Option<String>, String> {
        match self.get(name) {
            None => Ok(None),
            Some(Value::String(s)) if s.trim().is_empty() => Ok(None),
            Some(Value::String(s)) => Ok(Some(s.trim().to_string())),
            Some(_) => Err(format!("argument {name} must be a string")),
        }
    }

    fn opt_u64(&self, name: &str) -> std::result::Result<Option<u64>, String> {
        let bad = || format!("argument {name} must be a whole number");
        match self.get(name) {
            None => Ok(None),
            Some(Value::Number(n)) => n
                .as_u64()
                .or_else(|| {
                    n.as_f64()
                        .filter(|f| *f >= 0.0 && f.fract() == 0.0 && *f < u64::MAX as f64)
                        .map(|f| f as u64)
                })
                .map(Some)
                .ok_or_else(bad),
            Some(Value::String(s)) if s.trim().is_empty() => Ok(None),
            Some(Value::String(s)) => s.trim().parse::<u64>().map(Some).map_err(|_| bad()),
            Some(_) => Err(bad()),
        }
    }

    fn has(&self, name: &str) -> bool {
        self.get(name).is_some()
    }
}

fn error(msg: impl Into<String>) -> Value {
    json!({ "error": msg.into() })
}

/// A failed call: `{"error"}`, and for a write tool also `"success": false`.
fn failure(tool: &str, msg: String) -> Value {
    if WRITE_TOOLS.contains(&tool) {
        json!({ "success": false, "error": msg })
    } else {
        error(msg)
    }
}

/// A day or an RFC 3339 timestamp, as the knowledge graph takes it.
fn check_time(name: &str, v: Option<String>) -> std::result::Result<Option<String>, String> {
    match v {
        Some(t)
            if chrono::NaiveDate::parse_from_str(&t, "%Y-%m-%d").is_err()
                && chrono::DateTime::parse_from_rfc3339(&t).is_err() =>
        {
            Err(format!(
                "argument {name} must be a date (YYYY-MM-DD) or an RFC 3339 timestamp"
            ))
        }
        other => Ok(other),
    }
}

/// An instant for ordering two caller-given times (a bare date is midnight UTC).
fn instant(t: &str) -> chrono::DateTime<chrono::Utc> {
    chrono::DateTime::parse_from_rfc3339(t)
        .map(|d| d.with_timezone(&chrono::Utc))
        .unwrap_or_else(|_| {
            chrono::NaiveDate::parse_from_str(t, "%Y-%m-%d")
                .map(|d| d.and_time(chrono::NaiveTime::MIN).and_utc())
                .unwrap_or_default()
        })
}

fn now_millis() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

/// An error as the caller sees it. A message written for the caller (a bad argument) goes
/// through; anything from the store (a database or file error) could name a path, so it becomes
/// one fixed sentence.
fn safe_error(e: alice_core::error::AliceError) -> String {
    use alice_core::error::AliceError as E;
    match e {
        E::Invalid(_) | E::Refused { .. } => e.to_string(),
        E::NotFound(_) => "not found".into(),
        _ => "Alice could not read its memory store just now; run `alice status` for the cause"
            .into(),
    }
}

/// [`safe_error`] for a write: the same rules, with sentences that say nothing was saved.
fn write_error(e: alice_core::error::AliceError) -> String {
    use alice_core::error::AliceError as E;
    match e {
        E::ReadOnly(_) => "Alice's memory is read-only just now, so this change was not saved; run `alice status` for the cause".into(),
        E::Invalid(_) | E::Refused { .. } => e.to_string(),
        E::NotFound(_) => "not found".into(),
        _ => "Alice could not save that just now; run `alice status` for the cause".into(),
    }
}

fn no_such_wing(wing: &str) -> String {
    format!("no project or alias named {wing}")
}

/// A day, or an ISO timestamp with or without an offset.
fn is_date(s: &str) -> bool {
    chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d").is_ok()
        || chrono::DateTime::parse_from_rfc3339(s).is_ok()
        || chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S%.f").is_ok()
}

/// The last path component (a source is shown by its file name, never its directory).
fn base_name(uri: &str) -> &str {
    uri.rsplit(['/', '\\']).next().unwrap_or(uri)
}

fn round(x: f64, places: i32) -> f64 {
    let f = 10f64.powi(places);
    (x * f).round() / f
}

// -------------------------------------------------------------------------------------- tools

impl Service {
    /// Answers one `mempalace_*` call. Never an `Err`: a failure is `{"error": "..."}`.
    pub async fn compat_call(&self, call: CompatCall) -> Result<CompatResult> {
        let _busy = self.activity().enter();
        let value = match Args::parse(call.arguments.clone()) {
            Ok(args) => match self.compat_dispatch(&call, &args).await {
                Ok(v) => v,
                Err(msg) => failure(&call.tool, msg),
            },
            Err(msg) => failure(&call.tool, msg),
        };
        Ok(CompatResult { value })
    }

    async fn compat_dispatch(
        &self,
        call: &CompatCall,
        args: &Args,
    ) -> std::result::Result<Value, String> {
        let internal = safe_error;
        match call.tool.as_str() {
            "mempalace_status" => self.compat_status(call).await.map_err(internal),
            "mempalace_list_wings" => self.compat_list_wings().await.map_err(internal),
            "mempalace_list_rooms" => self.compat_list_rooms(args).await,
            "mempalace_search" => self.compat_search(call, args).await,
            "mempalace_get_drawer" => self.compat_get_drawer(args).await,
            "mempalace_add_drawer" => self.compat_add_drawer(call, args).await,
            "mempalace_update_drawer" => self.compat_update_drawer(call, args).await,
            "mempalace_delete_drawer" => self.compat_delete_drawer(call, args).await,
            "mempalace_diary_write" => self.compat_diary_write(call, args).await,
            "mempalace_diary_read" => self.compat_diary_read(args).await,
            "mempalace_kg_query" => self.compat_kg_query(args).await,
            "mempalace_kg_add" => self.compat_kg_add(call, args).await,
            "mempalace_kg_invalidate" => self.compat_kg_invalidate(call, args).await,
            "mempalace_kg_supersede" => self.compat_kg_supersede(call, args).await,
            "mempalace_kg_timeline" => self.compat_kg_timeline(args).await,
            name if is_compat_name(name) => Ok(unsupported(name)),
            name => Err(format!(
                "unknown tool {name}; the compatibility tools are named mempalace_*"
            )),
        }
    }

    /// How many times the overview query ran (tests: the cache keeps this from growing).
    pub fn compat_overview_runs(&self) -> u64 {
        self.compat.runs.load(Ordering::Relaxed)
    }

    async fn snapshot(&self) -> Result<std::sync::Arc<Snapshot>> {
        if let Some((at, s)) = &*self.compat.cached.lock()
            && at.elapsed() < OVERVIEW_TTL
        {
            return Ok(s.clone());
        }
        let fresh = self
            .read(|c| {
                Ok(Snapshot {
                    overview: store::overview(c)?,
                    aliases: store::wing_aliases(c)?,
                })
            })
            .await?;
        self.compat.runs.fetch_add(1, Ordering::Relaxed);
        let fresh = std::sync::Arc::new(fresh);
        *self.compat.cached.lock() = Some((Instant::now(), fresh.clone()));
        Ok(fresh)
    }

    async fn compat_status(&self, call: &CompatCall) -> Result<Value> {
        let snap = self.snapshot().await?;
        let o = &snap.overview;
        let top = |m: &BTreeMap<String, u64>, n: usize| -> BTreeMap<String, u64> {
            let mut v: Vec<(&String, &u64)> = m.iter().collect();
            v.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
            v.into_iter()
                .take(n)
                .map(|(k, n)| (k.clone(), *n))
                .collect()
        };
        let aliases: BTreeMap<&String, &String> =
            snap.aliases.iter().take(STATUS_ALIASES).collect();
        let mut out = json!({
            "total_drawers": o.memories + o.chunks,
            "wings": top(&o.wings, STATUS_WINGS),
            "rooms": top(&o.rooms, STATUS_ROOMS),
            "protocol": PROTOCOL,
            "aaak_dialect": "",
            "backend": "alice",
            "alice": true,
            "memories": o.memories,
            "raw_chunks": o.chunks,
            "wing_project_map": aliases,
            "context": "",
        });
        if o.wings.len() > STATUS_WINGS || o.rooms.len() > STATUS_ROOMS {
            out["note"] = json!(
                "wings and rooms show the largest only; mempalace_list_wings and mempalace_list_rooms list them"
            );
        }
        let context = match self
            .context(ContextRequest {
                project: None,
                max_tokens: Some(STATUS_CONTEXT_TOKENS),
                cwd: call.cwd.clone(),
            })
            .await
        {
            Ok(c) => c.text,
            Err(_) => String::new(),
        };
        // The context text is what gives way when the answer would pass the budget.
        let mut keep = context.chars().count();
        loop {
            let text: String = context.chars().take(keep).collect();
            out["context"] = json!(text);
            if serde_json::to_vec(&out).map(|v| v.len()).unwrap_or(0) <= STATUS_BUDGET || keep == 0
            {
                break;
            }
            keep = keep * 4 / 5;
        }
        Ok(out)
    }

    async fn compat_list_wings(&self) -> Result<Value> {
        let snap = self.snapshot().await?;
        Ok(json!({ "wings": snap.overview.wings }))
    }

    async fn compat_list_rooms(&self, args: &Args) -> std::result::Result<Value, String> {
        let wing = args.opt_str("wing")?;
        let Some(wing) = wing else {
            let snap = self.snapshot().await.map_err(safe_error)?;
            return Ok(json!({ "wing": "all", "rooms": snap.overview.rooms }));
        };
        let asked = wing.clone();
        let found = self
            .read(move |c| {
                let Some(p) = projects::resolve_slug(c, &asked)? else {
                    return Ok(None);
                };
                let rooms = store::rooms(c, Some(&p.id))?;
                Ok(Some((p.slug, rooms)))
            })
            .await
            .map_err(safe_error)?;
        Ok(match found {
            Some((slug, rooms)) => json!({ "wing": slug, "rooms": rooms }),
            None => json!({ "wing": wing, "rooms": {}, "note": no_such_wing(&wing) }),
        })
    }

    /// The project a wing names (a slug or any alias), or `None`.
    async fn compat_wing(&self, wing: &str) -> Result<Option<String>> {
        let wing = wing.to_string();
        self.read(move |c| Ok(projects::resolve_slug(c, &wing)?.map(|p| p.slug)))
            .await
    }

    async fn compat_search(
        &self,
        call: &CompatCall,
        args: &Args,
    ) -> std::result::Result<Value, String> {
        let query = args.required_str("query")?;
        let limit = args
            .opt_u64("limit")?
            .unwrap_or(SEARCH_DEFAULT_LIMIT)
            .clamp(1, SEARCH_MAX_LIMIT) as usize;
        let wing = args.opt_str("wing")?;
        let room = args.opt_str("room")?;
        let since = args.opt_str("since")?;
        let before = args.opt_str("before")?;
        for (name, v) in [("since", &since), ("before", &before)] {
            if v.as_deref().is_some_and(|d| !is_date(d)) {
                return Err(format!(
                    "argument {name} must be a date (YYYY-MM-DD) or an ISO timestamp"
                ));
            }
        }
        let source_file = args.opt_str("source_file")?;

        let mut notes: Vec<String> = Vec::new();
        let ignored: Vec<&str> = [
            "max_distance",
            "min_similarity",
            "candidate_strategy",
            "cli_compatible",
            "context",
        ]
        .into_iter()
        .filter(|n| args.has(n))
        .collect();
        if !ignored.is_empty() {
            notes.push(format!(
                "{} {} accepted and ignored; Alice ranks by its own hybrid search",
                ignored.join(", "),
                if ignored.len() == 1 { "is" } else { "are" }
            ));
        }
        if source_file.is_some() {
            notes.push("source_file filtering is not supported; the filter was ignored".into());
        }
        let envelope = |results: Vec<Value>, notes: &[String]| {
            let mut v = json!({
                "query": query,
                "filters": {
                    "wing": wing, "room": room, "source_file": source_file,
                    "since": since, "before": before,
                },
                "total_before_filter": results.len(),
                "results": results,
            });
            if args.has("context") {
                v["context_received"] = json!(true);
            }
            if !notes.is_empty() {
                v["note"] = json!(notes.join("; "));
            }
            v
        };

        let project = match &wing {
            Some(w) => match self.compat_wing(w).await.map_err(safe_error)? {
                Some(slug) => Some(slug),
                None => {
                    notes.push(no_such_wing(w));
                    return Ok(envelope(Vec::new(), &notes));
                }
            },
            None => None,
        };
        // The room as a topic; one that does not normalise names nothing, so nothing matches
        // (never an unfiltered search).
        let room_topic = match &room {
            Some(r) => match normalize_topic(r) {
                Some(t) => Some(t),
                None => {
                    notes.push(format!(
                        "room {r} is not a valid room name, so no memory can match"
                    ));
                    return Ok(envelope(Vec::new(), &notes));
                }
            },
            None => None,
        };
        if room.is_some() {
            notes.push("raw chunks carry no rooms, so with a room only memories can match".into());
        }
        let response = self
            .recall(RecallRequest {
                query: query.clone(),
                project,
                topics: room_topic.iter().cloned().collect(),
                since: since.clone(),
                before: before.clone(),
                include_raw: true,
                format: OutputFormat::Json,
                limit: Some(limit),
                max_tokens: Some(SEARCH_MAX_TOKENS),
                cwd: call.cwd.clone(),
                ..Default::default()
            })
            .await
            .map_err(safe_error)?;
        notes.extend(response.notes.iter().cloned());

        let (mut memory_ids, mut chunk_ids) = (Vec::new(), Vec::new());
        for h in &response.results {
            match h.tier {
                Tier::Knowledge => memory_ids.push(h.id.clone()),
                Tier::Raw => chunk_ids.push(h.id.clone()),
            }
        }
        let details = self
            .read(move |c| store::hit_details(c, &memory_ids, &chunk_ids))
            .await
            .map_err(safe_error)?;
        let best = response
            .results
            .iter()
            .map(|h| h.score)
            .fold(0.0_f64, f64::max);
        let results = response
            .results
            .iter()
            .filter(|h| match &room_topic {
                Some(t) => details.get(&h.id).is_some_and(|d| d.topics.contains(t)),
                None => true,
            })
            .map(|h| {
                let d = details.get(&h.id).cloned().unwrap_or_default();
                let similarity = if best > 0.0 {
                    round((h.score / best).clamp(0.0, 1.0), 3)
                } else {
                    1.0
                };
                let matched: Vec<Value> = h
                    .matched
                    .iter()
                    .filter_map(|m| serde_json::to_value(m).ok())
                    .collect();
                json!({
                    "drawer_id": d.legacy_id.clone().unwrap_or_else(|| h.id.clone()),
                    "text": h.body.clone().unwrap_or_else(|| h.snippet.clone()),
                    "wing": h.project,
                    "room": room_topic.clone().or_else(|| d.topics.first().cloned()).unwrap_or_default(),
                    "source_file": d.source_uri.as_deref().map(base_name).unwrap_or(""),
                    "source_path": "",
                    "created_at": h.created_at,
                    "filed_at": h.created_at,
                    "authored_at": h.created_at,
                    "authored_at_source": "filed_at",
                    "content_date": Value::Null,
                    "content_date_source": "unknown",
                    "similarity": similarity,
                    "distance": round(1.0 - similarity, 4),
                    "effective_distance": round(1.0 - similarity, 4),
                    "closet_boost": 0.0,
                    "matched_via": "drawer",
                    "matched": matched,
                    "alice_id": h.id,
                    "tier": h.tier,
                })
            })
            .collect();
        Ok(envelope(results, &notes))
    }

    async fn compat_get_drawer(&self, args: &Args) -> std::result::Result<Value, String> {
        let id = args.required_str("drawer_id")?;
        let asked = id.clone();
        let found = self
            .read(move |c| store::resolve_drawer(c, &asked))
            .await
            .map_err(safe_error)?;
        match found {
            None => Ok(error(format!(
                "Drawer not found: {}",
                self.redact_echo(&id)
            ))),
            Some(CompatDrawer::Chunk {
                id,
                project_slug,
                text,
                source_uri,
                ord,
                created_at,
            }) => Ok(json!({
                "drawer_id": id,
                "content": text,
                "wing": project_slug,
                "room": "",
                "metadata": {
                    "wing": project_slug,
                    "room": "",
                    "source_file": base_name(&source_uri),
                    "chunk_index": ord,
                    "filed_at": created_at,
                    "last_modified": created_at,
                },
            })),
            Some(CompatDrawer::Memory(memory_id)) => {
                // Through `get`, so a merged duplicate answers with its survivor.
                let got = self
                    .get(GetRequest {
                        ids: vec![memory_id],
                        include_provenance: false,
                    })
                    .await
                    .map_err(safe_error)?;
                let Some(full) = got.memories.into_iter().next() else {
                    return Ok(error(format!(
                        "Drawer not found: {}",
                        self.redact_echo(&id)
                    )));
                };
                let m = full.memory;
                let room = m.topics.first().cloned().unwrap_or_default();
                let mut out = json!({
                    "drawer_id": m.id,
                    "content": m.body,
                    "wing": m.project,
                    "room": room,
                    "metadata": {
                        "wing": m.project,
                        "room": room,
                        "title": m.title,
                        "kind": m.kind,
                        "status": m.status,
                        "importance": m.importance,
                        "topics": m.topics,
                        "added_by": m.author.agent,
                        "filed_at": m.created_at,
                        "last_modified": m.updated_at,
                        "legacy_id": m.legacy_id,
                    },
                });
                let mut notes: Vec<String> = Vec::new();
                notes.extend(self.merged_note(&id, &m));
                match m.status {
                    Status::Trashed => {
                        out["trashed"] = json!(true);
                        notes.push("this memory was forgotten (it is in the trash); it is shown for reference and is not returned by search".into());
                    }
                    Status::Superseded => {
                        notes.push("this memory was superseded by a newer one".into());
                    }
                    _ => {}
                }
                if !notes.is_empty() {
                    out["note"] = json!(notes.join("; "));
                }
                Ok(out)
            }
        }
    }
}

// ------------------------------------------------------------------------------- write tools

/// Where a write goes: a project named by the wing, or (unknown wing) the caller's own.
struct Target {
    /// The project slug to write to; `None` leaves the choice to the caller's `cwd`.
    project: Option<String>,
    note: Option<String>,
}

impl Service {
    async fn compat_target(&self, wing: Option<&str>) -> std::result::Result<Target, String> {
        let Some(wing) = wing else {
            return Ok(Target {
                project: None,
                note: None,
            });
        };
        Ok(match self.compat_wing(wing).await.map_err(write_error)? {
            Some(slug) => Target {
                project: Some(slug),
                note: None,
            },
            None => Target {
                project: None,
                note: Some(format!("wing {wing} is not an Alice project; saved in ")),
            },
        })
    }

    /// The author of a write. A name is a caller's text like any other, so it is redacted
    /// before it is stored (`author_agent`, the event's actor) or shown.
    fn author(&self, call: &CompatCall, name: Option<String>) -> Author {
        Author {
            agent: name
                .or_else(|| {
                    call.agent
                        .as_deref()
                        .map(str::trim)
                        .filter(|a| !a.is_empty())
                        .map(String::from)
                })
                .map(|a| self.redact_echo(&a)),
            ..Default::default()
        }
    }

    async fn compat_add_drawer(
        &self,
        call: &CompatCall,
        args: &Args,
    ) -> std::result::Result<Value, String> {
        let wing = args.required_str("wing")?;
        let room = args.required_str("room")?;
        let content = args.required_str("content")?;
        let source_file = args.opt_str("source_file")?;
        let added_by = args.opt_str("added_by")?;
        let target = self.compat_target(Some(&wing)).await?;
        let content = match &source_file {
            Some(sf) => format!("{content}\nSource: {}", base_name(sf)),
            None => content,
        };
        let topic = normalize_topic(&room);
        let r = self
            .remember_or_spool(RememberRequest {
                content,
                kind: Some(kind_for_room(&room)),
                project: target.project.clone(),
                topics: topic.iter().cloned().collect(),
                cwd: call.cwd.clone(),
                author: self.author(call, added_by),
                ..Default::default()
            })
            .await
            .map_err(write_error)?;
        let mut notes: Vec<String> = Vec::new();
        if topic.is_none() {
            notes.push(format!(
                "room {} is not a usable room name; the memory was saved without a room",
                self.redact_echo(&room)
            ));
        }
        let mut out = match (r.status, r.id) {
            (RememberStatus::Spooled, _) | (_, None) => {
                if let Some(why) = r.reason {
                    notes.push(why);
                }
                json!({ "success": true, "queued": true })
            }
            (RememberStatus::Duplicate, Some(id)) => {
                json!({ "success": true, "reason": "already_exists", "drawer_id": id, "wing": r.project })
            }
            (_, Some(id)) => json!({
                "success": true, "drawer_id": id, "wing": r.project,
                "room": topic.clone().unwrap_or_default(),
            }),
        };
        if let Some(n) = target.note {
            notes.insert(0, format!("{n}{}", r.project));
        }
        if !notes.is_empty() {
            out["note"] = json!(notes.join("; "));
        }
        Ok(out)
    }

    /// What an answer says when `asked` named the memory through the old id of another drawer
    /// (a duplicate the import or the judge merged into it). The ids of the memory's own drawer
    /// are no merge: its own and its old id, and, for a drawer MemPalace cut into fragments, the
    /// fragment ids and the id MemPalace answered for it (they share its old id's base).
    fn merged_note(&self, asked: &str, m: &Memory) -> Option<String> {
        /// A drawer id without Alice's part suffix (`#2`) and MemPalace's fragment suffix.
        fn drawer(id: &str) -> &str {
            let id = match id.rsplit_once('#') {
                Some((base, n)) if !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()) => base,
                _ => id,
            };
            chunk_suffix(id).map_or(id, |(base, _)| base)
        }
        let own = asked == m.id
            || m.legacy_id
                .as_deref()
                .is_some_and(|l| l == asked || drawer(l) == drawer(asked));
        (!own).then(|| format!("{} was merged into {}", self.redact_echo(asked), m.id))
    }

    /// The live-or-not memory a drawer id names; a raw chunk or an unknown id is the caller's
    /// error.
    async fn compat_memory(&self, drawer_id: &str) -> std::result::Result<Memory, String> {
        let asked = drawer_id.to_string();
        let id = match self
            .read(move |c| store::resolve_drawer(c, &asked))
            .await
            .map_err(write_error)?
        {
            Some(CompatDrawer::Memory(id)) => id,
            Some(CompatDrawer::Chunk { .. }) => return Err(RAW_CHUNK_REFUSAL.into()),
            None => {
                return Err(format!("Drawer not found: {}", self.redact_echo(drawer_id)));
            }
        };
        self.get(GetRequest {
            ids: vec![id],
            include_provenance: false,
        })
        .await
        .map_err(write_error)?
        .memories
        .into_iter()
        .next()
        .map(|m| m.memory)
        .ok_or_else(|| format!("Drawer not found: {}", self.redact_echo(drawer_id)))
    }

    async fn compat_update_drawer(
        &self,
        call: &CompatCall,
        args: &Args,
    ) -> std::result::Result<Value, String> {
        let drawer_id = args.required_str("drawer_id")?;
        let content = args.opt_str("content")?;
        let wing = args.opt_str("wing")?;
        let room = args.opt_str("room")?;
        let current = self.compat_memory(&drawer_id).await?;
        if current.status == Status::Trashed {
            return Err(TRASHED_REFUSAL.into());
        }
        let id = current.id.clone();
        if let Some(w) = &wing {
            let same = self.compat_wing(w).await.map_err(write_error)?.as_deref()
                == Some(current.project.as_str());
            if !same {
                return Err("moving a memory to another project is not supported; remember it in the new project and forget this one".into());
            }
        }
        let topics = match &room {
            Some(r) => Some(vec![normalize_topic(r).ok_or_else(|| {
                format!(
                    "argument room \"{}\" is not a usable room name",
                    self.redact_echo(r)
                )
            })?]),
            None => None,
        };
        if content.is_none() && topics.is_none() {
            return Ok(json!({ "success": true, "drawer_id": drawer_id, "noop": true }));
        }
        let done = self
            .update(UpdateRequest {
                id,
                update: MemoryUpdate {
                    action: UpdateAction::Edit,
                    body: content,
                    topics,
                    ..Default::default()
                },
                author: self.author(call, None),
            })
            .await
            .map_err(write_error)?;
        let m = done.memory;
        let mut out = json!({
            "success": true,
            "drawer_id": m.id,
            "wing": m.project,
            "room": m.topics.first().cloned().unwrap_or_default(),
            "chunks": 1,
        });
        if let Some(e) = done.event_id {
            out["event_id"] = json!(e);
        }
        if let Some(n) = self.merged_note(&drawer_id, &current) {
            out["note"] = json!(n);
        }
        Ok(out)
    }

    async fn compat_delete_drawer(
        &self,
        call: &CompatCall,
        args: &Args,
    ) -> std::result::Result<Value, String> {
        let drawer_id = args.required_str("drawer_id")?;
        let current = self.compat_memory(&drawer_id).await?;
        if current.status == Status::Trashed {
            return Err(TRASHED_REFUSAL.into());
        }
        let merged = self.merged_note(&drawer_id, &current);
        let id = current.id;
        let done = self
            .update(UpdateRequest {
                id: id.clone(),
                update: MemoryUpdate {
                    action: UpdateAction::Forget,
                    ..Default::default()
                },
                author: self.author(call, None),
            })
            .await
            .map_err(write_error)?;
        let mut out = json!({
            "success": true,
            "drawer_id": drawer_id,
            "deleted_ids": [id],
            "chunks_deleted": 1,
            "closets_deleted": 0,
            "note": merged.into_iter().chain(["the memory was forgotten (moved to the trash); `alice undo` can bring it back".to_string()]).collect::<Vec<_>>().join("; "),
        });
        if let Some(e) = done.event_id {
            out["event_id"] = json!(e);
        }
        Ok(out)
    }

    async fn compat_diary_write(
        &self,
        call: &CompatCall,
        args: &Args,
    ) -> std::result::Result<Value, String> {
        let agent = self
            .redact_echo(&args.required_str("agent_name")?)
            .trim()
            .to_lowercase();
        let entry = match args.opt_str("entry")? {
            Some(e) => e,
            None => args
                .required_str("content")
                .map_err(|_| "missing required argument: entry".to_string())?,
        };
        let topic = args.opt_str("topic")?;
        let wing = args.opt_str("wing")?;
        let target = self.compat_target(wing.as_deref()).await?;
        // Redacted, then flattened to one line, and only then shortened: a cut made first could
        // leave half a secret that no detector recognises.
        let shown = self
            .redact_echo(&topic.clone().unwrap_or_else(|| "general".into()))
            .split_whitespace()
            .map(|w| w.chars().filter(|c| !c.is_control()).collect::<String>())
            .collect::<Vec<_>>()
            .join(" ");
        // A diary entry is an event, not a fact: the same words from another agent, or from the
        // same agent later, are a new entry. The time and the agent are in the title, which the
        // store's duplicate check reads, so only a retry in the same millisecond collides.
        let stamp = chrono::Utc::now().format("%Y-%m-%d %H:%M:%S%.3f");
        let title = format!(
            "Diary {stamp} {}: {}",
            agent.chars().take(40).collect::<String>(),
            shown.chars().take(80).collect::<String>()
        );
        let r = self
            .remember_or_spool(RememberRequest {
                content: entry,
                kind: Some(Kind::Diary),
                title: Some(title),
                project: target.project.clone(),
                topics: topic.iter().filter_map(|t| normalize_topic(t)).collect(),
                importance: Some(2),
                cwd: call.cwd.clone(),
                author: Author {
                    agent: Some(agent.clone()),
                    ..Default::default()
                },
                ..Default::default()
            })
            .await
            .map_err(write_error)?;
        let mut notes: Vec<String> = Vec::new();
        let mut out = match (r.status, r.id) {
            (RememberStatus::Spooled, _) | (_, None) => {
                if let Some(why) = r.reason {
                    notes.push(why);
                }
                json!({ "success": true, "queued": true, "agent": agent, "topic": shown })
            }
            (RememberStatus::Duplicate, Some(id)) => json!({
                "success": true, "reason": "already_exists", "entry_id": id,
                "agent": agent, "topic": shown, "wing": r.project,
            }),
            (_, Some(id)) => json!({
                "success": true, "entry_id": id, "agent": agent, "topic": shown,
                "timestamp": now_millis(), "wing": r.project,
            }),
        };
        if let Some(n) = target.note {
            notes.insert(0, format!("{n}{}", r.project));
        }
        if !notes.is_empty() {
            out["note"] = json!(notes.join("; "));
        }
        Ok(out)
    }

    async fn compat_diary_read(&self, args: &Args) -> std::result::Result<Value, String> {
        let agent = self
            .redact_echo(&args.required_str("agent_name")?)
            .trim()
            .to_lowercase();
        let last_n = args
            .opt_u64("last_n")?
            .unwrap_or(DIARY_DEFAULT_ENTRIES)
            .clamp(1, DIARY_MAX_ENTRIES) as usize;
        let wing = args.opt_str("wing")?;
        let project = match &wing {
            Some(w) => match self.compat_wing(w).await.map_err(safe_error)? {
                Some(slug) => Some(slug),
                None => {
                    return Ok(json!({
                        "agent": agent, "entries": [], "total": 0, "showing": 0,
                        "note": no_such_wing(w),
                    }));
                }
            },
            None => None,
        };
        let (a, p) = (agent.clone(), project.clone());
        let (rows, total) = self
            .read(move |c| {
                Ok((
                    store::recent_diaries(c, &a, p.as_deref(), last_n)?,
                    store::count_diaries(c, &a, p.as_deref())?,
                ))
            })
            .await
            .map_err(safe_error)?;
        let mut bytes = 0usize;
        let mut cut = false;
        let mut entries: Vec<Value> = Vec::new();
        for m in &rows {
            let content: String = m.body.chars().take(DIARY_ENTRY_CHARS).collect();
            let truncated = content.len() < m.body.len();
            bytes += content.len();
            if bytes > DIARY_READ_BYTES && !entries.is_empty() {
                cut = true;
                break;
            }
            let mut e = json!({
                "date": m.created_at.chars().take(10).collect::<String>(),
                "timestamp": m.created_at,
                "topic": m.topics.first().cloned().unwrap_or_else(|| "general".into()),
                "content": content,
            });
            if truncated {
                e["truncated"] = json!(true);
            }
            entries.push(e);
        }
        let mut out = json!({
            "agent": agent, "entries": entries, "total": total, "showing": entries.len(),
        });
        if total == 0 {
            out["message"] = json!("No diary entries yet.");
        }
        if cut || entries.iter().any(|e| e.get("truncated").is_some()) {
            out["note"] = json!(
                "long entries are shortened and the answer is limited in size; ask for fewer entries (last_n) to see more"
            );
        }
        Ok(out)
    }

    // ------------------------------------------------------------------------ knowledge graph

    async fn compat_kg_query(&self, args: &Args) -> std::result::Result<Value, String> {
        let entity = args.required_str("entity")?;
        let as_of = check_time("as_of", args.opt_str("as_of")?)?;
        let direction = args.opt_str("direction")?.unwrap_or_else(|| "both".into());
        if !["outgoing", "incoming", "both"].contains(&direction.as_str()) {
            return Err("direction must be 'outgoing', 'incoming', or 'both'".into());
        }
        let r = self
            .facts(FactsRequest {
                entity: entity.clone(),
                as_of: as_of.clone(),
                timeline: as_of.is_none(),
                ..Default::default()
            })
            .await
            .map_err(safe_error)?;
        let me = kg::alias_key(&entity);
        let now = now_millis();
        let (mut active, mut historical, mut future) = (Vec::new(), Vec::new(), Vec::new());
        for f in &r.facts {
            let dir = if kg::alias_key(&f.subject) == me {
                "outgoing"
            } else {
                "incoming"
            };
            if direction != "both" && direction != dir {
                continue;
            }
            let row = fact_json(f, Some(dir));
            if as_of.is_some() {
                active.push(row);
            } else if f.valid_from > now {
                future.push(row);
            } else if f.valid_to.as_deref().is_some_and(|t| t <= now.as_str()) {
                historical.push(row);
            } else {
                active.push(row);
            }
        }
        let matched = active.len() + future.len() + historical.len();
        // A hub entity must not make a huge answer: what is true now (active, then future)
        // is kept first, then the newest ended facts; the oldest ended ones are dropped.
        let mut cut: Vec<&str> = Vec::new();
        let mut room = KG_QUERY_MAX_FACTS;
        for (name, rows, newest_first) in [
            ("active_facts", &mut active, false),
            ("future_facts", &mut future, false),
            ("historical_facts", &mut historical, true),
        ] {
            if rows.len() > room {
                cut.push(name);
                if newest_first {
                    rows.drain(..rows.len() - room);
                } else {
                    rows.truncate(room);
                }
            }
            room -= rows.len();
        }
        let shown = active.len() + future.len() + historical.len();
        // `facts` repeats the buckets for old clients; above a handful it is left out.
        let facts: Vec<Value> = if shown <= KG_QUERY_ALL_ROWS {
            active
                .iter()
                .chain(&future)
                .chain(&historical)
                .cloned()
                .collect()
        } else {
            Vec::new()
        };
        let mut out = json!({
            "entity": entity,
            "as_of": as_of,
            "active_facts": active,
            "historical_facts": historical,
            "future_facts": future,
            "facts": facts,
            "count": shown,
        });
        if shown > KG_QUERY_ALL_ROWS {
            out["facts_omitted"] = json!(true);
        }
        if matched > shown {
            out["truncated"] = json!(cut);
            out["total"] = json!(matched);
            out["note"] = json!(format!(
                "only {shown} of {matched} facts are shown: current and future facts first, then the newest ended ones; {} were cut. Use as_of or mempalace_kg_timeline with offset for the rest",
                cut.join(", ")
            ));
        }
        Ok(out)
    }

    async fn compat_kg_add(
        &self,
        call: &CompatCall,
        args: &Args,
    ) -> std::result::Result<Value, String> {
        let subject = args.required_str("subject")?;
        let predicate = args.required_str("predicate")?;
        let object = args.required_str("object")?;
        let valid_from = check_time("valid_from", args.opt_str("valid_from")?)?;
        let valid_to = check_time("valid_to", args.opt_str("valid_to")?)?;
        if let Some(to) = &valid_to {
            let Some(from) = &valid_from else {
                return Err("argument valid_to needs valid_from: say when the fact began".into());
            };
            if instant(to) < instant(from) {
                return Err(
                    "argument valid_to is before valid_from; a fact cannot end before it begins"
                        .into(),
                );
            }
        }
        // A memory the fact came from, when the drawer id names one.
        let source_memory_id = match args.opt_str("source_drawer_id")? {
            Some(d) => self.compat_memory(&d).await.ok().map(|m| m.id),
            None => None,
        };
        let author = self.author(call, None);
        let added = self
            .assert_fact(AssertFact {
                subject: subject.clone(),
                predicate: predicate.clone(),
                object: object.clone(),
                valid_from,
                action: FactAction::Add,
                source_memory_id,
                author: author.clone(),
                ..Default::default()
            })
            .await
            .map_err(write_error)?;
        let fact = self.fact_text(&subject, &added.predicate, &object);
        let mut out = json!({
            "success": true,
            "triple_id": added.relation_id,
            "fact": fact,
        });
        if let Some(s) = added.suggestion {
            out["note"] = json!(format!("a similar predicate exists: {s}"));
        }
        if let Some(until) = valid_to {
            let ended = self
                .assert_fact(AssertFact {
                    subject,
                    predicate,
                    object,
                    valid_from: Some(until),
                    action: FactAction::End,
                    author,
                    ..Default::default()
                })
                .await;
            let problem = match ended {
                Ok(e) if e.ended > 0 => None,
                Ok(_) => Some("no current fact matched to end".to_string()),
                Err(e) => Some(write_error(e)),
            };
            if let Some(p) = problem {
                out["success"] = json!(false);
                out["error"] = json!(format!(
                    "the fact was added (triple_id above) but ending it at valid_to failed: {p}"
                ));
            }
        }
        Ok(out)
    }

    /// "subject → predicate → object" from values that may be shown back.
    fn fact_text(&self, subject: &str, predicate: &str, object: &str) -> String {
        format!(
            "{} → {} → {}",
            self.redact_echo(subject),
            self.redact_echo(predicate),
            self.redact_echo(object)
        )
    }

    async fn compat_kg_invalidate(
        &self,
        call: &CompatCall,
        args: &Args,
    ) -> std::result::Result<Value, String> {
        let subject = args.required_str("subject")?;
        let predicate = args.required_str("predicate")?;
        let object = args.required_str("object")?;
        let ended = check_time("ended", args.opt_str("ended")?)?;
        let resolved = ended
            .clone()
            .unwrap_or_else(|| chrono::Utc::now().format("%Y-%m-%d").to_string());
        let r = self
            .assert_fact(AssertFact {
                subject: subject.clone(),
                predicate: predicate.clone(),
                object: object.clone(),
                valid_from: ended,
                action: FactAction::End,
                author: self.author(call, None),
                ..Default::default()
            })
            .await
            .map_err(write_error)?;
        let mut out = json!({
            "success": true,
            "fact": self.fact_text(&subject, &r.predicate, &object),
            "ended": resolved,
        });
        if r.ended == 0 {
            out["note"] = json!("no current fact matched, so nothing was ended");
        }
        Ok(out)
    }

    async fn compat_kg_supersede(
        &self,
        call: &CompatCall,
        args: &Args,
    ) -> std::result::Result<Value, String> {
        let subject = args.required_str("subject")?;
        let predicate = args.required_str("predicate")?;
        let old_object = args.required_str("old_object")?;
        let new_object = args.required_str("new_object")?;
        let at = check_time("at", args.opt_str("at")?)?;
        // Supersede closes every current value of the predicate; say so when the caller's idea
        // of the old value was not what was current.
        let current = self
            .facts(FactsRequest {
                entity: subject.clone(),
                predicate: Some(predicate.clone()),
                ..Default::default()
            })
            .await
            .map_err(write_error)?;
        let (me, old) = (kg::alias_key(&subject), kg::alias_key(&old_object));
        let values: Vec<&str> = current
            .facts
            .iter()
            .filter(|f| kg::alias_key(&f.subject) == me)
            .map(|f| f.object.as_str())
            .collect();
        let (shown_subject, shown_old) =
            (self.redact_echo(&subject), self.redact_echo(&old_object));
        let note = if values.iter().any(|v| kg::alias_key(v) == old) {
            None
        } else if values.is_empty() {
            Some(format!(
                "{shown_subject} had no current {predicate}, so nothing was closed (old_object {shown_old} did not match)"
            ))
        } else {
            Some(format!(
                "the current {predicate} of {shown_subject} was {}, not {shown_old}; it was closed anyway",
                values.join(", ")
            ))
        };
        let r = self
            .assert_fact(AssertFact {
                subject: subject.clone(),
                predicate: predicate.clone(),
                object: new_object.clone(),
                valid_from: at,
                action: FactAction::Supersede,
                author: self.author(call, None),
                ..Default::default()
            })
            .await
            .map_err(write_error)?;
        let mut out = json!({
            "success": true,
            "triple_id": r.relation_id,
            "fact": self.fact_text(&subject, &r.predicate, &new_object),
            "superseded": shown_old,
        });
        if let Some(n) = note {
            out["note"] = json!(n);
        }
        Ok(out)
    }

    async fn compat_kg_timeline(&self, args: &Args) -> std::result::Result<Value, String> {
        let entity = args.required_str("entity").map_err(|_| {
            "missing required argument: entity (Alice lists the timeline of one entity)".to_string()
        })?;
        let limit = args
            .opt_u64("limit")?
            .unwrap_or(TIMELINE_DEFAULT_LIMIT)
            .clamp(1, TIMELINE_MAX_LIMIT) as usize;
        let offset = args.opt_u64("offset")?.unwrap_or(0) as usize;
        let r = self
            .facts(FactsRequest {
                entity: entity.clone(),
                timeline: true,
                ..Default::default()
            })
            .await
            .map_err(write_error)?;
        let total = r.facts.len();
        let timeline: Vec<Value> = r
            .facts
            .iter()
            .skip(offset)
            .take(limit)
            .map(|f| fact_json(f, None))
            .collect();
        Ok(json!({
            "entity": entity,
            "timeline": timeline,
            "count": timeline.len(),
            "total": total,
            "offset": offset,
            "limit": limit,
        }))
    }
}

fn fact_json(f: &kg::Fact, direction: Option<&str>) -> Value {
    let mut v = json!({
        "subject": f.subject,
        "predicate": f.predicate,
        "object": f.object,
        "valid_from": f.valid_from,
        "valid_to": f.valid_to,
        "current": f.valid_to.is_none(),
    });
    if let Some(d) = direction {
        v["direction"] = json!(d);
        v["confidence"] = json!(1.0);
        v["source_closet"] = json!(f.source_memory_id);
    }
    v
}

/// What Alice does instead of a MemPalace tool it does not have. A normal result, not an error.
fn unsupported(name: &str) -> Value {
    let message: String = match name {
        "mempalace_mine"
        | "mempalace_sync"
        | "mempalace_delete_by_source"
        | "mempalace_checkpoint" => "Alice captures sessions through its agent hooks and imports old palaces with `alice import mempalace`; save a memory with `remember`.".into(),
        "mempalace_check_duplicate" => "`remember` detects exact duplicates itself (status duplicate); use `recall` to look for similar memories.".into(),
        "mempalace_list_drawers" => "use `recall` with `topics`, `since` and `before`.".into(),
        "mempalace_get_taxonomy" => "use `mempalace_list_wings` and `mempalace_list_rooms`, or the `context` tool.".into(),
        "mempalace_kg_stats" | "mempalace_graph_stats" | "mempalace_memories_filed_away" => {
            "the admin `stats` tool (mcp.admin_tools) and `alice status` show counts.".into()
        }
        "mempalace_traverse"
        | "mempalace_find_tunnels"
        | "mempalace_create_tunnel"
        | "mempalace_list_tunnels"
        | "mempalace_delete_tunnel"
        | "mempalace_follow_tunnels"
        | "mempalace_list_hallways"
        | "mempalace_delete_hallway" => "Alice links memories (`remember` with `supersedes`, `update_memory` supersede) and the graph view in the app shows connections; there are no tunnels.".into(),
        "mempalace_hook_settings" => "use `alice config`.".into(),
        "mempalace_get_aaak_spec" => "Alice does not use AAAK; memories are plain text.".into(),
        "mempalace_mesh_peers"
        | "mempalace_reconnect"
        | "mempalace_task_create"
        | "mempalace_patch_submit" => COORDINATION.into(),
        n if n.starts_with("mempalace_event_") || n.starts_with("mempalace_artifact_") => {
            COORDINATION.into()
        }
        n => format!(
            "Alice does not provide {n}. Its tools are recall, remember, context, get_memory, update_memory, query_facts and assert_fact."
        ),
    };
    json!({ "supported": false, "tool": name, "message": message })
}

const COORDINATION: &str = "coordination features are not part of Alice; agents share memory through `remember` and `recall`.";
