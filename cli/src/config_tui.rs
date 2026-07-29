//! Interactive configuration TUI (opened by running `duscan` with no
//! subcommand). Manages targets, teams, and users across three tabs and writes
//! every change straight to disk via the existing `Config` API (each op calls
//! `save()`, so `duscan.toml` + `targets/*.toml` stay current). This is separate
//! from `ui.rs`, which is the read-only scan monitor.

use crossterm::event::{self, Event, KeyCode, KeyModifiers};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen};
use crossterm::ExecutableCommand;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Tabs};
use ratatui::{Frame, Terminal};
use std::cell::RefCell;

use crate::config::Config;

/// Which top-level tab is focused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tab {
    Targets,
    TeamsUsers,
    ScanSync,
    Output,
    Settings,
}

/// Which report view is shown on the Output tab.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OutputView {
    History,
    Detail,
    Permission,
    Inode,
    Treemap,
}

/// What kind of value the single-line input box is currently collecting, so the
/// commit handler knows what to do with the typed text.
#[derive(Debug, Clone)]
enum InputKind {
    NewTargetName,
    NewTargetPath { name: String },
    EditPath,
    EditEndScan,
    EditPurgeTime,
    NewTeam,
    AddUsers { team: String },
    SetOutputDir,
    SetWorkers,
    SetMaxParallel,
    SetNfsParallel,
    SetHddParallel,
    SetSsdParallel,
    // Per-target Scan/Sync tab fields.
    SetTargetLevel,
    SetTargetWorkers,
    SetSyncHost,
    SetSyncDest,
    SetSyncUser,
    SetExportDir,
    SetWebhookUrl,
    // LSF Settings tab fields.
    SetLsfEnabled,
    SetLsfCmd,
    SetLsfOs,
    SetLsfMemMb,
    SetLsfQueue,
}

/// Pending destructive action awaiting a yes/no confirmation.
#[derive(Debug, Clone)]
enum ConfirmAction {
    DeleteTarget(String),
    DeleteTeam(String),
    DeleteUser(String),
}

/// Interaction mode: browsing, typing into the input box, or confirming.
enum Mode {
    Browse,
    /// `completions` holds directory candidates shown below the box after a Tab
    /// on a path input; empty when there is nothing to show.
    Input { kind: InputKind, prompt: String, buf: String, completions: Vec<String> },
    Confirm { action: ConfirmAction, prompt: String },
}

/// LSF scan tracking: a background thread polls scan_status.json from shared
/// storage every ~2s; the TUI reads the latest snapshot without blocking.
struct LsfScanRun {
    /// Per-target progress snapshots, refreshed by the background poller.
    targets: std::sync::Arc<std::sync::Mutex<Vec<LsfTargetProgress>>>,
    /// Signal the poller to stop.
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// Join handle for the poller thread.
    handle: Option<std::thread::JoinHandle<()>>,
}

#[derive(Debug, Clone)]
struct LsfTargetProgress {
    name: String,
    phase: String,
    files: u64,
    dirs: u64,
    size_bytes: u64,
    error: String,
    elapsed_sec: f64,
    done: bool,
}

/// Whole-app state: the live config plus cursor positions per tab.
/// A txt export running on a background thread. The worker does the SQLite
/// reads + file writes (potentially minutes on a large NFS report) and reports
/// its outcome back through `done`; the event loop polls `done` each tick and
/// swaps the result into the footer status, so export never blocks rendering or
/// keystrokes. Mirrors the `ScanRun` background-work pattern.
struct ExportRun {
    /// Set by the worker when it exits, carrying the message to show in the
    /// footer (either the "Exported N …" success line or an "Export failed: …").
    done: std::sync::Arc<std::sync::Mutex<Option<String>>>,
    /// Join handle for the worker; joined once `done` is populated.
    handle: Option<std::thread::JoinHandle<()>>,
}

impl ExportRun {
    /// The finished status message, if the worker has exited; `None` while it is
    /// still running.
    fn take_result(&self) -> Option<String> {
        self.done.lock().ok().and_then(|mut g| g.take())
    }

