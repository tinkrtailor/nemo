pub mod actions;
pub mod cost;
pub mod diff_pane;
pub mod multi_view;
pub mod rounds_table;
pub mod summary;
pub mod themes;

use std::collections::{HashMap, VecDeque};
use std::io::{self, Stdout};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::event::{self, Event as CrosstermEvent, KeyCode, KeyEvent, KeyEventKind};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use futures::StreamExt;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap};
use tokio::sync::{mpsc, watch};

use crate::api_types::{InspectResponse, LoopSummary, PodIntrospectResponse, RoundSummary};
use crate::client::NemoClient;
use crate::commands::{inspect, ps, status};
use crate::config::HelmConfig;

use self::actions::LoopCommand;
use self::cost::{
    PricingConfig, calculate_loop_round_cost, format_cost, format_tokens, round_total_tokens,
};
use self::summary::approval_hints;
use self::themes::{Theme, ThemeName};

const MAX_LOG_LINES: usize = 500;
const POD_TAIL_LINES: u32 = 200;
const STATUS_FLASH_DURATION: Duration = Duration::from_secs(3);

#[derive(Debug)]
enum AppEvent {
    Input(KeyEvent),
    Resize,
    Status(Vec<LoopSummary>),
    StatusError(String),
    InspectLoaded(String, InspectResponse),
    InspectError(String, String),
    LogLine(uuid::Uuid, String),
    LogStatus(uuid::Uuid, String),
    IntrospectSnapshot(uuid::Uuid, PodIntrospectResponse),
    IntrospectStatus(uuid::Uuid, String),
    DiffLoaded(uuid::Uuid, String),
    DiffError(uuid::Uuid, String),
    BatchInspectLoaded(uuid::Uuid, InspectResponse),
}

/// Check if a loop state is terminal (shared across submodules).
pub(crate) fn is_terminal_state(state: &str) -> bool {
    matches!(
        state,
        "CONVERGED" | "FAILED" | "CANCELLED" | "HARDENED" | "SHIPPED"
    )
}

/// Active main view (FR-5/FR-6/FR-9).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MainView {
    Logs,
    Diff,
    MultiLoop,
    RoundsTable,
    RoundDetail,
}

