// Phase B: dictation history in an embedded SQLite database. Each committed
// dictation is stored as a row; the settings-window History tab lists, searches,
// copies, and deletes them. Local-only and single-user, matching the tool's
// scope; gated by `config.history_enabled` and capped by `config.history_limit`.

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use rusqlite::{params, Connection};
use serde::Serialize;

fn db_path() -> Option<PathBuf> {
    let mut p = PathBuf::from(std::env::var_os("HOME")?);
    p.push("Library/Application Support/murmur/history.db");
    Some(p)
}

/// Open (creating if needed) the history database and ensure the schema exists.
/// Falls back to an in-memory database if the on-disk file can't be opened, so
/// history features degrade gracefully instead of crashing setup.
pub fn open() -> Connection {
    let conn = db_path()
        .and_then(|path| {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).ok();
            }
            Connection::open(path).ok()
        })
        .unwrap_or_else(|| {
            log::warn!("history: on-disk db unavailable; using in-memory (not persisted)");
            Connection::open_in_memory().expect("in-memory sqlite")
        });
    if let Err(e) = conn.execute(
        "CREATE TABLE IF NOT EXISTS entries (
            id      INTEGER PRIMARY KEY AUTOINCREMENT,
            ts      INTEGER NOT NULL,
            raw     TEXT NOT NULL,
            refined TEXT
        )",
        [],
    ) {
        log::warn!("history: create table failed: {e}");
    }
    conn
}

#[derive(Serialize)]
pub struct Entry {
    pub id: i64,
    /// Unix seconds.
    pub ts: i64,
    pub raw: String,
    /// Present when the dictation was refined (Fn + modifier).
    pub refined: Option<String>,
}

pub fn insert(conn: &Connection, raw: &str, refined: Option<&str>) -> Result<()> {
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    conn.execute(
        "INSERT INTO entries (ts, raw, refined) VALUES (?1, ?2, ?3)",
        params![ts, raw, refined],
    )
    .context("insert history row")?;
    Ok(())
}

/// Newest-first, optionally filtered by a substring match on either text.
pub fn list(conn: &Connection, query: &str, limit: i64, offset: i64) -> Result<Vec<Entry>> {
    let like = format!("%{query}%");
    let mut stmt = conn.prepare(
        "SELECT id, ts, raw, refined FROM entries
         WHERE ?1 = '' OR raw LIKE ?2 OR refined LIKE ?2
         ORDER BY id DESC LIMIT ?3 OFFSET ?4",
    )?;
    let rows = stmt.query_map(params![query, like, limit, offset], |r| {
        Ok(Entry {
            id: r.get(0)?,
            ts: r.get(1)?,
            raw: r.get(2)?,
            refined: r.get(3)?,
        })
    })?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

pub fn delete(conn: &Connection, id: i64) -> Result<()> {
    conn.execute("DELETE FROM entries WHERE id = ?1", params![id])?;
    Ok(())
}

pub fn clear(conn: &Connection) -> Result<()> {
    conn.execute("DELETE FROM entries", [])?;
    Ok(())
}

/// Keep only the newest `keep` rows.
pub fn prune(conn: &Connection, keep: u32) -> Result<()> {
    conn.execute(
        "DELETE FROM entries WHERE id NOT IN
            (SELECT id FROM entries ORDER BY id DESC LIMIT ?1)",
        params![keep],
    )?;
    Ok(())
}
