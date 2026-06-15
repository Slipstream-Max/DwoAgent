//! Local terminal dashboard for agent profile files.

use std::collections::{BTreeMap, HashMap};
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Result;
use chrono::{Datelike, Duration as ChronoDuration, Local, NaiveDate};
use crossterm::cursor::{Hide, Show};
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use crossterm::execute;
use crossterm::terminal::{self as crossterm_terminal, EnterAlternateScreen, LeaveAlternateScreen};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, BorderType, Borders, Gauge, List, ListItem, Paragraph, Tabs, Wrap};
use ratatui::{Frame, Terminal};
use serde::Deserialize;
use serde_json::{Map, Value};

use crate::agent::session::{
    SESSION_CLIENT_TRANSCRIPT_FILE, SESSION_META_FILE, SESSION_MODEL_CONTEXT_FILE,
};
use crate::automation::{
    AutomationConfig, AutomationRunRecord, AutomationRunStatus, AutomationSchedule,
    AutomationSessionConfig, load_automation_config,
};
use crate::config::loader::{
    agent_yaml_path, channel_secret_dir, read_agent_meta, read_json_model, read_model_registry,
    resolve_agent_structure_dir, resolve_session_store_dir,
};
use crate::config::models::{
    AgentMeta, AgentState, ContextUsageSnapshot, ModelProfile, SessionMetaPayload,
    SessionModelContextPayload,
};
use crate::context::contextblock::skills::parser::read_properties;
use crate::context::contextblock::skills::prompt::discover_skills;
use crate::ingress::config::{
    ChannelRuntimeConfig, FeishuAccessPolicy, FeishuChannelDomain, load_channel_runtime_config,
};
use crate::utils::files::read_utf8_text;

const PAGE_COUNT: usize = 6;
const AGENT_TAB_COUNT: usize = 4;

pub fn run_tui(agent_folder: PathBuf) -> Result<()> {
    let snapshot = TuiSnapshot::load(&agent_folder)?;
    let mut app = App {
        agent_folder,
        snapshot,
        page_index: 0,
        selected_index: 0,
        agent_tab_index: 0,
        focus: FocusPane::Nav,
        last_error: None,
    };
    run_loop(&mut app)
}

fn run_loop(app: &mut App) -> Result<()> {
    let _guard = TerminalGuard::enter()?;
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend)?;

    terminal.draw(|frame| draw(app, frame))?;
    loop {
        if !event::poll(Duration::from_millis(250))? {
            continue;
        }
        let should_draw = match event::read()? {
            Event::Key(key) if key.kind == KeyEventKind::Press => match key.code {
                KeyCode::Char('q') => break,
                KeyCode::Esc => {
                    if app.focus == FocusPane::Content {
                        app.focus_nav();
                        true
                    } else {
                        break;
                    }
                }
                KeyCode::Char('r') => {
                    app.refresh();
                    true
                }
                KeyCode::Tab | KeyCode::BackTab => {
                    app.toggle_focus();
                    true
                }
                KeyCode::Enter | KeyCode::Right if app.focus == FocusPane::Nav => {
                    app.focus_content();
                    true
                }
                KeyCode::Left if app.focus == FocusPane::Content => {
                    app.focus_nav();
                    true
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    if app.focus == FocusPane::Nav {
                        app.move_page(1);
                    } else {
                        app.move_selection(1);
                    }
                    true
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    if app.focus == FocusPane::Nav {
                        app.move_page(-1);
                    } else {
                        app.move_selection(-1);
                    }
                    true
                }
                KeyCode::Char(']')
                    if app.focus == FocusPane::Content && app.page() == Page::Agent =>
                {
                    app.next_agent_tab();
                    true
                }
                KeyCode::Char('[')
                    if app.focus == FocusPane::Content && app.page() == Page::Agent =>
                {
                    app.previous_agent_tab();
                    true
                }
                KeyCode::Char(ch) if app.focus == FocusPane::Nav && ('1'..='6').contains(&ch) => {
                    app.page_index = (ch as usize) - ('1' as usize);
                    app.selected_index = 0;
                    true
                }
                _ => false,
            },
            Event::Key(_) => false,
            Event::Resize(_, _) => true,
            _ => false,
        };
        if should_draw {
            terminal.draw(|frame| draw(app, frame))?;
        }
    }
    Ok(())
}

struct TerminalGuard;