/// Side-panel toggle states for the 'i' key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SidePanel {
    Closed,
    Inspect,
    Introspect,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LogSource {
    Persisted,
    AgentPod,
    SidecarPod,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct LogSelection {
    loop_id: uuid::Uuid,
    source: LogSource,
    job_name: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AppAction {
    None,
    Quit,
    SelectionChanged,
    SourceChanged,
    Trigger(LoopCommand),
    PanelToggle,
    ViewSwitch(MainView),
    ThemeCycle,
    RoundSelect,
    ScrollUp,
    ScrollDown,
    EscapeView,
}

#[derive(serde::Deserialize)]
struct ApproveActionResponse {
    loop_id: uuid::Uuid,
    state: String,
    approve_requested: bool,
}

#[derive(serde::Deserialize)]
struct ResumeActionResponse {
    loop_id: uuid::Uuid,
    state: String,
    resume_requested: bool,
}

#[derive(serde::Deserialize)]
struct CancelActionResponse {
    loop_id: uuid::Uuid,
    state: String,
    cancel_requested: bool,
}

#[derive(serde::Deserialize)]
struct ExtendActionResponse {
    loop_id: uuid::Uuid,
    prior_max_rounds: u32,
    new_max_rounds: u32,
    resumed_to_state: String,
}

#[derive(Debug)]
struct App {
    loops: Vec<LoopSummary>,
    list_state: ListState,
    selected_loop_id: Option<uuid::Uuid>,
    logs: VecDeque<Arc<String>>,
    log_source: LogSource,
    status_line: String,
    log_status: String,
    inspect_status: String,
    inspect: Option<InspectResponse>,
    team_view: bool,
    side_panel: SidePanel,
    introspect: Option<PodIntrospectResponse>,
    introspect_status: String,
    // Phase 2 additions
    theme_name: ThemeName,
    pricing: PricingConfig,
    main_view: MainView,
    // Diff pane (FR-5)
    diff_content: Option<String>,
    diff_status: String,
    diff_scroll: u16,
    // Rounds table (FR-9)
    rounds_table_selected: usize,
    rounds_table_scroll: usize,
    round_detail_scroll: usize,
    // Multi-loop logs (FR-6) - Arc<String> shared with main log buffer
    multi_logs: HashMap<uuid::Uuid, VecDeque<Arc<String>>>,
    // Batch inspect for header summary (FR-1)
    all_inspect: HashMap<uuid::Uuid, InspectResponse>,
    // Convergence detection (FR-4)
    previous_states: HashMap<uuid::Uuid, String>,
    // Row flash for convergence events (FR-4a)
    row_flash: HashMap<uuid::Uuid, Instant>,
    // Status flash (FR-3d)
    status_flash: Option<(String, Instant)>,
    // Cancel confirmation (FR-3b) with 3-second timeout
    cancel_confirm_at: Option<Instant>,
    // Bell queued for next render cycle (FR-4a)
    pending_bell: bool,
    // Desktop notifications (FR-4b) - read behind `desktop-notifications` feature
    #[allow(dead_code)]
    desktop_notifications: bool,
    // Active profile name for header display (FR-5b)
    profile_name: String,
}

impl App {
    fn new(team_view: bool, helm_config: &HelmConfig, profile_name: String) -> Self {
        // Load theme from config (FR-8a)
        let theme_name = helm_config
            .theme
            .as_deref()
            .and_then(|s| s.parse::<ThemeName>().ok())
            .unwrap_or(ThemeName::Dark);

        // Load pricing from nemo.toml if available (FR-7a)
        let pricing = match std::env::current_dir() {
            Ok(cwd) => {
                match crate::project_config::load_project_pricing(&cwd) {
                    Some(pricing_val) => {
                        // Wrap in a table with "pricing" key for from_toml
                        let mut wrapper = toml::map::Map::new();
                        wrapper.insert("pricing".to_string(), pricing_val);
                        PricingConfig::from_toml(&toml::Value::Table(wrapper))
                    }
                    None => PricingConfig::default(),
                }
            }
            Err(_) => PricingConfig::default(),
        };

        Self {
            loops: Vec::new(),
            list_state: ListState::default(),
            selected_loop_id: None,
            logs: VecDeque::new(),
            log_source: LogSource::Persisted,
            status_line: "Loading active loops...".to_string(),
            log_status: "Select a loop to tail persisted logs".to_string(),
            inspect_status: "Loading inspect data...".to_string(),
            inspect: None,
            team_view,
            side_panel: SidePanel::Closed,
            introspect: None,
            introspect_status: "Press 'i' to toggle introspect pane".to_string(),
            theme_name,
            pricing,
            main_view: MainView::Logs,
            diff_content: None,
            diff_status: "Press 'd' to load diff".to_string(),
            diff_scroll: 0,
            rounds_table_selected: 0,
            rounds_table_scroll: 0,
            round_detail_scroll: 0,
            multi_logs: HashMap::new(),
            all_inspect: HashMap::new(),
            previous_states: HashMap::new(),
            row_flash: HashMap::new(),
            status_flash: None,
            cancel_confirm_at: None,
            pending_bell: false,
            desktop_notifications: helm_config.desktop_notifications,
            profile_name,
        }
    }

    fn theme(&self) -> Theme {
        self.theme_name.theme()
    }

    fn selected_loop(&self) -> Option<&LoopSummary> {
        self.selected_loop_id.and_then(|loop_id| {
            self.loops
                .iter()
                .find(|loop_item| loop_item.loop_id == loop_id)
        })
    }

    fn current_log_selection(&self) -> Option<LogSelection> {
        self.selected_loop().map(|loop_item| LogSelection {
            loop_id: loop_item.loop_id,
            source: self.log_source,
            job_name: match self.log_source {
                LogSource::Persisted => None,
                LogSource::AgentPod | LogSource::SidecarPod => loop_item.active_job_name.clone(),
            },
        })
    }

    fn selected_branch(&self) -> Option<String> {
        self.selected_loop()
            .map(|loop_item| loop_item.branch.clone())
    }

    fn set_loops(&mut self, mut loops: Vec<LoopSummary>) {
        loops.sort_by(|left, right| right.updated_at.cmp(&left.updated_at));
        self.loops = loops;

        if self.loops.is_empty() {
            self.selected_loop_id = None;
            self.list_state.select(None);
            self.inspect = None;
            self.inspect_status = "No selected loop".to_string();
            self.status_line = if self.team_view {
                "No active loops across the team".to_string()
            } else {
                "No active loops for this engineer".to_string()
            };
            return;
        }

        if self.selected_loop_id.is_none_or(|selected| {
            self.loops
                .iter()
                .all(|loop_item| loop_item.loop_id != selected)
        }) {
            self.selected_loop_id = Some(self.loops[0].loop_id);
        }

        if let Some(selected_loop_id) = self.selected_loop_id {
            let selected_index = self
                .loops
                .iter()
                .position(|loop_item| loop_item.loop_id == selected_loop_id)
                .unwrap_or(0);
            self.list_state.select(Some(selected_index));
        }
    }

    /// Detect convergence/failure transitions and ring terminal bell (FR-4).
    fn detect_state_changes(&mut self) {
        // Collect transitions first to avoid borrow conflict
        let transitions: Vec<(uuid::Uuid, String, String, Option<String>)> = self
            .loops
            .iter()
            .filter_map(|loop_item| {
                let new_state = &loop_item.state;
                let old_state = self.previous_states.get(&loop_item.loop_id)?;
                if old_state != new_state && is_convergence_or_failure(new_state) {
                    let spec_name = loop_item
                        .spec_path
                        .rsplit('/')
                        .next()
                        .unwrap_or(&loop_item.spec_path)
                        .to_string();
                    Some((
                        loop_item.loop_id,
                        spec_name,
                        new_state.clone(),
                        loop_item.spec_pr_url.clone(),
                    ))
                } else {
                    None
                }
            })
            .collect();

        // Update previous_states
        for loop_item in &self.loops {
            self.previous_states
                .insert(loop_item.loop_id, loop_item.state.clone());
        }

        // Now apply side effects
        for (loop_id, spec_name, new_state, pr_url) in transitions {
            // Queue bell for next render cycle (FR-4a)
            self.pending_bell = true;
            // Set row flash for 1-second highlight (FR-4a)
            self.row_flash.insert(loop_id, Instant::now());
            // Build notification message with PR URL if available (FR-4a)
            let flash = match &pr_url {
                Some(url) => format!("✓ {new_state}: {spec_name} → {url}"),
                None => format!("✓ {new_state}: {spec_name}"),
            };
            self.set_status_flash(flash.clone());

            // FR-4b: desktop notification if enabled
            #[cfg(feature = "desktop-notifications")]
            if self.desktop_notifications {
                let body = match &pr_url {
                    Some(url) => format!("{spec_name}\n{url}"),
                    None => spec_name.clone(),
                };
                // Fire-and-forget: don't block TUI on notification delivery
                let _ = notify_rust::Notification::new()
                    .summary(&format!("nautiloop: {new_state}"))
                    .body(&body)
                    .show();
            }
        }
    }

    /// Check if a loop has an active row flash (within 1 second).
    fn has_row_flash(&self, loop_id: &uuid::Uuid) -> bool {
        self.row_flash
            .get(loop_id)
            .is_some_and(|when| when.elapsed() < Duration::from_secs(1))
    }

    fn set_status_flash(&mut self, message: String) {
        self.status_flash = Some((message, Instant::now()));
    }

    fn active_status_flash(&self) -> Option<&str> {
        self.status_flash.as_ref().and_then(|(msg, when)| {
            if when.elapsed() < STATUS_FLASH_DURATION {
                Some(msg.as_str())
            } else {
                None
            }
        })
    }

    fn move_selection(&mut self, delta: isize) -> bool {
        if self.loops.is_empty() {
            return false;
        }

        let current = self.list_state.selected().unwrap_or(0) as isize;
        let next = (current + delta).clamp(0, self.loops.len().saturating_sub(1) as isize) as usize;
        self.list_state.select(Some(next));
        let next_loop_id = self.loops[next].loop_id;
        let changed = self.selected_loop_id != Some(next_loop_id);
        self.selected_loop_id = Some(next_loop_id);
        changed
    }

    fn select_first(&mut self) -> bool {
        if self.loops.is_empty() {
            return false;
        }
        self.list_state.select(Some(0));
        let loop_id = self.loops[0].loop_id;
        let changed = self.selected_loop_id != Some(loop_id);
        self.selected_loop_id = Some(loop_id);
        changed
    }

    fn select_last(&mut self) -> bool {
        if self.loops.is_empty() {
            return false;
        }
        let last = self.loops.len() - 1;
        self.list_state.select(Some(last));
        let loop_id = self.loops[last].loop_id;
        let changed = self.selected_loop_id != Some(loop_id);
        self.selected_loop_id = Some(loop_id);
        changed
    }

    fn reset_logs(&mut self) {
        self.logs.clear();
        self.log_status = match self.selected_loop() {
            Some(loop_item) => match self.log_source {
                LogSource::Persisted => {
                    format!("Connecting persisted logs for {}", loop_item.loop_id)
                }
                LogSource::AgentPod => format!("Polling agent pod logs for {}", loop_item.loop_id),
                LogSource::SidecarPod => {
                    format!("Polling auth-sidecar logs for {}", loop_item.loop_id)
                }
            },
            None => "Select a loop to tail logs".to_string(),
        };
    }

    fn reset_inspect(&mut self) {
        self.inspect = None;
        self.inspect_status = self
            .selected_loop()
            .and_then(|loop_item| {
                if loop_item.branch.is_empty() {
                    None
                } else {
                    Some(format!("Loading inspect data for {}", loop_item.branch))
                }
            })
            .unwrap_or_else(|| {
                if self.selected_loop().is_some() {
                    "No branch available for this loop".to_string()
                } else {
                    "Select a loop to inspect".to_string()
                }
            });
    }

    fn cycle_log_source(&mut self) {
        self.log_source = match self.log_source {
            LogSource::Persisted => LogSource::AgentPod,
            LogSource::AgentPod => LogSource::SidecarPod,
            LogSource::SidecarPod => LogSource::Persisted,
        };
    }

    fn cycle_side_panel(&mut self) {
        self.side_panel = match self.side_panel {
            SidePanel::Closed => SidePanel::Inspect,
            SidePanel::Inspect => SidePanel::Introspect,
            SidePanel::Introspect => SidePanel::Closed,
        };
    }

    fn push_log_line(&mut self, line: Arc<String>) {
        if self.logs.len() == MAX_LOG_LINES {
            self.logs.pop_front();
        }
        self.logs.push_back(line);
    }

    fn is_harden_loop(&self) -> bool {
        self.selected_loop()
            .map(|l| l.kind == "harden")
            .unwrap_or(false)
    }

    fn handle_input(&mut self, key: KeyEvent) -> AppAction {
        // Cancel confirmation mode (FR-3d): only 'y' within 3s confirms
        if let Some(confirm_at) = self.cancel_confirm_at.take() {
            if confirm_at.elapsed() > Duration::from_secs(3) {
                self.set_status_flash("cancel confirmation timed out".to_string());
                return AppAction::None;
            }
            if key.code == KeyCode::Char('y') {
                return AppAction::Trigger(LoopCommand::Cancel);
            }
            self.set_status_flash("cancel aborted".to_string());
            return AppAction::None;
        }

        // View-specific input handling
        match self.main_view {
            MainView::RoundsTable => return self.handle_rounds_table_input(key),
            MainView::RoundDetail => return self.handle_round_detail_input(key),
            MainView::Diff => match key.code {
                KeyCode::Esc | KeyCode::Char('d') => return AppAction::EscapeView,
                KeyCode::PageUp | KeyCode::Char('b') => return AppAction::ScrollUp,
                KeyCode::PageDown | KeyCode::Char('f') => return AppAction::ScrollDown,
                _ => {}
            },
            MainView::MultiLoop => {
                if matches!(key.code, KeyCode::Esc | KeyCode::Char('m')) {
                    return AppAction::EscapeView;
                }
            }
            _ => {}
        }

        match key.code {
            KeyCode::Char('q') => AppAction::Quit,
            KeyCode::Esc => {
                if self.main_view != MainView::Logs {
                    AppAction::EscapeView
                } else {
                    AppAction::Quit
                }
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if self.move_selection(1) {
                    AppAction::SelectionChanged
                } else {
                    AppAction::None
                }
            }
            KeyCode::Up | KeyCode::Char('k') => {
                if self.move_selection(-1) {
                    AppAction::SelectionChanged
                } else {
                    AppAction::None
                }
            }
            KeyCode::Char('g') => {
                if self.select_first() {
                    AppAction::SelectionChanged
                } else {
                    AppAction::None
                }
            }
            KeyCode::Char('G') | KeyCode::End => {
                if self.select_last() {
                    AppAction::SelectionChanged
                } else {
                    AppAction::None
                }
            }
            KeyCode::Home => {
                if self.select_first() {
                    AppAction::SelectionChanged
                } else {
                    AppAction::None
                }
            }
            KeyCode::Char('l') => {
                self.cycle_log_source();
                AppAction::SourceChanged
            }
            KeyCode::Char('i') => {
                self.cycle_side_panel();
                AppAction::PanelToggle
            }
            // FR-3a: action keybinds
            KeyCode::Char('a') => AppAction::Trigger(LoopCommand::Approve),
            KeyCode::Char('r') => AppAction::Trigger(LoopCommand::Resume),
            KeyCode::Char('x') => {
                // FR-3d: cancel requires y confirmation within 3s
                if self.selected_loop().is_some_and(|loop_item| {
                    actions::validate_action(LoopCommand::Cancel, loop_item).is_ok()
                }) {
                    self.cancel_confirm_at = Some(Instant::now());
                    self.set_status_flash("press y within 3s to confirm cancel".to_string());
                    return AppAction::None;
                }
                self.set_status_flash("cancel not available in current state".to_string());
                AppAction::None
            }
            KeyCode::Char('e') => AppAction::Trigger(LoopCommand::Extend),
            KeyCode::Char('o') => AppAction::Trigger(LoopCommand::OpenPr),
            // View switching
            KeyCode::Char('d') => AppAction::ViewSwitch(MainView::Diff),
            KeyCode::Char('m') => AppAction::ViewSwitch(MainView::MultiLoop),
            KeyCode::Char('R') => AppAction::ViewSwitch(MainView::RoundsTable),
            KeyCode::Char('T') => AppAction::ThemeCycle,
            _ => AppAction::None,
        }
    }

    fn handle_rounds_table_input(&mut self, key: KeyEvent) -> AppAction {
        match key.code {
            KeyCode::Esc | KeyCode::Char('R') => AppAction::EscapeView,
            KeyCode::Down | KeyCode::Char('j') => {
                if self
                    .inspect
                    .as_ref()
                    .is_some_and(|inspect| self.rounds_table_selected + 1 < inspect.rounds.len())
                {
                    self.rounds_table_selected += 1;
                }
                AppAction::None
            }
            KeyCode::Up | KeyCode::Char('k') => {
                if self.rounds_table_selected > 0 {
                    self.rounds_table_selected -= 1;
                }
                AppAction::None
            }
            KeyCode::Enter => AppAction::RoundSelect,
            _ => AppAction::None,
        }
    }

    fn handle_round_detail_input(&mut self, key: KeyEvent) -> AppAction {
        match key.code {
            KeyCode::Esc => AppAction::EscapeView,
            KeyCode::PageUp | KeyCode::Char('b') => AppAction::ScrollUp,
            KeyCode::PageDown | KeyCode::Char('f') => AppAction::ScrollDown,
            _ => AppAction::None,
        }
    }
}

fn is_convergence_or_failure(state: &str) -> bool {
    matches!(state, "CONVERGED" | "FAILED" | "HARDENED" | "SHIPPED")
}

enum StreamOutcome {
    HistoricalComplete,
    Ended(String),
    Disconnected,
}

pub async fn run(
    client: &NemoClient,
    engineer: &str,
    team: bool,
    helm_config: &HelmConfig,
    profile_name: &str,
) -> Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    terminal.clear()?;

    let result = run_app(
        &mut terminal,
        client.clone(),
        engineer.to_string(),
        team,
        helm_config,
        profile_name.to_string(),
    )
    .await;

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    result
}

async fn run_app(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    client: NemoClient,
    engineer: String,
    team: bool,
    helm_config: &HelmConfig,
    profile_name: String,
) -> Result<()> {
    let (event_tx, mut event_rx) = mpsc::unbounded_channel();
    let (selection_tx, selection_rx) = watch::channel(None::<LogSelection>);
    let (inspect_tx, inspect_rx) = watch::channel(None::<String>);
    let (introspect_tx, introspect_rx) = watch::channel(None::<uuid::Uuid>);
    let (diff_tx, diff_rx) = watch::channel(None::<(uuid::Uuid, String)>);
    let (loops_tx, loops_rx) = watch::channel(Vec::<LoopSummary>::new());

    spawn_input_task(event_tx.clone());
    spawn_status_task(client.clone(), engineer.clone(), team, event_tx.clone());
    spawn_log_task(client.clone(), selection_rx, event_tx.clone());
    spawn_inspect_task(client.clone(), inspect_rx, event_tx.clone());
    spawn_introspect_task(client.clone(), introspect_rx, event_tx.clone());
    spawn_diff_task(client.clone(), diff_rx, event_tx.clone());
    spawn_batch_inspect_task(client.clone(), loops_rx, event_tx.clone());
    spawn_background_log_task(
        client.clone(),
        loops_tx.subscribe(),
        selection_tx.subscribe(),
        event_tx.clone(),
    );

    let mut app = App::new(team, helm_config, profile_name);

    loop {
        terminal.draw(|frame| render(frame, &mut app))?;

        // Emit queued bell after render to avoid bypassing ratatui's pipeline (FR-4a)
        if app.pending_bell {
            app.pending_bell = false;
            let _ = crossterm::execute!(io::stdout(), crossterm::style::Print('\x07'));
        }

        let Some(event) = event_rx.recv().await else {
            break;
        };

        let previous_log_selection = app.current_log_selection();
        let previous_branch = app.selected_branch();
        match event {
            AppEvent::Input(key) => match app.handle_input(key) {
                AppAction::Quit => break,
                AppAction::SelectionChanged => {
                    app.reset_logs();
                    app.reset_inspect();
                    app.introspect = None;
                    app.diff_content = None;
                    app.diff_status = "Loading diff...".to_string();
                    app.rounds_table_selected = 0;
                    app.rounds_table_scroll = 0;
                    if app.side_panel == SidePanel::Introspect {
                        app.introspect_status = "Loading...".to_string();
                        let _ = introspect_tx.send(app.selected_loop_id);
                    } else {
                        app.introspect_status = "Press i to toggle introspect pane".to_string();
                    }
                    let _ = selection_tx.send(app.current_log_selection());
                    let _ = inspect_tx.send(app.selected_branch());
                    // Trigger diff load if in diff view
                    if app.main_view == MainView::Diff
                        && let Some(loop_item) = app.selected_loop()
                    {
                        let _ = diff_tx.send(Some((loop_item.loop_id, loop_item.branch.clone())));
                    }
                }
                AppAction::SourceChanged => {
                    app.reset_logs();
                    let _ = selection_tx.send(app.current_log_selection());
                }
                AppAction::PanelToggle => match app.side_panel {
                    SidePanel::Introspect => {
                        app.introspect = None;
                        app.introspect_status = "Loading...".to_string();
                        let _ = introspect_tx.send(app.selected_loop_id);
                    }
                    SidePanel::Inspect => {
                        app.reset_inspect();
                        let _ = inspect_tx.send(app.selected_branch());
                        let _ = introspect_tx.send(None);
                    }
                    SidePanel::Closed => {
                        let _ = introspect_tx.send(None);
                    }
                },
                AppAction::ViewSwitch(view) => {
                    app.main_view = view;
                    match view {
                        MainView::Diff => {
                            app.diff_scroll = 0;
                            let diff_target =
                                app.selected_loop().map(|l| (l.loop_id, l.branch.clone()));
                            if let Some((loop_id, branch)) = diff_target {
                                app.diff_status = "Loading diff...".to_string();
                                let _ = diff_tx.send(Some((loop_id, branch)));
                            }
                        }
                        MainView::RoundsTable => {
                            app.rounds_table_selected = 0;
                            app.rounds_table_scroll = 0;
                        }
                        MainView::RoundDetail => {
                            app.round_detail_scroll = 0;
                        }
                        _ => {}
                    }
                }
                AppAction::EscapeView => match app.main_view {
                    MainView::RoundDetail => app.main_view = MainView::RoundsTable,
                    _ => app.main_view = MainView::Logs,
                },
                AppAction::ThemeCycle => {
                    app.theme_name = app.theme_name.cycle();
                    app.set_status_flash(format!("theme: {}", app.theme_name.label()));
                }
                AppAction::RoundSelect => {
                    // Enter round detail view
                    if app.inspect.as_ref().is_some_and(|i| !i.rounds.is_empty()) {
                        app.main_view = MainView::RoundDetail;
                        app.round_detail_scroll = 0;
                    }
                }
                AppAction::ScrollUp => match app.main_view {
                    MainView::Diff => {
                        app.diff_scroll = app.diff_scroll.saturating_sub(10);
                    }
                    MainView::RoundDetail => {
                        app.round_detail_scroll = app.round_detail_scroll.saturating_sub(5);
                    }
                    _ => {}
                },
                AppAction::ScrollDown => match app.main_view {
                    MainView::Diff => {
                        app.diff_scroll = app.diff_scroll.saturating_add(10);
                    }
                    MainView::RoundDetail => {
                        app.round_detail_scroll = app.round_detail_scroll.saturating_add(5);
                    }
                    _ => {}
                },
                AppAction::Trigger(command) => {
                    if let Some(loop_item) = app.selected_loop() {
                        // FR-3c: validate before sending
                        if let Err(reason) = actions::validate_action(command, loop_item) {
                            app.set_status_flash(reason);
                        } else if command == LoopCommand::OpenPr {
                            // OpenPr is client-side: open URL in browser
                            if let Some(url) = loop_item.spec_pr_url.clone() {
                                match open::that(&url) {
                                    Ok(()) => app.set_status_flash(format!("opened {url}")),
                                    Err(e) => app.set_status_flash(format!("open PR failed: {e}")),
                                }
                            }
                        } else {
                            let loop_id = loop_item.loop_id;
                            app.set_status_flash(format!(
                                "sending {} for {loop_id}",
                                command.verb()
                            ));
                            match perform_loop_action(&client, command, loop_id).await {
                                Ok(message) => {
                                    app.set_status_flash(message);
                                }
                                Err(error) => {
                                    app.set_status_flash(format!(
                                        "{} failed for {loop_id}: {error}",
                                        command.verb()
                                    ));
                                }
                            }

                            match status::fetch(&client, &engineer, team).await {
                                Ok(response) => {
                                    app.set_loops(response.loops);
                                    app.detect_state_changes();
                                    if app.current_log_selection() != previous_log_selection {
                                        app.reset_logs();
                                        let _ = selection_tx.send(app.current_log_selection());
                                    }
                                    if app.selected_branch() != previous_branch {
                                        app.reset_inspect();
                                        let _ = inspect_tx.send(app.selected_branch());
                                    }
                                }
                                Err(error) => {
                                    app.set_status_flash(format!(
                                        "{} sent, but refresh failed: {error}",
                                        command.verb()
                                    ));
                                }
                            }
                        }
                    } else {
                        app.set_status_flash(format!("No loop selected for {}", command.verb()));
                    }
                }
                AppAction::None => {}
            },
            AppEvent::Resize => {}
            AppEvent::Status(loops) => {
                // Share loop list with batch inspect task
                let _ = loops_tx.send(loops.clone());
                app.set_loops(loops);
                app.detect_state_changes();
                if app.current_log_selection() != previous_log_selection {
                    app.reset_logs();
                    let _ = selection_tx.send(app.current_log_selection());
                }
                if app.selected_branch() != previous_branch {
                    app.reset_inspect();
                    let _ = inspect_tx.send(app.selected_branch());
                }
            }
            AppEvent::StatusError(error) => {
                app.status_line = format!("status refresh failed: {error}");
            }
            AppEvent::InspectLoaded(branch, inspect_data) => {
                if app.selected_branch().as_deref() == Some(branch.as_str()) {
                    app.inspect_status = format!("inspect synced for {branch}");
                    app.inspect = Some(inspect_data);
                }
            }
            AppEvent::InspectError(branch, error) => {
                if app.selected_branch().as_deref() == Some(branch.as_str()) {
                    app.inspect = None;
                    app.inspect_status = format!("inspect refresh failed: {error}");
                }
            }
            AppEvent::LogLine(loop_id, line) => {
                let line = Arc::new(line);
                if Some(loop_id) == app.selected_loop_id {
                    app.push_log_line(Arc::clone(&line));
                }
                // Also store in multi_logs for multi-loop view (FR-6)
                let entry = app.multi_logs.entry(loop_id).or_default();
                if entry.len() >= MAX_LOG_LINES {
                    entry.pop_front();
                }
                entry.push_back(line);
            }
            AppEvent::LogStatus(loop_id, status_line) => {
                if Some(loop_id) == app.selected_loop_id {
                    app.log_status = status_line;
                }
            }
            AppEvent::IntrospectSnapshot(loop_id, snapshot) => {
                if Some(loop_id) == app.selected_loop_id {
                    app.introspect_status = format!("updated {}", snapshot.collected_at);
                    app.introspect = Some(snapshot);
                }
            }
            AppEvent::IntrospectStatus(loop_id, status_msg) => {
                if Some(loop_id) == app.selected_loop_id {
                    app.introspect_status = status_msg;
                }
            }
            AppEvent::DiffLoaded(loop_id, diff) => {
                if Some(loop_id) == app.selected_loop_id {
                    app.diff_status = if diff.is_empty() {
                        "No changes".to_string()
                    } else {
                        "diff loaded".to_string()
                    };
                    app.diff_content = Some(diff);
                }
            }
            AppEvent::DiffError(loop_id, error) => {
                if Some(loop_id) == app.selected_loop_id {
                    app.diff_content = None;
                    app.diff_status = format!("diff failed: {error}");
                }
            }
            AppEvent::BatchInspectLoaded(loop_id, inspect_data) => {
                app.all_inspect.insert(loop_id, inspect_data);
            }
        }
    }

    Ok(())
}

impl LogSource {
    fn label(self) -> &'static str {
        match self {
            Self::Persisted => "persisted",
            Self::AgentPod => "agent",
            Self::SidecarPod => "sidecar",
        }
    }
}

