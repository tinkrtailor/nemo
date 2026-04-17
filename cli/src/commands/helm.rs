use std::collections::VecDeque;
use std::io::{self, Stdout};
use std::time::Duration;

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
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap};
use tokio::sync::{mpsc, watch};

use crate::api_types::{InspectResponse, LoopSummary, PodIntrospectResponse, RoundSummary};
use crate::client::NemoClient;
use crate::commands::{inspect, ps, status};

const BG: Color = Color::Rgb(15, 15, 14);
const SURFACE: Color = Color::Rgb(26, 25, 24);
const BORDER: Color = Color::Rgb(46, 45, 43);
const TEXT: Color = Color::Rgb(232, 230, 227);
const MUTED: Color = Color::Rgb(138, 135, 132);
const TEAL: Color = Color::Rgb(27, 107, 90);
const AMBER: Color = Color::Rgb(232, 168, 56);
const GREEN: Color = Color::Rgb(45, 122, 79);
const RED: Color = Color::Rgb(196, 57, 45);
const BLUE: Color = Color::Rgb(59, 123, 192);
const MAX_LOG_LINES: usize = 500;
const POD_TAIL_LINES: u32 = 200;

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
}

/// Side-panel toggle states for the 'p' key (FR-5a).
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
enum LoopCommand {
    Approve,
    Resume,
    Cancel,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AppAction {
    None,
    Quit,
    SelectionChanged,
    SourceChanged,
    ReconnectLogs,
    Trigger(LoopCommand),
    PanelToggle,
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

#[derive(Debug)]
struct App {
    loops: Vec<LoopSummary>,
    list_state: ListState,
    selected_loop_id: Option<uuid::Uuid>,
    logs: VecDeque<String>,
    log_source: LogSource,
    status_line: String,
    log_status: String,
    inspect_status: String,
    inspect: Option<InspectResponse>,
    team_view: bool,
    side_panel: SidePanel,
    introspect: Option<PodIntrospectResponse>,
    introspect_status: String,
}

impl App {
    fn new(team_view: bool) -> Self {
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
            introspect_status: "Press 'p' to toggle introspect pane".to_string(),
        }
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
            self.status_line = format!(
                "{} active loop{}",
                self.loops.len(),
                if self.loops.len() == 1 { "" } else { "s" }
            );
        }
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
            .map(|loop_item| format!("Loading inspect data for {}", loop_item.branch))
            .unwrap_or_else(|| "Select a loop to inspect".to_string());
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

    fn push_log_line(&mut self, line: String) {
        if self.logs.len() == MAX_LOG_LINES {
            self.logs.pop_front();
        }
        self.logs.push_back(line);
    }

    fn handle_input(&mut self, key: KeyEvent) -> AppAction {
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => AppAction::Quit,
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
            KeyCode::Char('p') => {
                self.cycle_side_panel();
                AppAction::PanelToggle
            }
            KeyCode::Char('a') => AppAction::Trigger(LoopCommand::Approve),
            KeyCode::Char('u') => AppAction::Trigger(LoopCommand::Resume),
            KeyCode::Char('c') => AppAction::Trigger(LoopCommand::Cancel),
            KeyCode::Char('r') => AppAction::ReconnectLogs,
            _ => AppAction::None,
        }
    }
}

enum StreamOutcome {
    HistoricalComplete,
    Ended(String),
    Disconnected,
}