    /// Join the worker thread (it has already exited by the time this is called).
    fn join(&mut self) {
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

struct App {
    cfg: Config,
    tab: Tab,
    mode: Mode,
    /// Selected index in the Targets list.
    target_sel: usize,
    /// Selected team index within the current target.
    team_sel: usize,
    /// Selected user index within the current team.
    user_sel: usize,
    /// Selected row on the Settings tab (0..4).
    settings_sel: usize,
    /// Selected row on the Scan/Sync tab (0..5).
    scansync_sel: usize,
    /// One-line status/error message shown in the footer.
    status: String,
    /// Set to true to exit the event loop.
    quit: bool,
    /// When Some, the main loop should start an in-place scan of these targets
    /// (empty = all) on the next tick. Set by the r/R keys; cleared once the
    /// scan is launched into `scan`.
    pending_scan: Option<Vec<String>>,
    /// The live in-place scan, if one is running. Its per-view `ViewProgress`
    /// sinks are polled each tick to draw the scan-jobs panel; `None` when idle.
    scan: Option<crate::ScanRun>,
    /// LSF-submitted scan, tracked via scan_status.json polling.
    lsf_scan: Option<LsfScanRun>,
    /// A running txt export, if any. Export I/O runs on a worker thread so the
    /// event loop keeps drawing and reading keys while it works; polled each
    /// tick and cleared once its thread signals `done`. `None` when idle.
    export: Option<ExportRun>,
    // ── Output tab state ──
    /// Which report view is shown on the Output tab.
    output_view: OutputView,
    /// History sub-view scroll offset.
    hist_sel: usize,
    /// Detail sub-view: selected user index (into the current target's users).
    detail_sel: usize,
    /// Treemap navigation stack: (dir_id, name) from root to current node.
    tm_stack: Vec<(i64, String)>,
    /// Selected child index in the current treemap node.
    tm_sel: usize,
    /// Live filter string for long lists (user columns + treemap). Empty = no
    /// filter. Reset whenever the view/target/tab changes.
    filter: String,
    /// True while `/` filter-input mode is active (keystrokes edit `filter`).
    filtering: bool,
    /// Whether the LSF submit command (`bs`) was found on PATH at startup.
    bs_available: bool,
    /// Precomputed lowercase filter for case-insensitive matching (avoids
    /// recomputing `.to_lowercase()` per element in filter hot-paths).
    filter_lower: String,
    // ── Report caches (avoid SQLite queries in draw functions) ──
    /// Users from report.db keyed by target name. None = not yet loaded or stale.
    cache_users: RefCell<Option<(String, Vec<crate::ReportUser>)>>,
    /// History snapshots keyed by target name.
    cache_history: RefCell<Option<(String, Vec<crate::HistorySnapshot>)>>,
    /// User detail keyed by (target, username).
    cache_detail: RefCell<Option<(String, String, crate::UserDetail)>>,
    /// User permissions keyed by (target, username).
    cache_perm: RefCell<Option<(String, String, (i64, Vec<crate::PermIssue>))>>,
    /// User inode keyed by (target, username).
    cache_inode: RefCell<Option<(String, String, (i64, i64, Vec<crate::InodeDir>))>>,
    /// Treemap children keyed by (target, node_id).
    cache_tm: RefCell<Option<(String, i64, Vec<crate::TreeEntry>)>>,
}

impl App {
    fn new(cfg: Config) -> Self {
        let bs_available = crate::which_in_path(&cfg.lsf.cmd).is_some();
        Self {
            cfg,
            tab: Tab::Targets,
            mode: Mode::Browse,
            target_sel: 0,
            team_sel: 0,
            user_sel: 0,
            settings_sel: 0,
            scansync_sel: 0,
            status: "↹ tab · ↑↓ move · a add · e edit · d delete · r scan · R scan-all · q quit".into(),
            quit: false,
            pending_scan: None,
            scan: None,
            lsf_scan: None,
            export: None,
            output_view: OutputView::History,
            hist_sel: 0,
            detail_sel: 0,
            tm_stack: Vec::new(),
            tm_sel: 0,
            filter: String::new(),
            filtering: false,
            bs_available,
            filter_lower: String::new(),
            cache_users: RefCell::new(None),
            cache_history: RefCell::new(None),
            cache_detail: RefCell::new(None),
            cache_perm: RefCell::new(None),
            cache_inode: RefCell::new(None),
            cache_tm: RefCell::new(None),
        }
    }

    /// Clear any active filter (called when the view/target/tab context changes
    /// so a stale filter never hides a fresh list).
    fn clear_filter(&mut self) {
        self.filter.clear();
        self.filter_lower.clear();
        self.filtering = false;
    }

    /// Case-insensitive substring match against the current filter (always true
    /// when the filter is empty).
    fn matches_filter(&self, s: &str) -> bool {
        if self.filter_lower.is_empty() { return true; }
        s.to_lowercase().contains(&self.filter_lower)
    }

    /// Drop all report caches so the next access re-queries report.db. Called
    /// on target switch, view switch, scan completion, and filter changes.
    fn invalidate_report_cache(&self) {
        self.cache_users.replace(None);
        self.cache_history.replace(None);
        self.cache_detail.replace(None);
        self.cache_perm.replace(None);
        self.cache_inode.replace(None);
        self.cache_tm.replace(None);
    }

    /// Sync `filter_lower` when `filter` changes (typed char, backspace, clear).
    fn on_filter_changed(&mut self) {
        self.filter_lower = self.filter.to_lowercase();
        self.invalidate_report_cache();
    }

    /// Name of the currently selected target, if any.
    fn current_target_name(&self) -> Option<String> {
        self.cfg.targets.get(self.target_sel).map(|t| t.name.clone())
    }

    /// Name of the currently selected team within the selected target.
    fn current_team_name(&self) -> Option<String> {
        let t = self.cfg.targets.get(self.target_sel)?;
        t.teams.get(self.team_sel).map(|tm| tm.name.clone())
    }

    /// Usernames belonging to the currently selected team, in stored order.
    fn current_team_users(&self) -> Vec<String> {
        let Some(t) = self.cfg.targets.get(self.target_sel) else { return Vec::new() };
        let Some(tm) = t.teams.get(self.team_sel) else { return Vec::new() };
        t.users.iter().filter(|u| u.team_id == tm.team_id).map(|u| u.name.clone()).collect()
    }

    /// `current_team_users` narrowed by the active filter.
    fn filtered_team_users(&self) -> Vec<String> {
        self.current_team_users().into_iter().filter(|u| self.matches_filter(u)).collect()
    }

    /// Clamp all selection indices so they stay within the current data after
    /// add/remove operations change list lengths.
    fn clamp_selections(&mut self) {
        let ntargets = self.cfg.targets.len();
        if self.target_sel >= ntargets { self.target_sel = ntargets.saturating_sub(1); }
        let nteams = self.cfg.targets.get(self.target_sel).map(|t| t.teams.len()).unwrap_or(0);
        if self.team_sel >= nteams { self.team_sel = nteams.saturating_sub(1); }
        // Clamp against the filtered lists so a shrunken view never leaves the
        // selection past the end.
        let nusers = self.filtered_team_users().len();
        if self.user_sel >= nusers { self.user_sel = nusers.saturating_sub(1); }
        if self.settings_sel > 10 { self.settings_sel = 10; }
        if self.scansync_sel > 8 { self.scansync_sel = 8; }
        // Detail/Perm/Inode list users from report.db (team + Other), filtered.
        let ntusers = filtered_report_users(self).len();
        if ntusers > 0 && self.detail_sel >= ntusers { self.detail_sel = ntusers - 1; }
    }
}

/// RAII terminal guard: enables raw mode + alternate screen and renders through
/// `/dev/tty` (a dedicated fd), then redirects stdout+stderr (fd 1/2) to
/// /dev/null for the whole session. This matters because running a scan in-place
/// lets the core's Phase 2/3 `println!`/`eprintln!` noise fire while the TUI is
/// up; routing the display through /dev/tty and silencing fd 1/2 keeps that
/// noise off the screen. On drop — including panic — it restores fd 1/2, leaves
/// the alternate screen, and disables raw mode, so a crash never wedges the
/// user's terminal.
struct TermGuard {
    /// The /dev/tty handle the ratatui backend renders through.
    tty: std::fs::File,
    saved_stdout: libc::c_int,
    saved_stderr: libc::c_int,
}

impl TermGuard {
    fn enter() -> Result<(Self, Terminal<CrosstermBackend<std::fs::File>>), String> {
        use std::os::unix::io::AsRawFd;

        // Render target: the controlling terminal, independent of fd 1/2.
        let mut tty = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/tty")
            .map_err(|e| format!("/dev/tty: {}", e))?;
        enable_raw_mode().map_err(|e| format!("raw mode: {}", e))?;
        if let Err(e) = tty.execute(EnterAlternateScreen) {
            let _ = disable_raw_mode();
            return Err(format!("alt screen: {}", e));
        }

        // Suppress core stdout/stderr: save fd 1/2, point both at /dev/null.
        let saved_stdout;
        let saved_stderr;
        unsafe {
            saved_stdout = libc::dup(1);
            saved_stderr = libc::dup(2);
            if let Ok(devnull) = std::fs::OpenOptions::new().write(true).open("/dev/null") {
                let nfd = devnull.as_raw_fd();
                libc::dup2(nfd, 1);
                libc::dup2(nfd, 2);
            }
        }

        let backend = CrosstermBackend::new(tty.try_clone().map_err(|e| format!("tty clone: {}", e))?);
        let terminal = Terminal::new(backend).map_err(|e| format!("terminal: {}", e))?;
        Ok((TermGuard { tty, saved_stdout, saved_stderr }, terminal))
    }
}

impl Drop for TermGuard {
    fn drop(&mut self) {
        // Restore fd 1/2 first so LeaveAlternateScreen and later prints land on
        // the real terminal.
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
        let _ = self.tty.execute(LeaveAlternateScreen);
        let _ = disable_raw_mode();
    }
}

/// Open the interactive config TUI. Returns once the user quits.
pub fn run(cfg: Config) -> Result<(), String> {
    // Refuse to start without a real terminal (e.g. piped/redirected).
    let is_tty = unsafe { libc::isatty(libc::STDOUT_FILENO) != 0 };
    if !is_tty {
        return Err("not a terminal — run `duscan` interactively, or use subcommands".into());
    }

    let (_guard, mut terminal) = TermGuard::enter()?;
    let mut app = App::new(cfg);

    // Event loop. When no scan is running we block on `event::read()` so an idle
    // TUI costs nothing; while a scan runs we switch to a short poll so the
    // scan-jobs panel refreshes ~7×/s and keystrokes stay responsive.
    while !app.quit {
        terminal.draw(|f| draw(f, &app)).map_err(|e| format!("draw: {}", e))?;

        // Poll (rather than block) while a scan or export runs so the footer
        // reflects their progress/outcome without waiting on a keystroke.
        let busy = app.scan.is_some() || app.export.is_some();
        let got_event = if busy {
            event::poll(std::time::Duration::from_millis(150)).map_err(|e| format!("poll: {}", e))?
        } else {
            true
        };
        if got_event {
            match event::read().map_err(|e| format!("read event: {}", e))? {
                Event::Key(key) if key.kind == event::KeyEventKind::Press => handle_key(&mut app, key),
                _ => {}
            }
        }

        // A scan was requested: launch the workers in-place (they run in the
        // background; the TUI keeps rendering and polling their progress).
        if let Some(names) = app.pending_scan.take() {
            start_scan(&mut app, &names);
        }

        // Poll the running scan; when every worker has finished, join, reload
        // config from disk (so any changes made during the scan show up), and
        // clear the scan handle.
        if let Some(run) = app.scan.as_ref() {
            if run.all_finished() {
                if let Some(mut run) = app.scan.take() {
                    run.join();
                }
                app.cfg = Config::load();
                app.invalidate_report_cache();
                app.clamp_selections();
                app.status = "Scan finished — back to config.".into();
            }
        }

        // Poll LSF scan completion; when all targets are done, clear and reload.
        if let Some(lsf) = app.lsf_scan.as_ref() {
            let snap = lsf.targets.lock().unwrap().clone();
            if !snap.is_empty() && snap.iter().all(|t| t.done) {
                if let Some(mut lsf) = app.lsf_scan.take() {
                    lsf.stop.store(true, std::sync::atomic::Ordering::SeqCst);
                    if let Some(h) = lsf.handle.take() {
                        let _ = h.join();
                    }
                }
                app.cfg = Config::load();
                app.invalidate_report_cache();
                app.clamp_selections();
                app.status = "LSF scan finished — back to config.".into();
            }
        }

        // Poll a running export; once its worker has posted a result, join it,
        // clear the handle, and show the outcome in the footer.
        if let Some(result) = app.export.as_ref().and_then(|e| e.take_result()) {
            if let Some(mut run) = app.export.take() {
                run.join();
            }
            app.status = result;
        }
    }

    // If the user quits mid-scan, signal the workers to cancel.
    if let Some(mut run) = app.scan.take() {
        run.request_abort();
        run.join();
    }
    // Stop the LSF poller thread if still running.
    if let Some(mut lsf) = app.lsf_scan.take() {
        lsf.stop.store(true, std::sync::atomic::Ordering::SeqCst);
        if let Some(h) = lsf.handle.take() {
            let _ = h.join();
        }
    }
    // Wait for any in-flight export so we don't exit mid-write and leave a
    // truncated txt file behind.
    if let Some(mut run) = app.export.take() {
        run.join();
    }
    Ok(())
}

/// Launch an in-place scan of `names` (empty = all targets). When LSF is
/// enabled, submits one job per target instead of scanning locally.
fn start_scan(app: &mut App, names: &[String]) {
    let mut cfg = app.cfg.clone();

    // If LSF is enabled, submit per-target to the cluster instead of scanning
    // locally. A background thread polls scan_status.json every 2s so the
    // TUI shows live progress without blocking.
    if let Some(lsf_prefix) = crate::lsf_prefix_args(&cfg) {
        let exe = match std::env::current_exe() {
            Ok(p) => p.to_string_lossy().into_owned(),
            Err(_) => {
                app.status = "Cannot resolve duscan path for LSF submit.".into();
                return;
            }
        };
        let targets: Vec<String> = if names.is_empty() {
            cfg.targets.iter().map(|t| t.name.clone()).collect()
        } else {
            names.to_vec()
        };
        let total = targets.len();
        let out = cfg.resolved_output_dir();

        // Submit all targets.
        let mut ok = 0;
        for t in &targets {
            if crate::submit_lsf_target(&cfg, &exe, &lsf_prefix, t) {
                ok += 1;
            }
        }
        if ok == 0 {
            app.status = "All LSF submits failed.".into();
            return;
        }

        // Spawn background poller.
        let progress = std::sync::Arc::new(std::sync::Mutex::new(vec![
            LsfTargetProgress { name: String::new(), phase: "submitted".into(),
                files: 0, dirs: 0, size_bytes: 0, error: String::new(),
                elapsed_sec: 0.0, done: false };
            targets.len()
        ]));
        for (i, t) in targets.iter().enumerate() {
            progress.lock().unwrap()[i].name = t.clone();
        }
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let progress_clone = progress.clone();
        let stop_clone = stop.clone();
        let targets_clone = targets.clone();
        let handle = std::thread::spawn(move || {
            let start = std::time::Instant::now();
            while !stop_clone.load(std::sync::atomic::Ordering::SeqCst) {
                let mut snap = progress_clone.lock().unwrap();
                let mut all_done = true;
                for (i, t) in targets_clone.iter().enumerate() {
                    if let Some(s) = crate::read_target_status(&out, t) {
                        snap[i] = LsfTargetProgress {
                            name: t.clone(),
                            phase: s.stage.clone(),
                            files: s.files,
                            dirs: s.dirs,
                            size_bytes: s.size_bytes,
                            error: s.error.clone(),
                            elapsed_sec: s.total_elapsed_sec as f64,
                            done: !s.running,
                        };
                        if s.running { all_done = false; }
                    } else if snap[i].phase == "submitted" {
                        // Target hasn't started writing status yet — still queueing.
                        snap[i].elapsed_sec = start.elapsed().as_secs_f64();
                        all_done = false;
                    }
                }
                drop(snap);
                if all_done {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(2000));
            }
        });

        app.lsf_scan = Some(LsfScanRun { targets: progress, stop, handle: Some(handle) });
        app.status = format!("Submitted {}/{} target(s) to LSF — tracking live...", ok, total);
        return;
    }

    // Local scan mode.
    match crate::plan_scan_jobs(&mut cfg, None, None, names) {
        Some((out, group_jobs, view_names, max_parallel_devices)) => {
            std::fs::create_dir_all(&out).ok();
            let run = crate::spawn_scan_workers(
                &out, false, 3, max_parallel_devices, group_jobs, &view_names, false,
            );
            app.status = format!("Scanning {} target(s)… (q to quit — cancels the scan)", view_names.len());
            app.scan = Some(run);
        }
        None => {
            app.status = "Nothing to scan (no matching targets, or all past end_scan).".into();
        }
    }
}

/// Dispatch a key press based on the current mode.
fn handle_key(app: &mut App, key: event::KeyEvent) {
    match &app.mode {
        Mode::Browse => handle_browse_key(app, key),
        Mode::Input { .. } => handle_input_key(app, key),
        Mode::Confirm { .. } => handle_confirm_key(app, key),
    }
}

/// Begin collecting a line of input for `kind` with the given prompt.
fn begin_input(app: &mut App, kind: InputKind, prompt: &str, initial: &str) {
    app.mode = Mode::Input { kind, prompt: prompt.to_string(), buf: initial.to_string(), completions: Vec::new() };
}

/// Whether Tab should offer directory completion for this input.
fn is_path_input(kind: &InputKind) -> bool {
    matches!(kind, InputKind::NewTargetPath { .. } | InputKind::EditPath | InputKind::SetOutputDir | InputKind::SetSyncDest | InputKind::SetExportDir)
}

fn handle_browse_key(app: &mut App, key: event::KeyEvent) {
    // While a filter is being typed, global keys (q/Esc/digits/Tab) must NOT
    // fire — they are text for the filter. Route straight to the tab handler,
    // which owns the filter-input state.
    if app.filtering {
        match app.tab {
            Tab::TeamsUsers => browse_teams_users(app, key),
            Tab::Output => browse_output(app, key),
            _ => app.filtering = false, // no filter on other tabs; drop the flag
        }
        return;
    }
    // Global keys.
    match key.code {
        KeyCode::Char('q') | KeyCode::Esc => {
            // If a scan is running, cancel it instead of quitting.
            if let Some(r) = app.scan.as_ref() {
                if !r.all_finished() {
                    r.request_abort();
                    app.status = "Scan cancelled — back to config.".into();
                    return;
                }
            }
            // If LSF scan is active, just clear the panel (jobs keep running).
            if app.lsf_scan.is_some() {
                app.lsf_scan = None;
                app.status = "LSF scan still running on cluster. Use: duscan status --watch".into();
                return;
            }
            app.quit = true; return;
        }
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => { app.quit = true; return; }
        KeyCode::Tab | KeyCode::Char('\t') => { app.tab = next_tab(app.tab); app.clear_filter(); return; }
        KeyCode::BackTab => { app.tab = prev_tab(app.tab); app.clear_filter(); return; }
        KeyCode::Char('1') => { app.tab = Tab::Targets; app.clear_filter(); return; }
        KeyCode::Char('2') => { app.tab = Tab::TeamsUsers; app.clear_filter(); return; }
        KeyCode::Char('3') => { app.tab = Tab::ScanSync; app.clear_filter(); return; }
        KeyCode::Char('4') => { app.tab = Tab::Output; app.clear_filter(); return; }
        KeyCode::Char('5') => { app.tab = Tab::Settings; app.clear_filter(); return; }
        // r = scan selected target; R = scan all. Treemap navigation uses
        // arrows/Enter/Backspace so r/R don't clash on the Output tab. A scan
        // already in flight blocks a new one so its worker threads aren't
        // orphaned.
        KeyCode::Char('r') => {
            if app.scan.is_some() {
                app.status = "A scan is already running — wait for it to finish.".into();
            } else {
                match app.current_target_name() {
                    Some(n) => { app.pending_scan = Some(vec![n]); }
                    None => { app.status = "No target to scan — add one first.".into(); }
                }
            }
            return;
        }
        KeyCode::Char('R') => {
            if app.scan.is_some() {
                app.status = "A scan is already running — wait for it to finish.".into();
            } else if app.cfg.targets.is_empty() {
                app.status = "No targets to scan.".into();
            } else {
                app.pending_scan = Some(Vec::new()); // empty = all
            }
            return;
        }
        _ => {}
    }
    match app.tab {
        Tab::Targets => browse_targets(app, key),
        Tab::TeamsUsers => browse_teams_users(app, key),
        Tab::ScanSync => browse_scansync(app, key),
        Tab::Output => browse_output(app, key),
        Tab::Settings => browse_settings(app, key),
    }
}

fn next_tab(t: Tab) -> Tab {
    match t {
        Tab::Targets => Tab::TeamsUsers,
        Tab::TeamsUsers => Tab::ScanSync,
        Tab::ScanSync => Tab::Output,
        Tab::Output => Tab::Settings,
        Tab::Settings => Tab::Targets,
    }
}
fn prev_tab(t: Tab) -> Tab {
    match t {
        Tab::Targets => Tab::Settings,
        Tab::TeamsUsers => Tab::Targets,
        Tab::ScanSync => Tab::TeamsUsers,
        Tab::Output => Tab::ScanSync,
        Tab::Settings => Tab::Output,
    }
}

fn browse_targets(app: &mut App, key: event::KeyEvent) {
    match key.code {
        KeyCode::Up | KeyCode::Char('k') => { if app.target_sel > 0 { app.target_sel -= 1; app.team_sel = 0; app.user_sel = 0; } }
        KeyCode::Down | KeyCode::Char('j') => {
            if app.target_sel + 1 < app.cfg.targets.len() { app.target_sel += 1; app.team_sel = 0; app.user_sel = 0; }
        }
        KeyCode::Char('a') => begin_input(app, InputKind::NewTargetName, "New target name:", ""),
        KeyCode::Char('e') => {
            if let Some(t) = app.cfg.targets.get(app.target_sel) {
                let p = t.path.clone();
                begin_input(app, InputKind::EditPath, "Path:", &p);
            }
        }
        KeyCode::Char('s') => {
            if let Some(t) = app.cfg.targets.get(app.target_sel) {
                let v = t.end_scan.clone().unwrap_or_default();
                begin_input(app, InputKind::EditEndScan, "End scan (YYYYMMDD, empty=none):", &v);
            }
        }
        KeyCode::Char('p') => {
            if let Some(t) = app.cfg.targets.get(app.target_sel) {
                let v = t.purge_time.map(|n| n.to_string()).unwrap_or_default();
                begin_input(app, InputKind::EditPurgeTime, "Purge days (empty=none):", &v);
            }
        }
        KeyCode::Char('d') => {
            if let Some(name) = app.current_target_name() {
                app.mode = Mode::Confirm {
                    action: ConfirmAction::DeleteTarget(name.clone()),
                    prompt: format!("Delete target '{}' and its file? (y/n)", name),
                };
            }
        }
        KeyCode::Enter => { app.tab = Tab::TeamsUsers; }
        _ => {}
    }
}

fn browse_teams_users(app: &mut App, key: event::KeyEvent) {
    if app.current_target_name().is_none() {
        app.status = "No target selected — add one on the Targets tab first.".into();
        return;
    }
    // ── Filter-input mode: keystrokes edit the user filter. ──
    if app.filtering {
        match key.code {
            KeyCode::Esc => { app.clear_filter(); }
            KeyCode::Enter => { app.filtering = false; }
            KeyCode::Backspace => { app.filter.pop(); app.user_sel = 0; }
            KeyCode::Char(c) => { app.filter.push(c); app.user_sel = 0; }
            _ => {}
        }
        return;
    }
    let nteams = app.cfg.targets.get(app.target_sel).map(|t| t.teams.len()).unwrap_or(0);
    let nusers = app.filtered_team_users().len();
    match key.code {
        // [ / ] switch which target this tab operates on, without leaving the tab.
        KeyCode::Char('[') => {
            if app.target_sel > 0 {
                app.target_sel -= 1;
                app.team_sel = 0; app.user_sel = 0; app.clear_filter();
                app.status = format!("Target: {}", app.current_target_name().unwrap_or_default());
            }
        }
        KeyCode::Char(']') => {
            if app.target_sel + 1 < app.cfg.targets.len() {
                app.target_sel += 1;
                app.team_sel = 0; app.user_sel = 0; app.clear_filter();
                app.status = format!("Target: {}", app.current_target_name().unwrap_or_default());
            }
        }
        // Start filtering the user list.
        KeyCode::Char('/') => { app.filtering = true; app.user_sel = 0; }
        // Teams: k/j move team selection (resets user filter). Users: ←→ move.
        KeyCode::Up | KeyCode::Char('k') => { if app.team_sel > 0 { app.team_sel -= 1; app.user_sel = 0; app.clear_filter(); } }
        KeyCode::Down | KeyCode::Char('j') => { if app.team_sel + 1 < nteams { app.team_sel += 1; app.user_sel = 0; app.clear_filter(); } }
        KeyCode::Left => { if app.user_sel > 0 { app.user_sel -= 1; } }
        KeyCode::Right => { if app.user_sel + 1 < nusers { app.user_sel += 1; } }
        KeyCode::Char('a') => begin_input(app, InputKind::NewTeam, "New team name:", ""),
        KeyCode::Char('d') => {
            if let Some(team) = app.current_team_name() {
                app.mode = Mode::Confirm {
                    action: ConfirmAction::DeleteTeam(team.clone()),
                    prompt: format!("Delete team '{}' and its users? (y/n)", team),
                };
            }
        }
        // Users on the selected team.
        KeyCode::Char('u') => {
            if let Some(team) = app.current_team_name() {
                begin_input(app, InputKind::AddUsers { team }, "Add users (alice,bob or @file):", "");
            } else {
                app.status = "Select or add a team first.".into();
            }
        }
        KeyCode::Char('x') => {
            let users = app.filtered_team_users();
            if let Some(uname) = users.get(app.user_sel).cloned() {
                app.mode = Mode::Confirm {
                    action: ConfirmAction::DeleteUser(uname.clone()),
                    prompt: format!("Remove user '{}'? (y/n)", uname),
                };
            }
        }
        _ => {}
    }
}

/// Scan/Sync tab: per-target overrides. Rows: 0 tree_map, 1 level, 2 workers,
/// 3 sync_host, 4 sync_dest_dir, 5 sync_user.
fn browse_scansync(app: &mut App, key: event::KeyEvent) {
    if app.current_target_name().is_none() {
        app.status = "No target selected — add one on the Targets tab first.".into();
        return;
    }
    match key.code {
        KeyCode::Char('[') => {
            if app.target_sel > 0 { app.target_sel -= 1; app.team_sel = 0; app.user_sel = 0;
                app.status = format!("Target: {}", app.current_target_name().unwrap_or_default()); }
        }
        KeyCode::Char(']') => {
            if app.target_sel + 1 < app.cfg.targets.len() { app.target_sel += 1; app.team_sel = 0; app.user_sel = 0;
                app.status = format!("Target: {}", app.current_target_name().unwrap_or_default()); }
        }
        KeyCode::Up | KeyCode::Char('k') => { if app.scansync_sel > 0 { app.scansync_sel -= 1; } }
        KeyCode::Down | KeyCode::Char('j') => { if app.scansync_sel < 8 { app.scansync_sel += 1; } }
        KeyCode::Enter | KeyCode::Char('e') => {
            // Snapshot the fields we need so no immutable borrow of app.cfg is
            // held across the mutable edit_current_target/begin_input calls.
            let Some(t) = app.cfg.targets.get(app.target_sel) else { return };
            let (tree_map, level, workers) = (t.tree_map, t.level, t.workers);
            let (host, dest, user) = (
                t.sync_host.clone().unwrap_or_default(),
                t.sync_dest_dir.clone().unwrap_or_default(),
                t.sync_user.clone().unwrap_or_default(),
            );
            let export_dir = t.export_dir.clone().unwrap_or_default();
            let webhook = t.webhook_url.clone().unwrap_or_default();
            let sync_pass = t.sync_pass;
            match app.scansync_sel {
                // tree_map: cycle unset → true → false → unset without a modal.
                0 => {
                    let next = match tree_map { None => Some(true), Some(true) => Some(false), Some(false) => None };
                    match edit_current_target(app, |t| t.tree_map = next) {
                        Ok(_) => app.status = format!("tree_map = {}", opt_bool_str(next)),
                        Err(e) => app.status = format!("Error: {}", e),
                    }
                }
                1 => begin_input(app, InputKind::SetTargetLevel, "level (empty=default):", &opt_i64_str(level)),
                2 => begin_input(app, InputKind::SetTargetWorkers, "workers (empty=default):", &opt_i64_str(workers)),
                3 => begin_input(app, InputKind::SetSyncHost, "sync host (empty=disable sync):", &host),
                4 => begin_input(app, InputKind::SetSyncDest, "sync dest dir:", &dest),
                5 => begin_input(app, InputKind::SetSyncUser, "sync user (empty=none):", &user),
                6 => begin_input(app, InputKind::SetExportDir, "export dir (empty=exports):", &export_dir),
                7 => begin_input(app, InputKind::SetWebhookUrl, "Teams webhook URL (empty=off):", &webhook),
                // sync_pass: toggle unset → true → false → unset (no modal).
                8 => {
                    let next = match sync_pass { None => Some(true), Some(true) => Some(false), Some(false) => None };
                    match edit_current_target(app, |t| t.sync_pass = next) {
                        Ok(_) => app.status = format!("sync_pass = {} (password via SSHPASS env)", opt_bool_str(next)),
                        Err(e) => app.status = format!("Error: {}", e),
                    }
                }
                _ => {}
            }
        }
        _ => {}
    }
}

fn opt_bool_str(v: Option<bool>) -> String {
    match v { None => "(default)".into(), Some(true) => "true".into(), Some(false) => "false".into() }
}
fn opt_i64_str(v: Option<i64>) -> String {
    v.map(|n| n.to_string()).unwrap_or_default()
}
fn bool_str(v: &bool) -> String {
    if *v { "yes".into() } else { "no".into() }
}

/// report.db path of the currently selected target, if it exists on disk.
fn current_report_db(app: &App) -> Option<std::path::PathBuf> {
    let name = app.current_target_name()?;
    // Resolve a relative output_dir against the binary dir (same as the CLI
    // readers), so the TUI finds reports regardless of the launch cwd.
    crate::resolve_report_db(&app.cfg.resolved_output_dir(), &name)
}

/// Users recorded in the current target's report.db, loaded once then cached
/// per target. The cache is invalidated on target switch, scan completion, and
/// filter changes. Empty when the target has not been scanned yet.
fn current_report_users(app: &App) -> Vec<crate::ReportUser> {
    cached_report_users(app)
}

/// `current_report_users` narrowed by the active filter (by username). Used by
/// Detail/Perm/Inode so navigation + rendering agree on the same visible set.
fn filtered_report_users(app: &App) -> Vec<crate::ReportUser> {
    current_report_users(app).into_iter().filter(|u| app.matches_filter(&u.username)).collect()
}

// ── Cache helpers ────────────────────────────────────────────────────

fn ensure_report_users(app: &App) {
    let target = app.current_target_name().unwrap_or_default();
    let stale = app.cache_users.borrow().as_ref().map(|(t, _)| *t != target).unwrap_or(true);
    if !stale { return; }
    let users = match current_report_db(app) {
        Some(db) => crate::query_report_users(&db),
        None => Vec::new(),
    };
    app.cache_users.replace(Some((target, users)));
}

fn cached_report_users(app: &App) -> Vec<crate::ReportUser> {
    ensure_report_users(app);
    let cache = app.cache_users.borrow();
    match cache.as_ref() {
        Some((_, users)) => users.clone(),
        None => Vec::new(),
    }
}

fn ensure_history(app: &App) {
    let target = app.current_target_name().unwrap_or_default();
    let stale = app.cache_history.borrow().as_ref().map(|(t, _)| *t != target).unwrap_or(true);
    if !stale { return; }
    let snaps = match current_report_db(app) {
        Some(db) => crate::query_history(&db, 60),
        None => Vec::new(),
    };
    app.cache_history.replace(Some((target, snaps)));
}

fn cached_history(app: &App) -> Vec<crate::HistorySnapshot> {
    ensure_history(app);
    let cache = app.cache_history.borrow();
    match cache.as_ref() {
        Some((_, snaps)) => snaps.clone(),
        None => Vec::new(),
    }
}

fn ensure_user_detail(app: &App, username: &str) {
    let target = app.current_target_name().unwrap_or_default();
    let stale = app.cache_detail.borrow().as_ref().map(|(t, u, _)| *t != target || u != username).unwrap_or(true);
    if !stale { return; }
    let detail = match current_report_db(app) {
        Some(db) => crate::query_user_detail(&db, username, 15),
        None => None,
    };
    if let Some(d) = detail {
        app.cache_detail.replace(Some((target, username.to_string(), d)));
    } else {
        app.cache_detail.replace(None);
    }
}

fn cached_user_detail(app: &App, username: &str) -> Option<crate::UserDetail> {
    ensure_user_detail(app, username);
    let cache = app.cache_detail.borrow();
    cache.as_ref().map(|(_, _, d)| d.clone())
}

fn ensure_user_perm(app: &App, username: &str) {
    let target = app.current_target_name().unwrap_or_default();
    let stale = app.cache_perm.borrow().as_ref().map(|(t, u, _)| *t != target || u != username).unwrap_or(true);
    if !stale { return; }
    let (total, issues) = match current_report_db(app) {
        Some(db) => crate::query_user_permissions(&db, username, 200),
        None => (0, Vec::new()),
    };
    app.cache_perm.replace(Some((target, username.to_string(), (total, issues))));
}

fn cached_user_perm(app: &App, username: &str) -> (i64, Vec<crate::PermIssue>) {
    ensure_user_perm(app, username);
    let cache = app.cache_perm.borrow();
    match cache.as_ref() {
        Some((_, _, p)) => p.clone(),
        None => (0, Vec::new()),
    }
}

fn ensure_user_inode(app: &App, username: &str) {
    let target = app.current_target_name().unwrap_or_default();
    let stale = app.cache_inode.borrow().as_ref().map(|(t, u, _)| *t != target || u != username).unwrap_or(true);
    if !stale { return; }
    let (files, dirs, items) = match current_report_db(app) {
        Some(db) => crate::query_user_inode(&db, username, 100),
        None => (0, 0, Vec::new()),
    };
    app.cache_inode.replace(Some((target, username.to_string(), (files, dirs, items))));
}

fn cached_user_inode(app: &App, username: &str) -> (i64, i64, Vec<crate::InodeDir>) {
    ensure_user_inode(app, username);
    let cache = app.cache_inode.borrow();
    match cache.as_ref() {
        Some((_, _, i)) => i.clone(),
        None => (0, 0, Vec::new()),
    }
}

fn ensure_tm_children(app: &App) {
    let target = app.current_target_name().unwrap_or_default();
    let node = match app.tm_stack.last() {
        Some((id, _)) => *id,
        None => {
            // Root node: need to query root id to build the cache key.
            // Use 0 as a sentinel; the loader below resolves root properly.
            0
        }
    };
    let stale = app.cache_tm.borrow().as_ref().map(|(t, n, _)| *t != target || *n != node).unwrap_or(true);
    if !stale { return; }
    let children = load_tm_children_raw(app);
    app.cache_tm.replace(Some((target, node, children)));
}

fn cached_tm_children(app: &App) -> Vec<crate::TreeEntry> {
    ensure_tm_children(app);
    let cache = app.cache_tm.borrow();
    match cache.as_ref() {
        Some((_, _, c)) => c.clone(),
        None => Vec::new(),
    }
}

/// Raw DB load for treemap children (no cache — used by the ensurer).
fn load_tm_children_raw(app: &App) -> Vec<crate::TreeEntry> {
    let Some(db) = current_report_db(app) else { return Vec::new() };
    let Ok(conn) = rusqlite::Connection::open(&db) else { return Vec::new() };
    let Some(tp) = crate::treemap_prefix(&conn) else { return Vec::new() };
    let node = match app.tm_stack.last() {
        Some((id, _)) => *id,
        None => match crate::treemap_root(&conn, tp) { Some(r) => r, None => return Vec::new() },
    };
    let all = crate::treemap_children(&conn, tp, node, 500);
    all.into_iter().filter(|e| app.matches_filter(&e.name)).collect()
}

/// Export usage txt for the current target from the Output/Detail view.
/// `only_user = Some(u)` exports just that user; `None` exports every user.
/// Destination mirrors the CLI layout: `<export_dir>/<target>/` where
/// `export_dir` is the target's per-target override or `exports` by default.
fn export_from_output(app: &mut App, only_user: Option<&str>) {
    if app.export.is_some() {
        app.status = "Export already in progress…".into();
        return;
    }
    let Some(name) = app.current_target_name() else {
        app.status = "No target selected.".into();
        return;
    };
    let Some(db) = current_report_db(app) else {
        app.status = "No report yet — press r to scan this target first.".into();
        return;
    };
    let base = app.cfg.targets.get(app.target_sel)
        .and_then(|t| t.export_dir.clone())
        .unwrap_or_else(|| "exports".into());
    let dest = std::path::Path::new(&base).join(&name);

    // Run the SQLite reads + txt writes on a worker thread so the event loop
    // keeps drawing and reading keys; the outcome is polled back each tick.
    let only_user = only_user.map(str::to_owned);
    let starting = match &only_user {
        Some(u) => format!("Exporting {}…", u),
        None => "Exporting all users…".into(),
    };
    let done = std::sync::Arc::new(std::sync::Mutex::new(None));
    let done_w = std::sync::Arc::clone(&done);
    let handle = std::thread::spawn(move || {
        let msg = match crate::export_target_users(&db, &dest, only_user.as_deref()) {
            Ok(n) => format!("Exported {} user(s) -> {}", n, dest.display()),
            Err(e) => format!("Export failed: {}", e),
        };
        if let Ok(mut g) = done_w.lock() {
            *g = Some(msg);
        }
    });
    app.export = Some(ExportRun { done, handle: Some(handle) });
    app.status = starting;
}

/// Load the treemap children of the node at the top of `tm_stack` (or root when
/// empty). Results are cached per (target, node_id); invalidated on target switch,
/// filter change, and scan completion.
fn load_tm_children(app: &App) -> Vec<crate::TreeEntry> {
    cached_tm_children(app)
}

/// Reset treemap navigation to the root of the current target.
fn reset_treemap(app: &mut App) {
    app.tm_stack.clear();
    app.tm_sel = 0;
    app.cache_tm.replace(None);
}

fn browse_output(app: &mut App, key: event::KeyEvent) {
    // ── Filter-input mode: keystrokes edit the filter, not the view. ──
    if app.filtering {
        match key.code {
            KeyCode::Esc => { app.clear_filter(); }
            KeyCode::Enter => { app.filtering = false; }
            KeyCode::Backspace => { app.filter.pop(); app.on_filter_changed(); app.detail_sel = 0; app.tm_sel = 0; }
            KeyCode::Char(c) => { app.filter.push(c); app.on_filter_changed(); app.detail_sel = 0; app.tm_sel = 0; }
            _ => {}
        }
        return;
    }

    // Switch target with [ ] (resets treemap nav + selections + filter + cache).
    match key.code {
        KeyCode::Char('[') => {
            if app.target_sel > 0 { app.target_sel -= 1; app.team_sel = 0; app.user_sel = 0;
                app.hist_sel = 0; app.detail_sel = 0; reset_treemap(app); app.clear_filter();
                app.invalidate_report_cache();
                app.status = format!("Target: {}", app.current_target_name().unwrap_or_default()); }
            return;
        }
        KeyCode::Char(']') => {
            if app.target_sel + 1 < app.cfg.targets.len() { app.target_sel += 1; app.team_sel = 0; app.user_sel = 0;
                app.hist_sel = 0; app.detail_sel = 0; reset_treemap(app); app.clear_filter();
                app.invalidate_report_cache();
                app.status = format!("Target: {}", app.current_target_name().unwrap_or_default()); }
            return;
        }
        // Switch sub-view (each resets the filter — it's list-specific).
        KeyCode::Char('h') => { app.output_view = OutputView::History; app.hist_sel = 0; app.clear_filter(); app.invalidate_report_cache(); return; }
        KeyCode::Char('d') => { app.output_view = OutputView::Detail; app.detail_sel = 0; app.clear_filter(); app.invalidate_report_cache(); return; }
        KeyCode::Char('p') => { app.output_view = OutputView::Permission; app.detail_sel = 0; app.clear_filter(); app.invalidate_report_cache(); return; }
        KeyCode::Char('i') => { app.output_view = OutputView::Inode; app.detail_sel = 0; app.clear_filter(); app.invalidate_report_cache(); return; }
        KeyCode::Char('t') => { app.output_view = OutputView::Treemap; reset_treemap(app); app.clear_filter(); app.invalidate_report_cache(); return; }
        _ => {}
    }
    match app.output_view {
        OutputView::History => {
            let n = cached_history(app).len();
            match key.code {
                KeyCode::Up | KeyCode::Char('k') => { if app.hist_sel > 0 { app.hist_sel -= 1; } }
                KeyCode::Down | KeyCode::Char('j') => { if app.hist_sel + 1 < n { app.hist_sel += 1; } }
                _ => {}
            }
        }
        // Detail / Permission / Inode all navigate the same (filtered) user list.
        OutputView::Detail | OutputView::Permission | OutputView::Inode => {
            let report_users = filtered_report_users(app);
            let nusers = report_users.len();
            match key.code {
                KeyCode::Char('/') => { app.filtering = true; app.detail_sel = 0; }
                KeyCode::Up | KeyCode::Char('k') => { if app.detail_sel > 0 { app.detail_sel -= 1; } }
                KeyCode::Down | KeyCode::Char('j') => { if app.detail_sel + 1 < nusers { app.detail_sel += 1; } }
                // Export usage txt (Detail only): `x` selected user, `X` all users.
                KeyCode::Char('x') if matches!(app.output_view, OutputView::Detail) => {
                    match report_users.get(app.detail_sel) {
                        Some(u) => { let name = u.username.clone(); export_from_output(app, Some(&name)); }
                        None => app.status = "No user selected to export.".into(),
                    }
                }
                KeyCode::Char('X') if matches!(app.output_view, OutputView::Detail) => export_from_output(app, None),
                _ => {}
            }
        }
        OutputView::Treemap => {
            let children = load_tm_children(app);
            match key.code {
                KeyCode::Char('/') => { app.filtering = true; app.tm_sel = 0; }
                KeyCode::Up | KeyCode::Char('k') => { if app.tm_sel > 0 { app.tm_sel -= 1; } }
                KeyCode::Down | KeyCode::Char('j') => { if app.tm_sel + 1 < children.len() { app.tm_sel += 1; } }
                KeyCode::Enter | KeyCode::Right => {
                    if let Some(entry) = children.get(app.tm_sel) {
                        app.tm_stack.push((entry.id, entry.name.clone()));
                        app.tm_sel = 0;
                        app.clear_filter();
                        app.cache_tm.replace(None); // invalidate tm cache for new node
                        if load_tm_children(app).is_empty() {
                            app.tm_stack.pop();
                            app.status = "Leaf directory (no sub-dirs).".into();
                        }
                    }
                }
                KeyCode::Backspace | KeyCode::Left => {
                    if app.tm_stack.pop().is_some() { app.tm_sel = 0; app.clear_filter(); app.cache_tm.replace(None); }
                }
                _ => {}
            }
        }
    }
}

fn browse_settings(app: &mut App, key: event::KeyEvent) {
    match key.code {
        KeyCode::Up | KeyCode::Char('k') => { if app.settings_sel > 0 { app.settings_sel -= 1; } }
        KeyCode::Down | KeyCode::Char('j') => { if app.settings_sel < 10 { app.settings_sel += 1; } }
        KeyCode::Enter | KeyCode::Char('e') => {
            match app.settings_sel {
                0 => begin_input(app, InputKind::SetOutputDir, "output_dir:", &app.cfg.output_dir.clone()),
                1 => begin_input(app, InputKind::SetWorkers, "workers (auto or N):", &app.cfg.workers.clone()),
                2 => begin_input(app, InputKind::SetMaxParallel, "max_parallel_devices (0=unlimited):", &app.cfg.max_parallel_devices.to_string()),
                3 => begin_input(app, InputKind::SetNfsParallel, "nfs_parallel:", &app.cfg.nfs_parallel.to_string()),
                4 => begin_input(app, InputKind::SetHddParallel, "hdd_parallel:", &app.cfg.hdd_parallel.to_string()),
                5 => begin_input(app, InputKind::SetSsdParallel, "ssd_parallel (0=unlimited):", &app.cfg.ssd_parallel.to_string()),
                6 => begin_input(app, InputKind::SetLsfEnabled, "lsf.enabled (yes/no):", &bool_str(&app.cfg.lsf.enabled)),
                7 => begin_input(app, InputKind::SetLsfCmd, "lsf.cmd (submit wrapper):", &app.cfg.lsf.cmd.clone()),
                8 => begin_input(app, InputKind::SetLsfOs, "lsf.os (e.g. RHEL8, empty=omit):", &app.cfg.lsf.os.clone()),
                9 => begin_input(app, InputKind::SetLsfMemMb, "lsf.mem_mb (0=omit):", &app.cfg.lsf.mem_mb.to_string()),
                10 => begin_input(app, InputKind::SetLsfQueue, "lsf.queue (empty=omit):", &app.cfg.lsf.queue.clone()),
                _ => {}
            }
        }
        _ => {}
    }
}

fn handle_input_key(app: &mut App, key: event::KeyEvent) {
    match key.code {
        KeyCode::Esc => { app.mode = Mode::Browse; app.status = "Cancelled.".into(); }
        KeyCode::Enter => commit_input(app),
        KeyCode::Tab => try_complete_path(app),
        KeyCode::Backspace => {
            if let Mode::Input { buf, completions, .. } = &mut app.mode { buf.pop(); completions.clear(); }
        }
        KeyCode::Char(c) => {
            if let Mode::Input { buf, completions, .. } = &mut app.mode { buf.push(c); completions.clear(); }
        }
        _ => {}
    }
}

/// Tab-complete a directory path in the input box. Fills the longest common
/// prefix of matching sub-directories and, when several match, records them in
/// `completions` for display. No-op for inputs that aren't a path (or, for
/// AddUsers, aren't currently typing an `@file` token).
fn try_complete_path(app: &mut App) {
    let Mode::Input { kind, buf, completions, .. } = &mut app.mode else { return };

    // Determine the region of `buf` that is a path to complete, and whether
    // files (not just directories) are valid candidates.
    //   - path inputs: the whole buffer, directories only.
    //   - AddUsers: only when the last comma-token starts with `@`; the path is
    //     after that `@`, and files are valid (an `@file` points at a file).
    let (region_start, allow_files): (usize, bool) = if is_path_input(kind) {
        (0, false)
    } else if matches!(kind, InputKind::AddUsers { .. }) {
        let tok_start = buf.rfind(',').map(|i| i + 1).unwrap_or(0);
        // Skip leading whitespace in the token.
        let tok = &buf[tok_start..];
        let ws = tok.len() - tok.trim_start().len();
        match buf[tok_start + ws..].strip_prefix('@') {
            Some(_) => (tok_start + ws + 1, true), // path starts just after '@'
            None => { completions.clear(); return; }
        }
    } else {
        return;
    };

    let region = buf[region_start..].to_string();

    // Split the path region into the directory to list and the partial name.
    let (dir, prefix) = match region.rfind('/') {
        Some(i) => (region[..=i].to_string(), region[i + 1..].to_string()),
        None => (String::new(), region.clone()),
    };
    let list_dir = if dir.is_empty() { ".".to_string() } else { dir.clone() };

    // Collect matching entries: directories always, files too when allow_files.
    let mut names: Vec<String> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&list_dir) {
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if !name.starts_with(&prefix) { continue; }
            let is_dir = e.metadata().map(|m| m.is_dir())
                .or_else(|_| e.file_type().map(|t| t.is_dir()))
                .unwrap_or(false);
            if is_dir || allow_files { names.push(if is_dir { format!("{}/", name) } else { name }); }
        }
    }
    names.sort();