async fn perform_loop_action(
    client: &NemoClient,
    command: LoopCommand,
    loop_id: uuid::Uuid,
) -> Result<String> {
    match command {
        LoopCommand::Approve => {
            let response: ApproveActionResponse = client
                .post(&format!("/approve/{loop_id}"), &serde_json::json!({}))
                .await?;
            Ok(if response.approve_requested {
                format!("approved {} ({})", response.loop_id, response.state)
            } else {
                format!(
                    "approve not applicable for {} ({})",
                    response.loop_id, response.state
                )
            })
        }
        LoopCommand::Resume => {
            let response: ResumeActionResponse = client
                .post(&format!("/resume/{loop_id}"), &serde_json::json!({}))
                .await?;
            Ok(if response.resume_requested {
                format!(
                    "resume requested for {} ({})",
                    response.loop_id, response.state
                )
            } else {
                format!(
                    "resume not applicable for {} ({})",
                    response.loop_id, response.state
                )
            })
        }
        LoopCommand::Cancel => {
            let response: CancelActionResponse =
                client.delete(&format!("/cancel/{loop_id}")).await?;
            Ok(if response.cancel_requested {
                format!(
                    "cancel requested for {} ({})",
                    response.loop_id, response.state
                )
            } else {
                format!(
                    "cancel not applicable for {} ({})",
                    response.loop_id, response.state
                )
            })
        }
        LoopCommand::Extend => {
            let response: ExtendActionResponse = client
                .post(
                    &format!("/extend/{loop_id}"),
                    &serde_json::json!({"add_rounds": 10}),
                )
                .await?;
            Ok(format!(
                "extended {} by {} rounds (now {} max, {})",
                response.loop_id,
                response.new_max_rounds - response.prior_max_rounds,
                response.new_max_rounds,
                response.resumed_to_state,
            ))
        }
        LoopCommand::OpenPr => {
            // OpenPr is handled client-side before this function is called.
            // This branch should not be reached.
            Ok("open PR handled client-side".to_string())
        }
    }
}