pub async fn run(client: &NemoClient, engineer: &str, team: bool) -> Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    terminal.clear()?;

    let result = run_app(&mut terminal, client.clone(), engineer.to_string(), team).await;

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
) -> Result<()> {
    let (event_tx, mut event_rx) = mpsc::unbounded_channel();
    let (selection_tx, selection_rx) = watch::channel(None::<LogSelection>);
    let (inspect_tx, inspect_rx) = watch::channel(None::<String>);
    let (introspect_tx, introspect_rx) = watch::channel(None::<uuid::Uuid>);

    spawn_input_task(event_tx.clone());
    spawn_status_task(client.clone(), engineer.clone(), team, event_tx.clone());
    spawn_log_task(client.clone(), selection_rx, event_tx.clone());
    spawn_inspect_task(client.clone(), inspect_rx, event_tx.clone());
    spawn_introspect_task(client.clone(), introspect_rx, event_tx.clone());

    let mut app = App::new(team);

    loop {
        terminal.draw(|frame| render(frame, &mut app))?;

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
                    // Only show "Loading..." when actively polling; otherwise neutral message
                    if app.side_panel == SidePanel::Introspect {
                        app.introspect_status = "Loading...".to_string();
                        let _ = introspect_tx.send(app.selected_loop_id);
                    } else {
                        app.introspect_status = "Press p to toggle introspect pane".to_string();
                    }
                    let _ = selection_tx.send(app.current_log_selection());
                    let _ = inspect_tx.send(app.selected_branch());
                }
                AppAction::ReconnectLogs | AppAction::SourceChanged => {
                    app.reset_logs();
                    let _ = selection_tx.send(app.current_log_selection());
                }
                AppAction::PanelToggle => {
                    match app.side_panel {
                        SidePanel::Introspect => {
                            // Reset to clean state before starting to poll
                            app.introspect = None;
                            app.introspect_status = "Loading...".to_string();
                            let _ = introspect_tx.send(app.selected_loop_id);
                        }
                        SidePanel::Inspect => {
                            // Ensure inspect data is fetched when toggling to panel
                            let _ = inspect_tx.send(app.selected_branch());
                            let _ = introspect_tx.send(None);
                        }
                        SidePanel::Closed => {
                            let _ = introspect_tx.send(None);
                        }
                    }
                }
                AppAction::Trigger(command) => {
                    if let Some(loop_id) = app.selected_loop_id {
                        app.status_line = format!("sending {} for {loop_id}", command.verb());
                        match perform_loop_action(&client, command, loop_id).await {
                            Ok(message) => {
                                app.status_line = message;
                            }
                            Err(error) => {
                                app.status_line =
                                    format!("{} failed for {loop_id}: {error}", command.verb());
                            }
                        }

                        match status::fetch(&client, &engineer, team).await {
                            Ok(response) => {
                                app.set_loops(response.loops);
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
                                app.status_line =
                                    format!("{} sent, but refresh failed: {error}", command.verb());
                            }
                        }
                    } else {
                        app.status_line = format!("No loop selected for {}", command.verb());
                    }
                }
                AppAction::None => {}
            },
            AppEvent::Resize => {}
            AppEvent::Status(loops) => {
                app.set_loops(loops);
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
            AppEvent::InspectLoaded(branch, inspect) => {
                if app.selected_branch().as_deref() == Some(branch.as_str()) {
                    app.inspect_status = format!("inspect synced for {branch}");
                    app.inspect = Some(inspect);
                }
            }
            AppEvent::InspectError(branch, error) => {
                if app.selected_branch().as_deref() == Some(branch.as_str()) {
                    app.inspect = None;
                    app.inspect_status = format!("inspect refresh failed: {error}");
                }
            }
            AppEvent::LogLine(loop_id, line) => {
                if Some(loop_id) == app.selected_loop_id {
                    app.push_log_line(line);
                }
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
        }
    }

    Ok(())
}

impl LoopCommand {
    fn verb(self) -> &'static str {
        match self {
            Self::Approve => "approve",
            Self::Resume => "resume",
            Self::Cancel => "cancel",
        }
    }
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
                return; // FR-5c: no polling after terminal
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

fn render(frame: &mut ratatui::Frame<'_>, app: &mut App) {
    let root = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(1)])
        .split(frame.area());
    let content = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(34), Constraint::Percentage(66)])
        .split(root[0]);
    let right = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(17), Constraint::Min(0)])
        .split(content[1]);

    frame.render_widget(render_details(app), right[0]);

    // FR-5a/FR-5d: log pane is always visible; side pane overlays right side
    match app.side_panel {
        SidePanel::Closed => {
            frame.render_widget(render_logs(app, right[1]), right[1]);
        }
        SidePanel::Inspect => {
            // FR-5a: inspect pane shows existing rounds view
            let log_inspect = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Percentage(55), Constraint::Percentage(45)])
                .split(right[1]);
            frame.render_widget(render_logs(app, log_inspect[0]), log_inspect[0]);
            frame.render_widget(render_inspect_pane(app), log_inspect[1]);
        }
        SidePanel::Introspect => {
            // FR-5d: horizontal split — logs left, introspect right
            let log_introspect = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Percentage(55), Constraint::Percentage(45)])
                .split(right[1]);
            frame.render_widget(render_logs(app, log_introspect[0]), log_introspect[0]);
            frame.render_widget(render_introspect_pane(app), log_introspect[1]);
        }
    }

    frame.render_stateful_widget(render_loop_selector(app), content[0], &mut app.list_state);
    frame.render_widget(render_footer(app), root[1]);
}

