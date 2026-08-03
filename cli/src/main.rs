mod config;
mod config_tui;
mod scheduler;

use clap::Parser;
use config::Config;
use scheduler::build_scan_plan;

// Replace the system allocator with mimalloc: large parallel workloads that
// churn millions of short-lived allocations (Phase 1/2 spill buffers, interning
// maps, per-row FileRows) fragment glibc's heap badly; mimalloc returns freed
// pages to the OS more aggressively and shows consistently lower RSS + higher
// throughput. Validated against glibc on production reports (see AGENTS.md).
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

/// mimalloc option index for `mi_option_arena_reserve` (mimalloc v2 options.c
/// table: ... 22=destroy_on_exit, 23=arena_reserve). Value is in KiB; 0
/// disables arena pre-reservation so large allocations fall back to direct
/// OS mmap. See `configure_mimalloc`.
const MI_OPTION_ARENA_RESERVE: i32 = 23;

extern "C" {
    fn mi_option_set(option: i32, value: std::os::raw::c_long);
}

/// Cap mimalloc's virtual address-space footprint.
///
/// By default mimalloc reserves arenas in 1GiB blocks and grows the per-arena
/// reserve exponentially (`1 << arena_count/8`, up to 2^16×). With many walker
/// threads this balloons VSZ (observed ~27GB) while RSS stays low (~10GB) — the
/// arenas are reserved but never committed. Under a virtual-memory cap
/// (RLIMIT_AS, e.g. LSF `-M 20000` → `ulimit -v` 24GB) the next arena mmap
/// fails with ENOMEM and Rust aborts with "memory allocation of N bytes
/// failed" (SIGABRT / core dump), even though real RSS is well under the cap.
/// Disabling arena pre-reservation routes large allocations straight to the OS
/// in exact-size mmaps, keeping VSZ close to RSS.
fn configure_mimalloc() {
    unsafe {
        mi_option_set(MI_OPTION_ARENA_RESERVE, 0);
    }
}