    let head = buf[..region_start].to_string();
    if names.is_empty() {
        completions.clear();
        app.status = format!("no match for '{}'", region);
        return;
    }
    if names.len() == 1 {
        // A single dir match ends in '/'; a single file match is complete as-is.
        *buf = format!("{}{}{}", head, dir, names[0]);
        completions.clear();
        return;
    }
    // Multiple: fill longest common prefix (strip any trailing '/' the entries
    // carry so the lcp doesn't glue a slash mid-name), then show the list.
    let bare: Vec<String> = names.iter().map(|n| n.trim_end_matches('/').to_string()).collect();
    let lcp = longest_common_prefix(&bare);
    *buf = format!("{}{}{}", head, dir, lcp);
    *completions = names;
}

/// Longest common prefix of a non-empty slice of strings (byte-wise; directory
/// names are UTF-8 and we only extend the already-typed ASCII-ish prefix).
fn longest_common_prefix(items: &[String]) -> String {
    if items.is_empty() { return String::new(); }
    let first = &items[0];
    let mut end = first.len();
    for s in &items[1..] {
        let common = first.bytes().zip(s.bytes()).take_while(|(a, b)| a == b).count();
        if common < end { end = common; }
    }
    // Back off to a char boundary so we never split a multibyte char.
    while end > 0 && !first.is_char_boundary(end) { end -= 1; }
    first[..end].to_string()
}