fn render_loop_selector(app: &App) -> List<'static> {
    let items = if app.loops.is_empty() {
        vec![ListItem::new(Line::from(Span::styled(
            "No active loops",
            Style::default().fg(MUTED),
        )))]
    } else {
        app.loops
            .iter()
            .map(|loop_item| {
                let stage = loop_item.current_stage.as_deref().unwrap_or("-");
                let line = format!(
                    "{: <10} {: <18} {: <8} r{: <3} {}",
                    loop_item.engineer,
                    state_label(loop_item),
                    stage,
                    loop_item.round,
                    loop_item.spec_path
                );
                ListItem::new(Line::from(Span::styled(line, Style::default().fg(TEXT))))
            })
            .collect()
    };

    List::new(items)
        .block(
            Block::default()
                .title(Span::styled(
                    format!(" helm {} ", app.status_line),
                    Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
                ))
                .borders(Borders::ALL)
                .border_style(Style::default().fg(BORDER).bg(SURFACE))
                .style(Style::default().bg(SURFACE)),
        )
        .highlight_style(
            Style::default()
                .fg(TEXT)
                .bg(Color::Rgb(36, 35, 34))
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("> ")
}

fn render_details(app: &App) -> Paragraph<'static> {
    let body = if let Some(loop_item) = app.selected_loop() {
        let mut lines = vec![
            detail_line("engineer", &loop_item.engineer),
            Line::from(vec![
                Span::styled(
                    format!("{:>8} ", "state"),
                    Style::default().fg(MUTED).add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    state_label(loop_item),
                    Style::default()
                        .fg(state_color(&loop_item.state))
                        .add_modifier(Modifier::BOLD),
                ),
            ]),
            detail_line("stage", loop_item.current_stage.as_deref().unwrap_or("-")),
            detail_line("round", &loop_item.round.to_string()),
            detail_line("job", loop_item.active_job_name.as_deref().unwrap_or("-")),
            detail_line("branch", &loop_item.branch),
            detail_line("loop", &loop_item.loop_id.to_string()),
            detail_line("spec", &loop_item.spec_path),
            Line::from(Span::styled("", Style::default())),
            Line::from(vec![
                Span::styled(
                    format!("{:>8} ", "inspect"),
                    Style::default().fg(MUTED).add_modifier(Modifier::BOLD),
                ),
                Span::styled(app.inspect_status.clone(), Style::default().fg(MUTED)),
            ]),
        ];

        if let Some(inspect) = &app.inspect
            && let Some(round) = latest_round(inspect)
        {
            lines.push(detail_line("latest", &format!("round {}", round.round)));
            for (label, summary) in round_stage_summaries(round) {
                lines.push(detail_line(label, &summary));
            }
        }

        Text::from(lines)
    } else {
        Text::from(vec![Line::from(Span::styled(
            "Waiting for an active loop selection",
            Style::default().fg(MUTED),
        ))])
    };

    Paragraph::new(body)
        .block(
            Block::default()
                .title(Span::styled(
                    " overview + inspect ",
                    Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
                ))
                .borders(Borders::ALL)
                .border_style(Style::default().fg(BORDER).bg(SURFACE))
                .style(Style::default().bg(SURFACE)),
        )
        .style(Style::default().fg(TEXT).bg(SURFACE))
        .wrap(Wrap { trim: false })
}

