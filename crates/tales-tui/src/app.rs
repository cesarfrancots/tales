//! TUI state + Warp-style block rendering.
//!
//! Each agent turn renders as a "block": a colored left bar + name + role badge
//! header, then an indented body. Minimal chrome, restrained palette, monospace
//! (a terminal is monospace by nature) — clean and lightweight.

use std::collections::HashMap;

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use tales_core::event::{OrchestratorEvent, UserCommand};
use uuid::Uuid;

// ── palette (truecolor; degrades gracefully) ─────────────────────────────────
const TEXT: Color = Color::Rgb(0xd2, 0xd8, 0xe2);
const DIM: Color = Color::Rgb(0x6b, 0x74, 0x83);
const FAINT: Color = Color::Rgb(0x44, 0x4d, 0x5a);
const CLAUDE: Color = Color::Rgb(0x5c, 0xb0, 0xff);
const CODEX: Color = Color::Rgb(0xc0, 0x8c, 0xff);
const YOU: Color = Color::Rgb(0x7e, 0xe0, 0xa3);
const ACCENT: Color = Color::Rgb(0x2d, 0xd4, 0xbf);
const ERRC: Color = Color::Rgb(0xff, 0x7a, 0x85);

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
        }
    }

    fn sys(&mut self, text: impl Into<String>, kind: SysKind) {
        self.blocks.push(Block::Sys(text.into(), kind));
    }

    fn label_of(&self, agent: &Uuid) -> String {
        self.labels.get(agent).cloned().unwrap_or_else(|| "?".to_string())
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
                    self.sys(format!("{} · ${c:.4}", pretty(&self.label_of(&agent))), SysKind::Note);
                }
            }
            OrchestratorEvent::PhaseChanged { phase } => {
                if phase != "awaitingconfirmation" {
                    self.awaiting = false;
                }
                self.phase = phase;
            }
            OrchestratorEvent::RecommendationReady { executor, rationale } => {
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
                self.sys(format!("{} exited ({code:?})", pretty(&self.label_of(&agent))), SysKind::Note);
            }
            OrchestratorEvent::Log { level, msg } => {
                if msg.contains(" speaking as ") {
                    return; // shown as the thinking block instead
                }
                let kind = if level == "error" { SysKind::Err } else { SysKind::Note };
                self.sys(msg, kind);
            }
            OrchestratorEvent::Fatal { msg } => self.sys(format!("✗ {msg}"), SysKind::Err),
        }
    }

    /// Interpret the input line on Enter and clear it.
    pub fn submit_input(&mut self) -> Option<UserCommand> {
        let text = self.input.trim().to_string();
        self.input.clear();
        if text.is_empty() {
            return None;
        }
        if let Some(rest) = text.strip_prefix("/confirm") {
            let arg = rest.trim();
            let executor = if arg.is_empty() {
                self.recommended.clone().unwrap_or_default()
            } else {
                arg.to_string()
            };
            return Some(UserCommand::ConfirmExecution { executor });
        }
        if text == "/reject" {
            return Some(UserCommand::Reject);
        }
        if text == "/quit" {
            self.should_quit = true;
            return Some(UserCommand::Shutdown);
        }
        Some(UserCommand::InjectNote {
            agent: Uuid::nil(),
            text,
        })
    }

    /// Build the Warp-style block render lines (transcript + live partials).
    pub fn render_lines(&self, width: usize) -> Vec<Line<'static>> {
        let width = width.max(10);
        let mut out: Vec<Line<'static>> = Vec::new();

        for b in &self.blocks {
            match b {
                Block::Agent { label, role, text } => {
                    agent_block(&mut out, color_for(label), &pretty(label), role.as_deref(), text, width);
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
        Span::styled(name.to_string(), Style::default().fg(color).add_modifier(Modifier::BOLD)),
    ];
    if let Some(r) = role {
        spans.push(Span::raw("  "));
        spans.push(Span::styled(r.to_uppercase(), Style::default().fg(FAINT)));
    }
    out.push(Line::from(spans));
}

fn indent_line(s: &str, color: Color) -> Line<'static> {
    Line::from(vec![Span::raw("  "), Span::styled(s.to_string(), Style::default().fg(color))])
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

fn agent_block(out: &mut Vec<Line<'static>>, color: Color, name: &str, role: Option<&str>, text: &str, width: usize) {
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

fn pretty(label: &str) -> String {
    match label.to_lowercase().as_str() {
        "claude" => "Claude Code".to_string(),
        "codex" => "Codex".to_string(),
        other => {
            let mut c = other.chars();
            match c.next() {
                Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
                None => other.to_string(),
            }
        }
    }
}

fn color_for(label: &str) -> Color {
    match label.to_lowercase().as_str() {
        "claude" => CLAUDE,
        "codex" => CODEX,
        _ => Color::Rgb(0xff, 0xc7, 0x77),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text_of(lines: &[Line<'static>]) -> String {
        lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn renders_streamed_then_finalized_messages() {
        let agent = Uuid::new_v4();
        let mut app = App::new("build a thing".into());
        app.apply(OrchestratorEvent::AgentSpawned { agent, label: "claude".into(), session_id: String::new() });
        app.apply(OrchestratorEvent::Token { agent, text: "hello ".into() });
        app.apply(OrchestratorEvent::Token { agent, text: "world".into() });
        let mid = text_of(&app.render_lines(80));
        assert!(mid.contains("Claude Code"));
        assert!(mid.contains("hello world"));

        app.apply(OrchestratorEvent::Message { agent, text: "hello world, done".into() });
        let done = text_of(&app.render_lines(80));
        assert!(done.contains("hello world, done"));
    }

    #[test]
    fn role_badge_from_turn_started() {
        let agent = Uuid::new_v4();
        let mut app = App::new("t".into());
        app.apply(OrchestratorEvent::AgentSpawned { agent, label: "codex".into(), session_id: String::new() });
        app.apply(OrchestratorEvent::TurnStarted { agent, role: "Critic".into() });
        app.apply(OrchestratorEvent::Message { agent, text: "a critique".into() });
        let r = text_of(&app.render_lines(80));
        assert!(r.contains("Codex"));
        assert!(r.contains("CRITIC"));
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