/// Resolve a local path to an absolute one. Empty stays empty; an already
/// absolute path is returned unchanged; a relative path is joined onto the
/// current working directory (not canonicalized — the path may not exist yet,
/// e.g. a scan target being created). Used for local path inputs so config
/// stores unambiguous paths regardless of where duscan is later run.
fn to_absolute(p: &str) -> String {
    if p.is_empty() { return String::new(); }
    let path = std::path::Path::new(p);
    if path.is_absolute() {
        return p.to_string();
    }
    std::path::absolute(path)
        .map(|ab| ab.to_string_lossy().into_owned())
        .unwrap_or_else(|_| p.to_string())
}

/// Absolute-ify the `@file` tokens in an add-users buffer, leaving literal
/// usernames untouched. `alice,@rel/list.txt,bob` → `alice,@/abs/rel/list.txt,bob`.
fn abs_at_file_tokens(raw: &str) -> String {
    raw.split(',')
        .map(|tok| {
            let t = tok.trim();
            if let Some(rest) = t.strip_prefix('@') {
                format!("@{}", to_absolute(rest))
            } else {
                t.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join(",")
}

/// Apply the finished input line. Every mutating branch calls a `Config` method
/// that persists immediately, so there is no separate "save" step.
fn commit_input(app: &mut App) {
    // Take the kind + buffer out so we can borrow `app.cfg` mutably below.
    let (kind, mut buf) = match std::mem::replace(&mut app.mode, Mode::Browse) {
        Mode::Input { kind, buf, .. } => (kind, buf.trim().to_string()),
        other => { app.mode = other; return; }
    };

    // Local path inputs are stored absolute so config is unambiguous regardless
    // of the cwd duscan later runs from. sync_dest_dir (a remote path) and
    // output_dir (anchored to the binary dir) are deliberately left as-is.
    match &kind {
        InputKind::NewTargetPath { .. } | InputKind::EditPath | InputKind::SetExportDir => {
            buf = to_absolute(&buf);
        }
        InputKind::AddUsers { .. } => {
            buf = abs_at_file_tokens(&buf);
        }
        _ => {}
    }

    let target = app.current_target_name();

    let result: Result<String, String> = match kind {
        InputKind::NewTargetName => {
            if buf.is_empty() { Err("target name cannot be empty".into()) }
            else {
                // Ask for the path next before creating anything.
                begin_input(app, InputKind::NewTargetPath { name: buf.clone() }, "Path to scan:", "");
                Ok(String::new()) // status set when path is entered
            }
        }
        InputKind::NewTargetPath { name } => {
            if buf.is_empty() { Err("path cannot be empty".into()) }
            else {
                app.cfg.add_target(&name, &buf, None, None)
                    .map(|_| { app.target_sel = app.cfg.targets.len().saturating_sub(1); format!("Created target '{}'", name) })
            }
        }
        InputKind::EditPath => edit_current_target(app, |t| t.path = buf.clone()).map(|n| format!("Updated path of '{}'", n)),
        InputKind::EditEndScan => {
            let v = if buf.is_empty() { None } else { Some(buf.clone()) };
            edit_current_target(app, |t| t.end_scan = v.clone()).map(|n| format!("Updated end_scan of '{}'", n))
        }
        InputKind::EditPurgeTime => {
            match parse_opt_i64(&buf) {
                Ok(v) => edit_current_target(app, |t| t.purge_time = v).map(|n| format!("Updated purge_time of '{}'", n)),
                Err(e) => Err(e),
            }
        }
        InputKind::NewTeam => {
            match &target {
                Some(tn) => app.cfg.add_team(&buf, tn).map(|_| {
                    // Select the just-added team so a following "add users" targets it.
                    if let Some(t) = app.cfg.targets.get(app.target_sel) {
                        app.team_sel = t.teams.len().saturating_sub(1);
                        app.user_sel = 0;
                    }
                    format!("Added team '{}'", buf)
                }),
                None => Err("no target selected".into()),
            }
        }
        InputKind::AddUsers { team } => add_users(app, &team, &buf),
        InputKind::SetOutputDir => { app.cfg.output_dir = buf.clone(); save_globals(app).map(|_| "output_dir updated".into()) }
        InputKind::SetWorkers => { app.cfg.workers = buf.clone(); save_globals(app).map(|_| "workers updated".into()) }
        InputKind::SetMaxParallel => match buf.parse::<i64>() {
            Ok(n) => { app.cfg.max_parallel_devices = n.max(0); save_globals(app).map(|_| "max_parallel_devices updated".into()) }
            Err(_) => Err("must be a number".into()),
        },
        InputKind::SetNfsParallel => match buf.parse::<i64>() {
            Ok(n) => { app.cfg.nfs_parallel = n.max(1); save_globals(app).map(|_| "nfs_parallel updated".into()) }
            Err(_) => Err("must be a number".into()),
        },
        InputKind::SetHddParallel => match buf.parse::<i64>() {
            Ok(n) => { app.cfg.hdd_parallel = n.max(1); save_globals(app).map(|_| "hdd_parallel updated".into()) }
            Err(_) => Err("must be a number".into()),
        },
        InputKind::SetSsdParallel => match buf.parse::<i64>() {
            Ok(n) => { app.cfg.ssd_parallel = n.max(0); save_globals(app).map(|_| "ssd_parallel updated".into()) }
            Err(_) => Err("must be a number".into()),
        },
        // Per-target Scan/Sync fields (empty clears to None = default/disabled).
        InputKind::SetTargetLevel => match parse_opt_i64(&buf) {
            Ok(v) => edit_current_target(app, |t| t.level = v).map(|n| format!("Updated level of '{}'", n)),
            Err(e) => Err(e),
        },
        InputKind::SetTargetWorkers => match parse_opt_i64(&buf) {
            Ok(v) => edit_current_target(app, |t| t.workers = v).map(|n| format!("Updated workers of '{}'", n)),
            Err(e) => Err(e),
        },
        InputKind::SetSyncHost => {
            let v = if buf.is_empty() { None } else { Some(buf.clone()) };
            edit_current_target(app, |t| t.sync_host = v).map(|n| format!("Updated sync_host of '{}'", n))
        }
        InputKind::SetSyncDest => {
            let v = if buf.is_empty() { None } else { Some(buf.clone()) };
            edit_current_target(app, |t| t.sync_dest_dir = v).map(|n| format!("Updated sync_dest_dir of '{}'", n))
        }
        InputKind::SetSyncUser => {
            let v = if buf.is_empty() { None } else { Some(buf.clone()) };
            edit_current_target(app, |t| t.sync_user = v).map(|n| format!("Updated sync_user of '{}'", n))
        }
        InputKind::SetExportDir => {
            let v = if buf.is_empty() { None } else { Some(buf.clone()) };
            edit_current_target(app, |t| t.export_dir = v).map(|n| format!("Updated export_dir of '{}'", n))
        }
        InputKind::SetWebhookUrl => {
            let v = if buf.is_empty() { None } else { Some(buf.clone()) };
            edit_current_target(app, |t| t.webhook_url = v).map(|n| format!("Updated webhook_url of '{}'", n))
        }
        // LSF settings.
        InputKind::SetLsfEnabled => {
            let v = matches!(buf.as_str(), "yes" | "y" | "true" | "1");
            app.cfg.lsf.enabled = v;
            save_globals(app).map(|_| format!("lsf.enabled = {}", bool_str(&v)))
        }
        InputKind::SetLsfCmd => {
            app.cfg.lsf.cmd = buf.clone();
            app.bs_available = crate::which_in_path(&app.cfg.lsf.cmd).is_some();
            save_globals(app).map(|_| format!("lsf.cmd = {}", buf))
        }
        InputKind::SetLsfOs => {
            app.cfg.lsf.os = buf.clone();
            save_globals(app).map(|_| if buf.is_empty() { "lsf.os cleared".into() } else { format!("lsf.os = {}", buf) })
        }
        InputKind::SetLsfMemMb => match buf.parse::<i64>() {
            Ok(n) => { app.cfg.lsf.mem_mb = n.max(0); save_globals(app).map(|_| format!("lsf.mem_mb = {}", n.max(0))) }
            Err(_) => Err("must be a number".into()),
        },
        InputKind::SetLsfQueue => {
            app.cfg.lsf.queue = buf.clone();
            save_globals(app).map(|_| if buf.is_empty() { "lsf.queue cleared".into() } else { format!("lsf.queue = {}", buf) })
        }
    };

    app.clamp_selections();
    match result {
        Ok(msg) => if !msg.is_empty() { app.status = msg; },
        Err(e) => app.status = format!("Error: {}", e),
    }
}

/// Mutate the selected target in place then persist. Returns the target name.
fn edit_current_target(app: &mut App, f: impl FnOnce(&mut crate::config::Target)) -> Result<String, String> {
    let idx = app.target_sel;
    let name = match app.cfg.targets.get(idx) { Some(t) => t.name.clone(), None => return Err("no target selected".into()) };
    if let Some(t) = app.cfg.targets.get_mut(idx) { f(t); }
    app.cfg.save()?;
    Ok(name)
}

/// Parse an optional i64: empty string → None, otherwise a non-negative number.
fn parse_opt_i64(s: &str) -> Result<Option<i64>, String> {
    if s.is_empty() { return Ok(None); }
    s.parse::<i64>().map(Some).map_err(|_| "must be a number or empty".into())
}

/// Add users to a team, expanding a leading `@file` token via the same reader
/// the CLI uses. Each user is persisted through `add_user` (auto-saves).
fn add_users(app: &mut App, team: &str, raw: &str) -> Result<String, String> {
    let Some(target) = app.current_target_name() else { return Err("no target selected".into()) };
    // Reuse the CLI parser: build a fake "team=users" spec so @file + commas work.
    let spec_str = format!("{}={}", team, raw);
    let specs = crate::parse_team_specs(&[spec_str])?;
    let users = specs.into_iter().next().map(|s| s.users).unwrap_or_default();
    if users.is_empty() { return Err("no users given".into()); }
    let mut n = 0;
    for u in &users {
        app.cfg.add_user(u, team, &target)?;
        n += 1;
    }
    Ok(format!("Added {} user(s) to '{}'", n, team))
}

/// Persist just the global settings (targets untouched) by calling the unified
/// `save()`, which writes duscan.toml + reconciles targets/.
fn save_globals(app: &mut App) -> Result<(), String> {
    app.cfg.save()
}

fn handle_confirm_key(app: &mut App, key: event::KeyEvent) {
    match key.code {
        KeyCode::Char('y') | KeyCode::Char('Y') => {
            let action = match std::mem::replace(&mut app.mode, Mode::Browse) {
                Mode::Confirm { action, .. } => action,
                other => { app.mode = other; return; }
            };
            let target = app.current_target_name();
            let res: Result<String, String> = match action {
                ConfirmAction::DeleteTarget(name) => app.cfg.remove_target(&name).map(|_| format!("Deleted target '{}'", name)),
                ConfirmAction::DeleteTeam(team) => match &target {
                    Some(tn) => app.cfg.remove_team(&team, tn).map(|_| format!("Deleted team '{}'", team)),
                    None => Err("no target".into()),
                },
                ConfirmAction::DeleteUser(user) => match &target {
                    Some(tn) => app.cfg.remove_user(&user, tn).map(|_| format!("Removed user '{}'", user)),
                    None => Err("no target".into()),
                },
            };
            app.clamp_selections();
            app.status = match res { Ok(m) => m, Err(e) => format!("Error: {}", e) };
        }
        _ => { app.mode = Mode::Browse; app.status = "Cancelled.".into(); }
    }
}

// ─────────────────────────── rendering ───────────────────────────

fn draw(frame: &mut Frame, app: &App) {
    let area = frame.area();
    // While a scan runs, reserve a strip above the footer for the live jobs
    // panel (one row per target job + border). Cap it so many targets can't
    // crowd out the body.
    let scan_rows = match app.scan.as_ref() {
        Some(run) => (run.progresses.len().min(8) as u16) + 2,
        None => 0,
    };
    let mut constraints = vec![
        Constraint::Length(3), // tab bar
        Constraint::Min(0),    // body
    ];
    if scan_rows > 0 {
        constraints.push(Constraint::Length(scan_rows)); // scan-jobs panel
    }
    constraints.push(Constraint::Length(4)); // footer: status + key hint
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(area);
    // Footer is always the last chunk; the scan panel (if any) sits just above.
    let footer_idx = chunks.len() - 1;
    let scan_idx = if scan_rows > 0 { Some(footer_idx - 1) } else { None };

    // Tab bar with the active config path in the block title.
    let titles = vec!["1 Targets", "2 Teams & Users", "3 Scan/Sync", "4 Output", "5 Settings"];
    let sel = match app.tab {
        Tab::Targets => 0, Tab::TeamsUsers => 1, Tab::ScanSync => 2, Tab::Output => 3, Tab::Settings => 4,
    };
    let path = Config::path();
    let tabs = Tabs::new(titles)
        .select(sel)
        .block(Block::default().borders(Borders::ALL).title(format!(" duscan config — {} ", path.display())))
        .highlight_style(Style::default().fg(Color::Black).bg(Color::Cyan).add_modifier(Modifier::BOLD));
    frame.render_widget(tabs, chunks[0]);

    match app.tab {
        Tab::Targets => draw_targets(frame, app, chunks[1]),
        Tab::TeamsUsers => draw_teams_users(frame, app, chunks[1]),
        Tab::ScanSync => draw_scansync(frame, app, chunks[1]),
        Tab::Output => draw_output(frame, app, chunks[1]),
        Tab::Settings => draw_settings(frame, app, chunks[1]),
    }

    // Live scan-jobs panel (only while a scan is running).
    if let Some(idx) = scan_idx {
        draw_scan_jobs(frame, app, chunks[idx]);
    }

    // Footer: status line + context hint.
    let hint = footer_hint(app);
    let footer = Paragraph::new(vec![
        Line::from(Span::styled(app.status.clone(), Style::default().fg(Color::Yellow))),
        Line::from(Span::styled(hint, Style::default().fg(Color::DarkGray))),
    ])
    .block(Block::default().borders(Borders::ALL));
    frame.render_widget(footer, chunks[footer_idx]);

    // Modal overlays.
    match &app.mode {
        Mode::Input { kind, prompt, buf, completions } =>
            draw_input_modal(frame, area, prompt, buf, completions, is_path_input(kind)),
        Mode::Confirm { prompt, .. } => draw_confirm_modal(frame, area, prompt),
        Mode::Browse => {}
    }
}

/// Map a coarse scan phase label to a human-readable stage name + color.
fn phase_display(phase: &str) -> (String, Color) {
    match phase {
        "waiting" => ("Waiting".into(), Color::DarkGray),
        "queued" => ("Queued".into(), Color::DarkGray),
        "scanning" => ("Scanning".into(), Color::Cyan),
        "building" => ("Building detail".into(), Color::Yellow),
        "treemap" => ("Building treemap".into(), Color::Yellow),
        "merging" => ("Merging".into(), Color::Magenta),
        "history" => ("History".into(), Color::Magenta),
        "syncing" => ("Syncing".into(), Color::Blue),
        "done" => ("Done".into(), Color::Green),
        "error" => ("Error".into(), Color::Red),
        other => (other.to_string(), Color::White),
    }
}

/// Render the live scan-jobs panel: one row per target showing its current
/// stage (Scanning / Building detail / Merging / …) plus live file+dir counts.
fn draw_scan_jobs(frame: &mut Frame, app: &App, area: Rect) {
    let mem_mb = check_disk_core::pipe_types::get_rss_mb();

    // LSF scan: render from polled scan_status.json snapshots.
    if let Some(lsf) = app.lsf_scan.as_ref() {
        let snap = lsf.targets.lock().unwrap().clone();
        let mut rows: Vec<ListItem> = Vec::new();
        for t in &snap {
            let (label, color) = phase_display(&t.phase);
            let detail = if t.phase == "error" && !t.error.is_empty() {
                format!("  {}", t.error)
            } else if t.phase == "submitted" {
                format!("  waiting for job to start...")
            } else {
                format!("  {} files · {} dirs · {}  |  {:.1}s",
                    fmt_count(t.files), fmt_count(t.dirs), crate::fmt_size(t.size_bytes as i64), t.elapsed_sec)
            };
            rows.push(ListItem::new(Line::from(vec![
                Span::styled(format!("{:<16}", truncate(&t.name, 16)), Style::default().fg(Color::White).add_modifier(Modifier::BOLD)),
                Span::styled(format!("{:<18}", label), Style::default().fg(color)),
                Span::styled(detail, Style::default().fg(Color::Gray)),
            ])));
        }
        let all_done = snap.iter().all(|t| t.done);
        let title = if all_done {
            " LSF scan — finished ".to_string()
        } else {
            " LSF scan — running (q quits, scan continues) ".to_string()
        };
        let list = List::new(rows).block(Block::default().borders(Borders::ALL).title(title));
        frame.render_widget(list, area);
        return;
    }

    let run = match app.scan.as_ref() {
        Some(r) => r,
        None => return,
    };
    let mut rows: Vec<ListItem> = Vec::new();
    for p in &run.progresses {
        let (files, dirs, size) = p.scan.snapshot();
        let phase = p.phase.lock().map(|g| g.clone()).unwrap_or_default();
        let err = p.error.lock().map(|g| g.clone()).unwrap_or_default();
        let elapsed = p.started.elapsed().as_secs_f64();
        let (label, color) = phase_display(&phase);
        let detail = if phase == "error" && !err.is_empty() {
            format!("  {}", err)
        } else {
            format!("  {} files · {} dirs · {}  |  {:.1}s  {:.0} MB",
                fmt_count(files), fmt_count(dirs), crate::fmt_size(size as i64), elapsed, mem_mb)
        };
        rows.push(ListItem::new(Line::from(vec![
            Span::styled(format!("{:<16}", truncate(&p.name, 16)), Style::default().fg(Color::White).add_modifier(Modifier::BOLD)),
            Span::styled(format!("{:<18}", label), Style::default().fg(color)),
            Span::styled(detail, Style::default().fg(Color::Gray)),
        ])));
    }
    let done = run.all_finished();
    let title = if done {
        " Scan jobs — finished ".to_string()
    } else {
        " Scan jobs — running (q quits & cancels the scan) ".to_string()
    };
    let list = List::new(rows)
        .block(Block::default().borders(Borders::ALL).title(title));
    frame.render_widget(list, area);
}

/// Thousands-separated count for the scan panel.
fn fmt_count(n: u64) -> String {
    let s = n.to_string();
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, b) in bytes.iter().enumerate() {
        if i > 0 && (bytes.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(*b as char);
    }
    out
}

/// Truncate `s` to `max` chars, adding an ellipsis when cut.
fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let keep = max.saturating_sub(1);
        format!("{}…", s.chars().take(keep).collect::<String>())
    }
}

fn footer_hint(app: &App) -> &'static str {
    // While typing a filter, show the filter-mode keys.
    if app.filtering {
        return "Type to filter · Enter keep · Esc clear · Backspace delete";
    }
    match &app.mode {
        Mode::Input { kind, .. } if is_path_input(kind) => "Type value · Tab complete dir · Enter confirm · Esc cancel",
        Mode::Input { kind, .. } if matches!(kind, InputKind::AddUsers { .. }) => "Type users (alice,bob or @file) · Tab complete @file · Enter confirm · Esc cancel",
        Mode::Input { .. } => "Type value · Enter confirm · Esc cancel",
        Mode::Confirm { .. } => "y confirm · any other key cancel",
        Mode::Browse => match app.tab {
            Tab::Targets => "↑↓ move · a add · e path · s end-scan · p purge · d delete · r scan · R scan-all · ↹ tab · q quit",
            Tab::TeamsUsers => "[ ] target · ↑↓ team · ←→ user · / search · a add-team · d del-team · u add-users · x del-user · q quit",
            Tab::ScanSync => "[ ] target · ↑↓ move · Enter/e edit · r scan this · R scan-all · ↹ tab · q quit",
            Tab::Output => match app.output_view {
                OutputView::History => "[ ] target · h/d/p/i/t view · ↑↓ scroll · r/R scan · ↹ tab · q quit",
                OutputView::Detail => "[ ] target · h/d/p/i/t · / search · ↑↓ user · x export · X export-all · r/R scan · q quit",
                OutputView::Permission => "[ ] target · h/d/p/i/t · / search · ↑↓ user · perm issues · r/R scan · q quit",
                OutputView::Inode => "[ ] target · h/d/p/i/t · / search · ↑↓ user · inode counts · r/R scan · q quit",
                OutputView::Treemap => "[ ] target · h/d/p/i/t · / search · ↑↓ move · Enter open · Bksp up · q quit",
            },
            Tab::Settings => "↑↓ move · Enter/e edit · r/R scan · ↹ tab · q quit",
        },
    }
}

