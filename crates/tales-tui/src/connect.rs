//! The "connect your tools" screen — the first thing `tales` shows.
//!
//! A checklist of every tool Tales knows how to drive (from
//! [`tales_core::agent::KNOWN_TOOLS`]), with live "installed?" detection. You
//! toggle the ones you want, and the first two enabled become the drafter and
//! critic for the planning discussion. Pure state + rendering; key handling
//! lives in `main` so this stays unit-testable.

use ratatui::layout::{Alignment, Constraint, Layout};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use tales_core::agent::{bin_on_path, KNOWN_TOOLS};

use crate::theme::{color_for, pretty, ACCENT, DIM, ERRC, FAINT, OK, TEXT};

/// A connected tool plus the optional model/effort the user picked for it on
/// the connect screen. `None` means "use the tool's own default".
#[derive(Clone, Debug)]
pub struct ToolChoice {
    pub key: String,
    pub model: Option<String>,
    pub effort: Option<String>,
}

/// One selectable tool row.
struct Row {
    key: &'static str,
    install: &'static str,
    installed: bool,
    enabled: bool,
    /// Suggested models / effort levels for this tool (from the registry row).
    models: &'static [&'static str],
    efforts: &'static [&'static str],
    /// Selected index into `models` / `efforts`, or `None` for the tool default.
    model_sel: Option<usize>,
    effort_sel: Option<usize>,
}

impl Row {
    fn model(&self) -> Option<String> {
        self.model_sel.map(|i| self.models[i].to_string())
    }
    fn effort(&self) -> Option<String> {
        self.effort_sel.map(|i| self.efforts[i].to_string())
    }
}

/// The connect-your-tools checklist.
pub struct ConnectScreen {
    rows: Vec<Row>,
    cursor: usize,
    status: Option<String>,
}

impl ConnectScreen {
    /// Build the checklist. Tools whose key is in `preselect` start enabled —
    /// but only if they're actually installed (you can't connect a missing CLI).
    pub fn new(preselect: &[String]) -> Self {
        let rows = KNOWN_TOOLS
            .iter()
            .map(|t| {
                let installed = bin_on_path(t.bin);
                let wanted = preselect.iter().any(|p| p.eq_ignore_ascii_case(t.key));
                Row {
                    key: t.key,
                    install: t.install,
                    installed,
                    enabled: installed && wanted,
                    models: t.models,
                    efforts: t.efforts,
                    model_sel: None,
                    effort_sel: None,
                }
            })
            .collect();
        Self {
            rows,
            cursor: 0,
            status: None,
        }
    }

    pub fn up(&mut self) {
        if self.cursor > 0 {
            self.cursor -= 1;
        }
    }

    pub fn down(&mut self) {
        if self.cursor + 1 < self.rows.len() {
            self.cursor += 1;
        }
    }

    /// Toggle the row under the cursor. Refuses to enable a tool that isn't
    /// installed, surfacing its install hint instead.
    pub fn toggle(&mut self) {
        let Some(row) = self.rows.get_mut(self.cursor) else {
            return;
        };
        if !row.installed {
            self.status = Some(format!(
                "{} isn't installed — {}",
                pretty(row.key),
                row.install
            ));
            return;
        }
        row.enabled = !row.enabled;
        self.status = None;
    }

    /// Cycle the model for the row under the cursor: tool-default → first
    /// suggestion → … → last → default. Tools with no suggestions hint instead.
    pub fn cycle_model(&mut self) {
        let Some(row) = self.rows.get_mut(self.cursor) else {
            return;
        };
        if !row.installed {
            return;
        }
        if row.models.is_empty() {
            self.status = Some(format!(
                "{} has no model presets — connect it and pass a model on the CLI",
                pretty(row.key)
            ));
            return;
        }
        row.model_sel = match row.model_sel {
            None => Some(0),
            Some(i) if i + 1 < row.models.len() => Some(i + 1),
            Some(_) => None,
        };
        self.status = None;
    }

    /// Cycle the reasoning effort for the row under the cursor. Tools with no
    /// effort knob (e.g. Claude Code) hint instead.
    pub fn cycle_effort(&mut self) {
        let Some(row) = self.rows.get_mut(self.cursor) else {
            return;
        };
        if !row.installed {
            return;
        }
        if row.efforts.is_empty() {
            self.status = Some(format!("{} has no effort levels to set", pretty(row.key)));
            return;
        }
        row.effort_sel = match row.effort_sel {
            None => Some(0),
            Some(i) if i + 1 < row.efforts.len() => Some(i + 1),
            Some(_) => None,
        };
        self.status = None;
    }