#[derive(Parser)]
#[command(name = "duscan", version = "0.1.0")]
struct Cli {
    /// No subcommand → open the interactive config TUI.
    #[command(subcommand)]
    command: Option<Command>,
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
        /// Emit core Phase 2/3 profiling + RSS diagnostics to the per-scan log.
        #[arg(long)]
        debug: bool,
    },
    /// Show the latest scan status per target (reads each target's
    /// scan_status.json). Works for local and background scans.
    Status {
        #[arg(long)]
        output_dir: Option<String>,
        #[arg(long)]
        target: Option<String>,
        /// Poll and redraw every 2s until interrupted (Ctrl+C).
        #[arg(long)]
        watch: bool,
        #[arg(long)]
        json: bool,
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
    /// A user token may be @file to load usernames from a text file.
    SetTarget {
        name: String,
        path: String,
        /// team spec: "teamname=user1,user2" (repeatable). A user token may be
        /// "@path" to load names from a text file (newline/comma/space separated,
        /// # comments ok), e.g. --team dev=alice,@more_users.txt
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
    Detail {
        #[arg(long)] user: Vec<String>,
        #[arg(long)] output_dir: Option<String>,
        #[arg(long, default_value = "30")] top: usize,
        #[arg(long)] target: Option<String>,
        #[arg(long)] json: bool,
        /// Section to show: report (default), permission, inode.
        #[arg(long = "type", default_value = "report")] section: String,
        /// Filter permission issues by path substring (permission section only).
        #[arg(long)] search: Option<String>,
    },
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
        /// Use password auth via `sshpass -e` (reads password from SSHPASS env).
        #[arg(long)]
        pass: bool,
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
        /// Show a per-user growth/trend table across the snapshots instead of the
        /// per-day dump: columns are dates (old→new), plus Abs/%/Trend growth rows.
        #[arg(long)]
        compare: bool,
        /// Max users in the compare table (ranked by absolute growth).
        #[arg(long, default_value = "10")]
        top: usize,
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

/// What one statvfs() call says about the filesystem holding a path: bytes and
/// inodes side by side, because they come from the same struct and the dashboard
/// shows them as two views of the same capacity.
#[derive(Clone, Copy, Debug, Default)]
struct FsCapacity {
    total: i64,
    used: i64,
    available: i64,
    inodes_total: i64,
    inodes_used: i64,
    inodes_free: i64,
}

/// statvfs(path) → byte and inode capacity of the filesystem holding `path`.
///
/// A filesystem with no fixed inode table (btrfs, XFS with dynamic inodes, most
/// NFS mounts) reports `f_files = 0`. That is passed through as 0 rather than
/// faked, so the dashboard can say "not reported" instead of showing 100% used.
fn statvfs_meta(path: &str) -> FsCapacity {
    use std::ffi::CString;
    let c_path = match CString::new(path) {
        Ok(p) => p,
        Err(_) => return FsCapacity::default(),
    };
    unsafe {
        let mut s: libc::statvfs = std::mem::zeroed();
        if libc::statvfs(c_path.as_ptr(), &mut s) != 0 {
            return FsCapacity::default();
        }
        let bsize = s.f_frsize as i64;
        let total = s.f_blocks as i64 * bsize;
        let available = s.f_bavail as i64 * bsize;
        let used = total - s.f_bfree as i64 * bsize;
        let inodes_total = s.f_files as i64;
        let inodes_free = s.f_ffree as i64;
        FsCapacity {
            total,
            used,
            available,
            inodes_total,
            inodes_used: (inodes_total - inodes_free).max(0),
            inodes_free,
        }
    }
}

/// Atomically write `<target_dir>/scan_status.json` so a dashboard — or another
/// duscan process (`duscan status`) — can poll scan progress. Mirrors the legacy
/// Python heartbeat fields, plus live `files`/`dirs`/`size_bytes` counts so a
/// reader can show progress even when the scan runs elsewhere.
#[allow(clippy::too_many_arguments)]
fn write_scan_status(
    target_dir: &std::path::Path,
    stage: &str,
    running: bool,
    started_at: i64,
    phase_started_at: i64,
    message: &str,
    error: &str,
    tree_map_enabled: bool,
    files: u64,
    dirs: u64,
    size_bytes: u64,
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
        "files": files,
        "dirs": dirs,
        "size_bytes": size_bytes,
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

/// One target's scan status, read back from `scan_status.json`.
pub(crate) struct TargetStatus {
    pub name: String,
    pub stage: String,
    pub running: bool,
    pub files: u64,
    pub dirs: u64,
    pub size_bytes: u64,
    pub total_elapsed_sec: i64,
    pub updated_at: i64,
    pub error: String,
}

/// Read `<out>/<target>/scan_status.json` into a `TargetStatus`, or `None` if
/// the file is missing/unreadable/unparseable (target never scanned).
pub(crate) fn read_target_status(out: &str, target: &str) -> Option<TargetStatus> {
    let path = std::path::Path::new(out).join(target).join("scan_status.json");
    let text = std::fs::read_to_string(&path).ok()?;
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    Some(TargetStatus {
        name: target.to_string(),
        stage: v["stage"].as_str().unwrap_or("unknown").to_string(),
        running: v["running"].as_bool().unwrap_or(false),
        files: v["files"].as_u64().unwrap_or(0),
        dirs: v["dirs"].as_u64().unwrap_or(0),
        size_bytes: v["size_bytes"].as_u64().unwrap_or(0),
        total_elapsed_sec: v["total_elapsed_sec"].as_i64().unwrap_or(0),
        updated_at: v["updated_at"].as_i64().unwrap_or(0),
        error: v["error"].as_str().unwrap_or("").to_string(),
    })
}

/// `duscan status`: print the latest scan status for each configured target by
/// reading its `scan_status.json`. Works regardless of WHERE the scan runs —
/// local or background — because the status file is on shared
/// storage. `--watch` polls every 2s until Ctrl+C; `--json` emits a JSON array.
fn show_status(cfg: &Config, out: &str, target: Option<&str>, watch: bool, json: bool) {
    // Which targets to report: a single --target, else every configured target.
    let names: Vec<String> = match target {
        Some(t) => vec![t.to_string()],
        None => cfg.targets.iter().map(|t| t.name.clone()).collect(),
    };

    let render = || {
        let statuses: Vec<TargetStatus> = names
            .iter()
            .filter_map(|n| read_target_status(out, n))
            .collect();
        if json {
            let arr: Vec<serde_json::Value> = statuses.iter().map(|s| serde_json::json!({
                "target": s.name,
                "stage": s.stage,
                "running": s.running,
                "files": s.files,
                "dirs": s.dirs,
                "size_bytes": s.size_bytes,
                "total_elapsed_sec": s.total_elapsed_sec,
                "updated_at": s.updated_at,
                "error": s.error,
            })).collect();
            println!("{}", serde_json::Value::Array(arr));
            return;
        }
        // Plain table.
        println!("{:<18} {:<10} {:>12} {:>10} {:>10} {:>8}  {}",
            "Target", "Stage", "Files", "Dirs", "Size", "Elapsed", "Note");
        println!("{}", "-".repeat(90));
        if statuses.is_empty() {
            println!("(no scan status yet — run `duscan run --target <name>` first)");
        }
        for s in &statuses {
            let note = if !s.error.is_empty() {
                format!("ERROR: {}", s.error)
            } else if s.running {
                let age = now_epoch_secs().saturating_sub(s.updated_at);
                // Flag a stale heartbeat: running but not updated in >30s.
                if age > 30 { format!("running (stale {}s)", age) } else { "running".to_string() }
            } else {
                "idle/done".to_string()
            };
            println!("{:<18} {:<10} {:>12} {:>10} {:>10} {:>7}s  {}",
                truncate_str(&s.name, 18),
                s.stage,
                fmt_count_u64(s.files),
                fmt_count_u64(s.dirs),
                fmt_size(s.size_bytes as i64),
                s.total_elapsed_sec,
                note);
        }
    };

    if !watch {
        render();
        return;
    }
    // --watch: clear + redraw every 2s until interrupted.
    loop {
        // ANSI clear screen + home cursor.
        print!("\x1b[2J\x1b[H");
        let ts = chrono::Local::now().format("%Y-%m-%d %H:%M:%S");
        println!("duscan status — {} (Ctrl+C to stop)\n", ts);
        render();
        let all_idle = names
            .iter()
            .filter_map(|n| read_target_status(out, n))
            .all(|s| !s.running);
        // Keep watching even when idle (a new scan may start); the user stops it.
        let _ = all_idle;
        std::thread::sleep(std::time::Duration::from_secs(2));
    }
}

/// Current unix time in seconds (0 on clock error).
fn now_epoch_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Thousands-separated u64.
fn fmt_count_u64(n: u64) -> String {
    let s = n.to_string();
    let b = s.as_bytes();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in b.iter().enumerate() {
        if i > 0 && (b.len() - i) % 3 == 0 { out.push(','); }
        out.push(*c as char);
    }
    out
}

/// Truncate to `max` chars with an ellipsis when cut.
fn truncate_str(s: &str, max: usize) -> String {
    if s.chars().count() <= max { s.to_string() }
    else { format!("{}…", s.chars().take(max.saturating_sub(1)).collect::<String>()) }
}

/// Aggregate per-user/per-team usage from the merged report.db and upsert one
/// per-day history snapshot. `team_map` maps username → team_id, `team_names`
/// maps team_id → team name (both from config).
///
/// `inodes_scanned` is the walk's own inode count. statvfs cannot supply it: it
/// knows the whole filesystem, not the scan root.
#[allow(clippy::too_many_arguments)]
fn write_history_snapshot(
    db_path: &std::path::Path,
    scan_path: &str,
    team_map: &std::collections::HashMap<String, i64>,
    team_names: &std::collections::HashMap<i64, String>,
    timestamp: i64,
    purge_days: Option<i64>,
    inodes_scanned: i64,
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

    let fs = statvfs_meta(scan_path);
    let scanned_size: i64 = conn
        .query_row("SELECT COALESCE(SUM(total_size), 0) FROM detail_users", [], |r| r.get(0))
        .unwrap_or(0);
    let meta = SnapshotMeta {
        path: scan_path.to_string(),
        total: fs.total,
        used: fs.used,
        available: fs.available,
        scanned_size,
        inodes_total: fs.inodes_total,
        inodes_used: fs.inodes_used,
        inodes_free: fs.inodes_free,
        inodes_scanned,
    };

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
    configure_mimalloc();
    let cli = Cli::parse();
    let mut cfg = Config::load();

    // No subcommand: launch the interactive config TUI.
    let command = match &cli.command {
        Some(c) => c,
        None => {
            if let Err(e) = config_tui::run(cfg) {
                eprintln!("config TUI error: {}", e);
                std::process::exit(1);
            }
            return;
        }
    };

    match command {
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
                    .map(|t| {
                        // Unassigned users found in the report (team_id empty),
                        // mirroring the "Other" bucket the dashboard shows.
                        let report = std::path::Path::new(&cfg.resolved_output_dir())
                            .join(&t.name).join("report.db");
                        let other_users: Vec<serde_json::Value> = query_report_users(&report)
                            .into_iter()
                            .filter(|u| !u.has_team)
                            .map(|u| serde_json::json!({"name": u.username, "size": u.size}))
                            .collect();
                        serde_json::json!({
                            "name": t.name,
                            "path": t.path,
                            "end_scan": t.end_scan,
                            "purge_time": t.purge_time,
                            "teams": t.teams.iter().map(|tm| serde_json::json!({"name": tm.name, "team_id": tm.team_id})).collect::<Vec<_>>(),
                            "users": t.users.iter()
                                .filter(|u| team.as_ref().map(|tn| t.teams.iter().any(|tm| tm.name == *tn && tm.team_id == u.team_id)).unwrap_or(true))
                                .map(|u| serde_json::json!({"name": u.name, "team_id": u.team_id})).collect::<Vec<_>>(),
                            "other_users": other_users,
                        })
                    })
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
                        // Unassigned users found in the report (team_id empty)
                        // are shown as "Other" — the same bucket the dashboard
                        // uses. Reads the report directly so it works whether or
                        // not those users were configured before the scan.
                        if team.is_none() {
                            let report = std::path::Path::new(&cfg.resolved_output_dir())
                                .join(&t.name).join("report.db");
                            let others: Vec<ReportUser> = query_report_users(&report)
                                .into_iter()
                                .filter(|u| !u.has_team)
                                .collect();
                            if !others.is_empty() {
                                println!("\nOther (unassigned): {} users", others.len());
                                for u in others {
                                    println!("  {:>12}  {}", crate::fmt_size(u.size), u.username);
                                }
                            } else if report.exists() {
                                println!("\nOther (unassigned): 0 users");
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
        Command::Run { output_dir, tree_map, workers, level, target, debug } => {
            run_scan(&mut cfg, output_dir.clone(), *tree_map, *workers, *level, target, *debug);
        }
        Command::Status { output_dir, target, watch, json } => {
            let out = output_dir.clone().unwrap_or_else(|| cfg.resolved_output_dir());
            show_status(&cfg, &out, target.as_deref(), *watch, *json);
        }
        Command::Detail { user, output_dir, top, target, json, section, search } => {
            let out = output_dir.clone().unwrap_or_else(|| cfg.resolved_output_dir());
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

                        match section.as_str() {
                            // Permission issues for the user (from the merged perm_issues table).
                            "permission" => {
                                if detail_query_permission(&conn, username, *top, search.as_deref(),
                                    &tname, *json, &mut json_out) { found = true; }
                            }
                            // Per-directory file-count (inode) breakdown, sorted by file count.
                            "inode" => {
                                if detail_query_inode(&conn, &prefix, username, *top,
                                    &tname, *json, &mut json_out) { found = true; }
                            }
                            // Default: size report (top dirs/files by size).
                            _ => {
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
                    }
                }
                if !found && !*json {
                    match section.as_str() {
                        "permission" => println!("No permission issues for user '{}'.", username),
                        "inode" => println!("No inode data for user '{}'.", username),
                        _ => println!("User '{}' not found.", username),
                    }
                }
            }
            if *json {
                println!("{}", serde_json::to_string_pretty(&serde_json::json!(json_out)).unwrap_or_else(|_| "[]".into()));
            }
        }
        Command::TreeShow { output_dir, level, limit, path, search, target } => {
            let out = output_dir.clone().unwrap_or_else(|| cfg.resolved_output_dir());
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
            let out = output_dir.clone().unwrap_or_else(|| cfg.resolved_output_dir());
            let exp = export_dir.clone().unwrap_or_else(|| "exports".into());

            if let Ok(entries) = std::fs::read_dir(&out) {
                for entry in entries.flatten() {
                    let tname = entry.file_name().to_string_lossy().to_string();
                    if let Some(t) = target {
                        if &tname != t { continue; }
                    }
                    let db_path = entry.path().join("report.db");
                    if !db_path.exists() { continue; }
                    let tgt_dir = std::path::Path::new(&exp).join(&tname);
                    // `--user` may name several users; None (empty) means all.
                    let only: Vec<Option<&str>> = if user.is_empty() {
                        vec![None]
                    } else {
                        user.iter().map(|u| Some(u.as_str())).collect()
                    };
                    for who in only {
                        match export_target_users(&db_path, &tgt_dir, who) {
                            Ok(n) => println!("Exported {} user(s) from '{}' -> {}", n, tname, tgt_dir.display()),
                            Err(e) => eprintln!("Export '{}': {}", tname, e),
                        }
                    }
                }
            }
        }
        Command::Notify { webhook_url, output_dir, target } => {
            let out = output_dir.clone().unwrap_or_else(|| cfg.resolved_output_dir());
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
        Command::Sync { output_dir, host, dest_dir, user, pass } => {
            let out = output_dir.clone().unwrap_or_else(|| cfg.resolved_output_dir());
            match run_rsync(&out, host, dest_dir, user.as_deref(), *pass) {
                Ok(remote) => println!("Synced '{}' -> {}", out, remote),
                Err(e) => eprintln!("{}", e),
            }
        }
        Command::History { output_dir, target, days, json, compare, top } => {
            let out = output_dir.clone().unwrap_or_else(|| cfg.resolved_output_dir());
            if *compare {
                show_history_compare(&out, target.as_deref(), *days, *top, *json);
            } else {
                show_history(&out, target.as_deref(), *days, *json);
            }
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
/// One history snapshot for a target, with its top users. Shared by the CLI
/// `history` command and the config-TUI Output tab.
#[derive(Clone)]
pub struct HistorySnapshot {
    pub scan_date: i64,
    pub path: String,
    pub total: i64,
    pub used: i64,
    pub available: i64,
    pub scanned_size: i64,
    pub top_users: Vec<(String, i64)>,
}

/// Per-user detail for one target. Shared by the CLI `detail` command and the
/// config-TUI Output tab.
#[derive(Clone)]
pub struct UserDetail {
    pub uid: i64,
    pub total_files: i64,
    pub total_dirs: i64,
    pub total_size: i64,
    pub top_dirs: Vec<(i64, String)>,
    pub top_files: Vec<(i64, String)>,
}

/// One directory node in the treemap. Shared by the TUI treemap browser.
#[derive(Clone)]
pub struct TreeEntry {
    pub id: i64,
    pub name: String,
    pub size: i64,
    pub file_count: i64,
}

/// Resolve a target's report.db path: `<out>/<target>/report.db` if it exists.
pub fn resolve_report_db(out: &str, target: &str) -> Option<std::path::PathBuf> {
    let p = std::path::Path::new(out).join(target).join("report.db");
    if p.exists() { Some(p) } else { None }
}

/// Query up to `days` newest snapshots (with top users) from a target's
/// report.db. Empty vec if the DB is missing or has no history.
pub fn query_history(db: &std::path::Path, days: i64) -> Vec<HistorySnapshot> {
    let Ok(conn) = rusqlite::Connection::open(db) else { return Vec::new() };
    query_history_conn(&conn, days)
}

/// Same as `query_history` but reuses an open connection.
pub fn query_history_conn(conn: &rusqlite::Connection, days: i64) -> Vec<HistorySnapshot> {
    let mut stmt = match conn.prepare(
        "SELECT h.id, h.scan_date, h.path, h.total, h.used, h.available, \
         COALESCE(h.scanned_size, 0), uu.name, uu.size \
         FROM hist_snapshots h \
         LEFT JOIN hist_user_usage uu ON uu.snapshot_id = h.id AND uu.kind = 'user' \
         WHERE h.id IN (SELECT id FROM hist_snapshots ORDER BY scan_date DESC LIMIT ?1) \
         ORDER BY h.scan_date DESC, uu.size DESC",
    ) { Ok(s) => s, Err(_) => return Vec::new() };
    let rows: Vec<(i64, i64, String, i64, i64, i64, i64, Option<String>, Option<i64>)> =
        stmt.query_map(rusqlite::params![days.max(1)], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?, r.get::<_, String>(2)?,
                r.get::<_, i64>(3)?, r.get::<_, i64>(4)?, r.get::<_, i64>(5)?,
                r.get::<_, i64>(6)?,
                r.get::<_, Option<String>>(7)?, r.get::<_, Option<i64>>(8)?))
        }).map(|rows| rows.flatten().collect()).unwrap_or_default();
    let mut snaps: Vec<HistorySnapshot> = Vec::new();
    let mut current: Option<HistorySnapshot> = None;
    for (_id, scan_date, path, total, used, available, scanned_size, uname, usize) in rows {
        if current.as_ref().map(|s: &HistorySnapshot| s.scan_date != scan_date).unwrap_or(true) {
            if let Some(s) = current.take() { snaps.push(s); }
            current = Some(HistorySnapshot { scan_date, path, total, used, available, scanned_size, top_users: Vec::new() });
        }
        if let (Some(name), Some(size)) = (uname, usize) {
            current.as_mut().unwrap().top_users.push((name, size));
        }
    }
    if let Some(s) = current.take() { snaps.push(s); }
    snaps
}

/// Query one user's detail (totals + top dirs/files) from a report.db.
pub fn query_user_detail(db: &std::path::Path, username: &str, top: usize) -> Option<UserDetail> {
    let conn = rusqlite::Connection::open(db).ok()?;
    query_user_detail_conn(&conn, username, top)
}

/// Same as `query_user_detail` but reuses an open connection.
pub fn query_user_detail_conn(conn: &rusqlite::Connection, username: &str, top: usize) -> Option<UserDetail> {
    let prefix = detail_prefix(conn);
    let sql = format!("SELECT uid, total_files, total_dirs, total_size FROM {}users WHERE username = ?1", prefix);
    let mut stmt = conn.prepare(&sql).ok()?;
    let (uid, total_files, total_dirs, total_size) = stmt.query_row([username], |r| {
        Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?, r.get::<_, i64>(2)?, r.get::<_, i64>(3)?))
    }).ok()?;
    let top_dirs = query_top(conn, &format!(
        "SELECT d.size, d.path FROM {}dirs d WHERE d.uid = ?1 ORDER BY d.size DESC LIMIT ?2", prefix), uid, top);
    let top_files = query_top(conn, &format!(
        "SELECT f.size, n.name FROM {}files f JOIN {}file_names n ON f.name_id = n.id \
         WHERE f.uid = ?1 ORDER BY f.size DESC LIMIT ?2", prefix, prefix), uid, top);
    Some(UserDetail { uid, total_files, total_dirs, total_size, top_dirs, top_files })
}

/// One user row for the Output/Detail list, read straight from the scan's
/// report.db (every uid the scan saw), not from config. `has_team` is false
/// when the user was not in any configured team — those are the "Other" users,
/// matching legacy which buckets unassigned users under Other.
#[derive(Clone)]
pub struct ReportUser {
    pub username: String,
    pub size: i64,
    pub has_team: bool,
}

/// All users recorded in a report.db, configured-team users first (largest
/// first), then the unassigned "Other" users (largest first). Empty when the
/// DB is missing or has no detail_users.
pub fn query_report_users(db: &std::path::Path) -> Vec<ReportUser> {
    let Ok(conn) = rusqlite::Connection::open(db) else { return Vec::new() };
    query_report_users_conn(&conn)
}

/// Same as `query_report_users` but reuses an open connection.
pub fn query_report_users_conn(conn: &rusqlite::Connection) -> Vec<ReportUser> {
    let prefix = detail_prefix(conn);
    let sql = format!("SELECT username, total_size, team_id FROM {}users", prefix);
    let Ok(mut stmt) = conn.prepare(&sql) else { return Vec::new() };
    let rows = stmt.query_map([], |r| {
        let username: String = r.get(0)?;
        let size: i64 = r.get(1)?;
        let team_id: String = r.get::<_, Option<String>>(2)?.unwrap_or_default();
        Ok(ReportUser { username, size, has_team: !team_id.trim().is_empty() })
    });
    let mut users: Vec<ReportUser> = rows.map(|it| it.flatten().collect()).unwrap_or_default();
    users.sort_by(|a, b| b.has_team.cmp(&a.has_team).then(b.size.cmp(&a.size)));
    users
}

/// Filter `all_users` to only those who have entries in `perm_issues`.
/// Returns them in the same order as `all_users` (team users first, then Other).
pub fn filter_users_with_perm_issues(conn: &rusqlite::Connection, all_users: &[ReportUser]) -> Vec<ReportUser> {
    let mut has_issue: std::collections::HashSet<String> = std::collections::HashSet::new();
    if let Ok(mut stmt) = conn.prepare("SELECT DISTINCT user FROM perm_issues") {
        if let Ok(rows) = stmt.query_map([], |r| r.get::<_, String>(0)) {
            for name in rows.flatten() { has_issue.insert(name); }
        }
    }
    if has_issue.is_empty() { return Vec::new(); }
    all_users.iter().filter(|u| has_issue.contains(&u.username)).cloned().collect()
}

/// One permission issue row (Type / Error / Path). Shared by the TUI Output tab.
#[derive(Clone)]
pub struct PermIssue {
    pub item_type: String,
    pub error: String,
    pub path: String,
}

/// A user's permission issues from a report.db (total + top rows). Returns
/// `(total, rows)`; empty when the DB has no perm_issues table or none match.
pub fn query_user_permissions(db: &std::path::Path, user: &str, top: usize) -> (i64, Vec<PermIssue>) {
    let Ok(conn) = rusqlite::Connection::open(db) else { return (0, Vec::new()) };
    query_user_permissions_conn(&conn, user, top)
}

/// Same as `query_user_permissions` but reuses an open connection.
pub fn query_user_permissions_conn(conn: &rusqlite::Connection, user: &str, top: usize) -> (i64, Vec<PermIssue>) {
    if !has_perm_issues(conn) { return (0, Vec::new()); }
    let total: i64 = conn.query_row(
        "SELECT COUNT(*) FROM perm_issues WHERE user = ?1",
        rusqlite::params![user], |r| r.get(0)).unwrap_or(0);
    let rows = {
        let Ok(mut stmt) = conn.prepare(
            "SELECT item_type, error, path FROM perm_issues WHERE user = ?1 ORDER BY id LIMIT ?2")
        else { return (total, Vec::new()); };
        stmt.query_map(rusqlite::params![user, top as i64], |r| {
            Ok(PermIssue { item_type: r.get(0)?, error: r.get(1)?, path: r.get(2)? })
        }).map(|it| it.flatten().collect()).unwrap_or_default()
    };
    (total, rows)
}

/// A per-directory file-count row (files, size, path). Shared by the TUI Output tab.
#[derive(Clone)]
pub struct InodeDir {
    pub files: i64,
    pub size: i64,
    pub path: String,
}

/// A user's per-directory file-count breakdown, sorted by file count desc.
/// Returns `(total_files, total_dirs, rows)`; empty rows when the user is absent.
pub fn query_user_inode(db: &std::path::Path, user: &str, top: usize) -> (i64, i64, Vec<InodeDir>) {
    let Ok(conn) = rusqlite::Connection::open(db) else { return (0, 0, Vec::new()) };
    query_user_inode_conn(&conn, user, top)
}

/// Same as `query_user_inode` but reuses an open connection.
pub fn query_user_inode_conn(conn: &rusqlite::Connection, user: &str, top: usize) -> (i64, i64, Vec<InodeDir>) {
    let prefix = detail_prefix(conn);
    let sql = format!("SELECT uid, total_files, total_dirs FROM {}users WHERE username = ?1", prefix);
    let (uid, tf, td): (i64, i64, i64) = match conn.query_row(
        &sql, rusqlite::params![user], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))) {
        Ok(v) => v, Err(_) => return (0, 0, Vec::new()),
    };
    let dsql = format!(
        "SELECT files, size, path FROM {}dirs WHERE uid = ?1 ORDER BY files DESC, size DESC LIMIT ?2", prefix);
    let rows = {
        let Ok(mut stmt) = conn.prepare(&dsql) else { return (tf, td, Vec::new()); };
        stmt.query_map(rusqlite::params![uid, top as i64], |r| {
            Ok(InodeDir { files: r.get(0)?, size: r.get(1)?, path: r.get(2)? })
        }).map(|it| it.flatten().collect()).unwrap_or_default()
    };
    (tf, td, rows)
}

/// Whether the merged report.db has the permission-issues table.
fn has_perm_issues(conn: &rusqlite::Connection) -> bool {
    conn.prepare("SELECT 1 FROM perm_issues LIMIT 1").is_ok()
}

/// Permission-issues section of `detail`: list a user's permission errors from
/// the merged `perm_issues` table (Type / Error / Path), optionally filtered by
/// a path substring. Returns true if any issue was printed/collected.
fn detail_query_permission(
    conn: &rusqlite::Connection,
    user: &str,
    top: usize,
    search: Option<&str>,
    tname: &str,
    json: bool,
    json_out: &mut Vec<serde_json::Value>,
) -> bool {
    if !has_perm_issues(conn) { return false; }
    // Total + rows, with an optional case-insensitive path LIKE filter.
    let (total, rows): (i64, Vec<(String, String, String)>) = if let Some(kw) = search {
        let pat = format!("%{}%", kw);
        let total: i64 = conn.query_row(
            "SELECT COUNT(*) FROM perm_issues WHERE user = ?1 AND path LIKE ?2 COLLATE NOCASE",
            rusqlite::params![user, pat], |r| r.get(0)).unwrap_or(0);
        let mut stmt = match conn.prepare(
            "SELECT item_type, error, path FROM perm_issues \
             WHERE user = ?1 AND path LIKE ?2 COLLATE NOCASE ORDER BY id LIMIT ?3") {
            Ok(s) => s, Err(_) => return false,
        };
        let rows = stmt.query_map(rusqlite::params![user, pat, top as i64], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?))
        }).map(|it| it.flatten().collect()).unwrap_or_default();
        (total, rows)
    } else {
        let total: i64 = conn.query_row(
            "SELECT COUNT(*) FROM perm_issues WHERE user = ?1",
            rusqlite::params![user], |r| r.get(0)).unwrap_or(0);
        let mut stmt = match conn.prepare(
            "SELECT item_type, error, path FROM perm_issues WHERE user = ?1 ORDER BY id LIMIT ?2") {
            Ok(s) => s, Err(_) => return false,
        };
        let rows = stmt.query_map(rusqlite::params![user, top as i64], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?))
        }).map(|it| it.flatten().collect()).unwrap_or_default();
        (total, rows)
    };

    if total == 0 { return false; }

    if json {
        json_out.push(serde_json::json!({
            "user": user, "target": tname, "section": "permission", "total": total,
            "issues": rows.iter().map(|(t, e, p)|
                serde_json::json!({"type": t, "error": e, "path": p})).collect::<Vec<_>>(),
        }));
    } else {
        println!("\n=== permission issues: {} on {} ===", user, tname);
        println!("  Total: {}", total);
        if let Some(kw) = search { println!("  Search: '{}'", kw); }
        println!("  {:<10}  {:<22}  {}", "Type", "Error", "Path");
        println!("  {}", "─".repeat(72));
        for (t, e, p) in &rows {
            println!("  {:<10}  {:<22}  {}", t, e, p);
        }
    }
    true
}