fn render_logs(app: &App, area: Rect) -> Paragraph<'static> {
    let lines: Vec<Line<'static>> = if app.logs.is_empty() {
        vec![Line::from(Span::styled(
            app.log_status.clone(),
            Style::default().fg(MUTED),
        ))]
    } else {
        app.logs
            .iter()
            .map(|line| Line::from(Span::styled(line.clone(), Style::default().fg(TEXT))))
            .collect()
    };

    let inner_height = area.height.saturating_sub(2) as usize;
    let scroll = lines.len().saturating_sub(inner_height) as u16;

    Paragraph::new(Text::from(lines))
        .block(
            Block::default()
                .title(Span::styled(
                    format!(" logs {} ", app.log_source.label()),
                    Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
                ))
                .borders(Borders::ALL)
                .border_style(Style::default().fg(BORDER).bg(SURFACE))
                .style(Style::default().bg(BG)),
        )
        .style(Style::default().fg(TEXT).bg(BG))
        .wrap(Wrap { trim: false })
        .scroll((scroll, 0))
}

/// Render the inspect pane showing rounds/stage data (FR-5a).
fn render_inspect_pane(app: &App) -> Paragraph<'static> {
    let body = if let Some(inspect) = &app.inspect {
        let mut lines = vec![
            Line::from(vec![
                Span::styled("Branch ", Style::default().fg(MUTED)),
                Span::styled(inspect.branch.clone(), Style::default().fg(TEXT)),
                Span::styled(
                    format!("  {} round{}", inspect.rounds.len(), if inspect.rounds.len() == 1 { "" } else { "s" }),
                    Style::default().fg(MUTED),
                ),
            ]),
            Line::from(Span::styled("", Style::default())),
        ];

        for round in inspect.rounds.iter().rev() {
            lines.push(Line::from(Span::styled(
                format!("Round {}", round.round),
                Style::default().fg(TEAL).add_modifier(Modifier::BOLD),
            )));
            let summaries = round_stage_summaries(round);
            for (label, summary) in summaries {
                lines.push(Line::from(vec![
                    Span::styled(
                        format!("  {label:>7} "),
                        Style::default().fg(MUTED).add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(summary, Style::default().fg(TEXT)),
                ]));
            }
            lines.push(Line::from(Span::styled("", Style::default())));
        }

        Text::from(lines)
    } else {
        Text::from(vec![Line::from(Span::styled(
            app.inspect_status.clone(),
            Style::default().fg(MUTED),
        ))])
    };

    Paragraph::new(body)
        .block(
            Block::default()
                .title(Span::styled(
                    " inspect ",
                    Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
                ))
                .borders(Borders::ALL)
                .border_style(Style::default().fg(BORDER).bg(SURFACE))
                .style(Style::default().bg(SURFACE)),
        )
        .style(Style::default().fg(TEXT).bg(SURFACE))
        .wrap(Wrap { trim: false })
}