impl TerminalGuard {
    fn enter() -> Result<Self> {
        crossterm_terminal::enable_raw_mode()?;
        execute!(io::stdout(), EnterAlternateScreen, Hide)?;
        Ok(Self)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = crossterm_terminal::disable_raw_mode();
        let _ = execute!(io::stdout(), Show, LeaveAlternateScreen);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Page {
    Overview,
    Agent,
    Sessions,
    Channels,
    Automation,
    Logs,
}

impl Page {
    const ALL: [Self; PAGE_COUNT] = [
        Self::Overview,
        Self::Agent,
        Self::Sessions,
        Self::Channels,
        Self::Automation,
        Self::Logs,
    ];

    fn title(self) -> &'static str {
        match self {
            Self::Overview => "Overview",
            Self::Agent => "Agent",
            Self::Sessions => "Sessions",
            Self::Channels => "Channels",
            Self::Automation => "Automation",
            Self::Logs => "Logs",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FocusPane {
    Nav,
    Content,
}

struct App {
    agent_folder: PathBuf,
    snapshot: TuiSnapshot,
    page_index: usize,
    selected_index: usize,
    agent_tab_index: usize,
    focus: FocusPane,
    last_error: Option<String>,
}

impl App {
    fn page(&self) -> Page {
        Page::ALL[self.page_index]
    }

    fn refresh(&mut self) {
        match TuiSnapshot::load(&self.agent_folder) {
            Ok(snapshot) => {
                self.snapshot = snapshot;
                self.last_error = None;
                self.clamp_selection();
            }
            Err(err) => {
                self.last_error = Some(format!("{err:#}"));
            }
        }
    }

    fn next_agent_tab(&mut self) {
        self.agent_tab_index = (self.agent_tab_index + 1) % AGENT_TAB_COUNT;
        self.selected_index = 0;
    }

    fn previous_agent_tab(&mut self) {
        self.agent_tab_index = (self.agent_tab_index + AGENT_TAB_COUNT - 1) % AGENT_TAB_COUNT;
        self.selected_index = 0;
    }

    fn toggle_focus(&mut self) {
        if self.focus == FocusPane::Nav {
            self.focus_content();
        } else {
            self.focus_nav();
        }
    }

    fn focus_content(&mut self) {
        self.focus = FocusPane::Content;
        self.clamp_selection();
    }

    fn focus_nav(&mut self) {
        self.focus = FocusPane::Nav;
    }

    fn move_page(&mut self, delta: isize) {
        let current = self.page_index as isize;
        let max = (PAGE_COUNT - 1) as isize;
        let next = (current + delta).clamp(0, max) as usize;
        if next != self.page_index {
            self.page_index = next;
            self.selected_index = 0;
        }
    }

    fn move_selection(&mut self, delta: isize) {
        let len = self.selection_len();
        if len == 0 {
            self.selected_index = 0;
            return;
        }
        let current = self.selected_index as isize;
        let max = (len - 1) as isize;
        self.selected_index = (current + delta).clamp(0, max) as usize;
    }

    fn clamp_selection(&mut self) {
        let len = self.selection_len();
        if len == 0 {
            self.selected_index = 0;
        } else if self.selected_index >= len {
            self.selected_index = len - 1;
        }
    }

    fn selection_len(&self) -> usize {
        match self.page() {
            Page::Overview => self.snapshot.logs.len(),
            Page::Agent => match self.agent_tab_index {
                0 => self.snapshot.skills.len(),
                1 => 1,
                2 => self.snapshot.mcp.servers.len(),
                _ => self.snapshot.rules.len(),
            },
            Page::Sessions => self.snapshot.sessions.len(),
            Page::Channels => channel_rows(&self.snapshot.channels).len(),
            Page::Automation => self.snapshot.automation.jobs.len(),
            Page::Logs => self.snapshot.logs.len(),
        }
    }
}

#[derive(Clone)]
struct TuiSnapshot {
    agent_structure_dir: PathBuf,
    meta: AgentMeta,
    default_model_id: String,
    model_profiles: HashMap<String, ModelProfile>,
    channels: ChannelRuntimeConfig,
    automation: AutomationConfig,
    sessions: Vec<SessionView>,
    skills: Vec<SkillView>,
    prompt: PromptView,
    mcp: McpView,
    rules: Vec<RuleView>,
    runs: Vec<RunView>,
    service: ServiceView,
    logs: Vec<LogEvent>,
    daily_usage: Vec<DailyModelUsage>,
}

impl TuiSnapshot {
    fn load(agent_folder: &Path) -> Result<Self> {
        let agent_structure_dir = resolve_agent_structure_dir(agent_folder)?;
        let agent_yaml = agent_yaml_path(&agent_structure_dir);
        let meta = read_agent_meta(&agent_yaml)?;
        let (default_model_id, model_profiles) = read_model_registry(&agent_yaml)?;
        let session_store_dir =
            resolve_session_store_dir(&meta.session_store_dir, &agent_structure_dir)?;
        let channels = load_channel_runtime_config(&agent_structure_dir)?;
        let automation = load_automation_config(&agent_structure_dir)?;
        let sessions = load_sessions(&session_store_dir, &model_profiles);
        let skills = load_skills(&agent_structure_dir, &meta);
        let prompt = load_prompt(&agent_structure_dir, &meta);
        let mcp = load_mcp(&agent_structure_dir);
        let rules = load_rules(&agent_structure_dir, &meta);
        let runs = load_runs(&sessions);
        let service = load_service(&agent_structure_dir);
        let logs = load_logs(&sessions, &runs);
        let daily_usage = load_daily_model_usage(&sessions);

        Ok(Self {
            agent_structure_dir,
            meta,
            default_model_id,
            model_profiles,
            channels,
            automation,
            sessions,
            skills,
            prompt,
            mcp,
            rules,
            runs,
            service,
            logs,
            daily_usage,
        })
    }
}

#[derive(Clone)]
struct SessionView {
    meta: SessionMetaPayload,
    dir: PathBuf,
    usage: ContextUsageSnapshot,
    context_window: u64,
    tool_names: Vec<String>,
    model_call_count: usize,
}

#[derive(Clone)]
struct SkillView {
    name: String,
    source: String,
    path: PathBuf,
    description: String,
    status: String,
}

#[derive(Clone)]
struct PromptView {
    path: PathBuf,
    status: String,
    lines: Vec<String>,
}

#[derive(Clone)]
struct McpView {
    path: PathBuf,
    status: String,
    servers: Vec<McpServerView>,
    error: Option<String>,
}

#[derive(Clone)]
struct McpServerView {
    name: String,
    transport: String,
    target: String,
    details: Vec<String>,
}

#[derive(Clone)]
struct RuleView {
    source: String,
    path: PathBuf,
    status: String,
    lines: Vec<String>,
}

#[derive(Clone)]
struct RunView {
    record: AutomationRunRecord,
    path: PathBuf,
}

#[derive(Clone)]
struct ServiceView {
    status: String,
    pid: Option<u32>,
    started_at: Option<String>,
    manifest_path: PathBuf,
}

#[derive(Clone)]
struct LogEvent {
    timestamp: String,
    source: String,
    summary: String,
    path: Option<PathBuf>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct DailyModelUsage {
    date: NaiveDate,
    calls: u32,
    tokens: u64,
}

#[derive(Debug, Deserialize)]
struct DaemonManifest {
    pid: u32,
    started_at: String,
}

fn load_sessions(
    session_store_dir: &Path,
    model_profiles: &HashMap<String, ModelProfile>,
) -> Vec<SessionView> {
    let mut sessions = Vec::new();
    for session_dir in session_dirs(session_store_dir) {
        let meta_path = session_dir.join(SESSION_META_FILE);
        let Ok(meta) = read_json_model::<SessionMetaPayload>(&meta_path) else {
            continue;
        };
        let context_path = session_dir.join(SESSION_MODEL_CONTEXT_FILE);
        let context_payload = read_json_model::<SessionModelContextPayload>(&context_path).ok();
        let usage = context_payload
            .as_ref()
            .and_then(|payload| payload.usage)
            .unwrap_or_default();
        let model_call_count = context_payload
            .as_ref()
            .map(|payload| assistant_message_count(&payload.messages))
            .unwrap_or(0);
        let profile_window = model_profiles
            .get(&meta.model_id)
            .map(|profile| profile.context_window as u64)
            .unwrap_or(0);
        let context_window = if usage.size > 0 {
            usage.size
        } else {
            profile_window
        };
        let tool_names = session_tool_names(&meta);
        sessions.push(SessionView {
            meta,
            dir: session_dir,
            usage,
            context_window,
            tool_names,
            model_call_count,
        });
    }
    sessions.sort_by(|a, b| updated_at(&b.meta).cmp(updated_at(&a.meta)));
    sessions
}

fn session_dirs(session_store_dir: &Path) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    for year_dir in date_child_dirs(session_store_dir, 4) {
        for month_dir in date_child_dirs(&year_dir, 2) {
            for day_dir in date_child_dirs(&month_dir, 2) {
                let Ok(entries) = std::fs::read_dir(&day_dir) else {
                    continue;
                };
                for entry in entries.flatten() {
                    let session_dir = entry.path();
                    if session_dir.is_dir()
                        && session_dir.join(SESSION_META_FILE).is_file()
                        && session_dir.join(SESSION_MODEL_CONTEXT_FILE).is_file()
                    {
                        dirs.push(session_dir);
                    }
                }
            }
        }
    }
    dirs
}

fn date_child_dirs(parent: &Path, name_len: usize) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(parent) else {
        return Vec::new();
    };
    let mut out: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.is_dir()
                && path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| {
                        name.len() == name_len && name.chars().all(|ch| ch.is_ascii_digit())
                    })
        })
        .collect();
    out.sort();
    out
}

fn load_skills(agent_structure_dir: &Path, meta: &AgentMeta) -> Vec<SkillView> {
    let mut skills = Vec::new();
    let mut roots = Vec::new();
    roots.push((
        "resources/skills".to_string(),
        agent_structure_dir.join("resources").join("skills"),
    ));
    for configured in &meta.external_skills_dirs {
        let path = resolve_config_path(agent_structure_dir, configured);
        roots.push((format!("external: {}", path.display()), path));
    }

    for (source, root) in roots {
        for skill_dir in discover_skills(&root) {
            let skill_md = skill_dir.join("SKILL.md");
            match read_properties(&skill_dir) {
                Ok(props) => skills.push(SkillView {
                    name: props.name,
                    source: source.clone(),
                    path: skill_md,
                    description: props.description,
                    status: "available".to_string(),
                }),
                Err(err) => skills.push(SkillView {
                    name: display_file_name(&skill_dir),
                    source: source.clone(),
                    path: skill_md,
                    description: format!("{err}"),
                    status: "invalid".to_string(),
                }),
            }
        }
    }
    skills.sort_by(|a, b| a.name.cmp(&b.name).then(a.path.cmp(&b.path)));
    skills
}

fn load_prompt(agent_structure_dir: &Path, _meta: &AgentMeta) -> PromptView {
    let path = agent_structure_dir
        .join("resources")
        .join("prompt")
        .join("system.md");
    match read_preview_lines(&path) {
        Ok(lines) if lines.is_empty() => PromptView {
            path,
            status: "empty".to_string(),
            lines,
        },
        Ok(lines) => PromptView {
            path,
            status: "available".to_string(),
            lines,
        },
        Err(_) => PromptView {
            path,
            status: "missing".to_string(),
            lines: Vec::new(),
        },
    }
}

fn load_rules(agent_structure_dir: &Path, meta: &AgentMeta) -> Vec<RuleView> {
    let mut rules = Vec::new();
    let agent_rule = agent_structure_dir
        .join("resources")
        .join("prompt")
        .join("AGENTS.md");
    rules.push(load_rule_view("agent rule", agent_rule));
    for configured in &meta.external_rule_files {
        let path = resolve_config_path(agent_structure_dir, configured);
        rules.push(load_rule_view("external rule", path));
    }
    rules
}

fn load_mcp(agent_structure_dir: &Path) -> McpView {
    let path = agent_structure_dir.join("resources").join("mcp.json");
    if !path.is_file() {
        return McpView {
            path,
            status: "missing".to_string(),
            servers: Vec::new(),
            error: None,
        };
    }

    let text = match read_utf8_text(&path) {
        Ok(text) => text,
        Err(err) => {
            return McpView {
                path,
                status: "invalid".to_string(),
                servers: Vec::new(),
                error: Some(err.to_string()),
            };
        }
    };
    let value = match serde_json::from_str::<Value>(&text) {
        Ok(value) => value,
        Err(err) => {
            return McpView {
                path,
                status: "invalid".to_string(),
                servers: Vec::new(),
                error: Some(err.to_string()),
            };
        }
    };

    let Some((_source, server_map)) = mcp_server_map(&value) else {
        return McpView {
            path,
            status: "empty".to_string(),
            servers: Vec::new(),
            error: Some("no mcpServers object found".to_string()),
        };
    };

    let mut servers = server_map
        .iter()
        .map(|(name, config)| mcp_server_view(name, config))
        .collect::<Vec<_>>();
    servers.sort_by(|a, b| a.name.cmp(&b.name));
    let status = if servers.is_empty() {
        "empty"
    } else {
        "available"
    };
    McpView {
        path,
        status: status.to_string(),
        servers,
        error: None,
    }
}

