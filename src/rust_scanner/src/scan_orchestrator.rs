//! scan_orchestrator.rs — multi-target scan orchestration (GĐ1d).
//!
//! Drives the full pipeline for a `ScanPlan` (built in Python, serialised to
//! JSON). For each device group, each physical root, each view:
//!   1. Phase 1 `run_scan_core` ONCE per physical root (superset, target_uids=None).
//!   2. Phase 2 `build_detail_db_impl` per view (parent prefix=None + nested
//!      children via path_prefix) into the view's own output subdir.
//!   3. Resolve per-target team/user summary from the detail.db and upsert a
//!      daily history snapshot into the view's report.db.
//!   4. Remove the Phase 1 tmpdir after the LAST view of the root.
//!
//! Milestone 2 (this version): device groups run in PARALLEL (rayon::par_iter).
//! Within each group, physical roots are still sequential (safe for HDDs).
//! GIL is released across groups via py.allow_threads; re-acquired per PyO3 call
//! via Python::with_gil inside the parallel closure.
//!
//! Output layout per target (single-DB-merged):
//!   <output_dir>/<name>/report.db          (merged: detail + treemap + perm + history)
//!   <output_dir>/<name>/scan_status.json   (live status, atomic)
//!   <output_dir>/<name>/scan.log          (structured per-target log)

use pyo3::exceptions::PyRuntimeError;
use pyo3::prelude::*;
use pyo3::types::PyDict;
use serde::Deserialize;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use crate::db_writer;
use crate::report_history::{self, SnapshotMeta, UsageRow};
use crate::report_pipeline;
use crate::scan_core;

// ─── Plan deserialisation (mirror of Python plan_to_dict) ──────────────

#[derive(Debug, Deserialize)]
struct PlanJson {
    output_dir: String,
    groups: Vec<GroupJson>,
}

#[derive(Debug, Deserialize)]
struct GroupJson {
    #[allow(dead_code)]
    group_index: usize,
    #[allow(dead_code)]
    dev_class: String,
    workers: usize,
    #[allow(dead_code)]
    intra_parallel: usize,
    physical_roots: Vec<RootJson>,
}

#[derive(Debug, Deserialize)]
struct RootJson {
    scan_path: String,
    views: Vec<ViewJson>,
}

#[derive(Debug, Deserialize)]
struct ViewJson {
    name: String,
    prefix: Option<String>,
    view_path: String,
    team_map: HashMap<String, String>,
    #[serde(default)]
    teams: Vec<TeamJson>,
    #[serde(default)]
    users: Vec<UserJson>,
    output_subdir: String,
    #[serde(default)]
    end_scan: Option<String>,
    #[serde(default)]
    purge_time: Option<i64>,
}

#[derive(Debug, Deserialize, Clone)]
struct TeamJson {
    name: String,
    #[serde(default)]
    team_id: serde_json::Value,
}

#[derive(Debug, Deserialize, Clone)]
struct UserJson {
    name: String,
    #[serde(default)]
    team_id: serde_json::Value,
}

// ─── uid → username via libc getpwuid_r ────────────────────────────────

fn username_for_uid(uid: u32) -> String {
    use std::ffi::CStr;
    let mut buf = vec![0u8; 4096];
    let mut pwd: libc::passwd = unsafe { std::mem::zeroed() };
    let mut result: *mut libc::passwd = std::ptr::null_mut();
    let rc = unsafe {
        libc::getpwuid_r(
            uid as libc::uid_t,
            &mut pwd,
            buf.as_mut_ptr() as *mut libc::c_char,
            buf.len(),
            &mut result,
        )
    };
    if rc == 0 && !result.is_null() && !pwd.pw_name.is_null() {
        let cstr = unsafe { CStr::from_ptr(pwd.pw_name) };
        if let Ok(s) = cstr.to_str() {
            return s.to_string();
        }
    }
    format!("uid-{}", uid)
}

// ─── status.json (atomic write) ────────────────────────────────────────

fn write_status_atomic(subdir: &Path, phase: &str, message: &str, extra: &str) {
    let status_path = subdir.join("scan_status.json");
    let tmp_path = subdir.join(".scan_status.json.tmp");
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let json = format!(
        "{{\"phase\":\"{}\",\"message\":\"{}\",\"updated_at\":{}{}}}",
        phase, message, now, extra
    );
    if fs::write(&tmp_path, json).is_ok() {
        let _ = fs::rename(&tmp_path, &status_path);
    }
}

// ─── scan.log (append-only structured log) ─────────────────────────────

fn write_scan_log(subdir: &Path, phase: &str, message: &str) {
    use std::io::Write;
    let log_path = subdir.join("scan.log");
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
    {
        let _ = writeln!(f, "[{}] [{}] {}", now, phase, message);
    }
}

