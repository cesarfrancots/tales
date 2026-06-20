//! Warp-style terminal workspace for Tales.
//!
//! This is the default no-argument surface: an in-process Tales orchestrator
//! pane plus sibling terminal panes for shells and agent CLIs. The pane/session
//! model is intentionally isolated here so a later `portable-pty` + full VTE
//! grid can replace the native PTY/line renderer without touching the planner.

use std::collections::VecDeque;
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use crossterm::event::{Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use futures::StreamExt;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block as RBlock, Borders, List, ListItem, Paragraph, Wrap};
use ratatui::{Frame, Terminal};
use serde_json::json;
use tales_core::agent::{
    bin_path, project_mcp_config_risks, tool_info, validate_roster, McpConfigRisk, KNOWN_TOOLS,
};
use tales_core::bus::EventBus;
use tales_core::conductor::Role;
use tales_core::event::{OrchestratorEvent, UserCommand};
use tokio::sync::broadcast::error::TryRecvError;
use tokio::sync::mpsc;
use tokio::time::{self, Duration};

use crate::app::{commands_message, help_message, App, RecoveryCommand, SubmitAction};
use crate::theme::{color_for, pretty, ACCENT, DIM, ERRC, FAINT, TEXT};
use crate::{run_session, Args, Connection};

const MAX_SCROLLBACK: usize = 2_000;
const PLAN_DIR: &str = ".tales";
const LAST_PLAN_FILE: &str = "last-plan.md";
const RUNS_DIR: &str = "runs";

type PaneId = u64;
type ChildInput = Arc<Mutex<Box<dyn Write + Send>>>;

pub(crate) async fn run_terminal_workspace(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    keys: &mut EventStream,
    args: &Args,
) -> Result<(), Box<dyn std::error::Error>> {
    let initial_cwd = args
        .cwd
        .clone()
        .map(PathBuf::from)
        .unwrap_or(std::env::current_dir()?);
    let (cwd, sandbox) = if args.cwd.is_none() {
        let Some(selection) =
            run_workspace_onboarding(terminal, keys, initial_cwd, &args.sandbox).await?
        else {
            return Ok(());
        };
        (selection.cwd, selection.sandbox)
    } else {
        (initial_cwd, args.sandbox.clone())
    };
    let cfg = WorkspaceConfig {
        connect: args.connect.clone(),
        prefill: args.prefill.clone(),
        cwd,
        sandbox,
        turns: args.turns,
    };
    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut workspace = Workspace::new(cfg, tx);

    loop {
        workspace.poll_tales_events();
        workspace.poll_children();
        workspace.tick_animations();
        terminal.draw(|f| workspace.draw(f))?;
        if workspace.should_quit {
            break;
        }

        // ~30fps while the Tales pane is animating its reveal/spinner, idle
        // slower otherwise. Keys and PTY output still wake the loop instantly.
        let frame = if workspace.is_animating() {
            Duration::from_millis(33)
        } else {
            Duration::from_millis(80)
        };

        tokio::select! {
            maybe_key = keys.next() => {
                if maybe_key.is_none() {
                    break;
                }
                if let Some(Ok(Event::Key(key))) = maybe_key {
                    workspace.handle_key(key).await;
                }
            }
            maybe_ev = rx.recv() => {
                if let Some(ev) = maybe_ev {
                    workspace.apply_terminal_event(ev);
                }
            }
            _ = time::sleep(frame) => {}
        }
    }

    workspace.shutdown();
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct WorkspaceSelection {
    cwd: PathBuf,
    sandbox: String,
}

async fn run_workspace_onboarding(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    keys: &mut EventStream,
    initial_cwd: PathBuf,
    default_sandbox: &str,
) -> Result<Option<WorkspaceSelection>, Box<dyn std::error::Error>> {
    let mut screen = WorkspaceOnboardingScreen::new(initial_cwd, default_sandbox)?;
    loop {
        terminal.draw(|f| screen.draw(f))?;
        match keys.next().await {
            Some(Ok(Event::Key(key))) => {
                if let Some(outcome) = screen.handle_key(key)? {
                    return match outcome {
                        OnboardingOutcome::Selected(selection) => Ok(Some(selection)),
                        OnboardingOutcome::Quit => Ok(None),
                    };
                }
            }
            Some(Ok(_)) => {}
            Some(Err(e)) => return Err(e.into()),
            None => return Ok(None),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct FolderEntry {
    label: String,
    path: PathBuf,
    kind: FolderEntryKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FolderEntryKind {
    Current,
    Parent,
    Directory,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OnboardingPhase {
    Browse,
    PathInput,
    Permissions,
}

enum OnboardingOutcome {
    Selected(WorkspaceSelection),
    Quit,
}

struct PermissionOption {
    label: &'static str,
    sandbox: &'static str,
    detail: &'static str,
}

const PERMISSION_OPTIONS: &[PermissionOption] = &[
    PermissionOption {
        label: "Workspace write",
        sandbox: "workspace-write",
        detail: "Allow Tales and supported agent CLIs to read/edit files in this workspace and save .tales artifacts.",
    },
    PermissionOption {
        label: "Read only",
        sandbox: "read-only",
        detail: "Allow planning and inspection only where supported. Some executor actions may be blocked.",
    },
    PermissionOption {
        label: "Full access",
        sandbox: "danger-full-access",
        detail: "Run agent CLIs without workspace sandbox restrictions. Use only for trusted repositories.",
    },
];

struct WorkspaceOnboardingScreen {
    phase: OnboardingPhase,
    current: PathBuf,
    entries: Vec<FolderEntry>,
    selected: usize,
    path_input: String,
    permission_selected: usize,
    notice: String,
    mcp_risks: Vec<McpConfigRisk>,
}

impl WorkspaceOnboardingScreen {
    fn new(initial_cwd: PathBuf, default_sandbox: &str) -> io::Result<Self> {
        let current = normalize_workspace_dir(initial_cwd)?;
        let entries = load_folder_entries(&current)?;
        Ok(Self {
            phase: OnboardingPhase::Browse,
            current,
            entries,
            selected: 0,
            path_input: String::new(),
            permission_selected: permission_index_for(default_sandbox),
            notice: "Select the folder Tales should use as its workspace".to_string(),
            mcp_risks: Vec::new(),
        })
    }

    fn handle_key(&mut self, key: KeyEvent) -> io::Result<Option<OnboardingOutcome>> {
        if key.kind == KeyEventKind::Release {
            return Ok(None);
        }
        if matches!((key.code, key.modifiers), (KeyCode::Char('q'), modifiers) if modifiers.contains(KeyModifiers::CONTROL))
        {
            return Ok(Some(OnboardingOutcome::Quit));
        }
        match self.phase {
            OnboardingPhase::Browse => self.handle_browser_key(key),
            OnboardingPhase::PathInput => self.handle_path_key(key),
            OnboardingPhase::Permissions => self.handle_permission_key(key),
        }
    }

    fn handle_browser_key(&mut self, key: KeyEvent) -> io::Result<Option<OnboardingOutcome>> {
        match key.code {
            KeyCode::Char('q') => return Ok(Some(OnboardingOutcome::Quit)),
            KeyCode::Up | KeyCode::Char('k') => self.move_selection(-1),
            KeyCode::Down | KeyCode::Char('j') => self.move_selection(1),
            KeyCode::PageUp => self.move_selection(-8),
            KeyCode::PageDown => self.move_selection(8),
            KeyCode::Home => self.selected = 0,
            KeyCode::End => self.selected = self.entries.len().saturating_sub(1),
            KeyCode::Backspace | KeyCode::Char('h') => self.enter_parent()?,
            KeyCode::Char('~') => {
                if let Some(home) = std::env::var_os("HOME") {
                    self.change_dir(PathBuf::from(home))?;
                }
            }
            KeyCode::Char('p') | KeyCode::Char(':') => self.open_path_input(""),
            KeyCode::Char('/') => self.open_path_input("/"),
            KeyCode::Char('c') | KeyCode::Char(' ') => self.confirm_current(),
            KeyCode::Enter => self.activate_selected()?,
            _ => {}
        }
        Ok(None)
    }

    fn handle_path_key(&mut self, key: KeyEvent) -> io::Result<Option<OnboardingOutcome>> {
        match key.code {
            KeyCode::Esc => {
                self.phase = OnboardingPhase::Browse;
                self.path_input.clear();
                self.notice = "Path entry cancelled".to_string();
            }
            KeyCode::Enter => self.confirm_path_input()?,
            KeyCode::Backspace => {
                self.path_input.pop();
            }
            KeyCode::Char(c) => self.path_input.push(c),
            _ => {}
        }
        Ok(None)
    }

    fn handle_permission_key(&mut self, key: KeyEvent) -> io::Result<Option<OnboardingOutcome>> {
        match key.code {
            KeyCode::Char('q') => return Ok(Some(OnboardingOutcome::Quit)),
            KeyCode::Up | KeyCode::Char('k') => {
                self.permission_selected = self.permission_selected.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.permission_selected =
                    (self.permission_selected + 1).min(PERMISSION_OPTIONS.len() - 1);
            }
            KeyCode::Esc | KeyCode::Char('b') => {
                self.phase = OnboardingPhase::Browse;
                self.notice = "Choose another workspace or press c to continue".to_string();
            }
            KeyCode::Enter => {
                let option = &PERMISSION_OPTIONS[self.permission_selected];
                return Ok(Some(OnboardingOutcome::Selected(WorkspaceSelection {
                    cwd: self.current.clone(),
                    sandbox: option.sandbox.to_string(),
                })));
            }
            _ => {}
        }
        Ok(None)
    }

    fn move_selection(&mut self, delta: isize) {
        if self.entries.is_empty() {
            self.selected = 0;
            return;
        }
        let last = self.entries.len() - 1;
        self.selected = if delta.is_negative() {
            self.selected.saturating_sub(delta.unsigned_abs())
        } else {
            (self.selected + delta as usize).min(last)
        };
    }

    fn activate_selected(&mut self) -> io::Result<()> {
        let Some(entry) = self.entries.get(self.selected) else {
            self.confirm_current();
            return Ok(());
        };
        match entry.kind {
            FolderEntryKind::Current => self.confirm_current(),
            FolderEntryKind::Parent | FolderEntryKind::Directory => {
                self.change_dir(entry.path.clone())?
            }
        }
        Ok(())
    }

    fn enter_parent(&mut self) -> io::Result<()> {
        if let Some(parent) = self.current.parent() {
            self.change_dir(parent.to_path_buf())?;
        }
        Ok(())
    }

    fn change_dir(&mut self, path: PathBuf) -> io::Result<()> {
        match normalize_workspace_dir(path).and_then(|dir| {
            let entries = load_folder_entries(&dir)?;
            Ok((dir, entries))
        }) {
            Ok((dir, entries)) => {
                self.current = dir;
                self.entries = entries;
                self.selected = 0;
                self.notice =
                    "Press Enter on a folder to browse it, or c to use the current folder"
                        .to_string();
            }
            Err(e) => {
                self.notice = format!("Could not open folder: {e}");
            }
        }
        Ok(())
    }

    fn open_path_input(&mut self, seed: &str) {
        self.phase = OnboardingPhase::PathInput;
        self.path_input = seed.to_string();
        self.notice =
            "Type an absolute, home-relative, or workspace-relative path, then press Enter"
                .to_string();
    }

    fn confirm_path_input(&mut self) -> io::Result<()> {
        match resolve_cd_path(&self.current, &self.path_input)
            .and_then(normalize_workspace_dir)
            .and_then(|dir| {
                let entries = load_folder_entries(&dir)?;
                Ok((dir, entries))
            }) {
            Ok((dir, entries)) => {
                self.current = dir;
                self.entries = entries;
                self.selected = 0;
                self.path_input.clear();
                self.confirm_current();
            }
            Err(e) => {
                self.notice = format!("Could not cd to path: {e}");
            }
        }
        Ok(())
    }

    fn confirm_current(&mut self) {
        self.mcp_risks = project_mcp_config_risks(&self.current);
        self.phase = OnboardingPhase::Permissions;
        self.notice = "Approve workspace permissions to start Tales".to_string();
    }

    fn draw(&self, f: &mut Frame) {
        match self.phase {
            OnboardingPhase::Browse => self.draw_browser(f),
            OnboardingPhase::PathInput => self.draw_path_input(f),
            OnboardingPhase::Permissions => self.draw_permissions(f),
        }
    }

    fn draw_browser(&self, f: &mut Frame) {
        let area = f.area();
        let chunks = Layout::vertical([
            Constraint::Length(1),
            Constraint::Min(8),
            Constraint::Length(2),
        ])
        .split(area);
        f.render_widget(onboarding_header("workspace"), chunks[0]);

        let block = RBlock::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(FAINT))
            .title("Select workspace folder");
        let inner = block.inner(chunks[1]);
        f.render_widget(block, chunks[1]);

        let visible = visible_entry_range(
            self.entries.len(),
            self.selected,
            inner.height.saturating_sub(5) as usize,
        );
        let mut lines = vec![
            Line::from(Span::styled(
                "Tales will run planners, shells, agent CLIs, and .tales artifacts from this folder.",
                Style::default().fg(TEXT),
            )),
            Line::from(Span::styled(
                format!("Current: {}", self.current.display()),
                Style::default().fg(DIM),
            )),
            Line::from(""),
        ];
        for idx in visible {
            if let Some(entry) = self.entries.get(idx) {
                let selected = idx == self.selected;
                let marker = if selected { ">" } else { " " };
                let suffix = match entry.kind {
                    FolderEntryKind::Current => " use this folder",
                    FolderEntryKind::Parent => " parent",
                    FolderEntryKind::Directory => "",
                };
                let style = if selected {
                    Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(TEXT)
                };
                lines.push(Line::from(vec![
                    Span::styled(marker, Style::default().fg(ACCENT)),
                    Span::raw(" "),
                    Span::styled(entry.label.clone(), style),
                    Span::styled(suffix, Style::default().fg(DIM)),
                ]));
            }
        }

        f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
        f.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(
                    "Enter open/select · c use current · p or : cd path · / absolute path · Backspace parent · q quit",
                    Style::default().fg(FAINT),
                ),
                Span::styled(format!(" · {}", self.notice), Style::default().fg(DIM)),
            ])),
            chunks[2],
        );
    }

    fn draw_path_input(&self, f: &mut Frame) {
        let area = f.area();
        let chunks = Layout::vertical([
            Constraint::Length(1),
            Constraint::Min(8),
            Constraint::Length(2),
        ])
        .split(area);
        f.render_widget(onboarding_header("cd path"), chunks[0]);

        let block = RBlock::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(FAINT))
            .title("Change workspace by path");
        let inner = block.inner(chunks[1]);
        f.render_widget(block, chunks[1]);

        let lines = vec![
            Line::from(Span::styled(
                "Technical path entry",
                Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
            )),
            Line::from(Span::styled(
                "Use this when you already know the workspace path.",
                Style::default().fg(DIM),
            )),
            Line::from(""),
            Line::from(Span::styled(
                format!("Current: {}", self.current.display()),
                Style::default().fg(DIM),
            )),
            Line::from(vec![
                Span::styled("cd ", Style::default().fg(ACCENT)),
                Span::styled(self.path_input.clone(), Style::default().fg(TEXT)),
            ]),
            Line::from(""),
            Line::from(Span::styled(
                "Examples: /Users/cesar/Documents/Tales · ~/Documents/Tales · ../OtherRepo · cd ~/repo",
                Style::default().fg(FAINT),
            )),
        ];
        f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
        f.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(
                    "Enter cd and continue to permissions · Esc cancel · Ctrl-Q quit",
                    Style::default().fg(FAINT),
                ),
                Span::styled(format!(" · {}", self.notice), Style::default().fg(DIM)),
            ])),
            chunks[2],
        );
    }

    fn draw_permissions(&self, f: &mut Frame) {
        let area = f.area();
        let chunks = Layout::vertical([
            Constraint::Length(1),
            Constraint::Min(8),
            Constraint::Length(2),
        ])
        .split(area);
        f.render_widget(onboarding_header("permissions"), chunks[0]);

        let block = RBlock::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(FAINT))
            .title("Approve workspace permissions");
        let inner = block.inner(chunks[1]);
        f.render_widget(block, chunks[1]);

        let mut lines = vec![
            Line::from(Span::styled(
                format!("Workspace: {}", self.current.display()),
                Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
            )),
            Line::from(Span::styled(
                "Tales needs permission to write .tales artifacts and to launch selected CLIs from this folder.",
                Style::default().fg(DIM),
            )),
            Line::from(""),
        ];

        for (idx, option) in PERMISSION_OPTIONS.iter().enumerate() {
            let selected = idx == self.permission_selected;
            let label_style = if selected {
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
            } else if option.sandbox == "danger-full-access" {
                Style::default().fg(ERRC)
            } else {
                Style::default().fg(TEXT)
            };
            lines.push(Line::from(vec![
                Span::styled(
                    if selected { ">" } else { " " },
                    Style::default().fg(ACCENT),
                ),
                Span::raw(" "),
                Span::styled(option.label, label_style),
                Span::styled(format!("  ({})", option.sandbox), Style::default().fg(DIM)),
            ]));
            lines.push(Line::from(Span::styled(
                format!("    {}", option.detail),
                Style::default().fg(FAINT),
            )));
        }

        if !self.mcp_risks.is_empty() {
            lines.extend([
                Line::from(""),
                Line::from(Span::styled(
                    "Safety notice",
                    Style::default().fg(ERRC).add_modifier(Modifier::BOLD),
                )),
                Line::from(Span::styled(
                    format!(
                        "Project-local MCP/tool config detected: {}",
                        risk_paths_summary(&self.mcp_risks)
                    ),
                    Style::default().fg(DIM),
                )),
            ]);
        }

        f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
        f.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(
                    "Enter approve · b back to folder browser · q quit",
                    Style::default().fg(FAINT),
                ),
                Span::styled(format!(" · {}", self.notice), Style::default().fg(DIM)),
            ])),
            chunks[2],
        );
    }
}

