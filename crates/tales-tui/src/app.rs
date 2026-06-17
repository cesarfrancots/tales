//! TUI state + Warp-style block rendering.
//!
//! Each agent turn renders as a "block": a colored left bar + name + role badge
//! header, then an indented body. Minimal chrome, restrained palette, monospace
//! (a terminal is monospace by nature) — clean and lightweight.
//!
//! Streaming is *smoothed*: incoming text (whether Claude's bursty token deltas
//! or Codex's whole-message dump) is buffered and revealed at a steady, catch-up
//! rate by [`App::tick`], so the conversation types out evenly instead of
//! freezing then dumping. While an agent is thinking with no output yet, an
//! animated spinner makes the wait read as "working", not "frozen".

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Instant;

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use tales_core::event::{OrchestratorEvent, UserCommand};
use uuid::Uuid;

use crate::theme::{color_for, pretty, ACCENT, DIM, ERRC, FAINT, TEXT, YOU};

/// Braille spinner frames for the "thinking" indicator.
const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
/// Seconds to drain whatever text is currently buffered — small so reveal stays
/// snappy. Because the rate scales with the backlog, a big dump clears in about
/// this long while a trickle reveals at [`MIN_CPS`].
const CATCHUP_SECS: f64 = 0.18;
/// Floor reveal speed (chars/sec) so a slow trickle still types out visibly.
const MIN_CPS: f64 = 160.0;
/// Seconds between spinner frames.
const SPIN_INTERVAL: f64 = 0.09;

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

/// An in-progress agent turn, revealed progressively for a smooth typewriter
/// feel. `full` is everything received; `revealed` is how much is shown so far.
#[derive(Default)]
struct Stream {
    full: String,
    revealed: usize,
    /// The turn has finished (TurnComplete seen); finalize once fully revealed.
    done: bool,
    /// Cost note to emit when the block finalizes (keeps ordering correct).
    cost: Option<f64>,
}

/// All UI state.
pub struct App {
    pub task: String,
    pub phase: String,
    blocks: Vec<Block>,
    /// In-progress streamed text per agent, revealed gradually.
    partial: HashMap<Uuid, Stream>,
    /// Stable render order for partials (a HashMap iterates arbitrarily, and two
    /// partials can briefly coexist while one finishes revealing).
    partial_order: Vec<Uuid>,
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
    /// Animation clock + spinner frame, advanced by [`App::tick`].
    last_tick: Option<Instant>,
    spin_accum: f64,
    spinner: usize,
}

