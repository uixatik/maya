//! Interactive Terminal User Interface (TUI) for Maya Downloader.
//!
//! Built with `ratatui` and `crossterm`. Features:
//! - Alternate screen with RAII raw-mode cleanup
//! - Active downloads table (Name, Progress %, Speed, ETA, Status)
//! - Sparkline rendering aggregate download throughput history
//! - Interactive control: [p] Pause, [r] Resume, [c] Cancel, [a] Add URL, [q] Quit

use std::io::{self, Stdout};
use std::path::Path;
use std::time::{Duration, Instant};

use crossterm::cursor::{Hide, Show};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use maya_ipc::{default_ipc_endpoint, IpcClient, JobInfo, JobOptions, JobState};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, BorderType, Borders, Cell, Clear, Paragraph, Row, Sparkline, Table, TableState,
};
use ratatui::{Frame, Terminal};

/// Item representation for display in the TUI table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DownloadItem {
    pub id: String,
    pub name: String,
    pub url: String,
    pub downloaded_bytes: u64,
    pub total_bytes: Option<u64>,
    pub speed_bps: u64,
    pub status: JobState,
}

impl DownloadItem {
    pub fn from_job_info(info: &JobInfo) -> Self {
        let name = Path::new(&info.dest)
            .file_name()
            .and_then(|s| s.to_str())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| {
                info.url
                    .rsplit('/')
                    .next()
                    .unwrap_or(&info.url)
                    .split('?')
                    .next()
                    .unwrap_or(&info.url)
            })
            .to_string();

        Self {
            id: info.id.clone(),
            name,
            url: info.url.clone(),
            downloaded_bytes: info.downloaded_bytes,
            total_bytes: info.total_bytes,
            speed_bps: 0,
            status: info.status,
        }
    }

    pub fn progress_pct(&self) -> Option<f64> {
        self.total_bytes.and_then(|total| {
            if total == 0 {
                None
            } else {
                Some(((self.downloaded_bytes as f64) / (total as f64)) * 100.0)
            }
        })
    }

    pub fn eta_secs(&self) -> Option<u64> {
        if self.speed_bps == 0 {
            return None;
        }
        let total = self.total_bytes?;
        let remaining = total.saturating_sub(self.downloaded_bytes);
        Some(remaining / self.speed_bps)
    }
}

/// Format bytes-per-second into human-readable string.
pub fn format_speed(bps: u64) -> String {
    if bps == 0 {
        return "0 B/s".to_string();
    }
    const KB: f64 = 1024.0;
    const MB: f64 = 1024.0 * 1024.0;
    const GB: f64 = 1024.0 * 1024.0 * 1024.0;

    let val = bps as f64;
    if val >= GB {
        format!("{:.2} GB/s", val / GB)
    } else if val >= MB {
        format!("{:.1} MB/s", val / MB)
    } else if val >= KB {
        format!("{:.1} KB/s", val / KB)
    } else {
        format!("{bps} B/s")
    }
}

/// Format bytes count into human-readable string.
pub fn format_bytes(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = 1024.0 * 1024.0;
    const GB: f64 = 1024.0 * 1024.0 * 1024.0;

    let val = bytes as f64;
    if val >= GB {
        format!("{:.2} GB", val / GB)
    } else if val >= MB {
        format!("{:.1} MB", val / MB)
    } else if val >= KB {
        format!("{:.1} KB", val / KB)
    } else {
        format!("{bytes} B")
    }
}

/// Format remaining time into human-readable string.
pub fn format_eta(eta_secs: Option<u64>) -> String {
    match eta_secs {
        None => "--".to_string(),
        Some(s) if s < 60 => format!("{s}s"),
        Some(s) if s < 3600 => format!("{}m {:02}s", s / 60, s % 60),
        Some(s) => format!("{}h {:02}m", s / 3600, (s % 3600) / 60),
    }
}

/// Input prompt mode when adding a download URL in TUI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputMode {
    Normal,
    AddingUrl,
}

/// State of the interactive TUI application.
pub struct TuiApp {
    pub items: Vec<DownloadItem>,
    pub table_state: TableState,
    pub speed_history: Vec<u64>,
    pub max_history: usize,
    pub input_mode: InputMode,
    pub input_buffer: String,
    pub status_message: Option<(String, Instant)>,
    pub daemon_connected: bool,
    pub should_quit: bool,
}