fn onboarding_header(step: &str) -> Paragraph<'static> {
    Paragraph::new(Line::from(vec![
        Span::styled(
            "❯",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            " tales ",
            Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
        ),
        Span::styled("setup ", Style::default().fg(DIM)),
        Span::styled(format!("· {step}"), Style::default().fg(FAINT)),
    ]))
}

fn normalize_workspace_dir(path: PathBuf) -> io::Result<PathBuf> {
    let path = if path.as_os_str().is_empty() {
        std::env::current_dir()?
    } else {
        path
    };
    let canonical = fs::canonicalize(&path)?;
    if canonical.is_dir() {
        Ok(canonical)
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{} is not a directory", path.display()),
        ))
    }
}

fn resolve_cd_path(current: &Path, input: &str) -> io::Result<PathBuf> {
    let mut text = input.trim();
    if let Some(rest) = text.strip_prefix("cd ") {
        text = rest.trim_start();
    }
    if text.is_empty() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "path is empty"));
    }
    if text == "~" {
        return std::env::var_os("HOME")
            .map(PathBuf::from)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "HOME is not set"));
    }
    if let Some(rest) = text.strip_prefix("~/") {
        return std::env::var_os("HOME")
            .map(|home| PathBuf::from(home).join(rest))
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "HOME is not set"));
    }
    let path = PathBuf::from(text);
    if path.is_absolute() {
        Ok(path)
    } else {
        Ok(current.join(path))
    }
}

fn load_folder_entries(current: &Path) -> io::Result<Vec<FolderEntry>> {
    let mut entries = vec![FolderEntry {
        label: ".".to_string(),
        path: current.to_path_buf(),
        kind: FolderEntryKind::Current,
    }];
    if let Some(parent) = current.parent() {
        entries.push(FolderEntry {
            label: "..".to_string(),
            path: parent.to_path_buf(),
            kind: FolderEntryKind::Parent,
        });
    }

    let mut dirs = Vec::new();
    for entry in fs::read_dir(current)? {
        let entry = entry?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let Some(name) = path
            .file_name()
            .and_then(|name| name.to_str())
            .map(str::to_string)
        else {
            continue;
        };
        dirs.push(FolderEntry {
            label: name,
            path,
            kind: FolderEntryKind::Directory,
        });
    }
    dirs.sort_by_key(|entry| entry.label.to_ascii_lowercase());
    entries.extend(dirs);
    Ok(entries)
}

fn permission_index_for(sandbox: &str) -> usize {
    PERMISSION_OPTIONS
        .iter()
        .position(|option| option.sandbox == sandbox)
        .unwrap_or(0)
}

fn visible_entry_range(total: usize, selected: usize, height: usize) -> std::ops::Range<usize> {
    if total == 0 {
        return 0..0;
    }
    let height = height.max(1).min(total);
    let start = selected
        .saturating_sub(height / 2)
        .min(total.saturating_sub(height));
    start..(start + height)
}

#[derive(Clone)]
struct WorkspaceConfig {
    connect: Vec<String>,
    prefill: Option<String>,
    cwd: PathBuf,
    sandbox: String,
    turns: usize,
}

struct Workspace {
    cfg: WorkspaceConfig,
    panes: Vec<Pane>,
    active: usize,
    next_id: PaneId,
    tx: mpsc::UnboundedSender<TerminalEvent>,
    should_quit: bool,
    notice: String,
}

impl Workspace {
    fn new(cfg: WorkspaceConfig, tx: mpsc::UnboundedSender<TerminalEvent>) -> Self {
        let mut tales = TalesPane::new(1, cfg.clone());
        if let Some(prefill) = &cfg.prefill {
            tales.app.input = prefill.clone();
        }
        Self {
            cfg,
            panes: vec![Pane::Tales(tales)],
            active: 0,
            next_id: 2,
            tx,
            should_quit: false,
            notice: "Tales terminal ready".to_string(),
        }
    }

    async fn handle_key(&mut self, key: KeyEvent) {
        if key.kind == KeyEventKind::Release {
            return;
        }

        match (key.code, key.modifiers) {
            (KeyCode::Char('q'), KeyModifiers::CONTROL) => {
                self.should_quit = true;
                return;
            }
            (KeyCode::Tab, _) => {
                self.next_pane();
                return;
            }
            (KeyCode::BackTab, _) => {
                self.prev_pane();
                return;
            }
            (KeyCode::Char('t'), KeyModifiers::CONTROL) => {
                self.active = 0;
                self.notice = "focused Tales orchestrator".to_string();
                return;
            }
            (KeyCode::Char('n'), KeyModifiers::CONTROL) => {
                self.spawn_shell();
                return;
            }
            (KeyCode::Char('x'), KeyModifiers::CONTROL) => {
                let _ = self.spawn_agent("codex");
                return;
            }
            (KeyCode::Char('l'), KeyModifiers::CONTROL) => {
                let _ = self.spawn_agent("claude");
                return;
            }
            (KeyCode::Char('o'), KeyModifiers::CONTROL) => {
                let _ = self.spawn_agent("opencode");
                return;
            }
            (KeyCode::Char('s'), KeyModifiers::CONTROL) => {
                self.send_handoff_to_active();
                return;
            }
            (KeyCode::Char('a'), KeyModifiers::CONTROL) => {
                self.approve_active();
                return;
            }
            _ => {}
        }

        let tales_action = match self.panes.get_mut(self.active) {
            Some(Pane::Tales(tales)) => {
                let action = tales.handle_key(key).await;
                self.notice = tales.notice.clone();
                action
            }
            Some(Pane::Process(proc)) => {
                if let Some(bytes) = key_to_bytes(key) {
                    proc.write(&bytes);
                }
                None
            }
            None => None,
        };

        if let Some(action) = tales_action {
            self.apply_tales_action(action);
        }
    }

