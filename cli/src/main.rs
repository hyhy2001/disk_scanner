mod config;
mod scheduler;
mod ui;

use clap::Parser;
use config::Config;
use scheduler::build_scan_plan;

#[derive(Parser)]
#[command(name = "duscan", version = "0.1.0")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(clap::Subcommand)]
enum Command {
    Run {
        #[arg(long)]
        output_dir: Option<String>,
        #[arg(long)]
        tree_map: bool,
        #[arg(long)]
        workers: Option<usize>,
        #[arg(long, default_value = "3")]
        level: usize,
        #[arg(long)]
        target: Vec<String>,
    },
    List {
        #[arg(long)]
        target: Option<String>,
        #[arg(long)]
        team: Option<String>,
        #[arg(long)]
        json: bool,
    },
    AddTarget {
        name: String,
        path: String,
        #[arg(long)]
        end_scan: Option<String>,
        #[arg(long)]
        purge_time: Option<i64>,
    },
    /// Create or update a target with its teams and users in one shot.
    /// Repeat --team NAME=user1,user2 per team (empty user list allowed: --team NAME).
    SetTarget {
        name: String,
        path: String,
        /// team spec: "teamname=user1,user2" (repeatable)
        #[arg(long = "team")]
        teams: Vec<String>,
        #[arg(long)]
        end_scan: Option<String>,
        #[arg(long)]
        purge_time: Option<i64>,
        /// Merge into existing teams/users instead of replacing them.
        #[arg(long)]
        merge: bool,
    },
    RemoveTarget { name: String },
    AddTeam { name: String, #[arg(long)] target: String },
    AddUser { users: Vec<String>, #[arg(long)] team: String, #[arg(long)] target: String },
    RemoveUser { users: Vec<String>, #[arg(long)] target: String },
    Detail { #[arg(long)] user: Vec<String>, #[arg(long)] output_dir: Option<String>, #[arg(long, default_value = "30")] top: usize, #[arg(long)] target: Option<String>, #[arg(long)] json: bool },
    /// Show directory tree from treemap data
    TreeShow {
        #[arg(long)]
        output_dir: Option<String>,
        #[arg(long, default_value = "3")]
        level: usize,
        #[arg(long, default_value = "20")]
        limit: usize,
        #[arg(long)]
        path: Option<String>,
        #[arg(long)]
        search: Option<String>,
        #[arg(long)]
        target: Option<String>,
    },
    /// Export full per-user dir/file usage to text files (usage_dir_*/usage_file_*).
    Export {
        #[arg(long)]
        user: Vec<String>,
        #[arg(long)]
        output_dir: Option<String>,
        #[arg(long)]
        export_dir: Option<String>,
        #[arg(long)]
        target: Option<String>,
    },
    /// Send a disk-usage summary card to an MS Teams workflow webhook.
    Notify {
        #[arg(long)]
        webhook_url: String,
        #[arg(long)]
        output_dir: Option<String>,
        #[arg(long)]
        target: Option<String>,
    },
    /// rsync report output to a remote host (shells out to rsync over SSH).
    Sync {
        #[arg(long)]
        output_dir: Option<String>,
        #[arg(long)]
        host: String,
        #[arg(long)]
        dest_dir: String,
        #[arg(long)]
        user: Option<String>,
    },
    /// Show per-day usage history from report.db (hist_* tables).
    History {
        #[arg(long)]
        output_dir: Option<String>,
        #[arg(long)]
        target: Option<String>,
        #[arg(long, default_value = "14")]
        days: i64,
        #[arg(long)]
        json: bool,
    },
    /// Import a legacy JSON config tree into duscan.toml.
    ImportLegacy {
        /// Directory holding legacy configs (configs/<target>/config.json).
        #[arg(long)]
        dir: String,
        /// Overwrite an existing duscan.toml if present.
        #[arg(long)]
        force: bool,
    },
    /// Apply a declarative targets file (.toml/.json) to the config.
    /// By default the file is the source of truth (targets not listed are removed);
    /// use --merge to only add/update. --dry-run prints the diff without writing.
    Apply {
        /// Path to a declarative targets file (.toml or .json).
        file: String,
        #[arg(long)]
        dry_run: bool,
        #[arg(long)]
        merge: bool,
    },
}

/// Declarative file schema for `apply`. Ergonomic shape: teams carry their
/// member usernames directly (team_ids are assigned internally).
#[derive(serde::Deserialize)]
struct ApplyFile {
    #[serde(default)]
    targets: Vec<ApplyTarget>,
}

#[derive(serde::Deserialize)]
struct ApplyTarget {
    name: String,
    path: String,
    #[serde(default)]
    teams: Vec<ApplyTeam>,
    #[serde(default)]
    end_scan: Option<String>,
    #[serde(default)]
    purge_time: Option<i64>,
}

#[derive(serde::Deserialize)]
struct ApplyTeam {
    name: String,
    #[serde(default)]
    users: Vec<String>,
}

fn fmt_size(sz: i64) -> String {
    if sz >= 1_000_000_000 { format!("{:.1} GB", sz as f64 / 1e9) }
    else if sz >= 1_000_000 { format!("{:.1} MB", sz as f64 / 1e6) }
    else if sz >= 1_000 { format!("{:.1} KB", sz as f64 / 1e3) }
    else { format!("{} B", sz) }
}

fn username_for_uid(uid: u32) -> String {
    unsafe {
        let mut buf = vec![0u8; 4096];
        let mut pwd: libc::passwd = std::mem::zeroed();
        let mut result: *mut libc::passwd = std::ptr::null_mut();
        let rc = libc::getpwuid_r(uid, &mut pwd, buf.as_mut_ptr() as *mut i8, buf.len(), &mut result);
        if rc == 0 && !result.is_null() && !pwd.pw_name.is_null() {
            let cstr = std::ffi::CStr::from_ptr(pwd.pw_name);
            if let Ok(s) = cstr.to_str() {
                return s.to_string();
            }
        }
        format!("uid-{}", uid)
    }
}

/// statvfs(path) → (total, used, available) bytes of the filesystem holding `path`.
fn statvfs_meta(path: &str) -> (i64, i64, i64) {
    use std::ffi::CString;
    let c_path = match CString::new(path) {
        Ok(p) => p,
        Err(_) => return (0, 0, 0),
    };
    unsafe {
        let mut s: libc::statvfs = std::mem::zeroed();
        if libc::statvfs(c_path.as_ptr(), &mut s) != 0 {
            return (0, 0, 0);
        }
        let bsize = s.f_frsize as i64;
        let total = s.f_blocks as i64 * bsize;
        let available = s.f_bavail as i64 * bsize;
        let used = total - s.f_bfree as i64 * bsize;
        (total, used, available)
    }
}

/// Atomically write `<target_dir>/scan_status.json` so a dashboard can poll
/// scan progress. Mirrors the legacy Python heartbeat fields.
fn write_scan_status(
    target_dir: &std::path::Path,
    stage: &str,
    running: bool,
    started_at: i64,
    phase_started_at: i64,
    message: &str,
    error: &str,
    tree_map_enabled: bool,
) {
    use std::io::Write;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let payload = serde_json::json!({
        "running": running,
        "stage": stage,
        "started_at": started_at,
        "phase_started_at": phase_started_at,
        "phase_elapsed_sec": (now - phase_started_at).max(0),
        "total_elapsed_sec": (now - started_at).max(0),
        "updated_at": now,
        "finished_at": if running { 0 } else { now },
        "pid": std::process::id(),
        "message": message,
        "error": error,
        "tree_map_enabled": tree_map_enabled,
        "sync_enabled": false,
    });
    if std::fs::create_dir_all(target_dir).is_err() {
        return;
    }
    let path = target_dir.join("scan_status.json");
    let tmp = path.with_extension("json.tmp");
    if let Ok(mut f) = std::fs::File::create(&tmp) {
        if f.write_all(payload.to_string().as_bytes()).is_ok() {
            let _ = std::fs::rename(&tmp, &path);
        }
    }
}

/// Aggregate per-user/per-team usage from the merged report.db and upsert one
/// per-day history snapshot. `team_map` maps username → team_id, `team_names`
/// maps team_id → team name (both from config).
fn write_history_snapshot(
    db_path: &std::path::Path,
    scan_path: &str,
    team_map: &std::collections::HashMap<String, i64>,
    team_names: &std::collections::HashMap<i64, String>,
    timestamp: i64,
    purge_days: Option<i64>,
) -> Result<(), String> {
    use check_disk_core::report_history::{self, SnapshotMeta, UsageRow};

    let conn = rusqlite::Connection::open(db_path).map_err(|e| e.to_string())?;

    // Per-user rows from detail_users. A user listed in the config (present in
    // team_map) is a tracked "user"; everyone else is "other".
    let mut users: Vec<UsageRow> = Vec::new();
    let mut team_sizes: std::collections::HashMap<i64, i64> = std::collections::HashMap::new();
    {
        let mut stmt = conn
            .prepare("SELECT username, total_size FROM detail_users")
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))
            .map_err(|e| e.to_string())?;
        for row in rows.flatten() {
            let (name, size) = row;
            let team_id = team_map.get(&name).copied();
            if let Some(tid) = team_id {
                *team_sizes.entry(tid).or_insert(0) += size;
            }
            users.push(UsageRow {
                name,
                team_id,
                size,
                kind: if team_id.is_some() { "user".into() } else { "other".into() },
            });
        }
    }

    let teams: Vec<UsageRow> = team_sizes
        .into_iter()
        .map(|(tid, size)| UsageRow {
            name: team_names.get(&tid).cloned().unwrap_or_default(),
            team_id: Some(tid),
            size,
            kind: "team".into(),
        })
        .collect();

    let (total, used, available) = statvfs_meta(scan_path);
    let meta = SnapshotMeta { path: scan_path.to_string(), total, used, available };

    report_history::upsert_snapshot(&conn, timestamp, &meta, &teams, &users)
        .map_err(|e| e.to_string())?;

    if let Some(days) = purge_days {
        if days > 0 {
            let cutoff = report_history::epoch_to_yyyymmdd(timestamp - days * 86400);
            let _ = report_history::purge_older_than(&conn, cutoff);
        }
    }
    Ok(())
}

fn main() {
    let cli = Cli::parse();
    let mut cfg = Config::load();

    match &cli.command {
        Command::AddTarget { name, path, end_scan, purge_time } => {
            match cfg.add_target(name, path, end_scan.clone(), *purge_time) {
                Ok(()) => println!("Added target '{}' -> {}", name, path),
                Err(e) => eprintln!("Error: {}", e),
            }
        }
        Command::SetTarget { name, path, teams, end_scan, purge_time, merge } => {
            match parse_team_specs(teams) {
                Ok(team_specs) => {
                    let existed = cfg.find_target(name).is_some();
                    let spec = config::TargetSpec {
                        name: name.clone(),
                        path: path.clone(),
                        teams: team_specs,
                        end_scan: end_scan.clone(),
                        purge_time: *purge_time,
                    };
                    cfg.upsert_target_full(&spec, *merge);
                    match cfg.save() {
                        Ok(()) => {
                            let verb = if existed { "Updated" } else { "Created" };
                            let nusers: usize = spec.teams.iter().map(|t| t.users.len()).sum();
                            println!("{} target '{}' -> {} ({} teams, {} users, {})",
                                verb, name, path, spec.teams.len(), nusers,
                                if *merge { "merge" } else { "replace" });
                        }
                        Err(e) => eprintln!("Error: {}", e),
                    }
                }
                Err(e) => eprintln!("Error: {}", e),
            }
        }
        Command::RemoveTarget { name } => {
            match cfg.remove_target(name) {
                Ok(()) => println!("Removed target '{}'", name),
                Err(e) => eprintln!("Error: {}", e),
            }
        }
        Command::AddTeam { name, target } => {
            match cfg.add_team(name, target) {
                Ok(()) => println!("Team '{}' added to target '{}'", name, target),
                Err(e) => eprintln!("Error: {}", e),
            }
        }
        Command::AddUser { users, team, target } => {
            for u in users {
                match cfg.add_user(u, team, target) {
                    Ok(()) => println!("User '{}' added to team '{}' (target: {})", u, team, target),
                    Err(e) => eprintln!("Error: {}", e),
                }
            }
        }
        Command::RemoveUser { users, target } => {
            for u in users {
                match cfg.remove_user(u, target) {
                    Ok(()) => println!("User '{}' removed from target '{}'", u, target),
                    Err(e) => eprintln!("Error: {}", e),
                }
            }
        }
        Command::List { target, team, json } => {
            if *json {
                let arr: Vec<serde_json::Value> = cfg.targets.iter()
                    .filter(|t| target.as_deref().map(|w| t.name == w).unwrap_or(true))
                    .map(|t| serde_json::json!({
                        "name": t.name,
                        "path": t.path,
                        "end_scan": t.end_scan,
                        "purge_time": t.purge_time,
                        "teams": t.teams.iter().map(|tm| serde_json::json!({"name": tm.name, "team_id": tm.team_id})).collect::<Vec<_>>(),
                        "users": t.users.iter()
                            .filter(|u| team.as_ref().map(|tn| t.teams.iter().any(|tm| tm.name == *tn && tm.team_id == u.team_id)).unwrap_or(true))
                            .map(|u| serde_json::json!({"name": u.name, "team_id": u.team_id})).collect::<Vec<_>>(),
                    }))
                    .collect();
                println!("{}", serde_json::to_string_pretty(&serde_json::json!(arr)).unwrap_or_else(|_| "[]".into()));
            } else if let Some(tname) = target {
                match cfg.find_target(tname) {
                    Some(t) => {
                        println!("\n=== Target: {} ({}) ===", t.name, t.path);
                        println!("Teams: {}", t.teams.len());
                        println!("Users: {}", t.users.len());
                        if let Some(ref es) = t.end_scan {
                            println!("End scan: {}", es);
                        }
                        if let Some(pt) = t.purge_time {
                            println!("Max store: {} days", pt);
                        }
                        if !t.teams.is_empty() {
                            println!("\nTeams:");
                            for tm in &t.teams {
                                // Filter to a single team when --team is given.
                                if let Some(tn) = team {
                                    if &tm.name != tn { continue; }
                                }
                                let members: Vec<&str> = t.users.iter()
                                    .filter(|u| u.team_id == tm.team_id)
                                    .map(|u| u.name.as_str())
                                    .collect();
                                println!("  {} ({}): {}", tm.name, members.len(), members.join(", "));
                            }
                        }
                    }
                    None => println!("Target '{}' not found.", tname),
                }
            } else {
                println!("{:<20} {:<40} {:<6} {:<6}", "Target", "Path", "Teams", "Users");
                println!("{}", "-".repeat(80));
                for t in &cfg.targets {
                    println!("{:<20} {:<40} {:<6} {:<6}",
                        t.name, t.path, t.teams.len(), t.users.len());
                }
            }
        }
        Command::Run { output_dir, tree_map, workers, level, target } => {
            run_scan(&mut cfg, output_dir.clone(), *tree_map, *workers, *level, target);
        }
        Command::Detail { user, output_dir, top, target, json } => {
            let out = output_dir.clone().unwrap_or_else(|| cfg.output_dir.clone());
            let mut json_out: Vec<serde_json::Value> = Vec::new();
            for username in user {
                let mut found = false;
                if let Ok(entries) = std::fs::read_dir(&out) {
                    for entry in entries.flatten() {
                        let path = entry.path();
                        if !path.is_dir() { continue; }
                        if let Some(want) = target {
                            if path.file_name().map(|n| n.to_string_lossy() != *want).unwrap_or(true) { continue; }
                        }
                        let db_path = path.join("report.db");
                        if !db_path.exists() { continue; }
                        let Ok(conn) = rusqlite::Connection::open(&db_path) else { continue };
                        let prefix = detail_prefix(&conn);
                        let tname = path.file_name().unwrap_or_default().to_string_lossy().to_string();

                        let sql = format!(
                            "SELECT uid, total_files, total_dirs, total_size FROM {}users WHERE username = ?1", prefix
                        );
                        let Ok(mut stmt) = conn.prepare(&sql) else { continue };
                        let Ok(rows) = stmt.query_map([&username], |r| {
                            Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?, r.get::<_, i64>(2)?, r.get::<_, i64>(3)?))
                        }) else { continue };
                        for row in rows.flatten() {
                            let (uid, total_files, total_dirs, total_size) = row;
                            found = true;
                            let top_dirs = query_top(&conn, &format!(
                                "SELECT d.size, d.path FROM {}dirs d WHERE d.uid = ?1 ORDER BY d.size DESC LIMIT ?2", prefix), uid, *top);
                            let top_files = query_top(&conn, &format!(
                                "SELECT f.size, n.name FROM {}files f JOIN {}file_names n ON f.name_id = n.id \
                                 WHERE f.uid = ?1 ORDER BY f.size DESC LIMIT ?2", prefix, prefix), uid, *top);

                            if *json {
                                json_out.push(serde_json::json!({
                                    "user": username, "target": tname, "uid": uid,
                                    "total_files": total_files, "total_dirs": total_dirs, "total_size": total_size,
                                    "top_dirs": top_dirs.iter().map(|(s, p)| serde_json::json!({"size": s, "path": p})).collect::<Vec<_>>(),
                                    "top_files": top_files.iter().map(|(s, p)| serde_json::json!({"size": s, "name": p})).collect::<Vec<_>>(),
                                }));
                            } else {
                                println!("\n=== root on {} ===", tname);
                                println!("  Files: {}  Dirs: {}  Size: {}", total_files, total_dirs, fmt_size(total_size));
                                println!("\n  {}  Top Directories", "─".repeat(40));
                                for (s, p) in &top_dirs { println!("    {:>10}  {}", fmt_size(*s), p); }
                                println!("\n  {}  Top Files", "─".repeat(40));
                                for (s, p) in &top_files { println!("    {:>10}  {}", fmt_size(*s), p); }
                            }
                        }
                    }
                }
                if !found && !*json {
                    println!("User '{}' not found.", username);
                }
            }
            if *json {
                println!("{}", serde_json::to_string_pretty(&serde_json::json!(json_out)).unwrap_or_else(|_| "[]".into()));
            }
        }
        Command::TreeShow { output_dir, level, limit, path, search, target } => {
            let out = output_dir.clone().unwrap_or_else(|| cfg.output_dir.clone());
            if let Ok(entries) = std::fs::read_dir(&out) {
                for entry in entries.flatten() {
                    let dir_path = entry.path();
                    if !dir_path.is_dir() { continue; }
                    if let Some(want) = target {
                        if dir_path.file_name().map(|n| n.to_string_lossy() != *want).unwrap_or(true) { continue; }
                    }
                    let db_path = dir_path.join("report.db");
                    if !db_path.exists() { continue; }
                    if let Ok(conn) = rusqlite::Connection::open(&db_path) {
                        let tp = if conn.prepare("SELECT 1 FROM treemap_dirs LIMIT 1").is_ok()
                            { "treemap_" } else { "" };

                        // Check if treemap data exists
                        let has_tm = conn.query_row(
                            &format!("SELECT COUNT(*) FROM {}dirs", tp), [], |r| r.get::<_, i64>(0),
                        ).unwrap_or(0) > 0;
                        if !has_tm { continue; }

                        let tname = dir_path.file_name().unwrap_or_default().to_string_lossy();

                        // Get root dir
                        let root_id: Option<i64> = conn.query_row(
                            &format!("SELECT id FROM {}dirs WHERE parent_id IS NULL LIMIT 1", tp),
                            [], |r| r.get(0),
                        ).ok();

                        if let Some(root) = root_id {
                            println!("\n=== Tree: {} ===\n", tname);
                            print_tree(&conn, &tp, root, 0, *level, *limit, "", path.as_deref(), search.as_deref());
                        } else {
                            eprintln!("No treemap root found for '{}'", tname);
                        }
                    }
                }
            }
        }
        Command::Export { user, output_dir, export_dir, target } => {
            let out = output_dir.clone().unwrap_or_else(|| cfg.output_dir.clone());
            let exp = export_dir.clone().unwrap_or_else(|| "exports".into());
            std::fs::create_dir_all(&exp).ok();
            let want: std::collections::HashSet<&str> = user.iter().map(|s| s.as_str()).collect();

            if let Ok(entries) = std::fs::read_dir(&out) {
                for entry in entries.flatten() {
                    let tname = entry.file_name().to_string_lossy().to_string();
                    if let Some(t) = target {
                        if &tname != t { continue; }
                    }
                    let db_path = entry.path().join("report.db");
                    if !db_path.exists() { continue; }
                    let Ok(conn) = rusqlite::Connection::open(&db_path) else { continue };

                    // username → uid map for the users we want (all if none specified)
                    let mut stmt = match conn.prepare("SELECT uid, username FROM detail_users") {
                        Ok(s) => s,
                        Err(_) => continue,
                    };
                    let users: Vec<(i64, String)> = stmt
                        .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))
                        .map(|rows| rows.flatten().collect())
                        .unwrap_or_default();

                    let tgt_dir = std::path::Path::new(&exp).join(&tname);
                    std::fs::create_dir_all(&tgt_dir).ok();
                    for (uid, uname) in &users {
                        if !want.is_empty() && !want.contains(uname.as_str()) { continue; }
                        export_user_text(&conn, *uid, uname, &tgt_dir);
                    }
                }
            }
        }
        Command::Notify { webhook_url, output_dir, target } => {
            let out = output_dir.clone().unwrap_or_else(|| cfg.output_dir.clone());
            let mut sent = 0;
            if let Ok(entries) = std::fs::read_dir(&out) {
                for entry in entries.flatten() {
                    let tname = entry.file_name().to_string_lossy().to_string();
                    if let Some(want) = target {
                        if &tname != want { continue; }
                    }
                    let db_path = entry.path().join("report.db");
                    if !db_path.exists() { continue; }
                    match send_teams_notification(webhook_url, &db_path, &tname) {
                        Ok(()) => { println!("Notified for target '{}'", tname); sent += 1; }
                        Err(e) => eprintln!("Notify '{}' failed: {}", tname, e),
                    }
                }
            }
            if sent == 0 { eprintln!("No report.db found to notify under '{}'", out); }
        }
        Command::Sync { output_dir, host, dest_dir, user } => {
            let out = output_dir.clone().unwrap_or_else(|| cfg.output_dir.clone());
            let remote = match user {
                Some(u) => format!("{}@{}:{}", u, host, dest_dir),
                None => format!("{}:{}", host, dest_dir),
            };
            let src = format!("{}/", out.trim_end_matches('/'));
            let status = std::process::Command::new("rsync")
                .args(["-az", "--delete", "-e", "ssh -o BatchMode=yes", &src, &remote])
                .status();
            match status {
                Ok(s) if s.success() => println!("Synced '{}' -> {}", out, remote),
                Ok(s) => eprintln!("rsync exited with {}", s),
                Err(e) => eprintln!("rsync failed to start: {} (is rsync installed?)", e),
            }
        }
        Command::History { output_dir, target, days, json } => {
            let out = output_dir.clone().unwrap_or_else(|| cfg.output_dir.clone());
            show_history(&out, target.as_deref(), *days, *json);
        }
        Command::ImportLegacy { dir, force } => {
            match import_legacy_config(dir, *force) {
                Ok(path) => println!("Imported legacy config -> {}", path.display()),
                Err(e) => eprintln!("Import failed: {}", e),
            }
        }
        Command::Apply { file, dry_run, merge } => {
            if let Err(e) = apply_targets_file(&mut cfg, file, *dry_run, *merge) {
                eprintln!("Apply failed: {}", e);
            }
        }
    }
}

