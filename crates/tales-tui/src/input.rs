//! The prompt editor — a small, cursor-aware, multi-line text buffer shared by
//! every input surface (classic prompt, live chat, terminal workspace).
//!
//! A terminal is monospace, so layout is character-cell exact. The buffer stores
//! `Vec<char>` for O(1) cursor math and clean multi-byte handling, tracks a
//! cursor, and knows how to wrap itself to a width, place a block cursor, and
//! scroll so the cursor stays visible. Keeping all editing here means the chat,
//! the classic prompt, and the terminal pane all get the same feel for free.
//!
//! Long-prompt ergonomics this enables: insert anywhere (not just append),
//! word/line motions, kill-word / kill-line, real newlines (Alt+Enter, Shift+
//! Enter where the terminal reports it, or Ctrl-J), and bracketed paste of a
//! whole multi-line block as one edit.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use crate::theme::{ACCENT, TEXT};

/// An editable multi-line prompt buffer with a cursor.
#[derive(Default, Clone)]
pub struct Input {
    chars: Vec<char>,
    /// Cursor position as a char index in `0..=chars.len()`.
    cursor: usize,
}

impl Input {
    pub fn new() -> Self {
        Self::default()
    }

    /// Build from text, cursor at the end (used for prefills / Back navigation).
    pub fn from_text(text: &str) -> Self {
        let chars: Vec<char> = text.chars().collect();
        let cursor = chars.len();
        Self { chars, cursor }
    }

    pub fn as_string(&self) -> String {
        self.chars.iter().collect()
    }

    /// The trimmed text — what a submit actually sends.
    pub fn trimmed(&self) -> String {
        self.as_string().trim().to_string()
    }

    pub fn is_empty(&self) -> bool {
        self.chars.is_empty()
    }

    pub fn clear(&mut self) {
        self.chars.clear();
        self.cursor = 0;
    }

    // ── editing ──────────────────────────────────────────────────────────────

    pub fn insert_char(&mut self, c: char) {
        self.chars.insert(self.cursor, c);
        self.cursor += 1;
    }

    /// Insert a run of text (e.g. a bracketed paste) at the cursor as one edit,
    /// preserving any embedded newlines.
    pub fn insert_str(&mut self, text: &str) {
        for c in text.chars() {
            // Normalize CRLF/CR pastes to LF so wrapping/rendering stay simple.
            if c == '\r' {
                continue;
            }
            self.chars.insert(self.cursor, c);
            self.cursor += 1;
        }
    }

    /// Insert a literal newline (Alt/Shift+Enter, Ctrl-J).
    pub fn newline(&mut self) {
        self.insert_char('\n');
    }

    pub fn backspace(&mut self) {
        if self.cursor > 0 {
            self.cursor -= 1;
            self.chars.remove(self.cursor);
        }
    }

    pub fn delete(&mut self) {
        if self.cursor < self.chars.len() {
            self.chars.remove(self.cursor);
        }
    }

    /// Delete the word (and trailing run of spaces) before the cursor — Ctrl-W.
    pub fn delete_word_left(&mut self) {
        let start = self.word_left_index();
        self.chars.drain(start..self.cursor);
        self.cursor = start;
    }

    /// Delete from the cursor back to the start of the current line — Ctrl-U.
    pub fn kill_to_line_start(&mut self) {
        let start = self.line_start();
        self.chars.drain(start..self.cursor);
        self.cursor = start;
    }

    /// Delete from the cursor to the end of the current line — Ctrl-K.
    pub fn kill_to_line_end(&mut self) {
        let end = self.line_end();
        self.chars.drain(self.cursor..end);
    }

    // ── motion ───────────────────────────────────────────────────────────────

    pub fn move_left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    pub fn move_right(&mut self) {
        if self.cursor < self.chars.len() {
            self.cursor += 1;
        }
    }

    pub fn move_word_left(&mut self) {
        self.cursor = self.word_left_index();
    }

    pub fn move_word_right(&mut self) {
        let mut i = self.cursor;
        let n = self.chars.len();
        while i < n && self.chars[i].is_whitespace() {
            i += 1;
        }
        while i < n && !self.chars[i].is_whitespace() {
            i += 1;
        }
        self.cursor = i;
    }