// ─── per-view summary from detail.db ────────────────────────────────────

/// Read the detail.db `users` table and bucket per-user/team sizes using the
/// view's own scoped team/user lists. Returns (teams, users) UsageRows.
fn summarise_from_detail(
    detail_db: &Path,
    view: &ViewJson,
) -> rusqlite::Result<(Vec<UsageRow>, Vec<UsageRow>)> {
    // team_id (string from config) per username; and the set of target users.
    let user_team: HashMap<String, Option<i64>> = view
        .users
        .iter()
        .map(|u| (u.name.clone(), json_to_i64(&u.team_id)))
        .collect();
    // team name -> team_id
    let team_ids: HashMap<String, Option<i64>> = view
        .teams
        .iter()
        .map(|t| (t.name.clone(), json_to_i64(&t.team_id)))
        .collect();
    // username -> team name (via team_map: name -> team_id string)
    // team_map maps username -> team_id string; resolve team name by matching.
    let conn = rusqlite::Connection::open(detail_db)?;
    let mut stmt = conn.prepare("SELECT username, total_size FROM users")?;
    let rows = stmt.query_map([], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
    })?;

    let mut user_rows: Vec<UsageRow> = Vec::new();
    let mut team_totals: HashMap<String, i64> = HashMap::new();

    for row in rows {
        let (username, size) = row?;
        if user_team.contains_key(&username) {
            let tid = user_team.get(&username).cloned().flatten();
            user_rows.push(UsageRow {
                name: username.clone(),
                team_id: tid,
                size,
                kind: "user".to_string(),
            });
            // map to team via team_map (username -> team_id string)
            if let Some(team_id_str) = view.team_map.get(&username) {
                // find team name whose team_id matches
                let team_name = team_ids
                    .iter()
                    .find(|(_, v)| {
                        v.map(|x| x.to_string()).as_deref() == Some(team_id_str.as_str())
                    })
                    .map(|(k, _)| k.clone())
                    .unwrap_or_else(|| team_id_str.clone());
                *team_totals.entry(team_name).or_insert(0) += size;
            }
        } else {
            user_rows.push(UsageRow {
                name: username,
                team_id: None,
                size,
                kind: "other".to_string(),
            });
        }
    }

    let team_rows: Vec<UsageRow> = team_totals
        .into_iter()
        .map(|(name, size)| {
            let tid = team_ids.get(&name).cloned().flatten();
            UsageRow {
                name,
                team_id: tid,
                size,
                kind: "team".to_string(),
            }
        })
        .collect();

    Ok((team_rows, user_rows))
}

fn json_to_i64(v: &serde_json::Value) -> Option<i64> {
    match v {
        serde_json::Value::Number(n) => n.as_i64(),
        serde_json::Value::String(s) => s.parse::<i64>().ok(),
        _ => None,
    }
}