/// Inode section of `detail`: per-directory file-count breakdown for a user,
/// sorted by file count (largest first). Uses the `files` column already in the
/// detail dirs table. Returns true if any dir row was printed/collected.
fn detail_query_inode(
    conn: &rusqlite::Connection,
    prefix: &str,
    user: &str,
    top: usize,
    tname: &str,
    json: bool,
    json_out: &mut Vec<serde_json::Value>,
) -> bool {
    // Resolve uid + totals for the user.
    let sql = format!("SELECT uid, total_files, total_dirs FROM {}users WHERE username = ?1", prefix);
    let (uid, total_files, total_dirs): (i64, i64, i64) = match conn.query_row(
        &sql, rusqlite::params![user], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))) {
        Ok(v) => v, Err(_) => return false,
    };
    // Per-dir file count, largest first.
    let dsql = format!(
        "SELECT files, size, path FROM {}dirs WHERE uid = ?1 ORDER BY files DESC, size DESC LIMIT ?2", prefix);
    let mut stmt = match conn.prepare(&dsql) { Ok(s) => s, Err(_) => return false };
    let dirs: Vec<(i64, i64, String)> = stmt.query_map(rusqlite::params![uid, top as i64], |r| {
        Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?, r.get::<_, String>(2)?))
    }).map(|it| it.flatten().collect()).unwrap_or_default();

    if json {
        json_out.push(serde_json::json!({
            "user": user, "target": tname, "section": "inode", "uid": uid,
            "total_files": total_files, "total_dirs": total_dirs,
            "dirs": dirs.iter().map(|(f, s, p)|
                serde_json::json!({"files": f, "size": s, "path": p})).collect::<Vec<_>>(),
        }));
    } else {
        println!("\n=== inode report: {} on {} ===", user, tname);
        println!("  Total files: {}   Total dirs: {}", total_files, total_dirs);
        println!("  {:>10}  {:>10}  {}", "Files", "Size", "Directory");
        println!("  {}", "─".repeat(72));
        for (f, s, p) in &dirs {
            println!("  {:>10}  {:>10}  {}", f, fmt_size(*s), p);
        }
    }
    true
}