    /// Home — start of the current logical line.
    pub fn home(&mut self) {
        self.cursor = self.line_start();
    }

    /// End — end of the current logical line.
    pub fn end(&mut self) {
        self.cursor = self.line_end();
    }

    /// Move up one logical line, keeping the column where possible.
    pub fn move_up(&mut self) {
        let start = self.line_start();
        if start == 0 {
            self.cursor = 0;
            return;
        }
        let col = self.cursor - start;
        let prev_end = start - 1; // the '\n' ending the previous line
        let prev_start = self.line_start_of(prev_end);
        let prev_len = prev_end - prev_start;
        self.cursor = prev_start + col.min(prev_len);
    }

    /// Move down one logical line, keeping the column where possible.
    pub fn move_down(&mut self) {
        let end = self.line_end();
        if end == self.chars.len() {
            self.cursor = end;
            return;
        }
        let col = self.cursor - self.line_start();
        let next_start = end + 1; // skip the '\n'
        let next_end = self.line_end_of(next_start);
        let next_len = next_end - next_start;
        self.cursor = next_start + col.min(next_len);
    }

    // ── key handling ──────────────────────────────────────────────────────────

    /// Apply an editing/motion key. Returns `true` if it was consumed, leaving
    /// submit/command keys (plain Enter, Esc, bare digits at the gate) to the
    /// caller. Plain printable characters are NOT handled here so the caller can
    /// first intercept them (e.g. the executor gate's number picker); use
    /// [`Input::insert_char`] for those.
    pub fn handle_edit_key(&mut self, key: KeyEvent) -> bool {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        match key.code {
            KeyCode::Char(c) if ctrl => match c.to_ascii_lowercase() {
                'a' => self.home(),
                'e' => self.end(),
                'w' => self.delete_word_left(),
                'u' => self.kill_to_line_start(),
                'k' => self.kill_to_line_end(),
                'j' => self.newline(),
                _ => return false,
            },
            KeyCode::Char(c) if alt => match c.to_ascii_lowercase() {
                'b' => self.move_word_left(),
                'f' => self.move_word_right(),
                _ => return false,
            },
            KeyCode::Enter if alt || shift => self.newline(),
            KeyCode::Left if alt || ctrl => self.move_word_left(),
            KeyCode::Right if alt || ctrl => self.move_word_right(),
            KeyCode::Left => self.move_left(),
            KeyCode::Right => self.move_right(),
            KeyCode::Up => self.move_up(),
            KeyCode::Down => self.move_down(),
            KeyCode::Home => self.home(),
            KeyCode::End => self.end(),
            KeyCode::Backspace => self.backspace(),
            KeyCode::Delete => self.delete(),
            _ => return false,
        }
        true
    }

    // ── internal helpers ──────────────────────────────────────────────────────

    fn word_left_index(&self) -> usize {
        let mut i = self.cursor;
        while i > 0 && self.chars[i - 1].is_whitespace() {
            i -= 1;
        }
        while i > 0 && !self.chars[i - 1].is_whitespace() {
            i -= 1;
        }
        i
    }

    fn line_start(&self) -> usize {
        self.line_start_of(self.cursor)
    }

    fn line_start_of(&self, pos: usize) -> usize {
        let mut i = pos;
        while i > 0 && self.chars[i - 1] != '\n' {
            i -= 1;
        }
        i
    }

    fn line_end(&self) -> usize {
        self.line_end_of(self.cursor)
    }

    fn line_end_of(&self, pos: usize) -> usize {
        let mut i = pos;
        while i < self.chars.len() && self.chars[i] != '\n' {
            i += 1;
        }
        i
    }

    // ── layout + rendering ────────────────────────────────────────────────────

    /// Height in rows this buffer needs at `width`, clamped to `[1, max_height]`.
    pub fn height(&self, prefix: &str, width: u16, max_height: u16) -> u16 {
        let rows = self.layout(prefix, width).rows.len() as u16;
        rows.clamp(1, max_height.max(1))
    }