impl Default for TuiApp {
    fn default() -> Self {
        Self::new()
    }
}

impl TuiApp {
    pub fn new() -> Self {
        let mut table_state = TableState::default();
        table_state.select(Some(0));

        Self {
            items: Vec::new(),
            table_state,
            speed_history: vec![0; 60],
            max_history: 120,
            input_mode: InputMode::Normal,
            input_buffer: String::new(),
            status_message: None,
            daemon_connected: false,
            should_quit: false,
        }
    }

    pub fn selected_item(&self) -> Option<&DownloadItem> {
        let idx = self.table_state.selected()?;
        self.items.get(idx)
    }

    pub fn select_next(&mut self) {
        if self.items.is_empty() {
            self.table_state.select(None);
            return;
        }
        let i = match self.table_state.selected() {
            Some(i) => {
                if i + 1 >= self.items.len() {
                    0
                } else {
                    i + 1
                }
            }
            None => 0,
        };
        self.table_state.select(Some(i));
    }

    pub fn select_prev(&mut self) {
        if self.items.is_empty() {
            self.table_state.select(None);
            return;
        }
        let i = match self.table_state.selected() {
            Some(i) => {
                if i == 0 {
                    self.items.len().saturating_sub(1)
                } else {
                    i - 1
                }
            }
            None => 0,
        };
        self.table_state.select(Some(i));
    }

    pub fn push_speed(&mut self, aggregate_speed: u64) {
        self.speed_history.push(aggregate_speed);
        if self.speed_history.len() > self.max_history {
            self.speed_history.remove(0);
        }
    }

    pub fn set_status(&mut self, msg: impl Into<String>) {
        self.status_message = Some((msg.into(), Instant::now()));
    }

    pub fn current_status(&self) -> Option<&str> {
        if let Some((msg, created)) = &self.status_message {
            if created.elapsed() < Duration::from_secs(5) {
                return Some(msg.as_str());
            }
        }
        None
    }

    pub fn update_from_jobs(&mut self, jobs: Vec<JobInfo>) {
        let prev_speeds: std::collections::HashMap<String, u64> = self
            .items
            .iter()
            .map(|it| (it.id.clone(), it.speed_bps))
            .collect();

        let mut updated = Vec::with_capacity(jobs.len());
        for job in jobs {
            let mut item = DownloadItem::from_job_info(&job);
            if let Some(&spd) = prev_speeds.get(&job.id) {
                item.speed_bps = spd;
            }
            updated.push(item);
        }

        self.items = updated;
        if self.items.is_empty() {
            self.table_state.select(None);
        } else if let Some(sel) = self.table_state.selected() {
            if sel >= self.items.len() {
                self.table_state.select(Some(self.items.len() - 1));
            }
        } else {
            self.table_state.select(Some(0));
        }
    }

    pub fn aggregate_speed(&self) -> u64 {
        self.items.iter().map(|it| it.speed_bps).sum()
    }
}

/// RAII Terminal guard that restores raw mode, alternate screen, and cursor on drop.
pub struct TerminalGuard {
    terminal: Terminal<CrosstermBackend<Stdout>>,
}

impl TerminalGuard {
    pub fn init() -> io::Result<Self> {
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        execute!(stdout, EnterAlternateScreen, Hide)?;
        let backend = CrosstermBackend::new(stdout);
        let terminal = Terminal::new(backend)?;
        Ok(Self { terminal })
    }

    pub fn terminal_mut(&mut self) -> &mut Terminal<CrosstermBackend<Stdout>> {
        &mut self.terminal
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = execute!(self.terminal.backend_mut(), Show, LeaveAlternateScreen);
        let _ = disable_raw_mode();
    }
}

