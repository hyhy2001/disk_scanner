// SQLite builders for the Phase 2 reports.
//
// Output databases:
//   * `treemap.db`     — directory tree (adjacency list + per-dir aggregates)
//   * `data_detail.db` — per-user file/dir breakdown
//   * `report.db`      — merged single-DB (detail + treemap + perm + history)
//
// Schemas: STRICT tables, INTEGER PRIMARY KEY rowid aliases, lookup tables
// for path segments and extensions, partial / covering indexes.
// See `/root/.claude/plans/immutable-purring-adleman.md` for the full design.
//
// `treemap.db` is built via `build_treemap_db()` from a single in-memory
// `TreemapInput`.
//
// `data_detail.db` is built incrementally:
//   1. `detail_open()`            — DDL only
//   2. `detail_insert_files_chunk()` — repeated while streaming spill files
//   3. `detail_insert_*` helpers  — for dictionaries / aggregates
//   4. `detail_finalize()`         — CREATE INDEX + ANALYZE + VACUUM INTO + atomic rename

use crate::pyo3::exceptions::PyRuntimeError;
use crate::pyo3::prelude::*;
use rusqlite::{params, params_from_iter, Connection, ToSql};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

// ─── Constants / magic ────────────────────────────────────────────────

pub const TREEMAP_APP_ID: i32 = 0xC0DD15C0u32 as i32;
pub const DETAIL_APP_ID: i32 = 0xC0DD15D1u32 as i32;
pub const MERGED_APP_ID: i32 = 0xC0DD15D2u32 as i32;
pub const SCHEMA_VERSION: i32 = 1;
pub const PAGE_SIZE: i32 = 16384;

pub const FILE_INSERT_CHUNK: usize = 200_000;

/// Rows packed into one `INSERT … VALUES (…),(…),…` statement.
/// SQLite default binding cap is 32766; 100 rows × 9 cols = 900 binds is safe.
const PACK_ROWS: usize = 100;

// ─── DDL ──────────────────────────────────────────────────────────────

const TREEMAP_DDL: &str = "
CREATE TABLE meta (
  key   TEXT PRIMARY KEY,
  value TEXT
);

-- DIR segment dictionary. Lives in treemap.db only — referenced by tm.dirs
-- for path reconstruction. File basenames have their own table in detail.db
-- (so consumers wanting paths ATTACH treemap.db; detail-only consumers stay
-- self-contained).
CREATE TABLE names (
  id   INTEGER PRIMARY KEY,
  name TEXT    NOT NULL
);

CREATE TABLE owners (
  uid      INTEGER PRIMARY KEY,
  username TEXT    NOT NULL
);

CREATE TABLE dirs (
  id          INTEGER PRIMARY KEY,
  parent_id   INTEGER,
  name_id     INTEGER NOT NULL,
  total_size  INTEGER NOT NULL,
  file_count  INTEGER NOT NULL,
  dir_count   INTEGER NOT NULL,
  owner_uid   INTEGER NOT NULL,
  has_files   INTEGER NOT NULL
);
";

const TREEMAP_INDEX_DDL: &str = "
-- (parent_id, total_size DESC) covers both `WHERE parent_id=?` (leftmost
-- prefix lookup) and the UI sort. A separate (parent_id) index is redundant.
CREATE INDEX ix_dirs_parent_size ON dirs(parent_id, total_size DESC);
";

const DETAIL_DDL: &str = "
CREATE TABLE meta (
  key   TEXT PRIMARY KEY,
  value TEXT
);

CREATE TABLE users (
  uid               INTEGER PRIMARY KEY,
  username          TEXT    NOT NULL,
  team_id           TEXT,
  total_files       INTEGER NOT NULL,
  total_dirs        INTEGER NOT NULL,
  total_size        INTEGER NOT NULL,
  permission_issues INTEGER NOT NULL DEFAULT 0,
  is_target         INTEGER NOT NULL DEFAULT 0,
  -- Directories where this user is the owner, as opposed to total_dirs, which
  -- counts every directory the user has a byte in. The dashboard lists owned
  -- directories, so counting them at read time meant a range scan over the
  -- user's whole slice on every page load; tallying it here during the merge
  -- turns that into a primary-key lookup. Column order is load-bearing: the
  -- merge copies rows with SELECT *, so this must stay last in both this table
  -- and detail_users below.
  owned_dirs        INTEGER NOT NULL DEFAULT 0
);

-- File basename dictionary (unique basenames across all files).
CREATE TABLE file_names (
  id   INTEGER PRIMARY KEY,
  name TEXT NOT NULL
);

