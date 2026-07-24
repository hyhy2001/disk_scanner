//! Interactive configuration TUI (opened by running `duscan` with no
//! subcommand). Manages targets, teams, and users across three tabs and writes
//! every change straight to disk via the existing `Config` API (each op calls
//! `save()`, so `duscan.toml` + `targets/*.toml` stay current). This is separate
//! from `ui.rs`, which is the read-only scan monitor.

use std::io::Stdout;

use crossterm::event::{self, Event, KeyCode, KeyModifiers};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen};
use crossterm::ExecutableCommand;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Tabs};
use ratatui::{Frame, Terminal};

use crate::config::Config;

/// Which top-level tab is focused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tab {
    Targets,
    TeamsUsers,
    ScanSync,
    Settings,
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
    // Per-target Scan/Sync tab fields.
    SetTargetLevel,
    SetTargetWorkers,
    SetSyncHost,
    SetSyncDest,
    SetSyncUser,
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

/// Whole-app state: the live config plus cursor positions per tab.
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
    /// When Some, the run loop should drop the TUI, scan these targets (empty =
    /// all), then re-enter the TUI. Set by the r/R keys.
    pending_scan: Option<Vec<String>>,
}

impl App {
    fn new(cfg: Config) -> Self {
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
        }
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

    /// Clamp all selection indices so they stay within the current data after
    /// add/remove operations change list lengths.
    fn clamp_selections(&mut self) {
        let ntargets = self.cfg.targets.len();
        if self.target_sel >= ntargets { self.target_sel = ntargets.saturating_sub(1); }
        let nteams = self.cfg.targets.get(self.target_sel).map(|t| t.teams.len()).unwrap_or(0);
        if self.team_sel >= nteams { self.team_sel = nteams.saturating_sub(1); }
        let nusers = self.current_team_users().len();
        if self.user_sel >= nusers { self.user_sel = nusers.saturating_sub(1); }
        if self.settings_sel > 3 { self.settings_sel = 3; }
        if self.scansync_sel > 5 { self.scansync_sel = 5; }
    }
}

/// RAII terminal guard: enables raw mode + alternate screen on construction and
/// restores both on drop (including panic), so a crash never wedges the user's
/// terminal. Unlike the scan monitor's guard it renders on stdout and does not
/// redirect fds — the config TUI produces no stray stdout.
struct TermGuard;

impl TermGuard {
    fn enter() -> Result<(Self, Terminal<CrosstermBackend<Stdout>>), String> {
        enable_raw_mode().map_err(|e| format!("raw mode: {}", e))?;
        let mut out = std::io::stdout();
        out.execute(EnterAlternateScreen).map_err(|e| format!("alt screen: {}", e))?;
        let backend = CrosstermBackend::new(out);
        let terminal = Terminal::new(backend).map_err(|e| format!("terminal: {}", e))?;
        Ok((TermGuard, terminal))
    }
}