    fn next_pane(&mut self) {
        if !self.panes.is_empty() {
            self.active = (self.active + 1) % self.panes.len();
        }
    }

    fn prev_pane(&mut self) {
        if self.panes.is_empty() {
            return;
        }
        self.active = if self.active == 0 {
            self.panes.len() - 1
        } else {
            self.active - 1
        };
    }

    fn spawn_shell(&mut self) {
        let id = self.alloc_id();
        let shell = default_shell();
        let title = Path::new(&shell)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("shell")
            .to_string();
        let pane = ProcessPane::spawn(
            id,
            SessionKind::Shell,
            title,
            shell,
            Vec::new(),
            self.cfg.cwd.clone(),
            self.tx.clone(),
        );
        self.panes.push(Pane::Process(pane));
        self.active = self.panes.len() - 1;
        self.notice = "opened shell pane".to_string();
    }

    fn spawn_agent(&mut self, key: &str) -> Option<usize> {
        let id = self.alloc_id();
        let Some(info) = tool_info(key) else {
            self.notice = format!("unknown agent: {key}");
            return None;
        };
        let (args, safety_note) = agent_launch_args(info.key, &self.cfg.cwd);
        let pane = ProcessPane::spawn(
            id,
            SessionKind::Agent {
                key: info.key.to_string(),
            },
            info.pretty.to_string(),
            info.bin.to_string(),
            args,
            self.cfg.cwd.clone(),
            self.tx.clone(),
        );
        self.panes.push(Pane::Process(pane));
        self.active = self.panes.len() - 1;
        if let Some(note) = safety_note {
            if let Some(Pane::Process(proc)) = self.panes.get_mut(self.active) {
                proc.push_line(note.clone());
            }
            self.notice = note;
        } else {
            self.notice = format!("opened {} pane", info.pretty);
        }
        Some(self.active)
    }

    fn apply_tales_action(&mut self, action: TalesAction) {
        match action {
            TalesAction::LaunchExecutor {
                key,
                prompt,
                plan_path,
            } => {
                self.launch_executor_with_prompt(&key, &prompt, plan_path.as_deref());
            }
            TalesAction::Handoff {
                executor,
                prompt,
                plan_path,
            } => {
                if let Some(index) = self.reusable_agent_pane(executor.as_deref()) {
                    if let Some(Pane::Process(proc)) = self.panes.get_mut(index) {
                        proc.write(prompt.as_bytes());
                        proc.write(b"\r");
                        self.active = index;
                        self.notice = match plan_path {
                            Some(path) => {
                                format!("resent plan to {} from {}", proc.title, path.display())
                            }
                            None => format!("resent plan to {}", proc.title),
                        };
                    }
                } else if let Some(key) = executor {
                    self.launch_executor_with_prompt(&key, &prompt, plan_path.as_deref());
                } else {
                    self.notice =
                        "no live executor pane; use /switch <executor> from Tales".to_string();
                }
            }
        }
    }

    fn launch_executor_with_prompt(&mut self, key: &str, prompt: &str, plan_path: Option<&Path>) {
        let Some(index) = self.spawn_agent(key) else {
            return;
        };
        if let Some(Pane::Process(proc)) = self.panes.get_mut(index) {
            proc.reset_for_execution(plan_path);
            proc.write(prompt.as_bytes());
            proc.write(b"\r");
            self.notice = match plan_path {
                Some(path) => format!("launched {} with {}", proc.title, path.display()),
                None => format!("launched {} executor pane", proc.title),
            };
        }
    }

    fn reusable_agent_pane(&self, key: Option<&str>) -> Option<usize> {
        self.panes
            .iter()
            .enumerate()
            .rev()
            .find_map(|(index, pane)| {
                let Pane::Process(proc) = pane else {
                    return None;
                };
                if !proc.can_receive_input() {
                    return None;
                }
                match &proc.kind {
                    SessionKind::Agent { key: pane_key }
                        if key.is_none_or(|wanted| wanted.eq_ignore_ascii_case(pane_key)) =>
                    {
                        Some(index)
                    }
                    _ => None,
                }
            })
    }

    fn send_handoff_to_active(&mut self) {
        let prompt = self.tales().map(TalesPane::executor_prompt);
        let Some(prompt) = prompt else {
            self.notice = "no Tales pane found".to_string();
            return;
        };
        match self.panes.get_mut(self.active) {
            Some(Pane::Process(proc)) => {
                proc.write(prompt.as_bytes());
                proc.write(b"\r");
                self.notice = format!("sent plan to {}", proc.title);
            }
            Some(Pane::Tales(_)) => {
                self.notice = "focus an agent pane before sending the plan".to_string();
            }
            None => {}
        }
    }

    fn approve_active(&mut self) {
        match self.panes.get_mut(self.active) {
            Some(Pane::Process(proc)) if proc.state == PaneState::AwaitingApproval => {
                proc.write(b"y\r");
                proc.state = PaneState::Running;
                self.notice = format!("approved prompt in {}", proc.title);
            }
            Some(Pane::Process(proc)) => {
                self.notice = format!("{} is not awaiting approval", proc.title);
            }
            Some(Pane::Tales(_)) => {
                self.notice = "approval applies to a focused agent pane".to_string();
            }
            None => {}
        }
    }

    fn alloc_id(&mut self) -> PaneId {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    fn tales(&self) -> Option<&TalesPane> {
        self.panes.iter().find_map(|pane| match pane {
            Pane::Tales(tales) => Some(tales),
            Pane::Process(_) => None,
        })
    }

    fn poll_tales_events(&mut self) {
        for pane in &mut self.panes {
            if let Pane::Tales(tales) = pane {
                tales.poll_events();
            }
        }
    }

    /// Advance the Tales pane's smooth-reveal + spinner animation each frame.
    fn tick_animations(&mut self) {
        for pane in &mut self.panes {
            if let Pane::Tales(tales) = pane {
                tales.app.tick();
            }
        }
    }

    /// Whether the Tales pane is still revealing/spinning, so the loop should
    /// keep refreshing at the smooth frame rate.
    fn is_animating(&self) -> bool {
        self.panes
            .iter()
            .any(|pane| matches!(pane, Pane::Tales(tales) if tales.app.is_animating()))
    }

    fn poll_children(&mut self) {
        for pane in &mut self.panes {
            if let Pane::Process(proc) = pane {
                proc.poll_exit();
            }
        }
    }

    fn apply_terminal_event(&mut self, ev: TerminalEvent) {
        match ev {
            TerminalEvent::Output { pane_id, bytes } => {
                if let Some(proc) = self.process_mut(pane_id) {
                    proc.push_output(&bytes);
                }
            }
            TerminalEvent::ReaderClosed { pane_id } => {
                if let Some(proc) = self.process_mut(pane_id) {
                    if proc.state != PaneState::Exited {
                        proc.state = PaneState::WaitingForInput;
                    }
                }
            }
        }
    }

    fn process_mut(&mut self, id: PaneId) -> Option<&mut ProcessPane> {
        self.panes.iter_mut().find_map(|pane| match pane {
            Pane::Process(proc) if proc.id == id => Some(proc),
            _ => None,
        })
    }

    fn shutdown(&mut self) {
        for pane in &mut self.panes {
            if let Pane::Process(proc) = pane {
                proc.shutdown();
            }
        }
    }

    fn draw(&self, f: &mut Frame) {
        let chunks = Layout::vertical([
            Constraint::Length(1),
            Constraint::Min(3),
            Constraint::Length(3),
            Constraint::Length(1),
        ])
        .split(f.area());

        self.draw_header(f, chunks[0]);

        if self.active_is_process() {
            self.draw_active_pane(f, chunks[1]);
        } else {
            let body = if chunks[1].width >= 112 {
                Layout::horizontal([Constraint::Min(20), Constraint::Length(30)]).split(chunks[1])
            } else {
                Layout::vertical([Constraint::Min(8), Constraint::Length(7)]).split(chunks[1])
            };
            self.draw_active_pane(f, body[0]);
            self.draw_status(f, body[1]);
        }
        self.draw_input(f, chunks[2]);
        self.draw_footer(f, chunks[3]);
    }

    fn active_is_process(&self) -> bool {
        matches!(self.panes.get(self.active), Some(Pane::Process(_)))
    }

    fn draw_header(&self, f: &mut Frame, area: Rect) {
        let title = self
            .panes
            .get(self.active)
            .map(Pane::title)
            .unwrap_or("Tales");
        f.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(
                    "❯",
                    Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    " tales ",
                    Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
                ),
                Span::styled("terminal ", Style::default().fg(DIM)),
                Span::styled(format!("· {title}"), Style::default().fg(FAINT)),
            ])),
            area,
        );
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                format!("{} panes ", self.panes.len()),
                Style::default().fg(ACCENT),
            )))
            .alignment(Alignment::Right),
            area,
        );
    }

    fn draw_active_pane(&self, f: &mut Frame, area: Rect) {
        let block = RBlock::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(FAINT))
            .title(
                self.panes
                    .get(self.active)
                    .map(Pane::title)
                    .unwrap_or("Tales"),
            );
        let inner = block.inner(area);
        f.render_widget(block, area);
        match self.panes.get(self.active) {
            Some(Pane::Tales(tales)) => tales.draw(f, inner),
            Some(Pane::Process(proc)) => proc.draw(f, inner),
            None => {}
        }
    }

    fn draw_status(&self, f: &mut Frame, area: Rect) {
        let items: Vec<ListItem> = self
            .panes
            .iter()
            .enumerate()
            .map(|(idx, pane)| {
                let active = if idx == self.active { ">" } else { " " };
                let state = pane.state();
                let color = state.color();
                ListItem::new(Line::from(vec![
                    Span::styled(active, Style::default().fg(ACCENT)),
                    Span::raw(" "),
                    Span::styled(state.badge(), Style::default().fg(color)),
                    Span::raw(" "),
                    Span::styled(pane.title().to_string(), Style::default().fg(TEXT)),
                ]))
            })
            .collect();
        let block = RBlock::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(FAINT))
            .title("sessions");
        f.render_widget(List::new(items).block(block), area);
    }

    fn draw_input(&self, f: &mut Frame, area: Rect) {
        let line = match self.panes.get(self.active) {
            Some(Pane::Tales(tales)) => Line::from(vec![
                Span::styled(
                    "you ❯ ",
                    Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
                ),
                Span::styled(tales.app.input.clone(), Style::default().fg(TEXT)),
            ]),
            Some(Pane::Process(proc)) => Line::from(vec![
                Span::styled("typing → ", Style::default().fg(ACCENT)),
                Span::styled(
                    proc.title.clone(),
                    Style::default().fg(color_for(proc.kind.label())),
                ),
                Span::styled(
                    " · this pane can ask for input directly",
                    Style::default().fg(FAINT),
                ),
            ]),
            None => Line::from(""),
        };
        f.render_widget(Paragraph::new(line).wrap(Wrap { trim: false }), area);
    }

    fn draw_footer(&self, f: &mut Frame, area: Rect) {
        let help = if self.active_is_process() {
            "Tab switch · Ctrl-T Tales for /handoff or /switch · Ctrl-A approve · Ctrl-Q quit"
        } else {
            "Tab switch · Ctrl-T Tales · Ctrl-N shell · Ctrl-X Codex · Ctrl-L Claude · Ctrl-S send plan · Ctrl-A approve · Ctrl-Q quit"
        };
        f.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(help, Style::default().fg(FAINT)),
                Span::styled(format!(" · {}", self.notice), Style::default().fg(DIM)),
            ])),
            area,
        );
    }
}