fn selected_style() -> Style {
    Style::default().fg(Color::Black).bg(Color::Cyan).add_modifier(Modifier::BOLD)
}

fn draw_targets(frame: &mut Frame, app: &App, area: Rect) {
    let block = Block::default().borders(Borders::ALL).title(format!(" Targets ({}) ", app.cfg.targets.len()));
    if app.cfg.targets.is_empty() {
        let hint = Paragraph::new(vec![
            Line::from(""),
            Line::from(Span::styled("  No targets yet.", Style::default().fg(Color::White).add_modifier(Modifier::BOLD))),
            Line::from(""),
            Line::from(Span::styled("  Press  a  to add your first target.", Style::default().fg(Color::Cyan))),
        ]).block(block);
        frame.render_widget(hint, area);
        return;
    }
    let items: Vec<ListItem> = app.cfg.targets.iter().map(|t| {
        let end = t.end_scan.clone().unwrap_or_else(|| "-".into());
        let purge = t.purge_time.map(|n| n.to_string()).unwrap_or_else(|| "-".into());
        ListItem::new(format!(
            "{:<18} {:<32} teams:{:<3} users:{:<3} end:{:<9} purge:{}",
            t.name, t.path, t.teams.len(), t.users.len(), end, purge
        ))
    }).collect();

    let list = List::new(items)
        .block(block)
        .highlight_style(selected_style());
    let mut state = ListState::default();
    if !app.cfg.targets.is_empty() { state.select(Some(app.target_sel)); }
    frame.render_stateful_widget(list, area, &mut state);
}