    /// Render the visible window of the input, with the prefix on the first row,
    /// a blank gutter on continuation rows, and a block cursor. `scroll` is the
    /// number of rows scrolled up from the bottom (manual PageUp/PageDown);
    /// while at the bottom the window auto-follows the cursor.
    pub fn view_lines(
        &self,
        prefix: &str,
        width: u16,
        height: u16,
        scroll: usize,
    ) -> Vec<Line<'static>> {
        let height = (height.max(1)) as usize;
        let layout = self.layout(prefix, width);
        let rows = &layout.rows;
        let total = rows.len();

        // Pick the visible window. At the bottom (scroll == 0) keep the cursor in
        // view; if the user paged up, honor that offset instead.
        let (start, end) = if scroll == 0 {
            let mut end = total;
            let mut start = end.saturating_sub(height);
            if layout.cursor_row < start {
                start = layout.cursor_row;
                end = (start + height).min(total);
            }
            (start, end)
        } else {
            let max_scroll = total.saturating_sub(height);
            let end = total - scroll.min(max_scroll);
            (end.saturating_sub(height), end)
        };

        let blank_prefix = " ".repeat(prefix.chars().count());
        let mut out = Vec::with_capacity(end - start);
        for (row_idx, row) in rows[start..end].iter().enumerate() {
            let row_idx = row_idx + start;
            let shown_prefix = if row.first { prefix } else { &blank_prefix };
            let mut spans = vec![Span::styled(
                shown_prefix.to_string(),
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            )];
            if row_idx == layout.cursor_row {
                push_with_cursor(&mut spans, &row.text, layout.cursor_col);
            } else {
                spans.push(Span::styled(row.text.clone(), Style::default().fg(TEXT)));
            }
            out.push(Line::from(spans));
        }
        out
    }

    /// Wrap the buffer to `width` and locate the cursor's row/column.
    fn layout(&self, prefix: &str, width: u16) -> Layout {
        let body = (width as usize)
            .saturating_sub(prefix.chars().count())
            .max(1);
        let mut rows: Vec<Row> = Vec::new();
        let mut row = String::new();
        let mut col = 0usize;
        let mut cursor_row = 0usize;
        let mut cursor_col = 0usize;
        let mut cursor_set = false;

        let push_row = |rows: &mut Vec<Row>, text: String| {
            let first = rows.is_empty();
            rows.push(Row { first, text });
        };

        for (i, &ch) in self.chars.iter().enumerate() {
            if ch == '\n' {
                if i == self.cursor {
                    cursor_row = rows.len();
                    cursor_col = col;
                    cursor_set = true;
                }
                push_row(&mut rows, std::mem::take(&mut row));
                col = 0;
                continue;
            }
            if col >= body {
                push_row(&mut rows, std::mem::take(&mut row));
                col = 0;
            }
            if i == self.cursor {
                cursor_row = rows.len();
                cursor_col = col;
                cursor_set = true;
            }
            row.push(ch);
            col += 1;
        }
        if !cursor_set {
            // Cursor at end of buffer.
            cursor_row = rows.len();
            cursor_col = col;
        }
        push_row(&mut rows, row);

        Layout {
            rows,
            cursor_row,
            cursor_col,
        }
    }
}

impl From<&str> for Input {
    fn from(text: &str) -> Self {
        Self::from_text(text)
    }
}

impl From<String> for Input {
    fn from(text: String) -> Self {
        Self::from_text(&text)
    }
}

struct Row {
    /// First visual row of the whole input — the only one that shows the prefix.
    first: bool,
    text: String,
}

struct Layout {
    rows: Vec<Row>,
    cursor_row: usize,
    cursor_col: usize,
}