/// Launch and run the interactive TUI.
pub async fn run_tui(custom_endpoint: Option<String>) -> io::Result<()> {
    let endpoint = custom_endpoint.unwrap_or_else(default_ipc_endpoint);
    let mut guard = TerminalGuard::init()?;
    let mut app = TuiApp::new();

    let mut maybe_client: Option<IpcClient> =
        IpcClient::connect_with_timeout(&endpoint, Duration::from_millis(500))
            .await
            .ok();

    app.daemon_connected = maybe_client.is_some();
    if app.daemon_connected {
        app.set_status("Connected to Maya daemon");
    } else {
        app.set_status("Daemon offline (press 'a' to add or start mayad)");
    }

    let mut last_tick = Instant::now();
    let tick_rate = Duration::from_millis(250);
    let mut last_reconnect_attempt = Instant::now();

    loop {
        // Draw frame
        guard.terminal_mut().draw(|f| ui(f, &mut app))?;

        let timeout = tick_rate.saturating_sub(last_tick.elapsed());
        if event::poll(timeout)? {
            if let Event::Key(key) = event::read()? {
                handle_key_event(&mut app, key, maybe_client.as_ref()).await;
            }
        }

        if app.should_quit {
            break;
        }

        if last_tick.elapsed() >= tick_rate {
            last_tick = Instant::now();

            // Try reconnecting if client is disconnected
            if maybe_client.is_none() && last_reconnect_attempt.elapsed() > Duration::from_secs(2) {
                last_reconnect_attempt = Instant::now();
                if let Ok(c) =
                    IpcClient::connect_with_timeout(&endpoint, Duration::from_millis(200)).await
                {
                    app.daemon_connected = true;
                    app.set_status("Connected to Maya daemon");
                    maybe_client = Some(c);
                }
            }

            // Poll daemon jobs
            if let Some(client) = &maybe_client {
                match client.list_jobs().await {
                    Ok(jobs) => {
                        app.daemon_connected = true;
                        app.update_from_jobs(jobs);
                    }
                    Err(_) => {
                        app.daemon_connected = false;
                        maybe_client = None;
                        app.set_status("Lost connection to Maya daemon");
                    }
                }
            }

            let agg = app.aggregate_speed();
            app.push_speed(agg);
        }
    }

    Ok(())
}

async fn handle_key_event(app: &mut TuiApp, key: KeyEvent, client: Option<&IpcClient>) {
    match app.input_mode {
        InputMode::Normal => match key.code {
            KeyCode::Char('q') | KeyCode::Esc => {
                app.should_quit = true;
            }
            KeyCode::Down | KeyCode::Char('j') => {
                app.select_next();
            }
            KeyCode::Up | KeyCode::Char('k') => {
                app.select_prev();
            }
            KeyCode::Char('a') => {
                app.input_mode = InputMode::AddingUrl;
                app.input_buffer.clear();
            }
            KeyCode::Char('p') => {
                if let Some(item) = app.selected_item() {
                    let id = item.id.clone();
                    if let Some(c) = client {
                        match c.pause_job(&id).await {
                            Ok(_) => app.set_status(format!("Paused job {id}")),
                            Err(e) => app.set_status(format!("Failed to pause: {e}")),
                        }
                    } else {
                        app.set_status("Cannot pause: daemon offline");
                    }
                }
            }
            KeyCode::Char('r') => {
                if let Some(item) = app.selected_item() {
                    let id = item.id.clone();
                    if let Some(c) = client {
                        match c.resume_job(&id).await {
                            Ok(_) => app.set_status(format!("Resumed job {id}")),
                            Err(e) => app.set_status(format!("Failed to resume: {e}")),
                        }
                    } else {
                        app.set_status("Cannot resume: daemon offline");
                    }
                }
            }
            KeyCode::Char('c') => {
                if let Some(item) = app.selected_item() {
                    let id = item.id.clone();
                    if let Some(c) = client {
                        match c.cancel_job(&id).await {
                            Ok(_) => app.set_status(format!("Cancelled job {id}")),
                            Err(e) => app.set_status(format!("Failed to cancel: {e}")),
                        }
                    } else {
                        app.set_status("Cannot cancel: daemon offline");
                    }
                }
            }
            _ => {}
        },
        InputMode::AddingUrl => match key.code {
            KeyCode::Enter => {
                let url = app.input_buffer.trim().to_string();
                if !url.is_empty() {
                    if let Some(c) = client {
                        match c.add_job(&url, JobOptions::default()).await {
                            Ok(job) => app.set_status(format!("Added job {}", job.id)),
                            Err(e) => app.set_status(format!("Failed to add job: {e}")),
                        }
                    } else {
                        app.set_status("Cannot add job: daemon offline");
                    }
                }
                app.input_buffer.clear();
                app.input_mode = InputMode::Normal;
            }
            KeyCode::Esc => {
                app.input_buffer.clear();
                app.input_mode = InputMode::Normal;
            }
            KeyCode::Backspace => {
                app.input_buffer.pop();
            }
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                app.input_buffer.push(c);
            }
            _ => {}
        },
    }
}

