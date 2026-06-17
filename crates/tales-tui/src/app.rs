//! TUI state + Warp-style block rendering.
//!
//! Each agent turn renders as a "block": a colored left bar + name + role badge
//! header, then an indented body. Minimal chrome, restrained palette, monospace
//! (a terminal is monospace by nature) — clean and lightweight.

use std::collections::HashMap;
use std::path::PathBuf;

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use tales_core::event::{OrchestratorEvent, UserCommand};
use uuid::Uuid;

use crate::theme::{color_for, pretty, ACCENT, DIM, ERRC, FAINT, TEXT, YOU};

#[derive(Clone, Copy)]
enum SysKind {
    Note,
    Rec,
    Err,
}

enum Block {
    Agent {
        label: String,
        role: Option<String>,
        text: String,
    },
    You(String),
    Sys(String, SysKind),
}

/// All UI state.
pub struct App {
    pub task: String,
    pub phase: String,
    blocks: Vec<Block>,
    /// In-progress streamed text per agent (empty string = "thinking").
    partial: HashMap<Uuid, String>,
    labels: HashMap<Uuid, String>,
    roles: HashMap<Uuid, String>,
    pub input: String,
    pub recommended: Option<String>,
    pub awaiting: bool,
    pub should_quit: bool,
    /// Media queued via /attach, sent with the next message.
    pending_attachments: Vec<PathBuf>,
    /// The connected tools, in roster order — the executor candidates the user
    /// can pick at the gate (by name or by 1-based number).
    candidates: Vec<String>,
}

impl App {
    pub fn new(task: String) -> Self {
        Self {
            task,
            phase: "idle".to_string(),
            blocks: Vec::new(),
            partial: HashMap::new(),
            labels: HashMap::new(),
            roles: HashMap::new(),
            input: String::new(),
            recommended: None,
            awaiting: false,
            should_quit: false,
            pending_attachments: Vec::new(),
            candidates: Vec::new(),
        }
    }

    /// Record the connected tools so the gate can offer them as numbered
    /// executor choices.
    pub fn set_candidates(&mut self, candidates: Vec<String>) {
        self.candidates = candidates;
    }

    /// Number of files queued to send with the next message.
    pub fn pending_count(&self) -> usize {
        self.pending_attachments.len()
    }

    fn sys(&mut self, text: impl Into<String>, kind: SysKind) {
        self.blocks.push(Block::Sys(text.into(), kind));
    }

    fn label_of(&self, agent: &Uuid) -> String {
        self.labels
            .get(agent)
            .cloned()
            .unwrap_or_else(|| "?".to_string())
    }

    /// Fold a bus event into the chat state.
    pub fn apply(&mut self, ev: OrchestratorEvent) {
        match ev {
            OrchestratorEvent::AgentSpawned { agent, label, .. } => {
                self.labels.insert(agent, label.clone());
                self.sys(format!("{} joined", pretty(&label)), SysKind::Note);
            }
            OrchestratorEvent::TurnStarted { agent, role } => {
                self.roles.insert(agent, role);
                self.partial.entry(agent).or_default(); // empty = thinking placeholder
            }
            OrchestratorEvent::Token { agent, text } => {
                self.partial.entry(agent).or_default().push_str(&text);
            }
            OrchestratorEvent::Message { agent, text } => {
                self.partial.remove(&agent);
                let label = self.label_of(&agent);
                let role = self.roles.get(&agent).cloned();
                self.blocks.push(Block::Agent { label, role, text });
            }
            OrchestratorEvent::UserMessage { text } => {
                self.blocks.push(Block::You(text));
            }
            OrchestratorEvent::ToolActivity { agent, summary } => {
                let label = self.label_of(&agent);
                self.sys(format!("⚙ {} · {summary}", pretty(&label)), SysKind::Note);
            }
            OrchestratorEvent::TurnComplete { agent, cost_usd } => {
                self.partial.remove(&agent);
                if let Some(c) = cost_usd {
                    self.sys(
                        format!("{} · ${c:.4}", pretty(&self.label_of(&agent))),
                        SysKind::Note,
                    );
                }
            }
            OrchestratorEvent::PhaseChanged { phase } => {
                if phase != "awaitingconfirmation" {
                    self.awaiting = false;
                }
                self.phase = phase;
            }
            OrchestratorEvent::RecommendationReady {
                executor,
                rationale,
            } => {
                self.recommended = Some(executor.clone());
                self.sys(
                    format!("recommend {}\n{}", pretty(&executor), rationale.trim()),
                    SysKind::Rec,
                );
            }
            OrchestratorEvent::AwaitingConfirmation { .. } => {
                self.awaiting = true;
            }
            OrchestratorEvent::AgentExited { agent, code } => {
                self.sys(
                    format!("{} exited ({code:?})", pretty(&self.label_of(&agent))),
                    SysKind::Note,
                );
            }
            OrchestratorEvent::Log { level, msg } => {
                if msg.contains(" speaking as ") {
                    return; // shown as the thinking block instead
                }
                let kind = if level == "error" {
                    SysKind::Err
                } else {
                    SysKind::Note
                };
                self.sys(msg, kind);
            }
            OrchestratorEvent::Fatal { msg } => self.sys(format!("✗ {msg}"), SysKind::Err),
        }
    }