/// Render the introspect pane (FR-5b).
fn render_introspect_pane(app: &App) -> Paragraph<'static> {
    let body = if let Some(snapshot) = &app.introspect {
        let mut lines = vec![
            Line::from(vec![
                Span::styled("Pod ", Style::default().fg(MUTED)),
                Span::styled(snapshot.pod_name.clone(), Style::default().fg(TEXT)),
                Span::styled("  Phase ", Style::default().fg(MUTED)),
                Span::styled(
                    snapshot.pod_phase.clone(),
                    Style::default()
                        .fg(if snapshot.pod_phase == "Running" {
                            GREEN
                        } else {
                            AMBER
                        })
                        .add_modifier(Modifier::BOLD),
                ),
            ]),
        ];

        // Stats section
        match &snapshot.container_stats {
            Some(stats) => {
                let mem_mib = stats.memory_bytes / (1024 * 1024);
                lines.push(Line::from(vec![
                    Span::styled("CPU ", Style::default().fg(MUTED)),
                    Span::styled(format!("{}m", stats.cpu_millicores), Style::default().fg(TEXT)),
                    Span::styled("  Mem ", Style::default().fg(MUTED)),
                    Span::styled(format!("{mem_mib} MiB"), Style::default().fg(TEXT)),
                ]));
            }
            None => {
                lines.push(Line::from(Span::styled(
                    "Stats: unavailable",
                    Style::default().fg(MUTED),
                )));
            }
        }

        // Worktree section
        let wt = &snapshot.worktree;
        let head = wt.head_sha.as_deref().map(|s| &s[..s.len().min(7)]).unwrap_or("-");
        let target_info = match (wt.target_dir_bytes, wt.target_dir_artifacts) {
            (Some(bytes), Some(arts)) => {
                let gib = bytes as f64 / (1024.0 * 1024.0 * 1024.0);
                format!("{gib:.1} GiB ({arts} arts)")
            }
            _ => "-".to_string(),
        };
        lines.push(Line::from(vec![
            Span::styled("HEAD ", Style::default().fg(MUTED)),
            Span::styled(head.to_string(), Style::default().fg(TEXT)),
            Span::styled(
                match wt.uncommitted_files {
                    Some(n) => format!("  dirty={n}"),
                    None => "  dirty=?".to_string(),
                },
                Style::default().fg(
                    if wt.uncommitted_files.unwrap_or(0) > 0 { AMBER } else { TEXT }
                ),
            ),
            Span::styled(format!("  target={target_info}"), Style::default().fg(MUTED)),
        ]));

        lines.push(Line::from(Span::styled("", Style::default())));

        // Processes section (top 10 by CPU)
        lines.push(Line::from(Span::styled(
            format!(
                "{:<5}{:<5}{:<6}{:<6}{}",
                "PID", "PPID", "CPU%", "AGE", "COMMAND"
            ),
            Style::default().fg(MUTED).add_modifier(Modifier::BOLD),
        )));
        for p in snapshot.processes.iter().take(10) {
            let age = if p.age_seconds >= 3600 {
                format!("{}h{}m", p.age_seconds / 3600, (p.age_seconds % 3600) / 60)
            } else if p.age_seconds >= 60 {
                format!("{}m", p.age_seconds / 60)
            } else {
                format!("{}s", p.age_seconds)
            };
            let cpu_color = if p.cpu_percent > 10.0 { AMBER } else { TEXT };
            lines.push(Line::from(vec![
                Span::styled(format!("{:<5}", p.pid), Style::default().fg(TEXT)),
                Span::styled(format!("{:<5}", p.ppid), Style::default().fg(MUTED)),
                Span::styled(format!("{:<6.1}", p.cpu_percent), Style::default().fg(cpu_color)),
                Span::styled(format!("{:<6}", age), Style::default().fg(TEXT)),
                Span::styled(
                    p.cmd.chars().take(40).collect::<String>(),
                    Style::default().fg(TEXT),
                ),
            ]));
        }

        if snapshot.processes.is_empty() {
            lines.push(Line::from(Span::styled(
                "(no processes)",
                Style::default().fg(MUTED),
            )));
        }

        if !snapshot.warnings.is_empty() {
            lines.push(Line::from(Span::styled("", Style::default())));
            for w in &snapshot.warnings {
                lines.push(Line::from(Span::styled(
                    format!("⚠ {w}"),
                    Style::default().fg(AMBER),
                )));
            }
        }

        Text::from(lines)
    } else {
        Text::from(vec![Line::from(Span::styled(
            app.introspect_status.clone(),
            Style::default().fg(MUTED),
        ))])
    };

    Paragraph::new(body)
        .block(
            Block::default()
                .title(Span::styled(
                    " introspect ",
                    Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
                ))
                .borders(Borders::ALL)
                .border_style(Style::default().fg(BORDER).bg(SURFACE))
                .style(Style::default().bg(SURFACE)),
        )
        .style(Style::default().fg(TEXT).bg(SURFACE))
        .wrap(Wrap { trim: false })
}

fn latest_round(inspect: &InspectResponse) -> Option<&RoundSummary> {
    inspect.rounds.iter().max_by_key(|round| round.round)
}

