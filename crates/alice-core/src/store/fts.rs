//! Maintenance of the contentless FTS5 tables (rowid = memories.seq / chunks.seq).

use crate::codetokens::expand_code_tokens;
use crate::error::Result;
use rusqlite::{Connection, params};

pub fn index_memory(
    conn: &Connection,
    seq: i64,
    title: &str,
    body: &str,
    topics: &[String],
) -> Result<()> {
    let code = expand_code_tokens(&format!("{title}\n{body}"));
    conn.execute(
        "INSERT INTO memories_fts (rowid, title, body, topics, code_tokens) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![seq, title, body, topics.join(" "), code],
    )?;
    conn.execute(
        "INSERT INTO memories_tri (rowid, title, body) VALUES (?1, ?2, ?3)",
        params![seq, title, body],
    )?;
    Ok(())
}

pub fn unindex_memory(conn: &Connection, seq: i64) -> Result<()> {
    conn.execute("DELETE FROM memories_fts WHERE rowid = ?1", [seq])?;
    conn.execute("DELETE FROM memories_tri WHERE rowid = ?1", [seq])?;
    Ok(())
}

pub fn index_chunk(conn: &Connection, seq: i64, text: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO chunks_fts (rowid, text, code_tokens) VALUES (?1, ?2, ?3)",
        params![seq, text, expand_code_tokens(text)],
    )?;
    conn.execute(
        "INSERT INTO chunks_tri (rowid, text) VALUES (?1, ?2)",
        params![seq, text],
    )?;
    Ok(())
}

pub fn unindex_chunk(conn: &Connection, seq: i64) -> Result<()> {
    conn.execute("DELETE FROM chunks_fts WHERE rowid = ?1", [seq])?;
    conn.execute("DELETE FROM chunks_tri WHERE rowid = ?1", [seq])?;
    Ok(())
}