fn mcp_server_map(value: &Value) -> Option<(&'static str, &Map<String, Value>)> {
    value
        .get("mcpServers")
        .and_then(Value::as_object)
        .map(|servers| ("mcpServers", servers))
        .or_else(|| {
            value
                .get("servers")
                .and_then(Value::as_object)
                .map(|servers| ("servers", servers))
        })
}

fn mcp_server_view(name: &str, config: &Value) -> McpServerView {
    let Some(map) = config.as_object() else {
        return McpServerView {
            name: name.to_string(),
            transport: "invalid".to_string(),
            target: "server entry is not an object".to_string(),
            details: Vec::new(),
        };
    };

    McpServerView {
        name: name.to_string(),
        transport: mcp_transport_label(map),
        target: mcp_target_label(map),
        details: mcp_server_details(map),
    }
}

fn mcp_transport_label(config: &Map<String, Value>) -> String {
    config
        .get("transport")
        .and_then(Value::as_str)
        .or_else(|| config.get("type").and_then(Value::as_str))
        .map(str::to_string)
        .unwrap_or_else(|| {
            if config.get("url").and_then(Value::as_str).is_some() {
                "http".to_string()
            } else if config.get("command").and_then(Value::as_str).is_some() {
                "stdio".to_string()
            } else {
                "unknown".to_string()
            }
        })
}

fn mcp_target_label(config: &Map<String, Value>) -> String {
    config
        .get("command")
        .and_then(Value::as_str)
        .or_else(|| config.get("url").and_then(Value::as_str))
        .filter(|value| !value.trim().is_empty())
        .unwrap_or("-")
        .to_string()
}

fn mcp_server_details(config: &Map<String, Value>) -> Vec<String> {
    let mut details = Vec::new();
    if let Some(args) = config.get("args").and_then(Value::as_array) {
        details.push(format!("args: {}", args.len()));
    }
    if let Some(keys) = object_keys(config.get("env")) {
        details.push(format!("env keys: {}", join_limited(&keys, 120)));
    }
    if let Some(keys) = object_keys(config.get("headers")) {
        details.push(format!("header keys: {}", join_limited(&keys, 120)));
    }

    let known = [
        "args",
        "command",
        "env",
        "headers",
        "transport",
        "type",
        "url",
    ];
    let mut other_keys = config
        .keys()
        .filter(|key| !known.contains(&key.as_str()))
        .cloned()
        .collect::<Vec<_>>();
    other_keys.sort();
    if !other_keys.is_empty() {
        details.push(format!("other keys: {}", join_limited(&other_keys, 120)));
    }
    details
}

fn object_keys(value: Option<&Value>) -> Option<Vec<String>> {
    let mut keys = value?.as_object()?.keys().cloned().collect::<Vec<_>>();
    keys.sort();
    Some(keys)
}

fn load_rule_view(source: &str, path: PathBuf) -> RuleView {
    match read_preview_lines(&path) {
        Ok(lines) if lines.is_empty() => RuleView {
            source: source.to_string(),
            path,
            status: "empty".to_string(),
            lines,
        },
        Ok(lines) => RuleView {
            source: source.to_string(),
            path,
            status: "available".to_string(),
            lines,
        },
        Err(_) => RuleView {
            source: source.to_string(),
            path,
            status: "missing".to_string(),
            lines: Vec::new(),
        },
    }
}

fn read_preview_lines(path: &Path) -> Result<Vec<String>> {
    let text = read_utf8_text(path)?;
    Ok(text
        .trim()
        .lines()
        .take(120)
        .map(|line| line.to_string())
        .collect())
}

fn load_runs(sessions: &[SessionView]) -> Vec<RunView> {
    let mut runs = Vec::new();
    for session in sessions {
        let automation_dir = session.dir.join("automation");
        let Ok(job_entries) = std::fs::read_dir(&automation_dir) else {
            continue;
        };
        for job_entry in job_entries.flatten() {
            let runs_dir = job_entry.path().join("runs");
            let Ok(run_entries) = std::fs::read_dir(&runs_dir) else {
                continue;
            };
            for run_entry in run_entries.flatten() {
                let run_path = run_entry.path().join("run.yaml");
                let Ok(text) = read_utf8_text(&run_path) else {
                    continue;
                };
                let Ok(record) = serde_yaml::from_str::<AutomationRunRecord>(&text) else {
                    continue;
                };
                runs.push(RunView {
                    record,
                    path: run_path,
                });
            }
        }
    }
    runs.sort_by(|a, b| b.record.started_at.cmp(&a.record.started_at));
    runs
}

fn load_service(agent_structure_dir: &Path) -> ServiceView {
    let manifest_path = channel_secret_dir(agent_structure_dir)
        .join("stdio")
        .join("daemon.yaml");
    if let Ok(text) = read_utf8_text(&manifest_path)
        && let Ok(manifest) = serde_yaml::from_str::<DaemonManifest>(&text)
    {
        return ServiceView {
            status: "stdio daemon manifest present".to_string(),
            pid: Some(manifest.pid),
            started_at: Some(manifest.started_at),
            manifest_path,
        };
    }
    ServiceView {
        status: "not connected; local files only".to_string(),
        pid: None,
        started_at: None,
        manifest_path,
    }
}

fn load_logs(sessions: &[SessionView], runs: &[RunView]) -> Vec<LogEvent> {
    let mut logs = Vec::new();
    for run in runs {
        logs.push(LogEvent {
            timestamp: run.record.started_at.clone(),
            source: "automation".to_string(),
            summary: format!(
                "{} {} session {}",
                run.record.job_id,
                run_status(run.record.status),
                short_id(&run.record.session_id)
            ),
            path: Some(run.path.clone()),
        });
    }
    for session in sessions {
        let transcript_path = session.dir.join(SESSION_CLIENT_TRANSCRIPT_FILE);
        let Ok(text) = read_utf8_text(&transcript_path) else {
            continue;
        };
        for line in text.lines().rev().take(3) {
            if let Ok(value) = serde_json::from_str::<Value>(line) {
                let timestamp = value
                    .get("updated_at")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                logs.push(LogEvent {
                    timestamp,
                    source: "session".to_string(),
                    summary: format!(
                        "{} {}",
                        short_id(&session.meta.session_id),
                        summarize_json_value(&value)
                    ),
                    path: Some(transcript_path.clone()),
                });
            }
        }
    }
    logs.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));
    logs
}

fn load_daily_model_usage(sessions: &[SessionView]) -> Vec<DailyModelUsage> {
    let mut days = BTreeMap::new();
    for session in sessions {
        let transcript_path = session.dir.join(SESSION_CLIENT_TRANSCRIPT_FILE);
        let (call_days, token_days) = match read_utf8_text(&transcript_path) {
            Ok(text) => (
                model_call_days_from_transcript(&text),
                token_days_from_transcript(&text),
            ),
            Err(_) => (BTreeMap::new(), BTreeMap::new()),
        };

        if call_days.is_empty() {
            if let Some(date) = session_date(session) {
                add_daily_calls(
                    &mut days,
                    date,
                    session.model_call_count.min(u32::MAX as usize) as u32,
                );
            }
        } else {
            for (date, calls) in call_days {
                add_daily_calls(&mut days, date, calls);
            }
        }

        if token_days.is_empty() {
            if let Some(date) = session_date(session) {
                add_daily_tokens(&mut days, date, session.usage.used);
            }
        } else {
            for (date, tokens) in token_days {
                add_daily_tokens(&mut days, date, tokens);
            }
        }
    }
    days.into_values().collect()
}

fn add_daily_calls(days: &mut BTreeMap<NaiveDate, DailyModelUsage>, date: NaiveDate, calls: u32) {
    let entry = days.entry(date).or_insert(DailyModelUsage {
        date,
        calls: 0,
        tokens: 0,
    });
    entry.calls = entry.calls.saturating_add(calls);
}

fn add_daily_tokens(days: &mut BTreeMap<NaiveDate, DailyModelUsage>, date: NaiveDate, tokens: u64) {
    let entry = days.entry(date).or_insert(DailyModelUsage {
        date,
        calls: 0,
        tokens: 0,
    });
    entry.tokens = entry.tokens.saturating_add(tokens);
}

fn model_call_days_from_transcript(text: &str) -> BTreeMap<NaiveDate, u32> {
    let mut days = BTreeMap::new();
    let mut in_model_call = false;
    for line in text.lines() {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let update_type = transcript_update_type(&value);
        if is_model_call_event(update_type) {
            if !in_model_call && let Some(date) = transcript_date(&value) {
                *days.entry(date).or_insert(0) += 1;
            }
            in_model_call = true;
        } else {
            in_model_call = false;
        }
    }
    days
}