fn ui(f: &mut Frame, app: &mut TuiApp) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .margin(1)
        .constraints([
            Constraint::Length(3), // Header
            Constraint::Min(6),    // Downloads table
            Constraint::Length(5), // Sparkline
            Constraint::Length(3), // Keybindings footer
        ])
        .split(f.area());

    // 1. Header
    let daemon_tag = if app.daemon_connected {
        Span::styled(
            "● Daemon Online",
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        )
    } else {
        Span::styled(
            "○ Daemon Offline",
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        )
    };

    let agg_spd = format_speed(app.aggregate_speed());
    let active_count = app
        .items
        .iter()
        .filter(|it| it.status == JobState::Downloading)
        .count();

    let header_text = vec![Line::from(vec![
        Span::styled(
            "Maya TUI v0.0.1",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("   |   "),
        daemon_tag,
        Span::raw("   |   Active: "),
        Span::styled(active_count.to_string(), Style::default().fg(Color::Yellow)),
        Span::raw("   |   Total Speed: "),
        Span::styled(agg_spd, Style::default().fg(Color::Green)),
    ])];

    let header = Paragraph::new(header_text)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .title(" System Status "),
        )
        .alignment(Alignment::Left);
    f.render_widget(header, chunks[0]);

    // 2. Active downloads table
    let selected_style = Style::default()
        .bg(Color::DarkGray)
        .fg(Color::White)
        .add_modifier(Modifier::BOLD);
    let normal_style = Style::default().fg(Color::White);

    let header_cells = ["Name", "Progress %", "Speed", "ETA", "Status"]
        .iter()
        .map(|h| {
            Cell::from(*h).style(
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            )
        });
    let header_row = Row::new(header_cells).height(1).bottom_margin(1);

    let rows: Vec<Row> = app
        .items
        .iter()
        .map(|it| {
            let progress_str = match it.progress_pct() {
                Some(pct) => format!("{pct:.1}%"),
                None => format_bytes(it.downloaded_bytes),
            };
            let speed_str = format_speed(it.speed_bps);
            let eta_str = format_eta(it.eta_secs());

            let status_style = match it.status {
                JobState::Downloading => Style::default().fg(Color::Green),
                JobState::Queued => Style::default().fg(Color::Yellow),
                JobState::Paused => Style::default().fg(Color::Cyan),
                JobState::Completed => Style::default().fg(Color::Blue),
                JobState::Failed => Style::default().fg(Color::Red),
                JobState::Cancelled => Style::default().fg(Color::DarkGray),
            };

            let cells = vec![
                Cell::from(it.name.as_str()),
                Cell::from(progress_str),
                Cell::from(speed_str),
                Cell::from(eta_str),
                Cell::from(it.status.as_str()).style(status_style),
            ];
            Row::new(cells).style(normal_style).height(1)
        })
        .collect();

    let table = Table::new(
        rows,
        [
            Constraint::Percentage(35),
            Constraint::Percentage(15),
            Constraint::Percentage(15),
            Constraint::Percentage(15),
            Constraint::Percentage(20),
        ],
    )
    .header(header_row)
    .block(
        Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .title(format!(" Active Downloads ({}) ", app.items.len())),
    )
    .row_highlight_style(selected_style)
    .highlight_symbol(">> ");

    f.render_stateful_widget(table, chunks[1], &mut app.table_state);

    // 3. Sparkline of aggregate download speed
    let sparkline_title = format!(
        " Aggregate Download Speed History (Current: {}) ",
        format_speed(app.aggregate_speed())
    );
    let sparkline = Sparkline::default()
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .title(sparkline_title),
        )
        .data(&app.speed_history)
        .style(Style::default().fg(Color::Cyan));
    f.render_widget(sparkline, chunks[2]);

    // 4. Footer & Keybindings
    let footer_text = if let Some(status) = app.current_status() {
        Line::from(vec![
            Span::styled("INFO: ", Style::default().fg(Color::Cyan)),
            Span::raw(status),
        ])
    } else {
        Line::from(vec![
            Span::styled(
                "[p]",
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" Pause  "),
            Span::styled(
                "[r]",
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" Resume  "),
            Span::styled(
                "[c]",
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" Cancel  "),
            Span::styled(
                "[a]",
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" Add  "),
            Span::styled(
                "[q]",
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" Quit  "),
            Span::styled("[↑/↓]", Style::default().fg(Color::DarkGray)),
            Span::raw(" Select"),
        ])
    };

    let footer = Paragraph::new(footer_text)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .title(" Controls "),
        )
        .alignment(Alignment::Left);
    f.render_widget(footer, chunks[3]);

    // Input popup when adding URL
    if app.input_mode == InputMode::AddingUrl {
        let area = centered_rect(60, 20, f.area());
        f.render_widget(Clear, area);

        let input_block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Double)
            .title(" Add Download URL (Enter to confirm, Esc to cancel) ")
            .style(Style::default().fg(Color::Yellow));

        let input_paragraph = Paragraph::new(app.input_buffer.as_str())
            .block(input_block)
            .style(Style::default().fg(Color::White));

        f.render_widget(input_paragraph, area);
    }
}