// ─── main entry ─────────────────────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
pub fn run_scan_plan_impl(
    py: Python<'_>,
    plan_json: String,
    build_treemap: bool,
    max_level: usize,
    timestamp: i64,
    debug: bool,
) -> PyResult<String> {
    let plan: PlanJson = serde_json::from_str(&plan_json)
        .map_err(|e| PyRuntimeError::new_err(format!("parse plan json: {}", e)))?;

    // Device groups run in parallel — each group is on a distinct physical device,
    // so concurrent I/O is safe. Within each group, physical roots run sequentially
    // (HDD-friendly). The GIL is released across groups; re-acquired per PyO3 call
    // via Python::with_gil inside the rayon closure.
    let results: Vec<Result<Vec<String>, String>> = py.allow_threads(|| {
        use rayon::prelude::*;
        plan.groups.par_iter().map(|group| {
            let mut entries: Vec<String> = Vec::new();

            for root in &group.physical_roots {
                // ── Phase 1 + extraction in one with_gil block ──
                // PyObject is not Send, so we must not let it escape the GIL block.
                let scan_tmp_base = scan_tmp_base_for(&plan.output_dir);
                let (tmpdir, uids_map) = Python::with_gil(|py| {
                    let result = scan_core::run_scan_core(
                        py,
                        root.scan_path.clone(),
                        Vec::new(),   // skip_dirs
                        None,         // target_uids = None (superset)
                        Some(group.workers),
                        debug,
                        "production",
                        Some(scan_tmp_base.to_string_lossy().into_owned()),
                    )?;
                    extract_phase1(py, &result)
                }).map_err(|e: PyErr| {
                    let msg = format!("phase1 {}: {}", root.scan_path, e);
                    // Log to first view's subdir if accessible (best-effort)
                    if let Some(first_view) = root.views.first() {
                        let sd = PathBuf::from(&first_view.output_subdir);
                        write_scan_log(&sd, "error", &msg);
                    }
                    msg
                })?;

                // ── Phase 2 per view ──
                for view in &root.views {
                    let subdir = PathBuf::from(&view.output_subdir);
                    fs::create_dir_all(&subdir)
                        .map_err(|e| format!("mkdir {}: {}", subdir.display(), e))?;
                    write_status_atomic(&subdir, "detail", "Building detail DB", "");
                    write_scan_log(&subdir, "phase1",
                        &format!("Phase 1 complete: {} files from '{}'",
                            group.workers, root.scan_path));
                    write_scan_log(&subdir, "detail", "Building detail DB");

                    let detail_db = subdir.join("data_detail.db");
                    let treemap_db = subdir.join("tree_map_data").join("treemap.db");

                    let detail_start = std::time::Instant::now();
                    let phase2_result = Python::with_gil(|py| {
                        report_pipeline::build_detail_db_impl(
                            py,
                            tmpdir.clone(),
                            uids_map.clone(),
                            view.team_map.clone(),
                            detail_db.to_string_lossy().to_string(),
                            treemap_db.to_string_lossy().to_string(),
                            view.view_path.clone(),
                            max_level,
                            0,
                            timestamp,
                            group.workers,
                            build_treemap,
                            debug,
                            view.prefix.clone(),
                        )
                    });
                    let agg_path = match phase2_result {
                        Ok((total_files, agg_path)) => {
                            write_scan_log(
                                &subdir, "detail",
                                &format!("Detail DB built: {} files in {:.2}s",
                                    total_files, detail_start.elapsed().as_secs_f64()),
                            );
                            agg_path
                        }
                        Err(e) => {
                            let msg = format!("build_detail failed: {}", e);
                            write_scan_log(&subdir, "error", &msg);
                            return Err(msg);
                        }
                    };

                    // ── Phase 3: build treemap.db from aggregates (if requested) ──
                    if build_treemap {
                        if let Some(ref agg) = agg_path {
                            if agg.exists() {
                                let treemap_start = std::time::Instant::now();
                                let tm_result = report_pipeline::build_treemap_db_impl(
                                    agg,
                                    &treemap_db,
                                    &view.view_path,
                                    max_level,
                                    0,
                                    timestamp,
                                    debug,
                                );
                                match tm_result {
                                    Ok(()) => write_scan_log(
                                        &subdir, "treemap",
                                        &format!("Treemap DB built in {:.2}s",
                                            treemap_start.elapsed().as_secs_f64()),
                                    ),
                                    Err(e) => write_scan_log(
                                        &subdir, "treemap",
                                        &format!("Treemap build skipped (non-fatal): {}", e),
                                    ),
                                }
                            } else {
                                write_scan_log(&subdir, "treemap", "No aggregates to build treemap");
                            }
                        }
                    }

                    // ── History snapshot into report.db ──
                    let report_db = subdir.join("report.db");
                    let (teams, users) = summarise_from_detail(&detail_db, view)
                        .map_err(|e| {
                            let msg = format!("summarise {}: {}", view.name, e);
                            write_scan_log(&subdir, "error", &msg);
                            msg
                        })?;
                    write_scan_log(
                        &subdir, "history",
                        &format!("Team summaries: {} teams, {} users",
                            teams.len(), users.len()),
                    );
                    let meta = system_meta(&view.view_path);
                    let conn = rusqlite::Connection::open(&report_db)
                        .map_err(|e| {
                            let msg = format!("open report.db {}: {}", view.name, e);
                            write_scan_log(&subdir, "error", &msg);
                            msg
                        })?;
                    report_history::upsert_snapshot(&conn, timestamp, &meta, &teams, &users)
                        .map_err(|e| {
                            let msg = format!("history {}: {}", view.name, e);
                            write_scan_log(&subdir, "error", &msg);
                            msg
                        })?;

                    // Purge old history if purge_time is configured
                    if let Some(purge_days) = view.purge_time {
                        if purge_days > 0 {
                            let cutoff = timestamp - purge_days * 86400;
                            let cutoff_yyyymmdd = report_history::epoch_to_yyyymmdd(cutoff);
                            match report_history::purge_older_than(&conn, cutoff_yyyymmdd) {
                                Ok(n) => {
                                    if n > 0 {
                                        write_scan_log(&subdir, "history",
                                            &format!("Purged {} old snapshot(s) (>{} days)", n, purge_days));
                                    }
                                }
                                Err(e) => write_scan_log(&subdir, "history",
                                    &format!("Purge skipped: {}", e)),
                            }
                        }
                    }

                    drop(conn);
                    write_scan_log(&subdir, "history", "History snapshot written");

                    // ── Merge all 4 DBs into a single report.db ──
                    let merged_db = subdir.join("report.db");
                    match db_writer::merge_into_single_db(
                        &subdir, &merged_db, &view.name, &view.view_path, timestamp,
                    ) {
                        Ok(()) => {
                            write_scan_log(&subdir, "merge", "Merged into single report.db");
                            // Remove source DBs + work dirs after successful merge
                            let _ = std::fs::remove_file(&subdir.join("data_detail.db"));
                            let _ = std::fs::remove_dir_all(&subdir.join("tree_map_data"));
                            let _ = std::fs::remove_file(&subdir.join("permission_issues.db"));
                            // Clean up leftover treemap build work dir at subdir level
                            let build_prefix = format!(".tree_map_data_build_{}", std::process::id());
                            let legacy_work = subdir.join(&build_prefix);
                            let _ = std::fs::remove_dir_all(&legacy_work);
                        }
                        Err(e) => {
                            write_scan_log(&subdir, "merge",
                                &format!("Merge skipped (non-fatal): {}", e));
                            // Non-fatal: keep all 4 source files as fallback
                        }
                    }

                    write_status_atomic(&subdir, "done", "Scan complete", "");
                    write_scan_log(&subdir, "done", "Scan complete");
                    let kind = if view.prefix.is_none() { "physical_root" } else { "derived" };
                    entries.push(format!(
                        "{{\"name\":\"{}\",\"path\":\"{}\",\"kind\":\"{}\",\"status\":\"scanned\"}}",
                        view.name, view.view_path, kind
                    ));
                }

                // ── Cleanup Phase 1 tmpdir after last view ──
                if !tmpdir.is_empty() {
                    let _ = fs::remove_dir_all(&tmpdir);
                }
            }

            Ok(entries)
        }).collect()
    });

    // Merge results; collect any per-group errors but don't abort early.
    let mut manifest_entries: Vec<String> = Vec::new();
    let mut errors: Vec<String> = Vec::new();
    for r in results {
        match r {
            Ok(entries) => manifest_entries.extend(entries),
            Err(e) => errors.push(e),
        }
    }

    if !errors.is_empty() && manifest_entries.is_empty() {
        return Err(PyRuntimeError::new_err(errors.join("; ")));
    }

    let error_json = if errors.is_empty() {
        String::new()
    } else {
        format!(
            ",\"errors\":[{}]",
            errors
                .iter()
                .map(|e| format!("\"{}\"", e.replace('"', "\\\"")))
                .collect::<Vec<_>>()
                .join(",")
        )
    };

    Ok(format!(
        "{{\"targets\":[{}]{}}}",
        manifest_entries.join(","),
        error_json
    ))
}