fn token_days_from_transcript(text: &str) -> BTreeMap<NaiveDate, u64> {
    let mut days = BTreeMap::new();
    let mut previous_used = 0;
    let mut saw_usage = false;
    for line in text.lines() {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if transcript_update_type(&value) != "usage_update" {
            continue;
        }
        let Some(date) = transcript_date(&value) else {
            continue;
        };
        let Some(used) = value
            .get("update")
            .and_then(|update| update.get("used"))
            .and_then(Value::as_u64)
        else {
            continue;
        };
        let delta = if !saw_usage || used < previous_used {
            used
        } else {
            used - previous_used
        };
        saw_usage = true;
        previous_used = used;
        *days.entry(date).or_insert(0) += delta;
    }
    days
}

fn is_model_call_event(update_type: &str) -> bool {
    matches!(
        update_type,
        "agent_thought_chunk" | "agent_message_chunk" | "tool_call"
    )
}

fn transcript_update_type(value: &Value) -> &str {
    value
        .get("update")
        .and_then(|update| update.get("session_update"))
        .and_then(Value::as_str)
        .unwrap_or("")
}

fn transcript_date(value: &Value) -> Option<NaiveDate> {
    value
        .get("updated_at")
        .and_then(Value::as_str)
        .and_then(parse_date_prefix)
}

fn draw(app: &App, frame: &mut Frame) {
    let area = frame.area();
    frame.render_widget(Block::new().style(Style::default().bg(color_bg())), area);

    let root = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .split(area);
    render_header(app, frame, root[0]);
    render_body(app, frame, root[1]);
    render_footer(app, frame, root[2]);
}

fn render_header(app: &App, frame: &mut Frame, area: Rect) {
    let title = Line::from(vec![
        Span::styled("[ ", muted_style()),
        Span::styled(&app.snapshot.meta.name, accent_style()),
        Span::styled(" - dwo-agent TUI ", text_style()),
        Span::styled("• ", muted_style()),
        Span::styled(app.page().title(), blue_style()),
        Span::styled(" • ", muted_style()),
        Span::styled(focus_label(app.focus), green_style()),
        Span::styled(" ]", muted_style()),
    ]);
    let folder = Line::from(vec![
        Span::styled("folder ", muted_style()),
        Span::styled(
            app.snapshot.agent_structure_dir.display().to_string(),
            dim_style(),
        ),
    ]);
    frame.render_widget(
        Paragraph::new(Text::from(vec![title, folder]))
            .alignment(Alignment::Center)
            .style(text_style()),
        area,
    );
}

fn render_body(app: &App, frame: &mut Frame, area: Rect) {
    let nav_width = if area.width < 90 { 24 } else { 30 };
    let body = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(nav_width), Constraint::Min(40)])
        .split(area);
    let left = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(8), Constraint::Length(8)])
        .split(body[0]);

    render_nav(app, frame, left[0]);
    render_service_panel(app, frame, left[1]);
    render_page(app, frame, body[1]);
}

fn render_nav(app: &App, frame: &mut Frame, area: Rect) {
    let items = Page::ALL
        .iter()
        .enumerate()
        .map(|(index, page)| {
            let is_current = index == app.page_index;
            let is_focused = is_current && app.focus == FocusPane::Nav;
            let marker = if is_focused {
                "→"
            } else if is_current {
                "•"
            } else {
                " "
            };
            let style = if is_focused {
                selected_style()
            } else if is_current {
                blue_style().add_modifier(Modifier::BOLD)
            } else {
                text_style()
            };
            ListItem::new(Line::from(vec![
                Span::styled(format!("{marker} "), green_style()),
                Span::styled(format!("{}. ", index + 1), muted_style()),
                Span::styled(page.title(), style),
            ]))
        })
        .collect::<Vec<_>>();

    let block = panel_block("Pages", app.focus == FocusPane::Nav);
    frame.render_widget(
        List::new(items)
            .block(block)
            .style(text_style())
            .highlight_symbol(""),
        area,
    );
}

fn render_service_panel(app: &App, frame: &mut Frame, area: Rect) {
    let service = &app.snapshot.service;
    let mut lines = vec![
        kv_line("status", &service.status),
        kv_line(
            "pid",
            &service
                .pid
                .map(|pid| pid.to_string())
                .unwrap_or_else(|| "-".to_string()),
        ),
        kv_line("started", service.started_at.as_deref().unwrap_or("-")),
        kv_line("manifest", service.manifest_path.display()),
    ];
    if app.last_error.is_some() {
        lines.push(Line::from(Span::styled("refresh failed", error_style())));
    }
    frame.render_widget(
        Paragraph::new(Text::from(lines))
            .block(panel_block("Service", false))
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn render_page(app: &App, frame: &mut Frame, area: Rect) {
    match app.page() {
        Page::Overview => render_overview(app, frame, area),
        Page::Agent => render_agent(app, frame, area),
        Page::Sessions => render_sessions(app, frame, area),
        Page::Channels => render_channels(app, frame, area),
        Page::Automation => render_automation(app, frame, area),
        Page::Logs => render_logs(app, frame, area),
    }
}

fn render_overview(app: &App, frame: &mut Frame, area: Rect) {
    let snapshot = &app.snapshot;

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(8),
            Constraint::Length(12),
            Constraint::Min(7),
        ])
        .split(area);

    let profile_lines = vec![
        kv_line("agent", &snapshot.meta.name),
        kv_line("agent_id", &snapshot.meta.agent_id),
        kv_line("default_model", &snapshot.default_model_id),
        kv_line("models", &snapshot.model_profiles.len().to_string()),
        kv_line("policy", snapshot.meta.policy_mode.as_str()),
        kv_line("mcp", &mcp_overview_label(&snapshot.mcp)),
    ];
    frame.render_widget(
        Paragraph::new(Text::from(profile_lines))
            .block(panel_block("Overview", app.focus == FocusPane::Content))
            .wrap(Wrap { trim: false }),
        chunks[0],
    );

    render_model_usage_heatmap(app, frame, chunks[1]);

    render_log_list(app, frame, chunks[2], "Recent Activity", 10);
}

fn render_agent(app: &App, frame: &mut Frame, area: Rect) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(1)])
        .split(area);
    let tabs = Tabs::new(vec!["Skills", "Prompt", "MCP", "Rules"])
        .select(app.agent_tab_index)
        .block(panel_block("Agent", app.focus == FocusPane::Content))
        .style(muted_style())
        .highlight_style(accent_style().add_modifier(Modifier::BOLD));
    frame.render_widget(tabs, chunks[0]);

    match app.agent_tab_index {
        0 => render_agent_skills(app, frame, chunks[1]),
        1 => render_agent_prompt(app, frame, chunks[1]),
        2 => render_agent_mcp(app, frame, chunks[1]),
        _ => render_agent_rules(app, frame, chunks[1]),
    }
}

fn render_agent_skills(app: &App, frame: &mut Frame, area: Rect) {
    let chunks = split_pair(area);
    let items = app
        .snapshot
        .skills
        .iter()
        .enumerate()
        .map(|(index, skill)| {
            list_item(
                app,
                index,
                vec![
                    Span::styled(&skill.name, text_style()),
                    Span::styled("  ", muted_style()),
                    Span::styled(&skill.status, status_style(&skill.status)),
                ],
            )
        })
        .collect::<Vec<_>>();
    frame.render_widget(
        list_or_empty(items, "no skills found")
            .block(panel_block("Skills", app.focus == FocusPane::Content)),
        chunks[0],
    );

    let lines = if app.snapshot.skills.is_empty() {
        vec![Line::from(Span::styled(
            "resources/skills and external skill dirs are empty",
            dim_style(),
        ))]
    } else {
        let skill = selected(&app.snapshot.skills, app.selected_index);
        vec![
            kv_line("name", &skill.name),
            kv_line("source", &skill.source),
            kv_line("status", &skill.status),
            kv_line("path", &skill.path.display().to_string()),
            Line::from(""),
            Line::from(Span::styled("description", accent_style())),
            Line::from(Span::styled(&skill.description, text_style())),
        ]
    };
    render_text_panel(frame, chunks[1], "Skill Detail", lines, false);
}

fn render_agent_prompt(app: &App, frame: &mut Frame, area: Rect) {
    let prompt = &app.snapshot.prompt;
    let mut lines = vec![
        kv_line("file", &prompt.path.display().to_string()),
        kv_line("status", &prompt.status),
        Line::from(""),
    ];
    if prompt.lines.is_empty() {
        lines.push(Line::from(Span::styled("no prompt content", dim_style())));
    } else {
        lines.extend(numbered_lines(&prompt.lines, 80));
    }
    render_text_panel(
        frame,
        area,
        "Prompt",
        lines,
        app.focus == FocusPane::Content,
    );
}