fn draw_teams_users(frame: &mut Frame, app: &App, area: Rect) {
    // No target at all: guide the user back to the Targets tab.
    if app.current_target_name().is_none() {
        let hint = Paragraph::new(vec![
            Line::from(""),
            Line::from(Span::styled("  No target selected.", Style::default().fg(Color::White).add_modifier(Modifier::BOLD))),
            Line::from(""),
            Line::from(Span::styled("  Go to the Targets tab (press 1) and add a target first.", Style::default().fg(Color::Cyan))),
        ]).block(Block::default().borders(Borders::ALL).title(" Teams & Users "));
        frame.render_widget(hint, area);
        return;
    }
    let tname = app.current_target_name().unwrap_or_else(|| "(no target)".into());
    // Show which target (n/total) and that [ ] switches it, so the binding is discoverable.
    let target_label = format!("{} [{}/{}]  ([ ] switch)", tname, app.target_sel + 1, app.cfg.targets.len());
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(45), Constraint::Percentage(55)])
        .split(area);

    // Left: teams of the current target (or a hint if there are none yet).
    let has_teams = app.cfg.targets.get(app.target_sel).map(|t| !t.teams.is_empty()).unwrap_or(false);
    let team_items: Vec<ListItem> = match app.cfg.targets.get(app.target_sel) {
        Some(t) if has_teams => t.teams.iter().map(|tm| {
            let count = t.users.iter().filter(|u| u.team_id == tm.team_id).count();
            ListItem::new(format!("{:<20} ({} users)", tm.name, count))
        }).collect(),
        _ => vec![ListItem::new(Span::styled("Press  a  to add a team", Style::default().fg(Color::Cyan)))],
    };
    let teams = List::new(team_items)
        .block(Block::default().borders(Borders::ALL).title(format!(" Teams — target: {} ", target_label)))
        .highlight_style(selected_style());
    let mut ts = ListState::default();
    if app.cfg.targets.get(app.target_sel).map(|t| !t.teams.is_empty()).unwrap_or(false) {
        ts.select(Some(app.team_sel));
    }
    frame.render_stateful_widget(teams, cols[0], &mut ts);

    // Right: users of the selected team (filterable via `/`), or a hint if empty.
    // Get filtered list once; total from the team's user count avoids iterating twice.
    let users = app.filtered_team_users();
    let total_users = app.cfg.targets.get(app.target_sel)
        .and_then(|tgt| tgt.teams.get(app.team_sel))
        .map(|tm| {
            let target = &app.cfg.targets[app.target_sel];
            target.users.iter().filter(|u| u.team_id == tm.team_id).count()
        })
        .unwrap_or(0);
    let user_items: Vec<ListItem> = if total_users == 0 {
        if has_teams {
            vec![ListItem::new(Span::styled("Press  u  to add users (alice,bob or @file)", Style::default().fg(Color::Cyan)))]
        } else {
            Vec::new()
        }
    } else {
        users.iter().map(|u| ListItem::new(u.clone())).collect()
    };
    let team_label = app.current_team_name().unwrap_or_else(|| "-".into());
    // Title reflects the filter (shown/total) while it's active.
    let utitle = if app.filter.is_empty() && !app.filtering {
        format!(" Users — team: {} ", team_label)
    } else {
        let cursor = if app.filtering { "_" } else { "" };
        format!(" Users — team: {} ({}/{}) — /{}{} ", team_label, users.len(), total_users, app.filter, cursor)
    };
    let ulist = List::new(user_items)
        .block(Block::default().borders(Borders::ALL).title(utitle))
        .highlight_style(selected_style());
    let mut us = ListState::default();
    if !users.is_empty() { us.select(Some(app.user_sel.min(users.len() - 1))); }
    frame.render_stateful_widget(ulist, cols[1], &mut us);
}

