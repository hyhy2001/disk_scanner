use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Cell, Row, Table, Paragraph},
};
use std::time::Instant;

/// Per-target live progress, updated by the scan worker threads and rendered by
/// the TUI thread. `phase` walks: waiting → scanning → building → treemap →
/// merging → history → done / error.
#[derive(Debug, Clone)]
pub struct ScanProgress {
    pub target: String,
    pub phase: String,
    pub files: u64,
    pub dirs: u64,
    pub size: u64,
    pub rate: f64,
    pub mem_mb: f64,
    pub elapsed: f64,
    pub error: String,
}

impl ScanProgress {
    pub fn new(target: &str) -> Self {
        Self {
            target: target.to_string(),
            phase: "waiting".into(),
            files: 0,
            dirs: 0,
            size: 0,
            rate: 0.0,
            mem_mb: 0.0,
            elapsed: 0.0,
            error: String::new(),
        }
    }
}

pub struct AppState {
    pub targets: Vec<ScanProgress>,
    pub start: Instant,
    pub running: bool,
    /// Set when the user asks to abort (q / Ctrl+C). Worker threads check this
    /// between targets so an abort stops queued work promptly.
    pub abort: bool,
}

impl AppState {
    pub fn new(target_names: &[String]) -> Self {
        Self {
            targets: target_names.iter().map(|n| ScanProgress::new(n)).collect(),
            start: Instant::now(),
            running: true,
            abort: false,
        }
    }

    /// Mutable handle to a target's progress row by name.
    pub fn target_mut(&mut self, name: &str) -> Option<&mut ScanProgress> {
        self.targets.iter_mut().find(|t| t.target == name)
    }
}

fn fmt_size(sz: u64) -> String {
    let sz = sz as f64;
    if sz >= 1e12 { format!("{:.1} TB", sz / 1e12) }
    else if sz >= 1e9 { format!("{:.1} GB", sz / 1e9) }
    else if sz >= 1e6 { format!("{:.1} MB", sz / 1e6) }
    else if sz >= 1e3 { format!("{:.1} KB", sz / 1e3) }
    else { format!("{:.0} B", sz) }
}

fn fmt_rate(rate: f64) -> String {
    if rate >= 1e6 { format!("{:.1}M/s", rate / 1e6) }
    else if rate >= 1e3 { format!("{:.1}K/s", rate / 1e3) }
    else { format!("{:.0}/s", rate) }
}

fn phase_style(phase: &str) -> (Style, &'static str) {
    match phase {
        "done" => (Style::default().fg(Color::Green), " ✓"),
        "error" => (Style::default().fg(Color::Red), " ✗"),
        "waiting" => (Style::default().fg(Color::Gray), " ○"),
        // waiting for the serialized build slot — distinct amber so it's clear
        // the target finished scanning and is queued, not hung.
        "queued" => (Style::default().fg(Color::Yellow), " ⏸"),
        // any active phase (scanning/building/treemap/merging/history)
        _ => (Style::default().fg(Color::Cyan), " ◇"),
    }
}

pub fn draw(frame: &mut Frame, state: &AppState) {
    let area = frame.area();
    // Degrade gracefully on very short terminals: drop the title/footer chrome
    // and render just the table so the layout never panics.
    if area.height < 8 {
        draw_table(frame, state, area);
        return;
    }

    let areas = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(0), Constraint::Length(3)])
        .split(area);

    let title = Block::default()
        .title(" check-disk ")
        .borders(Borders::ALL)
        .style(Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD));
    frame.render_widget(title, areas[0]);

    draw_table(frame, state, areas[1]);

    // Footer: overall totals + rate + hint.
    let (tot_files, tot_size): (u64, u64) = state
        .targets
        .iter()
        .fold((0, 0), |(f, s), t| (f + t.files, s + t.size));
    let tot_rate: f64 = state.targets.iter().map(|t| t.rate).sum();
    let elapsed = state.start.elapsed().as_secs_f64();
    let status = if state.abort {
        Span::styled("ABORTING…", Style::default().fg(Color::Red).add_modifier(Modifier::BOLD))
    } else if state.running {
        Span::styled("running", Style::default().fg(Color::Green))
    } else {
        Span::styled("done", Style::default().fg(Color::Green).add_modifier(Modifier::BOLD))
    };
    let footer = Paragraph::new(Line::from(vec![
        status,
        Span::raw("  "),
        Span::styled(
            format!("Σ {} files  {}  {}", tot_files, fmt_size(tot_size), fmt_rate(tot_rate)),
            Style::default().fg(Color::White),
        ),
        Span::raw("  "),
        Span::styled(format!("{:.1}s", elapsed), Style::default().fg(Color::Green)),
        Span::raw("  "),
        Span::styled("q / Ctrl+C to abort", Style::default().fg(Color::DarkGray)),
    ]))
    .block(Block::default().borders(Borders::ALL));
    frame.render_widget(footer, areas[2]);
}

fn draw_table(frame: &mut Frame, state: &AppState, area: ratatui::layout::Rect) {
    let header_cells = ["Target", "Phase", "Files", "Dirs", "Size", "Rate", "Mem", "Elapsed"]
        .iter()
        .map(|h| Cell::from(*h).style(Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)));
    let header = Row::new(header_cells)
        .style(Style::default().bg(Color::DarkGray))
        .height(1);

    let rows: Vec<Row> = state
        .targets
        .iter()
        .map(|t| {
            let (style, icon) = phase_style(&t.phase);
            let phase_disp = if t.phase == "error" && !t.error.is_empty() {
                format!("error: {}", t.error)
            } else {
                t.phase.clone()
            };
            Row::new(vec![
                Cell::from(format!("{}{}", icon, t.target)),
                Cell::from(phase_disp),
                Cell::from(format!("{}", t.files)),
                Cell::from(format!("{}", t.dirs)),
                Cell::from(fmt_size(t.size)),
                Cell::from(fmt_rate(t.rate)),
                Cell::from(format!("{:.0}M", t.mem_mb)),
                Cell::from(format!("{:.1}s", t.elapsed)),
            ])
            .style(style)
        })
        .collect();

    let table = Table::new(
        rows,
        vec![
            Constraint::Length(22),
            Constraint::Length(14),
            Constraint::Length(9),
            Constraint::Length(8),
            Constraint::Length(10),
            Constraint::Length(9),
            Constraint::Length(7),
            Constraint::Length(9),
        ],
    )
    .header(header)
    .block(Block::default().borders(Borders::ALL).title(" Targets "));

    frame.render_widget(table, area);
}