/// Detect the treemap table prefix, or None if this report.db has no treemap.
pub fn treemap_prefix(conn: &rusqlite::Connection) -> Option<&'static str> {
    if conn.prepare("SELECT 1 FROM treemap_dirs LIMIT 1").is_ok() { Some("treemap_") }
    else if conn.prepare("SELECT 1 FROM dirs LIMIT 1").is_ok() { Some("") }
    else { None }
}

/// Root directory id of the treemap (the node with no parent).
pub fn treemap_root(conn: &rusqlite::Connection, tp: &str) -> Option<i64> {
    conn.query_row(&format!("SELECT id FROM {}dirs WHERE parent_id IS NULL LIMIT 1", tp), [], |r| r.get(0)).ok()
}

/// Direct children of a directory node, largest first, capped at `limit`.
pub fn treemap_children(conn: &rusqlite::Connection, tp: &str, dir_id: i64, limit: usize) -> Vec<TreeEntry> {
    conn.prepare(&format!(
        "SELECT d.id, n.name, d.total_size, d.file_count FROM {}dirs d \
         JOIN {}names n ON d.name_id = n.id WHERE d.parent_id = ?1 \
         ORDER BY d.total_size DESC LIMIT ?2", tp, tp))
        .and_then(|mut s| {
            s.query_map(rusqlite::params![dir_id, limit as i64], |r| {
                Ok(TreeEntry { id: r.get(0)?, name: r.get(1)?, size: r.get(2)?, file_count: r.get(3)? })
            }).map(|rows| rows.flatten().collect())
        })
        .unwrap_or_default()
}

/// Name + total size of a treemap node.
pub fn treemap_node(conn: &rusqlite::Connection, tp: &str, dir_id: i64) -> (String, i64) {
    let name = conn.query_row(
        &format!("SELECT COALESCE(n.name, '<root>') FROM {}dirs d LEFT JOIN {}names n ON d.name_id = n.id WHERE d.id = ?1", tp, tp),
        rusqlite::params![dir_id], |r| r.get::<_, String>(0)).unwrap_or_else(|_| "<root>".into());
    let size = conn.query_row(
        &format!("SELECT total_size FROM {}dirs WHERE id = ?1", tp),
        rusqlite::params![dir_id], |r| r.get::<_, i64>(0)).unwrap_or(0);
    (name, size)
}

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

/// Growth metrics for one user across a chronological series of usage values.
/// Mirrors legacy report_comparison: abs = last − first-nonzero, pct relative to
/// that first value, trend from consecutive-diff direction (needs ≥3 points).
struct Growth {
    abs: i64,
    pct: Option<f64>,
    trend: &'static str,
}

