//! report_history.rs — time-series summary tables inside the per-target
//! `report.db` (merged single-DB-per-target design).
//!
//! The history tables (`snapshots`, `hist_team_usage`, `hist_user_usage`) live
//! in the SAME file as the detail/treemap/permission tables, but are managed
//! independently: they MUST survive a rebuild of the detail/treemap tables and
//! accumulate one snapshot PER DAY (re-scanning the same day overrides it).
//!
//! `scan_date` is an integer `yyyymmdd` (the dedup key). `scanned_at` stores the
//! full epoch of the latest scan in that day so a dashboard can show HH:MM:SS.

use rusqlite::{params, Connection};

pub const HISTORY_DDL: &str = "
CREATE TABLE IF NOT EXISTS snapshots (
  id         INTEGER PRIMARY KEY,
  scan_date  INTEGER NOT NULL UNIQUE,   -- yyyymmdd
  scanned_at INTEGER,                   -- full epoch of latest scan that day
  path       TEXT,
  total      INTEGER,
  used       INTEGER,
  available  INTEGER
);
CREATE TABLE IF NOT EXISTS hist_team_usage (
  snapshot_id INTEGER NOT NULL,
  name        TEXT,
  team_id     INTEGER,
  size        INTEGER,
  FOREIGN KEY(snapshot_id) REFERENCES snapshots(id) ON DELETE CASCADE
);
CREATE TABLE IF NOT EXISTS hist_user_usage (
  snapshot_id INTEGER NOT NULL,
  name        TEXT,
  team_id     INTEGER,
  size        INTEGER,
  kind        TEXT,                     -- 'user' | 'other'
  FOREIGN KEY(snapshot_id) REFERENCES snapshots(id) ON DELETE CASCADE
);
CREATE INDEX IF NOT EXISTS ix_hist_team_snap ON hist_team_usage(snapshot_id);
CREATE INDEX IF NOT EXISTS ix_hist_user_snap ON hist_user_usage(snapshot_id);
";

/// A single team/user usage row to record in the snapshot.
#[derive(Clone, Debug)]
pub struct UsageRow {
    pub name: String,
    pub team_id: Option<i64>,
    pub size: i64,
    pub kind: String, // "team", "user", or "other"
}

/// General system disk figures for the snapshot row.
#[derive(Clone, Debug, Default)]
pub struct SnapshotMeta {
    pub path: String,
    pub total: i64,
    pub used: i64,
    pub available: i64,
}

/// Convert an epoch timestamp (seconds) to an integer `yyyymmdd` in LOCAL time.
pub fn epoch_to_yyyymmdd(epoch: i64) -> i64 {
    // Use libc localtime_r for a dependency-free local-date conversion.
    let t = epoch as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe {
        libc::localtime_r(&t, &mut tm);
    }
    let year = tm.tm_year + 1900;
    let mon = tm.tm_mon + 1;
    let day = tm.tm_mday;
    (year as i64) * 10000 + (mon as i64) * 100 + (day as i64)
}

/// Ensure the history tables exist on this connection.
pub fn ensure_schema(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(HISTORY_DDL)
}

/// Upsert one snapshot for the day derived from `timestamp`.
///
/// If a snapshot for that `yyyymmdd` already exists it is deleted (cascading to
/// the hist_* rows) and re-inserted — i.e. the latest scan of the day wins.
/// `teams` / `users` rows are written into the hist_* tables.
pub fn upsert_snapshot(
    conn: &Connection,
    timestamp: i64,
    meta: &SnapshotMeta,
    teams: &[UsageRow],
    users: &[UsageRow],
) -> rusqlite::Result<()> {
    ensure_schema(conn)?;
    conn.pragma_update(None, "foreign_keys", true)?;

    let scan_date = epoch_to_yyyymmdd(timestamp);

    // Override any existing snapshot for this day.
    conn.execute("DELETE FROM snapshots WHERE scan_date = ?1", params![scan_date])?;

    conn.execute(
        "INSERT INTO snapshots (scan_date, scanned_at, path, total, used, available)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            scan_date,
            timestamp,
            meta.path,
            meta.total,
            meta.used,
            meta.available
        ],
    )?;
    let snapshot_id = conn.last_insert_rowid();

    {
        let mut team_stmt = conn.prepare(
            "INSERT INTO hist_team_usage (snapshot_id, name, team_id, size)
             VALUES (?1, ?2, ?3, ?4)",
        )?;
        for r in teams {
            team_stmt.execute(params![snapshot_id, r.name, r.team_id, r.size])?;
        }
    }
    {
        let mut user_stmt = conn.prepare(
            "INSERT INTO hist_user_usage (snapshot_id, name, team_id, size, kind)
             VALUES (?1, ?2, ?3, ?4, ?5)",
        )?;
        for r in users {
            user_stmt.execute(params![snapshot_id, r.name, r.team_id, r.size, r.kind])?;
        }
    }
    Ok(())
}

/// Delete snapshots strictly older than `cutoff_yyyymmdd` (cascades hist_*).
/// Returns the number of snapshot rows removed.
pub fn purge_older_than(conn: &Connection, cutoff_yyyymmdd: i64) -> rusqlite::Result<usize> {
    ensure_schema(conn)?;
    conn.pragma_update(None, "foreign_keys", true)?;
    conn.execute(
        "DELETE FROM snapshots WHERE scan_date < ?1",
        params![cutoff_yyyymmdd],
    )
}