fn round_stage_summaries(round: &RoundSummary) -> Vec<(&'static str, String)> {
    let mut summaries = Vec::new();

    if let Some(summary) = summarize_impl_stage(round.implement.as_ref()) {
        summaries.push(("impl", summary));
    }
    if let Some(summary) = summarize_test_stage(round.test.as_ref()) {
        summaries.push(("test", summary));
    }
    if let Some(summary) = summarize_verdict_stage(round.review.as_ref(), "review") {
        summaries.push(("review", summary));
    }
    if let Some(summary) = summarize_verdict_stage(round.audit.as_ref(), "audit") {
        summaries.push(("audit", summary));
    }
    if let Some(summary) = summarize_revise_stage(round.revise.as_ref()) {
        summaries.push(("revise", summary));
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
    let summary = verdict
        .get("summary")
        .and_then(|summary| summary.as_str())
        .unwrap_or("")
        .trim();

    Some(match (clean, summary.is_empty()) {
        (true, true) => format!("clean {kind}"),
        (true, false) => format!("clean, {summary}"),
        (false, true) => format!(
            "{issue_count} issue{}",
            if issue_count == 1 { "" } else { "s" }
        ),
        (false, false) => format!(
            "{issue_count} issue{}, {summary}",
            if issue_count == 1 { "" } else { "s" }
        ),
    })
}

fn short_sha(sha: &str) -> &str {
    let len = sha.len().min(8);
    &sha[..len]
}

fn render_footer(app: &App) -> Paragraph<'static> {
    let mode = if app.team_view { "team" } else { "engineer" };
    Paragraph::new(Line::from(vec![
        Span::styled("mode ", Style::default().fg(MUTED)),
        Span::styled(mode, Style::default().fg(BLUE).add_modifier(Modifier::BOLD)),
        Span::raw("   "),
        Span::styled("keys ", Style::default().fg(MUTED)),
        Span::styled("q", Style::default().fg(TEAL).add_modifier(Modifier::BOLD)),
        Span::styled(" quit  ", Style::default().fg(MUTED)),
        Span::styled(
            "j/k",
            Style::default().fg(TEAL).add_modifier(Modifier::BOLD),
        ),
        Span::styled(" move  ", Style::default().fg(MUTED)),
        Span::styled(
            "g/G",
            Style::default().fg(TEAL).add_modifier(Modifier::BOLD),
        ),
        Span::styled(" top/bottom  ", Style::default().fg(MUTED)),
        Span::styled("l", Style::default().fg(BLUE).add_modifier(Modifier::BOLD)),
        Span::styled(" log source  ", Style::default().fg(MUTED)),
        Span::styled("r", Style::default().fg(AMBER).add_modifier(Modifier::BOLD)),
        Span::styled(" reconnect  ", Style::default().fg(MUTED)),
        Span::styled("p", Style::default().fg(BLUE).add_modifier(Modifier::BOLD)),
        Span::styled(" introspect  ", Style::default().fg(MUTED)),
        Span::styled(
            "a/u/c",
            Style::default().fg(TEAL).add_modifier(Modifier::BOLD),
        ),
        Span::styled(" approve/resume/cancel", Style::default().fg(MUTED)),
    ]))
    .style(Style::default().fg(TEXT).bg(BG))
}

fn detail_line(label: &str, value: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            format!("{label:>8} "),
            Style::default().fg(MUTED).add_modifier(Modifier::BOLD),
        ),
        Span::styled(value.to_string(), Style::default().fg(TEXT)),
    ])
}

fn state_label(loop_item: &LoopSummary) -> String {
    match &loop_item.sub_state {
        Some(sub_state) => format!("{}/{}", loop_item.state, sub_state),
        None => loop_item.state.clone(),
    }
}

fn state_color(state: &str) -> Color {
    if matches!(state, "CONVERGED" | "HARDENED" | "SHIPPED") {
        GREEN
    } else if matches!(state, "FAILED" | "CANCELLED") {
        RED
    } else if matches!(state, "PAUSED" | "AWAITING_REAUTH" | "AWAITING_APPROVAL") {
        AMBER
    } else {
        TEAL
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
            created_at: updated_at.to_string(),
            updated_at: updated_at.to_string(),
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
        let mut app = App::new(false);
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
        let mut app = App::new(false);

        assert_eq!(
            app.handle_input(KeyEvent::from(KeyCode::Char('a'))),
            AppAction::Trigger(LoopCommand::Approve)
        );
        assert_eq!(
            app.handle_input(KeyEvent::from(KeyCode::Char('u'))),
            AppAction::Trigger(LoopCommand::Resume)
        );
        assert_eq!(
            app.handle_input(KeyEvent::from(KeyCode::Char('c'))),
            AppAction::Trigger(LoopCommand::Cancel)
        );
    }

    #[test]
    fn log_source_hotkey_cycles_sources() {
        let mut app = App::new(false);

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
        let mut app = App::new(false);
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
        let mut app = App::new(false);
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
}