fn centered_rect(percent_x: u16, percent_y: u16, r: Rect) -> Rect {
    let popup_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(r);

    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(popup_layout[1])[1]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tui_app_navigation() {
        let mut app = TuiApp::new();
        assert_eq!(app.selected_item(), None);

        app.items = vec![
            DownloadItem {
                id: "1".into(),
                name: "file1.iso".into(),
                url: "http://example.com/file1.iso".into(),
                downloaded_bytes: 100,
                total_bytes: Some(1000),
                speed_bps: 50,
                status: JobState::Downloading,
            },
            DownloadItem {
                id: "2".into(),
                name: "file2.iso".into(),
                url: "http://example.com/file2.iso".into(),
                downloaded_bytes: 500,
                total_bytes: Some(1000),
                speed_bps: 100,
                status: JobState::Paused,
            },
        ];

        app.table_state.select(Some(0));
        assert_eq!(app.selected_item().unwrap().id, "1");

        app.select_next();
        assert_eq!(app.selected_item().unwrap().id, "2");

        app.select_next();
        assert_eq!(app.selected_item().unwrap().id, "1");

        app.select_prev();
        assert_eq!(app.selected_item().unwrap().id, "2");
    }

    #[test]
    fn test_speed_history_and_aggregate() {
        let mut app = TuiApp::new();
        app.items = vec![
            DownloadItem {
                id: "1".into(),
                name: "file1".into(),
                url: "http://example.com/1".into(),
                downloaded_bytes: 0,
                total_bytes: None,
                speed_bps: 1024,
                status: JobState::Downloading,
            },
            DownloadItem {
                id: "2".into(),
                name: "file2".into(),
                url: "http://example.com/2".into(),
                downloaded_bytes: 0,
                total_bytes: None,
                speed_bps: 2048,
                status: JobState::Downloading,
            },
        ];

        assert_eq!(app.aggregate_speed(), 3072);
        app.push_speed(app.aggregate_speed());
        assert_eq!(*app.speed_history.last().unwrap(), 3072);
    }

    #[test]
    fn test_formatters() {
        assert_eq!(format_speed(0), "0 B/s");
        assert_eq!(format_speed(500), "500 B/s");
        assert_eq!(format_speed(1024 * 1024), "1.0 MB/s");

        assert_eq!(format_bytes(500), "500 B");
        assert_eq!(format_bytes(1024), "1.0 KB");
        assert_eq!(format_bytes(1024 * 1024 * 1024), "1.00 GB");

        assert_eq!(format_eta(None), "--");
        assert_eq!(format_eta(Some(45)), "45s");
        assert_eq!(format_eta(Some(125)), "2m 05s");
    }

    #[test]
    fn test_download_item_computations() {
        let item = DownloadItem {
            id: "1".into(),
            name: "test".into(),
            url: "http://example.com/test".into(),
            downloaded_bytes: 500,
            total_bytes: Some(1000),
            speed_bps: 100,
            status: JobState::Downloading,
        };

        assert_eq!(item.progress_pct(), Some(50.0));
        assert_eq!(item.eta_secs(), Some(5));
    }
}