fn render_agent_mcp(app: &App, frame: &mut Frame, area: Rect) {
    let mcp = &app.snapshot.mcp;
    let chunks = split_pair(area);
    let items = mcp
        .servers
        .iter()
        .enumerate()
        .map(|(index, server)| {
            list_item(
                app,
                index,
                vec![
                    Span::styled(&server.name, text_style()),
                    Span::styled("  ", muted_style()),
                    Span::styled(&server.transport, blue_style()),
                ],
            )
        })
        .collect::<Vec<_>>();
    frame.render_widget(
        list_or_empty(items, "no MCP servers configured")
            .block(panel_block("MCP Servers", app.focus == FocusPane::Content)),
        chunks[0],
    );

    let mut lines = vec![
        kv_line("file", &mcp.path.display().to_string()),
        kv_line("status", &mcp.status),
        kv_line("servers", &mcp.servers.len().to_string()),
        Line::from(""),
    ];
    if let Some(error) = &mcp.error {
        lines.push(Line::from(Span::styled(error, error_style())));
    } else if mcp.servers.is_empty() {
        lines.push(Line::from(Span::styled(
            "add entries under resources/mcp.json -> mcpServers to expose MCP candidates",
            dim_style(),
        )));
    } else {
        let server = selected(&mcp.servers, app.selected_index);
        lines.extend([
            kv_line("name", &server.name),
            kv_line("transport", &server.transport),
            kv_line("target", &server.target),
            Line::from(""),
        ]);
        if server.details.is_empty() {
            lines.push(Line::from(Span::styled("no extra fields", dim_style())));
        } else {
            lines.extend(
                server
                    .details
                    .iter()
                    .map(|detail| Line::from(Span::styled(detail, text_style()))),
            );
        }
    }
    render_text_panel(frame, chunks[1], "MCP Detail", lines, false);
}

fn render_agent_rules(app: &App, frame: &mut Frame, area: Rect) {
    let chunks = split_pair(area);
    let items = app
        .snapshot
        .rules
        .iter()
        .enumerate()
        .map(|(index, rule)| {
            list_item(
                app,
                index,
                vec![
                    Span::styled(&rule.source, text_style()),
                    Span::styled("  ", muted_style()),
                    Span::styled(&rule.status, status_style(&rule.status)),
                ],
            )
        })
        .collect::<Vec<_>>();
    frame.render_widget(
        list_or_empty(items, "no rule files configured")
            .block(panel_block("Rules", app.focus == FocusPane::Content)),
        chunks[0],
    );

    let lines = if app.snapshot.rules.is_empty() {
        vec![Line::from(Span::styled(
            "no rule files configured",
            dim_style(),
        ))]
    } else {
        let rule = selected(&app.snapshot.rules, app.selected_index);
        let mut lines = vec![
            kv_line("source", &rule.source),
            kv_line("status", &rule.status),
            kv_line("path", &rule.path.display().to_string()),
            Line::from(""),
        ];
        if rule.lines.is_empty() {
            lines.push(Line::from(Span::styled("no rule content", dim_style())));
        } else {
            lines.extend(numbered_lines(&rule.lines, 50));
        }
        lines
    };
    render_text_panel(frame, chunks[1], "Rule Detail", lines, false);
}

fn render_sessions(app: &App, frame: &mut Frame, area: Rect) {
    let chunks = split_pair(area);
    let items = app
        .snapshot
        .sessions
        .iter()
        .enumerate()
        .map(|(index, session)| {
            list_item(
                app,
                index,
                vec![
                    Span::styled(display_timestamp(updated_at(&session.meta)), muted_style()),
                    Span::styled("  ", muted_style()),
                    Span::styled(session.meta.state.as_str(), state_style(session.meta.state)),
                    Span::styled("  ", muted_style()),
                    Span::styled(display_session_title(session), text_style()),
                ],
            )
        })
        .collect::<Vec<_>>();
    frame.render_widget(
        list_or_empty(items, "no sessions under runtime/sessions/YYYY/MM/DD")
            .block(panel_block("Sessions", app.focus == FocusPane::Content)),
        chunks[0],
    );

    if app.snapshot.sessions.is_empty() {
        render_text_panel(
            frame,
            chunks[1],
            "Session Detail",
            vec![Line::from(Span::styled("no session selected", dim_style()))],
            false,
        );
        return;
    }
    let session = selected(&app.snapshot.sessions, app.selected_index);
    let detail = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(5), Constraint::Min(1)])
        .split(chunks[1]);
    let ratio = context_ratio(session);
    frame.render_widget(
        Gauge::default()
            .block(panel_block("Context Window", false))
            .gauge_style(Style::default().fg(color_green()).bg(color_surface()))
            .label(format!(
                "{} / {} ({})",
                session.usage.used,
                session.context_window,
                context_percent_label(session)
            ))
            .ratio(ratio),
        detail[0],
    );
    let lines = vec![
        kv_line("id", &session.meta.session_id),
        kv_line("title", &display_session_title(session)),
        kv_line("model", &session.meta.model_id),
        kv_line("thinking", session.meta.reasoning_mode.as_str()),
        kv_line("policy", session.meta.mode_id.as_str()),
        kv_line("state", session.meta.state.as_str()),
        kv_line("cwd", &session.meta.cwd),
        kv_line("path", &session.dir.display().to_string()),
        kv_line("tools", &join_limited(&session.tool_names, 180)),
    ];
    render_text_panel(frame, detail[1], "Session Detail", lines, false);
}

fn render_channels(app: &App, frame: &mut Frame, area: Rect) {
    let rows = channel_rows(&app.snapshot.channels);
    let chunks = split_pair(area);
    let items = rows
        .iter()
        .enumerate()
        .map(|(index, row)| {
            list_item(
                app,
                index,
                vec![
                    Span::styled(row.name, text_style()),
                    Span::styled("  ", muted_style()),
                    Span::styled(
                        if row.enabled { "enabled" } else { "disabled" },
                        bool_style(row.enabled),
                    ),
                ],
            )
        })
        .collect::<Vec<_>>();
    frame.render_widget(
        List::new(items).block(panel_block("Channels", app.focus == FocusPane::Content)),
        chunks[0],
    );

    let row = selected(&rows, app.selected_index);
    let mut lines = vec![
        kv_line("channel", row.name),
        kv_line("status", if row.enabled { "enabled" } else { "disabled" }),
        kv_line("session switch", "supported"),
        Line::from(""),
    ];
    lines.extend(row.details.iter().map(|line| Line::from(line.as_str())));
    render_text_panel(frame, chunks[1], "Channel Detail", lines, false);
}

fn render_automation(app: &App, frame: &mut Frame, area: Rect) {
    let chunks = split_pair(area);
    let jobs = &app.snapshot.automation.jobs;
    let items = jobs
        .iter()
        .enumerate()
        .map(|(index, job)| {
            list_item(
                app,
                index,
                vec![
                    Span::styled(&job.id, text_style()),
                    Span::styled("  ", muted_style()),
                    Span::styled(
                        if job.enabled { "enabled" } else { "disabled" },
                        bool_style(job.enabled),
                    ),
                ],
            )
        })
        .collect::<Vec<_>>();
    frame.render_widget(
        list_or_empty(items, "no automation jobs configured")
            .block(panel_block("Jobs", app.focus == FocusPane::Content)),
        chunks[0],
    );

    let right = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(8), Constraint::Min(1)])
        .split(chunks[1]);
    if jobs.is_empty() {
        render_text_panel(
            frame,
            right[0],
            "Job Detail",
            vec![Line::from(Span::styled(
                "scheduler has no jobs",
                dim_style(),
            ))],
            false,
        );
        render_text_panel(
            frame,
            right[1],
            "Runs",
            vec![Line::from(Span::styled("no runs", dim_style()))],
            false,
        );
        return;
    }
    let job = selected(jobs, app.selected_index);
    let lines = vec![
        kv_line("id", &job.id),
        kv_line("status", if job.enabled { "enabled" } else { "disabled" }),
        kv_line("schedule", &schedule_label(&job.schedule)),
        kv_line("session", &session_mode_label(&job.session)),
        kv_line("workspace", &job.workspace_dir),
        kv_line("notify", &job.notify.len().to_string()),
    ];
    render_text_panel(frame, right[0], "Job Detail", lines, false);

    let run_items = app
        .snapshot
        .runs
        .iter()
        .filter(|run| run.record.job_id == job.id)
        .take(12)
        .map(|run| {
            ListItem::new(Line::from(vec![
                Span::styled(display_timestamp(&run.record.started_at), muted_style()),
                Span::styled("  ", muted_style()),
                Span::styled(
                    run_status(run.record.status),
                    run_status_style(run.record.status),
                ),
                Span::styled("  session ", muted_style()),
                Span::styled(short_id(&run.record.session_id), text_style()),
            ]))
        })
        .collect::<Vec<_>>();
    frame.render_widget(
        list_or_empty(run_items, "no runs for this job").block(panel_block("Runs", false)),
        right[1],
    );
}