    /// Interpret the input line on Enter and clear it.
    pub fn submit_input(&mut self) -> Option<UserCommand> {
        let text = self.input.trim().to_string();
        self.input.clear();

        // Match commands exactly (or followed by a space) so `/attachfoo` and
        // `/confirmation` fall through to a normal message rather than mis-firing.
        if text == "/attach" || text.starts_with("/attach ") {
            let raw = text["/attach".len()..].trim();
            if raw.is_empty() {
                self.sys("usage: /attach <path-to-image-or-pdf>", SysKind::Note);
            } else {
                let p = expand_path(raw);
                if p.is_file() {
                    let name = p
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or("file")
                        .to_string();
                    self.pending_attachments.push(p);
                    self.sys(
                        format!("📎 {name} — sent with your next message"),
                        SysKind::Note,
                    );
                } else {
                    self.sys(format!("not found: {raw}"), SysKind::Err);
                }
            }
            return None;
        }
        if text == "/confirm" || text.starts_with("/confirm ") {
            let arg = text["/confirm".len()..].trim();
            return Some(UserCommand::ConfirmExecution {
                executor: self.resolve_executor(arg),
            });
        }
        if text == "/reject" {
            return Some(UserCommand::Reject);
        }
        if text == "/quit" {
            self.should_quit = true;
            return Some(UserCommand::Shutdown);
        }
        // A normal message — or attachments on their own.
        if text.is_empty() && self.pending_attachments.is_empty() {
            // At the gate, a bare Enter accepts the recommended executor.
            if self.awaiting {
                if let Some(rec) = &self.recommended {
                    return Some(UserCommand::ConfirmExecution {
                        executor: rec.clone(),
                    });
                }
            }
            return None;
        }
        let attachments = std::mem::take(&mut self.pending_attachments);
        Some(UserCommand::InjectNote {
            agent: Uuid::nil(),
            text,
            attachments,
        })
    }

    /// Resolve a `/confirm` argument to an executor label. Accepts a 1-based
    /// number (matching the gate's picker), a tool name, or empty (the
    /// recommended one).
    fn resolve_executor(&self, arg: &str) -> String {
        if arg.is_empty() {
            return self.recommended.clone().unwrap_or_default();
        }
        if let Ok(n) = arg.parse::<usize>() {
            if n >= 1 {
                if let Some(label) = self.candidates.get(n - 1) {
                    return label.clone();
                }
            }
        }
        // Accept the displayed pretty name too (e.g. "Claude Code" → "claude").
        for key in &self.candidates {
            if pretty(key).eq_ignore_ascii_case(arg) {
                return key.clone();
            }
        }
        arg.to_string()
    }