/// Apply a declarative targets file to the config. Replace semantics by default
/// (targets absent from the file are removed); `merge` only adds/updates.
/// `dry_run` prints the diff and does not write.
fn apply_targets_file(cfg: &mut Config, file: &str, dry_run: bool, merge: bool) -> Result<(), String> {
    let text = std::fs::read_to_string(file).map_err(|e| format!("read {}: {}", file, e))?;
    let parsed: ApplyFile = if file.ends_with(".json") {
        serde_json::from_str(&text).map_err(|e| format!("parse JSON: {}", e))?
    } else {
        toml::from_str(&text).map_err(|e| format!("parse TOML: {}", e))?
    };
    if parsed.targets.is_empty() {
        return Err("file declares no targets".into());
    }

    // Compute the diff for reporting.
    let before: std::collections::HashSet<String> =
        cfg.targets.iter().map(|t| t.name.clone()).collect();
    let declared: std::collections::HashSet<String> =
        parsed.targets.iter().map(|t| t.name.clone()).collect();

    let added: Vec<&String> = declared.iter().filter(|n| !before.contains(*n)).collect();
    let updated: Vec<&String> = declared.iter().filter(|n| before.contains(*n)).collect();
    let removed: Vec<&String> = if merge {
        Vec::new()
    } else {
        before.iter().filter(|n| !declared.contains(*n)).collect()
    };

    println!("Apply {} ({} mode):", file, if merge { "merge" } else { "replace" });
    for n in &added { println!("  + add    {}", n); }
    for n in &updated { println!("  ~ update {}", n); }
    for n in &removed { println!("  - remove {}", n); }
    if added.is_empty() && updated.is_empty() && removed.is_empty() {
        println!("  (no changes)");
    }

    if dry_run {
        println!("(dry-run: config not written)");
        return Ok(());
    }

    // Replace mode: drop targets not present in the file.
    if !merge {
        cfg.targets.retain(|t| declared.contains(&t.name));
    }
    // Upsert every declared target (single save at the end).
    for at in &parsed.targets {
        let spec = config::TargetSpec {
            name: at.name.clone(),
            path: at.path.clone(),
            teams: at.teams.iter().map(|tm| config::TeamSpec {
                name: tm.name.clone(),
                users: tm.users.clone(),
            }).collect(),
            end_scan: at.end_scan.clone(),
            purge_time: at.purge_time,
        };
        cfg.upsert_target_full(&spec, merge);
    }
    cfg.save()?;
    println!("Applied: {} target(s) now configured.", cfg.targets.len());
    Ok(())
}