impl App {
    pub fn new(task: String) -> Self {
        Self {
            task,
            phase: "idle".to_string(),
            blocks: Vec::new(),
            partial: HashMap::new(),
            partial_order: Vec::new(),
            labels: HashMap::new(),
            roles: HashMap::new(),
            input: String::new(),
            recommended: None,
            awaiting: false,
            should_quit: false,
            pending_attachments: Vec::new(),
            candidates: Vec::new(),
            last_tick: None,
            spin_accum: 0.0,
            spinner: 0,
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

    /// Get (creating if needed) the live stream for an agent, preserving order.
    fn stream_mut(&mut self, agent: Uuid) -> &mut Stream {
        use std::collections::hash_map::Entry;
        match self.partial.entry(agent) {
            Entry::Occupied(e) => e.into_mut(),
            Entry::Vacant(e) => {
                self.partial_order.push(agent);
                e.insert(Stream::default())
            }
        }
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
                self.stream_mut(agent); // empty stream = "thinking" placeholder
            }
            OrchestratorEvent::Token { agent, text } => {
                self.stream_mut(agent).full.push_str(&text);
            }
            OrchestratorEvent::Message { agent, text } => {
                let s = self.stream_mut(agent);
                s.full = text;
                let len = s.full.chars().count();
                if s.revealed > len {
                    s.revealed = len;
                }
            }
            OrchestratorEvent::UserMessage { text } => {
                self.blocks.push(Block::You(text));
            }
            OrchestratorEvent::ToolActivity { agent, summary } => {
                let label = self.label_of(&agent);
                self.sys(format!("⚙ {} · {summary}", pretty(&label)), SysKind::Note);
            }
            OrchestratorEvent::TurnComplete { agent, cost_usd } => {
                // Mark done + stash the cost; the block (and its cost note) are
                // emitted together when the reveal catches up, preserving order.
                if self.partial.contains_key(&agent) {
                    let s = self.stream_mut(agent);
                    s.done = true;
                    s.cost = cost_usd;
                } else if let Some(c) = cost_usd {
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
                // End the agent's live stream so it finalizes instead of
                // animating a "thinking…" spinner forever for a dead agent.
                if let Some(s) = self.partial.get_mut(&agent) {
                    s.done = true;
                }
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
            OrchestratorEvent::Fatal { msg } => {
                // A fatal ends the run; mark every live stream done so they
                // finalize and the animation loop can settle (Fatal carries no
                // agent id, so we can't target one).
                for s in self.partial.values_mut() {
                    s.done = true;
                }
                self.sys(format!("✗ {msg}"), SysKind::Err);
            }
        }
    }

    /// Advance reveal + spinner animation using the real clock. Call once per
    /// frame. Cheap and a no-op when nothing is streaming.
    pub fn tick(&mut self) {
        let now = Instant::now();
        let elapsed = match self.last_tick.replace(now) {
            // Clamp so a long pause (no frames) doesn't dump everything at once.
            Some(prev) => (now - prev).as_secs_f64().min(0.1),
            None => return,
        };
        self.advance(elapsed);
    }

    /// Reveal more buffered text and tick the spinner. Pulled out of [`tick`] so
    /// tests/snapshots can drive it deterministically. Returns whether anything
    /// animated.
    pub(crate) fn advance(&mut self, elapsed_secs: f64) -> bool {
        // Spinner.
        self.spin_accum += elapsed_secs;
        while self.spin_accum >= SPIN_INTERVAL {
            self.spin_accum -= SPIN_INTERVAL;
            self.spinner = self.spinner.wrapping_add(1);
        }

        // Reveal, oldest stream first.
        let ids: Vec<Uuid> = self.partial_order.clone();
        for id in &ids {
            if let Some(s) = self.partial.get_mut(id) {
                let len = s.full.chars().count();
                if s.revealed < len {
                    let backlog = (len - s.revealed) as f64;
                    let cps = (backlog / CATCHUP_SECS).max(MIN_CPS);
                    let budget = (cps * elapsed_secs).ceil() as usize;
                    s.revealed = (s.revealed + budget).min(len);
                }
            }
        }
        self.finalize_ready();
        self.is_animating()
    }

    /// Move fully-revealed, finished streams into the permanent block list
    /// (block first, then its cost note), preserving conversation order.
    fn finalize_ready(&mut self) {
        let ready: Vec<Uuid> = self
            .partial_order
            .iter()
            .copied()
            .filter(|id| {
                self.partial
                    .get(id)
                    .map(|s| s.done && s.revealed >= s.full.chars().count())
                    .unwrap_or(false)
            })
            .collect();
        for id in ready {
            let Some(s) = self.partial.remove(&id) else {
                continue;
            };
            self.partial_order.retain(|x| *x != id);
            let label = self.label_of(&id);
            let role = self.roles.get(&id).cloned();
            if !s.full.trim().is_empty() {
                self.blocks.push(Block::Agent {
                    label: label.clone(),
                    role,
                    text: s.full,
                });
            }
            if let Some(c) = s.cost {
                self.sys(format!("{} · ${c:.4}", pretty(&label)), SysKind::Note);
            }
        }
    }

    /// Whether any stream is still revealing or thinking (so the render loop
    /// should keep ticking at the smooth animation rate).
    pub fn is_animating(&self) -> bool {
        self.partial.values().any(|s| {
            let len = s.full.chars().count();
            s.revealed < len || (len == 0 && !s.done)
        })
    }

    /// At the gate, a bare digit key picks that executor (1-based) — the easiest
    /// way to choose. Only fires when awaiting and the input line is empty, so it
    /// never eats a digit the user is typing into a message.
    pub fn gate_pick(&self, c: char) -> Option<UserCommand> {
        if !self.awaiting || !self.input.is_empty() {
            return None;
        }
        let n = c.to_digit(10)? as usize;
        if n >= 1 {
            if let Some(label) = self.candidates.get(n - 1) {
                return Some(UserCommand::ConfirmExecution {
                    executor: label.clone(),
                });
            }
        }
        None
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
        for agent in &self.partial_order {
            if let Some(s) = self.partial.get(agent) {
                let label = self.label_of(agent);
                if !s.full.trim().is_empty() {
                    out.push_str(&format!(
                        "{} (in progress):\n{}\n\n",
                        pretty(&label),
                        s.full.trim()
                    ));
                }
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

    /// Build the Warp-style block render lines (transcript + live partials + the
    /// action banner at the gate).
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
                Block::You(text) => you_block(&mut out, text, width),
                Block::Sys(text, kind) => sys_block(&mut out, text, *kind, width),
            }
        }

        // Live partials, revealed progressively (oldest first).
        for agent in &self.partial_order {
            let Some(s) = self.partial.get(agent) else {
                continue;
            };
            let label = self.label_of(agent);
            let role = self.roles.get(agent).cloned();
            let color = color_for(&label);
            header(&mut out, color, &pretty(&label), role.as_deref());
            let len = s.full.chars().count();
            if len == 0 {
                let spin = SPINNER[self.spinner % SPINNER.len()];
                out.push(indent_line(&format!("{spin} thinking…"), DIM));
            } else {
                let shown: String = s.full.chars().take(s.revealed).collect();
                body(&mut out, &shown, width);
            }
            out.push(Line::from(""));
        }

        // The gate: a prominent "action needed" banner with numbered choices.
        if self.awaiting {
            action_banner(&mut out, &self.candidates, self.recommended.as_deref());
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

/// Human messages get a full green left-gutter on every line, so the human's
/// voice stands clearly apart from the AIs (whose only the header bar is tinted).
fn you_block(out: &mut Vec<Line<'static>>, text: &str, width: usize) {
    out.push(Line::from(vec![
        Span::styled("▐ ", Style::default().fg(YOU).add_modifier(Modifier::BOLD)),
        Span::styled("You", Style::default().fg(YOU).add_modifier(Modifier::BOLD)),
    ]));
    let iw = width.saturating_sub(2).max(8);
    for segment in text.split('\n') {
        if segment.is_empty() {
            out.push(Line::from(Span::styled("▐", Style::default().fg(YOU))));
            continue;
        }
        let chars: Vec<char> = segment.chars().collect();
        for chunk in chars.chunks(iw) {
            out.push(Line::from(vec![
                Span::styled("▐ ", Style::default().fg(YOU)),
                Span::styled(chunk.iter().collect::<String>(), Style::default().fg(TEXT)),
            ]));
        }
    }
    out.push(Line::from(""));
}

/// The gate affordance: a clear "action needed" banner with numbered, pickable
/// executor choices and the recommendation marked.
fn action_banner(out: &mut Vec<Line<'static>>, candidates: &[String], recommended: Option<&str>) {
    out.push(Line::from(""));
    out.push(Line::from(vec![
        Span::styled(
            "▌ ACTION NEEDED",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            "  —  pick who executes the plan",
            Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
        ),
    ]));
    for (i, key) in candidates.iter().enumerate() {
        let is_rec = recommended.is_some_and(|r| r.eq_ignore_ascii_case(key));
        let marker = if is_rec { "▸" } else { " " };
        let mut spans = vec![
            Span::styled(format!("  {marker} "), Style::default().fg(ACCENT)),
            Span::styled(
                format!("{}  ", i + 1),
                Style::default().fg(FAINT).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                pretty(key),
                Style::default()
                    .fg(color_for(key))
                    .add_modifier(Modifier::BOLD),
            ),
        ];
        if is_rec {
            spans.push(Span::styled(
                "   ← recommended",
                Style::default().fg(YOU).add_modifier(Modifier::BOLD),
            ));
        }
        out.push(Line::from(spans));
    }
    out.push(Line::from(Span::styled(
        "  Enter accept · press 1-9 to pick · /reject to decline",
        Style::default().fg(DIM),
    )));
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
        // Reveal the buffered tokens (a generous slice fully reveals).
        app.advance(10.0);
        let mid = text_of(&app.render_lines(80));
        assert!(mid.contains("Claude Code"));
        assert!(mid.contains("hello world"));

        app.apply(OrchestratorEvent::Message {
            agent,
            text: "hello world, done".into(),
        });
        app.apply(OrchestratorEvent::TurnComplete {
            agent,
            cost_usd: None,
        });
        app.advance(10.0); // reveal + finalize into a permanent block
        let done = text_of(&app.render_lines(80));
        assert!(done.contains("hello world, done"));
    }

    #[test]
    fn thinking_spinner_before_output() {
        let agent = Uuid::new_v4();
        let mut app = App::new("t".into());
        app.apply(OrchestratorEvent::AgentSpawned {
            agent,
            label: "codex".into(),
            session_id: String::new(),
        });
        app.apply(OrchestratorEvent::TurnStarted {
            agent,
            role: "Drafter".into(),
        });
        let r = text_of(&app.render_lines(80));
        assert!(r.contains("thinking"), "{r}");
        assert!(app.is_animating());
    }

    #[test]
    fn animation_settles_after_turn_so_the_loop_can_idle() {
        let agent = Uuid::new_v4();
        let mut app = App::new("t".into());
        app.apply(OrchestratorEvent::TurnStarted {
            agent,
            role: "Drafter".into(),
        });
        assert!(app.is_animating(), "thinking should animate");
        app.apply(OrchestratorEvent::Message {
            agent,
            text: "done".into(),
        });
        app.apply(OrchestratorEvent::TurnComplete {
            agent,
            cost_usd: Some(0.01),
        });
        app.advance(10.0); // fully reveal + finalize
        assert!(
            !app.is_animating(),
            "must settle so the render loop stops spinning at 30fps"
        );
        // A turn that produces NO output at all must also settle (not linger).
        let ghost = Uuid::new_v4();
        app.apply(OrchestratorEvent::TurnStarted {
            agent: ghost,
            role: "Critic".into(),
        });
        app.apply(OrchestratorEvent::TurnComplete {
            agent: ghost,
            cost_usd: None,
        });
        app.advance(10.0);
        assert!(
            !app.is_animating(),
            "an empty turn must not animate forever"
        );
    }

    #[test]
    fn agent_exit_without_turn_complete_settles() {
        // A timeout/crash path: TurnStarted (thinking) then only AgentExited,
        // no TurnComplete. The stream must still settle, not spin forever.
        let agent = Uuid::new_v4();
        let mut app = App::new("t".into());
        app.apply(OrchestratorEvent::TurnStarted {
            agent,
            role: "Drafter".into(),
        });
        app.apply(OrchestratorEvent::Token {
            agent,
            text: "partial work".into(),
        });
        app.apply(OrchestratorEvent::AgentExited {
            agent,
            code: Some(1),
        });
        app.advance(10.0);
        assert!(!app.is_animating(), "a dead agent must not animate forever");
        // Any text it did produce is preserved as a permanent block.
        let r = text_of(&app.render_lines(80));
        assert!(r.contains("partial work"), "{r}");
    }

    #[test]
    fn codex_whole_message_reveals_smoothly() {
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
        // Codex dumps the whole message at once.
        let msg = "a".repeat(400);
        app.apply(OrchestratorEvent::Message {
            agent,
            text: msg.clone(),
        });
        // One tiny frame reveals only a slice, not the whole dump.
        app.advance(0.016);
        let partial = text_of(&app.render_lines(120));
        let shown = partial.matches('a').count();
        assert!(shown > 0 && shown < 400, "revealed {shown} of 400");
        // Catch up.
        app.advance(10.0);
        assert!(text_of(&app.render_lines(120)).matches('a').count() == 400);
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
        app.apply(OrchestratorEvent::TurnComplete {
            agent,
            cost_usd: None,
        });
        app.advance(10.0);
        let r = text_of(&app.render_lines(80));
        assert!(r.contains("Codex"));
        assert!(r.contains("CRITIC"));
    }

    #[test]
    fn action_banner_and_digit_pick_at_gate() {
        let mut app = App::new("t".into());
        app.set_candidates(vec!["claude".into(), "codex".into()]);
        app.recommended = Some("claude".into());
        app.awaiting = true;

        let r = text_of(&app.render_lines(80));
        assert!(r.contains("ACTION NEEDED"), "{r}");
        assert!(r.contains("recommended"), "{r}");

        // Digit picks the matching candidate when input is empty.
        match app.gate_pick('2') {
            Some(UserCommand::ConfirmExecution { executor }) => assert_eq!(executor, "codex"),
            other => panic!("expected confirm codex, got {other:?}"),
        }
        // Not at the gate / mid-typing → digit is just a character.
        app.input = "2 cents".into();
        assert!(app.gate_pick('2').is_none());
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
