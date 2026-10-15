//! The input schemas of the MemPalace tools Alice answers over MCP: one tool per
//! [`SUPPORTED`](crate::compat::SUPPORTED) name, with MemPalace's own argument names, types and
//! required arguments (copied by hand from its `schemas.py`; the descriptions are Alice's).

use rmcp::model::Tool;
use serde_json::{Map, Value, json};
use std::sync::Arc;

/// One argument: name, JSON type, required.
type Arg = (&'static str, &'static str, bool);

/// Name, one-sentence description, arguments.
const TABLE: [(&str, &str, &[Arg]); 15] = [
    (
        "mempalace_status",
        "Overview of the memory: counts, the wing map and a short protocol note.",
        &[],
    ),
    (
        "mempalace_search",
        "Semantic search over memories; returns verbatim content with similarity scores.",
        &[
            ("query", "string", true),
            ("limit", "integer", false),
            ("wing", "string", false),
            ("room", "string", false),
            ("source_file", "string", false),
            ("since", "string", false),
            ("before", "string", false),
            ("max_distance", "number", false),
            ("candidate_strategy", "string", false),
            ("cli_compatible", "boolean", false),
            ("context", "string", false),
        ],
    ),
    (
        "mempalace_add_drawer",
        "Save one memory under a wing and a room.",
        &[
            ("wing", "string", true),
            ("room", "string", true),
            ("content", "string", true),
            ("source_file", "string", false),
            ("added_by", "string", false),
        ],
    ),
    (
        "mempalace_diary_write",
        "Write a diary entry for an agent.",
        &[
            ("agent_name", "string", true),
            ("entry", "string", false),
            ("topic", "string", false),
            ("wing", "string", false),
            ("content", "string", false),
        ],
    ),
    (
        "mempalace_diary_read",
        "Read an agent's latest diary entries.",
        &[
            ("agent_name", "string", true),
            ("last_n", "integer", false),
            ("wing", "string", false),
        ],
    ),
    (
        "mempalace_get_drawer",
        "Fetch one memory by its drawer id.",
        &[("drawer_id", "string", true)],
    ),
    (
        "mempalace_update_drawer",
        "Change a memory's content, wing or room.",
        &[
            ("drawer_id", "string", true),
            ("content", "string", false),
            ("wing", "string", false),
            ("room", "string", false),
        ],
    ),
    (
        "mempalace_delete_drawer",
        "Forget one memory by its drawer id.",
        &[("drawer_id", "string", true)],
    ),
    (
        "mempalace_kg_query",
        "Facts about an entity from the knowledge graph, optionally as of a date.",
        &[
            ("entity", "string", true),
            ("as_of", "string", false),
            ("direction", "string", false),
        ],
    ),
    (
        "mempalace_kg_add",
        "Record a fact (subject, predicate, object) with optional validity dates.",
        &[
            ("subject", "string", true),
            ("predicate", "string", true),
            ("object", "string", true),
            ("valid_from", "string", false),
            ("valid_to", "string", false),
            ("source_closet", "string", false),
            ("source_file", "string", false),
            ("source_drawer_id", "string", false),
        ],
    ),
    (
        "mempalace_kg_invalidate",
        "Mark a fact as no longer true.",
        &[
            ("subject", "string", true),
            ("predicate", "string", true),
            ("object", "string", true),
            ("ended", "string", false),
        ],
    ),
    (
        "mempalace_kg_supersede",
        "Replace the current value of a fact with a new one.",
        &[
            ("subject", "string", true),
            ("predicate", "string", true),
            ("old_object", "string", true),
            ("new_object", "string", true),
            ("at", "string", false),
        ],
    ),
    (
        "mempalace_kg_timeline",
        "The chronological story of the facts about an entity (or all of them).",
        &[
            ("entity", "string", false),
            ("limit", "integer", false),
            ("offset", "integer", false),
        ],
    ),
    (
        "mempalace_list_wings",
        "List the wings with their memory counts.",
        &[],
    ),
    (
        "mempalace_list_rooms",
        "List the rooms of a wing (or of the current project) with their counts.",
        &[("wing", "string", false)],
    ),
];

fn schema(args: &[Arg]) -> Map<String, Value> {
    let mut props = Map::new();
    let mut required = Vec::new();
    for (name, ty, req) in args {
        let mut p = json!({ "type": ty });
        if *name == "candidate_strategy" {
            p["enum"] = json!(["vector", "union"]);
        }
        props.insert((*name).into(), p);
        if *req {
            required.push(json!(name));
        }
    }
    let mut s = Map::new();
    s.insert("type".into(), json!("object"));
    s.insert("properties".into(), Value::Object(props));
    if !required.is_empty() {
        s.insert("required".into(), Value::Array(required));
    }
    s
}

/// One tool per [`SUPPORTED`](crate::compat::SUPPORTED) name.
pub fn tools() -> Vec<Tool> {
    TABLE
        .iter()
        .map(|(name, desc, args)| Tool::new(*name, *desc, Arc::new(schema(args))))
        .collect()
}

/// One listed tool by name.
pub fn tool(name: &str) -> Option<Tool> {
    TABLE
        .iter()
        .find(|(n, _, _)| *n == name)
        .map(|(n, d, a)| Tool::new(*n, *d, Arc::new(schema(a))))
}