    /// Enabled tools with their chosen model/effort, in display order.
    pub fn choices(&self) -> Vec<ToolChoice> {
        self.rows
            .iter()
            .filter(|r| r.enabled)
            .map(|r| ToolChoice {
                key: r.key.to_string(),
                model: r.model(),
                effort: r.effort(),
            })
            .collect()
    }

    /// Need at least two tools to hold a discussion (a drafter and a critic).
    pub fn can_continue(&self) -> bool {
        self.rows.iter().filter(|r| r.enabled).count() >= 2
    }

    /// Enter pressed: return the chosen tools if we have enough, else nudge with
    /// an actionable message — "toggle more" if enough are installed, or "install
    /// a second CLI" (with a hint) when fewer than two even exist on this machine.
    pub fn confirm(&mut self) -> Option<Vec<ToolChoice>> {
        if self.can_continue() {
            return Some(self.choices());
        }
        let installed = self.rows.iter().filter(|r| r.installed).count();
        self.status = Some(if installed < 2 {
            match self.rows.iter().find(|r| !r.installed) {
                Some(r) => format!(
                    "Need a second AI CLI to collaborate — install one, e.g. {}: {}",
                    pretty(r.key),
                    r.install
                ),
                None => "Need at least two AI CLIs installed to collaborate.".to_string(),
            }
        } else {
            "Pick at least two tools (Space to toggle).".to_string()
        });
        None
    }

    pub fn draw(&self, f: &mut Frame) {
        let chunks = Layout::vertical([
            Constraint::Length(1), // header
            Constraint::Length(1), // spacer
            Constraint::Length(1), // instruction
            Constraint::Min(1),    // list (+ status)
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
                Span::styled("· connect your tools", Style::default().fg(DIM)),
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

        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "Which AI tools should collaborate? The first two become drafter + critic.",
                Style::default().fg(DIM),
            ))),
            chunks[2],
        );

        // Roles follow enabled order: first enabled = drafter, rest = critic.
        let mut enabled_seen = 0usize;
        let mut lines: Vec<Line<'static>> = Vec::new();
        for (i, row) in self.rows.iter().enumerate() {
            let role = if row.enabled {
                let r = if enabled_seen == 0 {
                    "drafter"
                } else {
                    "critic"
                };
                enabled_seen += 1;
                Some(r)
            } else {
                None
            };
            lines.push(row_line(row, i == self.cursor, role));
        }
        lines.push(Line::from(""));
        if let Some(s) = &self.status {
            lines.push(Line::from(Span::styled(
                format!("  {s}"),
                Style::default().fg(ERRC),
            )));
        }
        f.render_widget(Paragraph::new(lines), chunks[3]);

        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "Space toggle · ↑/↓ move · m model · e effort · Enter connect · q quit",
                Style::default().fg(FAINT),
            ))),
            chunks[4],
        );
    }
}

