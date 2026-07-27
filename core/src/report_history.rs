//! report_history.rs — time-series summary tables inside the per-target
//! `report.db` (merged single-DB-per-target design).
//!
//! The history tables (`hist_snapshots`, `hist_team_usage`, `hist_user_usage`)
//! live in the SAME file as the detail/treemap/permission tables, but are managed
//! independently: they MUST survive a rebuild of the detail/treemap tables and
//! accumulate one snapshot PER DAY (re-scanning the same day overrides it).
//!
//! `scan_date` is an integer `yyyymmdd` (the dedup key). `scanned_at` stores the
//! full epoch of the latest scan in that day so a dashboard can show HH:MM:SS.

use rusqlite::{params, Connection};

pub const HISTORY_DDL: &str = "
CREATE TABLE IF NOT EXISTS hist_snapshots (
  id         INTEGER PRIMARY KEY,
  scan_date  INTEGER NOT NULL UNIQUE,   -- yyyymmdd
  scanned_at INTEGER,                   -- full epoch of latest scan that day
  path       TEXT,
  total      INTEGER,
  used       INTEGER,
  available  INTEGER,
  -- Inode capacity, the count-side twin of total/used/available. The first
  -- three come from the same statvfs() call as the byte figures; the fourth is
  -- what the walk actually visited, so used-minus-scanned is the part of the
  -- filesystem outside the scan root (or unreadable).
  inodes_total   INTEGER,
  inodes_used    INTEGER,
  inodes_free    INTEGER,
  inodes_scanned INTEGER
);
CREATE TABLE IF NOT EXISTS hist_team_usage (
  snapshot_id INTEGER NOT NULL,
  name        TEXT,
  team_id     INTEGER,
  size        INTEGER,
  FOREIGN KEY(snapshot_id) REFERENCES hist_snapshots(id) ON DELETE CASCADE
);
CREATE TABLE IF NOT EXISTS hist_user_usage (
  snapshot_id INTEGER NOT NULL,
  name        TEXT,
  team_id     INTEGER,
  size        INTEGER,
  kind        TEXT,                     -- 'user' | 'other'
  FOREIGN KEY(snapshot_id) REFERENCES hist_snapshots(id) ON DELETE CASCADE
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
    /// Inode capacity from statvfs: `f_files`, `f_files - f_ffree`, `f_ffree`.
    pub inodes_total: i64,
    pub inodes_used: i64,
    pub inodes_free: i64,
    /// Inodes the scan itself visited (files + dirs + symlinks, hardlinks
    /// counted once). Bounded by the scan root, so it is normally well below
    /// `inodes_used`.
    pub inodes_scanned: i64,
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

/// Columns added to `hist_snapshots` after the table shipped. `CREATE TABLE IF
/// NOT EXISTS` is a no-op on an existing table, so a report.db written by an
/// older duscan keeps the old column set until it is widened here.
const HIST_SNAPSHOT_ADDED_COLUMNS: [&str; 4] =
    ["inodes_total", "inodes_used", "inodes_free", "inodes_scanned"];

/// Add any missing `hist_snapshots` columns to a database written by an older
/// duscan. New columns are nullable with no default, so existing rows read back
/// as NULL — which is the truth: that scan did not record inode figures.
fn migrate_hist_snapshots(conn: &Connection) -> rusqlite::Result<()> {
    let mut present = std::collections::HashSet::new();
    {
        let mut stmt = conn.prepare("PRAGMA table_info(hist_snapshots)")?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            present.insert(row.get::<_, String>(1)?);
        }
    }
    for col in HIST_SNAPSHOT_ADDED_COLUMNS {
        if !present.contains(col) {
            conn.execute_batch(&format!(
                "ALTER TABLE hist_snapshots ADD COLUMN {} INTEGER",
                col
            ))?;
        }
    }
    Ok(())
}