/// Render a row's text with a reverse-video block cursor at `col`. When the
/// cursor sits past the last char (end of line), the block falls on a trailing
/// space so it stays visible.
fn push_with_cursor(spans: &mut Vec<Span<'static>>, text: &str, col: usize) {
    let chars: Vec<char> = text.chars().collect();
    let before: String = chars.iter().take(col).collect();
    if !before.is_empty() {
        spans.push(Span::styled(before, Style::default().fg(TEXT)));
    }
    let under = chars.get(col).copied().unwrap_or(' ');
    spans.push(Span::styled(
        under.to_string(),
        Style::default().fg(TEXT).add_modifier(Modifier::REVERSED),
    ));
    if col + 1 < chars.len() {
        let after: String = chars.iter().skip(col + 1).collect();
        spans.push(Span::styled(after, Style::default().fg(TEXT)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

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
    fn insert_edit_and_motion_work_mid_string() {
        let mut input = Input::from_text("helo");
        // Move to between 'l' and 'o' and fix the typo.
        input.move_left();
        input.insert_char('l');
        assert_eq!(input.as_string(), "hello");
        // Home/End.
        input.home();
        assert_eq!(input.cursor, 0);
        input.end();
        assert_eq!(input.cursor, 5);
        // Backspace from the end.
        input.backspace();
        assert_eq!(input.as_string(), "hell");
    }

    #[test]
    fn word_motions_and_kills() {
        let mut input = Input::from_text("alpha beta gamma");
        input.end();
        input.delete_word_left();
        assert_eq!(input.as_string(), "alpha beta ");
        input.move_word_left();
        input.move_word_left();
        // Cursor now before "alpha"; kill to line end clears the rest.
        input.home();
        input.kill_to_line_end();
        assert_eq!(input.as_string(), "");
    }

    #[test]
    fn newline_and_multiline_vertical_motion() {
        let mut input = Input::from_text("");
        input.insert_str("first");
        input.newline();
        input.insert_str("second line");
        assert_eq!(input.as_string(), "first\nsecond line");
        // Up keeps the column (clamped to the shorter first line length).
        input.home(); // start of "second line"
        input.move_right();
        input.move_right(); // col 2 on line 2
        input.move_up();
        assert_eq!(input.cursor, 2); // col 2 on line 1 ("fi|rst")
        input.move_down();
        assert_eq!(input.cursor, "first\n".len() + 2);
    }

    #[test]
    fn paste_inserts_multiline_block_and_strips_cr() {
        let mut input = Input::from_text("a");
        input.insert_str("b\r\nc");
        assert_eq!(input.as_string(), "ab\nc");
    }

    #[test]
    fn alt_enter_and_ctrl_j_insert_newlines() {
        let mut input = Input::from_text("x");
        assert!(input.handle_edit_key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::ALT
        )));
        assert!(input.handle_edit_key(KeyEvent::new(
            KeyCode::Char('j'),
            KeyModifiers::CONTROL
        )));
        assert_eq!(input.as_string(), "x\n\n");
        // Plain Enter is NOT consumed here (caller handles submit).
        assert!(!input.handle_edit_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)));
    }

    #[test]
    fn height_and_view_wrap_to_width() {
        let input = Input::from_text("abcdefgh");
        // prefix "p> " = 3 cols, width 5 => body 2 => 4 rows.
        assert_eq!(input.height("p> ", 5, 10), 4);

        let bottom = text_of(&input.view_lines("p> ", 5, 2, 0));
        assert!(bottom.contains("ef"), "{bottom}");
        assert!(bottom.contains("gh"), "{bottom}");
        assert!(!bottom.contains("ab"), "{bottom}");

        let top = text_of(&input.view_lines("p> ", 5, 2, 2));
        assert!(top.contains("ab"), "{top}");
        assert!(top.contains("cd"), "{top}");
    }

    #[test]
    fn cursor_block_renders_in_view() {
        let mut input = Input::from_text("hi");
        input.home(); // cursor on 'h'
        let lines = input.view_lines("> ", 20, 1, 0);
        // The cursor cell is reverse-video; find a reversed span carrying 'h'.
        let has_cursor = lines.iter().any(|l| {
            l.spans
                .iter()
                .any(|s| s.content.as_ref() == "h" && s.style.add_modifier.contains(Modifier::REVERSED))
        });
        assert!(has_cursor, "expected a reverse-video cursor on 'h'");
    }
}