fn spawn_input_task(event_tx: mpsc::UnboundedSender<AppEvent>) {
    tokio::task::spawn_blocking(move || {
        loop {
            match event::poll(Duration::from_millis(250)) {
                Ok(true) => match event::read() {
                    Ok(CrosstermEvent::Key(key)) if key.kind == KeyEventKind::Press => {
                        if event_tx.send(AppEvent::Input(key)).is_err() {
                            break;
                        }
                    }
                    Ok(CrosstermEvent::Resize(_, _)) => {
                        if event_tx.send(AppEvent::Resize).is_err() {
                            break;
                        }
                    }
                    Ok(_) => {}
                    Err(_) => {
                        if event_tx
                            .send(AppEvent::StatusError(
                                "terminal input stream failed".to_string(),
                            ))
                            .is_err()
                        {
                            break;
                        }
                    }
                },
                Ok(false) => {}
                Err(_) => {
                    if event_tx
                        .send(AppEvent::StatusError(
                            "terminal input polling failed".to_string(),
                        ))
                        .is_err()
                    {
                        break;
                    }
                }
            }
        }
    });
}

fn spawn_inspect_task(
    client: NemoClient,
    mut branch_rx: watch::Receiver<Option<String>>,
    event_tx: mpsc::UnboundedSender<AppEvent>,
) {
    tokio::spawn(async move {
        let mut current_task: Option<tokio::task::JoinHandle<()>> = None;

        loop {
            if let Some(task) = current_task.take() {
                task.abort();
            }

            if let Some(branch) = branch_rx.borrow().clone() {
                let client = client.clone();
                let event_tx = event_tx.clone();
                current_task = Some(tokio::spawn(async move {
                    poll_inspect_for_branch(client, branch, event_tx).await;
                }));
            }

            if branch_rx.changed().await.is_err() {
                if let Some(task) = current_task {
                    task.abort();
                }
                break;
            }
        }
    });
}