fn compute_growth(values: &[i64]) -> Growth {
    if values.len() < 2 {
        return Growth { abs: 0, pct: None, trend: "-" };
    }
    // First non-zero value is the baseline (legacy behavior).
    let mut first = values[0];
    if first == 0 {
        if let Some(&v) = values.iter().find(|&&v| v > 0) { first = v; }
    }
    let last = *values.last().unwrap();
    let abs = last - first;
    let pct = if first > 0 { Some(abs as f64 / first as f64 * 100.0) } else { None };
    Growth { abs, pct, trend: trend_indicator(values) }
}

/// Trend arrow from consecutive diffs: ^ mostly up, v mostly down, ~ mixed,
/// - stable / too few points. Needs ≥3 points like legacy.
fn trend_indicator(values: &[i64]) -> &'static str {
    if values.len() < 3 { return "-"; }
    if values.iter().all(|&v| v == values[0]) { return "-"; }
    let diffs: Vec<i64> = values.windows(2).map(|w| w[1] - w[0]).collect();
    let pos = diffs.iter().filter(|&&d| d > 0).count();
    let neg = diffs.iter().filter(|&&d| d < 0).count();
    let n = diffs.len() as f64;
    if pos as f64 > n * 0.7 { "^" }
    else if neg as f64 > n * 0.7 { "v" }
    else { "~" }
}

/// `history --compare`: per-user growth/trend across the last `days` snapshots.
/// Builds a transposed table — one row per user, one column per scan date
/// (chronological), followed by Abs Growth / % Growth / Trend. Users are ranked
/// by absolute growth and capped at `top`.
fn show_history_compare(out: &str, target: Option<&str>, days: i64, top: usize, json: bool) {
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

        // Dates oldest→newest (chronological) so growth reads left-to-right.
        let dates: Vec<(i64, i64)> = {
            let Ok(mut stmt) = conn.prepare(
                "SELECT id, scan_date FROM (SELECT id, scan_date FROM hist_snapshots \
                 ORDER BY scan_date DESC LIMIT ?1) ORDER BY scan_date ASC",
            ) else { continue };
            stmt.query_map(rusqlite::params![days.max(1)], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?))
            }).map(|rows| rows.flatten().collect()).unwrap_or_default()
        };
        if dates.len() < 2 { continue; }

        // Per-user usage series aligned to `dates` (0 where a user is absent
        // that day). Collected user-first for easy growth computation.
        let mut series: std::collections::BTreeMap<String, Vec<i64>> = std::collections::BTreeMap::new();
        for (col, (snap_id, _)) in dates.iter().enumerate() {
            let rows = {
                let Ok(mut stmt) = conn.prepare(
                    "SELECT name, size FROM hist_user_usage WHERE snapshot_id = ?1 AND kind = 'user'",
                ) else { continue };
                let v: Vec<(String, i64)> = stmt.query_map(rusqlite::params![snap_id], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
                }).map(|rows| rows.flatten().collect()).unwrap_or_default();
                v
            };
            for (name, size) in rows {
                let e = series.entry(name).or_insert_with(|| vec![0; dates.len()]);
                e[col] = size;
            }
        }
        if series.is_empty() { continue; }
        any = true;

        // Rank users by absolute growth, keep top N.
        let mut ranked: Vec<(String, Vec<i64>, Growth)> = series.into_iter()
            .map(|(name, vals)| { let g = compute_growth(&vals); (name, vals, g) })
            .collect();
        ranked.sort_by(|a, b| b.2.abs.cmp(&a.2.abs));
        ranked.truncate(top.max(1));

        emit_history_compare(&tname, &dates, &ranked, json, &mut json_targets);
    }

    if json {
        println!("{}", serde_json::to_string_pretty(&serde_json::json!(json_targets)).unwrap_or_else(|_| "[]".into()));
    } else if !any {
        println!("No comparable history under '{}' (need ≥2 snapshots).", out);
    }
}

/// Render one target's compare table (text) or push its JSON object.
fn emit_history_compare(
    tname: &str,
    dates: &[(i64, i64)],
    ranked: &[(String, Vec<i64>, Growth)],
    json: bool,
    json_targets: &mut Vec<serde_json::Value>,
) {
    if json {
        let users: Vec<serde_json::Value> = ranked.iter().map(|(name, vals, g)| {
            serde_json::json!({
                "user": name,
                "usage": dates.iter().zip(vals).map(|((_, d), v)|
                    serde_json::json!({"date": d, "size": v})).collect::<Vec<_>>(),
                "abs_growth": g.abs,
                "pct_growth": g.pct,
                "trend": g.trend,
            })
        }).collect();
        json_targets.push(serde_json::json!({
            "target": tname,
            "dates": dates.iter().map(|(_, d)| *d).collect::<Vec<_>>(),
            "users": users,
        }));
        return;
    }

    println!("\n=== History comparison: {} ({} snapshots) ===", tname, dates.len());
    // Header: user column + one column per date + growth columns.
    let uw = ranked.iter().map(|(n, _, _)| n.len()).max().unwrap_or(4).max(4);
    print!("  {:<width$}", "User", width = uw);
    for (_, d) in dates { print!("  {:>10}", fmt_date_short(*d)); }
    println!("  {:>10}  {:>7}  {}", "Abs", "%", "Trend");
    for (name, vals, g) in ranked {
        print!("  {:<width$}", name, width = uw);
        for v in vals { print!("  {:>10}", fmt_size(*v)); }
        let pct = match g.pct { Some(p) => format!("{:+.1}%", p), None => "N/A".into() };
        println!("  {:>10}  {:>7}  {}", fmt_size(g.abs), pct, g.trend);
    }
}

/// yyyymmdd → MM-DD (compact column header).
fn fmt_date_short(d: i64) -> String {
    format!("{:02}-{:02}", (d / 100) % 100, d % 100)
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
pub fn parse_team_specs(raw: &[String]) -> Result<Vec<config::TeamSpec>, String> {
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
        // Each comma-separated token is either a literal username or `@file`,
        // which loads usernames from a text file (split on newlines/commas/
        // whitespace, `#` starts a comment). Literals and files can be mixed.
        let mut users: Vec<String> = Vec::new();
        for tok in users_str.split(',') {
            let tok = tok.trim();
            if tok.is_empty() { continue; }
            if let Some(path) = tok.strip_prefix('@') {
                for u in read_user_list(path)? {
                    if !users.contains(&u) { users.push(u); }
                }
            } else if !users.contains(&tok.to_string()) {
                users.push(tok.to_string());
            }
        }
        specs.push(config::TeamSpec { name: name.to_string(), users });
    }
    Ok(specs)
}

/// Read a username list from a text file: usernames are separated by any of
/// newline, comma, or whitespace; blank lines and `#` comments are ignored.
/// Used by `--team NAME=@file` so large teams don't have to be typed inline.
pub fn read_user_list(path: &str) -> Result<Vec<String>, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("read user list '{}': {}", path, e))?;
    let mut out: Vec<String> = Vec::new();
    for line in text.lines() {
        let line = line.split('#').next().unwrap_or("");
        for tok in line.split([',', ' ', '\t']) {
            let tok = tok.trim();
            if !tok.is_empty() && !out.contains(&tok.to_string()) {
                out.push(tok.to_string());
            }
        }
    }
    Ok(out)
}

/// rsync a directory to a remote host over SSH (BatchMode, no prompts). Returns
/// the remote spec on success. Shared by the `sync` command and per-target
/// auto-sync after a scan. `src_dir` is synced as its own contents (trailing
/// slash), mirrored with --delete.
/// rsync a directory to a remote host over SSH. With `use_pass = false` (default)
/// SSH runs in BatchMode (key auth, no prompts). With `use_pass = true` the rsync
/// is wrapped in `sshpass -e`, which reads the password from the `SSHPASS` env
/// var — the password is never passed on the command line or stored in config.
/// Returns the resolved `user@host:dest` on success.
pub fn run_rsync(src_dir: &str, host: &str, dest_dir: &str, user: Option<&str>, use_pass: bool) -> Result<String, String> {
    let remote = match user {
        Some(u) => format!("{}@{}:{}", u, host, dest_dir),
        None => format!("{}:{}", host, dest_dir),
    };
    let src = format!("{}/", src_dir.trim_end_matches('/'));
    let status = if use_pass {
        if std::env::var_os("SSHPASS").is_none() {
            return Err("sync_pass is set but the SSHPASS env var is empty — export SSHPASS=<password> before running".into());
        }
        // sshpass -e rsync ... -e "ssh" : password auth, allow first-connect host key.
        std::process::Command::new("sshpass")
            .args(["-e", "rsync", "-az", "--delete", "-e",
                   "ssh -o StrictHostKeyChecking=accept-new", &src, &remote])
            .status()
    } else {
        std::process::Command::new("rsync")
            .args(["-az", "--delete", "-e", "ssh -o BatchMode=yes", &src, &remote])
            .status()
    };
    match status {
        Ok(s) if s.success() => Ok(remote),
        Ok(s) => Err(format!("rsync exited with {}", s)),
        Err(e) => Err(format!(
            "{} failed to start: {} (is {} installed?)",
            if use_pass { "sshpass" } else { "rsync" }, e,
            if use_pass { "sshpass" } else { "rsync" })),
    }
}