fn render_logs(app: &App, frame: &mut Frame, area: Rect) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(8), Constraint::Length(7)])
        .split(area);
    render_log_list(app, frame, chunks[0], "Logs", 30);

    let lines = if app.snapshot.logs.is_empty() {
        vec![Line::from(Span::styled("no local logs found", dim_style()))]
    } else {
        let log = selected(&app.snapshot.logs, app.selected_index);
        let mut lines = vec![
            kv_line("time", &log.timestamp),
            kv_line("source", &log.source),
            kv_line("summary", &log.summary),
        ];
        if let Some(path) = &log.path {
            lines.push(kv_line("path", &path.display().to_string()));
        }
        lines
    };
    render_text_panel(frame, chunks[1], "Log Detail", lines, false);
}

fn render_log_list(app: &App, frame: &mut Frame, area: Rect, title: &'static str, limit: usize) {
    let items = app
        .snapshot
        .logs
        .iter()
        .take(limit)
        .enumerate()
        .map(|(index, log)| {
            list_item(
                app,
                index,
                vec![
                    Span::styled(display_timestamp(&log.timestamp), muted_style()),
                    Span::styled("  ", muted_style()),
                    Span::styled(format!("[{}]", log.source), blue_style()),
                    Span::styled("  ", muted_style()),
                    Span::styled(&log.summary, text_style()),
                ],
            )
        })
        .collect::<Vec<_>>();
    frame.render_widget(
        list_or_empty(items, "no recent activity")
            .block(panel_block(title, app.focus == FocusPane::Content)),
        area,
    );
}

fn render_model_usage_heatmap(app: &App, frame: &mut Frame, area: Rect) {
    let today = Local::now().date_naive();
    let data = app
        .snapshot
        .daily_usage
        .iter()
        .map(|usage| (usage.date, *usage))
        .collect::<BTreeMap<_, _>>();
    let total_calls: u32 = app
        .snapshot
        .daily_usage
        .iter()
        .map(|usage| usage.calls)
        .sum();
    let peak_calls = app
        .snapshot
        .daily_usage
        .iter()
        .map(|usage| usage.calls)
        .max()
        .unwrap_or(0);
    let today_usage = data.get(&today).copied();
    let today_calls = today_usage.map(|usage| usage.calls).unwrap_or(0);
    let today_tokens = today_usage.map(|usage| usage.tokens).unwrap_or(0);
    let week_count = heatmap_week_count(area);
    let start = heatmap_start_date(today, week_count);

    let mut lines = vec![
        Line::from(vec![
            Span::styled("today ", muted_style()),
            Span::styled(format!("{today_calls} calls"), green_style()),
            Span::styled(" / ", muted_style()),
            Span::styled(
                format!("{} tokens", format_token_count(today_tokens)),
                blue_style(),
            ),
            Span::styled("   total ", muted_style()),
            Span::styled(format!("{total_calls} calls"), text_style()),
            Span::styled("   peak ", muted_style()),
            Span::styled(format!("{peak_calls}/day"), text_style()),
        ]),
        heatmap_month_header(start, week_count),
    ];

    for row in 0..7 {
        lines.push(heatmap_day_line(
            row, start, today, week_count, peak_calls, &data,
        ));
    }
    lines.push(heatmap_legend_line(peak_calls));

    frame.render_widget(
        Paragraph::new(Text::from(lines)).block(panel_block("Model Calls", false)),
        area,
    );
}

fn heatmap_week_count(area: Rect) -> usize {
    let inner_width = area.width.saturating_sub(2) as usize;
    let grid_width = inner_width.saturating_sub(4);
    (grid_width / 2).clamp(4, 53)
}

fn heatmap_start_date(today: NaiveDate, week_count: usize) -> NaiveDate {
    let days_since_monday = today.weekday().num_days_from_monday() as i64;
    let current_week_start = today - ChronoDuration::days(days_since_monday);
    current_week_start - ChronoDuration::weeks(week_count.saturating_sub(1) as i64)
}

fn heatmap_month_header(start: NaiveDate, week_count: usize) -> Line<'static> {
    let width = 4 + week_count * 2;
    let mut chars = vec![' '; width];
    let mut previous_month = None;
    for col in 0..week_count {
        let date = start + ChronoDuration::weeks(col as i64);
        let month = date.month();
        if col == 0 || previous_month != Some(month) {
            let label = month_label(month);
            let offset = 4 + col * 2;
            for (index, ch) in label.chars().enumerate() {
                if offset + index < chars.len() {
                    chars[offset + index] = ch;
                }
            }
        }
        previous_month = Some(month);
    }
    Line::from(Span::styled(
        chars.into_iter().collect::<String>(),
        muted_style(),
    ))
}

fn heatmap_day_line(
    row: usize,
    start: NaiveDate,
    today: NaiveDate,
    week_count: usize,
    peak_calls: u32,
    data: &BTreeMap<NaiveDate, DailyModelUsage>,
) -> Line<'static> {
    let mut spans = vec![Span::styled(
        format!("{:<3} ", weekday_label(row)),
        muted_style(),
    )];
    for col in 0..week_count {
        let date = start + ChronoDuration::days((col * 7 + row) as i64);
        if date > today {
            spans.push(Span::styled("  ", dim_style()));
            continue;
        }
        let calls = data.get(&date).map(|usage| usage.calls).unwrap_or(0);
        let marker = if calls == 0 { "· " } else { "● " };
        spans.push(Span::styled(
            marker,
            model_call_dot_style(calls, peak_calls),
        ));
    }
    Line::from(spans)
}

fn heatmap_legend_line(peak_calls: u32) -> Line<'static> {
    Line::from(vec![
        Span::styled("less ", muted_style()),
        Span::styled("· ", model_call_dot_style(0, peak_calls)),
        Span::styled("● ", model_call_dot_style(1, 4)),
        Span::styled("● ", model_call_dot_style(2, 4)),
        Span::styled("● ", model_call_dot_style(3, 4)),
        Span::styled("● ", model_call_dot_style(4, 4)),
        Span::styled("more", muted_style()),
    ])
}

fn render_text_panel(
    frame: &mut Frame,
    area: Rect,
    title: &'static str,
    lines: Vec<Line<'_>>,
    focused: bool,
) {
    frame.render_widget(
        Paragraph::new(Text::from(lines))
            .block(panel_block(title, focused))
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn split_pair(area: Rect) -> Vec<Rect> {
    if area.width < 88 {
        Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Percentage(45), Constraint::Percentage(55)])
            .split(area)
            .to_vec()
    } else {
        Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(42), Constraint::Percentage(58)])
            .split(area)
            .to_vec()
    }
}

fn panel_block(title: &'static str, focused: bool) -> Block<'static> {
    let style = if focused {
        Style::default().fg(color_green()).bg(color_bg())
    } else {
        Style::default().fg(color_border()).bg(color_bg())
    };
    let title_style = if focused {
        green_style().add_modifier(Modifier::BOLD)
    } else {
        muted_style()
    };
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Plain)
        .border_style(style)
        .title(Span::styled(title, title_style))
        .style(Style::default().bg(color_bg()))
}

fn list_item<'a>(app: &App, index: usize, spans: Vec<Span<'a>>) -> ListItem<'a> {
    let selected = app.focus == FocusPane::Content && app.selected_index == index;
    let mut line_spans = vec![Span::styled(
        if selected { "→ " } else { "  " },
        if selected {
            green_style()
        } else {
            muted_style()
        },
    )];
    line_spans.extend(spans);
    ListItem::new(Line::from(line_spans)).style(if selected {
        Style::default().fg(color_text()).bg(color_selected())
    } else {
        text_style()
    })
}

fn list_or_empty<'a>(items: Vec<ListItem<'a>>, empty: &'static str) -> List<'a> {
    if items.is_empty() {
        List::new(vec![ListItem::new(Line::from(Span::styled(
            empty,
            dim_style(),
        )))])
    } else {
        List::new(items)
    }
}

fn kv_line(key: &'static str, value: impl std::fmt::Display) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{key:<14}"), muted_style()),
        Span::styled(value.to_string(), text_style()),
    ])
}

fn numbered_lines(lines: &[String], limit: usize) -> Vec<Line<'_>> {
    lines
        .iter()
        .take(limit)
        .enumerate()
        .map(|(index, line)| {
            Line::from(vec![
                Span::styled(format!("{:>3}  ", index + 1), muted_style()),
                Span::styled(line.as_str(), text_style()),
            ])
        })
        .collect()
}