async fn poll_inspect_for_branch(
    client: NemoClient,
    branch: String,
    event_tx: mpsc::UnboundedSender<AppEvent>,
) {
    loop {
        match inspect::fetch(&client, &branch).await {
            Ok(response) => {
                if event_tx
                    .send(AppEvent::InspectLoaded(branch.clone(), response))
                    .is_err()
                {
                    return;
                }
            }
            Err(error) => {
                if event_tx
                    .send(AppEvent::InspectError(branch.clone(), error.to_string()))
                    .is_err()
                {
                    return;
                }
            }
        }

        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

fn spawn_introspect_task(
    client: NemoClient,
    mut loop_id_rx: watch::Receiver<Option<uuid::Uuid>>,
    event_tx: mpsc::UnboundedSender<AppEvent>,
) {
    tokio::spawn(async move {
        let mut current_task: Option<tokio::task::JoinHandle<()>> = None;

        loop {
            if let Some(task) = current_task.take() {
                task.abort();
            }

            if let Some(loop_id) = *loop_id_rx.borrow_and_update() {
                let client = client.clone();
                let event_tx = event_tx.clone();
                current_task = Some(tokio::spawn(async move {
                    poll_introspect_for_loop(client, loop_id, event_tx).await;
                }));
            }

            if loop_id_rx.changed().await.is_err() {
                if let Some(task) = current_task {
                    task.abort();
                }
                break;
            }

            // Debounce: after a change arrives, wait 300ms for additional rapid changes
            loop {
                match tokio::time::timeout(Duration::from_millis(300), loop_id_rx.changed()).await {
                    Ok(Ok(())) => continue,
                    Ok(Err(_)) => {
                        if let Some(task) = current_task {
                            task.abort();
                        }
                        return;
                    }
                    Err(_) => break,
                }
            }
        }
    });
}

async fn poll_introspect_for_loop(
    client: NemoClient,
    loop_id: uuid::Uuid,
    event_tx: mpsc::UnboundedSender<AppEvent>,
) {
    let loop_id_str = loop_id.to_string();
    loop {
        match ps::fetch(&client, &loop_id_str).await {
            Ok(ps::FetchResult::Ok(snapshot)) => {
                if event_tx
                    .send(AppEvent::IntrospectSnapshot(loop_id, *snapshot))
                    .is_err()
                {
                    return;
                }
            }
            Ok(ps::FetchResult::Terminal(msg)) => {
                let _ = event_tx.send(AppEvent::IntrospectStatus(
                    loop_id,
                    format!("Pod gone. {msg}"),
                ));
                return;
            }
            Ok(ps::FetchResult::NotReady(msg)) => {
                let _ = event_tx.send(AppEvent::IntrospectStatus(loop_id, msg));
            }
            Ok(ps::FetchResult::Timeout) => {
                let _ = event_tx.send(AppEvent::IntrospectStatus(
                    loop_id,
                    "introspect timeout, retrying...".to_string(),
                ));
            }
            Err(e) => {
                let _ = event_tx.send(AppEvent::IntrospectStatus(
                    loop_id,
                    format!("introspect error: {e}"),
                ));
            }
        }

        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

fn spawn_status_task(
    client: NemoClient,
    engineer: String,
    team: bool,
    event_tx: mpsc::UnboundedSender<AppEvent>,
) {
    tokio::spawn(async move {
        loop {
            match status::fetch(&client, &engineer, team).await {
                Ok(response) => {
                    if event_tx.send(AppEvent::Status(response.loops)).is_err() {
                        break;
                    }
                }
                Err(error) => {
                    if event_tx
                        .send(AppEvent::StatusError(error.to_string()))
                        .is_err()
                    {
                        break;
                    }
                }
            }

            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    });
}

fn spawn_log_task(
    client: NemoClient,
    mut selection_rx: watch::Receiver<Option<LogSelection>>,
    event_tx: mpsc::UnboundedSender<AppEvent>,
) {
    tokio::spawn(async move {
        let mut current_task: Option<tokio::task::JoinHandle<()>> = None;

        loop {
            if let Some(task) = current_task.take() {
                task.abort();
            }

            if let Some(selection) = selection_rx.borrow().clone() {
                let client = client.clone();
                let event_tx = event_tx.clone();
                current_task = Some(tokio::spawn(async move {
                    stream_logs_for_selection(client, selection, event_tx).await;
                }));
            }

            if selection_rx.changed().await.is_err() {
                if let Some(task) = current_task {
                    task.abort();
                }
                break;
            }
        }
    });
}

/// Spawn diff polling task (FR-5).
fn spawn_diff_task(
    client: NemoClient,
    mut diff_rx: watch::Receiver<Option<(uuid::Uuid, String)>>,
    event_tx: mpsc::UnboundedSender<AppEvent>,
) {
    tokio::spawn(async move {
        let mut current_task: Option<tokio::task::JoinHandle<()>> = None;

        loop {
            if let Some(task) = current_task.take() {
                task.abort();
            }

            if let Some((loop_id, _branch)) = diff_rx.borrow().clone() {
                let client = client.clone();
                let event_tx = event_tx.clone();
                current_task = Some(tokio::spawn(async move {
                    let path = format!("/diff/{loop_id}");
                    match client.get::<crate::api_types::DiffResponse>(&path).await {
                        Ok(resp) => {
                            let _ = event_tx.send(AppEvent::DiffLoaded(loop_id, resp.diff));
                        }
                        Err(e) => {
                            let _ = event_tx.send(AppEvent::DiffError(loop_id, e.to_string()));
                        }
                    }
                    // Don't poll continuously for diff, just fetch once
                }));
            }

            if diff_rx.changed().await.is_err() {
                if let Some(task) = current_task {
                    task.abort();
                }
                break;
            }
        }
    });
}

/// Spawn batch inspect polling for all active loops (FR-1b header summary).
///
/// Receives loop list from the main status poller via a watch channel
/// to avoid duplicate status fetches. Parallelizes inspect calls.
fn spawn_batch_inspect_task(
    client: NemoClient,
    loops_rx: watch::Receiver<Vec<LoopSummary>>,
    event_tx: mpsc::UnboundedSender<AppEvent>,
) {
    tokio::spawn(async move {
        let mut loops_rx = loops_rx;
        loop {
            // Collect branches to inspect from the shared loop list
            let targets: Vec<(uuid::Uuid, String)> = loops_rx
                .borrow()
                .iter()
                .filter(|l| !l.branch.is_empty())
                .map(|l| (l.loop_id, l.branch.clone()))
                .collect();

            if !targets.is_empty() {
                // Fetch all inspects in parallel
                let futures: Vec<_> = targets
                    .iter()
                    .map(|(loop_id, branch)| {
                        let client = client.clone();
                        let branch = branch.clone();
                        let loop_id = *loop_id;
                        async move {
                            match inspect::fetch(&client, &branch).await {
                                Ok(data) => Some((loop_id, data)),
                                Err(_) => None,
                            }
                        }
                    })
                    .collect();

                let results = futures::future::join_all(futures).await;
                for result in results.into_iter().flatten() {
                    if event_tx
                        .send(AppEvent::BatchInspectLoaded(result.0, result.1))
                        .is_err()
                    {
                        return;
                    }
                }
            }

            tokio::time::sleep(Duration::from_secs(2)).await;

            // Wait for a change in the loop list or just proceed after sleep
            // This ensures we pick up new loops when the status poller updates
            let _ = tokio::time::timeout(Duration::from_secs(1), loops_rx.changed()).await;
        }
    });
}

/// Spawn background log streams for non-selected active loops (FR-6 multi-loop view).
///
/// Watches the loop list and maintains persisted-log SSE connections for all
/// active loops that aren't the currently-selected one (which is handled by the
/// primary log task). This ensures multi-loop view has log data for all loops.
fn spawn_background_log_task(
    client: NemoClient,
    mut loops_rx: watch::Receiver<Vec<LoopSummary>>,
    selection_rx: watch::Receiver<Option<LogSelection>>,
    event_tx: mpsc::UnboundedSender<AppEvent>,
) {
    tokio::spawn(async move {
        let mut active_tasks: HashMap<uuid::Uuid, tokio::task::JoinHandle<()>> = HashMap::new();

        loop {
            // Determine which loop is currently selected (handled by primary log task)
            let selected_id = selection_rx.borrow().as_ref().map(|s| s.loop_id);

            let current_loops: Vec<uuid::Uuid> = loops_rx
                .borrow()
                .iter()
                .filter(|l| !is_terminal_state(&l.state))
                .filter(|l| Some(l.loop_id) != selected_id)
                .map(|l| l.loop_id)
                .collect();

            // Remove tasks for loops that are no longer active or became selected
            active_tasks.retain(|id, task| {
                if current_loops.contains(id) {
                    true
                } else {
                    task.abort();
                    false
                }
            });

            // Start tasks for new active loops
            for loop_id in &current_loops {
                if active_tasks.contains_key(loop_id) {
                    continue;
                }
                let client = client.clone();
                let event_tx = event_tx.clone();
                let lid = *loop_id;
                let task = tokio::spawn(async move {
                    stream_persisted_logs(client, lid, event_tx).await;
                });
                active_tasks.insert(*loop_id, task);
            }

            // Wait for loop list to change
            if loops_rx.changed().await.is_err() {
                // Channel closed, clean up
                for (_, task) in active_tasks.drain() {
                    task.abort();
                }
                break;
            }
        }
    });
}

async fn stream_logs_for_selection(
    client: NemoClient,
    selection: LogSelection,
    event_tx: mpsc::UnboundedSender<AppEvent>,
) {
    match selection.source {
        LogSource::Persisted => {
            stream_persisted_logs(client, selection.loop_id, event_tx).await;
        }
        LogSource::AgentPod => {
            stream_pod_logs(client, selection.loop_id, "agent", event_tx).await;
        }
        LogSource::SidecarPod => {
            stream_pod_logs(client, selection.loop_id, "auth-sidecar", event_tx).await;
        }
    }
}

async fn stream_persisted_logs(
    client: NemoClient,
    loop_id: uuid::Uuid,
    event_tx: mpsc::UnboundedSender<AppEvent>,
) {
    let mut emitted_lines = Vec::new();

    loop {
        let path = format!("/logs/{loop_id}");
        let response = match client.get_stream(&path).await {
            Ok(response) => response,
            Err(error) => {
                if event_tx
                    .send(AppEvent::LogStatus(
                        loop_id,
                        format!("log stream failed: {error}"),
                    ))
                    .is_err()
                {
                    return;
                }
                tokio::time::sleep(Duration::from_secs(2)).await;
                continue;
            }
        };

        let content_type = response
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok())
            .unwrap_or("")
            .to_string();

        let outcome = if content_type.contains("text/event-stream") {
            if event_tx
                .send(AppEvent::LogStatus(
                    loop_id,
                    "Streaming persisted loop logs".to_string(),
                ))
                .is_err()
            {
                return;
            }
            stream_sse_logs(response, loop_id, &event_tx, &mut emitted_lines).await
        } else {
            if event_tx
                .send(AppEvent::LogStatus(
                    loop_id,
                    "Showing persisted historical logs".to_string(),
                ))
                .is_err()
            {
                return;
            }
            stream_historical_logs(response, loop_id, &event_tx, &mut emitted_lines).await
        };

        match outcome {
            Ok(StreamOutcome::HistoricalComplete) => return,
            Ok(StreamOutcome::Ended(state)) => {
                let _ = event_tx.send(AppEvent::LogStatus(loop_id, format!("Loop ended: {state}")));
                return;
            }
            Ok(StreamOutcome::Disconnected) => {
                if event_tx
                    .send(AppEvent::LogStatus(
                        loop_id,
                        "Log stream disconnected, reconnecting...".to_string(),
                    ))
                    .is_err()
                {
                    return;
                }
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
            Err(error) => {
                if event_tx
                    .send(AppEvent::LogStatus(
                        loop_id,
                        format!("log decode failed: {error}"),
                    ))
                    .is_err()
                {
                    return;
                }
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        }
    }
}

async fn stream_pod_logs(
    client: NemoClient,
    loop_id: uuid::Uuid,
    container: &'static str,
    event_tx: mpsc::UnboundedSender<AppEvent>,
) {
    let mut previous_lines: Vec<String> = Vec::new();

    loop {
        match fetch_pod_log_snapshot(&client, loop_id, container).await {
            Ok(PodLogSnapshot::Lines(lines)) => {
                if event_tx
                    .send(AppEvent::LogStatus(
                        loop_id,
                        format!("Polling {container} pod logs"),
                    ))
                    .is_err()
                {
                    return;
                }

                let overlap = overlapping_suffix_len(&previous_lines, &lines);
                for line in lines.iter().skip(overlap) {
                    if event_tx
                        .send(AppEvent::LogLine(loop_id, line.clone()))
                        .is_err()
                    {
                        return;
                    }
                }
                previous_lines = lines;
            }
            Ok(PodLogSnapshot::Info(message)) => {
                if event_tx
                    .send(AppEvent::LogStatus(loop_id, message))
                    .is_err()
                {
                    return;
                }
            }
            Err(error) => {
                if event_tx
                    .send(AppEvent::LogStatus(
                        loop_id,
                        format!("{container} pod log polling failed: {error}"),
                    ))
                    .is_err()
                {
                    return;
                }
            }
        }

        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

enum PodLogSnapshot {
    Lines(Vec<String>),
    Info(String),
}

async fn fetch_pod_log_snapshot(
    client: &NemoClient,
    loop_id: uuid::Uuid,
    container: &str,
) -> Result<PodLogSnapshot> {
    let path = format!("/pod-logs/{loop_id}?tail={POD_TAIL_LINES}&container={container}");
    let response = client.get_stream(&path).await?;
    let status = response.status();
    let body = response.text().await?;

    if status == reqwest::StatusCode::NO_CONTENT {
        let message = body.trim();
        return Ok(PodLogSnapshot::Info(if message.is_empty() {
            format!("No {container} pod logs available yet")
        } else {
            message.to_string()
        }));
    }

    let lines = body.lines().map(ToOwned::to_owned).collect();
    Ok(PodLogSnapshot::Lines(lines))
}

fn overlapping_suffix_len(previous: &[String], current: &[String]) -> usize {
    let max_overlap = previous.len().min(current.len());
    (0..=max_overlap)
        .rev()
        .find(|count| previous[previous.len().saturating_sub(*count)..] == current[..*count])
        .unwrap_or(0)
}

async fn stream_historical_logs(
    response: reqwest::Response,
    loop_id: uuid::Uuid,
    event_tx: &mpsc::UnboundedSender<AppEvent>,
    emitted_lines: &mut Vec<String>,
) -> Result<StreamOutcome> {
    let body = response.text().await?;
    let logs: Vec<serde_json::Value> = serde_json::from_str(&body)?;

    let mut replay_index = 0;
    for log in logs {
        let Some(formatted_line) = format_log_json(&log) else {
            continue;
        };
        emit_or_skip_replayed_line(
            loop_id,
            formatted_line,
            emitted_lines,
            &mut replay_index,
            event_tx,
        )?;
    }

    Ok(StreamOutcome::HistoricalComplete)
}

async fn stream_sse_logs(
    response: reqwest::Response,
    loop_id: uuid::Uuid,
    event_tx: &mpsc::UnboundedSender<AppEvent>,
    emitted_lines: &mut Vec<String>,
) -> Result<StreamOutcome> {
    let mut stream = response.bytes_stream();
    let mut buffer = String::new();
    let mut replay_index = 0;

    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        buffer.push_str(&String::from_utf8_lossy(&chunk));

        while let Some(position) = buffer.find("\n\n") {
            let event = buffer[..position].to_string();
            buffer = buffer[position + 2..].to_string();

            for line in event.lines() {
                let Some(data) = line.strip_prefix("data: ") else {
                    continue;
                };
                let parsed: serde_json::Value = match serde_json::from_str(data) {
                    Ok(parsed) => parsed,
                    Err(_) => continue,
                };

                if parsed.get("type").and_then(|value| value.as_str()) == Some("end") {
                    let state = parsed
                        .get("state")
                        .and_then(|value| value.as_str())
                        .unwrap_or("UNKNOWN")
                        .to_string();
                    return Ok(StreamOutcome::Ended(state));
                }

                let Some(formatted_line) = format_log_json(&parsed) else {
                    continue;
                };
                emit_or_skip_replayed_line(
                    loop_id,
                    formatted_line,
                    emitted_lines,
                    &mut replay_index,
                    event_tx,
                )?;
            }
        }
    }

    Ok(StreamOutcome::Disconnected)
}

fn emit_or_skip_replayed_line(
    loop_id: uuid::Uuid,
    formatted_line: String,
    emitted_lines: &mut Vec<String>,
    replay_index: &mut usize,
    event_tx: &mpsc::UnboundedSender<AppEvent>,
) -> Result<()> {
    if *replay_index < emitted_lines.len() && emitted_lines[*replay_index] == formatted_line {
        *replay_index += 1;
        return Ok(());
    }

    emitted_lines.push(formatted_line.clone());
    event_tx
        .send(AppEvent::LogLine(loop_id, formatted_line))
        .map_err(|_| anyhow::anyhow!("helm event channel closed"))
}

fn format_log_json(value: &serde_json::Value) -> Option<String> {
    let stage = value.get("stage")?.as_str()?;
    let round = value.get("round")?.as_i64()?;
    let line = value.get("line")?.as_str()?;
    Some(format!("[{stage}/r{round}] {line}"))
}

// ──────────────────────────────── Rendering ────────────────────────────────

fn render(frame: &mut ratatui::Frame<'_>, app: &mut App) {
    let theme = app.theme();

    let root = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // FR-1: header summary
            Constraint::Min(0),    // main content
            Constraint::Length(1), // footer
        ])
        .split(frame.area());

    // FR-1: Header summary line
    // FR-5b: profile name portion uses dim/secondary text color
    let header_text = summary::build_header(
        &app.loops,
        &app.all_inspect,
        &app.pricing,
        app.team_view,
        &app.profile_name,
    );
    let bold_teal = Style::default().fg(theme.teal).add_modifier(Modifier::BOLD);
    let dim_style = Style::default().fg(theme.muted);
    // Split: "nautiloop · <profile>" from the rest; profile name gets dim style
    let header_line = if let Some(rest) = header_text.strip_prefix("nautiloop") {
        // rest starts with " · <profile> · ..." or " · <profile> · team ..."
        // Find the profile name portion: " · <profile_name>"
        let profile_marker = format!(" · {}", app.profile_name);
        if let Some(after_profile) = rest.strip_prefix(&profile_marker) {
            Line::from(vec![
                Span::styled("nautiloop", bold_teal),
                Span::styled(format!(" · {}", app.profile_name), dim_style),
                Span::styled(after_profile.to_string(), bold_teal),
            ])
        } else {
            Line::from(Span::styled(header_text, bold_teal))
        }
    } else {
        Line::from(Span::styled(header_text, bold_teal))
    };
    let header = Paragraph::new(header_line).style(Style::default().bg(theme.surface));
    frame.render_widget(header, root[0]);

    // Main content area
    match app.main_view {
        MainView::MultiLoop => {
            multi_view::render(frame, &app.loops, &app.multi_logs, root[1], &theme);
        }
        _ => {
            let content = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Percentage(34), Constraint::Percentage(66)])
                .split(root[1]);

            let right = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Length(17), Constraint::Min(0)])
                .split(content[1]);

            frame.render_widget(render_details(app, &theme), right[0]);

            // Main view in right lower area
            match app.main_view {
                MainView::Logs => render_logs_with_side_panel(frame, app, right[1], &theme),
                MainView::Diff => {
                    let diff_widget = diff_pane::render(
                        app.diff_content.as_deref(),
                        &app.diff_status,
                        app.diff_scroll,
                        right[1],
                        &theme,
                    );
                    frame.render_widget(diff_widget, right[1]);
                }
                MainView::RoundsTable => {
                    let is_harden = app.is_harden_loop();
                    let current_round = app.selected_loop().map(|l| l.round).unwrap_or(0);
                    let current_stage =
                        app.selected_loop().and_then(|l| l.current_stage.as_deref());
                    let model_impl = app
                        .selected_loop()
                        .and_then(|l| l.model_implementor.as_deref());
                    let model_rev = app
                        .selected_loop()
                        .and_then(|l| l.model_reviewer.as_deref());
                    let table_widget =
                        rounds_table::render_table(&rounds_table::RoundsTableConfig {
                            inspect: app.inspect.as_ref(),
                            inspect_status: &app.inspect_status,
                            selected_row: app.rounds_table_selected,
                            scroll: app.rounds_table_scroll,
                            is_harden,
                            current_round,
                            current_stage,
                            pricing: &app.pricing,
                            model_implementor: model_impl,
                            model_reviewer: model_rev,
                            area: right[1],
                            theme: &theme,
                        });
                    frame.render_widget(table_widget, right[1]);
                }
                MainView::RoundDetail => {
                    if let Some(inspect) = &app.inspect
                        && let Some(round) = inspect.rounds.get(app.rounds_table_selected)
                    {
                        let detail_widget =
                            rounds_table::render_detail(&rounds_table::RoundDetailConfig {
                                round,
                                is_harden: app.is_harden_loop(),
                                pricing: &app.pricing,
                                model_implementor: app
                                    .selected_loop()
                                    .and_then(|l| l.model_implementor.as_deref()),
                                model_reviewer: app
                                    .selected_loop()
                                    .and_then(|l| l.model_reviewer.as_deref()),
                                scroll: app.round_detail_scroll,
                                area: right[1],
                                theme: &theme,
                            });
                        frame.render_widget(detail_widget, right[1]);
                    }
                }
                MainView::MultiLoop => unreachable!(),
            }

            frame.render_stateful_widget(
                render_loop_selector(app, &theme),
                content[0],
                &mut app.list_state,
            );
        }
    }

    frame.render_widget(render_footer(app, &theme), root[2]);
}

fn render_logs_with_side_panel(
    frame: &mut ratatui::Frame<'_>,
    app: &App,
    area: Rect,
    theme: &Theme,
) {
    match app.side_panel {
        SidePanel::Closed => {
            frame.render_widget(render_logs(app, area, theme), area);
        }
        SidePanel::Inspect => {
            let log_inspect = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Percentage(55), Constraint::Percentage(45)])
                .split(area);
            frame.render_widget(render_logs(app, log_inspect[0], theme), log_inspect[0]);
            frame.render_widget(render_inspect_pane(app, theme), log_inspect[1]);
        }
        SidePanel::Introspect => {
            let log_introspect = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Percentage(55), Constraint::Percentage(45)])
                .split(area);
            frame.render_widget(
                render_logs(app, log_introspect[0], theme),
                log_introspect[0],
            );
            frame.render_widget(render_introspect_pane(app, theme), log_introspect[1]);
        }
    }
}