#[allow(clippy::large_enum_variant)]
enum Pane {
    Tales(TalesPane),
    Process(ProcessPane),
}

impl Pane {
    fn title(&self) -> &str {
        match self {
            Pane::Tales(tales) => &tales.title,
            Pane::Process(proc) => &proc.title,
        }
    }

    fn state(&self) -> PaneState {
        match self {
            Pane::Tales(tales) => tales.state,
            Pane::Process(proc) => proc.state,
        }
    }
}

struct TalesPane {
    title: String,
    state: PaneState,
    app: App,
    cfg: WorkspaceConfig,
    artifacts: Option<RunArtifacts>,
    mcp_risks: Vec<McpConfigRisk>,
    bus: Option<EventBus>,
    commands: Option<mpsc::Sender<UserCommand>>,
    events: Option<tokio::sync::broadcast::Receiver<OrchestratorEvent>>,
    started: bool,
    startup_page: StartupPage,
    notice: String,
    last_executor: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StartupPage {
    Welcome,
    Help,
    Commands,
}

enum TalesAction {
    LaunchExecutor {
        key: String,
        prompt: String,
        plan_path: Option<PathBuf>,
    },
    Handoff {
        executor: Option<String>,
        prompt: String,
        plan_path: Option<PathBuf>,
    },
}

#[derive(Clone)]
struct RunArtifacts {
    dir: PathBuf,
    manifest_path: PathBuf,
    events_path: PathBuf,
    plan_path: PathBuf,
    started_at: u64,
    task: String,
    workspace: PathBuf,
    roster: Vec<String>,
}

impl RunArtifacts {
    fn create(workspace: &Path, task: &str, roster: &[String]) -> io::Result<Self> {
        let started_at = unix_time();
        let run_id = format!("{started_at}-{}", slug(task));
        let dir = workspace.join(PLAN_DIR).join(RUNS_DIR).join(run_id);
        fs::create_dir_all(&dir)?;
        let artifacts = Self {
            manifest_path: dir.join("manifest.json"),
            events_path: dir.join("events.jsonl"),
            plan_path: dir.join("plan.md"),
            dir,
            started_at,
            task: task.to_string(),
            workspace: workspace.to_path_buf(),
            roster: roster.to_vec(),
        };
        artifacts.write_manifest("planning", None)?;
        artifacts.write_plan_markdown("planning", None, "")?;
        artifacts.append_manual_event("run_started", task)?;
        Ok(artifacts)
    }

    fn relative_dir(&self) -> String {
        self.dir
            .strip_prefix(&self.workspace)
            .unwrap_or(&self.dir)
            .display()
            .to_string()
    }

    fn write_manifest(&self, status: &str, executor: Option<&str>) -> io::Result<()> {
        let value = json!({
            "task": self.task,
            "workspace": self.workspace.display().to_string(),
            "run_dir": self.dir.display().to_string(),
            "started_unix": self.started_at,
            "updated_unix": unix_time(),
            "status": status,
            "executor": executor,
            "roster": self.roster,
            "plan_path": self.plan_path.display().to_string(),
            "events_path": self.events_path.display().to_string(),
        });
        fs::write(&self.manifest_path, format!("{value:#}\n"))
    }

    fn append_manual_event(&self, kind: &str, message: &str) -> io::Result<()> {
        let value = json!({
            "time_unix": unix_time(),
            "kind": kind,
            "message": message,
        });
        self.append_json(value)
    }

    fn append_event(&self, ev: &OrchestratorEvent) -> io::Result<()> {
        self.append_json(event_record(ev))
    }

    fn append_json(&self, value: serde_json::Value) -> io::Result<()> {
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.events_path)?;
        writeln!(file, "{value}")
    }

    fn write_plan_markdown(
        &self,
        status: &str,
        executor: Option<&str>,
        transcript: &str,
    ) -> io::Result<()> {
        let roster = self
            .roster
            .iter()
            .map(|key| pretty(key))
            .collect::<Vec<_>>()
            .join(" + ");
        let executor_line = executor
            .map(|key| format!("- Executor: {} (`{key}`)\n", pretty(key)))
            .unwrap_or_default();
        let transcript = if transcript.trim().is_empty() {
            "_Planning has started. No transcript has been captured yet._"
        } else {
            transcript.trim()
        };
        let text = format!(
            "# Tales run plan\n\n\
             - Status: {status}\n\
             - Task: {}\n\
             - Workspace: {}\n\
             - Roster: {roster}\n\
             - Started unix time: {}\n\
             {executor_line}\n\
             ## Current transcript and plan\n\n\
             {transcript}\n",
            self.task,
            self.workspace.display(),
            self.started_at
        );
        fs::write(&self.plan_path, text)
    }
}

impl TalesPane {
    fn new(_id: PaneId, cfg: WorkspaceConfig) -> Self {
        let mcp_risks = project_mcp_config_risks(&cfg.cwd);
        Self {
            title: "Tales orchestrator".to_string(),
            state: PaneState::WaitingForInput,
            app: App::new("new plan".to_string()),
            cfg,
            artifacts: None,
            mcp_risks,
            bus: None,
            commands: None,
            events: None,
            started: false,
            startup_page: StartupPage::Welcome,
            notice: "type help, commands, or a planning prompt".to_string(),
            last_executor: None,
        }
    }

    async fn handle_key(&mut self, key: KeyEvent) -> Option<TalesAction> {
        match (key.code, key.modifiers) {
            (KeyCode::Char('c'), KeyModifiers::CONTROL) => {
                if let Some(commands) = &self.commands {
                    let _ = commands.send(UserCommand::Shutdown).await;
                }
                self.notice = "Tales run stopped".to_string();
            }
            (KeyCode::Enter, _) => {
                if !self.started {
                    let task = self.app.input.trim().to_string();
                    if task.is_empty() {
                        self.notice = "type help, commands, or a planning prompt".to_string();
                    } else if self.handle_startup_command(&task) {
                        self.app.input.clear();
                    } else {
                        self.start(task);
                    }
                } else if let Some(action) = self.app.submit_action() {
                    if let Some(action) = self.handle_submit_action(action).await {
                        return Some(action);
                    }
                }
            }
            (KeyCode::Backspace, _) => {
                self.app.input.pop();
            }
            (KeyCode::Esc, _) => {
                self.app.input.clear();
                if !self.started {
                    self.startup_page = StartupPage::Welcome;
                    self.notice = "back to welcome".to_string();
                }
            }
            // At the gate, a bare digit picks that executor; otherwise type it.
            (KeyCode::Char(c), _) => {
                if let Some(cmd) = self.app.gate_pick(c) {
                    if let Some(action) = self.handle_submit_action(SubmitAction::Core(cmd)).await {
                        return Some(action);
                    }
                } else {
                    self.app.input.push(c);
                }
            }
            _ => {}
        }
        None
    }

    fn handle_startup_command(&mut self, text: &str) -> bool {
        match text.trim().to_ascii_lowercase().as_str() {
            "help" | "/help" => {
                self.startup_page = StartupPage::Help;
                self.notice = "showing Tales help".to_string();
                true
            }
            "commands" | "/commands" => {
                self.startup_page = StartupPage::Commands;
                self.notice = "showing Tales commands".to_string();
                true
            }
            _ => false,
        }
    }

    async fn handle_submit_action(&mut self, action: SubmitAction) -> Option<TalesAction> {
        match action {
            SubmitAction::Core(UserCommand::ConfirmExecution { executor }) if self.app.awaiting => {
                if let Some(commands) = &self.commands {
                    let _ = commands.send(UserCommand::Shutdown).await;
                }
                // Close the gate so the "ACTION NEEDED" banner disappears and a
                // second digit press can't launch another executor (we short-cut
                // the run with Shutdown, so no PhaseChanged arrives to clear it).
                self.app.awaiting = false;
                self.app.recommended = None;
                self.state = PaneState::WaitingForInput;
                let prompt = self.executor_prompt();
                self.last_executor = Some(executor.clone());
                let plan_path = match self.save_executor_plan(&executor, &prompt) {
                    Ok(path) => {
                        self.notice = format!("saved plan to {}", path.display());
                        Some(path)
                    }
                    Err(e) => {
                        self.notice = format!("could not save plan: {e}");
                        None
                    }
                };
                Some(TalesAction::LaunchExecutor {
                    key: executor,
                    prompt,
                    plan_path,
                })
            }
            SubmitAction::Core(other) => {
                if let Some(commands) = &self.commands {
                    let _ = commands.send(other).await;
                }
                None
            }
            SubmitAction::Recovery(recovery) => self.handle_recovery_command(recovery),
        }
    }

    fn handle_recovery_command(&mut self, cmd: RecoveryCommand) -> Option<TalesAction> {
        match cmd {
            RecoveryCommand::Artifacts => {
                self.show_artifacts();
                None
            }
            RecoveryCommand::Handoff { executor } => {
                let executor = executor
                    .or_else(|| self.last_executor.clone())
                    .or_else(|| self.app.recommended.clone());
                let Some(executor) = executor else {
                    self.app
                        .note("No executor has been selected yet. Use /switch <executor> after a plan exists, or wait for the executor gate.");
                    self.notice = "no executor selected for handoff".to_string();
                    return None;
                };
                let prompt = self.executor_prompt();
                self.last_executor = Some(executor.clone());
                let plan_path = match self.save_executor_plan_for(
                    &executor,
                    &prompt,
                    "handoff_ready",
                    "executor_handoff_ready",
                ) {
                    Ok(path) => {
                        self.notice = format!("handoff saved to {}", path.display());
                        Some(path)
                    }
                    Err(e) => {
                        self.notice = format!("could not save handoff: {e}");
                        None
                    }
                };
                Some(TalesAction::Handoff {
                    executor: Some(executor),
                    prompt,
                    plan_path,
                })
            }
            RecoveryCommand::Switch { executor } => {
                let prompt = self.executor_prompt();
                self.last_executor = Some(executor.clone());
                let plan_path = match self.save_executor_plan_for(
                    &executor,
                    &prompt,
                    "executor_switched",
                    "executor_switched",
                ) {
                    Ok(path) => {
                        self.notice = format!("switch plan saved to {}", path.display());
                        Some(path)
                    }
                    Err(e) => {
                        self.notice = format!("could not save switch plan: {e}");
                        None
                    }
                };
                Some(TalesAction::LaunchExecutor {
                    key: executor,
                    prompt,
                    plan_path,
                })
            }
        }
    }

