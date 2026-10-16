//! The synthetic MemPalace palace the import and compatibility tests share (included with
//! `#[path]` by `import_flow.rs` and `compat_flow.rs`).
#![allow(dead_code)]

use alice_core::import::fixture::DrawerSpec;
use alice_core::import::palace::DrawerMeta;

pub fn s(v: &str) -> Option<String> {
    Some(v.to_string())
}

pub fn knowledge(id: &str, text: &str, wing: &str, room: &str) -> DrawerSpec {
    DrawerSpec {
        id: id.into(),
        text: text.into(),
        meta: DrawerMeta {
            wing: wing.into(),
            room: room.into(),
            added_by: s("claude-code"),
            filed_at: s("2026-04-10T10:00:00"),
            ..Default::default()
        },
    }
}

pub fn chunk(id: &str, text: &str, wing: &str, source: &str, index: i64) -> DrawerSpec {
    DrawerSpec {
        id: id.into(),
        text: text.into(),
        meta: DrawerMeta {
            wing: wing.into(),
            room: "technical".into(),
            source_file: s(source),
            ingest_mode: s("convos"),
            chunk_index: Some(index),
            authored_at: s(&format!("2026-04-16T11:{:02}:00", index % 60)),
            filed_at: s("2026-04-16T12:00:00"),
            ..Default::default()
        },
    }
}

pub const S1: &str = "/h/.claude/projects/-Users-me-Code-pg-ai/s1.jsonl";
pub const S2: &str = "/h/.claude/projects/-Users-me-Code-devspace/s2.jsonl";

/// The palace Tasks 9-11 share: 117 drawers (5 knowledge, 80 session chunks with one repeat,
/// 30 file chunks, a registry entry and a drawer with unreadable metadata), a closet, a
/// knowledge graph and a tunnels file.
pub fn fixture_specs() -> Vec<DrawerSpec> {
    let mut v = vec![
        knowledge("k_dec", "decision about the v4 frame", "pg-ai", "decisions"),
        knowledge(
            "k_dup_a",
            "Frames are framed protobuf",
            "pg_ai",
            "knowledge",
        ),
        knowledge(
            "k_dup_b",
            "Frames are framed protobuf",
            "wing_pg_ai",
            "knowledge",
        ),
        DrawerSpec {
            id: "k_diary".into(),
            text: "Set up the import today with alice".into(),
            meta: DrawerMeta {
                wing: "wing_claude_code".into(),
                room: "diary".into(),
                agent: s("claude-code"),
                topic: s("setup"),
                date: s("2026-04-16"),
                filed_at: s("2026-04-16T20:00:00"),
                ..Default::default()
            },
        },
        knowledge(
            "k_agent",
            "Notes from the agent: pg-ai needs the frame, and pg-ai tests pass",
            "wing_claude_code",
            "knowledge",
        ),
    ];
    for i in 0..40 {
        // Chunk 7 of s1 is repeated once, as its last chunk.
        let text = if i == 39 {
            "s1 chunk 7 text".to_string()
        } else {
            format!("s1 chunk {i} text")
        };
        v.push(chunk(&format!("s1_{i:03}"), &text, "sessions", S1, i));
        let text2 = if i == 3 {
            "s2 chunk 3 with the quickstart transcript phrase".to_string()
        } else {
            format!("s2 chunk {i} text")
        };
        v.push(chunk(&format!("s2_{i:03}"), &text2, "sessions", S2, i));
    }
    for i in 0..30 {
        v.push(DrawerSpec {
            id: format!("f_{i:03}"),
            text: format!("func f{i}() {{}}"),
            meta: DrawerMeta {
                wing: "devspace".into(),
                room: "src".into(),
                source_file: s(&format!("/h/Code/devspace/main_{}.go", i % 3)),
                added_by: s("claude"),
                ingest_mode: s("projects"),
                chunk_index: Some(i / 3),
                filed_at: s("2026-04-12T09:00:00"),
                ..Default::default()
            },
        });
    }
    v.push(DrawerSpec {
        id: "_reg_1".into(),
        text: "registry".into(),
        meta: DrawerMeta {
            wing: "devspace".into(),
            room: "_registry".into(),
            ingest_mode: s("registry"),
            ..Default::default()
        },
    });
    v
}