-- Directory table. One row per (dir entity, user) pair.
-- id = dir entity id (same dir shared across users).
-- uid = file owner who has files inside this dir.
-- path = pre-computed full path e.g. '/var/log/apache2'.
-- owner_uid = real owner of the directory inode itself, captured in Phase 1
--             (dirowner_t*.bin) and filled in by report_pipeline.rs; 0 means
--             unknown (the dir was never stat'd successfully). Do NOT treat
--             this as dead weight: the dashboard filters on it
--             (server/src/db/detail.ts, `WHERE uid = ? AND owner_uid = ?`) to
--             show only the dirs a user owns, so dropping it empties every
--             user's directory list and CSV export. treemap.db's
--             dirs.owner_uid is the same value but falls back to a known uid
--             instead of 0.
-- size = total size of uid's files in this dir.
-- files = count of uid's files in this dir.
CREATE TABLE dirs (
  id        INTEGER NOT NULL,
  uid       INTEGER NOT NULL,
  parent_id INTEGER,
  path      TEXT    NOT NULL,
  owner_uid INTEGER NOT NULL,
  size      INTEGER NOT NULL,
  files     INTEGER NOT NULL,
  PRIMARY KEY (id, uid)
);

-- File rows. No surrogate id (nothing references files.id after top_files removed).
-- ext stored inline (avoids JOIN with exts dictionary).
CREATE TABLE files (
  dir_id  INTEGER NOT NULL,
  name_id INTEGER NOT NULL,
  ext     TEXT    NOT NULL,
  uid     INTEGER NOT NULL,
  size    INTEGER NOT NULL
);
";

const DETAIL_INDEX_DDL: &str = "
-- Files: cover keyset pagination (uid, size DESC, dir_id ASC, name_id ASC).
-- This is the only index needed on `files`: every reader queries it as
-- WHERE uid = ?1 ORDER BY size DESC (detail views, --json, export). The former
-- ix_files_uid_ext_size_dir_name and ix_files_dir_uid_ext_size_name indexed
-- `ext`/`dir_id`, which no query in cli/ or core/ ever filters or orders by, so
-- they only added CREATE INDEX time and file size on the 1.5M-row table.
CREATE INDEX ix_files_uid_size_dir_name      ON files(uid, size DESC, dir_id ASC, name_id ASC);
-- Dirs: cover keyset pagination (uid, size DESC, id ASC).
CREATE INDEX ix_dirs_uid_size_dir            ON dirs(uid, size DESC, id ASC);
-- file_names: cover LIKE substring search.
CREATE INDEX ix_file_names_name              ON file_names(name);
";

// NOTE: application_id is NOT set here. `PRAGMA application_id` takes a signed
// 32-bit value; the magic tag 0xC0DD15D2 exceeds i32::MAX as a positive decimal
// literal, and SQLite silently clamps an out-of-range literal to 0. It is
// stamped correctly (as the i32-reinterpreted MERGED_APP_ID) via stamp_db()
// right after this DDL runs.
const MERGED_DDL: &str = "

CREATE TABLE IF NOT EXISTS meta (
  key   TEXT PRIMARY KEY,
  value TEXT
);

-- Detail tables (prefixed detail_)
CREATE TABLE IF NOT EXISTS detail_users (
  uid               INTEGER PRIMARY KEY,
  username          TEXT    NOT NULL,
  team_id           TEXT,
  total_files       INTEGER NOT NULL,
  total_dirs        INTEGER NOT NULL,
  total_size        INTEGER NOT NULL,
  permission_issues INTEGER NOT NULL DEFAULT 0,
  is_target         INTEGER NOT NULL DEFAULT 0,
  -- Must match the column order of `users` above: the merge is
  -- `INSERT OR IGNORE INTO detail_users SELECT * FROM srcdetail.users`.
  owned_dirs        INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE IF NOT EXISTS detail_file_names (
  id   INTEGER PRIMARY KEY,
  name TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS detail_dirs (
  id        INTEGER NOT NULL,
  uid       INTEGER NOT NULL,
  parent_id INTEGER,
  path      TEXT    NOT NULL,
  owner_uid INTEGER NOT NULL,
  size      INTEGER NOT NULL,
  files     INTEGER NOT NULL,
  PRIMARY KEY (id, uid)
);
CREATE TABLE IF NOT EXISTS detail_files (
  dir_id  INTEGER NOT NULL,
  name_id INTEGER NOT NULL,
  ext     TEXT    NOT NULL,
  uid     INTEGER NOT NULL,
  size    INTEGER NOT NULL
);

-- Treemap tables (prefixed treemap_)
CREATE TABLE IF NOT EXISTS treemap_names (
  id   INTEGER PRIMARY KEY,
  name TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS treemap_owners (
  uid      INTEGER PRIMARY KEY,
  username TEXT    NOT NULL
);
CREATE TABLE IF NOT EXISTS treemap_dirs (
  id          INTEGER PRIMARY KEY,
  parent_id   INTEGER,
  name_id     INTEGER NOT NULL,
  total_size  INTEGER NOT NULL,
  file_count  INTEGER NOT NULL,
  dir_count   INTEGER NOT NULL,
  owner_uid   INTEGER NOT NULL,
  has_files   INTEGER NOT NULL
);

-- Permission tables (prefixed perm_)
CREATE TABLE IF NOT EXISTS perm_issues (
  id          INTEGER PRIMARY KEY,
  user        TEXT NOT NULL,
  item_type   TEXT NOT NULL,
  error       TEXT NOT NULL,
  path        TEXT NOT NULL
);

-- The hist_* tables are NOT declared here. They are created from
-- report_history::HISTORY_DDL, which owns them, because this file used to carry
-- a second copy of the same three CREATE TABLE statements and the two drifted:
-- the column order of hist_snapshots disagreed, which broke the positional
-- carry-over below.
";

const MERGED_INDEX_DDL: &str = "
-- ix_detail_files_uid_size_dir_name covers the per-user export query
-- (WHERE uid = ?1 ORDER BY size DESC) as a covering index.
--
-- ix_detail_files_uid_ext_size_dir_name was dropped then re-added: the
-- dashboard's detail tab filters by extension (ext IN (?)), which needs an
-- index leading with (uid, ext) for a fast range seek instead of a full user
-- scan. It costs ~3.8s of CREATE INDEX time and ~60MB per report.
--
-- ix_detail_files_name_id / ix_treemap_dirs_name_id serve the name search's
-- join from an FTS hit back to the file/dir rows: without them the planner
-- falls back to scanning every row, which at 30M files is seconds. Leading with
-- name_id turns that into a per-hit lookup.
CREATE INDEX IF NOT EXISTS ix_detail_files_uid_size_dir_name
    ON detail_files(uid, size DESC, dir_id ASC, name_id ASC);
CREATE INDEX IF NOT EXISTS ix_detail_files_uid_ext_size_dir_name
    ON detail_files(uid, ext, size DESC, dir_id ASC, name_id ASC);
CREATE INDEX IF NOT EXISTS ix_detail_dirs_uid_size_dir
    ON detail_dirs(uid, size DESC, id ASC);
CREATE INDEX IF NOT EXISTS ix_detail_files_name_id
    ON detail_files(name_id, size DESC, dir_id ASC);
CREATE INDEX IF NOT EXISTS ix_treemap_dirs_name_id
    ON treemap_dirs(name_id, total_size DESC);
CREATE INDEX IF NOT EXISTS ix_detail_file_names_name
    ON detail_file_names(name);
CREATE INDEX IF NOT EXISTS ix_treemap_dirs_parent_size
    ON treemap_dirs(parent_id, total_size DESC);
CREATE INDEX IF NOT EXISTS ix_detail_users_files
    ON detail_users(total_files DESC);
CREATE INDEX IF NOT EXISTS ix_perm_user
    ON perm_issues(user);
CREATE INDEX IF NOT EXISTS ix_perm_user_type
    ON perm_issues(user, item_type);
CREATE INDEX IF NOT EXISTS ix_perm_path
    ON perm_issues(path COLLATE NOCASE);
CREATE INDEX IF NOT EXISTS ix_hist_team_snap
    ON hist_team_usage(snapshot_id);
CREATE INDEX IF NOT EXISTS ix_hist_user_snap
    ON hist_user_usage(snapshot_id);
";

/// FTS5 trigram search indexes for the dashboard's name search.
///
/// `content=` makes these external-content tables: they store only the trigram
/// index, and the `name` values are read from the base tables on demand, so a
/// report gains ~the size of the index (a few tens of MB) rather than a second
/// copy of every name. Trigram (SQLite >= 3.34) is what makes infix `LIKE` a
/// lookup: plain FTS5 only indexes whole tokens, and a B-tree index cannot serve
/// a `%…%` pattern.
///
/// The rebuild directives populate the index from the content tables. This is
/// deliberately separate from MERGED_INDEX_DDL so it can run once, after the
/// rows are copied and the regular indexes are up; FTS shadow tables need the
/// content rows to already exist.
const MERGED_FTS_DDL: &str = "
CREATE VIRTUAL TABLE IF NOT EXISTS fts_file_names USING fts5(
  name,
  content='detail_file_names',
  content_rowid='id',
  tokenize='trigram'
);
CREATE VIRTUAL TABLE IF NOT EXISTS fts_dir_names USING fts5(
  name,
  content='treemap_names',
  content_rowid='id',
  tokenize='trigram'
);
INSERT INTO fts_file_names(fts_file_names) VALUES('rebuild');
INSERT INTO fts_dir_names(fts_dir_names) VALUES('rebuild');
";

/// Create or open a merged report.db, initialising schema if needed.
/// Returns the connection with WAL mode, cache, and mmap configured.
///
/// WAL is set for the write side, but note who reads this file afterwards: the
/// dashboard opens report.db readonly with `journal_mode=OFF`
/// (`server/src/db/open.ts`). That is safe only because the merge writes to a
/// `.tmp` file and `rename()`s it into place, so a reader always has a complete,
/// quiescent database and never has to replay a `-wal` sidecar. If this ever
/// starts writing report.db in place, the dashboard's pragma has to change with
/// it — a readonly `journal_mode=OFF` connection cannot see committed data that
/// still lives in the WAL.
pub fn open_merged_db(merged_db_path: &Path) -> rusqlite::Result<Connection> {
    let exists = merged_db_path.exists();
    let conn = Connection::open(merged_db_path)?;
    conn.execute_batch(
        "PRAGMA journal_mode=WAL;
         PRAGMA synchronous=NORMAL;
         PRAGMA temp_store=MEMORY;
         PRAGMA cache_size=-65536;
         PRAGMA mmap_size=268435456;
         PRAGMA foreign_keys=ON;",
    )?;
    if !exists {
        conn.execute_batch(MERGED_DDL)?;
        // Stamp the format magic correctly (the DDL can't, see MERGED_DDL note).
        // pragma_update takes the value as i32, so the 0xC0DD15D2 bit pattern
        // round-trips exactly instead of being clamped to 0.
        conn.pragma_update(None, "application_id", MERGED_APP_ID)?;
    }
    // The hist_* tables come from report_history, which owns them; on an existing
    // file this also widens hist_snapshots if it predates a column.
    crate::report_history::ensure_schema(&conn)?;
    Ok(conn)
}

/// Columns a table actually has, in declared order.
fn table_columns(conn: &Connection, qualified_table: &str) -> rusqlite::Result<Vec<String>> {
    let stmt = conn.prepare(&format!("SELECT * FROM {} LIMIT 0", qualified_table))?;
    let names: Vec<String> = stmt.column_names().into_iter().map(str::to_string).collect();
    // `stmt` borrows `conn`; drop before returning so callers can reuse it.
    drop(stmt);
    Ok(names)
}

/// Carry one history table over from a previous report.db.
///
/// Naming both sides explicitly is what makes this safe. `SELECT *` matched
/// columns by position, so an older report missing a column failed the whole
/// merge ("table has 12 columns but 11 values were supplied" — which also skips
/// writing the new snapshot), and a report whose columns were declared in a
/// different order was copied with the values silently shifted one place. Only
/// the columns present in both are copied; anything the source lacks keeps its
/// default.
fn carry_history_table(conn: &Connection, table: &str) -> rusqlite::Result<()> {
    let has_source: bool = conn.query_row(
        "SELECT COUNT(*) FROM srcreport.sqlite_master WHERE type = 'table' AND name = ?1",
        rusqlite::params![table],
        |r| r.get::<_, i64>(0),
    )? > 0;
    if !has_source {
        return Ok(());
    }
    let target = table_columns(conn, table)?;
    let source = table_columns(conn, &format!("srcreport.{}", table))?;
    let shared: Vec<&str> = target
        .iter()
        .filter(|c| source.iter().any(|s| s == *c))
        .map(String::as_str)
        .collect();
    if shared.is_empty() {
        return Ok(());
    }
    let cols = shared.join(", ");
    conn.execute_batch(&format!(
        "INSERT OR IGNORE INTO {table} ({cols}) SELECT {cols} FROM srcreport.{table};"
    ))
}

/// Copy all rows from source DBs into a merged report.db.
/// Expects `source_dir` to contain the 4 source files.
/// The target `merged_db` is created/updated atomically via temp + rename.
/// Source files are NOT deleted (caller owns cleanup).
pub fn merge_into_single_db(
    source_dir: &Path,
    merged_db: &Path,
    view_name: &str,
    view_path: &str,
    timestamp: i64,
) -> rusqlite::Result<()> {
    let tmp = merged_db.with_extension("tmp");

    // Remove stale tmp from previous run if present
    let _ = std::fs::remove_file(&tmp);

    // Build schemas + pragmas
    let mut conn = open_merged_db(&tmp)?;

    // ATTACH all 4 source DBs
    let source_detail = source_dir.join("data_detail.db");
    // Treemap DB can be at old location (tree_map_data/treemap.db) or new (treemap.db at root)
    let source_treemap_old = source_dir.join("tree_map_data").join("treemap.db");
    let source_treemap_new = source_dir.join("treemap.db");
    let source_treemap = if source_treemap_new.exists() { source_treemap_new } else { source_treemap_old };
    let source_perm = source_dir.join("permission_issues.db");
    let source_report = source_dir.join("report.db");

    if source_detail.exists() {
        conn.execute("ATTACH DATABASE ?1 AS srcdetail", rusqlite::params![source_detail.to_string_lossy().as_ref()])?;
        conn.execute_batch(
            "INSERT OR IGNORE INTO meta SELECT key, value FROM srcdetail.meta;
             INSERT OR IGNORE INTO detail_users SELECT * FROM srcdetail.users;
             INSERT OR IGNORE INTO detail_file_names SELECT * FROM srcdetail.file_names;
             INSERT OR IGNORE INTO detail_dirs SELECT * FROM srcdetail.dirs;
             INSERT INTO detail_files SELECT * FROM srcdetail.files;"
        )?;
        conn.execute("DETACH DATABASE srcdetail", [])?;
    }

    if source_treemap.exists() {
        conn.execute("ATTACH DATABASE ?1 AS srctreemap", rusqlite::params![source_treemap.to_string_lossy().as_ref()])?;
        conn.execute_batch(
            "INSERT OR IGNORE INTO treemap_names SELECT * FROM srctreemap.names;
             INSERT OR IGNORE INTO treemap_owners SELECT * FROM srctreemap.owners;
             INSERT OR IGNORE INTO treemap_dirs SELECT * FROM srctreemap.dirs;"
        )?;
        conn.execute("DETACH DATABASE srctreemap", [])?;
    }

    if source_perm.exists() {
        conn.execute("ATTACH DATABASE ?1 AS srcperm", rusqlite::params![source_perm.to_string_lossy().as_ref()])?;
        conn.execute_batch(
            "INSERT OR IGNORE INTO perm_issues SELECT * FROM srcperm.issues;"
        )?;
        conn.execute("DETACH DATABASE srcperm", [])?;
    }

    if source_report.exists() {
        conn.execute("ATTACH DATABASE ?1 AS srcreport", rusqlite::params![source_report.to_string_lossy().as_ref()])?;
        for table in ["hist_snapshots", "hist_team_usage", "hist_user_usage"] {
            carry_history_table(&conn, table)?;
        }
        conn.execute("DETACH DATABASE srcreport", [])?;
    }

    // Ensure scan identity in meta
    {
        let tx = conn.transaction()?;
        tx.execute("INSERT OR REPLACE INTO meta(key, value) VALUES ('scan_name', ?1)", rusqlite::params![view_name])?;
        tx.execute("INSERT OR REPLACE INTO meta(key, value) VALUES ('scan_path', ?1)", rusqlite::params![view_path])?;
        tx.execute("INSERT OR REPLACE INTO meta(key, value) VALUES ('scan_timestamp', ?1)", rusqlite::params![timestamp.to_string()])?;
        tx.commit()?;
    }

    // Build indexes
    conn.execute_batch(MERGED_INDEX_DDL)?;

    // Build FTS5 trigram indexes so the dashboard's name search is a lookup
    // instead of a LIKE scan of every interned name. External-content tables
    // store only the index (the names already live in detail_file_names /
    // treemap_names), and trigram gives infix matching with an index. The
    // dashboard reads the report readonly, so this has to be built here, at
    // scan time. Rebuild populates the index from the content tables.
    conn.execute_batch(MERGED_FTS_DDL)?;

    // ANALYZE for query planner
    conn.execute_batch("ANALYZE;")?;

    // Close and atomically rename
    drop(conn);
    std::fs::rename(&tmp, merged_db)
        .map_err(|e| rusqlite::Error::InvalidPath(PathBuf::from(format!("rename {} -> {}: {}", tmp.display(), merged_db.display(), e))))?;

    Ok(())
}


fn apply_build_pragmas(conn: &Connection) -> rusqlite::Result<()> {
    // page_size MUST be set before any table is created.
    conn.pragma_update(None, "page_size", PAGE_SIZE)?;
    conn.pragma_update(None, "journal_mode", "OFF")?;
    conn.pragma_update(None, "synchronous", "OFF")?;
    conn.pragma_update(None, "temp_store", "MEMORY")?;
    conn.pragma_update(None, "locking_mode", "EXCLUSIVE")?;
    conn.pragma_update(None, "cache_size", -1_048_576i64)?;
    conn.pragma_update(None, "foreign_keys", "OFF")?;
    Ok(())
}

fn open_for_build(build_path: &Path) -> PyResult<Connection> {
    if build_path.exists() {
        fs::remove_file(build_path).map_err(|e| {
            PyRuntimeError::new_err(format!("rm old {}: {}", build_path.display(), e))
        })?;
    }
    if let Some(parent) = build_path.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| PyRuntimeError::new_err(format!("mkdir {}: {}", parent.display(), e)))?;
    }
    let conn = Connection::open(build_path)
        .map_err(|e| PyRuntimeError::new_err(format!("open {}: {}", build_path.display(), e)))?;
    apply_build_pragmas(&conn)
        .map_err(|e| PyRuntimeError::new_err(format!("pragma: {}", e)))?;
    Ok(conn)
}

fn stamp_db(conn: &Connection, app_id: i32) -> PyResult<()> {
    conn.pragma_update(None, "application_id", app_id)
        .map_err(|e| PyRuntimeError::new_err(format!("application_id: {}", e)))?;
    conn.pragma_update(None, "user_version", SCHEMA_VERSION)
        .map_err(|e| PyRuntimeError::new_err(format!("user_version: {}", e)))?;
    Ok(())
}

/// VACUUM INTO is skipped outside the `[MIN, MAX]` range. Inserts happen in
/// PK-ascending order with `journal_mode=OFF`, so freed pages are ~zero and
/// page clustering is already near-optimal — VACUUM mostly buys a marginally
/// smaller file at the cost of read-all + write-all I/O.
///
/// - Below MIN: page-clustering gain doesn't justify the extra IO.
/// - Above MAX: rewrite cost dominates (e.g. ~3min on 15 GB DB observed in
///   production). Build files are already well-clustered; ship them as-is.
const VACUUM_SIZE_MIN_BYTES: u64 = 100 * 1024 * 1024;
const VACUUM_SIZE_MAX_BYTES: u64 = 1024 * 1024 * 1024;

/// `skip_repack` drops the `PRAGMA optimize` + `VACUUM INTO` pass, for a DB that
/// is only going to be read sequentially once and then deleted. The build file is
/// still renamed into `final_path` either way.
fn finalize_db(
    conn: Connection,
    build_path: &Path,
    final_path: &Path,
    skip_repack: bool,
) -> PyResult<()> {
    let tmp_path = final_path.with_extension("tmp.db");
    if tmp_path.exists() {
        let _ = fs::remove_file(&tmp_path);
    }

    // Run ANALYZE / VACUUM INTO inside a closure so we can clean up the
    // partial tmp.db on any error path before propagating the failure.
    // VACUUM INTO is the only step here that can leave a half-written file
    // around (e.g. when /tmp or the destination disk runs out of space mid-
    // write); a leaked tmp.db would otherwise sit on disk until the next run.
    let result: PyResult<()> = (|| {
        let t_analyze = Instant::now();
        if !skip_repack {
            conn.execute_batch("PRAGMA optimize;")
                .map_err(|e| PyRuntimeError::new_err(format!("optimize: {}", e)))?;
        }
        let analyze_secs = t_analyze.elapsed().as_secs_f64();

        let build_size = fs::metadata(build_path).map(|m| m.len()).unwrap_or(0);
        let skip_vacuum = skip_repack
            || build_size < VACUUM_SIZE_MIN_BYTES
            || build_size > VACUUM_SIZE_MAX_BYTES;

        let mut vacuum_secs = 0.0f64;
        if !skip_vacuum {
            let tmp_str = tmp_path.to_string_lossy().replace('\'', "''");
            let t_vacuum = Instant::now();
            conn.execute(&format!("VACUUM INTO '{}'", tmp_str), [])
                .map_err(|e| PyRuntimeError::new_err(format!("vacuum into: {}", e)))?;
            vacuum_secs = t_vacuum.elapsed().as_secs_f64();
        }
        drop(conn);

        let final_name = final_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("<db>");
        let size_mb = build_size as f64 / (1024.0 * 1024.0);
        if skip_vacuum {
            let reason = if skip_repack {
                "merge input"
            } else if build_size < VACUUM_SIZE_MIN_BYTES {
                "small"
            } else {
                "huge"
            };
            println!(
                "[finalize] {}: analyze {:.2}s, vacuum skipped ({}, {:.1} MB)",
                final_name, analyze_secs, reason, size_mb
            );
        } else {
            println!(
                "[finalize] {}: analyze {:.2}s, vacuum {:.2}s ({:.1} MB)",
                final_name, analyze_secs, vacuum_secs, size_mb
            );
        }

        // No remove-then-rename: on POSIX rename() replaces the destination
        // atomically, so deleting the old file first would leave a window where
        // a kill loses the previous report entirely. The next merge then finds
        // no source file and silently skips detail/treemap.
        let source = if skip_vacuum { build_path } else { &tmp_path };
        fs::rename(source, final_path).map_err(|e| {
            PyRuntimeError::new_err(format!(
                "rename {} -> {}: {}",
                source.display(),
                final_path.display(),
                e
            ))
        })?;

        if !skip_vacuum {
            let _ = fs::remove_file(build_path);
        }
        Ok(())
    })();

    if result.is_err() && tmp_path.exists() {
        let _ = fs::remove_file(&tmp_path);
    }
    result
}

fn insert_meta(conn: &mut Connection, meta: &[(String, String)]) -> PyResult<()> {
    if meta.is_empty() {
        return Ok(());
    }
    let tx = conn
        .transaction()
        .map_err(|e| PyRuntimeError::new_err(format!("tx meta: {}", e)))?;
    {
        let mut stmt = tx
            .prepare("INSERT OR REPLACE INTO meta(key, value) VALUES (?, ?)")
            .map_err(|e| PyRuntimeError::new_err(format!("prep meta: {}", e)))?;
        for (k, v) in meta {
            stmt.execute(params![k, v])
                .map_err(|e| PyRuntimeError::new_err(format!("ins meta: {}", e)))?;
        }
    }
    tx.commit()
        .map_err(|e| PyRuntimeError::new_err(format!("commit meta: {}", e)))?;
    Ok(())
}

/// Build "(?,?,...,?)" placeholder group for `cols` columns.
fn placeholder_group(cols: usize) -> String {
    let mut s = String::with_capacity(2 + cols * 2);
    s.push('(');
    for i in 0..cols {
        if i > 0 {
            s.push(',');
        }
        s.push('?');
    }
    s.push(')');
    s
}

/// Multi-row `INSERT INTO <table>(<cols>) VALUES (...),(...),...` packing.
///
/// Splits `rows` into chunks of [`PACK_ROWS`] and emits one statement per
/// chunk inside a single transaction. This collapses N parameter-binding +
/// plan-execution round trips into N/PACK_ROWS, giving 2-5× speedup for
/// the hot insert paths (files, names, dirs, dir_user_size).
fn packed_insert<F>(
    conn: &mut Connection,
    table: &str,
    columns: &str,
    cols: usize,
    rows_len: usize,
    mut bind_row: F,
    label: &str,
) -> PyResult<()>
where
    F: FnMut(usize) -> Vec<Box<dyn ToSql + Send>>,
{
    if rows_len == 0 {
        return Ok(());
    }
    let tx = conn
        .transaction()
        .map_err(|e| PyRuntimeError::new_err(format!("tx {}: {}", label, e)))?;
    {
        let group = placeholder_group(cols);

        let full_chunks = rows_len / PACK_ROWS;
        let tail = rows_len % PACK_ROWS;

        // Cache the prepared statement for full-size chunks.
        let mut full_sql = String::new();
        let mut full_stmt_opt = None;
        if full_chunks > 0 {
            full_sql.push_str("INSERT INTO ");
            full_sql.push_str(table);
            full_sql.push('(');
            full_sql.push_str(columns);
            full_sql.push_str(") VALUES ");
            for i in 0..PACK_ROWS {
                if i > 0 {
                    full_sql.push(',');
                }
                full_sql.push_str(&group);
            }
            let stmt = tx
                .prepare(&full_sql)
                .map_err(|e| PyRuntimeError::new_err(format!("prep {} full: {}", label, e)))?;
            full_stmt_opt = Some(stmt);
        }

        let mut row_idx = 0usize;
        if let Some(mut stmt) = full_stmt_opt {
            for _ in 0..full_chunks {
                let mut binds: Vec<Box<dyn ToSql + Send>> =
                    Vec::with_capacity(PACK_ROWS * cols);
                for _ in 0..PACK_ROWS {
                    binds.append(&mut bind_row(row_idx));
                    row_idx += 1;
                }
                stmt.execute(params_from_iter(binds.iter().map(|b| b.as_ref())))
                    .map_err(|e| {
                        PyRuntimeError::new_err(format!("ins {} (packed): {}", label, e))
                    })?;
            }
        }

        if tail > 0 {
            let mut sql = String::new();
            sql.push_str("INSERT INTO ");
            sql.push_str(table);
            sql.push('(');
            sql.push_str(columns);
            sql.push_str(") VALUES ");
            for i in 0..tail {
                if i > 0 {
                    sql.push(',');
                }
                sql.push_str(&group);
            }
            let mut stmt = tx
                .prepare(&sql)
                .map_err(|e| PyRuntimeError::new_err(format!("prep {} tail: {}", label, e)))?;
            let mut binds: Vec<Box<dyn ToSql + Send>> = Vec::with_capacity(tail * cols);
            for _ in 0..tail {
                binds.append(&mut bind_row(row_idx));
                row_idx += 1;
            }
            stmt.execute(params_from_iter(binds.iter().map(|b| b.as_ref())))
                .map_err(|e| PyRuntimeError::new_err(format!("ins {} (tail): {}", label, e)))?;
        }
    }
    tx.commit()
        .map_err(|e| PyRuntimeError::new_err(format!("commit {}: {}", label, e)))?;
    Ok(())
}

// ─── Treemap input + builder ──────────────────────────────────────────

#[derive(Clone)]
pub struct DirRow {
    pub id: i64,
    pub parent_id: Option<i64>,
    pub name_id: i64,
    pub total_size: i64,
    pub file_count: i64,
    pub dir_count: i64,
    pub owner_uid: i64,
    pub has_files: i64,
}

pub struct OwnerRow {
    pub uid: i64,
    pub username: String,
}

pub struct TreemapInput {
    pub names: Vec<String>,
    pub owners: Vec<OwnerRow>,
    pub dirs: Vec<DirRow>,
    pub meta: Vec<(String, String)>,
}

pub fn build_treemap_db(
    final_path: &Path,
    work_dir: &Path,
    input: TreemapInput,
    debug: bool,
) -> PyResult<()> {
    let build_path: PathBuf = work_dir.join("treemap.build.db");
    let mut conn = open_for_build(&build_path)?;

    conn.execute_batch(TREEMAP_DDL)
        .map_err(|e| PyRuntimeError::new_err(format!("treemap ddl: {}", e)))?;

    // names: 2 cols × N rows
    {
        let names = &input.names;
        packed_insert(
            &mut conn,
            "names",
            "id, name",
            2,
            names.len(),
            |i| {
                let id = i as i64;
                let name = names[i].clone();
                vec![Box::new(id), Box::new(name)]
            },
            "names",
        )?;
    }

    // owners: 2 cols × N rows
    {
        let owners = &input.owners;
        packed_insert(
            &mut conn,
            "owners",
            "uid, username",
            2,
            owners.len(),
            |i| {
                let uid = owners[i].uid;
                let username = owners[i].username.clone();
                vec![Box::new(uid), Box::new(username)]
            },
            "owners",
        )?;
    }

    // dirs: 8 cols × N rows
    {
        let dirs = &input.dirs;
        packed_insert(
            &mut conn,
            "dirs",
            "id, parent_id, name_id, total_size, file_count, dir_count, owner_uid, has_files",
            8,
            dirs.len(),
            |i| {
                let d = &dirs[i];
                vec![
                    Box::new(d.id),
                    Box::new(d.parent_id),
                    Box::new(d.name_id),
                    Box::new(d.total_size),
                    Box::new(d.file_count),
                    Box::new(d.dir_count),
                    Box::new(d.owner_uid),
                    Box::new(d.has_files),
                ]
            },
            "dirs",
        )?;
    }

    insert_meta(&mut conn, &input.meta)?;

    conn.execute_batch(TREEMAP_INDEX_DDL)
        .map_err(|e| PyRuntimeError::new_err(format!("treemap idx: {}", e)))?;

    stamp_db(&conn, TREEMAP_APP_ID)?;

    if debug {
        println!(
            "[Phase 2] treemap.db built (names={}, dirs={}, owners={})",
            input.names.len(),
            input.dirs.len(),
            input.owners.len()
        );
    }

    // treemap.db keeps its repack pass: it is small enough that VACUUM is
    // already skipped by the size band, so there is nothing to save here.
    finalize_db(conn, &build_path, final_path, false)?;
    Ok(())
}

// ─── Detail incremental builder ───────────────────────────────────────

pub struct UserRow {
    pub uid: i64,
    pub username: String,
    pub team_id: String,
    pub total_files: i64,
    pub total_dirs: i64,
    pub total_size: i64,
    pub permission_issues: i64,
    pub is_target: i64,
    /// Directories owned by this user — see the `users` DDL for why it is
    /// tallied here rather than counted by the reader.
    pub owned_dirs: i64,
}


pub struct FileRow {
    pub dir_id: i64,
    pub name_id: i64,
    pub ext: String,
    pub uid: i64,
    pub size: i64,
}

pub struct DetailBuildHandle {
    conn: Connection,
    build_path: PathBuf,
    final_path: PathBuf,
    debug: bool,
    files_inserted: i64,
}

pub fn detail_open(
    final_path: &Path,
    work_dir: &Path,
    debug: bool,
) -> PyResult<DetailBuildHandle> {
    let build_path: PathBuf = work_dir.join("data_detail.build.db");
    let conn = open_for_build(&build_path)?;

    conn.execute_batch(DETAIL_DDL)
        .map_err(|e| PyRuntimeError::new_err(format!("detail ddl: {}", e)))?;

    Ok(DetailBuildHandle {
        conn,
        build_path,
        final_path: final_path.to_path_buf(),
        debug,
        files_inserted: 0,
    })
}

pub fn detail_insert_users(handle: &mut DetailBuildHandle, users: &[UserRow]) -> PyResult<()> {
    if users.is_empty() {
        return Ok(());
    }
    let tx = handle
        .conn
        .transaction()
        .map_err(|e| PyRuntimeError::new_err(format!("tx users: {}", e)))?;
    {
        let mut stmt = tx
            .prepare(
                "INSERT INTO users(uid, username, team_id, total_files, total_dirs, \
                 total_size, permission_issues, is_target, owned_dirs) \
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
            )
            .map_err(|e| PyRuntimeError::new_err(format!("prep users: {}", e)))?;
        for u in users {
            stmt.execute(params![
                u.uid,
                u.username,
                u.team_id,
                u.total_files,
                u.total_dirs,
                u.total_size,
                u.permission_issues,
                u.is_target,
                u.owned_dirs,
            ])
            .map_err(|e| PyRuntimeError::new_err(format!("ins user: {}", e)))?;
        }
    }
    tx.commit()
        .map_err(|e| PyRuntimeError::new_err(format!("commit users: {}", e)))?;
    Ok(())
}

/// Insert file basenames into detail.db's local `file_names` table.
/// Multi-row VALUES packing for fast bulk insert at scale.
pub fn detail_insert_file_names(handle: &mut DetailBuildHandle, names: &[String]) -> PyResult<()> {
    let len = names.len();
    if len == 0 {
        return Ok(());
    }
    packed_insert(
        &mut handle.conn,
        "file_names",
        "id, name",
        2,
        len,
        |i| {
            let id = i as i64;
            let name = names[i].clone();
            vec![Box::new(id), Box::new(name)]
        },
        "file_names",
    )
}

pub(crate) fn detail_insert_dirs(
    handle: &mut DetailBuildHandle,
    // (id, uid, parent_id, path, owner_uid, size, files)
    dirs: &[(i64, i64, Option<i64>, String, i64, i64, i64)],
) -> PyResult<()> {
    let tx = handle
        .conn
        .transaction()
        .map_err(|e| PyRuntimeError::new_err(format!("dirs tx: {}", e)))?;
    {
        let mut stmt = tx
            .prepare_cached("INSERT INTO dirs(id, uid, parent_id, path, owner_uid, size, files) VALUES (?,?,?,?,?,?,?)")
            .map_err(|e| PyRuntimeError::new_err(format!("dirs prepare: {}", e)))?;
        for (id, uid, parent_id, path, owner_uid, size, files) in dirs {
            stmt.execute(params![id, uid, parent_id, path, owner_uid, size, files])
                .map_err(|e| PyRuntimeError::new_err(format!("dirs insert: {}", e)))?;
        }
    }
    tx.commit()
        .map_err(|e| PyRuntimeError::new_err(format!("dirs commit: {}", e)))?;
    Ok(())
}

pub fn detail_insert_files_chunk(
    handle: &mut DetailBuildHandle,
    rows: &[FileRow],
) -> PyResult<()> {
    let len = rows.len();
    if len == 0 {
        return Ok(());
    }
    packed_insert(
        &mut handle.conn,
        "files",
        "dir_id, name_id, ext, uid, size",
        5,
        len,
        |i| {
            let r = &rows[i];
            vec![
                Box::new(r.dir_id),
                Box::new(r.name_id),
                Box::new(r.ext.clone()),
                Box::new(r.uid),
                Box::new(r.size),
            ]
        },
        "files",
    )?;
    handle.files_inserted += len as i64;
    Ok(())
}

pub fn detail_set_meta(handle: &mut DetailBuildHandle, meta: &[(String, String)]) -> PyResult<()> {
    insert_meta(&mut handle.conn, meta)
}

/// Finalize the intermediate `data_detail.db`.
///
/// `for_merge` says this DB exists only to be consumed by
/// `merge_into_single_db` and then deleted. In that case the indexes and the
/// VACUUM are pure waste: the merge reads every table with a sequential
/// `INSERT INTO … SELECT *` (no index can help that) and rebuilds the same
/// three indexes on `report.db` afterwards. Skipping them here removes a
/// duplicated index build over 1.5M rows plus a full file rewrite.
///
/// Pass `false` when the caller wants a standalone, queryable `data_detail.db`
/// (bare table names, as `detail_prefix()` supports) rather than a merge input.
pub fn detail_finalize(handle: DetailBuildHandle, for_merge: bool) -> PyResult<i64> {
    // Boost cache + mmap for CREATE INDEX phase. Indexes are built by
    // scanning the entire `files` table multiple times — bigger cache
    // means fewer disk re-reads. Restored to default after finalize.
    handle
        .conn
        .execute_batch(
            "PRAGMA cache_size = -4194304;\
             PRAGMA mmap_size = 8589934592;",
        )
        .map_err(|e| PyRuntimeError::new_err(format!("pragma boost: {}", e)))?;

    if !for_merge {
        handle
            .conn
            .execute_batch(DETAIL_INDEX_DDL)
            .map_err(|e| PyRuntimeError::new_err(format!("detail idx: {}", e)))?;
    }

    stamp_db(&handle.conn, DETAIL_APP_ID)?;

    if handle.debug {
        println!(
            "[Phase 2] data_detail.db built (files={})",
            handle.files_inserted
        );
    }

    let DetailBuildHandle {
        conn,
        build_path,
        final_path,
        files_inserted,
        ..
    } = handle;
    // `skip_repack`: for a merge input, PRAGMA optimize (stats for a query
    // planner that will never run a query here) and VACUUM INTO (a full rewrite
    // of a file about to be deleted) are both wasted work. The file still gets
    // moved into place, because the merge reads it by name.
    finalize_db(conn, &build_path, &final_path, for_merge)?;
    Ok(files_inserted)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Read a snapshot row back by column name, which is how the dashboard reads it.
    fn snapshot_by_name(db: &Path, col: &str) -> Option<i64> {
        let conn = Connection::open(db).unwrap();
        conn.query_row(
            &format!("SELECT {} FROM hist_snapshots WHERE scan_date = 20240101", col),
            [],
            |r| r.get::<_, Option<i64>>(0),
        )
        .unwrap()
    }

    /// Write a previous-generation report.db whose hist_snapshots has the given
    /// columns, then merge over it and return the merged path.
    fn merge_over_source(dir: &Path, columns: &str, values: &str) -> std::path::PathBuf {
        let source = dir.join("report.db");
        {
            let conn = Connection::open(&source).unwrap();
            conn.execute_batch(&format!(
                "CREATE TABLE hist_snapshots ({columns});
                 INSERT INTO hist_snapshots VALUES ({values});"
            ))
            .unwrap();
        }
        let merged = dir.join("merged.db");
        merge_into_single_db(dir, &merged, "t", "/t", 1_700_000_000).unwrap();
        merged
    }

    #[test]
    fn carries_history_from_a_report_missing_newer_columns() {
        // A report written before scanned_size and the inode columns existed.
        // Positional SELECT * failed the whole merge on this, which also cost the
        // run its new snapshot.
        let dir = tempfile::tempdir().unwrap();
        let merged = merge_over_source(
            dir.path(),
            "id INTEGER PRIMARY KEY, scan_date INTEGER NOT NULL UNIQUE, scanned_at INTEGER, \
             path TEXT, total INTEGER, used INTEGER, available INTEGER",
            "1, 20240101, 1704067200, '/', 10000, 6000, 4000",
        );

        assert_eq!(snapshot_by_name(&merged, "used"), Some(6000));
        // Absent in the source, so it stays unset rather than borrowing a neighbour.
        assert_eq!(snapshot_by_name(&merged, "inodes_total"), None);
    }

    #[test]
    fn carries_history_from_a_report_whose_columns_are_declared_in_another_order() {
        // Same column set, different declaration order: this is the case that used
        // to succeed while shifting every value one position.
        let dir = tempfile::tempdir().unwrap();
        let merged = merge_over_source(
            dir.path(),
            "id INTEGER PRIMARY KEY, scan_date INTEGER NOT NULL UNIQUE, scanned_at INTEGER, \
             path TEXT, total INTEGER, used INTEGER, available INTEGER, \
             inodes_total INTEGER, inodes_used INTEGER, inodes_free INTEGER, \
             inodes_scanned INTEGER, scanned_size INTEGER",
            "1, 20240101, 1704067200, '/', 10000, 6000, 4000, 8000, 3000, 5000, 31, 777",
        );

        assert_eq!(snapshot_by_name(&merged, "scanned_size"), Some(777));
        assert_eq!(snapshot_by_name(&merged, "inodes_total"), Some(8000));
        assert_eq!(snapshot_by_name(&merged, "inodes_scanned"), Some(31));
    }

    #[test]
    fn merges_a_report_with_no_history_tables_at_all() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("report.db");
        drop(Connection::open(&source).unwrap());
        let merged = dir.path().join("merged.db");

        merge_into_single_db(dir.path(), &merged, "t", "/t", 1_700_000_000).unwrap();

        let conn = Connection::open(&merged).unwrap();
        let rows: i64 = conn
            .query_row("SELECT COUNT(*) FROM hist_snapshots", [], |r| r.get(0))
            .unwrap();
        assert_eq!(rows, 0);
    }
}
