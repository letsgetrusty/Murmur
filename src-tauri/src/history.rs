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

/// One day's dictation count, for the Insights activity chart.
#[derive(Serialize)]
pub struct DayCount {
    /// Local date, `YYYY-MM-DD`.
    pub date: String,
    pub count: i64,
}

/// Aggregate stats for the Insights tab. Word counts are approximate
/// (space-delimited) — fine for a personal usage readout.
#[derive(Serialize)]
pub struct Stats {
    pub total: i64,
    pub refined: i64,
    pub words: i64,
    /// Unix seconds of the earliest stored dictation, if any.
    pub first_ts: Option<i64>,
    /// The last `window` days, oldest first, zero-filled.
    pub days: Vec<DayCount>,
}

/// Compute stats over the stored history. `window` = number of recent days for
/// the activity chart. Totals reflect what's currently retained (history can be
/// disabled or pruned), so the caller pairs them with lifetime usage counts.
pub fn stats(conn: &Connection, window: i64) -> Result<Stats> {
    let (total, refined, words, first_ts) = conn.query_row(
        "SELECT
            COUNT(*),
            COALESCE(SUM(CASE WHEN refined IS NOT NULL AND refined <> '' THEN 1 ELSE 0 END), 0),
            COALESCE(SUM(CASE WHEN TRIM(raw) = '' THEN 0
                ELSE LENGTH(TRIM(raw)) - LENGTH(REPLACE(TRIM(raw), ' ', '')) + 1 END), 0),
            MIN(ts)
         FROM entries",
        [],
        |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, Option<i64>>(3)?,
            ))
        },
    )?;

    // Build the day window in SQLite (local time) so we avoid Rust date math and
    // zero-fill days with no dictations.
    let mut stmt = conn.prepare(
        "WITH RECURSIVE d(day, n) AS (
            SELECT date('now', 'localtime'), 0
            UNION ALL
            SELECT date('now', 'localtime', '-' || (n + 1) || ' days'), n + 1
              FROM d WHERE n + 1 < ?1
         )
         SELECT d.day,
                (SELECT COUNT(*) FROM entries e
                  WHERE strftime('%Y-%m-%d', e.ts, 'unixepoch', 'localtime') = d.day)
         FROM d ORDER BY d.day ASC",
    )?;
    let rows = stmt.query_map(params![window], |r| {
        Ok(DayCount {
            date: r.get(0)?,
            count: r.get(1)?,
        })
    })?;
    let mut days = Vec::new();
    for row in rows {
        days.push(row?);
    }

    Ok(Stats {
        total,
        refined,
        words,
        first_ts,
        days,
    })
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
