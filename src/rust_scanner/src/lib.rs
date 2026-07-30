use pyo3::prelude::*;
use std::collections::HashMap;

mod db_writer;
mod pipe_events;
mod pipe_io;
mod pipe_permission;
mod pipe_treemap;
mod pipe_types;
mod report_history;
mod report_pipeline;
mod scan_constants;
mod scan_core;
mod scan_orchestrator;
mod scan_state;
mod scan_utils;

/// Sanitise a raw byte string (possibly lossy-decoded) so the result is
/// valid UTF-8 JSON: replace any surrogate or invalid code points with U+FFFD.
pub(crate) fn sanitise_path(raw: &str) -> String {
    raw.chars()
        .map(|c| {
            if c == '\u{FFFD}' || (c.is_control() && c != '\t') {
                '\u{FFFD}'
            } else {
                c
            }
        })
        .collect()
}

#[pyfunction(signature = (directory, skip_dirs, target_uids, max_workers=None, debug=false))]
fn scan_disk(
    py: Python,
    directory: String,
    skip_dirs: Vec<String>,
    target_uids: Option<Vec<u32>>,
    max_workers: Option<usize>,
    debug: bool,
) -> PyResult<PyObject> {
    scan_core::run_scan_core(
        py,
        directory,
        skip_dirs,
        target_uids,
        max_workers,
        debug,
        "production",
        None,
    )
}

#[pyfunction(signature = (tmpdir, uids_map, team_map, detail_db_path, treemap_db_path, treemap_root, max_level, min_size_bytes, timestamp, max_workers, build_treemap=true, path_prefix=None, debug=false))]
#[allow(clippy::too_many_arguments)]
fn build_detail_db(
    py: Python<'_>,
    tmpdir: String,
    uids_map: HashMap<u32, String>,
    team_map: HashMap<String, String>,
    detail_db_path: String,
    treemap_db_path: String,
    treemap_root: String,
    max_level: usize,
    min_size_bytes: i64,
    timestamp: i64,
    max_workers: usize,
    build_treemap: bool,
    path_prefix: Option<String>,
    debug: bool,
) -> PyResult<(u64, Option<String>)> {
    // Configure glibc allocator to reduce heap fragmentation during large
    // parallel workloads. M_MMAP_THRESHOLD forces allocations > 128KB to
    // use mmap() which is returned to OS immediately on free().
    // M_TRIM_THRESHOLD triggers heap trimming more aggressively.
    #[cfg(target_os = "linux")]
    unsafe {
        extern "C" {
            fn mallopt(param: i32, value: i32) -> i32;
        }
        mallopt(-3, 128 * 1024); // M_MMAP_THRESHOLD = 128KB
        mallopt(-1, 128 * 1024); // M_TRIM_THRESHOLD = 128KB
    }

    let (count, agg_path) = report_pipeline::build_detail_db_impl(
        py, tmpdir, uids_map, team_map, detail_db_path, treemap_db_path,
        treemap_root, max_level, min_size_bytes, timestamp, max_workers,
        build_treemap, debug, path_prefix,
    )?;
    Ok((count, agg_path.map(|p| p.to_string_lossy().into_owned())))
}

#[pyfunction(signature = (aggregates_path, treemap_db_path, treemap_root, max_level, min_size_bytes, timestamp, debug=false))]
#[allow(clippy::too_many_arguments)]
fn build_treemap_db(
    py: Python<'_>,
    aggregates_path: String,
    treemap_db_path: String,
    treemap_root: String,
    max_level: usize,
    min_size_bytes: i64,
    timestamp: i64,
    debug: bool,
) -> PyResult<()> {
    py.allow_threads(|| {
        report_pipeline::build_treemap_db_impl(
            std::path::Path::new(&aggregates_path),
            std::path::Path::new(&treemap_db_path),
            &treemap_root,
            max_level,
            min_size_bytes,
            timestamp,
            debug,
        )
    })
}