fn render_loop_selector(app: &App, theme: &Theme) -> List<'static> {
    let items = if app.loops.is_empty() {
        vec![ListItem::new(Line::from(Span::styled(
            "No active loops",
            Style::default().fg(theme.muted),
        )))]
    } else {
        app.loops
            .iter()
            .map(|loop_item| {
                let stage = loop_item.current_stage.as_deref().unwrap_or("-");

                // FR-2: Add token and cost columns (per-round cost for accuracy)
                let (tokens_str, cost_str) =
                    if let Some(inspect_data) = app.all_inspect.get(&loop_item.loop_id) {
                        let mut total_tokens = 0u64;
                        let mut total_cost = 0.0f64;
                        let mut any_priced = false;
                        for round in &inspect_data.rounds {
                            let (inp, out) = round_total_tokens(round);
                            total_tokens += inp + out;
                            if let Some(c) = calculate_loop_round_cost(
                                &app.pricing,
                                loop_item.model_implementor.as_deref(),
                                loop_item.model_reviewer.as_deref(),
                                round,
                            ) {
                                total_cost += c;
                                any_priced = true;
                            }
                        }
                        let tokens = format_tokens(total_tokens);
                        let cost = format_cost(if any_priced { Some(total_cost) } else { None });
                        (tokens, cost)
                    } else {
                        ("-".to_string(), "-".to_string())
                    };

                let line = format!(
                    "{: <10} {: <18} {: <8} r{: <3} {: <7} {: <7} {}",
                    loop_item.engineer,
                    state_label(loop_item),
                    stage,
                    loop_item.round,
                    tokens_str,
                    cost_str,
                    loop_item.spec_path
                );
                // FR-4a: row flash for convergence events (1-second highlight)
                let fg = if app.has_row_flash(&loop_item.loop_id) {
                    theme.green
                } else {
                    theme.text
                };
                ListItem::new(Line::from(Span::styled(line, Style::default().fg(fg))))
            })
            .collect()
    };

    // Show status flash or normal status in title
    let title_text = if let Some(flash) = app.active_status_flash() {
        format!(" helm {} ", flash)
    } else {
        format!(" helm {} loops ", app.loops.len())
    };

    List::new(items)
        .block(
            Block::default()
                .title(Span::styled(
                    title_text,
                    Style::default().fg(theme.text).add_modifier(Modifier::BOLD),
                ))
                .borders(Borders::ALL)
                .border_style(Style::default().fg(theme.border).bg(theme.surface))
                .style(Style::default().bg(theme.surface)),
        )
        .highlight_style(
            Style::default()
                .fg(theme.text)
                .bg(theme.border)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("> ")
}

fn render_details(app: &App, theme: &Theme) -> Paragraph<'static> {
    let body = if let Some(loop_item) = app.selected_loop() {
        let mut lines = vec![
            detail_line("engineer", &loop_item.engineer, theme),
            Line::from(vec![
                Span::styled(
                    format!("{:>8} ", "state"),
                    Style::default()
                        .fg(theme.muted)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    state_label(loop_item),
                    Style::default()
                        .fg(state_color(&loop_item.state, theme))
                        .add_modifier(Modifier::BOLD),
                ),
            ]),
            detail_line(
                "stage",
                loop_item.current_stage.as_deref().unwrap_or("-"),
                theme,
            ),
            detail_line(
                "round",
                &format!("{}/{}", loop_item.round, loop_item.max_rounds),
                theme,
            ),
            detail_line("kind", &loop_item.kind, theme),
            detail_line(
                "job",
                loop_item.active_job_name.as_deref().unwrap_or("-"),
                theme,
            ),
            detail_line("branch", &loop_item.branch, theme),
            detail_line("loop", &loop_item.loop_id.to_string(), theme),
            detail_line("spec", &loop_item.spec_path, theme),
            Line::from(Span::styled("", Style::default())),
            Line::from(vec![
                Span::styled(
                    format!("{:>8} ", "inspect"),
                    Style::default()
                        .fg(theme.muted)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(app.inspect_status.clone(), Style::default().fg(theme.muted)),
            ]),
        ];

        if let Some(inspect) = &app.inspect
            && let Some(round) = latest_round(inspect)
        {
            lines.push(detail_line(
                "latest",
                &format!("round {}", round.round),
                theme,
            ));
            for (label, round_summary) in round_stage_summaries(round) {
                lines.push(detail_line(label, &round_summary, theme));
            }
        }

        Text::from(lines)
    } else {
        Text::from(vec![Line::from(Span::styled(
            "Waiting for an active loop selection",
            Style::default().fg(theme.muted),
        ))])
    };

    Paragraph::new(body)
        .block(
            Block::default()
                .title(Span::styled(
                    " overview + inspect ",
                    Style::default().fg(theme.text).add_modifier(Modifier::BOLD),
                ))
                .borders(Borders::ALL)
                .border_style(Style::default().fg(theme.border).bg(theme.surface))
                .style(Style::default().bg(theme.surface)),
        )
        .style(Style::default().fg(theme.text).bg(theme.surface))
        .wrap(Wrap { trim: false })
}

fn render_logs(app: &App, area: Rect, theme: &Theme) -> Paragraph<'static> {
    let inner_height = area.height.saturating_sub(2) as usize;

    let lines: Vec<Line<'static>> = if app.logs.is_empty() {
        vec![Line::from(Span::styled(
            app.log_status.clone(),
            Style::default().fg(theme.muted),
        ))]
    } else {
        // Only convert the visible tail of logs to avoid per-frame cloning of all Arc<String>s
        let total = app.logs.len();
        let skip = total.saturating_sub(inner_height);
        app.logs
            .iter()
            .skip(skip)
            .map(|line| {
                Line::from(Span::styled(
                    line.as_str().to_owned(),
                    Style::default().fg(theme.text),
                ))
            })
            .collect()
    };

    Paragraph::new(Text::from(lines))
        .block(
            Block::default()
                .title(Span::styled(
                    format!(" logs {} ", app.log_source.label()),
                    Style::default().fg(theme.text).add_modifier(Modifier::BOLD),
                ))
                .borders(Borders::ALL)
                .border_style(Style::default().fg(theme.border).bg(theme.surface))
                .style(Style::default().bg(theme.bg)),
        )
        .style(Style::default().fg(theme.text).bg(theme.bg))
        .wrap(Wrap { trim: false })
}

fn render_inspect_pane(app: &App, theme: &Theme) -> Paragraph<'static> {
    let body = if let Some(inspect) = &app.inspect {
        let mut lines = vec![
            Line::from(vec![
                Span::styled("Branch ", Style::default().fg(theme.muted)),
                Span::styled(inspect.branch.clone(), Style::default().fg(theme.text)),
                Span::styled(
                    format!(
                        "  {} round{}",
                        inspect.rounds.len(),
                        if inspect.rounds.len() == 1 { "" } else { "s" }
                    ),
                    Style::default().fg(theme.muted),
                ),
            ]),
            Line::from(Span::styled("", Style::default())),
        ];

        for round in inspect.rounds.iter().rev() {
            lines.push(Line::from(Span::styled(
                format!("Round {}", round.round),
                Style::default().fg(theme.teal).add_modifier(Modifier::BOLD),
            )));
            let summaries = round_stage_summaries(round);
            for (label, round_summary) in summaries {
                lines.push(Line::from(vec![
                    Span::styled(
                        format!("  {label:>7} "),
                        Style::default()
                            .fg(theme.muted)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(round_summary, Style::default().fg(theme.text)),
                ]));
            }
            lines.push(Line::from(Span::styled("", Style::default())));
        }

        Text::from(lines)
    } else {
        Text::from(vec![Line::from(Span::styled(
            app.inspect_status.clone(),
            Style::default().fg(theme.muted),
        ))])
    };

    Paragraph::new(body)
        .block(
            Block::default()
                .title(Span::styled(
                    " inspect ",
                    Style::default().fg(theme.text).add_modifier(Modifier::BOLD),
                ))
                .borders(Borders::ALL)
                .border_style(Style::default().fg(theme.border).bg(theme.surface))
                .style(Style::default().bg(theme.surface)),
        )
        .style(Style::default().fg(theme.text).bg(theme.surface))
        .wrap(Wrap { trim: false })
}

