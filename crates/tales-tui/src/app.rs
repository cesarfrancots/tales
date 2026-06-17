//! TUI application state: the chat transcript, streaming partials, and the
//! mapping from user input to [`UserCommand`]s.

use std::collections::HashMap;

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Line;
use tales_core::event::{OrchestratorEvent, UserCommand};
use uuid::Uuid;

/// Who said a line.
pub enum Speaker {
    Agent(String),
    You,
    System,
}

pub struct ChatLine {
    pub speaker: Speaker,
    pub text: String,
}

/// All UI state.
pub struct App {
    pub task: String,
    pub phase: String,
    pub lines: Vec<ChatLine>,
    /// In-progress streamed text per agent: agent -> (label, partial).
    partial: HashMap<Uuid, (String, String)>,
    labels: HashMap<Uuid, String>,
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
            lines: Vec::new(),
            partial: HashMap::new(),
            labels: HashMap::new(),
            input: String::new(),
            recommended: None,
            awaiting: false,
            should_quit: false,
        }
    }

    fn sys(&mut self, text: impl Into<String>) {
        self.lines.push(ChatLine {
            speaker: Speaker::System,
            text: text.into(),
        });
    }

    fn label_of(&self, agent: &Uuid) -> String {
        self.labels.get(agent).cloned().unwrap_or_else(|| "?".to_string())
    }

    /// Fold a bus event into the chat state.
    pub fn apply(&mut self, ev: OrchestratorEvent) {
        match ev {
            OrchestratorEvent::AgentSpawned { agent, label, .. } => {
                self.labels.insert(agent, label.clone());
                self.sys(format!("● {label} joined the conversation"));
            }
            OrchestratorEvent::Token { agent, text } => {
                let label = self.label_of(&agent);
                self.partial
                    .entry(agent)
                    .or_insert((label, String::new()))
                    .1
                    .push_str(&text);
            }
            OrchestratorEvent::Message { agent, text } => {
                let label = self.label_of(&agent);
                self.partial.remove(&agent);
                self.lines.push(ChatLine {
                    speaker: Speaker::Agent(label),
                    text,
                });
            }
            OrchestratorEvent::UserMessage { text } => {
                self.lines.push(ChatLine {
                    speaker: Speaker::You,
                    text,
                });
            }
            // The TUI already shows the "speaking as" Log line; nothing to do.
            OrchestratorEvent::TurnStarted { .. } => {}
            OrchestratorEvent::ToolActivity { agent, summary } => {
                let label = self.label_of(&agent);
                self.sys(format!("⚙ {label}: {summary}"));
            }
            OrchestratorEvent::TurnComplete { agent, cost_usd } => {
                self.partial.remove(&agent);
                if let Some(c) = cost_usd {
                    self.sys(format!("   (turn cost ${c:.4})"));
                }
            }
            OrchestratorEvent::PhaseChanged { phase } => {
                self.phase = phase.clone();
                self.sys(format!("— phase: {phase} —"));
                if phase != "awaitingconfirmation" {
                    self.awaiting = false;
                }
            }
            OrchestratorEvent::RecommendationReady { executor, rationale } => {
                self.recommended = Some(executor.clone());
                self.lines.push(ChatLine {
                    speaker: Speaker::System,
                    text: format!("★ Recommended executor: {executor}\n{rationale}"),
                });
            }
            OrchestratorEvent::AwaitingConfirmation { prompt } => {
                self.awaiting = true;
                self.sys(format!(
                    "⏸ {prompt}\n   → /confirm  ·  /confirm <agent>  ·  /reject"
                ));
            }
            OrchestratorEvent::AgentExited { agent, code } => {
                let label = self.label_of(&agent);
                self.sys(format!("● {label} exited ({code:?})"));
            }
            OrchestratorEvent::Log { level, msg } => {
                self.sys(format!("[{level}] {msg}"));
            }
            OrchestratorEvent::Fatal { msg } => {
                self.sys(format!("✗ fatal: {msg}"));
            }
        }
    }

    /// Interpret the current input line on Enter and clear it. Returns the
    /// command to send to the engine, if any.
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
            self.sys(format!("you → confirm executor: {executor}"));
            return Some(UserCommand::ConfirmExecution { executor });
        }
        if text == "/reject" {
            self.sys("you → reject the plan");
            return Some(UserCommand::Reject);
        }
        if text == "/quit" {
            self.should_quit = true;
            return Some(UserCommand::Shutdown);
        }
        // Plain text is a chat interjection. The engine echoes it back as a
        // UserMessage, so we don't add the line here (avoids duplication).
        Some(UserCommand::InjectNote {
            agent: Uuid::nil(),
            text,
        })
    }

    /// Build styled, width-wrapped render lines (transcript + live partials).
    pub fn render_lines(&self, width: usize) -> Vec<Line<'static>> {
        let width = width.max(8);
        let mut out: Vec<Line<'static>> = Vec::new();

        for cl in &self.lines {
            let (prefix, style) = match &cl.speaker {
                Speaker::Agent(l) => (format!("{l}: "), style_for(l)),
                Speaker::You => (
                    "you: ".to_string(),
                    Style::default().fg(Color::Green).add_modifier(Modifier::BOLD),
                ),
                Speaker::System => (String::new(), Style::default().fg(Color::DarkGray)),
            };
            push_wrapped(&mut out, &prefix, &cl.text, style, width);
        }

        // Live, still-streaming text.
        for (label, text) in self.partial.values() {
            if text.is_empty() {
                continue;
            }
            push_wrapped(&mut out, &format!("{label} …: "), text, style_for(label), width);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tales_core::event::OrchestratorEvent;

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

        app.apply(OrchestratorEvent::AgentSpawned {
            agent,
            label: "claude".into(),
            session_id: String::new(),
        });
        // Streaming tokens show as a live partial...
        app.apply(OrchestratorEvent::Token { agent, text: "hello ".into() });
        app.apply(OrchestratorEvent::Token { agent, text: "world".into() });
        let mid = text_of(&app.render_lines(80));
        assert!(mid.contains("claude"));
        assert!(mid.contains("hello world"));

        // ...then a final Message replaces the partial.
        app.apply(OrchestratorEvent::Message {
            agent,
            text: "hello world, done".into(),
        });
        let done = text_of(&app.render_lines(80));
        assert!(done.contains("hello world, done"));
    }

    #[test]
    fn confirm_and_reject_map_to_commands() {
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

    #[test]
    fn shows_recommendation_and_user_message() {
        let mut app = App::new("t".into());
        app.apply(OrchestratorEvent::RecommendationReady {
            executor: "codex".into(),
            rationale: "codex is better here".into(),
        });
        app.apply(OrchestratorEvent::UserMessage { text: "go with codex".into() });
        let rendered = text_of(&app.render_lines(80));
        assert!(rendered.contains("Recommended executor: codex"));
        assert!(rendered.contains("you: go with codex"));
        assert_eq!(app.recommended.as_deref(), Some("codex"));
    }
}

fn style_for(label: &str) -> Style {
    let color = match label.to_lowercase().as_str() {
        "claude" => Color::Cyan,
        "codex" => Color::Magenta,
        _ => Color::Yellow,
    };
    Style::default().fg(color)
}

/// Append `prefix + text` to `out` as width-wrapped, uniformly-styled lines,
/// honoring embedded newlines.
fn push_wrapped(out: &mut Vec<Line<'static>>, prefix: &str, text: &str, style: Style, width: usize) {
    let full = format!("{prefix}{text}");
    for segment in full.split('\n') {
        if segment.is_empty() {
            out.push(Line::from(""));
            continue;
        }
        let chars: Vec<char> = segment.chars().collect();
        for chunk in chars.chunks(width) {
            let s: String = chunk.iter().collect();
            out.push(Line::styled(s, style));
        }
    }
}