    /// The footer hint line. At the gate it becomes an executor picker listing
    /// the connected tools as numbered choices; otherwise it's the chat help.
    pub fn footer_line(&self) -> Line<'static> {
        if !self.awaiting {
            return Line::from(Span::styled(
                "type to talk · /attach <file> · /confirm [agent] · /reject · /quit",
                Style::default().fg(FAINT),
            ));
        }
        let mut spans = vec![Span::styled(
            "pick executor: ",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        )];
        for (i, key) in self.candidates.iter().enumerate() {
            if i > 0 {
                spans.push(Span::raw("  "));
            }
            spans.push(Span::styled(
                format!("[{}] ", i + 1),
                Style::default().fg(FAINT),
            ));
            spans.push(Span::styled(
                pretty(key),
                Style::default()
                    .fg(color_for(key))
                    .add_modifier(Modifier::BOLD),
            ));
        }
        let tail = match &self.recommended {
            Some(r) => format!("  ·  Enter = {}  ·  /confirm <n>  ·  /reject", pretty(r)),
            None => "  ·  /confirm <n>  ·  /reject".to_string(),
        };
        spans.push(Span::styled(tail, Style::default().fg(DIM)));
        Line::from(spans)
    }

    /// Plain-text transcript for handing a plan off to a live executor pane.
    pub fn transcript_text(&self) -> String {
        let mut out = String::new();
        for b in &self.blocks {
            match b {
                Block::Agent { label, role, text } => {
                    let role = role
                        .as_deref()
                        .map(|r| format!(" ({r})"))
                        .unwrap_or_default();
                    out.push_str(&format!("{}{}:\n{}\n\n", pretty(label), role, text.trim()));
                }
                Block::You(text) => {
                    out.push_str(&format!("You:\n{}\n\n", text.trim()));
                }
                Block::Sys(text, _) => {
                    out.push_str(&format!("[Tales]\n{}\n\n", text.trim()));
                }
            }
        }
        for (agent, text) in &self.partial {
            let label = self.label_of(agent);
            if !text.trim().is_empty() {
                out.push_str(&format!(
                    "{} (in progress):\n{}\n\n",
                    pretty(&label),
                    text.trim()
                ));
            }
        }
        out.trim().to_string()
    }

    /// Executor prompt used when the terminal workspace sends the agreed plan to
    /// a live Claude/Codex pane instead of letting the core run headlessly.
    pub fn executor_handoff_prompt(&self) -> String {
        format!(
            "You are now EXECUTING the plan the team agreed on.\n\
             Task: {}\n\n\
             Discussion and plan:\n{}\n\n\
             Implement it now. Use your file-writing tool (Write/Edit) to create \
             every file the plan calls for. When finished, briefly summarize \
             what you wrote.\n",
            self.task,
            self.transcript_text()
        )
    }

    /// Build the Warp-style block render lines (transcript + live partials).
    pub fn render_lines(&self, width: usize) -> Vec<Line<'static>> {
        let width = width.max(10);
        let mut out: Vec<Line<'static>> = Vec::new();

        for b in &self.blocks {
            match b {
                Block::Agent { label, role, text } => {
                    agent_block(
                        &mut out,
                        color_for(label),
                        &pretty(label),
                        role.as_deref(),
                        text,
                        width,
                    );
                }
                Block::You(text) => agent_block(&mut out, YOU, "You", None, text, width),
                Block::Sys(text, kind) => sys_block(&mut out, text, *kind, width),
            }
        }

        // Live partials (one active at a time in the discussion).
        for (agent, text) in &self.partial {
            let label = self.label_of(agent);
            let role = self.roles.get(agent).cloned();
            let color = color_for(&label);
            header(&mut out, color, &pretty(&label), role.as_deref());
            if text.is_empty() {
                out.push(indent_line("thinking…", DIM));
            } else {
                body(&mut out, text, width);
            }
            out.push(Line::from(""));
        }
        out
    }
}

// ── rendering helpers ────────────────────────────────────────────────────────

fn header(out: &mut Vec<Line<'static>>, color: Color, name: &str, role: Option<&str>) {
    let mut spans = vec![
        Span::styled("▌ ", Style::default().fg(color)),
        Span::styled(
            name.to_string(),
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ),
    ];
    if let Some(r) = role {
        spans.push(Span::raw("  "));
        spans.push(Span::styled(r.to_uppercase(), Style::default().fg(FAINT)));
    }
    out.push(Line::from(spans));
}

fn indent_line(s: &str, color: Color) -> Line<'static> {
    Line::from(vec![
        Span::raw("  "),
        Span::styled(s.to_string(), Style::default().fg(color)),
    ])
}

fn body(out: &mut Vec<Line<'static>>, text: &str, width: usize) {
    let iw = width.saturating_sub(2).max(8);
    for segment in text.split('\n') {
        if segment.is_empty() {
            out.push(Line::from(""));
            continue;
        }
        let chars: Vec<char> = segment.chars().collect();
        for chunk in chars.chunks(iw) {
            out.push(indent_line(&chunk.iter().collect::<String>(), TEXT));
        }
    }
}

fn agent_block(
    out: &mut Vec<Line<'static>>,
    color: Color,
    name: &str,
    role: Option<&str>,
    text: &str,
    width: usize,
) {
    header(out, color, name, role);
    body(out, text, width);
    out.push(Line::from(""));
}