fn render_introspect_pane(app: &App, theme: &Theme) -> Paragraph<'static> {
    let body = if let Some(snapshot) = &app.introspect {
        let mut lines = vec![Line::from(vec![
            Span::styled("Pod ", Style::default().fg(theme.muted)),
            Span::styled(snapshot.pod_name.clone(), Style::default().fg(theme.text)),
            Span::styled("  Phase ", Style::default().fg(theme.muted)),
            Span::styled(
                snapshot.pod_phase.clone(),
                Style::default()
                    .fg(if snapshot.pod_phase == "Running" {
                        theme.green
                    } else {
                        theme.amber
                    })
                    .add_modifier(Modifier::BOLD),
            ),
        ])];

        match &snapshot.container_stats {
            Some(stats) => {
                let mem_mib = stats.memory_bytes / (1024 * 1024);
                lines.push(Line::from(vec![
                    Span::styled("CPU ", Style::default().fg(theme.muted)),
                    Span::styled(
                        format!("{}m", stats.cpu_millicores),
                        Style::default().fg(theme.text),
                    ),
                    Span::styled("  Mem ", Style::default().fg(theme.muted)),
                    Span::styled(format!("{mem_mib} MiB"), Style::default().fg(theme.text)),
                ]));
            }
            None => {
                lines.push(Line::from(Span::styled(
                    "Stats: unavailable",
                    Style::default().fg(theme.muted),
                )));
            }
        }

        let wt = &snapshot.worktree;
        let head = wt
            .head_sha
            .as_deref()
            .map(|s| &s[..s.len().min(7)])
            .unwrap_or("-");
        let target_info = match (wt.target_dir_bytes, wt.target_dir_artifacts) {
            (Some(bytes), Some(arts)) => {
                let gib = bytes as f64 / (1024.0 * 1024.0 * 1024.0);
                format!("{gib:.1} GiB ({arts} arts)")
            }
            _ => "-".to_string(),
        };
        lines.push(Line::from(vec![
            Span::styled("HEAD ", Style::default().fg(theme.muted)),
            Span::styled(head.to_string(), Style::default().fg(theme.text)),
            Span::styled(
                match wt.uncommitted_files {
                    Some(n) => format!("  dirty={n}"),
                    None => "  dirty=?".to_string(),
                },
                Style::default().fg(if wt.uncommitted_files.unwrap_or(0) > 0 {
                    theme.amber
                } else {
                    theme.text
                }),
            ),
            Span::styled(
                format!("  target={target_info}"),
                Style::default().fg(theme.muted),
            ),
        ]));

        lines.push(Line::from(Span::styled("", Style::default())));

        lines.push(Line::from(Span::styled(
            format!(
                "{:<5}{:<5}{:<6}{:<6}{}",
                "PID", "PPID", "CPU%", "AGE", "COMMAND"
            ),
            Style::default()
                .fg(theme.muted)
                .add_modifier(Modifier::BOLD),
        )));
        for p in snapshot.processes.iter().take(10) {
            let age = if p.age_seconds >= 3600 {
                format!("{}h{}m", p.age_seconds / 3600, (p.age_seconds % 3600) / 60)
            } else if p.age_seconds >= 60 {
                format!("{}m", p.age_seconds / 60)
            } else {
                format!("{}s", p.age_seconds)
            };
            let cpu_color = if p.cpu_percent > 10.0 {
                theme.amber
            } else {
                theme.text
            };
            lines.push(Line::from(vec![
                Span::styled(format!("{:<5}", p.pid), Style::default().fg(theme.text)),
                Span::styled(format!("{:<5}", p.ppid), Style::default().fg(theme.muted)),
                Span::styled(
                    format!("{:<6.1}", p.cpu_percent),
                    Style::default().fg(cpu_color),
                ),
                Span::styled(format!("{:<6}", age), Style::default().fg(theme.text)),
                Span::styled(
                    p.cmd.chars().take(40).collect::<String>(),
                    Style::default().fg(theme.text),
                ),
            ]));
        }

        if snapshot.processes.is_empty() {
            lines.push(Line::from(Span::styled(
                "(no processes)",
                Style::default().fg(theme.muted),
            )));
        }

        if !snapshot.warnings.is_empty() {
            lines.push(Line::from(Span::styled("", Style::default())));
            for w in &snapshot.warnings {
                lines.push(Line::from(Span::styled(
                    format!("! {w}"),
                    Style::default().fg(theme.amber),
                )));
            }
        }

        Text::from(lines)
    } else {
        Text::from(vec![Line::from(Span::styled(
            app.introspect_status.clone(),
            Style::default().fg(theme.muted),
        ))])
    };

    Paragraph::new(body)
        .block(
            Block::default()
                .title(Span::styled(
                    " introspect ",
                    Style::default().fg(theme.text).add_modifier(Modifier::BOLD),
                ))
                .borders(Borders::ALL)
                .border_style(Style::default().fg(theme.border).bg(theme.surface))
                .style(Style::default().bg(theme.surface)),
        )
        .style(Style::default().fg(theme.text).bg(theme.surface))
        .wrap(Wrap { trim: false })
}

fn latest_round(inspect: &InspectResponse) -> Option<&RoundSummary> {
    inspect.rounds.iter().max_by_key(|round| round.round)
}

fn round_stage_summaries(round: &RoundSummary) -> Vec<(&'static str, String)> {
    let mut summaries = Vec::new();

    if let Some(s) = summarize_impl_stage(round.implement.as_ref()) {
        summaries.push(("impl", s));
    }
    if let Some(s) = summarize_test_stage(round.test.as_ref()) {
        summaries.push(("test", s));
    }
    if let Some(s) = summarize_verdict_stage(round.review.as_ref(), "review") {
        summaries.push(("review", s));
    }
    if let Some(s) = summarize_verdict_stage(round.audit.as_ref(), "audit") {
        summaries.push(("audit", s));
    }
    if let Some(s) = summarize_revise_stage(round.revise.as_ref()) {
        summaries.push(("revise", s));
    }

    if summaries.is_empty() {
        summaries.push(("round", "No persisted stage outputs yet".to_string()));
    }

    summaries
}

fn summarize_impl_stage(value: Option<&serde_json::Value>) -> Option<String> {
    let value = value?;
    value
        .get("new_sha")
        .and_then(|sha| sha.as_str())
        .map(|sha| format!("new sha {}", short_sha(sha)))
}

fn summarize_revise_stage(value: Option<&serde_json::Value>) -> Option<String> {
    let value = value?;
    if let Some(path) = value
        .get("revised_spec_path")
        .and_then(|path| path.as_str())
    {
        return Some(format!("revised {path}"));
    }
    value
        .get("new_sha")
        .and_then(|sha| sha.as_str())
        .map(|sha| format!("new sha {}", short_sha(sha)))
}

fn summarize_test_stage(value: Option<&serde_json::Value>) -> Option<String> {
    let value = value?;
    let all_passed = value.get("all_passed").and_then(|flag| flag.as_bool())?;
    let ci_status = value
        .get("ci_status")
        .and_then(|status| status.as_str())
        .unwrap_or("unknown");
    let failing_services = value
        .get("services")
        .and_then(|services| services.as_array())
        .map(|services| {
            services
                .iter()
                .filter(|service| {
                    service.get("passed").and_then(|passed| passed.as_bool()) == Some(false)
                })
                .count()
        })
        .unwrap_or(0);
    Some(if all_passed {
        format!("pass ({ci_status})")
    } else {
        format!(
            "fail ({ci_status}, {failing_services} service{})",
            if failing_services == 1 { "" } else { "s" }
        )
    })
}

fn summarize_verdict_stage(value: Option<&serde_json::Value>, kind: &str) -> Option<String> {
    let value = value?;
    let verdict = value.get("verdict").unwrap_or(value);
    let clean = verdict.get("clean").and_then(|flag| flag.as_bool())?;
    let issue_count = verdict
        .get("issues")
        .and_then(|issues| issues.as_array())
        .map(|issues| issues.len())
        .unwrap_or(0);
    let verdict_summary = verdict
        .get("summary")
        .and_then(|s| s.as_str())
        .unwrap_or("")
        .trim();

    Some(match (clean, verdict_summary.is_empty()) {
        (true, true) => format!("clean {kind}"),
        (true, false) => format!("clean, {verdict_summary}"),
        (false, true) => format!(
            "{issue_count} issue{}",
            if issue_count == 1 { "" } else { "s" }
        ),
        (false, false) => format!(
            "{issue_count} issue{}, {verdict_summary}",
            if issue_count == 1 { "" } else { "s" }
        ),
    })
}

fn short_sha(sha: &str) -> &str {
    let len = sha.len().min(8);
    &sha[..len]
}

fn render_footer(app: &App, theme: &Theme) -> Paragraph<'static> {
    let mode = if app.team_view { "team" } else { "engineer" };
    let view_label = match app.main_view {
        MainView::Logs => "logs",
        MainView::Diff => "diff",
        MainView::MultiLoop => "multi",
        MainView::RoundsTable => "rounds",
        MainView::RoundDetail => "detail",
    };

    let mut spans = vec![
        Span::styled("mode ", Style::default().fg(theme.muted)),
        Span::styled(
            mode,
            Style::default().fg(theme.blue).add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled("view ", Style::default().fg(theme.muted)),
        Span::styled(
            view_label,
            Style::default().fg(theme.blue).add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled(app.theme_name.label(), Style::default().fg(theme.muted)),
        Span::raw("   "),
    ];

    // Show flash message if active
    if let Some(flash) = app.active_status_flash() {
        spans.push(Span::styled(
            flash.to_string(),
            Style::default()
                .fg(theme.amber)
                .add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::raw("   "));
    }

    // Context-specific keybind hints (FR-10a)
    if let Some(loop_item) = app.selected_loop() {
        let hints = approval_hints(loop_item);
        for (key, label) in hints {
            spans.push(Span::styled(
                key,
                Style::default().fg(theme.teal).add_modifier(Modifier::BOLD),
            ));
            spans.push(Span::styled(
                format!(" {label}  "),
                Style::default().fg(theme.muted),
            ));
        }
    }

    // Standard keybinds
    spans.extend([
        Span::styled(
            "q",
            Style::default().fg(theme.teal).add_modifier(Modifier::BOLD),
        ),
        Span::styled(" quit  ", Style::default().fg(theme.muted)),
        Span::styled(
            "j/k",
            Style::default().fg(theme.teal).add_modifier(Modifier::BOLD),
        ),
        Span::styled(" move  ", Style::default().fg(theme.muted)),
        Span::styled(
            "l",
            Style::default().fg(theme.blue).add_modifier(Modifier::BOLD),
        ),
        Span::styled(" src  ", Style::default().fg(theme.muted)),
        Span::styled(
            "d",
            Style::default().fg(theme.blue).add_modifier(Modifier::BOLD),
        ),
        Span::styled(" diff  ", Style::default().fg(theme.muted)),
        Span::styled(
            "m",
            Style::default().fg(theme.blue).add_modifier(Modifier::BOLD),
        ),
        Span::styled(" multi  ", Style::default().fg(theme.muted)),
        Span::styled(
            "R",
            Style::default().fg(theme.blue).add_modifier(Modifier::BOLD),
        ),
        Span::styled(" rounds  ", Style::default().fg(theme.muted)),
        Span::styled(
            "T",
            Style::default().fg(theme.blue).add_modifier(Modifier::BOLD),
        ),
        Span::styled(" theme", Style::default().fg(theme.muted)),
    ]);

    Paragraph::new(Line::from(spans)).style(Style::default().fg(theme.text).bg(theme.bg))
}

fn detail_line(label: &str, value: &str, theme: &Theme) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            format!("{label:>8} "),
            Style::default()
                .fg(theme.muted)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(value.to_string(), Style::default().fg(theme.text)),
    ])
}