/// Print per-day usage history from each target's report.db (hist_* tables).
/// `days` limits how many recent snapshots to show; `json` emits machine output.
fn show_history(out: &str, target: Option<&str>, days: i64, json: bool) {
    let Ok(entries) = std::fs::read_dir(out) else {
        eprintln!("Cannot read output dir '{}'", out);
        return;
    };
    let mut any = false;
    let mut json_targets: Vec<serde_json::Value> = Vec::new();

    for entry in entries.flatten() {
        let tname = entry.file_name().to_string_lossy().to_string();
        if let Some(want) = target {
            if tname != want { continue; }
        }
        let db_path = entry.path().join("report.db");
        if !db_path.exists() { continue; }
        let Ok(conn) = rusqlite::Connection::open(&db_path) else { continue };

        // Snapshots newest-first, capped at `days`.
        let snaps: Vec<(i64, i64, String, i64, i64, i64)> = {
            let Ok(mut stmt) = conn.prepare(
                "SELECT id, scan_date, path, total, used, available \
                 FROM hist_snapshots ORDER BY scan_date DESC LIMIT ?1",
            ) else { continue };
            stmt.query_map(rusqlite::params![days.max(1)], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?, r.get::<_, String>(2)?,
                    r.get::<_, i64>(3)?, r.get::<_, i64>(4)?, r.get::<_, i64>(5)?))
            }).map(|rows| rows.flatten().collect()).unwrap_or_default()
        };
        if snaps.is_empty() { continue; }
        any = true;

        if json {
            let days_json: Vec<serde_json::Value> = snaps.iter().map(|(id, date, path, total, used, avail)| {
                let top = top_users_for_snapshot(&conn, *id);
                serde_json::json!({
                    "scan_date": date, "path": path,
                    "total": total, "used": used, "available": avail,
                    "top_users": top.iter().map(|(n, s)| serde_json::json!({"name": n, "size": s})).collect::<Vec<_>>(),
                })
            }).collect();
            json_targets.push(serde_json::json!({ "target": tname, "history": days_json }));
        } else {
            println!("\n=== History: {} ===", tname);
            for (id, date, _path, total, used, avail) in &snaps {
                let pct = if *total > 0 { *used as f64 / *total as f64 * 100.0 } else { 0.0 };
                println!("  {}  used {} / {} ({:.1}%)  free {}",
                    fmt_date(*date), fmt_size(*used), fmt_size(*total), pct, fmt_size(*avail));
                for (name, size) in top_users_for_snapshot(&conn, *id).into_iter().take(5) {
                    println!("      {:<16} {}", name, fmt_size(size));
                }
            }
        }
    }

    if json {
        println!("{}", serde_json::to_string_pretty(&serde_json::json!(json_targets)).unwrap_or_else(|_| "[]".into()));
    } else if !any {
        println!("No history found under '{}'.", out);
    }
}