fn scan_tmp_base_for(output_dir: &str) -> PathBuf {
    PathBuf::from(output_dir).join(".scan_tmp")
}

fn system_meta(path: &str) -> SnapshotMeta {
    // statvfs for total/used/available
    use std::ffi::CString;
    let mut meta = SnapshotMeta {
        path: path.to_string(),
        ..Default::default()
    };
    if let Ok(c) = CString::new(path) {
        let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
        if unsafe { libc::statvfs(c.as_ptr(), &mut st) } == 0 {
            let bsize = st.f_frsize as i64;
            let total = st.f_blocks as i64 * bsize;
            let avail = st.f_bavail as i64 * bsize;
            meta.total = total;
            meta.available = avail;
            meta.used = total - (st.f_bfree as i64 * bsize);
        }
    }
    meta
}

/// Extract (tmpdir, uids_map) from the Phase 1 result dict, resolving every
/// uid (file owners + dir owners) to a username.
fn extract_phase1(py: Python<'_>, result: &PyObject) -> PyResult<(String, HashMap<u32, String>)> {
    let dict: &PyDict = result.downcast::<PyDict>(py).map_err(|_| {
        PyRuntimeError::new_err("phase1 result is not a dict")
    })?;
    let tmpdir: String = dict
        .get_item("detail_tmpdir")?
        .and_then(|v| v.extract().ok())
        .unwrap_or_default();

    let mut uids_map: HashMap<u32, String> = HashMap::new();

    if let Some(uid_sizes) = dict.get_item("uid_sizes")? {
        if let Ok(d) = uid_sizes.downcast::<PyDict>() {
            for (k, _v) in d.iter() {
                if let Ok(uid) = k.extract::<u32>() {
                    uids_map.entry(uid).or_insert_with(|| username_for_uid(uid));
                }
            }
        }
    }
    if let Some(owners) = dict.get_item("dir_owner_uids")? {
        if let Ok(list) = owners.extract::<Vec<u32>>() {
            for uid in list {
                uids_map.entry(uid).or_insert_with(|| username_for_uid(uid));
            }
        }
    }

    Ok((tmpdir, uids_map))
}