fn render_footer(app: &App, frame: &mut Frame, area: Rect) {
    let text = if let Some(err) = app.last_error.as_deref() {
        Line::from(Span::styled(
            format!("refresh failed: {err}"),
            error_style(),
        ))
    } else {
        match app.focus {
            FocusPane::Nav => Line::from(vec![
                Span::styled("[↑/↓]", muted_style()),
                Span::styled(" page  ", dim_style()),
                Span::styled("[Enter/→]", muted_style()),
                Span::styled(" content  ", dim_style()),
                Span::styled("[q]", muted_style()),
                Span::styled(" quit", dim_style()),
            ]),
            FocusPane::Content if app.page() == Page::Agent => Line::from(vec![
                Span::styled("[↑/↓]", muted_style()),
                Span::styled(" item  ", dim_style()),
                Span::styled("[←/Esc]", muted_style()),
                Span::styled(" nav  ", dim_style()),
                Span::styled("[[/]]", muted_style()),
                Span::styled(" tab  ", dim_style()),
                Span::styled("[r]", muted_style()),
                Span::styled(" refresh  ", dim_style()),
                Span::styled("[q]", muted_style()),
                Span::styled(" quit", dim_style()),
            ]),
            FocusPane::Content => Line::from(vec![
                Span::styled("[↑/↓]", muted_style()),
                Span::styled(" item  ", dim_style()),
                Span::styled("[←/Esc]", muted_style()),
                Span::styled(" nav  ", dim_style()),
                Span::styled("[r]", muted_style()),
                Span::styled(" refresh  ", dim_style()),
                Span::styled("[q]", muted_style()),
                Span::styled(" quit", dim_style()),
            ]),
        }
    };
    frame.render_widget(
        Paragraph::new(text).style(Style::default().fg(color_muted()).bg(color_bg())),
        area,
    );
}

fn focus_label(focus: FocusPane) -> &'static str {
    match focus {
        FocusPane::Nav => "nav",
        FocusPane::Content => "content",
    }
}

fn context_ratio(session: &SessionView) -> f64 {
    if session.context_window == 0 {
        0.0
    } else {
        (session.usage.used as f64 / session.context_window as f64).clamp(0.0, 1.0)
    }
}

fn color_bg() -> Color {
    Color::Rgb(18, 18, 20)
}
fn color_surface() -> Color {
    Color::Rgb(31, 31, 35)
}
fn color_selected() -> Color {
    Color::Rgb(43, 43, 48)
}
fn color_border() -> Color {
    Color::Rgb(116, 116, 124)
}
fn color_text() -> Color {
    Color::Rgb(224, 224, 230)
}
fn color_muted() -> Color {
    Color::Rgb(145, 145, 153)
}
fn color_dim() -> Color {
    Color::Rgb(88, 88, 96)
}
fn color_green() -> Color {
    Color::Rgb(175, 239, 174)
}
fn color_blue() -> Color {
    Color::Rgb(132, 177, 255)
}
fn color_cyan() -> Color {
    Color::Rgb(111, 214, 207)
}
fn color_pink() -> Color {
    Color::Rgb(245, 151, 179)
}
fn color_yellow() -> Color {
    Color::Rgb(244, 218, 146)
}
fn color_red() -> Color {
    Color::Rgb(255, 111, 111)
}

fn text_style() -> Style {
    Style::default().fg(color_text()).bg(color_bg())
}
fn muted_style() -> Style {
    Style::default().fg(color_muted()).bg(color_bg())
}
fn dim_style() -> Style {
    Style::default().fg(color_dim()).bg(color_bg())
}
fn accent_style() -> Style {
    Style::default().fg(color_cyan()).bg(color_bg())
}
fn green_style() -> Style {
    Style::default().fg(color_green()).bg(color_bg())
}
fn blue_style() -> Style {
    Style::default().fg(color_blue()).bg(color_bg())
}
fn model_call_dot_style(calls: u32, peak_calls: u32) -> Style {
    if calls == 0 || peak_calls == 0 {
        return dim_style();
    }
    let ratio = calls as f64 / peak_calls as f64;
    let color = if ratio <= 0.25 {
        Color::Rgb(72, 126, 92)
    } else if ratio <= 0.5 {
        Color::Rgb(93, 170, 111)
    } else if ratio <= 0.75 {
        color_green()
    } else {
        color_yellow()
    };
    Style::default().fg(color).bg(color_bg())
}
fn selected_style() -> Style {
    Style::default()
        .fg(color_green())
        .bg(color_selected())
        .add_modifier(Modifier::BOLD)
}
fn error_style() -> Style {
    Style::default().fg(color_red()).bg(color_bg())
}
fn bool_style(value: bool) -> Style {
    if value { green_style() } else { dim_style() }
}
fn status_style(status: &str) -> Style {
    match status {
        "available" | "enabled" => green_style(),
        "missing" | "disabled" => dim_style(),
        "invalid" | "failed" => error_style(),
        "empty" => muted_style(),
        _ => text_style(),
    }
}
fn state_style(state: AgentState) -> Style {
    match state {
        AgentState::Running => green_style(),
        AgentState::WaitingUserConfirm => Style::default().fg(color_yellow()).bg(color_bg()),
        AgentState::Cancelling => Style::default().fg(color_pink()).bg(color_bg()),
        AgentState::Idle | AgentState::Stop => muted_style(),
    }
}
fn run_status_style(status: AutomationRunStatus) -> Style {
    match status {
        AutomationRunStatus::Completed => green_style(),
        AutomationRunStatus::Failed => error_style(),
        AutomationRunStatus::Skipped => Style::default().fg(color_yellow()).bg(color_bg()),
        AutomationRunStatus::Running => blue_style(),
    }
}

fn mcp_overview_label(mcp: &McpView) -> String {
    if mcp.servers.is_empty() {
        mcp.status.clone()
    } else {
        format!("{} ({} servers)", mcp.status, mcp.servers.len())
    }
}

#[derive(Clone)]
struct ChannelRow {
    name: &'static str,
    enabled: bool,
    details: Vec<String>,
}

fn channel_rows(config: &ChannelRuntimeConfig) -> Vec<ChannelRow> {
    vec![
        ChannelRow {
            name: "stdio",
            enabled: config.stdio.enabled,
            details: vec![format!("auth: {}", config.stdio.auth)],
        },
        ChannelRow {
            name: "websocket",
            enabled: config.websocket.enabled,
            details: vec![
                format!("bind: {}", config.websocket.bind_addr),
                format!("auth: {}", config.websocket.auth),
            ],
        },
        ChannelRow {
            name: "weixin",
            enabled: config.weixin.enabled,
            details: vec![
                format!("workspace: {}", config.weixin.workspace_dir),
                format!("markdown_filter: {}", config.weixin.markdown_filter),
                format!("media_input: {}", config.weixin.media_input),
                format!("media_output: {}", config.weixin.media_output),
                format!(
                    "override_model: {}",
                    config.weixin.override_model.as_deref().unwrap_or("-")
                ),
                format!(
                    "override_thinking: {}",
                    config
                        .weixin
                        .override_reasoning_mode
                        .map(|mode| mode.as_str())
                        .unwrap_or("-")
                ),
            ],
        },
        ChannelRow {
            name: "feishu",
            enabled: config.feishu.enabled,
            details: vec![
                format!("workspace: {}", config.feishu.workspace_dir),
                format!("domain: {}", feishu_domain_label(config.feishu.domain)),
                format!(
                    "dm_policy: {}",
                    feishu_policy_label(config.feishu.dm_policy)
                ),
                format!(
                    "group_policy: {}",
                    feishu_policy_label(config.feishu.group_policy)
                ),
                format!("allow_from: {}", config.feishu.allow_from.len()),
                format!("group_allow_from: {}", config.feishu.group_allow_from.len()),
                format!(
                    "group_require_mention: {}",
                    config.feishu.group_require_mention
                ),
                format!("media_input: {}", config.feishu.media_input),
                format!("media_output: {}", config.feishu.media_output),
                format!("card_output: {}", config.feishu.card_output),
            ],
        },
    ]
}

fn session_tool_names(meta: &SessionMetaPayload) -> Vec<String> {
    let mut names = Vec::new();
    if meta.runtime_tools.file_edit_enabled() {
        push_unique(&mut names, "file_edit".to_string());
    }
    if meta.runtime_tools.terminal_enabled() {
        push_unique(&mut names, "terminal".to_string());
    }
    if meta.runtime_tools.subagent_enabled() {
        push_unique(&mut names, "subagent".to_string());
    }
    for schema in &meta.tool_schemas {
        if let Some(name) = schema_tool_name(schema) {
            push_unique(&mut names, name);
        }
    }
    names
}

fn schema_tool_name(schema: &Value) -> Option<String> {
    schema
        .get("function")
        .and_then(|function| function.get("name"))
        .and_then(Value::as_str)
        .or_else(|| schema.get("name").and_then(Value::as_str))
        .map(str::to_string)
}

fn push_unique(values: &mut Vec<String>, value: String) {
    if !values.iter().any(|existing| existing == &value) {
        values.push(value);
    }
}

fn resolve_config_path(base_dir: &Path, configured: &str) -> PathBuf {
    let path = PathBuf::from(configured);
    let joined = if path.is_absolute() {
        path
    } else {
        base_dir.join(path)
    };
    std::fs::canonicalize(&joined).unwrap_or(joined)
}

fn updated_at(meta: &SessionMetaPayload) -> &str {
    meta.updated_at.as_deref().unwrap_or("")
}

