//! The MemPalace compatibility layer: `mempalace_*` tool calls answered from Alice's own data, in
//! the shapes MemPalace's tools return, so prompts and hooks written for it keep working.
//!
//! This is a plain function of the [`Service`] (`compat_call`): the HTTP route, and later the
//! MCP adapter, only carry a call to it. A call never fails at the protocol level; every
//! failure is a normal result `{"error": "..."}`, like MemPalace's own. Every text returned was
//! redacted when it was stored, and a source is named by its file name, not its path.
//!
//! This file holds the read tools. The write tools (and the knowledge-graph and diary ones)
//! answer "not available yet" through one `match` arm.

use crate::service::Service;
use alice_core::api::{ContextRequest, GetRequest, OutputFormat, RecallRequest, Tier};
use alice_core::error::Result;
use alice_core::store::compat::{self as store, CompatDrawer, Overview};
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

    #[allow(dead_code)] // the write tools (Task 14) take booleans
    fn opt_bool(&self, name: &str) -> std::result::Result<Option<bool>, String> {
        let bad = || format!("argument {name} must be a boolean");
        match self.get(name) {
            None => Ok(None),
            Some(Value::Bool(b)) => Ok(Some(*b)),
            Some(Value::String(s)) => match s.trim().to_ascii_lowercase().as_str() {
                "true" | "1" | "yes" => Ok(Some(true)),
                "false" | "0" | "no" => Ok(Some(false)),
                "" => Ok(None),
                _ => Err(bad()),
            },
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

fn no_such_wing(wing: &str) -> String {
    format!("no project or alias named {wing}")
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
                Err(msg) => error(msg),
            },
            Err(msg) => error(msg),
        };
        Ok(CompatResult { value })
    }

    async fn compat_dispatch(
        &self,
        call: &CompatCall,
        args: &Args,
    ) -> std::result::Result<Value, String> {
        let internal = |e: alice_core::error::AliceError| e.to_string();
        match call.tool.as_str() {
            "mempalace_status" => self.compat_status(call).await.map_err(internal),
            "mempalace_list_wings" => self.compat_list_wings().await.map_err(internal),
            "mempalace_list_rooms" => self.compat_list_rooms(args).await,
            "mempalace_search" => self.compat_search(call, args).await,
            "mempalace_get_drawer" => self.compat_get_drawer(args).await,
            // Task 14 replaces this arm with the write, graph and diary tools.
            name if SUPPORTED.contains(&name) => Ok(error("not available yet")),
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
            let snap = self.snapshot().await.map_err(|e| e.to_string())?;
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
            .map_err(|e| e.to_string())?;
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
            Some(w) => match self.compat_wing(w).await.map_err(|e| e.to_string())? {
                Some(slug) => Some(slug),
                None => {
                    notes.push(no_such_wing(w));
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
                topics: room.iter().cloned().collect(),
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
            .map_err(|e| e.to_string())?;
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
            .map_err(|e| e.to_string())?;
        let best = response
            .results
            .iter()
            .map(|h| h.score)
            .fold(0.0_f64, f64::max);
        let results = response
            .results
            .iter()
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
                    "room": d.topics.first().cloned().unwrap_or_default(),
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
            .map_err(|e| e.to_string())?;
        match found {
            None => Ok(error(format!("Drawer not found: {id}"))),
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
                    .map_err(|e| e.to_string())?;
                let Some(full) = got.memories.into_iter().next() else {
                    return Ok(error(format!("Drawer not found: {id}")));
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
                if !got.redirects.is_empty() {
                    out["note"] = json!(format!(
                        "{id} is a duplicate that was merged into {}",
                        out["drawer_id"].as_str().unwrap_or_default()
                    ));
                }
                Ok(out)
            }
        }
    }
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