fn import_legacy_config(dir: &str, force: bool) -> Result<std::path::PathBuf, String> {
    let root = std::path::Path::new(dir);
    if !root.is_dir() {
        return Err(format!("'{}' is not a directory", dir));
    }
    let dest = Config::path();
    // Refuse to clobber an existing config: either duscan.toml or any per-target
    // file under targets/ counts as "already configured".
    let targets_dir = Config::targets_dir();
    let has_target_files = std::fs::read_dir(&targets_dir)
        .map(|mut it| it.any(|e| e.as_ref().map(|e| {
            e.path().extension().and_then(|s| s.to_str()) == Some("toml")
        }).unwrap_or(false)))
        .unwrap_or(false);
    if (dest.exists() || has_target_files) && !force {
        return Err(format!("config already exists ({} / {}) — use --force to overwrite",
            dest.display(), targets_dir.display()));
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
    // Per-target overrides (None = use the scan-wide default) + sync config.
    tree_map: Option<bool>,
    level: Option<i64>,
    workers: Option<i64>,
    sync_host: Option<String>,
    sync_dest_dir: Option<String>,
    sync_user: Option<String>,
    sync_pass: Option<bool>,
    webhook_url: Option<String>,
}

/// Live per-view progress sink shared between a worker thread and the TUI.
pub(crate) struct ViewProgress {
    pub name: String,
    pub scan: check_disk_core::scan_core::ScanProgress,
    /// Current coarse phase label (scanning/building/treemap/merging/history/done/error).
    pub phase: std::sync::Mutex<String>,
    pub error: std::sync::Mutex<String>,
    pub started: std::time::Instant,
    pub finished: std::sync::atomic::AtomicBool,
}

/// A running (or completed) background scan: the per-view progress sinks the
/// caller polls for live counts/phase, the worker join handles, and the shared
/// abort flag. Returned by `spawn_scan_workers` so a caller that owns its own
/// terminal (e.g. the config TUI) can render progress in-place without handing
/// the screen to the standalone scan monitor.
pub(crate) struct ScanRun {
    pub progresses: Vec<std::sync::Arc<ViewProgress>>,
    pub handles: Vec<std::thread::JoinHandle<()>>,
    pub abort: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl ScanRun {
    /// True once every view's worker has finished (or errored).
    pub fn all_finished(&self) -> bool {
        use std::sync::atomic::Ordering;
        self.progresses.iter().all(|p| p.finished.load(Ordering::SeqCst))
    }

    /// Signal every worker to stop at the next safe checkpoint. Sets the
    /// between-jobs abort flag AND each view's in-walk cancel flag, so an
    /// already-running scan bails out mid-directory instead of only between
    /// whole views — this keeps `join()` (and thus a TUI quit) from blocking
    /// for the remainder of a large tree.
    pub fn request_abort(&self) {
        self.abort.store(true, std::sync::atomic::Ordering::SeqCst);
        for p in &self.progresses {
            p.scan.request_cancel();
        }
    }

    /// Join all worker threads (blocks until they exit).
    pub fn join(&mut self) {
        for h in self.handles.drain(..) {
            let _ = h.join();
        }
    }
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
    debug: bool,
) {
    let (out, group_jobs, view_names, max_parallel_devices) =
        match plan_scan_jobs(cfg, output_dir, workers, target) {
            Some(v) => v,
            None => return,
        };

    std::fs::create_dir_all(&out).ok();
    run_scan_headless(&out, tree_map, level, max_parallel_devices, group_jobs, view_names, debug);
}

/// Spawn the background scan: create one `ViewProgress` sink per view, then one
/// worker thread per device group (roots within a group run sequentially). A
/// counting semaphore caps concurrent device groups and a global build lock
/// serializes the RAM-heavy Phase 2/3/merge/history so peak RSS stays at ~one
/// pipeline while Phase 1 scans stay parallel.
///
/// This owns no terminal — it returns a `ScanRun` the caller polls for live
/// progress. `run_scan_headless` drives it under the standalone monitor; the config
/// TUI drives it in-place.
pub(crate) fn spawn_scan_workers(
    out: &str,
    tree_map: bool,
    level: usize,
    max_parallel_devices: usize,
    group_jobs: Vec<(usize, Vec<ViewJob>)>,
    view_names: &[String],
    debug: bool,
) -> ScanRun {
    use std::sync::atomic::Ordering;
    use std::sync::{Arc, Mutex};
    use std::time::Instant;

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
                scan_one_view(&out, tree_map, level, group_workers, &job, &sink, &build_lock, debug);
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

    ScanRun { progresses, handles, abort }
}

/// Build the device-aware scan plan for `cfg` (restricted to `target` when
/// non-empty) and return the flattened per-group job lists plus the flat list of
/// view names in plan order. Shared by the standalone monitor (`run_scan`) and
/// the config TUI's in-place scan. Returns `None` when there is nothing to scan
/// (no matching targets, or all skipped by `end_scan`).
pub(crate) fn plan_scan_jobs(
    cfg: &mut Config,
    output_dir: Option<String>,
    workers: Option<usize>,
    target: &[String],
) -> Option<(String, Vec<(usize, Vec<ViewJob>)>, Vec<String>, usize)> {
    let out = output_dir.unwrap_or_else(|| cfg.resolved_output_dir());
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
            return None;
        }
    }

    let plan = build_scan_plan(cfg, budget, workers.is_some());

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
                    tree_map: view.tree_map,
                    level: view.level,
                    workers: view.workers,
                    sync_host: view.sync_host.clone(),
                    sync_dest_dir: view.sync_dest_dir.clone(),
                    sync_user: view.sync_user.clone(),
                    sync_pass: view.sync_pass,
                    webhook_url: view.webhook_url.clone(),
                });
            }
        }
        if !jobs.is_empty() {
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
        return None;
    }

    let max_parallel_devices = cfg.max_parallel_devices.max(0) as usize;
    Some((out, group_jobs, view_names, max_parallel_devices))
}

/// Own the terminal, spawn one worker thread per device group, and drive the
/// live TUI: poll each view's ScanProgress ~7×/s, compute rate/mem, render, and
/// watch for q / Ctrl+C to abort. Returns after all group threads join.
fn run_scan_headless(
    out: &str,
    tree_map: bool,
    level: usize,
    max_parallel_devices: usize,
    group_jobs: Vec<(usize, Vec<ViewJob>)>,
    view_names: Vec<String>,
    debug: bool,
) {
    use std::sync::atomic::Ordering;
    use std::time::{Duration, Instant};

    let run = spawn_scan_workers(out, tree_map, level, max_parallel_devices, group_jobs, &view_names, debug);
    let progresses = run.progresses.clone();
    let abort = run.abort.clone();
    let handles = run.handles;

    let abort_sig = abort.clone();
    let progresses_sig = progresses.clone();
    ctrlc::set_handler(move || {
        abort_sig.store(true, Ordering::SeqCst);
        for p in &progresses_sig {
            p.scan.request_cancel();
        }
    }).ok();

    println!("Scanning {} target(s)...", view_names.len());

    let mut last_phases: Vec<String> = view_names.iter().map(|_| String::new()).collect();
    let mut last_progress: Vec<Instant> = view_names.iter().map(|_| Instant::now()).collect();
    let mut last_counts: Vec<(u64, u64)> = view_names.iter().map(|_| (0, 0)).collect();
    let scan_start = Instant::now();

    loop {
        let all_finished = progresses.iter().all(|p| p.finished.load(Ordering::SeqCst));
        let mem_mb = check_disk_core::pipe_types::get_rss_mb();

        for (i, p) in progresses.iter().enumerate() {
            let phase = p.phase.lock().unwrap().clone();
            let (files, dirs, size) = p.scan.snapshot();
            let elapsed = p.started.elapsed().as_secs_f64();

            if phase != last_phases[i] {
                last_phases[i] = phase.clone();
                let size_str = fmt_size(size as i64);
                match phase.as_str() {
                    "scanning" => {
                        println!("[{}] Started scanning...", p.name);
                    }
                    "queued" => {
                        println!("[{}] Queued (waiting for build slot)...", p.name);
                    }
                    "building" => {
                        println!("[{}] Building detail DB... ({} files, {} dirs, {} | {:.1}s {:.0} MB)",
                            p.name, files, dirs, size_str, elapsed, mem_mb);
                    }
                    "treemap" => {
                        println!("[{}] Building treemap...", p.name);
                    }
                    "merging" => {
                        println!("[{}] Merging into report.db...", p.name);
                    }
                    "history" => {
                        println!("[{}] Writing history snapshot...", p.name);
                    }
                    "syncing" => {
                        println!("[{}] Syncing to remote...", p.name);
                    }
                    "error" => {
                        let err = p.error.lock().unwrap().clone();
                        eprintln!("[{}] ERROR: {}", p.name, err);
                    }
                    "done" => {
                        println!("[{}] Done: {} files, {} dirs, {} | {:.1}s {:.0} MB",
                            p.name, files, dirs, size_str, elapsed, mem_mb);
                    }
                    _ => {}
                }
                last_progress[i] = Instant::now();
                last_counts[i] = (files, dirs);
            } else if phase == "scanning" {
                let elapsed_since = last_progress[i].elapsed().as_secs_f64();
                if elapsed_since >= 2.0 && (files, dirs) != last_counts[i] {
                    let rate = if elapsed_since > 0.0 {
                        ((files - last_counts[i].0) as f64 / elapsed_since) as u64
                    } else {
                        0
                    };
                    println!("[{}] {} files, {} dirs | {:.1}s {:.0} MB ({} files/s)",
                        p.name, files, dirs, elapsed, mem_mb, rate);
                    last_progress[i] = Instant::now();
                    last_counts[i] = (files, dirs);
                }
            }
        }

        if all_finished {
            break;
        }

        if abort.load(Ordering::SeqCst) {
            println!("Aborting...");
            break;
        }

        std::thread::sleep(Duration::from_millis(500));
    }

    for h in handles {
        let _ = h.join();
    }

    let (tf, td): (u64, u64) = progresses.iter().fold((0, 0), |(f, d), p| {
        let (pf, pd, _) = p.scan.snapshot();
        (f + pf, d + pd)
    });
    let total_elapsed = scan_start.elapsed().as_secs_f64();
    if abort.load(Ordering::SeqCst) {
        println!("Scan aborted: {} files, {} dirs | {:.1}s (partial)", tf, td, total_elapsed);
    } else {
        println!("Scan complete: {} files, {} dirs | {:.1}s", tf, td, total_elapsed);
    }
}