fn sys_block(out: &mut Vec<Line<'static>>, text: &str, kind: SysKind, width: usize) {
    let (color, bullet) = match kind {
        SysKind::Note => (DIM, "· "),
        SysKind::Rec => (ACCENT, "★ "),
        SysKind::Err => (ERRC, "✗ "),
    };
    for (i, seg) in text.split('\n').enumerate() {
        let prefix = if i == 0 { bullet } else { "  " };
        let chars: Vec<char> = seg.chars().collect();
        let iw = width.saturating_sub(2).max(8);
        if chars.is_empty() {
            continue;
        }
        for (j, chunk) in chars.chunks(iw).enumerate() {
            let p = if i == 0 && j == 0 { prefix } else { "  " };
            out.push(Line::from(vec![
                Span::styled(p.to_string(), Style::default().fg(color)),
                Span::styled(chunk.iter().collect::<String>(), Style::default().fg(color)),
            ]));
        }
    }
}

fn expand_path(raw: &str) -> PathBuf {
    if let Some(rest) = raw.strip_prefix("~/") {
        if let Ok(home) = std::env::var("HOME") {
            return PathBuf::from(home).join(rest);
        }
    }
    PathBuf::from(raw)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text_of(lines: &[Line<'static>]) -> String {
        lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn renders_streamed_then_finalized_messages() {
        let agent = Uuid::new_v4();
        let mut app = App::new("build a thing".into());
        app.apply(OrchestratorEvent::AgentSpawned {
            agent,
            label: "claude".into(),
            session_id: String::new(),
        });
        app.apply(OrchestratorEvent::Token {
            agent,
            text: "hello ".into(),
        });
        app.apply(OrchestratorEvent::Token {
            agent,
            text: "world".into(),
        });
        let mid = text_of(&app.render_lines(80));
        assert!(mid.contains("Claude Code"));
        assert!(mid.contains("hello world"));

        app.apply(OrchestratorEvent::Message {
            agent,
            text: "hello world, done".into(),
        });
        let done = text_of(&app.render_lines(80));
        assert!(done.contains("hello world, done"));
    }

    #[test]
    fn role_badge_from_turn_started() {
        let agent = Uuid::new_v4();
        let mut app = App::new("t".into());
        app.apply(OrchestratorEvent::AgentSpawned {
            agent,
            label: "codex".into(),
            session_id: String::new(),
        });
        app.apply(OrchestratorEvent::TurnStarted {
            agent,
            role: "Critic".into(),
        });
        app.apply(OrchestratorEvent::Message {
            agent,
            text: "a critique".into(),
        });
        let r = text_of(&app.render_lines(80));
        assert!(r.contains("Codex"));
        assert!(r.contains("CRITIC"));
    }

    #[test]
    fn gate_executor_resolution() {
        let mut app = App::new("t".into());
        app.set_candidates(vec!["claude".into(), "codex".into()]);
        app.recommended = Some("claude".into());

        // Numeric pick → the nth candidate.
        app.input = "/confirm 2".into();
        match app.submit_input() {
            Some(UserCommand::ConfirmExecution { executor }) => assert_eq!(executor, "codex"),
            other => panic!("expected confirm codex, got {other:?}"),
        }
        // Pretty name pick → underlying key.
        app.input = "/confirm Codex".into();
        match app.submit_input() {
            Some(UserCommand::ConfirmExecution { executor }) => assert_eq!(executor, "codex"),
            other => panic!("expected confirm codex, got {other:?}"),
        }
        // A near-miss command is NOT a confirm — it's a normal message.
        app.input = "/confirmation needed".into();
        match app.submit_input() {
            Some(UserCommand::InjectNote { text, .. }) => assert_eq!(text, "/confirmation needed"),
            other => panic!("expected inject note, got {other:?}"),
        }
        // At the gate, a bare Enter accepts the recommendation.
        app.awaiting = true;
        app.input = String::new();
        match app.submit_input() {
            Some(UserCommand::ConfirmExecution { executor }) => assert_eq!(executor, "claude"),
            other => panic!("expected confirm claude, got {other:?}"),
        }
    }

    #[test]
    fn commands_map_correctly() {
        let mut app = App::new("t".into());
        app.recommended = Some("claude".into());
        app.input = "/confirm".into();
        match app.submit_input() {
            Some(UserCommand::ConfirmExecution { executor }) => assert_eq!(executor, "claude"),
            other => panic!("expected confirm, got {other:?}"),
        }
        app.input = "/reject".into();
        assert!(matches!(app.submit_input(), Some(UserCommand::Reject)));
        app.input = "please add tests".into();
        match app.submit_input() {
            Some(UserCommand::InjectNote { text, .. }) => assert_eq!(text, "please add tests"),
            other => panic!("expected inject note, got {other:?}"),
        }
        app.input = "/quit".into();
        assert!(matches!(app.submit_input(), Some(UserCommand::Shutdown)));
        assert!(app.should_quit);
    }
}