fn fmt_size(sz: i64) -> String {
    let s = sz as f64;
    if s >= 1e12 { format!("{:.1} TB", s / 1e12) }
    else if s >= 1e9 { format!("{:.1} GB", s / 1e9) }
    else if s >= 1e6 { format!("{:.1} MB", s / 1e6) }
    else if s >= 1e3 { format!("{:.1} KB", s / 1e3) }
    else { format!("{} B", sz) }
}
fn fmt_date(d: i64) -> String {
    format!("{:04}-{:02}-{:02}", d / 10000, (d / 100) % 100, d % 100)
}

/// Small helper: a centered "empty state" paragraph in `area`.
fn empty_state(frame: &mut Frame, area: Rect, title: &str, lines: &[&str]) {
    let mut body: Vec<Line> = vec![Line::from("")];
    for (i, l) in lines.iter().enumerate() {
        let style = if i == 0 { Style::default().fg(Color::White).add_modifier(Modifier::BOLD) } else { Style::default().fg(Color::Cyan) };
        body.push(Line::from(Span::styled(format!("  {}", l), style)));
    }
    let p = Paragraph::new(body).block(Block::default().borders(Borders::ALL).title(format!(" {} ", title)));
    frame.render_widget(p, area);
}

fn draw_output(frame: &mut Frame, app: &App, area: Rect) {
    let Some(tname) = app.current_target_name() else {
        empty_state(frame, area, "Output", &["No target selected.", "Add a target on the Targets tab (press 1) first."]);
        return;
    };
    // Split: a one-line sub-view selector on top, the view below.
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(0)])
        .split(area);
    let vlabel = |v: OutputView, txt: &str| -> Span {
        if app.output_view == v { Span::styled(format!(" {} ", txt), selected_style()) }
        else { Span::styled(format!(" {} ", txt), Style::default().fg(Color::Gray)) }
    };
    let bar = Paragraph::new(Line::from(vec![
        Span::styled(format!("target: {}  ", tname), Style::default().fg(Color::White).add_modifier(Modifier::BOLD)),
        vlabel(OutputView::History, "h History"), Span::raw(" "),
        vlabel(OutputView::Detail, "d Detail"), Span::raw(" "),
        vlabel(OutputView::Permission, "p Perm"), Span::raw(" "),
        vlabel(OutputView::Inode, "i Inode"), Span::raw(" "),
        vlabel(OutputView::Treemap, "t Treemap"),
    ]));
    frame.render_widget(bar, rows[0]);

    match app.output_view {
        OutputView::History => draw_out_history(frame, app, rows[1]),
        OutputView::Detail => draw_out_detail(frame, app, rows[1]),
        OutputView::Permission => draw_out_permission(frame, app, rows[1]),
        OutputView::Inode => draw_out_inode(frame, app, rows[1]),
        OutputView::Treemap => draw_out_treemap(frame, app, rows[1]),
    }
}

fn draw_out_history(frame: &mut Frame, app: &App, area: Rect) {
    let Some(_db) = current_report_db(app) else {
        empty_state(frame, area, "History", &["No report yet.", "Press r to scan this target first."]);
        return;
    };
    let snaps = cached_history(app);
    if snaps.is_empty() {
        empty_state(frame, area, "History", &["No history in this report.", "Press r to scan this target."]);
        return;
    }
    let mut items: Vec<ListItem> = Vec::new();
    for s in &snaps {
        let pct = if s.used > 0 { s.used as f64 / s.total as f64 * 100.0 } else { 0.0 };
        let head = format!("{}   used {} / {} ({:.1}%)   free {}",
            fmt_date(s.scan_date), fmt_size(s.used), fmt_size(s.total), pct, fmt_size(s.available));
        let mut lines = vec![Line::from(Span::styled(head, Style::default().fg(Color::White)))];
        for (name, size) in s.top_users.iter().take(5) {
            lines.push(Line::from(Span::styled(format!("      {:<16} {}", name, fmt_size(*size)), Style::default().fg(Color::DarkGray))));
        }
        items.push(ListItem::new(lines));
    }
    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL).title(format!(" History ({} snapshots) ", snaps.len())))
        .highlight_style(selected_style());
    let mut st = ListState::default();
    st.select(Some(app.hist_sel.min(snaps.len().saturating_sub(1))));
    frame.render_stateful_widget(list, area, &mut st);
}

fn draw_out_detail(frame: &mut Frame, app: &App, area: Rect) {
    let Some(_db) = current_report_db(app) else {
        empty_state(frame, area, "Detail", &["No report yet.", "Press r to scan this target first."]);
        return;
    };
    let total = cached_report_users(app).len();
    let filtered = filtered_report_users(app);
    let Some((cols, uname_ref)) = draw_user_column(frame, app, area, "Detail", &filtered, total) else { return };
    let uname = uname_ref.to_string();

    match cached_user_detail(app, &uname) {
        Some(d) => {
            let mut lines = vec![
                Line::from(Span::styled(format!("{}  (uid {})", uname, d.uid), Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD))),
                Line::from(format!("files {}   dirs {}   size {}", d.total_files, d.total_dirs, fmt_size(d.total_size))),
                Line::from(""),
                Line::from(Span::styled("Top directories:", Style::default().fg(Color::Yellow))),
            ];
            for (sz, p) in d.top_dirs.iter().take(8) { lines.push(Line::from(format!("  {:>9}  {}", fmt_size(*sz), p))); }
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled("Top files:", Style::default().fg(Color::Yellow))));
            for (sz, p) in d.top_files.iter().take(8) { lines.push(Line::from(format!("  {:>9}  {}", fmt_size(*sz), p))); }
            let para = Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title(format!(" Detail — {} ", uname)));
            frame.render_widget(para, cols[1]);
        }
        None => empty_state(frame, cols[1], "Detail", &[&format!("No data for user '{}'.", uname), "This user may have no files in the last scan."]),
    }
}

/// Render the shared left-hand user column (team users then "Other") used by the
/// Detail / Permission / Inode views. `filtered` is the visible (possibly
/// narrowed) user list; `total` is the unfiltered count for the title. Returns
/// the two-column split plus the selected username, so the caller fills the
/// right pane. `None` when there is nothing selectable (an empty state is drawn:
/// "no report", or "no match" when a filter hides everything).
fn draw_user_column<'a>(
    frame: &mut Frame,
    app: &App,
    area: Rect,
    view_title: &str,
    filtered: &'a [crate::ReportUser],
    total: usize,
) -> Option<(std::rc::Rc<[Rect]>, &'a str)> {
    if total == 0 {
        empty_state(frame, area, view_title, &["No user data in this report.", "Press r to scan this target."]);
        return None;
    }
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(24), Constraint::Min(0)])
        .split(area);
    let title = user_col_title(app, filtered.len(), total);
    if filtered.is_empty() {
        // Filter hid everything — keep the split so the right pane stays blank,
        // and show the (still-typing) filter in the title.
        let empty = List::new(Vec::<ListItem>::new())
            .block(Block::default().borders(Borders::ALL).title(title));
        frame.render_widget(empty, cols[0]);
        empty_state(frame, cols[1], view_title, &[&format!("No user matches '{}'.", app.filter), "Esc to clear the filter."]);
        return None;
    }
    let uitems: Vec<ListItem> = filtered.iter().map(|u| {
        if u.has_team { ListItem::new(u.username.clone()) }
        else { ListItem::new(Span::styled(format!("{}  (Other)", u.username), Style::default().fg(Color::DarkGray))) }
    }).collect();
    let ulist = List::new(uitems)
        .block(Block::default().borders(Borders::ALL).title(title))
        .highlight_style(selected_style());
    let sel = app.detail_sel.min(filtered.len() - 1);
    let mut us = ListState::default();
    us.select(Some(sel));
    frame.render_stateful_widget(ulist, cols[0], &mut us);
    Some((cols, filtered[sel].username.as_str()))
}