/// Run the full pipeline for one target view: Phase 1 scan (feeding the live
/// progress sink), Phase 2 detail DB, optional treemap, merge into report.db,
/// and history snapshot. Updates the sink's phase label as it advances and
/// writes scan_status.json + a per-scan log. Never redirects stdout.
fn scan_one_view(
    out: &str,
    default_tree_map: bool,
    default_level: usize,
    group_workers: usize,
    job: &ViewJob,
    sink: &ViewProgress,
    build_lock: &std::sync::Arc<std::sync::Mutex<()>>,
    debug: bool,
) {
    use std::sync::atomic::Ordering;

    // Per-target overrides fall back to the scan-wide defaults when unset.
    let tree_map = job.tree_map.unwrap_or(default_tree_map);
    let level = job.level.map(|l| l.max(0) as usize).unwrap_or(default_level);
    let workers = job.workers.map(|w| w.max(1) as usize).unwrap_or(group_workers);

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
    write_scan_status(&target_out, "scanning", true, run_started, run_started, "Phase 1 scan", "", tree_map, 0, 0, 0);

    // Phase 1 — hand the engine our shared progress sink so the TUI sees live
    // counts. Because `run_scan_core` blocks, spawn a heartbeat thread that
    // snapshots the sink and rewrites scan_status.json every ~2s: this is what
    // lets `duscan status` show near-live file/dir counts even when the scan is
    // a separate process the in-memory sink can't reach.
    let progress = sink.scan.clone();
    let hb_stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let heartbeat = {
        let hb_stop = hb_stop.clone();
        let hb_sink = sink.scan.clone();
        let hb_dir = target_out.clone();
        std::thread::spawn(move || {
            while !hb_stop.load(Ordering::SeqCst) {
                // Sleep in short slices so stop is honored quickly, write ~every 2s.
                for _ in 0..20 {
                    if hb_stop.load(Ordering::SeqCst) { break; }
                    std::thread::sleep(std::time::Duration::from_millis(100));
                }
                if hb_stop.load(Ordering::SeqCst) { break; }
                let (f, d, s) = hb_sink.snapshot();
                write_scan_status(&hb_dir, "scanning", true, run_started, run_started, "Phase 1 scan", "", tree_map, f, d, s);
            }
        })
    };

    let phase1 = check_disk_core::scan_core::run_scan_core(
        dir_str.clone(), vec![], None, Some(workers), debug, "cli", None, Some(progress),
    );
    // Ensure the done flag is set regardless of engine internals, and stop the
    // heartbeat before writing any terminal status (so it can't overwrite it).
    sink.scan.done.store(true, Ordering::SeqCst);
    hb_stop.store(true, Ordering::SeqCst);
    let _ = heartbeat.join();

    let result = match phase1 {
        Ok(r) => r,
        Err(e) => {
            *sink.error.lock().unwrap() = e.to_string();
            set_phase("error");
            write_scan_status(&target_out, "error", false, run_started, run_started, "", &e.to_string(), tree_map, 0, 0, 0);
            eprintln!("Error scanning '{}': {}", job.name, e);
            return;
        }
    };

    let files = result["total_files"].as_u64().unwrap_or(0);
    let dirs = result["total_dirs"].as_u64().unwrap_or(0);
    let scanned_size = result["total_size"].as_u64().unwrap_or(0);
    let tmpdir = result["detail_tmpdir"].as_str().unwrap_or("").to_string();

    // The Phase-1 engine persists its scratch shards (scan_t*.bin, diragg_*,
    // perm_*, …) under `tmpdir` and detaches the TempDir, so nothing deletes it
    // automatically. This guard removes that directory on every exit path from
    // here on (cancel, Phase-2 error, or success), so repeated/cron'd scans no
    // longer leak gigabytes of temp files. Only the Phase-2 build reads it, and
    // that has finished (or been skipped) by the time this guard drops.
    struct TmpdirGuard(String);
    impl Drop for TmpdirGuard {
        fn drop(&mut self) {
            if !self.0.is_empty() {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
    }
    let _tmpdir_guard = TmpdirGuard(tmpdir.clone());

    // Cancellation only stops Phase 1's walk; the RAM-heavy Phase 2/3 build
    // below would still run to completion on the partial data (blocking a TUI
    // quit for the whole build). If a cancel was requested, stop here rather
    // than building a report from a truncated scan. The TmpdirGuard above still
    // removes the scratch shards on this early return.
    if sink.scan.is_cancelled() {
        set_phase("done");
        write_scan_status(&target_out, "cancelled", false, run_started, run_started, "Scan cancelled", "", tree_map, files, dirs, scanned_size);
        return;
    }

    if tmpdir.is_empty() {
        set_phase("done");
        write_scan_status(&target_out, "done", false, run_started, run_started, "Scan complete", "", tree_map, files, dirs, scanned_size);
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
    write_scan_status(&target_out, "queued", true, run_started, run_started, "Waiting for build slot", "", tree_map, files, dirs, scanned_size);
    let _build_guard = build_lock.lock().unwrap_or_else(|e| e.into_inner());

    let timestamp = now_epoch();
    let phase2_start = std::time::Instant::now();

    set_phase("building");
    write_scan_status(&target_out, "building", true, run_started, timestamp, "Phase 2 detail", "", tree_map, files, dirs, scanned_size);

    let build = check_disk_core::report_pipeline::build_detail_db_impl(
        tmpdir, uids_map, team_map,
        detail_db, treemap_db,
        dir_str.clone(), level, 0, timestamp,
        // `debug` was hard-coded false here, so `run --debug` never reached
        // Phase 2 and its [Phase 2 Profile] block could never print.
        1, tree_map, debug, job.prefix.clone(),
    );
    let (total, agg_opt) = match build {
        Ok(v) => v,
        Err(e) => {
            *sink.error.lock().unwrap() = e.clone();
            set_phase("error");
            write_scan_status(&target_out, "error", false, run_started, timestamp, "", &e, tree_map, files, dirs, scanned_size);
            eprintln!("Phase 2 error for '{}': {}", job.name, e);
            return;
        }
    };

    if debug {
        eprintln!("[stage] phase2 detail: {:.2}s", phase2_start.elapsed().as_secs_f64());
    }
    let treemap_start = std::time::Instant::now();

    if tree_map {
        if let Some(agg) = &agg_opt {
            set_phase("treemap");
            let tm_out = out_path.join("treemap.db");
            if let Err(e) = check_disk_core::report_pipeline::build_treemap_db_impl(
                agg, &tm_out, &dir_str, level, 0, timestamp, debug,
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

    if debug {
        eprintln!("[stage] phase3 treemap: {:.2}s", treemap_start.elapsed().as_secs_f64());
    }
    let merge_start = std::time::Instant::now();

    // Phase 1's inode count. Read once here because both the history snapshot and
    // the per-scan log want it.
    let total_inodes = result["total_inodes"].as_u64().unwrap_or(0);

    set_phase("merging");
    let merged = out_path.join("report.db");
    let merge_result = check_disk_core::db_writer::merge_into_single_db(
        &out_path, &merged, &job.name, &dir_str, timestamp,
    );
    if debug {
        eprintln!("[stage] merge: {:.2}s", merge_start.elapsed().as_secs_f64());
    }
    let merge_ok = merge_result.is_ok();
    if !merge_ok {
        eprintln!("Merge error for '{}': {:?}", job.name, merge_result);
    }
    if merge_ok {
        let _ = std::fs::remove_file(out_path.join("data_detail.db"));
        let _ = std::fs::remove_file(out_path.join("treemap.db"));
        let _ = std::fs::remove_file(out_path.join("report.tmp"));
        // perm_issues has been merged into report.db; drop the scratch sidecar so
        // the target dir holds a single source of truth (report.db + scan_status.json).
        let _ = std::fs::remove_file(out_path.join("permission_issues.db"));

        set_phase("history");
        let hist_start = std::time::Instant::now();
        if let Err(e) = write_history_snapshot(
            &merged, &dir_str, &job.team_map, &job.team_names, timestamp, job.purge_time,
            total_inodes as i64,
        ) {
            eprintln!("History error for '{}': {}", job.name, e);
        }
        if debug {
            eprintln!("[stage] history: {:.2}s", hist_start.elapsed().as_secs_f64());
        }
    }

    // Per-scan log (legacy-style summary banner).
    let logs_dir = out_path.join("logs");
    std::fs::create_dir_all(&logs_dir).ok();
    let log_name = chrono::Local::now().format("scan_%Y%m%d_%H%M%S.log").to_string();
    write_scan_log(
        &logs_dir.join(&log_name), &merged, &job.name, &dir_str,
        files, dirs,
        total_inodes,
        result["total_size"].as_i64().unwrap_or(0),
        result["permission_issues_count"].as_u64().unwrap_or(0),
        total, tree_map, merge_ok, phase2_start.elapsed().as_secs() as i64,
    );

    // Per-target auto-sync: if this target declares a sync host, mirror its own
    // output dir to the remote after a successful merge. Failures are logged but
    // don't fail the scan.
    if merge_ok {
        if let Some(host) = job.sync_host.as_deref() {
            if let Some(dest) = job.sync_dest_dir.as_deref() {
                set_phase("syncing");
                let src = out_path.to_string_lossy().to_string();
                let use_pass = job.sync_pass.unwrap_or(false);
                match run_rsync(&src, host, dest, job.sync_user.as_deref(), use_pass) {
                    Ok(remote) => eprintln!("Synced '{}' -> {}", job.name, remote),
                    Err(e) => eprintln!("Sync error for '{}': {}", job.name, e),
                }
            } else {
                eprintln!("Sync skipped for '{}': sync_host set but sync_dest_dir missing", job.name);
            }
        }

        // Per-target auto-notify: if this target declares a Teams webhook, send a
        // summary card after merge so a cron `duscan run` notifies on its own.
        if let Some(url) = job.webhook_url.as_deref() {
            let db_path = out_path.join("report.db");
            match send_teams_notification(url, &db_path, &job.name) {
                Ok(_) => eprintln!("Notified '{}' -> Teams webhook", job.name),
                Err(e) => eprintln!("Notify error for '{}': {}", job.name, e),
            }
        }
    }

    set_phase("done");
    write_scan_status(&target_out, "done", false, run_started, timestamp, "Scan complete", "", tree_map, files, dirs, scanned_size);
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
/// Export usage text files for users in a target's report.db into `export_dir`.
/// `only_user = Some(name)` exports just that user; None exports all. Returns the
/// number of users exported. Shared by the CLI `export` command and the TUI.
pub fn export_target_users(db: &std::path::Path, export_dir: &std::path::Path, only_user: Option<&str>) -> Result<usize, String> {
    let start = std::time::Instant::now();
    let conn = rusqlite::Connection::open(db).map_err(|e| format!("open {}: {}", db.display(), e))?;
    let mut stmt = conn.prepare("SELECT uid, username FROM detail_users")
        .map_err(|_| "no detail data in report.db — scan this target first".to_string())?;
    let users: Vec<(i64, String)> = stmt
        .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))
        .map(|rows| rows.flatten().collect())
        .unwrap_or_default();
    std::fs::create_dir_all(export_dir).map_err(|e| format!("mkdir {}: {}", export_dir.display(), e))?;
    let mut n = 0;
    for (uid, uname) in &users {
        if let Some(want) = only_user {
            if uname != want { continue; }
        }
        eprintln!("  Exporting '{}'...", uname);
        export_user_text(&conn, *uid, uname, export_dir);
        n += 1;
    }
    if n == 0 {
        return Err(match only_user {
            Some(u) => format!("user '{}' not found in report.db", u),
            None => "no users in report.db".to_string(),
        });
    }
    eprintln!("  Exported {} user(s) in {:.1}s", n, start.elapsed().as_secs_f64());
    Ok(n)
}

fn export_user_text(conn: &rusqlite::Connection, uid: i64, username: &str, export_dir: &std::path::Path) {
    use std::io::Write;
    let safe_user: String = username
        .chars()
        .map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect();
    let header = format!("{:<5} {:<20} {:>12}  {}\n{}\n", "Type", "User", "Size", "Path", "-".repeat(90));

    // Dirs.
    let dir_path = export_dir.join(format!("usage_dir_{}.txt", safe_user));
    let mut dir_rows: usize = 0;
    if let Ok(f) = std::fs::File::create(&dir_path) {
        let mut w = std::io::BufWriter::new(f);
        let _ = w.write_all(header.as_bytes());
        if let Ok(mut stmt) = conn.prepare(
            "SELECT size, path FROM detail_dirs WHERE uid = ?1 ORDER BY size DESC",
        ) {
            if let Ok(rows) = stmt.query_map(rusqlite::params![uid], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
            }) {
                for row in rows.flatten() {
                    let _ = writeln!(w, "{:<5} {:<20} {:>12}  {}", "dir", username, fmt_size(row.0), row.1);
                    dir_rows += 1;
                }
            }
        }
        let _ = w.flush();
    }

    // Files.
    let file_path = export_dir.join(format!("usage_file_{}.txt", safe_user));
    let mut file_rows: usize = 0;
    if let Ok(f) = std::fs::File::create(&file_path) {
        let mut w = std::io::BufWriter::new(f);
        let _ = w.write_all(header.as_bytes());
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
                    let _ = writeln!(w, "{:<5} {:<20} {:>12}  {}", "file", username, fmt_size(row.0), full);
                    file_rows += 1;
                }
            }
        }
        let _ = w.flush();
    }
    eprintln!("    usage_dir_{}.txt  — {} dirs", safe_user, dir_rows);
    eprintln!("    usage_file_{}.txt — {} files", safe_user, file_rows);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fmt_size_thresholds() {
        assert_eq!(fmt_size(0), "0 B");
        assert_eq!(fmt_size(512), "512 B");
        assert_eq!(fmt_size(1_500), "1.5 KB");
        assert_eq!(fmt_size(2_000_000), "2.0 MB");
        assert_eq!(fmt_size(3_000_000_000), "3.0 GB");
    }

    #[test]
    fn report_users_split_team_vs_other() {
        // detail_users rows with a team_id vs empty/NULL team_id must bucket
        // into has_team=true and has_team=false ("Other") respectively.
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE detail_users (username TEXT, total_size INTEGER, team_id TEXT);
             INSERT INTO detail_users VALUES
               ('alice', 1000, '1'),
               ('bob',   2000, '1'),
               ('carol', 3000, ''),
               ('dave',  4000, NULL);",
        )
        .unwrap();
        let users = query_report_users_conn(&conn);
        // Sorted team-first (by size), then Other (by size).
        let names: Vec<&str> = users.iter().map(|u| u.username.as_str()).collect();
        assert_eq!(names, vec!["bob", "alice", "dave", "carol"]);
        assert_eq!(users[0].has_team, true);
        assert_eq!(users[1].has_team, true);
        assert_eq!(users[2].has_team, false); // dave (NULL) -> Other
        assert_eq!(users[3].has_team, false); // carol ('') -> Other
    }

    #[test]
    fn fmt_date_pads() {
        assert_eq!(fmt_date(20260724), "2026-07-24");
        assert_eq!(fmt_date(20260101), "2026-01-01");
    }

    #[test]
    fn compute_growth_basic() {
        // Steady increase: abs = last - first, pct relative to first.
        let g = compute_growth(&[1000, 3000, 8000]);
        assert_eq!(g.abs, 7000);
        assert_eq!(g.pct, Some(700.0));
        assert_eq!(g.trend, "^");

        // Steady decrease.
        let g = compute_growth(&[9000, 6000, 2000]);
        assert_eq!(g.abs, -7000);
        assert_eq!(g.trend, "v");

        // Single point: no growth.
        let g = compute_growth(&[500]);
        assert_eq!(g.abs, 0);
        assert!(g.pct.is_none());
        assert_eq!(g.trend, "-");
    }

    #[test]
    fn compute_growth_zero_baseline_uses_first_nonzero() {
        // Leading zeros: baseline is the first non-zero value (2000).
        let g = compute_growth(&[0, 2000, 4000]);
        assert_eq!(g.abs, 4000 - 2000);
        assert_eq!(g.pct, Some(100.0));
    }

    #[test]
    fn trend_indicator_rules() {
        assert_eq!(trend_indicator(&[1, 2]), "-");            // <3 points
        assert_eq!(trend_indicator(&[5, 5, 5]), "-");         // stable
        assert_eq!(trend_indicator(&[1, 2, 3, 4]), "^");      // all up
        assert_eq!(trend_indicator(&[4, 3, 2, 1]), "v");      // all down
        assert_eq!(trend_indicator(&[1, 5, 2, 6]), "~");      // fluctuating
    }

    #[test]
    fn parse_team_specs_basic_and_errors() {
        let specs = parse_team_specs(&["dev=alice,bob".into(), "ops=carol".into()]).unwrap();
        assert_eq!(specs.len(), 2);
        assert_eq!(specs[0].name, "dev");
        assert_eq!(specs[0].users, vec!["alice".to_string(), "bob".to_string()]);
        assert_eq!(specs[1].users, vec!["carol".to_string()]);

        // Team with no users is allowed (empty user list).
        let specs = parse_team_specs(&["empty".into()]).unwrap();
        assert!(specs[0].users.is_empty());

        // Duplicate team name is rejected.
        assert!(parse_team_specs(&["dev=a".into(), "dev=b".into()]).is_err());
        // Empty team name is rejected.
        assert!(parse_team_specs(&["=a".into()]).is_err());
    }

    #[test]
    fn parse_team_specs_dedups_users() {
        let specs = parse_team_specs(&["dev=alice,alice,bob".into()]).unwrap();
        assert_eq!(specs[0].users, vec!["alice".to_string(), "bob".to_string()]);
    }
}