/// Top users (kind='user') for a snapshot, largest first.
fn top_users_for_snapshot(conn: &rusqlite::Connection, snap_id: i64) -> Vec<(String, i64)> {
    conn.prepare(
        "SELECT name, size FROM hist_user_usage \
         WHERE snapshot_id = ?1 AND kind = 'user' ORDER BY size DESC LIMIT 10",
    )
    .and_then(|mut s| {
        s.query_map(rusqlite::params![snap_id], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))
            .map(|rows| rows.flatten().collect())
    })
    .unwrap_or_default()
}

/// Format a yyyymmdd integer as YYYY-MM-DD.
fn fmt_date(d: i64) -> String {
    format!("{:04}-{:02}-{:02}", d / 10000, (d / 100) % 100, d % 100)
}

/// Detect the detail-table prefix in a report.db: the merged DB prefixes detail
/// tables with `detail_`, while a raw data_detail.db uses bare names.
fn detail_prefix(conn: &rusqlite::Connection) -> &'static str {
    if conn.prepare("SELECT 1 FROM detail_users LIMIT 1").is_ok() {
        "detail_"
    } else {
        ""
    }
}

/// Run a `SELECT size, text WHERE uid=?1 ... LIMIT ?2` query and collect rows.
fn query_top(conn: &rusqlite::Connection, sql: &str, uid: i64, top: usize) -> Vec<(i64, String)> {
    conn.prepare(sql)
        .and_then(|mut s| {
            s.query_map(rusqlite::params![uid, top as i64], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
            })
            .map(|rows| rows.flatten().collect())
        })
        .unwrap_or_default()
}

/// Import a legacy JSON config tree (`<dir>/<target>/config.json`) into a fresh
/// `duscan.toml`. Each legacy per-target JSON already matches the new
/// `Target` shape (name/path/teams/users/end_scan/purge_time), so we just
/// deserialize each one and collect them. Returns the written config path.
/// Parse repeated `--team NAME=user1,user2` args into TeamSpecs. `NAME` alone
/// (no `=`) is a team with no users. Empty user tokens are skipped.
fn parse_team_specs(raw: &[String]) -> Result<Vec<config::TeamSpec>, String> {
    let mut specs: Vec<config::TeamSpec> = Vec::new();
    for item in raw {
        let (name, users_str) = match item.split_once('=') {
            Some((n, u)) => (n.trim(), u),
            None => (item.trim(), ""),
        };
        if name.is_empty() {
            return Err(format!("invalid --team '{}': empty team name", item));
        }
        if specs.iter().any(|s: &config::TeamSpec| s.name == name) {
            return Err(format!("duplicate --team '{}'", name));
        }
        let users: Vec<String> = users_str
            .split(',')
            .map(|u| u.trim())
            .filter(|u| !u.is_empty())
            .map(|u| u.to_string())
            .collect();
        specs.push(config::TeamSpec { name: name.to_string(), users });
    }
    Ok(specs)
}

fn import_legacy_config(dir: &str, force: bool) -> Result<std::path::PathBuf, String> {
    let root = std::path::Path::new(dir);
    if !root.is_dir() {
        return Err(format!("'{}' is not a directory", dir));
    }
    let dest = Config::path();
    if dest.exists() && !force {
        return Err(format!("{} already exists (use --force to overwrite)", dest.display()));
    }

    let mut cfg = Config::default();
    let mut imported = 0usize;
    let entries = std::fs::read_dir(root).map_err(|e| format!("read {}: {}", dir, e))?;
    for entry in entries.flatten() {
        let p = entry.path();
        if !p.is_dir() { continue; }
        let cfg_json = p.join("config.json");
        if !cfg_json.exists() { continue; }
        let text = match std::fs::read_to_string(&cfg_json) {
            Ok(t) => t,
            Err(e) => { eprintln!("Skipping {}: {}", cfg_json.display(), e); continue; }
        };
        match serde_json::from_str::<config::Target>(&text) {
            Ok(target) => {
                if cfg.targets.iter().any(|t| t.name == target.name) {
                    eprintln!("Skipping duplicate target '{}'", target.name);
                    continue;
                }
                println!("  + target '{}' -> {}", target.name, target.path);
                cfg.targets.push(target);
                imported += 1;
            }
            Err(e) => eprintln!("Skipping {}: invalid config ({})", cfg_json.display(), e),
        }
    }

    if imported == 0 {
        return Err(format!("no valid <target>/config.json found under '{}'", dir));
    }
    cfg.save()?;
    Ok(dest)
}

