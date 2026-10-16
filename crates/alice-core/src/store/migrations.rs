//! Forward-only, numbered migrations. A backup is taken before migrating an existing database.

use crate::error::{AliceError, Result};
use crate::ids::now_rfc3339;
use rusqlite::{Connection, params};
use std::collections::HashSet;
use std::path::Path;

const MIGRATIONS: &[(i64, &str)] = &[
    (1, include_str!("sql/0001_init.sql")),
    (2, include_str!("sql/0002_events_reviews.sql")),
    (3, include_str!("sql/0003_judge.sql")),
];

pub const CURRENT_VERSION: i64 = 3;

// CURRENT_VERSION must name the newest migration.
const _: () = assert!(MIGRATIONS[MIGRATIONS.len() - 1].0 == CURRENT_VERSION);

pub fn migrate(conn: &mut Connection, db_path: &Path) -> Result<usize> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS schema_migrations (version INTEGER PRIMARY KEY, applied_at TEXT NOT NULL)",
    )?;
    let applied: HashSet<i64> = {
        let mut stmt = conn.prepare("SELECT version FROM schema_migrations")?;
        stmt.query_map([], |r| r.get(0))?
            .collect::<std::result::Result<_, _>>()?
    };
    if let Some(&newest) = applied.iter().filter(|&&v| v > CURRENT_VERSION).max() {
        return Err(AliceError::ReadOnly(format!(
            "database schema v{newest} is newer than this build (v{CURRENT_VERSION}); upgrade Alice"
        )));
    }
    let todo: Vec<&(i64, &str)> = MIGRATIONS
        .iter()
        .filter(|(v, _)| !applied.contains(v))
        .collect();
    if todo.is_empty() {
        return Ok(0);
    }
    if !applied.is_empty() {
        // Next to the other backups (`<home>/backups`), where rotation finds them.
        let from = applied.iter().max().copied().unwrap_or(0);
        let stamp = chrono::Utc::now().format("%Y%m%dT%H%M%SZ");
        let dir = db_path.parent().unwrap_or(Path::new(".")).join("backups");
        super::create_private_dir(&dir)?;
        let dest = dir.join(format!("alice-pre-migration-v{from}-{stamp}.db"));
        super::backup_into(conn, &dest)?;
    }
    for (version, sql) in &todo {
        let tx = conn.transaction()?;
        tx.execute_batch(sql)?;
        tx.execute(
            "INSERT INTO schema_migrations (version, applied_at) VALUES (?1, ?2)",
            params![version, now_rfc3339()],
        )?;
        tx.commit()?;
    }
    Ok(todo.len())
}
