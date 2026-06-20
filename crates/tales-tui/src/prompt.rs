//! The prompt screen — shown once tools are connected, before the discussion.
//!
//! You see who's connected and that you're in plan mode, then type the task.
//! Submitting it starts the live planning discussion. Pure state + rendering;
//! key handling lives in `main`.

use ratatui::layout::{Alignment, Constraint, Layout};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use crate::app::{input_area_height, input_view_lines};
use crate::theme::{color_for, pretty, ACCENT, DIM, FAINT, TEXT};

/// What the user did on the prompt screen.
pub enum PromptOutcome {
    /// Start the session with this task.
    Start(String),
    /// Go back to the connect screen to change tools, carrying the typed prompt
    /// so it isn't lost on return.
    Back(String),
    /// Quit Tales.
    Quit,
}

/// The "what should they build?" entry screen.
pub struct PromptScreen {
    connected: Vec<String>,
    pub input: String,
    input_scroll: usize,
}

impl PromptScreen {
    pub fn new(connected: &[String], prefill: Option<&str>) -> Self {
        Self {
            connected: connected.to_vec(),
            input: prefill.unwrap_or("").to_string(),
            input_scroll: 0,
        }
    }

    pub fn push(&mut self, c: char) {
        self.input.push(c);
        self.input_scroll = 0;
    }

    pub fn pop(&mut self) {
        self.input.pop();
        self.input_scroll = 0;
    }

    pub fn scroll_up(&mut self) {
        self.input_scroll = self.input_scroll.saturating_add(4);
    }

    pub fn scroll_down(&mut self) {
        self.input_scroll = self.input_scroll.saturating_sub(4);
    }

    pub fn draw(&self, f: &mut Frame) {
        let input_height = input_area_height("❯ ", &self.input, f.area().width, 12);
        let chunks = Layout::vertical([
            Constraint::Length(1), // header
            Constraint::Length(1), // spacer
            Constraint::Length(1), // connected summary
            Constraint::Length(1), // spacer
            Constraint::Length(1), // question
            Constraint::Length(input_height),
            Constraint::Min(0),    // spare room
            Constraint::Length(1), // footer
        ])
        .split(f.area());

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
                Span::styled("· new session", Style::default().fg(DIM)),
            ])),
            chunks[0],
        );
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "plan mode ",
                Style::default().fg(ACCENT),
            )))
            .alignment(Alignment::Right),
            chunks[0],
        );

        // Connected: "Claude Code + Codex"
        let mut summary = vec![Span::styled("connected: ", Style::default().fg(DIM))];
        for (i, key) in self.connected.iter().enumerate() {
            if i > 0 {
                summary.push(Span::styled(" + ", Style::default().fg(FAINT)));
            }
            summary.push(Span::styled(
                pretty(key),
                Style::default()
                    .fg(color_for(key))
                    .add_modifier(Modifier::BOLD),
            ));
        }
        f.render_widget(Paragraph::new(Line::from(summary)), chunks[2]);

        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "What should they plan and build together?",
                Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
            ))),
            chunks[4],
        );

        f.render_widget(
            Paragraph::new(input_view_lines(
                "❯ ",
                &self.input,
                chunks[5].width,
                chunks[5].height,
                self.input_scroll,
            )),
            chunks[5],
        );

        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "Enter start planning · PageUp/PageDown scroll prompt · Esc back · Ctrl-C quit",
                Style::default().fg(FAINT),
            ))),
            chunks[7],
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shows_connected_pretty_names() {
        let p = PromptScreen::new(&["claude".into(), "opencode".into()], Some("ship it"));
        let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 12)).unwrap();
        term.draw(|f| p.draw(f)).unwrap();
        let buf = term.backend().buffer().clone();
        let mut s = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                s.push_str(buf[(x, y)].symbol());
            }
            s.push('\n');
        }
        assert!(s.contains("Claude Code"), "{s}");
        assert!(s.contains("Open Code"), "{s}");
        assert!(s.contains("plan mode"), "{s}");
        assert!(s.contains("ship it"), "prefill should appear: {s}");
    }
}