/// One target-view scan unit, fully owned so it can be moved into a worker
/// thread without borrowing the scan plan.
struct ViewJob {
    name: String,
    scan_path: String,
    prefix: Option<String>,
    team_map: std::collections::HashMap<String, i64>,
    team_names: std::collections::HashMap<i64, String>,
    purge_time: Option<i64>,
}

/// Live per-view progress sink shared between a worker thread and the TUI.
struct ViewProgress {
    name: String,
    scan: check_disk_core::scan_core::ScanProgress,
    /// Current coarse phase label (scanning/building/treemap/merging/history/done/error).
    phase: std::sync::Mutex<String>,
    error: std::sync::Mutex<String>,
    started: std::time::Instant,
    finished: std::sync::atomic::AtomicBool,
}

/// Set up the TUI, build the device-aware plan, and run each device group on
/// its own thread (groups = distinct physical devices → real parallelism;
/// roots within a group run sequentially to avoid thrashing one disk). The
/// calling thread owns the terminal and polls live progress from each view's
/// ScanProgress sink, rendering the TUI and handling q / Ctrl+C.
fn run_scan(
    cfg: &mut Config,
    output_dir: Option<String>,
    tree_map: bool,
    workers: Option<usize>,
    level: usize,
    target: &[String],
) {
    let out = output_dir.unwrap_or(cfg.output_dir.clone());
    let budget = workers.unwrap_or_else(|| {
        std::thread::available_parallelism()
            .map(|n| n.get() * 2)
            .unwrap_or(8)
            .min(32)
    });

    // Restrict to requested targets, if any.
    if !target.is_empty() {
        let want: std::collections::HashSet<&str> = target.iter().map(|s| s.as_str()).collect();
        for m in target.iter().filter(|n| !cfg.targets.iter().any(|t| &t.name == *n)) {
            eprintln!("Warning: target '{}' not found in config", m);
        }
        cfg.targets.retain(|t| want.contains(t.name.as_str()));
        if cfg.targets.is_empty() {
            eprintln!("No matching targets to scan.");
            return;
        }
    }

    let plan = build_scan_plan(cfg, budget);

    // Flatten the plan into owned per-group job lists and a flat list of view
    // names (in plan order) for the TUI. `end_scan` cutoff is enforced here:
    // views past their end date are skipped with a warning.
    let today = chrono::Local::now().format("%Y%m%d").to_string();
    let mut group_jobs: Vec<(usize, Vec<ViewJob>)> = Vec::new();
    let mut view_names: Vec<String> = Vec::new();
    for group in &plan.groups {
        let mut jobs: Vec<ViewJob> = Vec::new();
        for root in &group.roots {
            for view in &root.views {
                if let Some(ref es) = view.end_scan {
                    if today.as_str() > es.as_str() {
                        eprintln!("Skipping target '{}': past end_scan {}", view.name, es);
                        continue;
                    }
                }
                view_names.push(view.name.clone());
                jobs.push(ViewJob {
                    name: view.name.clone(),
                    scan_path: root.scan_path.to_string_lossy().to_string(),
                    prefix: view.prefix.clone().map(|p| p.to_string_lossy().to_string()),
                    team_map: view.team_map.clone(),
                    team_names: view.team_names.clone(),
                    purge_time: view.purge_time,
                });
            }
        }
        if !jobs.is_empty() {
            // Diagnostic: confirms each device's class + worker allocation so it's
            // clear local disks aren't wrongly capped as NFS.
            let names: Vec<&str> = jobs.iter().map(|j| j.name.as_str()).collect();
            eprintln!(
                "Device group dev={} class={} workers={} targets=[{}]",
                group.st_dev, group.dev_class, group.workers, names.join(", ")
            );
            group_jobs.push((group.workers, jobs));
        }
    }
    if view_names.is_empty() {
        eprintln!("No targets to scan (all skipped or empty).");
        return;
    }

    let max_parallel_devices = cfg.max_parallel_devices.max(0) as usize;
    std::fs::create_dir_all(&out).ok();
    run_scan_tui(&out, tree_map, level, max_parallel_devices, group_jobs, view_names);
}

/// RAII guard that owns the terminal for the TUI. It renders to `/dev/tty`
/// directly (a dedicated fd) and redirects stdout+stderr (fd 1/2) to /dev/null
/// for the duration of the scan, so the core's Phase 2/3 `println!`/`eprintln!`
/// noise never bleeds onto the live table. On drop — including panics — it
/// restores fd 1/2, leaves the alternate screen, and disables raw mode, so an
/// abort or crash never leaves the user's terminal wedged.
struct TerminalGuard {
    active: bool,
    /// The /dev/tty handle the ratatui backend renders through.
    tty: Option<std::fs::File>,
    /// Saved originals of fd 1 and fd 2, restored on drop.
    saved_stdout: libc::c_int,
    saved_stderr: libc::c_int,
}

impl TerminalGuard {
    /// Enter TUI mode. Returns the guard plus the /dev/tty writer the caller
    /// hands to `CrosstermBackend`. `active == false` (and `tty == None`) when
    /// stdout is not a TTY or /dev/tty is unavailable — the caller then runs
    /// headless with plain stdout logging.
    fn enter() -> Self {
        use crossterm::terminal::{enable_raw_mode, EnterAlternateScreen};
        use crossterm::ExecutableCommand;
        use std::os::unix::io::AsRawFd;

        let is_tty = unsafe { libc::isatty(libc::STDOUT_FILENO) != 0 };
        let mut g = TerminalGuard { active: false, tty: None, saved_stdout: -1, saved_stderr: -1 };
        if !is_tty {
            return g;
        }

        // Render target: the controlling terminal, independent of fd 1/2.
        let mut tty = match std::fs::OpenOptions::new().read(true).write(true).open("/dev/tty") {
            Ok(f) => f,
            Err(_) => return g,
        };
        if enable_raw_mode().is_err() {
            return g;
        }
        if tty.execute(EnterAlternateScreen).is_err() {
            let _ = crossterm::terminal::disable_raw_mode();
            return g;
        }

        // Suppress core stdout/stderr: save fd 1/2, point both at /dev/null.
        unsafe {
            g.saved_stdout = libc::dup(1);
            g.saved_stderr = libc::dup(2);
            if let Ok(devnull) = std::fs::OpenOptions::new().write(true).open("/dev/null") {
                let nfd = devnull.as_raw_fd();
                libc::dup2(nfd, 1);
                libc::dup2(nfd, 2);
                // devnull dropped here closes its own fd; the dup2'd 1/2 remain.
            }
        }

        g.tty = Some(tty);
        g.active = true;
        g
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        if !self.active {
            return;
        }
        use crossterm::terminal::{disable_raw_mode, LeaveAlternateScreen};
        use crossterm::ExecutableCommand;
        // Restore fd 1/2 first so LeaveAlternateScreen and any later prints land
        // on the real terminal.
        unsafe {
            if self.saved_stdout >= 0 {
                libc::dup2(self.saved_stdout, 1);
                libc::close(self.saved_stdout);
            }
            if self.saved_stderr >= 0 {
                libc::dup2(self.saved_stderr, 2);
                libc::close(self.saved_stderr);
            }
        }
        if let Some(ref mut tty) = self.tty {
            let _ = tty.execute(LeaveAlternateScreen);
        }
        let _ = disable_raw_mode();
        self.active = false;
    }
}