/// Title for the user column, showing the filter (and shown/total counts) when
/// a filter is active, else just the total.
fn user_col_title(app: &App, shown: usize, total: usize) -> String {
    if app.filter.is_empty() && !app.filtering {
        format!(" Users ({}) ", total)
    } else {
        let cursor = if app.filtering { "_" } else { "" };
        format!(" Users ({}/{}) — /{}{} ", shown, total, app.filter, cursor)
    }
}

fn draw_out_permission(frame: &mut Frame, app: &App, area: Rect) {
    let Some(_db) = current_report_db(app) else {
        empty_state(frame, area, "Permission", &["No report yet.", "Press r to scan this target first."]);
        return;
    };
    let total_users = cached_report_users(app).len();
    let filtered = filtered_report_users(app);
    let Some((cols, uname)) = draw_user_column(frame, app, area, "Permission", &filtered, total_users) else { return };

    let (total, issues) = cached_user_perm(app, uname);
    if total == 0 {
        empty_state(frame, cols[1], "Permission",
            &[&format!("No permission issues for '{}'.", uname), "The scan hit no unreadable paths for this user."]);
        return;
    }
    let mut lines = vec![
        Line::from(Span::styled(format!("{}  —  {} permission issue(s)", uname, total),
            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD))),
        Line::from(Span::styled(format!("{:<8}  {:<20}  {}", "Type", "Error", "Path"), Style::default().fg(Color::Yellow))),
    ];
    for i in issues.iter() {
        lines.push(Line::from(format!("{:<8}  {:<20}  {}", i.item_type, i.error, i.path)));
    }
    let para = Paragraph::new(lines)
        .block(Block::default().borders(Borders::ALL).title(format!(" Permission — {} ", uname)));
    frame.render_widget(para, cols[1]);
}

fn draw_out_inode(frame: &mut Frame, app: &App, area: Rect) {
    let Some(_db) = current_report_db(app) else {
        empty_state(frame, area, "Inode", &["No report yet.", "Press r to scan this target first."]);
        return;
    };
    let total_users = cached_report_users(app).len();
    let filtered = filtered_report_users(app);
    let Some((cols, uname)) = draw_user_column(frame, app, area, "Inode", &filtered, total_users) else { return };

    let (total_files, total_dirs, dirs) = cached_user_inode(app, uname);
    if dirs.is_empty() {
        empty_state(frame, cols[1], "Inode",
            &[&format!("No directory data for '{}'.", uname), "This user may have no files in the last scan."]);
        return;
    }
    let inodes = total_files + total_dirs;
    let mut lines = vec![
        Line::from(Span::styled(format!("{}  —  {} inodes ({} files + {} dirs)", uname, inodes, total_files, total_dirs),
            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD))),
        Line::from(Span::styled(format!("{:>9}  {:>9}  {}", "Files", "Size", "Directory"), Style::default().fg(Color::Yellow))),
    ];
    for d in dirs.iter() {
        lines.push(Line::from(format!("{:>9}  {:>9}  {}", d.files, fmt_size(d.size), d.path)));
    }
    let para = Paragraph::new(lines)
        .block(Block::default().borders(Borders::ALL).title(format!(" Inode / file count — {} ", uname)));
    frame.render_widget(para, cols[1]);
}

/// Precomputed treemap bar segments (length 0 through 12) to avoid per-frame
/// String allocations from `repeat().chain().collect()` in the hot render path.
static TM_BARS: [&str; 13] = [
    "············",
    "█···········",
    "██··········",
    "███·········",
    "████········",
    "█████·······",
    "██████······",
    "███████·····",
    "████████····",
    "█████████···",
    "██████████··",
    "███████████·",
    "████████████",
];

fn draw_out_treemap(frame: &mut Frame, app: &App, area: Rect) {
    let Some(_db) = current_report_db(app) else {
        empty_state(frame, area, "Treemap", &["No report yet.", "Press r to scan this target first."]);
        return;
    };
    let children = load_tm_children(app);
    // Breadcrumb: avoid intermediate Vec allocation by folding.
    let crumb: String = {
        let mut s = String::from("/");
        for (i, (_, n)) in app.tm_stack.iter().enumerate() {
            if i > 0 { s.push('/'); }
            s.push_str(n);
        }
        s
    };
    if children.is_empty() {
        if app.filter.is_empty() && !app.filtering {
            empty_state(frame, area, "Treemap", &["No treemap data.", "Scan with tree_map enabled (Scan/Sync tab) first."]);
        } else {
            empty_state(frame, area, "Treemap", &[&format!("No entry matches '{}'.", app.filter), "Esc to clear the filter."]);
        }
        return;
    }
    let maxsz = children.iter().map(|c| c.size).max().unwrap_or(1).max(1);
    let items: Vec<ListItem> = children.iter().map(|c| {
        let barlen = ((c.size as f64 / maxsz as f64) * 12.0).round() as usize;
        let bar = TM_BARS[barlen.min(12)];
        ListItem::new(format!("{:>9}  [{}]  {:<28}  {} files", fmt_size(c.size), bar, c.name, c.file_count))
    }).collect();
    let title = if app.filter.is_empty() && !app.filtering {
        format!(" Treemap — {} ", crumb)
    } else {
        let cursor = if app.filtering { "_" } else { "" };
        format!(" Treemap — {} — /{}{} ", crumb, app.filter, cursor)
    };
    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL).title(title))
        .highlight_style(selected_style());
    let mut st = ListState::default();
    st.select(Some(app.tm_sel.min(children.len().saturating_sub(1))));
    frame.render_stateful_widget(list, area, &mut st);
}

fn draw_settings(frame: &mut Frame, app: &App, area: Rect) {
    let bs_status = if app.bs_available {
        Span::styled(" [available]", Style::default().fg(Color::Green))
    } else {
        Span::styled(" [not found]", Style::default().fg(Color::Red))
    };
    let rows = [
        format!("output_dir           = {}", app.cfg.output_dir),
        format!("workers              = {}", app.cfg.workers),
        format!("max_parallel_devices = {}", app.cfg.max_parallel_devices),
        format!("nfs_parallel         = {}", app.cfg.nfs_parallel),
        format!("hdd_parallel         = {}", app.cfg.hdd_parallel),
        format!("ssd_parallel         = {}", app.cfg.ssd_parallel),
    ];
    let lsf_rows = [
        Line::from(vec![
            Span::raw(format!("lsf.enabled          = {}", bool_str(&app.cfg.lsf.enabled))),
            bs_status,
        ]),
        Line::from(format!("lsf.cmd              = {}", app.cfg.lsf.cmd)),
        Line::from(format!("lsf.os               = {}", if app.cfg.lsf.os.is_empty() { String::from("(none)") } else { app.cfg.lsf.os.clone() })),
        Line::from(format!("lsf.mem_mb           = {}", if app.cfg.lsf.mem_mb <= 0 { String::from("(omit)") } else { app.cfg.lsf.mem_mb.to_string() })),
        Line::from(format!("lsf.queue            = {}", if app.cfg.lsf.queue.is_empty() { String::from("(none)") } else { app.cfg.lsf.queue.clone() })),
    ];
    let mut items: Vec<ListItem> = rows.iter().cloned().map(ListItem::new).collect();
    for r in &lsf_rows {
        items.push(ListItem::new(r.clone()));
    }
    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL).title(" Global settings "))
        .highlight_style(selected_style());
    let mut state = ListState::default();
    state.select(Some(app.settings_sel));
    frame.render_stateful_widget(list, area, &mut state);
}

fn draw_scansync(frame: &mut Frame, app: &App, area: Rect) {
    let Some(t) = app.cfg.targets.get(app.target_sel) else {
        let hint = Paragraph::new(vec![
            Line::from(""),
            Line::from(Span::styled("  No target selected.", Style::default().fg(Color::White).add_modifier(Modifier::BOLD))),
            Line::from(""),
            Line::from(Span::styled("  Add a target on the Targets tab (press 1) first.", Style::default().fg(Color::Cyan))),
        ]).block(Block::default().borders(Borders::ALL).title(" Scan / Sync "));
        frame.render_widget(hint, area);
        return;
    };
    let target_label = format!("{} [{}/{}]  ([ ] switch)", t.name, app.target_sel + 1, app.cfg.targets.len());
    let dash = |o: &Option<String>| o.clone().unwrap_or_else(|| "(none)".into());
    let rows = [
        format!("tree_map      = {}", opt_bool_str(t.tree_map)),
        format!("level         = {}", t.level.map(|n| n.to_string()).unwrap_or_else(|| "(default)".into())),
        format!("workers       = {}", t.workers.map(|n| n.to_string()).unwrap_or_else(|| "(default)".into())),
        format!("sync_host     = {}", dash(&t.sync_host)),
        format!("sync_dest_dir = {}", dash(&t.sync_dest_dir)),
        format!("sync_user     = {}", dash(&t.sync_user)),
        format!("export_dir    = {}", t.export_dir.clone().unwrap_or_else(|| "(exports)".into())),
        format!("webhook_url   = {}", dash(&t.webhook_url)),
        format!("sync_pass     = {}", opt_bool_str(t.sync_pass)),
    ];
    let items: Vec<ListItem> = rows.iter().cloned().map(ListItem::new).collect();
    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL).title(format!(" Scan / Sync — target: {} ", target_label)))
        .highlight_style(selected_style());
    let mut state = ListState::default();
    state.select(Some(app.scansync_sel));
    frame.render_stateful_widget(list, area, &mut state);
}

/// Centered box of `w`×`h` within `area`.
fn centered(area: Rect, w: u16, h: u16) -> Rect {
    let x = area.x + (area.width.saturating_sub(w)) / 2;
    let y = area.y + (area.height.saturating_sub(h)) / 2;
    Rect { x, y, width: w.min(area.width), height: h.min(area.height) }
}

fn draw_input_modal(frame: &mut Frame, area: Rect, prompt: &str, buf: &str, completions: &[String], is_path: bool) {
    const MAX_SHOWN: usize = 8;
    let w = area.width.min(70).max(20);

    let mut lines: Vec<Line> = vec![
        Line::from(Span::styled(prompt.to_string(), Style::default().fg(Color::Cyan))),
        Line::from(Span::styled(format!("{}▏", buf), Style::default().fg(Color::White))),
    ];
    if is_path {
        lines.push(Line::from(Span::styled("Tab to complete directory", Style::default().fg(Color::DarkGray))));
    }
    // Directory candidates after a Tab, capped with a "+N more" tail.
    if !completions.is_empty() {
        let shown = completions.len().min(MAX_SHOWN);
        for name in &completions[..shown] {
            lines.push(Line::from(Span::styled(format!("  {}/", name), Style::default().fg(Color::Green))));
        }
        if completions.len() > shown {
            lines.push(Line::from(Span::styled(
                format!("  …(+{} more)", completions.len() - shown),
                Style::default().fg(Color::DarkGray),
            )));
        }
    }

    let h = (lines.len() as u16) + 2; // + borders
    let rect = centered(area, w, h);
    frame.render_widget(Clear, rect);
    let body = Paragraph::new(lines)
        .block(Block::default().borders(Borders::ALL).title(" Input ").border_style(Style::default().fg(Color::Cyan)));
    frame.render_widget(body, rect);
}

fn draw_confirm_modal(frame: &mut Frame, area: Rect, prompt: &str) {
    let w = area.width.min(60).max(20);
    let rect = centered(area, w, 4);
    frame.render_widget(Clear, rect);
    let body = Paragraph::new(Line::from(Span::styled(prompt, Style::default().fg(Color::Red).add_modifier(Modifier::BOLD))))
        .block(Block::default().borders(Borders::ALL).title(" Confirm ").border_style(Style::default().fg(Color::Red)));
    frame.render_widget(body, rect);
}