#[pyfunction(signature = (tmpdir, uids_map, team_map, detail_db_path, treemap_db_path, treemap_root, max_level, min_size_bytes, timestamp, max_workers, build_treemap=true, debug=false))]
#[allow(clippy::too_many_arguments)]
fn build_pipeline(
    py: Python<'_>,
    tmpdir: String,
    uids_map: HashMap<u32, String>,
    team_map: HashMap<String, String>,
    detail_db_path: String,
    treemap_db_path: String,
    treemap_root: String,
    max_level: usize,
    min_size_bytes: i64,
    timestamp: i64,
    max_workers: usize,
    build_treemap: bool,
    debug: bool,
) -> PyResult<u64> {
    // Configure glibc allocator to reduce heap fragmentation during large
    // parallel workloads. M_MMAP_THRESHOLD forces allocations > 128KB to
    // use mmap() which is returned to OS immediately on free().
    // M_TRIM_THRESHOLD triggers heap trimming more aggressively.
    #[cfg(target_os = "linux")]
    unsafe {
        extern "C" {
            fn mallopt(param: i32, value: i32) -> i32;
        }
        mallopt(-3, 128 * 1024); // M_MMAP_THRESHOLD = 128KB
        mallopt(-1, 128 * 1024); // M_TRIM_THRESHOLD = 128KB
    }

    let (count, agg_path) = report_pipeline::build_detail_db_impl(
        py, tmpdir, uids_map, team_map, detail_db_path.clone(), treemap_db_path.clone(),
        treemap_root.clone(), max_level, min_size_bytes, timestamp, max_workers,
        build_treemap, debug, None,
    )?;
    if let Some(p) = agg_path {
        report_pipeline::build_treemap_db_impl(
            &p,
            std::path::Path::new(&treemap_db_path),
            &treemap_root,
            max_level, min_size_bytes, timestamp, debug,
        )?;
    }
    Ok(count)
}

/// Upsert one daily history snapshot into a target's `report.db`.
///
/// `teams` / `users` are lists of (name, team_id_or_None, size, kind) tuples.
/// `scan_date` is derived from `timestamp` (local yyyymmdd); re-running the same
/// day overrides the prior snapshot.
#[pyfunction(signature = (db_path, timestamp, path, total, used, available, teams, users))]
#[allow(clippy::too_many_arguments)]
fn upsert_history_snapshot(
    py: Python<'_>,
    db_path: String,
    timestamp: i64,
    path: String,
    total: i64,
    used: i64,
    available: i64,
    teams: Vec<(String, Option<i64>, i64, String)>,
    users: Vec<(String, Option<i64>, i64, String)>,
) -> PyResult<()> {
    use pyo3::exceptions::PyRuntimeError;
    py.allow_threads(move || {
        let conn = rusqlite::Connection::open(&db_path)
            .map_err(|e| PyRuntimeError::new_err(format!("open {}: {}", db_path, e)))?;
        let meta = report_history::SnapshotMeta { path, total, used, available };
        let to_rows = |v: Vec<(String, Option<i64>, i64, String)>| {
            v.into_iter()
                .map(|(name, team_id, size, kind)| report_history::UsageRow {
                    name,
                    team_id,
                    size,
                    kind,
                })
                .collect::<Vec<_>>()
        };
        report_history::upsert_snapshot(&conn, timestamp, &meta, &to_rows(teams), &to_rows(users))
            .map_err(|e| PyRuntimeError::new_err(format!("upsert snapshot: {}", e)))
    })
}

/// Delete history snapshots older than `cutoff_yyyymmdd`. Returns rows removed.
#[pyfunction]
fn purge_history(py: Python<'_>, db_path: String, cutoff_yyyymmdd: i64) -> PyResult<usize> {
    use pyo3::exceptions::PyRuntimeError;
    py.allow_threads(move || {
        let conn = rusqlite::Connection::open(&db_path)
            .map_err(|e| PyRuntimeError::new_err(format!("open {}: {}", db_path, e)))?;
        report_history::purge_older_than(&conn, cutoff_yyyymmdd)
            .map_err(|e| PyRuntimeError::new_err(format!("purge: {}", e)))
    })
}

/// Multi-target orchestrator entry. Takes a JSON-serialised ScanPlan (built by
/// Python scan_scheduler.plan_to_dict) and runs Phase 1 + Phase 2 per target,
/// writing per-target outputs and history snapshots. Returns a manifest-summary
/// JSON string.
#[pyfunction(signature = (plan_json, build_treemap=true, max_level=3, timestamp=0, debug=false))]
fn run_scan_plan(
    py: Python<'_>,
    plan_json: String,
    build_treemap: bool,
    max_level: usize,
    timestamp: i64,
    debug: bool,
) -> PyResult<String> {
    scan_orchestrator::run_scan_plan_impl(py, plan_json, build_treemap, max_level, timestamp, debug)
}

#[pymodule]
fn fast_scanner(_py: Python, m: &PyModule) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(scan_disk, m)?)?;
    m.add_function(wrap_pyfunction!(build_detail_db, m)?)?;
    m.add_function(wrap_pyfunction!(build_treemap_db, m)?)?;
    m.add_function(wrap_pyfunction!(build_pipeline, m)?)?;
    m.add_function(wrap_pyfunction!(upsert_history_snapshot, m)?)?;
    m.add_function(wrap_pyfunction!(purge_history, m)?)?;
    m.add_function(wrap_pyfunction!(run_scan_plan, m)?)?;
    Ok(())
}