fn row_line(row: &Row, is_cursor: bool, role: Option<&str>) -> Line<'static> {
    let marker = if is_cursor { "▸ " } else { "  " };
    let check = if row.enabled { "[x] " } else { "[ ] " };
    let name = pretty(row.key);
    let name_padded = format!("{name:<12}");
    let name_color = if row.installed {
        color_for(row.key)
    } else {
        DIM
    };

    let mut spans = vec![
        Span::styled(
            marker.to_string(),
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            check.to_string(),
            Style::default().fg(if row.enabled { OK } else { FAINT }),
        ),
        Span::styled(
            name_padded,
            Style::default().fg(name_color).add_modifier(Modifier::BOLD),
        ),
    ];
    spans.push(Span::raw("  "));
    if let Some(r) = role {
        spans.push(Span::styled(
            format!("{r:<8}"),
            Style::default().fg(FAINT).add_modifier(Modifier::BOLD),
        ));
    } else {
        spans.push(Span::raw(format!("{:<8}", "")));
    }
    spans.push(Span::raw("  "));
    if row.installed {
        spans.push(Span::styled(
            "installed".to_string(),
            Style::default().fg(DIM),
        ));
        // Show the picked model/effort (only meaningful once enabled).
        if row.enabled {
            if let Some(m) = row.model() {
                spans.push(Span::styled(
                    format!("  ◇ {m}"),
                    Style::default().fg(ACCENT),
                ));
            }
            if let Some(e) = row.effort() {
                spans.push(Span::styled(
                    format!("  ⚡{e}"),
                    Style::default().fg(ACCENT),
                ));
            }
        }
    } else {
        spans.push(Span::styled(
            format!("not installed · {}", row.install),
            Style::default().fg(FAINT),
        ));
    }
    Line::from(spans)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preselect_only_enables_installed_tools() {
        // claude is installed in this environment; a bogus tool never is.
        let s = ConnectScreen::new(&["claude".into(), "codex".into(), "opencode".into()]);
        for r in &s.rows {
            if r.enabled {
                assert!(r.installed, "{} enabled but not installed", r.key);
            }
        }
    }

    #[test]
    fn toggle_uninstalled_sets_hint_and_stays_off() {
        let mut s = ConnectScreen::new(&[]);
        // Find an uninstalled row and point the cursor at it.
        if let Some(idx) = s.rows.iter().position(|r| !r.installed) {
            s.cursor = idx;
            s.toggle();
            assert!(!s.rows[idx].enabled);
            assert!(s.status.is_some());
        }
    }

    #[test]
    fn enabled_order_is_display_order() {
        let mut s = ConnectScreen::new(&[]);
        // Force the first two rows on regardless of install state for the test.
        s.rows[0].installed = true;
        s.rows[0].enabled = true;
        s.rows[1].installed = true;
        s.rows[1].enabled = true;
        let keys: Vec<String> = s.choices().into_iter().map(|c| c.key).collect();
        assert_eq!(
            keys,
            vec![s.rows[0].key.to_string(), s.rows[1].key.to_string()]
        );
        assert!(s.can_continue());
    }

    #[test]
    fn cycle_model_walks_suggestions_then_back_to_default() {
        let mut s = ConnectScreen::new(&[]);
        // Point at claude (models: opus, sonnet, haiku) and enable it.
        let idx = s.rows.iter().position(|r| r.key == "claude").unwrap();
        s.cursor = idx;
        s.rows[idx].installed = true;
        s.rows[idx].enabled = true;
        assert_eq!(s.rows[idx].model(), None); // default
        s.cycle_model();
        assert_eq!(s.rows[idx].model().as_deref(), Some("opus"));
        s.cycle_model();
        assert_eq!(s.rows[idx].model().as_deref(), Some("sonnet"));
        // …through the last, then wraps back to default.
        s.cycle_model();
        s.cycle_model();
        assert_eq!(s.rows[idx].model(), None);
    }

    #[test]
    fn cycle_effort_only_for_tools_that_have_it() {
        let mut s = ConnectScreen::new(&[]);
        // Codex has efforts; cycling sets one. A tool without efforts hints.
        let cx = s.rows.iter().position(|r| r.key == "codex").unwrap();
        s.cursor = cx;
        s.rows[cx].installed = true;
        s.rows[cx].enabled = true;
        s.cycle_effort();
        assert_eq!(s.rows[cx].effort().as_deref(), Some("low"));

        let no_effort = s.rows.iter().position(|r| r.efforts.is_empty()).unwrap();
        s.cursor = no_effort;
        s.rows[no_effort].installed = true;
        s.cycle_effort();
        assert_eq!(s.rows[no_effort].effort(), None);
        assert!(s.status.is_some()); // hinted "no effort levels"
    }

    #[test]
    fn choices_carry_model_and_effort() {
        let mut s = ConnectScreen::new(&[]);
        let cx = s.rows.iter().position(|r| r.key == "codex").unwrap();
        s.rows[cx].installed = true;
        s.rows[cx].enabled = true;
        s.rows[cx].model_sel = Some(0); // gpt-5-codex
        s.rows[cx].effort_sel = Some(2); // high
        let other = if cx == 0 { 1 } else { 0 };
        s.rows[other].installed = true;
        s.rows[other].enabled = true;
        let choice = s.choices().into_iter().find(|c| c.key == "codex").unwrap();
        assert_eq!(choice.model.as_deref(), Some("gpt-5-codex"));
        assert_eq!(choice.effort.as_deref(), Some("high"));
    }

    #[test]
    fn confirm_needs_two() {
        let mut s = ConnectScreen::new(&[]);
        for r in &mut s.rows {
            r.enabled = false;
        }
        assert!(s.confirm().is_none());
        assert!(s.status.is_some());
    }

    #[test]
    fn confirm_with_under_two_installed_suggests_install() {
        let mut s = ConnectScreen::new(&[]);
        // Simulate a machine with only one CLI installed.
        for (i, r) in s.rows.iter_mut().enumerate() {
            r.installed = i == 0;
            r.enabled = i == 0;
        }
        assert!(s.confirm().is_none());
        let status = s.status.unwrap();
        assert!(status.contains("install"), "{status}");
    }
}