fn display_session_title(session: &SessionView) -> String {
    session
        .meta
        .title
        .as_deref()
        .filter(|title| !title.trim().is_empty())
        .unwrap_or(&session.meta.session_id)
        .to_string()
}

fn context_percent_label(session: &SessionView) -> String {
    if session.context_window == 0 {
        return "-".to_string();
    }
    format!(
        "{:.0}%",
        (session.usage.used as f64 / session.context_window as f64) * 100.0
    )
}

fn assistant_message_count(messages: &[Value]) -> usize {
    messages
        .iter()
        .filter(|message| message.get("role").and_then(Value::as_str) == Some("assistant"))
        .count()
}

fn session_date(session: &SessionView) -> Option<NaiveDate> {
    session
        .meta
        .updated_at
        .as_deref()
        .and_then(parse_date_prefix)
        .or_else(|| session_dir_date(&session.dir))
}

fn session_dir_date(session_dir: &Path) -> Option<NaiveDate> {
    let day = session_dir.parent()?.file_name()?.to_str()?;
    let month = session_dir.parent()?.parent()?.file_name()?.to_str()?;
    let year = session_dir
        .parent()?
        .parent()?
        .parent()?
        .file_name()?
        .to_str()?;
    NaiveDate::parse_from_str(&format!("{year}-{month}-{day}"), "%Y-%m-%d").ok()
}

fn parse_date_prefix(value: &str) -> Option<NaiveDate> {
    if value.len() < 10 {
        return None;
    }
    NaiveDate::parse_from_str(&value[..10], "%Y-%m-%d").ok()
}

fn weekday_label(row: usize) -> &'static str {
    match row {
        0 => "Mon",
        1 => "Tue",
        2 => "Wed",
        3 => "Thu",
        4 => "Fri",
        5 => "Sat",
        _ => "Sun",
    }
}

fn month_label(month: u32) -> &'static str {
    match month {
        1 => "Jan",
        2 => "Feb",
        3 => "Mar",
        4 => "Apr",
        5 => "May",
        6 => "Jun",
        7 => "Jul",
        8 => "Aug",
        9 => "Sep",
        10 => "Oct",
        11 => "Nov",
        _ => "Dec",
    }
}

fn format_token_count(value: u64) -> String {
    if value >= 1_000_000 {
        format!("{:.1}m", value as f64 / 1_000_000.0)
    } else if value >= 1_000 {
        format!("{:.1}k", value as f64 / 1_000.0)
    } else {
        value.to_string()
    }
}

fn schedule_label(schedule: &AutomationSchedule) -> String {
    match schedule {
        AutomationSchedule::Interval { every_seconds } => format!("interval {every_seconds}s"),
        AutomationSchedule::Daily { at } => format!("daily {at}"),
    }
}

fn session_mode_label(session: &AutomationSessionConfig) -> String {
    match session {
        AutomationSessionConfig::New => "new".to_string(),
        AutomationSessionConfig::Fixed { session_id } => format!("fixed {session_id}"),
        AutomationSessionConfig::Sticky => "sticky".to_string(),
    }
}

fn run_status(status: AutomationRunStatus) -> &'static str {
    match status {
        AutomationRunStatus::Running => "running",
        AutomationRunStatus::Completed => "completed",
        AutomationRunStatus::Failed => "failed",
        AutomationRunStatus::Skipped => "skipped",
    }
}

fn feishu_domain_label(domain: FeishuChannelDomain) -> &'static str {
    match domain {
        FeishuChannelDomain::Feishu => "feishu",
        FeishuChannelDomain::Lark => "lark",
    }
}

fn feishu_policy_label(policy: FeishuAccessPolicy) -> &'static str {
    match policy {
        FeishuAccessPolicy::AllowAll => "allow_all",
        FeishuAccessPolicy::WhiteList => "white_list",
    }
}

fn selected<T>(values: &[T], index: usize) -> &T {
    &values[index.min(values.len() - 1)]
}

fn display_timestamp(value: &str) -> String {
    value
        .replace('T', " ")
        .replace("+00:00", "Z")
        .chars()
        .take(19)
        .collect()
}

fn short_id(value: &str) -> String {
    value.chars().take(8).collect()
}

fn display_file_name(path: &Path) -> String {
    path.file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("-")
        .to_string()
}

fn summarize_json_value(value: &Value) -> String {
    if let Some(update) = value.get("update") {
        if let Some(method) = update.get("method").and_then(Value::as_str) {
            return method.to_string();
        }
        if let Some(kind) = update.get("type").and_then(Value::as_str) {
            return kind.to_string();
        }
    }
    "transcript event".to_string()
}

fn join_limited(values: &[String], max_chars: usize) -> String {
    if values.is_empty() {
        return "-".to_string();
    }
    let mut out = String::new();
    for value in values {
        let next = if out.is_empty() {
            value.clone()
        } else {
            format!(", {value}")
        };
        if out.chars().count() + next.chars().count() > max_chars {
            out.push_str(", ...");
            break;
        }
        out.push_str(&next);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn model_call_days_collapse_stream_chunks() {
        let text = [
            transcript_line("2026-06-13T10:00:00Z", "user_message_chunk", json!({})),
            transcript_line("2026-06-13T10:00:01Z", "agent_thought_chunk", json!({})),
            transcript_line("2026-06-13T10:00:02Z", "agent_thought_chunk", json!({})),
            transcript_line("2026-06-13T10:00:03Z", "tool_call", json!({})),
            transcript_line("2026-06-13T10:00:04Z", "tool_call_update", json!({})),
            transcript_line("2026-06-13T10:00:05Z", "agent_message_chunk", json!({})),
        ]
        .join("\n");

        let days = model_call_days_from_transcript(&text);
        let date = NaiveDate::from_ymd_opt(2026, 6, 13).unwrap();
        assert_eq!(days.get(&date), Some(&2));
    }

    #[test]
    fn token_days_use_usage_deltas() {
        let text = [
            transcript_line(
                "2026-06-13T10:00:00Z",
                "usage_update",
                json!({"used": 100, "size": 1000}),
            ),
            transcript_line(
                "2026-06-13T10:10:00Z",
                "usage_update",
                json!({"used": 160, "size": 1000}),
            ),
            transcript_line(
                "2026-06-14T09:00:00Z",
                "usage_update",
                json!({"used": 40, "size": 1000}),
            ),
        ]
        .join("\n");

        let days = token_days_from_transcript(&text);
        let first = NaiveDate::from_ymd_opt(2026, 6, 13).unwrap();
        let second = NaiveDate::from_ymd_opt(2026, 6, 14).unwrap();
        assert_eq!(days.get(&first), Some(&160));
        assert_eq!(days.get(&second), Some(&40));
    }

    #[test]
    fn load_mcp_lists_servers_from_config() {
        let temp_dir = tempfile::tempdir().unwrap();
        let resources_dir = temp_dir.path().join("resources");
        std::fs::create_dir_all(&resources_dir).unwrap();
        std::fs::write(
            resources_dir.join("mcp.json"),
            serde_json::to_string(&json!({
                "mcpServers": {
                    "local": {
                        "command": "node",
                        "args": ["server.js"],
                        "env": {
                            "TOKEN": "secret-value"
                        }
                    },
                    "remote": {
                        "url": "https://example.test/mcp",
                        "headers": {
                            "Authorization": "Bearer secret"
                        }
                    }
                }
            }))
            .unwrap(),
        )
        .unwrap();

        let mcp = load_mcp(temp_dir.path());

        assert_eq!(mcp.status, "available");
        assert_eq!(mcp.servers.len(), 2);
        assert_eq!(mcp.servers[0].name, "local");
        assert_eq!(mcp.servers[0].transport, "stdio");
        assert_eq!(mcp.servers[0].target, "node");
        assert!(
            mcp.servers[0]
                .details
                .contains(&"env keys: TOKEN".to_string())
        );
        assert!(
            !mcp.servers[0]
                .details
                .iter()
                .any(|line| line.contains("secret-value"))
        );
        assert_eq!(mcp.servers[1].name, "remote");
        assert_eq!(mcp.servers[1].transport, "http");
        assert_eq!(mcp.servers[1].target, "https://example.test/mcp");
        assert!(
            mcp.servers[1]
                .details
                .contains(&"header keys: Authorization".to_string())
        );
    }

    #[test]
    fn load_mcp_reports_missing_config() {
        let temp_dir = tempfile::tempdir().unwrap();

        let mcp = load_mcp(temp_dir.path());

        assert_eq!(mcp.status, "missing");
        assert!(mcp.servers.is_empty());
        assert!(mcp.error.is_none());
    }

    fn transcript_line(updated_at: &str, update_type: &str, mut update: Value) -> String {
        let update_obj = update.as_object_mut().unwrap();
        update_obj.insert(
            "session_update".to_string(),
            Value::String(update_type.to_string()),
        );
        serde_json::to_string(&json!({
            "updated_at": updated_at,
            "update": update,
        }))
        .unwrap()
    }
}