/// Own the terminal, spawn one worker thread per device group, and drive the
/// live TUI: poll each view's ScanProgress ~7×/s, compute rate/mem, render, and
/// watch for q / Ctrl+C to abort. Returns after all group threads join.
fn run_scan_tui(
    out: &str,
    tree_map: bool,
    level: usize,
    max_parallel_devices: usize,
    group_jobs: Vec<(usize, Vec<ViewJob>)>,
    view_names: Vec<String>,
) {
    use std::sync::atomic::Ordering;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    // Shared progress sinks, one per view, keyed by position in view_names.
    let progresses: Vec<Arc<ViewProgress>> = view_names
        .iter()
        .map(|name| {
            Arc::new(ViewProgress {
                name: name.clone(),
                scan: check_disk_core::scan_core::ScanProgress::new(),
                phase: Mutex::new("waiting".to_string()),
                error: Mutex::new(String::new()),
                started: Instant::now(),
                finished: std::sync::atomic::AtomicBool::new(false),
            })
        })
        .collect();
    // name -> sink for worker lookup.
    let sink_by_name: std::collections::HashMap<String, Arc<ViewProgress>> =
        progresses.iter().map(|p| (p.name.clone(), p.clone())).collect();

    let abort = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let app_state = Arc::new(Mutex::new(ui::AppState::new(&view_names)));

    // Optional cap on how many device groups scan concurrently (0 = unlimited).
    // A tiny counting semaphore built from a Mutex+Condvar: each group thread
    // acquires a permit before its first target and releases it when done.
    let permits = if max_parallel_devices > 0 { max_parallel_devices } else { usize::MAX };
    let sem = Arc::new((Mutex::new(permits), std::sync::Condvar::new()));

    // Global build lock: serializes the RAM-heavy Phase 2/3/merge/history across
    // all groups so peak RSS stays at ~one pipeline. Phase 1 scans stay parallel.
    let build_lock = Arc::new(Mutex::new(()));

    // Spawn one thread per device group; roots within a group run sequentially.
    let mut handles = Vec::new();
    for (group_workers, jobs) in group_jobs {
        let out = out.to_string();
        let abort = abort.clone();
        let sink_map = sink_by_name.clone();
        let sem = sem.clone();
        let build_lock = build_lock.clone();
        handles.push(std::thread::spawn(move || {
            // Acquire a device permit (skip the wait entirely when unlimited).
            if permits != usize::MAX {
                let (lock, cvar) = &*sem;
                let mut avail = lock.lock().unwrap();
                while *avail == 0 && !abort.load(Ordering::SeqCst) {
                    avail = cvar.wait(avail).unwrap();
                }
                if *avail > 0 {
                    *avail -= 1;
                }
            }
            for job in jobs {
                if abort.load(Ordering::SeqCst) {
                    break;
                }
                let sink = match sink_map.get(&job.name) {
                    Some(s) => s.clone(),
                    None => continue,
                };
                scan_one_view(&out, tree_map, level, group_workers, &job, &sink, &build_lock);
                sink.finished.store(true, Ordering::SeqCst);
            }
            // Release the device permit.
            if permits != usize::MAX {
                let (lock, cvar) = &*sem;
                let mut avail = lock.lock().unwrap();
                *avail += 1;
                cvar.notify_one();
            }
        }));
    }

    // ── TUI poll loop (this thread owns the terminal) ──
    // The guard renders to /dev/tty and silences core stdout/stderr for the
    // scan; it restores everything on drop (incl. panic).
    let guard = TerminalGuard::enter();
    let tui_active = guard.active;
    use ratatui::backend::CrosstermBackend;
    let mut terminal = match guard.tty.as_ref() {
        Some(tty) => {
            // Clone the /dev/tty handle for the backend; the guard keeps its own
            // for LeaveAlternateScreen on drop.
            tty.try_clone().ok().and_then(|w| ratatui::Terminal::new(CrosstermBackend::new(w)).ok())
        }
        None => None,
    };

    // Per-view rate tracking: (last_files, last_instant).
    let mut last_seen: std::collections::HashMap<String, (u64, Instant)> =
        view_names.iter().map(|n| (n.clone(), (0u64, Instant::now()))).collect();

    loop {
        // Poll keyboard for abort (q / Ctrl+C / Esc).
        if tui_active {
            while crossterm::event::poll(Duration::from_millis(0)).unwrap_or(false) {
                if let Ok(crossterm::event::Event::Key(k)) = crossterm::event::read() {
                    use crossterm::event::{KeyCode, KeyModifiers};
                    let quit = matches!(k.code, KeyCode::Char('q') | KeyCode::Esc)
                        || (k.code == KeyCode::Char('c')
                            && k.modifiers.contains(KeyModifiers::CONTROL));
                    if quit {
                        abort.store(true, Ordering::SeqCst);
                        let mut s = app_state.lock().unwrap();
                        s.abort = true;
                    }
                }
            }
        }

        // Sync sink → AppState, compute rate/mem.
        let mem_mb = check_disk_core::pipe_types::get_rss_mb();
        let all_finished = progresses.iter().all(|p| p.finished.load(Ordering::SeqCst));
        {
            let mut s = app_state.lock().unwrap();
            for p in &progresses {
                let (files, dirs, size) = p.scan.snapshot();
                let now = Instant::now();
                let entry = last_seen.get_mut(&p.name).unwrap();
                let dt = now.duration_since(entry.1).as_secs_f64();
                let rate = if dt > 0.0 { (files.saturating_sub(entry.0)) as f64 / dt } else { 0.0 };
                *entry = (files, now);
                let phase = p.phase.lock().unwrap().clone();
                let err = p.error.lock().unwrap().clone();
                if let Some(t) = s.target_mut(&p.name) {
                    t.files = files;
                    t.dirs = dirs;
                    t.size = size;
                    if phase != "waiting" {
                        t.rate = rate;
                        t.mem_mb = mem_mb;
                        t.elapsed = p.started.elapsed().as_secs_f64();
                    }
                    t.phase = phase;
                    t.error = err;
                }
            }
            if all_finished {
                s.running = false;
            }
            if let Some(ref mut term) = terminal {
                term.draw(|f| ui::draw(f, &s)).ok();
            }
        }

        if all_finished {
            break;
        }
        std::thread::sleep(Duration::from_millis(150));
    }

    for h in handles {
        let _ = h.join();
    }

    // Final render, brief pause so the user sees the completed table.
    {
        let mut s = app_state.lock().unwrap();
        s.running = false;
        if let Some(ref mut term) = terminal {
            term.draw(|f| ui::draw(f, &s)).ok();
        }
    }
    std::thread::sleep(Duration::from_millis(800));
    drop(terminal);
    drop(guard);

    let (tf, td): (u64, u64) = {
        let s = app_state.lock().unwrap();
        s.targets.iter().fold((0, 0), |(f, d), t| (f + t.files, d + t.dirs))
    };
    if abort.load(Ordering::SeqCst) {
        println!("Scan aborted: {} files, {} dirs (partial)", tf, td);
    } else {
        println!("Scan complete: {} files, {} dirs", tf, td);
    }
}