impl Drop for TermGuard {
    fn drop(&mut self) {
        let _ = std::io::stdout().execute(LeaveAlternateScreen);
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

    let mut guard_term = Some(TermGuard::enter()?);
    let mut app = App::new(cfg);

    while !app.quit {
        {
            let (_g, terminal) = guard_term.as_mut().unwrap();
            terminal.draw(|f| draw(f, &app)).map_err(|e| format!("draw: {}", e))?;
        }
        match event::read().map_err(|e| format!("read event: {}", e))? {
            Event::Key(key) if key.kind == event::KeyEventKind::Press => handle_key(&mut app, key),
            _ => {}
        }

        // A scan was requested: fully leave the config TUI (restore terminal),
        // hand the screen to run_scan's own monitor, then re-enter the TUI and
        // reload config from disk so any changes made during the scan show up.
        if let Some(names) = app.pending_scan.take() {
            drop(guard_term.take()); // restores terminal via TermGuard::drop
            crate::run_scan(&mut app.cfg, None, false, None, 3, &names);
            app.cfg = Config::load();
            app.clamp_selections();
            guard_term = Some(TermGuard::enter()?);
            app.status = "Scan finished — back to config.".into();
        }
    }
    Ok(())
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
    matches!(kind, InputKind::NewTargetPath { .. } | InputKind::EditPath | InputKind::SetOutputDir | InputKind::SetSyncDest)
}

fn handle_browse_key(app: &mut App, key: event::KeyEvent) {
    // Global keys.
    match key.code {
        KeyCode::Char('q') | KeyCode::Esc => { app.quit = true; return; }
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => { app.quit = true; return; }
        KeyCode::Tab | KeyCode::Char('\t') => { app.tab = next_tab(app.tab); return; }
        KeyCode::BackTab => { app.tab = prev_tab(app.tab); return; }
        KeyCode::Char('1') => { app.tab = Tab::Targets; return; }
        KeyCode::Char('2') => { app.tab = Tab::TeamsUsers; return; }
        KeyCode::Char('3') => { app.tab = Tab::ScanSync; return; }
        KeyCode::Char('4') => { app.tab = Tab::Settings; return; }
        // r = scan the selected target; R = scan all targets. Requests a scan;
        // the run loop drops the TUI, runs it, and re-enters.
        KeyCode::Char('r') => {
            match app.current_target_name() {
                Some(n) => { app.pending_scan = Some(vec![n]); }
                None => { app.status = "No target to scan — add one first.".into(); }
            }
            return;
        }
        KeyCode::Char('R') => {
            if app.cfg.targets.is_empty() { app.status = "No targets to scan.".into(); }
            else { app.pending_scan = Some(Vec::new()); } // empty = all
            return;
        }
        _ => {}
    }
    match app.tab {
        Tab::Targets => browse_targets(app, key),
        Tab::TeamsUsers => browse_teams_users(app, key),
        Tab::ScanSync => browse_scansync(app, key),
        Tab::Settings => browse_settings(app, key),
    }
}

fn next_tab(t: Tab) -> Tab {
    match t {
        Tab::Targets => Tab::TeamsUsers,
        Tab::TeamsUsers => Tab::ScanSync,
        Tab::ScanSync => Tab::Settings,
        Tab::Settings => Tab::Targets,
    }
}
fn prev_tab(t: Tab) -> Tab {
    match t {
        Tab::Targets => Tab::Settings,
        Tab::TeamsUsers => Tab::Targets,
        Tab::ScanSync => Tab::TeamsUsers,
        Tab::Settings => Tab::ScanSync,
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
    let nteams = app.cfg.targets.get(app.target_sel).map(|t| t.teams.len()).unwrap_or(0);
    let nusers = app.current_team_users().len();
    match key.code {
        // [ / ] switch which target this tab operates on, without leaving the tab.
        KeyCode::Char('[') => {
            if app.target_sel > 0 {
                app.target_sel -= 1;
                app.team_sel = 0; app.user_sel = 0;
                app.status = format!("Target: {}", app.current_target_name().unwrap_or_default());
            }
        }
        KeyCode::Char(']') => {
            if app.target_sel + 1 < app.cfg.targets.len() {
                app.target_sel += 1;
                app.team_sel = 0; app.user_sel = 0;
                app.status = format!("Target: {}", app.current_target_name().unwrap_or_default());
            }
        }
        // Teams: k/j move team selection. Users: ←→ move user selection.
        KeyCode::Up | KeyCode::Char('k') => { if app.team_sel > 0 { app.team_sel -= 1; app.user_sel = 0; } }
        KeyCode::Down | KeyCode::Char('j') => { if app.team_sel + 1 < nteams { app.team_sel += 1; app.user_sel = 0; } }
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
            let users = app.current_team_users();
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
        KeyCode::Down | KeyCode::Char('j') => { if app.scansync_sel < 5 { app.scansync_sel += 1; } }
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

fn browse_settings(app: &mut App, key: event::KeyEvent) {
    match key.code {
        KeyCode::Up | KeyCode::Char('k') => { if app.settings_sel > 0 { app.settings_sel -= 1; } }
        KeyCode::Down | KeyCode::Char('j') => { if app.settings_sel < 3 { app.settings_sel += 1; } }
        KeyCode::Enter | KeyCode::Char('e') => {
            match app.settings_sel {
                0 => begin_input(app, InputKind::SetOutputDir, "output_dir:", &app.cfg.output_dir.clone()),
                1 => begin_input(app, InputKind::SetWorkers, "workers (auto or N):", &app.cfg.workers.clone()),
                2 => begin_input(app, InputKind::SetMaxParallel, "max_parallel_devices (0=unlimited):", &app.cfg.max_parallel_devices.to_string()),
                3 => begin_input(app, InputKind::SetNfsParallel, "nfs_parallel:", &app.cfg.nfs_parallel.to_string()),
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
/// `completions` for display. No-op for non-path inputs.
fn try_complete_path(app: &mut App) {
    let Mode::Input { kind, buf, completions, .. } = &mut app.mode else { return };
    if !is_path_input(kind) { return; }

    // Split buf into the directory to list and the partial name being typed.
    let (dir, prefix) = match buf.rfind('/') {
        Some(i) => (buf[..=i].to_string(), buf[i + 1..].to_string()),
        None => (String::new(), buf.clone()),
    };
    let list_dir = if dir.is_empty() { ".".to_string() } else { dir.clone() };

    // Collect matching sub-directories (directories only).
    let mut names: Vec<String> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&list_dir) {
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if !name.starts_with(&prefix) { continue; }
            // Directory check follows symlinks (metadata) but falls back to file_type.
            let is_dir = e.metadata().map(|m| m.is_dir())
                .or_else(|_| e.file_type().map(|t| t.is_dir()))
                .unwrap_or(false);
            if is_dir { names.push(name); }
        }
    }
    names.sort();

    if names.is_empty() {
        completions.clear();
        app.status = format!("no directory matches '{}'", buf);
        return;
    }
    if names.len() == 1 {
        *buf = format!("{}{}/", dir, names[0]);
        completions.clear();
        return;
    }
    // Multiple: fill longest common prefix, then show the list.
    let lcp = longest_common_prefix(&names);
    *buf = format!("{}{}", dir, lcp);
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

/// Apply the finished input line. Every mutating branch calls a `Config` method
/// that persists immediately, so there is no separate "save" step.
fn commit_input(app: &mut App) {
    // Take the kind + buffer out so we can borrow `app.cfg` mutably below.
    let (kind, buf) = match std::mem::replace(&mut app.mode, Mode::Browse) {
        Mode::Input { kind, buf, .. } => (kind, buf.trim().to_string()),
        other => { app.mode = other; return; }
    };
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
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3), // tab bar
            Constraint::Min(0),    // body
            Constraint::Length(4), // footer: status + key hint (2 lines + border)
        ])
        .split(area);

    // Tab bar with the active config path in the block title.
    let titles = vec!["1 Targets", "2 Teams & Users", "3 Scan/Sync", "4 Settings"];
    let sel = match app.tab { Tab::Targets => 0, Tab::TeamsUsers => 1, Tab::ScanSync => 2, Tab::Settings => 3 };
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
        Tab::Settings => draw_settings(frame, app, chunks[1]),
    }

    // Footer: status line + context hint.
    let hint = footer_hint(app);
    let footer = Paragraph::new(vec![
        Line::from(Span::styled(app.status.clone(), Style::default().fg(Color::Yellow))),
        Line::from(Span::styled(hint, Style::default().fg(Color::DarkGray))),
    ])
    .block(Block::default().borders(Borders::ALL));
    frame.render_widget(footer, chunks[2]);

    // Modal overlays.
    match &app.mode {
        Mode::Input { kind, prompt, buf, completions } =>
            draw_input_modal(frame, area, prompt, buf, completions, is_path_input(kind)),
        Mode::Confirm { prompt, .. } => draw_confirm_modal(frame, area, prompt),
        Mode::Browse => {}
    }
}

fn footer_hint(app: &App) -> &'static str {
    match &app.mode {
        Mode::Input { kind, .. } if is_path_input(kind) => "Type value · Tab complete dir · Enter confirm · Esc cancel",
        Mode::Input { .. } => "Type value · Enter confirm · Esc cancel",
        Mode::Confirm { .. } => "y confirm · any other key cancel",
        Mode::Browse => match app.tab {
            Tab::Targets => "↑↓ move · a add · e path · s end-scan · p purge · d delete · r scan · R scan-all · ↹ tab · q quit",
            Tab::TeamsUsers => "[ ] target · ↑↓ team · ←→ user · a add-team · d del-team · u add-users · x del-user · r/R scan · q quit",
            Tab::ScanSync => "[ ] target · ↑↓ move · Enter/e edit · r scan this · R scan-all · ↹ tab · q quit",
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

    // Right: users of the selected team (or a hint if the team is empty).
    let users = app.current_team_users();
    let user_items: Vec<ListItem> = if users.is_empty() {
        if has_teams {
            vec![ListItem::new(Span::styled("Press  u  to add users (alice,bob or @file)", Style::default().fg(Color::Cyan)))]
        } else {
            Vec::new()
        }
    } else {
        users.iter().map(|u| ListItem::new(u.clone())).collect()
    };
    let team_label = app.current_team_name().unwrap_or_else(|| "-".into());
    let ulist = List::new(user_items)
        .block(Block::default().borders(Borders::ALL).title(format!(" Users — team: {} ", team_label)))
        .highlight_style(selected_style());
    let mut us = ListState::default();
    if !users.is_empty() { us.select(Some(app.user_sel)); }
    frame.render_stateful_widget(ulist, cols[1], &mut us);
}

fn draw_settings(frame: &mut Frame, app: &App, area: Rect) {
    let rows = [
        format!("output_dir           = {}", app.cfg.output_dir),
        format!("workers              = {}", app.cfg.workers),
        format!("max_parallel_devices = {}", app.cfg.max_parallel_devices),
        format!("nfs_parallel         = {}", app.cfg.nfs_parallel),
    ];
    let items: Vec<ListItem> = rows.iter().cloned().map(ListItem::new).collect();
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
