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

/// One selectable tool row.
struct Row {
    key: &'static str,
    install: &'static str,
    installed: bool,
    enabled: bool,
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

    /// Enabled tool keys, in display order. The first is the drafter.
    pub fn enabled_keys(&self) -> Vec<String> {
        self.rows
            .iter()
            .filter(|r| r.enabled)
            .map(|r| r.key.to_string())
            .collect()
    }

    /// Need at least two tools to hold a discussion (a drafter and a critic).
    pub fn can_continue(&self) -> bool {
        self.rows.iter().filter(|r| r.enabled).count() >= 2
    }

    /// Enter pressed: return the chosen tools if we have enough, else nudge with
    /// an actionable message — "toggle more" if enough are installed, or "install
    /// a second CLI" (with a hint) when fewer than two even exist on this machine.
    pub fn confirm(&mut self) -> Option<Vec<String>> {
        if self.can_continue() {
            return Some(self.enabled_keys());
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
                "Space toggle · ↑/↓ move · Enter connect · q quit",
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
        let keys = s.enabled_keys();
        assert_eq!(
            keys,
            vec![s.rows[0].key.to_string(), s.rows[1].key.to_string()]
        );
        assert!(s.can_continue());
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