/// Run the full pipeline for one target view: Phase 1 scan (feeding the live
/// progress sink), Phase 2 detail DB, optional treemap, merge into report.db,
/// and history snapshot. Updates the sink's phase label as it advances and
/// writes scan_status.json + a per-scan log. Never redirects stdout.
fn scan_one_view(
    out: &str,
    tree_map: bool,
    level: usize,
    group_workers: usize,
    job: &ViewJob,
    sink: &ViewProgress,
    build_lock: &std::sync::Arc<std::sync::Mutex<()>>,
) {
    use std::sync::atomic::Ordering;

    let set_phase = |p: &str| {
        *sink.phase.lock().unwrap() = p.to_string();
    };
    let now_epoch = || {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0)
    };

    let target_out = std::path::Path::new(out).join(&job.name);
    let run_started = now_epoch();
    let dir_str = job.scan_path.clone();

    set_phase("scanning");
    write_scan_status(&target_out, "scanning", true, run_started, run_started, "Phase 1 scan", "", tree_map);

    // Phase 1 — hand the engine our shared progress sink so the TUI sees live counts.
    let progress = sink.scan.clone();
    let phase1 = check_disk_core::scan_core::run_scan_core(
        dir_str.clone(), vec![], None, Some(group_workers), false, "cli", None, Some(progress),
    );
    // Ensure the done flag is set regardless of engine internals.
    sink.scan.done.store(true, Ordering::SeqCst);

    let result = match phase1 {
        Ok(r) => r,
        Err(e) => {
            *sink.error.lock().unwrap() = e.to_string();
            set_phase("error");
            write_scan_status(&target_out, "error", false, run_started, run_started, "", &e.to_string(), tree_map);
            eprintln!("Error scanning '{}': {}", job.name, e);
            return;
        }
    };

    let files = result["total_files"].as_u64().unwrap_or(0);
    let dirs = result["total_dirs"].as_u64().unwrap_or(0);
    let tmpdir = result["detail_tmpdir"].as_str().unwrap_or("").to_string();
    if tmpdir.is_empty() {
        set_phase("done");
        write_scan_status(&target_out, "done", false, run_started, run_started, "Scan complete", "", tree_map);
        return;
    }

    // Resolve uids seen in this scan → usernames.
    let uid_sizes = result["uid_sizes"].as_object().cloned().unwrap_or_default();
    let mut uids_map = std::collections::HashMap::new();
    for (uid_str, _) in &uid_sizes {
        if let Ok(uid) = uid_str.parse::<u32>() {
            uids_map.insert(uid, username_for_uid(uid));
        }
    }
    // team_map for the pipeline is keyed by username → team_id(as string).
    let mut team_map: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    for (uname, tid) in &job.team_map {
        team_map.insert(uname.clone(), tid.to_string());
    }

    let out_path = std::path::Path::new(out).join(&job.name);
    std::fs::create_dir_all(&out_path).ok();
    let detail_db = out_path.join("data_detail.db").to_string_lossy().to_string();
    let treemap_db = out_path.join("treemap.db").to_string_lossy().to_string();

    // Serialize the RAM-heavy build stages (Phase 2/3 + merge + history) across
    // all device groups: Phase 1 scans run in parallel, but only one target
    // builds its DB at a time, so peak RSS ≈ one pipeline regardless of how many
    // disks are scanned. Show `queued` while waiting for the build slot so the
    // TUI doesn't look hung. The guard is held to end of function.
    set_phase("queued");
    write_scan_status(&target_out, "queued", true, run_started, run_started, "Waiting for build slot", "", tree_map);
    let _build_guard = build_lock.lock().unwrap_or_else(|e| e.into_inner());

    let timestamp = now_epoch();
    let phase2_start = std::time::Instant::now();

    set_phase("building");
    write_scan_status(&target_out, "building", true, run_started, timestamp, "Phase 2 detail", "", tree_map);

    let build = check_disk_core::report_pipeline::build_detail_db_impl(
        tmpdir, uids_map, team_map,
        detail_db, treemap_db,
        dir_str.clone(), level, 0, timestamp,
        1, tree_map, false, job.prefix.clone(),
    );
    let (total, agg_opt) = match build {
        Ok(v) => v,
        Err(e) => {
            *sink.error.lock().unwrap() = e.clone();
            set_phase("error");
            write_scan_status(&target_out, "error", false, run_started, timestamp, "", &e, tree_map);
            eprintln!("Phase 2 error for '{}': {}", job.name, e);
            return;
        }
    };

    if tree_map {
        if let Some(agg) = &agg_opt {
            set_phase("treemap");
            let tm_out = out_path.join("treemap.db");
            if let Err(e) = check_disk_core::report_pipeline::build_treemap_db_impl(
                agg, &tm_out, &dir_str, level, 0, timestamp, false,
            ) {
                eprintln!("Treemap error for '{}': {:?}", job.name, e);
            }
        }
    }

    // Move permission_issues.db from output root into the target dir.
    let root_perm = std::path::Path::new(out).join("permission_issues.db");
    let target_perm = out_path.join("permission_issues.db");
    if root_perm.exists() {
        let _ = std::fs::rename(&root_perm, &target_perm);
    }

    set_phase("merging");
    let merged = out_path.join("report.db");
    let merge_result = check_disk_core::db_writer::merge_into_single_db(
        &out_path, &merged, &job.name, &dir_str, timestamp,
    );
    let merge_ok = merge_result.is_ok();
    if !merge_ok {
        eprintln!("Merge error for '{}': {:?}", job.name, merge_result);
    }
    if merge_ok {
        let _ = std::fs::remove_file(out_path.join("data_detail.db"));
        let _ = std::fs::remove_file(out_path.join("treemap.db"));
        let _ = std::fs::remove_file(out_path.join("report.tmp"));

        set_phase("history");
        if let Err(e) = write_history_snapshot(
            &merged, &dir_str, &job.team_map, &job.team_names, timestamp, job.purge_time,
        ) {
            eprintln!("History error for '{}': {}", job.name, e);
        }
    }

    // Per-scan log (legacy-style summary banner).
    let logs_dir = out_path.join("logs");
    std::fs::create_dir_all(&logs_dir).ok();
    let log_name = chrono::Local::now().format("scan_%Y%m%d_%H%M%S.log").to_string();
    write_scan_log(
        &logs_dir.join(&log_name), &merged, &job.name, &dir_str,
        files, dirs,
        result["total_inodes"].as_u64().unwrap_or(0),
        result["total_size"].as_i64().unwrap_or(0),
        result["permission_issues_count"].as_u64().unwrap_or(0),
        total, tree_map, merge_ok, phase2_start.elapsed().as_secs() as i64,
    );

    set_phase("done");
    write_scan_status(&target_out, "done", false, run_started, timestamp, "Scan complete", "", tree_map);
}

fn print_tree(
    conn: &rusqlite::Connection,
    tp: &str,
    dir_id: i64,
    depth: usize,
    max_depth: usize,
    limit: usize,
    prefix: &str,
    _start_path: Option<&str>,
    search: Option<&str>,
) {
    if depth > max_depth { return; }

    // Print current node name (for root)
    if depth == 0 && dir_id >= 0 {
        let root_name: String = conn.query_row(
            &format!("SELECT COALESCE(n.name, '<root>') FROM {}dirs d \
                      LEFT JOIN {}names n ON d.name_id = n.id WHERE d.id = ?1", tp, tp),
            rusqlite::params![dir_id],
            |r| r.get(0),
        ).unwrap_or_else(|_| "<root>".to_string());

        let root_size: i64 = conn.query_row(
            &format!("SELECT total_size FROM {}dirs WHERE id = ?1", tp),
            rusqlite::params![dir_id],
            |r| r.get(0),
        ).unwrap_or(0);

        println!("{} [{}]", root_name, fmt_size(root_size));
    }

    let sql = format!(
        "SELECT d.id, n.name, d.total_size, d.file_count \
         FROM {}dirs d JOIN {}names n ON d.name_id = n.id \
         WHERE d.parent_id = ?1 \
         ORDER BY d.total_size DESC \
         LIMIT ?2",
        tp, tp,
    );
    if let Ok(mut stmt) = conn.prepare(&sql) {
        if let Ok(rows) = stmt.query_map(rusqlite::params![dir_id, limit as i64], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, i64>(3)?,
            ))
        }) {
            let rows: Vec<_> = rows.flatten().collect();
            let n = rows.len();
            for (i, row) in rows.into_iter().enumerate() {
                let (id, name, size, files) = row;
                let is_last = i + 1 == n;
                let connector = if is_last { "└── " } else { "├── " };
                let child_prefix = if is_last { "    " } else { "│   " };

                let full_path = format!("{}/{}", prefix.trim_end_matches('/'), name);
                let disp = if depth == 0 { &full_path } else { &name };

                // Filter by search
                if let Some(kw) = search {
                    if !full_path.contains(kw) { continue; }
                }

                println!("{}{} [{}]", connector, disp, fmt_size(size));

                if files > 0 && depth + 1 < max_depth {
                    print_tree(conn, tp, id, depth + 1, max_depth, limit, &child_prefix, None, None);
                }
            }
        }
    }
}

/// Write full per-user dir and file usage dumps (all rows, sorted by size desc)
/// to `<export_dir>/usage_dir_<user>.txt` and `usage_file_<user>.txt`.
/// `export_dir` is already scoped to a single target's subdirectory.
fn export_user_text(conn: &rusqlite::Connection, uid: i64, username: &str, export_dir: &std::path::Path) {
    use std::io::Write;
    let safe_user: String = username
        .chars()
        .map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect();
    let header = format!("{:<5} {:<20} {:>12}  {}\n{}\n", "Type", "User", "Size", "Path", "-".repeat(90));

    // Dirs
    let dir_path = export_dir.join(format!("usage_dir_{}.txt", safe_user));
    if let Ok(mut f) = std::fs::File::create(&dir_path) {
        let _ = f.write_all(header.as_bytes());
        if let Ok(mut stmt) = conn.prepare(
            "SELECT size, path FROM detail_dirs WHERE uid = ?1 ORDER BY size DESC",
        ) {
            if let Ok(rows) = stmt.query_map(rusqlite::params![uid], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
            }) {
                for row in rows.flatten() {
                    let _ = writeln!(f, "{:<5} {:<20} {:>12}  {}", "dir", username, fmt_size(row.0), row.1);
                }
            }
        }
    }

    // Files (join names + parent dir path)
    let file_path = export_dir.join(format!("usage_file_{}.txt", safe_user));
    if let Ok(mut f) = std::fs::File::create(&file_path) {
        let _ = f.write_all(header.as_bytes());
        if let Ok(mut stmt) = conn.prepare(
            "SELECT fl.size, d.path, n.name \
             FROM detail_files fl \
             JOIN detail_dirs d ON fl.dir_id = d.id AND fl.uid = d.uid \
             JOIN detail_file_names n ON fl.name_id = n.id \
             WHERE fl.uid = ?1 ORDER BY fl.size DESC",
        ) {
            if let Ok(rows) = stmt.query_map(rusqlite::params![uid], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?))
            }) {
                for row in rows.flatten() {
                    let full = format!("{}/{}", row.1.trim_end_matches('/'), row.2);
                    let _ = writeln!(f, "{:<5} {:<20} {:>12}  {}", "file", username, fmt_size(row.0), full);
                }
            }
        }
    }
}