/// Ensure the history tables exist on this connection, and that
/// `hist_snapshots` carries every column this version writes.
pub fn ensure_schema(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(HISTORY_DDL)?;
    migrate_hist_snapshots(conn)
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
    conn.execute("DELETE FROM hist_snapshots WHERE scan_date = ?1", params![scan_date])?;

    conn.execute(
        "INSERT INTO hist_snapshots (scan_date, scanned_at, path, total, used, available,
                                     inodes_total, inodes_used, inodes_free, inodes_scanned)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        params![
            scan_date,
            timestamp,
            meta.path,
            meta.total,
            meta.used,
            meta.available,
            meta.inodes_total,
            meta.inodes_used,
            meta.inodes_free,
            meta.inodes_scanned
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
        "DELETE FROM hist_snapshots WHERE scan_date < ?1",
        params![cutoff_yyyymmdd],
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The schema as it shipped before the inode columns. Every report.db already
    /// on disk looks like this, so the migration path is the one that matters.
    const OLD_DDL: &str = "
CREATE TABLE hist_snapshots (
  id         INTEGER PRIMARY KEY,
  scan_date  INTEGER NOT NULL UNIQUE,
  scanned_at INTEGER,
  path       TEXT,
  total      INTEGER,
  used       INTEGER,
  available  INTEGER
);";

    fn columns(conn: &Connection) -> Vec<String> {
        let mut stmt = conn.prepare("PRAGMA table_info(hist_snapshots)").unwrap();
        let rows = stmt.query_map([], |r| r.get::<_, String>(1)).unwrap();
        rows.map(|r| r.unwrap()).collect()
    }

    fn meta() -> SnapshotMeta {
        SnapshotMeta {
            path: "/data".to_string(),
            total: 1000,
            used: 400,
            available: 600,
            inodes_total: 500,
            inodes_used: 120,
            inodes_free: 380,
            inodes_scanned: 90,
        }
    }

    #[test]
    fn widens_a_pre_inode_database_and_keeps_its_rows() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(OLD_DDL).unwrap();
        conn.execute(
            "INSERT INTO hist_snapshots (scan_date, scanned_at, path, total, used, available)
             VALUES (20260101, 100, '/data', 1000, 400, 600)",
            [],
        )
        .unwrap();

        ensure_schema(&conn).unwrap();

        let cols = columns(&conn);
        for want in HIST_SNAPSHOT_ADDED_COLUMNS {
            assert!(cols.contains(&want.to_string()), "missing column {}", want);
        }
        // The old row survives, with NULL inodes — which is the truth: that scan
        // recorded none.
        let (used, inodes): (i64, Option<i64>) = conn
            .query_row(
                "SELECT used, inodes_used FROM hist_snapshots WHERE scan_date = 20260101",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(used, 400);
        assert_eq!(inodes, None);
    }

    #[test]
    fn migration_is_idempotent() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(OLD_DDL).unwrap();
        ensure_schema(&conn).unwrap();
        let first = columns(&conn);
        // A second ALTER TABLE for the same column is an error, so re-running
        // ensure_schema must notice the column is already there.
        ensure_schema(&conn).unwrap();
        assert_eq!(first, columns(&conn));
    }

    #[test]
    fn upsert_stores_inode_figures() {
        let conn = Connection::open_in_memory().unwrap();
        upsert_snapshot(&conn, 1767225600, &meta(), &[], &[]).unwrap();

        let (total, used, free, scanned): (i64, i64, i64, i64) = conn
            .query_row(
                "SELECT inodes_total, inodes_used, inodes_free, inodes_scanned
                   FROM hist_snapshots",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap();
        assert_eq!((total, used, free, scanned), (500, 120, 380, 90));
    }

    #[test]
    fn rescanning_the_same_day_replaces_the_inode_figures() {
        let conn = Connection::open_in_memory().unwrap();
        let ts = 1767225600;
        upsert_snapshot(&conn, ts, &meta(), &[], &[]).unwrap();

        let mut later = meta();
        later.inodes_scanned = 91_000;
        // Same day, one hour on.
        upsert_snapshot(&conn, ts + 3600, &later, &[], &[]).unwrap();

        let rows: i64 = conn
            .query_row("SELECT COUNT(*) FROM hist_snapshots", [], |r| r.get(0))
            .unwrap();
        let scanned: i64 = conn
            .query_row("SELECT inodes_scanned FROM hist_snapshots", [], |r| r.get(0))
            .unwrap();
        assert_eq!(rows, 1);
        assert_eq!(scanned, 91_000);
    }
}