fn state_label(loop_item: &LoopSummary) -> String {
    match &loop_item.sub_state {
        Some(sub_state) => format!("{}/{}", loop_item.state, sub_state),
        None => loop_item.state.clone(),
    }
}

fn state_color(state: &str, theme: &Theme) -> ratatui::style::Color {
    if matches!(state, "CONVERGED" | "HARDENED" | "SHIPPED") {
        theme.green
    } else if matches!(state, "FAILED" | "CANCELLED") {
        theme.red
    } else if matches!(state, "PAUSED" | "AWAITING_REAUTH" | "AWAITING_APPROVAL") {
        theme.amber
    } else {
        theme.teal
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn default_helm_config() -> HelmConfig {
        HelmConfig::default()
    }

    fn loop_summary(id: uuid::Uuid, engineer: &str, updated_at: &str) -> LoopSummary {
        LoopSummary {
            loop_id: id,
            engineer: engineer.to_string(),
            spec_path: "specs/test.md".to_string(),
            branch: format!("agent/{engineer}/test"),
            state: "IMPLEMENTING".to_string(),
            sub_state: Some("RUNNING".to_string()),
            round: 2,
            current_stage: Some("implement".to_string()),
            active_job_name: Some("job-1".to_string()),
            spec_pr_url: None,
            failed_from_state: None,
            kind: "implement".to_string(),
            max_rounds: 15,
            model_implementor: None,
            model_reviewer: None,
            created_at: updated_at.to_string(),
            updated_at: updated_at.to_string(),
            last_activity_at: None,
            tokens_input: 0,
            tokens_output: 0,
        }
    }

    #[test]
    fn replay_dedupe_skips_replayed_prefix() {
        let loop_id = uuid::Uuid::new_v4();
        let (event_tx, mut event_rx) = mpsc::unbounded_channel();
        let mut emitted_lines = vec!["[implement/r1] first".to_string()];
        let mut replay_index = 0;

        emit_or_skip_replayed_line(
            loop_id,
            "[implement/r1] first".to_string(),
            &mut emitted_lines,
            &mut replay_index,
            &event_tx,
        )
        .unwrap();
        emit_or_skip_replayed_line(
            loop_id,
            "[implement/r1] second".to_string(),
            &mut emitted_lines,
            &mut replay_index,
            &event_tx,
        )
        .unwrap();

        let received = event_rx.try_recv().unwrap();
        match received {
            AppEvent::LogLine(received_loop_id, line) => {
                assert_eq!(received_loop_id, loop_id);
                assert_eq!(line, "[implement/r1] second");
            }
            _ => panic!("expected log line event"),
        }
        assert!(event_rx.try_recv().is_err());
    }

    #[test]
    fn set_loops_preserves_selected_loop_when_still_present() {
        let first_id = uuid::Uuid::new_v4();
        let second_id = uuid::Uuid::new_v4();
        let mut app = App::new(false, &default_helm_config(), "default".to_string());
        app.set_loops(vec![
            loop_summary(first_id, "alice", "2026-04-10T10:00:00Z"),
            loop_summary(second_id, "bob", "2026-04-10T09:00:00Z"),
        ]);
        app.selected_loop_id = Some(second_id);

        app.set_loops(vec![
            loop_summary(second_id, "bob", "2026-04-10T11:00:00Z"),
            loop_summary(first_id, "alice", "2026-04-10T10:00:00Z"),
        ]);

        assert_eq!(app.selected_loop_id, Some(second_id));
        assert_eq!(app.list_state.selected(), Some(0));
    }

    #[test]
    fn action_hotkeys_map_to_loop_commands() {
        let mut app = App::new(false, &default_helm_config(), "default".to_string());

        assert_eq!(
            app.handle_input(KeyEvent::from(KeyCode::Char('a'))),
            AppAction::Trigger(LoopCommand::Approve)
        );
        assert_eq!(
            app.handle_input(KeyEvent::from(KeyCode::Char('r'))),
            AppAction::Trigger(LoopCommand::Resume)
        );
        assert_eq!(
            app.handle_input(KeyEvent::from(KeyCode::Char('e'))),
            AppAction::Trigger(LoopCommand::Extend)
        );
    }

    #[test]
    fn log_source_hotkey_cycles_sources() {
        let mut app = App::new(false, &default_helm_config(), "default".to_string());

        assert_eq!(app.log_source, LogSource::Persisted);
        assert_eq!(
            app.handle_input(KeyEvent::from(KeyCode::Char('l'))),
            AppAction::SourceChanged
        );
        assert_eq!(app.log_source, LogSource::AgentPod);
        assert_eq!(
            app.handle_input(KeyEvent::from(KeyCode::Char('l'))),
            AppAction::SourceChanged
        );
        assert_eq!(app.log_source, LogSource::SidecarPod);
        assert_eq!(
            app.handle_input(KeyEvent::from(KeyCode::Char('l'))),
            AppAction::SourceChanged
        );
        assert_eq!(app.log_source, LogSource::Persisted);
    }

    #[test]
    fn persisted_log_selection_does_not_restart_on_job_name_changes() {
        let first_id = uuid::Uuid::new_v4();
        let mut app = App::new(false, &default_helm_config(), "default".to_string());
        app.set_loops(vec![loop_summary(
            first_id,
            "alice",
            "2026-04-10T10:00:00Z",
        )]);

        let first_selection = app.current_log_selection();
        app.set_loops(vec![LoopSummary {
            active_job_name: Some("job-2".to_string()),
            ..loop_summary(first_id, "alice", "2026-04-10T11:00:00Z")
        }]);

        assert_eq!(first_selection, app.current_log_selection());
    }

    #[test]
    fn pod_log_selection_tracks_active_job_name() {
        let first_id = uuid::Uuid::new_v4();
        let mut app = App::new(false, &default_helm_config(), "default".to_string());
        app.log_source = LogSource::AgentPod;
        app.set_loops(vec![loop_summary(
            first_id,
            "alice",
            "2026-04-10T10:00:00Z",
        )]);

        assert_eq!(
            app.current_log_selection(),
            Some(LogSelection {
                loop_id: first_id,
                source: LogSource::AgentPod,
                job_name: Some("job-1".to_string()),
            })
        );
    }

    #[test]
    fn overlapping_suffix_len_matches_appended_pod_logs() {
        let previous = vec!["a".to_string(), "b".to_string()];
        let current = vec!["a".to_string(), "b".to_string(), "c".to_string()];

        assert_eq!(overlapping_suffix_len(&previous, &current), 2);
    }

    #[test]
    fn theme_cycling_works() {
        let mut app = App::new(false, &default_helm_config(), "default".to_string());
        assert_eq!(app.theme_name, ThemeName::Dark);
        assert_eq!(
            app.handle_input(KeyEvent::from(KeyCode::Char('T'))),
            AppAction::ThemeCycle
        );
        app.theme_name = app.theme_name.cycle();
        assert_eq!(app.theme_name, ThemeName::Light);
    }

    #[test]
    fn view_switching_works() {
        let mut app = App::new(false, &default_helm_config(), "default".to_string());
        assert_eq!(app.main_view, MainView::Logs);
        assert_eq!(
            app.handle_input(KeyEvent::from(KeyCode::Char('d'))),
            AppAction::ViewSwitch(MainView::Diff)
        );
        assert_eq!(
            app.handle_input(KeyEvent::from(KeyCode::Char('m'))),
            AppAction::ViewSwitch(MainView::MultiLoop)
        );
        assert_eq!(
            app.handle_input(KeyEvent::from(KeyCode::Char('R'))),
            AppAction::ViewSwitch(MainView::RoundsTable)
        );
    }

    #[test]
    fn cancel_requires_confirmation() {
        let mut app = App::new(false, &default_helm_config(), "default".to_string());
        let id = uuid::Uuid::new_v4();
        app.set_loops(vec![LoopSummary {
            state: "IMPLEMENTING".to_string(),
            ..loop_summary(id, "alice", "2026-04-10T10:00:00Z")
        }]);

        // Press x -> should set cancel_confirm_at
        let action = app.handle_input(KeyEvent::from(KeyCode::Char('x')));
        assert_eq!(action, AppAction::None);
        assert!(app.cancel_confirm_at.is_some());

        // Press y -> should trigger cancel
        let action = app.handle_input(KeyEvent::from(KeyCode::Char('y')));
        assert_eq!(action, AppAction::Trigger(LoopCommand::Cancel));
        assert!(app.cancel_confirm_at.is_none());
    }

    #[test]
    fn cancel_confirmation_aborts_on_other_key() {
        let mut app = App::new(false, &default_helm_config(), "default".to_string());
        let id = uuid::Uuid::new_v4();
        app.set_loops(vec![LoopSummary {
            state: "IMPLEMENTING".to_string(),
            ..loop_summary(id, "alice", "2026-04-10T10:00:00Z")
        }]);

        app.handle_input(KeyEvent::from(KeyCode::Char('x')));
        assert!(app.cancel_confirm_at.is_some());

        // Press 'n' -> should abort
        let action = app.handle_input(KeyEvent::from(KeyCode::Char('n')));
        assert_eq!(action, AppAction::None);
        assert!(app.cancel_confirm_at.is_none());
    }

    #[test]
    fn cancel_confirmation_times_out_after_3s() {
        let mut app = App::new(false, &default_helm_config(), "default".to_string());
        let id = uuid::Uuid::new_v4();
        app.set_loops(vec![LoopSummary {
            state: "IMPLEMENTING".to_string(),
            ..loop_summary(id, "alice", "2026-04-10T10:00:00Z")
        }]);

        app.handle_input(KeyEvent::from(KeyCode::Char('x')));
        assert!(app.cancel_confirm_at.is_some());

        // Simulate 4 seconds passing
        app.cancel_confirm_at = Some(Instant::now() - Duration::from_secs(4));

        // Press y after timeout -> should NOT trigger cancel
        let action = app.handle_input(KeyEvent::from(KeyCode::Char('y')));
        assert_eq!(action, AppAction::None);
        assert!(app.cancel_confirm_at.is_none());
    }

    #[test]
    fn convergence_detection_sets_flash_and_row_flash() {
        let id = uuid::Uuid::new_v4();
        let helm_config = HelmConfig::default();
        let mut app = App::new(false, &helm_config, "default".to_string());

        // Set up initial state
        app.previous_states.insert(id, "IMPLEMENTING".to_string());
        app.set_loops(vec![LoopSummary {
            state: "CONVERGED".to_string(),
            ..loop_summary(id, "alice", "2026-04-10T10:00:00Z")
        }]);
        app.detect_state_changes();

        // Should have a status flash
        assert!(app.active_status_flash().is_some());
        assert!(app.active_status_flash().unwrap().contains("CONVERGED"));
        // Should have a row flash
        assert!(app.has_row_flash(&id));
    }
}