/// Read the latest snapshot from report.db, build an MS Teams Adaptive Card,
/// and POST it via curl. Returns Err on missing data or curl failure.
fn send_teams_notification(
    webhook_url: &str,
    db_path: &std::path::Path,
    target: &str,
) -> Result<(), String> {
    let conn = rusqlite::Connection::open(db_path).map_err(|e| e.to_string())?;

    // Latest snapshot's system figures.
    let (path, total, used, available): (String, i64, i64, i64) = conn
        .query_row(
            "SELECT path, total, used, available FROM hist_snapshots ORDER BY scan_date DESC LIMIT 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .map_err(|_| "no snapshot in report.db".to_string())?;
    let snap_id: i64 = conn
        .query_row("SELECT id FROM hist_snapshots ORDER BY scan_date DESC LIMIT 1", [], |r| r.get(0))
        .map_err(|e| e.to_string())?;

    let top = |kind: &str| -> Vec<(String, i64)> {
        conn.prepare(
            "SELECT name, size FROM hist_user_usage WHERE snapshot_id = ?1 AND kind = ?2 ORDER BY size DESC LIMIT 10",
        )
        .and_then(|mut s| {
            s.query_map(rusqlite::params![snap_id, kind], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))
                .map(|rows| rows.flatten().collect())
        })
        .unwrap_or_default()
    };
    let lines = |rows: &[(String, i64)]| -> String {
        if rows.is_empty() { return "- No data".into(); }
        rows.iter().map(|(n, s)| format!("- **{}**: {}", n, fmt_size(*s))).collect::<Vec<_>>().join("\n")
    };

    let now_str = chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
    let payload = serde_json::json!({
        "type": "message",
        "attachments": [{
            "contentType": "application/vnd.microsoft.card.adaptive",
            "contentUrl": null,
            "content": {
                "$schema": "http://adaptivecards.io/schemas/adaptive-card.json",
                "type": "AdaptiveCard",
                "version": "1.4",
                "body": [
                    {"type": "TextBlock", "size": "Large", "weight": "Bolder", "wrap": true,
                     "text": format!("📊 Disk Usage Scan: {}", target)},
                    {"type": "FactSet", "facts": [
                        {"title": "Time:", "value": now_str},
                        {"title": "Directory:", "value": path},
                        {"title": "Total Space:", "value": fmt_size(total)},
                        {"title": "Used Space:", "value": fmt_size(used)},
                        {"title": "Available:", "value": fmt_size(available)}
                    ]},
                    {"type": "TextBlock", "text": "🏆 **Top 10 Users:**", "wrap": true, "spacing": "Medium"},
                    {"type": "TextBlock", "text": lines(&top("user")), "wrap": true},
                    {"type": "TextBlock", "text": "🌍 **Top 10 Other Users:**", "wrap": true, "spacing": "Medium"},
                    {"type": "TextBlock", "text": lines(&top("other")), "wrap": true}
                ]
            }
        }]
    });

    let output = std::process::Command::new("curl")
        .args(["-s", "-S", "--max-time", "30", "-X", "POST",
               "-H", "Content-Type: application/json",
               "-d", &payload.to_string(), webhook_url])
        .output()
        .map_err(|e| format!("curl failed to start: {} (is curl installed?)", e))?;
    if !output.status.success() {
        return Err(format!("curl exited {}: {}", output.status, String::from_utf8_lossy(&output.stderr)));
    }
    Ok(())
}

/// Write a per-scan log with a legacy-style summary banner: phase counts,
/// scan rate, disk-info snapshot (statvfs), and a top-users table with bars.
#[allow(clippy::too_many_arguments)]
fn write_scan_log(
    log_path: &std::path::Path,
    db_path: &std::path::Path,
    target: &str,
    dir: &str,
    files: u64,
    dirs: u64,
    inodes: u64,
    total_size: i64,
    perms: u64,
    detail_files: u64,
    tree_map: bool,
    merge_ok: bool,
    elapsed: i64,
) {
    use std::io::Write;
    let Ok(mut f) = std::fs::File::create(log_path) else { return };
    let bar = "=".repeat(60);
    let rate = if elapsed > 0 { files / elapsed as u64 } else { files };

    let _ = writeln!(f, "=== STARTING DISK USAGE SCAN ===");
    let _ = writeln!(f, "Target: {}", target);
    let _ = writeln!(f, "Directory: {}", dir);
    let _ = writeln!(f, "Started: {}", chrono::Local::now().format("%Y-%m-%d %H:%M:%S"));

    let _ = writeln!(f);
    let _ = writeln!(f, "{}", bar);
    let _ = writeln!(f, "SCAN COMPLETED in {}s", elapsed);
    let _ = writeln!(f, "{}", bar);
    let _ = writeln!(f, "Total directories: {}", dirs);
    let _ = writeln!(f, "Total files:       {}", files);
    let _ = writeln!(f, "Total inodes:      {}", inodes);
    let _ = writeln!(f, "Total size:        {}", fmt_size(total_size));
    let _ = writeln!(f, "Scan rate:         {} files/sec", rate);
    let _ = writeln!(f, "Permission issues: {}", perms);
    let _ = writeln!(f, "{}", bar);

    // Disk-info snapshot + top users come from the merged report.db.
    if let Ok(conn) = rusqlite::Connection::open(db_path) {
        if let Ok((total, used, avail)) = conn.query_row(
            "SELECT total, used, available FROM hist_snapshots ORDER BY scan_date DESC LIMIT 1",
            [], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?, r.get::<_, i64>(2)?)),
        ) {
            let pct = if total > 0 { used as f64 / total as f64 * 100.0 } else { 0.0 };
            let _ = writeln!(f, "Disk Information:");
            let _ = writeln!(f, "  Total capacity: {}", fmt_size(total));
            let _ = writeln!(f, "  Used space:     {} ({:.1}%)", fmt_size(used), pct);
            let _ = writeln!(f, "  Available:      {}", fmt_size(avail));
            let _ = writeln!(f, "{}", bar);
        }

        write_top_users_table(&mut f, &conn, total_size);
    }

    let _ = writeln!(f, "[Phase 2] Detail DB built: {} files", detail_files);
    if tree_map {
        let _ = writeln!(f, "[Phase 2] Treemap built");
    }
    let _ = writeln!(f, "[Phase 3] Merged into report.db ({})", if merge_ok { "ok" } else { "FAILED" });
    let _ = writeln!(f, "=== SCAN COMPLETED SUCCESSFULLY ===");
}

/// Append a "Top 20 users by disk usage" table with ASCII percent bars.
fn write_top_users_table(f: &mut std::fs::File, conn: &rusqlite::Connection, total_size: i64) {
    use std::io::Write;
    let snap_id: i64 = match conn.query_row(
        "SELECT id FROM hist_snapshots ORDER BY scan_date DESC LIMIT 1", [], |r| r.get(0),
    ) { Ok(v) => v, Err(_) => return };

    let mut stmt = match conn.prepare(
        "SELECT name, size FROM hist_user_usage WHERE snapshot_id = ?1 ORDER BY size DESC LIMIT 20",
    ) { Ok(s) => s, Err(_) => return };
    let rows: Vec<(String, i64)> = stmt
        .query_map(rusqlite::params![snap_id], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))
        .map(|rows| rows.flatten().collect())
        .unwrap_or_default();
    if rows.is_empty() { return; }

    let _ = writeln!(f, "Top users by disk usage:");
    for (name, size) in rows {
        let pct = if total_size > 0 { size as f64 / total_size as f64 * 100.0 } else { 0.0 };
        let filled = ((pct / 5.0).round() as usize).min(20);
        let barbuf: String = "#".repeat(filled) + &"-".repeat(20 - filled);
        let _ = writeln!(f, "  {:<16} {:>10}  [{}] {:>5.1}%", name, fmt_size(size), barbuf, pct);
    }
    let _ = writeln!(f, "{}", "=".repeat(60));
}