    fn start(&mut self, task: String) {
        let roster_keys = if self.cfg.connect.is_empty() {
            vec!["claude".to_string(), "codex".to_string()]
        } else {
            self.cfg.connect.clone()
        };
        let keys = if roster_keys.len() == 1 {
            vec![roster_keys[0].clone(), roster_keys[0].clone()]
        } else {
            roster_keys.into_iter().take(2).collect::<Vec<_>>()
        };
        if let Err(e) = validate_roster(&keys) {
            self.notice = e.to_string();
            return;
        }
        let artifacts = match RunArtifacts::create(&self.cfg.cwd, &task, &keys) {
            Ok(artifacts) => {
                self.notice = format!("artifacts: {}", artifacts.relative_dir());
                Some(artifacts)
            }
            Err(e) => {
                self.notice = format!("planning started; artifacts unavailable: {e}");
                None
            }
        };
        let roster = keys
            .iter()
            .enumerate()
            .map(|(i, key)| Connection {
                tool: key.clone(),
                role: if i == 0 { Role::Drafter } else { Role::Critic },
                model: None,
                effort: None,
            })
            .collect::<Vec<_>>();

        let (bus, commands_rx) = EventBus::new(4096, 256);
        let events = bus.subscribe();
        let commands = bus.commands();
        let cwd = self.cfg.cwd.clone();
        let sandbox = self.cfg.sandbox.clone();
        let turns = self.cfg.turns;
        let task_for_engine = task.clone();
        let candidates = keys.clone();
        self.app = App::new(task.clone());
        self.app.set_candidates(candidates);
        self.app.input.clear();
        self.artifacts = artifacts;
        self.state = PaneState::Running;
        self.started = true;
        if self.notice.is_empty() {
            self.notice = "planning started".to_string();
        }
        if let Some(artifacts) = &self.artifacts {
            self.app.apply(OrchestratorEvent::Log {
                level: "info".to_string(),
                msg: format!(
                    "Artifacts are being saved to {}/ (plan.md, events.jsonl, manifest.json)",
                    artifacts.relative_dir()
                ),
            });
        }
        if !self.mcp_risks.is_empty() {
            let warning = mcp_warning_text(&self.mcp_risks);
            self.app.apply(OrchestratorEvent::Log {
                level: "warn".to_string(),
                msg: warning.clone(),
            });
            if let Some(artifacts) = &self.artifacts {
                let _ = artifacts.append_manual_event("mcp_config_warning", &warning);
            }
        }
        if let Some(artifacts) = &self.artifacts {
            let _ = artifacts.write_plan_markdown("planning", None, &self.app.transcript_text());
        }

        {
            let bus = bus.clone();
            tokio::spawn(async move {
                run_session(
                    bus,
                    commands_rx,
                    roster,
                    task_for_engine,
                    cwd,
                    sandbox,
                    turns,
                    false,
                )
                .await;
            });
        }

        self.bus = Some(bus);
        self.commands = Some(commands);
        self.events = Some(events);
    }

    fn poll_events(&mut self) {
        loop {
            let received = match self.events.as_mut() {
                Some(events) => events.try_recv(),
                None => return,
            };
            match received {
                Ok(ev) => {
                    if let Some(artifacts) = &self.artifacts {
                        let _ = artifacts.append_event(&ev);
                    }
                    let write_snapshot = matches!(
                        &ev,
                        OrchestratorEvent::Message { .. }
                            | OrchestratorEvent::RecommendationReady { .. }
                            | OrchestratorEvent::ExecutionPacket { .. }
                            | OrchestratorEvent::AwaitingConfirmation { .. }
                            | OrchestratorEvent::SessionReport { .. }
                            | OrchestratorEvent::Fatal { .. }
                            | OrchestratorEvent::PhaseChanged { .. }
                    );
                    let status = event_status(&ev);
                    if matches!(ev, OrchestratorEvent::AwaitingConfirmation { .. }) {
                        self.state = PaneState::AwaitingApproval;
                    } else if matches!(ev, OrchestratorEvent::AgentExited { .. })
                        || matches!(ev, OrchestratorEvent::PhaseChanged { ref phase } if phase == "done")
                    {
                        self.state = PaneState::WaitingForInput;
                    } else if self.started {
                        self.state = PaneState::Running;
                    }
                    self.app.apply(ev);
                    if write_snapshot {
                        self.persist_plan_snapshot(status);
                    }
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Lagged(_)) => continue,
                Err(TryRecvError::Closed) => {
                    self.state = PaneState::Exited;
                    break;
                }
            }
        }
    }

    fn executor_prompt(&self) -> String {
        self.app.executor_handoff_prompt()
    }

    fn persist_plan_snapshot(&self, status: &str) {
        if let Some(artifacts) = &self.artifacts {
            let _ = artifacts.write_plan_markdown(
                status,
                self.app.recommended.as_deref(),
                &self.app.transcript_text(),
            );
            let _ = artifacts.write_manifest(status, self.app.recommended.as_deref());
        }
    }

    fn save_executor_plan(&self, executor: &str, prompt: &str) -> io::Result<PathBuf> {
        self.save_executor_plan_for(executor, prompt, "executor_launched", "executor_launched")
    }

    fn save_executor_plan_for(
        &self,
        executor: &str,
        prompt: &str,
        status: &str,
        event: &str,
    ) -> io::Result<PathBuf> {
        let dir = self.cfg.cwd.join(PLAN_DIR);
        fs::create_dir_all(&dir)?;
        let path = dir.join(LAST_PLAN_FILE);
        let saved_at = unix_time();
        let text = format!(
            "# Tales executor plan\n\n\
             - Executor: {}\n\
             - Executor key: {executor}\n\
             - Saved unix time: {saved_at}\n\
             - Workspace: {}\n\n\
             ## Handoff prompt\n\n\
             {prompt}\n",
            pretty(executor),
            self.cfg.cwd.display()
        );
        fs::write(&path, text)?;
        if let Some(artifacts) = &self.artifacts {
            let run_path = artifacts.dir.join("executor-plan.md");
            fs::write(&run_path, fs::read_to_string(&path)?)?;
            let _ =
                artifacts.write_plan_markdown(status, Some(executor), &self.app.transcript_text());
            let _ = artifacts.write_manifest(status, Some(executor));
            let _ = artifacts.append_manual_event(event, &format!("executor={executor}"));
        }
        Ok(path)
    }

    fn show_artifacts(&mut self) {
        let text = match &self.artifacts {
            Some(artifacts) => format!(
                "Artifacts\n\
                 run dir: {}\n\
                 plan: {}\n\
                 events: {}\n\
                 manifest: {}\n\
                 executor handoff: {}\n\
                 Recovery: use /handoff to resend the plan, or /switch <executor> to launch a fresh pane.",
                artifacts.relative_dir(),
                artifacts.plan_path.display(),
                artifacts.events_path.display(),
                artifacts.manifest_path.display(),
                self.cfg.cwd.join(PLAN_DIR).join(LAST_PLAN_FILE).display()
            ),
            None if self.started => {
                "Artifacts are unavailable for this run. Use /handoff or /switch <executor> to recreate .tales/last-plan.md from the current transcript.".to_string()
            }
            None => {
                "Artifacts are created after you start planning. Type a task first, then use /artifacts during the run.".to_string()
            }
        };
        if let Some(artifacts) = &self.artifacts {
            let _ = artifacts.append_manual_event("artifacts_requested", "user ran /artifacts");
        }
        self.app.note(text);
        self.notice = "showing artifact paths".to_string();
    }

    fn draw(&self, f: &mut Frame, area: Rect) {
        if !self.started {
            let lines = match self.startup_page {
                StartupPage::Welcome => welcome_lines(&self.cfg),
                StartupPage::Help => startup_help_lines(),
                StartupPage::Commands => startup_commands_lines(),
            };
            f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
            return;
        }

        let width = area.width as usize;
        let height = area.height as usize;
        let mut lines = self.app.render_lines(width);
        if lines.len() > height {
            lines = lines.split_off(lines.len() - height);
        }
        f.render_widget(Paragraph::new(lines), area);
    }
}

#[derive(Clone)]
enum SessionKind {
    Shell,
    Agent { key: String },
}

impl SessionKind {
    fn label(&self) -> &str {
        match self {
            SessionKind::Shell => "shell",
            SessionKind::Agent { key } => key,
        }
    }
}

fn welcome_lines(cfg: &WorkspaceConfig) -> Vec<Line<'static>> {
    let roster = if cfg.connect.is_empty() {
        vec!["claude".to_string(), "codex".to_string()]
    } else {
        cfg.connect.clone()
    };
    let roster_text = roster
        .iter()
        .map(|key| pretty(key))
        .collect::<Vec<_>>()
        .join(" + ");
    let mcp_risks = project_mcp_config_risks(&cfg.cwd);

    let mut lines = pixel_logo_lines();
    lines.extend([
        Line::from(""),
        Line::from(Span::styled(
            "Plan with multiple coding CLIs, then hand the agreed plan to one live executor.",
            Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
        )),
        Line::from(vec![
            Span::styled(
                "Start: ",
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ),
            Span::styled("type a task", Style::default().fg(TEXT)),
            Span::styled("  |  ", Style::default().fg(DIM)),
            Span::styled(
                "help",
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ),
            Span::styled(" for guidance", Style::default().fg(TEXT)),
            Span::styled("  |  ", Style::default().fg(DIM)),
            Span::styled(
                "commands",
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ),
            Span::styled(" for every command", Style::default().fg(TEXT)),
        ]),
        Line::from(Span::styled(
            format!("Workspace: {}", cfg.cwd.display()),
            Style::default().fg(DIM),
        )),
        Line::from(Span::styled(
            format!("Planner roster: {roster_text}"),
            Style::default().fg(DIM),
        )),
        Line::from(Span::styled(
            format!(
                "Artifacts: {}/{}/<run>/plan.md + events.jsonl; executor handoff: {}/{}",
                PLAN_DIR, RUNS_DIR, PLAN_DIR, LAST_PLAN_FILE
            ),
            Style::default().fg(DIM),
        )),
        Line::from(""),
        Line::from(Span::styled(
            "Available CLIs",
            Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
        )),
    ]);

    for tool in KNOWN_TOOLS {
        let installed = bin_path(tool.bin).is_some();
        let status = if installed { "ready" } else { "missing" };
        let status_color = if installed { ACCENT } else { FAINT };
        let caps = if tool.supports_headless {
            "planner + executor"
        } else {
            "executor"
        };
        lines.push(Line::from(vec![
            Span::styled("  ", Style::default().fg(DIM)),
            Span::styled(
                format!("{:<13}", tool.pretty),
                Style::default()
                    .fg(color_for(tool.key))
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(format!(" {:<10}", tool.bin), Style::default().fg(TEXT)),
            Span::styled(format!("{:<8}", status), Style::default().fg(status_color)),
            Span::styled(caps, Style::default().fg(DIM)),
        ]));
    }

    if !mcp_risks.is_empty() {
        lines.extend([
            Line::from(""),
            Line::from(Span::styled(
                "Safety notice",
                Style::default().fg(ERRC).add_modifier(Modifier::BOLD),
            )),
            Line::from(Span::styled(
                "  Project-local MCP/tool config files were found. Claude launches with project MCP loading disabled.",
                Style::default().fg(TEXT),
            )),
            Line::from(Span::styled(
                format!("  {}", risk_paths_summary(&mcp_risks)),
                Style::default().fg(DIM),
            )),
        ]);
    }

    lines.extend([
        Line::from(""),
        Line::from(Span::styled(
            "Tips",
            Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(
            "  - Describe the outcome, constraints, and files that matter.",
            Style::default().fg(TEXT),
        )),
        Line::from(Span::styled(
            "  - At the executor gate: Enter accepts the recommendation; 1-9 picks another tool.",
            Style::default().fg(TEXT),
        )),
        Line::from(Span::styled(
            "  - Once the executor pane opens, type directly there if it asks for input.",
            Style::default().fg(TEXT),
        )),
        Line::from(Span::styled(
            "  - If a run stalls, recover from the saved run artifact plan.md.",
            Style::default().fg(TEXT),
        )),
        Line::from(Span::styled(
            "  - Run tales doctor --all outside the TUI for install details.",
            Style::default().fg(TEXT),
        )),
        Line::from(Span::styled(
            format!("Saved executor plan: {}/{}", PLAN_DIR, LAST_PLAN_FILE),
            Style::default().fg(DIM),
        )),
        Line::from(""),
        Line::from(Span::styled(
            "Shortcuts: Ctrl-X Codex · Ctrl-L Claude · Ctrl-O Open Code · Ctrl-N shell · Tab switch · Ctrl-Q quit",
            Style::default().fg(DIM),
        )),
    ]);
    lines
}

fn pixel_logo_lines() -> Vec<Line<'static>> {
    vec![
        Line::from(vec![
            Span::styled(
                "  ████████  █████   ██      ███████  ███████",
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ),
            Span::styled("    ▄▄▄▄▄▄▄▄", Style::default().fg(DIM)),
        ]),
        Line::from(vec![
            Span::styled(
                "     ██    ██   ██  ██      ██       ██     ",
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ),
            Span::styled("   ██ ▄▄ ▄▄ ██", Style::default().fg(DIM)),
        ]),
        Line::from(vec![
            Span::styled(
                "     ██    ███████  ██      █████    ███████",
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ),
            Span::styled("  ██ ██ ██ ██", Style::default().fg(DIM)),
        ]),
        Line::from(vec![
            Span::styled(
                "     ██    ██   ██  ██      ██            ██",
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ),
            Span::styled("  ██ ▀▀ ▀▀ ██", Style::default().fg(DIM)),
        ]),
        Line::from(vec![
            Span::styled(
                "     ██    ██   ██  ██████  ███████  ███████",
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ),
            Span::styled("   ▀██████▀", Style::default().fg(DIM)),
        ]),
    ]
}

fn startup_help_lines() -> Vec<Line<'static>> {
    let mut lines = pixel_logo_lines();
    lines.push(Line::from(""));
    lines.extend(message_lines("Help", help_message()));
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "Type commands for the command list, Esc for welcome, or enter a task to start planning.",
        Style::default().fg(TEXT),
    )));
    lines
}

fn startup_commands_lines() -> Vec<Line<'static>> {
    let mut lines = pixel_logo_lines();
    lines.push(Line::from(""));
    lines.extend(message_lines("Commands", commands_message()));
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "Shell commands: tales --help · tales commands · tales doctor --all",
        Style::default().fg(TEXT),
    )));
    lines
}

fn unix_time() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn slug(input: &str) -> String {
    let mut out = String::new();
    for ch in input.chars().flat_map(|c| c.to_lowercase()) {
        if ch.is_ascii_alphanumeric() {
            out.push(ch);
        } else if (ch.is_whitespace() || ch == '-' || ch == '_') && !out.ends_with('-') {
            out.push('-');
        }
        if out.len() >= 48 {
            break;
        }
    }
    let out = out.trim_matches('-').to_string();
    if out.is_empty() {
        "run".to_string()
    } else {
        out
    }
}

fn risk_paths_summary(risks: &[McpConfigRisk]) -> String {
    risks
        .iter()
        .map(|risk| {
            let path = risk.path.display().to_string();
            if risk.markers.is_empty() {
                path
            } else {
                format!("{path} ({})", risk.markers.join(", "))
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn mcp_warning_text(risks: &[McpConfigRisk]) -> String {
    format!(
        "Project-local MCP/tool config detected: {}. Claude planner/executor launches disable project MCP loading for this run; inspect these files before using other CLIs that may load them.",
        risk_paths_summary(risks)
    )
}

fn agent_launch_args(key: &str, cwd: &Path) -> (Vec<String>, Option<String>) {
    let risks = project_mcp_config_risks(cwd);
    if key == "claude" && !risks.is_empty() {
        return (
            vec![
                "--strict-mcp-config".to_string(),
                "--mcp-config".to_string(),
                r#"{"mcpServers":{}}"#.to_string(),
                "--setting-sources".to_string(),
                "user".to_string(),
            ],
            Some(
                "project MCP config detected; launched Claude with project MCP loading disabled"
                    .to_string(),
            ),
        );
    }
    let note = if !risks.is_empty() {
        Some(format!(
            "project MCP config detected; inspect before using this CLI: {}",
            risk_paths_summary(&risks)
        ))
    } else {
        None
    };
    (Vec::new(), note)
}

fn event_status(ev: &OrchestratorEvent) -> &'static str {
    match ev {
        OrchestratorEvent::AwaitingConfirmation { .. } => "awaiting_executor",
        OrchestratorEvent::RecommendationReady { .. } => "recommended",
        OrchestratorEvent::ExecutionPacket { .. } => "execution_packet",
        OrchestratorEvent::SessionReport { .. } => "report_ready",
        OrchestratorEvent::Fatal { .. } => "failed",
        OrchestratorEvent::PhaseChanged { phase } if phase == "done" => "done",
        OrchestratorEvent::PhaseChanged { phase } if phase == "executing" => "executing",
        OrchestratorEvent::PhaseChanged { phase } if phase == "recommending" => "recommending",
        _ => "planning",
    }
}

fn event_record(ev: &OrchestratorEvent) -> serde_json::Value {
    match ev {
        OrchestratorEvent::AgentSpawned {
            agent,
            label,
            session_id,
        } => json!({
            "time_unix": unix_time(),
            "kind": "agent_spawned",
            "agent": agent.to_string(),
            "label": label,
            "session_id": session_id,
        }),
        OrchestratorEvent::Token { agent, text } => json!({
            "time_unix": unix_time(),
            "kind": "token",
            "agent": agent.to_string(),
            "chars": text.chars().count(),
            "preview": preview(text, 240),
        }),
        OrchestratorEvent::TurnStarted { agent, role } => json!({
            "time_unix": unix_time(),
            "kind": "turn_started",
            "agent": agent.to_string(),
            "role": role,
        }),
        OrchestratorEvent::Message { agent, text } => json!({
            "time_unix": unix_time(),
            "kind": "message",
            "agent": agent.to_string(),
            "chars": text.chars().count(),
            "text": text,
        }),
        OrchestratorEvent::UserMessage { text } => json!({
            "time_unix": unix_time(),
            "kind": "user_message",
            "chars": text.chars().count(),
            "text": text,
        }),
        OrchestratorEvent::ToolActivity { agent, summary } => json!({
            "time_unix": unix_time(),
            "kind": "tool_activity",
            "agent": agent.to_string(),
            "summary": summary,
        }),
        OrchestratorEvent::TurnComplete {
            agent,
            cost_usd,
            token_usage,
        } => json!({
            "time_unix": unix_time(),
            "kind": "turn_complete",
            "agent": agent.to_string(),
            "cost_usd": cost_usd,
            "token_usage": token_usage.as_ref().map(|usage| json!({
                "input_tokens": usage.input_tokens,
                "output_tokens": usage.output_tokens,
                "total_tokens": usage.total_tokens,
            })),
        }),
        OrchestratorEvent::AgentExited { agent, code } => json!({
            "time_unix": unix_time(),
            "kind": "agent_exited",
            "agent": agent.to_string(),
            "code": code,
        }),
        OrchestratorEvent::PhaseChanged { phase } => json!({
            "time_unix": unix_time(),
            "kind": "phase_changed",
            "phase": phase,
        }),
        OrchestratorEvent::RecommendationReady {
            executor,
            rationale,
            confident,
            scores,
        } => json!({
            "time_unix": unix_time(),
            "kind": "recommendation_ready",
            "executor": executor,
            "rationale": rationale,
            "confident": confident,
            "scores": scores,
        }),
        OrchestratorEvent::ExecutionPacket {
            executor,
            text,
            included_in_prompt,
        } => json!({
            "time_unix": unix_time(),
            "kind": "execution_packet",
            "executor": executor,
            "chars": text.chars().count(),
            "included_in_prompt": included_in_prompt,
            "text": text,
        }),
        OrchestratorEvent::SessionReport { markdown, summary } => json!({
            "time_unix": unix_time(),
            "kind": "session_report",
            "markdown_chars": markdown.chars().count(),
            "markdown": markdown,
            "summary": summary,
        }),
        OrchestratorEvent::AwaitingConfirmation { prompt } => json!({
            "time_unix": unix_time(),
            "kind": "awaiting_confirmation",
            "prompt": prompt,
        }),
        OrchestratorEvent::Log { level, msg } => json!({
            "time_unix": unix_time(),
            "kind": "log",
            "level": level,
            "message": msg,
        }),
        OrchestratorEvent::Fatal { msg } => json!({
            "time_unix": unix_time(),
            "kind": "fatal",
            "message": msg,
        }),
    }
}

fn preview(text: &str, limit: usize) -> String {
    let mut out = text.chars().take(limit).collect::<String>();
    if text.chars().count() > limit {
        out.push_str("...");
    }
    out
}

fn message_lines(title: &str, text: &str) -> Vec<Line<'static>> {
    let mut lines = vec![Line::from(Span::styled(
        title.to_string(),
        Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
    ))];
    lines.extend(text.lines().map(|line| {
        let style = if line.starts_with("Tales ") {
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(TEXT)
        };
        Line::from(Span::styled(line.trim().to_string(), style))
    }));
    lines
}

struct ProcessPane {
    id: PaneId,
    kind: SessionKind,
    title: String,
    command: String,
    cwd: PathBuf,
    state: PaneState,
    lines: VecDeque<String>,
    pending: String,
    writer: Option<ChildInput>,
    child: Option<Child>,
    last_output: Instant,
}

impl ProcessPane {
    fn spawn(
        id: PaneId,
        kind: SessionKind,
        title: String,
        command: String,
        args: Vec<String>,
        cwd: PathBuf,
        tx: mpsc::UnboundedSender<TerminalEvent>,
    ) -> Self {
        match spawn_child(id, &command, &args, &cwd, tx) {
            Ok((child, writer)) => {
                let mut pane = Self {
                    id,
                    kind,
                    title,
                    command,
                    cwd,
                    state: PaneState::Running,
                    lines: VecDeque::new(),
                    pending: String::new(),
                    writer: Some(writer),
                    child: Some(child),
                    last_output: Instant::now(),
                };
                pane.push_line("session started");
                pane
            }
            Err(e) => {
                let mut pane = Self {
                    id,
                    kind,
                    title,
                    command,
                    cwd,
                    state: PaneState::Exited,
                    lines: VecDeque::new(),
                    pending: String::new(),
                    writer: None,
                    child: None,
                    last_output: Instant::now(),
                };
                pane.push_line(format!("failed to start session: {e}"));
                pane
            }
        }
    }

    fn write(&mut self, bytes: &[u8]) {
        let Some(writer) = &self.writer else {
            return;
        };
        if let Ok(mut w) = writer.lock() {
            let _ = w.write_all(bytes);
            let _ = w.flush();
        }
    }

    fn can_receive_input(&self) -> bool {
        self.state != PaneState::Exited && self.writer.is_some()
    }

    fn reset_for_execution(&mut self, plan_path: Option<&Path>) {
        self.lines.clear();
        self.pending.clear();
        self.push_line(format!("{} executor started", self.title));
        if let Some(path) = plan_path {
            self.push_line(format!("saved plan: {}", path.display()));
        }
        self.push_line("working on the handoff prompt now");
        self.push_line("");
    }

    fn push_output(&mut self, bytes: &[u8]) {
        let text = strip_ansi(&String::from_utf8_lossy(bytes));
        let mut chars = text.chars().peekable();
        while let Some(ch) = chars.next() {
            match ch {
                '\r' => {
                    if matches!(chars.peek(), Some('\n')) {
                        continue;
                    }
                    if !self.pending.is_empty() {
                        let line = std::mem::take(&mut self.pending);
                        self.push_line(line);
                    }
                }
                '\n' => {
                    let line = std::mem::take(&mut self.pending);
                    self.push_line(line);
                }
                '\u{0008}' | '\u{007f}' => {
                    self.pending.pop();
                }
                '\t' => self.pending.push_str("    "),
                c if c.is_control() => {}
                c => self.pending.push(c),
            }
        }
        self.last_output = Instant::now();
        self.detect_state();
    }

    fn push_line(&mut self, line: impl Into<String>) {
        self.lines.push_back(line.into());
        while self.lines.len() > MAX_SCROLLBACK {
            self.lines.pop_front();
        }
    }

    fn detect_state(&mut self) {
        if self.state == PaneState::Exited {
            return;
        }
        let sample = format!(
            "{}\n{}",
            self.lines.back().cloned().unwrap_or_default(),
            self.pending
        );
        let lower = sample.to_lowercase();
        if lower.contains("approve")
            || lower.contains("allow this")
            || lower.contains("permission")
            || lower.contains("[y/n]")
            || lower.contains("(y/n)")
            || lower.contains("yes/no")
        {
            self.state = PaneState::AwaitingApproval;
        } else if sample.trim_end().ends_with('$')
            || sample.trim_end().ends_with('%')
            || sample.trim_end().ends_with('>')
            || sample.trim_end().ends_with('?')
        {
            self.state = PaneState::WaitingForInput;
        } else {
            self.state = PaneState::Running;
        }
    }

    fn poll_exit(&mut self) {
        let Some(child) = &mut self.child else {
            return;
        };
        match child.try_wait() {
            Ok(Some(status)) => {
                self.state = PaneState::Exited;
                self.push_line(format!("process exited: {status}"));
                self.child = None;
                self.writer = None;
            }
            Ok(None) => {}
            Err(e) => {
                self.state = PaneState::Exited;
                self.push_line(format!("process status error: {e}"));
                self.child = None;
                self.writer = None;
            }
        }
    }

    fn shutdown(&mut self) {
        if let Some(child) = &mut self.child {
            let _ = child.kill();
            let _ = child.wait();
        }
        self.child = None;
        self.writer = None;
    }

    fn draw(&self, f: &mut Frame, area: Rect) {
        let height = area.height as usize;
        let width = area.width.saturating_sub(1).max(8) as usize;
        let mut logical = self.lines.iter().cloned().collect::<Vec<_>>();
        if !self.pending.is_empty() {
            logical.push(self.pending.clone());
        }
        if logical.is_empty() {
            logical.push(format!(
                "{} · {} · {}",
                self.kind.label(),
                self.command,
                self.cwd.to_string_lossy()
            ));
        }
        let mut visual = Vec::new();
        for line in logical {
            visual.extend(wrap_plain_line(&line, width));
        }
        if visual.len() > height {
            visual = visual.split_off(visual.len() - height);
        }
        let rendered = visual
            .into_iter()
            .map(|line| Line::from(Span::styled(line, Style::default().fg(TEXT))))
            .collect::<Vec<_>>();
        f.render_widget(Paragraph::new(rendered), area);
    }
}

fn wrap_plain_line(line: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    if line.is_empty() {
        return vec![String::new()];
    }

    let mut out = Vec::new();
    let mut current = String::new();
    let mut count = 0;
    for ch in line.chars() {
        if count >= width {
            out.push(std::mem::take(&mut current));
            count = 0;
        }
        current.push(ch);
        count += 1;
    }
    out.push(current);
    out
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PaneState {
    Running,
    WaitingForInput,
    AwaitingApproval,
    Exited,
}

impl PaneState {
    fn badge(self) -> &'static str {
        match self {
            PaneState::Running => "run",
            PaneState::WaitingForInput => "wait",
            PaneState::AwaitingApproval => "ask",
            PaneState::Exited => "exit",
        }
    }

    fn color(self) -> Color {
        match self {
            PaneState::Running => ACCENT,
            PaneState::WaitingForInput => TEXT,
            PaneState::AwaitingApproval => ERRC,
            PaneState::Exited => DIM,
        }
    }
}

enum TerminalEvent {
    Output { pane_id: PaneId, bytes: Vec<u8> },
    ReaderClosed { pane_id: PaneId },
}

fn default_shell() -> String {
    std::env::var("SHELL").unwrap_or_else(|_| {
        if cfg!(windows) {
            "cmd".to_string()
        } else {
            "/bin/sh".to_string()
        }
    })
}

fn key_to_bytes(key: KeyEvent) -> Option<Vec<u8>> {
    match (key.code, key.modifiers) {
        (KeyCode::Char(c), KeyModifiers::CONTROL) if c.is_ascii_alphabetic() => {
            Some(vec![(c.to_ascii_lowercase() as u8) - b'a' + 1])
        }
        (KeyCode::Char(c), _) => Some(c.to_string().into_bytes()),
        (KeyCode::Enter, _) => Some(b"\r".to_vec()),
        (KeyCode::Backspace, _) => Some(vec![0x7f]),
        (KeyCode::Tab, _) => Some(b"\t".to_vec()),
        (KeyCode::Esc, _) => Some(vec![0x1b]),
        (KeyCode::Up, _) => Some(b"\x1b[A".to_vec()),
        (KeyCode::Down, _) => Some(b"\x1b[B".to_vec()),
        (KeyCode::Right, _) => Some(b"\x1b[C".to_vec()),
        (KeyCode::Left, _) => Some(b"\x1b[D".to_vec()),
        (KeyCode::Home, _) => Some(b"\x1b[H".to_vec()),
        (KeyCode::End, _) => Some(b"\x1b[F".to_vec()),
        (KeyCode::Delete, _) => Some(b"\x1b[3~".to_vec()),
        _ => None,
    }
}

fn spawn_child(
    pane_id: PaneId,
    command: &str,
    args: &[String],
    cwd: &Path,
    tx: mpsc::UnboundedSender<TerminalEvent>,
) -> io::Result<(Child, ChildInput)> {
    #[cfg(unix)]
    {
        spawn_pty_child(pane_id, command, args, cwd, tx)
    }
    #[cfg(not(unix))]
    {
        spawn_piped_child(pane_id, command, args, cwd, tx)
    }
}

#[cfg(unix)]
fn spawn_pty_child(
    pane_id: PaneId,
    command: &str,
    args: &[String],
    cwd: &Path,
    tx: mpsc::UnboundedSender<TerminalEvent>,
) -> io::Result<(Child, ChildInput)> {
    use std::fs::File;
    use std::os::unix::io::{FromRawFd, RawFd};
    use std::os::unix::process::CommandExt;

    let mut master: RawFd = -1;
    let mut slave: RawFd = -1;
    let mut size = libc::winsize {
        ws_row: 32,
        ws_col: 120,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let rc = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut size,
        )
    };
    if rc == -1 {
        return Err(io::Error::last_os_error());
    }

    let dup_stdio = |fd| -> io::Result<Stdio> {
        let duped = unsafe { libc::dup(fd) };
        if duped == -1 {
            Err(io::Error::last_os_error())
        } else {
            Ok(unsafe { Stdio::from_raw_fd(duped) })
        }
    };

    let slave_for_ctty = slave;
    let mut cmd = Command::new(command);
    cmd.args(args)
        .current_dir(cwd)
        .env("TERM", "xterm-256color")
        .stdin(dup_stdio(slave)?)
        .stdout(dup_stdio(slave)?)
        .stderr(dup_stdio(slave)?);
    unsafe {
        cmd.pre_exec(move || {
            if libc::setsid() == -1 {
                return Err(io::Error::last_os_error());
            }
            let _ = libc::ioctl(slave_for_ctty, libc::TIOCSCTTY.into(), 0);
            Ok(())
        });
    }

    let child = match cmd.spawn() {
        Ok(child) => child,
        Err(e) => {
            unsafe {
                libc::close(master);
                libc::close(slave);
            }
            return Err(e);
        }
    };
    unsafe {
        libc::close(slave);
    }

    let mut reader = unsafe { File::from_raw_fd(master) };
    let writer = reader.try_clone()?;
    std::thread::spawn(move || {
        let mut buf = [0_u8; 4096];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if tx
                        .send(TerminalEvent::Output {
                            pane_id,
                            bytes: buf[..n].to_vec(),
                        })
                        .is_err()
                    {
                        break;
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }
        let _ = tx.send(TerminalEvent::ReaderClosed { pane_id });
    });

    Ok((child, Arc::new(Mutex::new(Box::new(writer)))))
}

#[cfg(not(unix))]
fn spawn_piped_child(
    pane_id: PaneId,
    command: &str,
    args: &[String],
    cwd: &Path,
    tx: mpsc::UnboundedSender<TerminalEvent>,
) -> io::Result<(Child, ChildInput)> {
    let mut child = Command::new(command)
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let stdin = child.stdin.take().expect("piped stdin");
    if let Some(mut stdout) = child.stdout.take() {
        let tx = tx.clone();
        std::thread::spawn(move || pipe_reader(pane_id, &mut stdout, tx));
    }
    if let Some(mut stderr) = child.stderr.take() {
        std::thread::spawn(move || pipe_reader(pane_id, &mut stderr, tx));
    }
    Ok((child, Arc::new(Mutex::new(Box::new(stdin)))))
}

#[cfg(not(unix))]
fn pipe_reader<R: Read>(pane_id: PaneId, reader: &mut R, tx: mpsc::UnboundedSender<TerminalEvent>) {
    let mut buf = [0_u8; 4096];
    loop {
        match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                if tx
                    .send(TerminalEvent::Output {
                        pane_id,
                        bytes: buf[..n].to_vec(),
                    })
                    .is_err()
                {
                    break;
                }
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }
    let _ = tx.send(TerminalEvent::ReaderClosed { pane_id });
}

fn strip_ansi(input: &str) -> String {
    #[derive(Clone, Copy)]
    enum State {
        Normal,
        Esc,
        Csi,
        Osc,
        OscEsc,
    }

    let mut out = String::new();
    let mut state = State::Normal;
    for ch in input.chars() {
        match state {
            State::Normal => {
                if ch == '\x1b' {
                    state = State::Esc;
                } else {
                    out.push(ch);
                }
            }
            State::Esc => match ch {
                '[' => state = State::Csi,
                ']' => state = State::Osc,
                _ => state = State::Normal,
            },
            State::Csi => {
                if ('@'..='~').contains(&ch) {
                    state = State::Normal;
                }
            }
            State::Osc => match ch {
                '\x07' => state = State::Normal,
                '\x1b' => state = State::OscEsc,
                _ => {}
            },
            State::OscEsc => {
                state = State::Normal;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_pane() -> ProcessPane {
        ProcessPane {
            id: 1,
            kind: SessionKind::Shell,
            title: "test".into(),
            command: "test".into(),
            cwd: PathBuf::from("."),
            state: PaneState::Running,
            lines: VecDeque::new(),
            pending: String::new(),
            writer: None,
            child: None,
            last_output: Instant::now(),
        }
    }

    fn lines_text(lines: &[Line<'static>]) -> String {
        lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn test_workspace_config(cwd: PathBuf) -> WorkspaceConfig {
        WorkspaceConfig {
            connect: vec!["claude".into(), "codex".into()],
            prefill: None,
            cwd,
            sandbox: "workspace-write".into(),
            turns: 4,
        }
    }

    #[test]
    fn welcome_screen_has_pixel_art_tips_and_command_cues() {
        let cfg = test_workspace_config(PathBuf::from("/tmp/tales"));
        let text = lines_text(&welcome_lines(&cfg));

        assert!(text.contains("████████"), "{text}");
        assert!(text.contains("help"), "{text}");
        assert!(text.contains("commands"), "{text}");
        assert!(text.contains("Tips"), "{text}");
        assert!(text.contains("tales doctor --all"), "{text}");
    }

    #[test]
    fn startup_help_and_commands_pages_render() {
        let help = lines_text(&startup_help_lines());
        let commands = lines_text(&startup_commands_lines());

        assert!(help.contains("Tales help"), "{help}");
        assert!(help.contains("Type /commands"), "{help}");
        assert!(commands.contains("Tales commands"), "{commands}");
        assert!(commands.contains("/attach <path>"), "{commands}");
    }

    #[test]
    fn startup_command_switches_pages() {
        let cwd = std::env::temp_dir().join("tales-startup-command-test");
        let mut pane = TalesPane::new(1, test_workspace_config(cwd));

        assert!(pane.handle_startup_command("help"));
        assert_eq!(pane.startup_page, StartupPage::Help);
        assert!(pane.handle_startup_command("/commands"));
        assert_eq!(pane.startup_page, StartupPage::Commands);
        assert!(!pane.handle_startup_command("build a feature"));
    }

    #[test]
    fn folder_browser_entries_include_current_parent_and_sorted_dirs() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let cwd = std::env::temp_dir().join(format!(
            "tales-folder-browser-test-{}-{unique}",
            std::process::id()
        ));
        fs::create_dir_all(cwd.join("zeta")).unwrap();
        fs::create_dir_all(cwd.join("Alpha")).unwrap();
        fs::write(cwd.join("not-a-dir.txt"), "skip").unwrap();

        let entries = load_folder_entries(&cwd).unwrap();
        let labels = entries
            .iter()
            .map(|entry| entry.label.as_str())
            .collect::<Vec<_>>();

        assert_eq!(labels[0], ".");
        assert_eq!(entries[0].kind, FolderEntryKind::Current);
        assert_eq!(labels[1], "..");
        assert_eq!(entries[1].kind, FolderEntryKind::Parent);
        assert_eq!(&labels[2..], ["Alpha", "zeta"]);

        let _ = fs::remove_dir_all(&cwd);
    }

    #[test]
    fn onboarding_permission_choices_map_to_sandbox_values() {
        assert_eq!(permission_index_for("workspace-write"), 0);
        assert_eq!(
            PERMISSION_OPTIONS[permission_index_for("read-only")].sandbox,
            "read-only"
        );
        assert_eq!(
            PERMISSION_OPTIONS[permission_index_for("danger-full-access")].sandbox,
            "danger-full-access"
        );
        assert_eq!(permission_index_for("unknown-sandbox"), 0);
    }

    #[test]
    fn onboarding_visible_entry_range_tracks_selection() {
        assert_eq!(visible_entry_range(0, 0, 5), 0..0);
        assert_eq!(visible_entry_range(10, 0, 4), 0..4);
        assert_eq!(visible_entry_range(10, 5, 4), 3..7);
        assert_eq!(visible_entry_range(10, 9, 4), 6..10);
    }

    #[test]
    fn cd_path_resolver_supports_absolute_relative_home_and_cd_prefix() {
        let current = PathBuf::from("/tmp/tales-current");

        assert_eq!(
            resolve_cd_path(&current, "/tmp/target").unwrap(),
            PathBuf::from("/tmp/target")
        );
        assert_eq!(
            resolve_cd_path(&current, "child/repo").unwrap(),
            PathBuf::from("/tmp/tales-current/child/repo")
        );
        assert_eq!(
            resolve_cd_path(&current, "cd ../other").unwrap(),
            PathBuf::from("/tmp/tales-current/../other")
        );

        if let Some(home) = std::env::var_os("HOME") {
            assert_eq!(
                resolve_cd_path(&current, "~").unwrap(),
                PathBuf::from(&home)
            );
            assert_eq!(
                resolve_cd_path(&current, "~/repo").unwrap(),
                PathBuf::from(home).join("repo")
            );
        }
        assert!(resolve_cd_path(&current, "   ").is_err());
    }

    #[test]
    fn onboarding_screen_confirm_current_opens_permission_phase() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let cwd = std::env::temp_dir().join(format!(
            "tales-onboarding-confirm-test-{}-{unique}",
            std::process::id()
        ));
        fs::create_dir_all(&cwd).unwrap();
        fs::write(
            cwd.join(".mcp.json"),
            r#"{"env":{"SUPABASE_ACCESS_TOKEN":"example"}}"#,
        )
        .unwrap();

        let mut screen = WorkspaceOnboardingScreen::new(cwd.clone(), "workspace-write").unwrap();
        screen.confirm_current();

        assert_eq!(screen.phase, OnboardingPhase::Permissions);
        assert_eq!(screen.current, fs::canonicalize(&cwd).unwrap());
        assert_eq!(screen.permission_selected, 0);
        assert_eq!(screen.mcp_risks.len(), 1);

        let _ = fs::remove_dir_all(&cwd);
    }

    #[test]
    fn onboarding_cd_path_jumps_to_permissions_for_known_path() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let cwd = std::env::temp_dir().join(format!(
            "tales-onboarding-cd-test-{}-{unique}",
            std::process::id()
        ));
        let target = cwd.join("repo");
        fs::create_dir_all(&target).unwrap();

        let mut screen = WorkspaceOnboardingScreen::new(cwd.clone(), "workspace-write").unwrap();
        screen.open_path_input("repo");
        screen.confirm_path_input().unwrap();

        assert_eq!(screen.phase, OnboardingPhase::Permissions);
        assert_eq!(screen.current, fs::canonicalize(&target).unwrap());
        assert!(screen.path_input.is_empty());

        let _ = fs::remove_dir_all(&cwd);
    }

    #[test]
    fn strips_common_ansi_sequences() {
        assert_eq!(strip_ansi("\x1b[32mhello\x1b[0m"), "hello");
        assert_eq!(strip_ansi("a\x1b]0;title\x07b"), "ab");
    }

    #[test]
    fn process_output_preserves_crlf_lines() {
        let mut pane = test_pane();
        pane.push_output(b"one\r\ntwo\r\n");
        let lines = pane.lines.into_iter().collect::<Vec<_>>();
        assert_eq!(lines, vec!["one", "two"]);
        assert!(pane.pending.is_empty());
    }

    #[test]
    fn process_output_turns_bare_carriage_return_into_history() {
        let mut pane = test_pane();
        pane.push_output(b"working 1\rworking 2\rready");
        let lines = pane.lines.into_iter().collect::<Vec<_>>();
        assert_eq!(lines, vec!["working 1", "working 2"]);
        assert_eq!(pane.pending, "ready");
    }

    #[test]
    fn wraps_process_lines_to_pane_width() {
        assert_eq!(
            wrap_plain_line("abcdef", 2),
            vec!["ab".to_string(), "cd".to_string(), "ef".to_string()]
        );
        assert_eq!(wrap_plain_line("", 2), vec!["".to_string()]);
    }

    #[test]
    fn save_executor_plan_writes_last_plan_in_workspace() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let cwd =
            std::env::temp_dir().join(format!("tales-plan-test-{}-{unique}", std::process::id()));
        fs::create_dir_all(&cwd).unwrap();

        let pane = TalesPane::new(
            1,
            WorkspaceConfig {
                connect: vec!["claude".into(), "codex".into()],
                prefill: None,
                cwd: cwd.clone(),
                sandbox: "workspace-write".into(),
                turns: 4,
            },
        );
        let path = pane
            .save_executor_plan("codex", "Execute this saved handoff.")
            .unwrap();
        let text = fs::read_to_string(&path).unwrap();

        assert_eq!(path, cwd.join(PLAN_DIR).join(LAST_PLAN_FILE));
        assert!(text.contains("Executor key: codex"));
        assert!(text.contains("Execute this saved handoff."));

        let _ = fs::remove_dir_all(&cwd);
    }

    #[test]
    fn show_artifacts_lists_recovery_paths() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let cwd = std::env::temp_dir().join(format!(
            "tales-artifacts-command-test-{}-{unique}",
            std::process::id()
        ));
        fs::create_dir_all(&cwd).unwrap();

        let mut pane = TalesPane::new(1, test_workspace_config(cwd.clone()));
        pane.started = true;
        pane.artifacts = Some(
            RunArtifacts::create(
                &cwd,
                "Recover a stuck executor",
                &["claude".into(), "codex".into()],
            )
            .unwrap(),
        );
        pane.show_artifacts();

        let text = lines_text(&pane.app.render_lines(120));
        assert!(text.contains("Artifacts"), "{text}");
        assert!(text.contains("plan.md"), "{text}");
        assert!(text.contains("events.jsonl"), "{text}");
        assert!(text.contains("/handoff"), "{text}");
        assert!(text.contains("/switch <executor>"), "{text}");

        let _ = fs::remove_dir_all(&cwd);
    }

    #[test]
    fn save_executor_plan_for_records_recovery_status() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let cwd = std::env::temp_dir().join(format!(
            "tales-handoff-status-test-{}-{unique}",
            std::process::id()
        ));
        fs::create_dir_all(&cwd).unwrap();

        let mut pane = TalesPane::new(1, test_workspace_config(cwd.clone()));
        pane.artifacts = Some(
            RunArtifacts::create(
                &cwd,
                "Retry the executor",
                &["claude".into(), "codex".into()],
            )
            .unwrap(),
        );
        pane.save_executor_plan_for(
            "claude",
            "Retry this plan.",
            "handoff_ready",
            "executor_handoff_ready",
        )
        .unwrap();

        let artifacts = pane.artifacts.as_ref().unwrap();
        let manifest = fs::read_to_string(&artifacts.manifest_path).unwrap();
        let events = fs::read_to_string(&artifacts.events_path).unwrap();
        let plan = fs::read_to_string(&artifacts.plan_path).unwrap();

        assert!(manifest.contains("handoff_ready"), "{manifest}");
        assert!(events.contains("executor_handoff_ready"), "{events}");
        assert!(plan.contains("Executor: Claude Code"), "{plan}");
        assert!(plan.contains("handoff_ready"), "{plan}");

        let _ = fs::remove_dir_all(&cwd);
    }

    #[test]
    fn run_artifacts_write_manifest_events_and_plan() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let cwd = std::env::temp_dir().join(format!(
            "tales-run-artifacts-test-{}-{unique}",
            std::process::id()
        ));
        fs::create_dir_all(&cwd).unwrap();

        let artifacts = RunArtifacts::create(
            &cwd,
            "Fix the timeout and save plan",
            &["claude".into(), "codex".into()],
        )
        .unwrap();
        artifacts
            .append_manual_event("test_event", "artifact smoke test")
            .unwrap();
        artifacts
            .write_plan_markdown(
                "recommended",
                Some("codex"),
                "Codex should execute the plan.",
            )
            .unwrap();

        let manifest = fs::read_to_string(&artifacts.manifest_path).unwrap();
        let events = fs::read_to_string(&artifacts.events_path).unwrap();
        let plan = fs::read_to_string(&artifacts.plan_path).unwrap();

        assert!(manifest.contains("Fix the timeout"), "{manifest}");
        assert!(events.contains("test_event"), "{events}");
        assert!(plan.contains("Status: recommended"), "{plan}");
        assert!(plan.contains("Executor: Codex"), "{plan}");

        let _ = fs::remove_dir_all(&cwd);
    }

    #[test]
    fn claude_launch_disables_project_mcp_when_config_exists() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let cwd = std::env::temp_dir().join(format!(
            "tales-mcp-risk-test-{}-{unique}",
            std::process::id()
        ));
        fs::create_dir_all(&cwd).unwrap();
        fs::write(
            cwd.join(".mcp.json"),
            r#"{"env":{"SUPABASE_ACCESS_TOKEN":"example"}}"#,
        )
        .unwrap();

        let (args, note) = agent_launch_args("claude", &cwd);
        assert!(
            args.contains(&"--strict-mcp-config".to_string()),
            "{args:?}"
        );
        assert!(
            args.contains(&r#"{"mcpServers":{}}"#.to_string()),
            "{args:?}"
        );
        assert!(note.unwrap().contains("disabled"));

        let text = lines_text(&welcome_lines(&test_workspace_config(cwd.clone())));
        assert!(text.contains("Safety notice"), "{text}");
        assert!(text.contains("SUPABASE_ACCESS_TOKEN"), "{text}");

        let _ = fs::remove_dir_all(&cwd);
    }

    #[test]
    fn approval_detection_marks_session() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut pane = ProcessPane::spawn(
            1,
            SessionKind::Shell,
            "missing".into(),
            "/definitely/not/a/bin".into(),
            Vec::new(),
            PathBuf::from("."),
            tx,
        );
        pane.state = PaneState::Running;
        pane.push_output(b"Approve command? [y/n]");
        assert_eq!(pane.state, PaneState::AwaitingApproval);
    }
}
