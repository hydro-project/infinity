use crate::component::{Component, KeyResult};
use ratatui::{
    buffer::Buffer,
    crossterm::event::{KeyCode, KeyEvent, KeyModifiers},
    layout::{Position, Rect},
    style::Color,
    widgets::Widget,
};
use std::cell::Cell;

const PAD_X: u16 = 1;
const PAD_Y: u16 = 1;
const BG: Color = Color::Rgb(40, 40, 40);
const FG: Color = Color::White;

/// A multi-line text input with its own wrapping, cursor tracking, and rendering.
pub struct TextInput {
    buf: String,
    /// Byte-offset cursor position within `buf`.
    cursor: usize,
    /// Last-known inner wrapping width (set during `preferred_height` / `render`).
    /// Used by `move_up` / `move_down` so they can navigate visual lines without
    /// needing an explicit width parameter.
    wrap_width: Cell<u16>,
}

/// Snapshot returned by `render_state` so the caller can place the terminal cursor.
pub struct RenderResult {
    /// Absolute screen position where the blinking cursor should be placed.
    pub cursor_position: Position,
}

impl Default for TextInput {
    fn default() -> Self {
        Self::new()
    }
}

impl TextInput {
    pub fn new() -> Self {
        Self {
            buf: String::new(),
            cursor: 0,
            wrap_width: Cell::new(0),
        }
    }

    // ── Public mutation API ──────────────────────────────────────────

    pub fn insert_char(&mut self, ch: char) {
        self.buf.insert(self.cursor, ch);
        self.cursor += ch.len_utf8();
    }

    /// Insert an arbitrary string at the cursor (e.g. from a bracketed paste).
    /// The cursor advances to the end of the inserted text.
    pub fn insert_str(&mut self, s: &str) {
        self.buf.insert_str(self.cursor, s);
        self.cursor += s.len();
    }

    pub fn backspace(&mut self) {
        if self.cursor > 0 {
            // Find the previous char boundary
            let prev = self.buf[..self.cursor]
                .char_indices()
                .next_back()
                .map(|(i, _)| i)
                .unwrap_or(0);
            self.buf.drain(prev..self.cursor);
            self.cursor = prev;
        }
    }

    pub fn delete(&mut self) {
        if self.cursor < self.buf.len() {
            let next = self.cursor
                + self.buf[self.cursor..]
                    .chars()
                    .next()
                    .expect("bug: cursor within bounds but no char found")
                    .len_utf8();
            self.buf.drain(self.cursor..next);
        }
    }

    pub fn move_left(&mut self) {
        if self.cursor > 0 {
            self.cursor = self.buf[..self.cursor]
                .char_indices()
                .next_back()
                .map(|(i, _)| i)
                .unwrap_or(0);
        }
    }

    pub fn move_right(&mut self) {
        if self.cursor < self.buf.len() {
            self.cursor += self.buf[self.cursor..]
                .chars()
                .next()
                .expect("bug: cursor within bounds but no char found")
                .len_utf8();
        }
    }

    pub fn move_home(&mut self) {
        self.cursor = 0;
    }

    pub fn move_end(&mut self) {
        self.cursor = self.buf.len();
    }

    /// Move the cursor up one visual (wrapped) line.
    /// If the cursor is already on the first line, it moves to the beginning.
    pub fn move_up(&mut self) {
        let wrap_w = self.wrap_width.get() as usize;
        if wrap_w == 0 {
            return;
        }

        let (row, col) = self.cursor_visual_pos(wrap_w);
        if row == 0 {
            self.cursor = 0;
        } else {
            self.set_cursor_to_visual_pos(row - 1, col, wrap_w);
        }
    }

    /// Move the cursor down one visual (wrapped) line.
    /// If the cursor is already on the last line, it moves to the end.
    pub fn move_down(&mut self) {
        let wrap_w = self.wrap_width.get() as usize;
        if wrap_w == 0 {
            return;
        }

        let (row, col) = self.cursor_visual_pos(wrap_w);
        let total_rows = self.visual_line_count(wrap_w);

        if row >= total_rows - 1 {
            self.cursor = self.buf.len();
        } else {
            self.set_cursor_to_visual_pos(row + 1, col, wrap_w);
        }
    }

    /// Returns `(visual_row, char_column)` for the current cursor position
    /// given a wrapping width.
    fn cursor_visual_pos(&self, wrap_w: usize) -> (usize, usize) {
        let lines = wrap_lines(&self.buf, wrap_w);
        let buf_start = self.buf.as_ptr() as usize;

        for (row_idx, line) in lines.iter().enumerate() {
            let line_byte_start = line.as_ptr() as usize - buf_start;
            let line_byte_end = line_byte_start + line.len();

            if self.cursor >= line_byte_start && self.cursor <= line_byte_end {
                let offset = self.cursor.saturating_sub(line_byte_start);
                let col = line[..offset.min(line.len())].chars().count();

                // If the cursor is at the end of a line that fills the full
                // width, it logically sits at col 0 of the next row (matching
                // the same logic used in `render`).
                let line_full = line.chars().count() >= wrap_w;
                if offset == line.len() && line_full {
                    continue;
                }

                return (row_idx, col);
            }
        }

        // Fallback: cursor is past all content.
        if let Some(last) = lines.last() {
            let last_full = last.chars().count() >= wrap_w;
            if last_full && self.cursor == self.buf.len() {
                return (lines.len(), 0);
            }
            return (lines.len().saturating_sub(1), last.chars().count());
        }

        (0, 0)
    }

    /// Total number of visual rows (at least 1) for the current buffer at the
    /// given wrapping width, including the potential extra row when the last
    /// line is exactly full and the cursor sits at the buffer end.
    fn visual_line_count(&self, wrap_w: usize) -> usize {
        let lines = wrap_lines(&self.buf, wrap_w);
        let extra = lines
            .last()
            .map(|l| l.chars().count() >= wrap_w && self.cursor == self.buf.len())
            .unwrap_or(false);
        lines.len() + extra as usize
    }

    /// Move the cursor to the given visual `(row, col)`, clamping the column
    /// to the actual length of the target line.
    fn set_cursor_to_visual_pos(&mut self, target_row: usize, target_col: usize, wrap_w: usize) {
        let new_cursor = {
            let lines = wrap_lines(&self.buf, wrap_w);
            let buf_start = self.buf.as_ptr() as usize;

            if target_row >= lines.len() {
                self.buf.len()
            } else {
                let line = lines[target_row];
                let line_byte_start = line.as_ptr() as usize - buf_start;
                let char_count = line.chars().count();
                let clamped_col = target_col.min(char_count);

                let byte_offset = line
                    .char_indices()
                    .nth(clamped_col)
                    .map(|(i, _)| i)
                    .unwrap_or(line.len());

                line_byte_start + byte_offset
            }
        };
        self.cursor = new_cursor;
    }

    /// Move cursor one word to the left (Option+Left / Ctrl+Left).
    pub fn move_word_left(&mut self) {
        // Skip whitespace to the left, then skip non-whitespace.
        let before = &self.buf[..self.cursor];
        let trimmed = before.trim_end();
        if trimmed.is_empty() {
            self.cursor = 0;
            return;
        }
        // Find the start of the current word
        if let Some((i, _)) = trimmed
            .char_indices()
            .rev()
            .find(|(_, c)| c.is_whitespace())
        {
            // `i` is the byte index of the space; skip past it
            self.cursor = i + trimmed[i..]
                .chars()
                .next()
                .expect("bug: whitespace char index valid but no char found")
                .len_utf8();
        } else {
            self.cursor = 0;
        }
    }

    /// Move cursor one word to the right (Option+Right / Ctrl+Right).
    pub fn move_word_right(&mut self) {
        let after = &self.buf[self.cursor..];
        // Skip non-whitespace, then skip whitespace.
        let skip_word = after
            .char_indices()
            .find(|(_, c)| c.is_whitespace())
            .map(|(i, _)| i)
            .unwrap_or(after.len());
        let rest = &after[skip_word..];
        let skip_space = rest
            .char_indices()
            .find(|(_, c)| !c.is_whitespace())
            .map(|(i, _)| i)
            .unwrap_or(rest.len());
        self.cursor += skip_word + skip_space;
    }

    /// Delete the word to the left of the cursor (Option+Backspace).
    pub fn delete_word_left(&mut self) {
        let start = self.cursor;
        self.move_word_left();
        self.buf.drain(self.cursor..start);
    }

    /// Insert a newline at the cursor (Shift+Enter).
    pub fn insert_newline(&mut self) {
        self.insert_char('\n');
    }

    /// Handle a key event. Returns `KeyResult::Captured` if the event was consumed by the
    /// input area, `KeyResult::NotCaptured` if it should fall through to other handlers.
    pub fn handle_keystroke(&mut self, key: KeyEvent) -> KeyResult {
        let mods = key.modifiers;
        let ctrl = mods.contains(KeyModifiers::CONTROL);
        let alt = mods.contains(KeyModifiers::ALT);

        match key.code {
            // ── Word-level navigation (Option/Alt + arrows) ─────────
            // On macOS, Option+Left/Right emit Alt+b / Alt+f (escape
            // sequences \x1bb / \x1bf) rather than Alt+Arrow.
            KeyCode::Left if alt => {
                self.move_word_left();
                KeyResult::Captured
            }
            KeyCode::Right if alt => {
                self.move_word_right();
                KeyResult::Captured
            }
            KeyCode::Char('b') if alt => {
                self.move_word_left();
                KeyResult::Captured
            }
            KeyCode::Char('f') if alt => {
                self.move_word_right();
                KeyResult::Captured
            }

            // ── Emacs-style line nav ────────────────────────────────
            KeyCode::Char('a') if ctrl => {
                self.move_home();
                KeyResult::Captured
            }
            KeyCode::Char('e') if ctrl => {
                self.move_end();
                KeyResult::Captured
            }

            // ── Ctrl+C clears input; falls through if already empty ─
            KeyCode::Char('c') if ctrl => {
                if !self.is_empty() {
                    self.buf.clear();
                    self.cursor = 0;
                    KeyResult::Captured
                } else {
                    KeyResult::NotCaptured
                }
            }

            // ── Ctrl+J → newline (universal alternative to Alt+Enter) ──
            // macOS Terminal.app uses Option for character composition by
            // default, so Alt+Enter is not recognized there.
            KeyCode::Char('j') if ctrl => {
                self.insert_newline();
                KeyResult::Captured
            }

            // ── Let Ctrl+<key> combos we don't handle fall through ──
            KeyCode::Char(_) if ctrl => KeyResult::NotCaptured,

            // ── Delete word left (Option+Backspace / Alt+Backspace) ──
            KeyCode::Backspace if alt => {
                self.delete_word_left();
                KeyResult::Captured
            }

            // ── Alt+Enter → newline ──────────────────────────────────
            KeyCode::Enter if alt => {
                self.insert_newline();
                KeyResult::Captured
            }

            // ── Plain Enter → not ours, let terminal submit ─────────
            KeyCode::Enter => KeyResult::NotCaptured,

            // ── Basic editing ───────────────────────────────────────
            KeyCode::Backspace => {
                self.backspace();
                KeyResult::Captured
            }
            KeyCode::Delete => {
                self.delete();
                KeyResult::Captured
            }
            KeyCode::Left => {
                self.move_left();
                KeyResult::Captured
            }
            KeyCode::Right => {
                self.move_right();
                KeyResult::Captured
            }
            KeyCode::Up => {
                self.move_up();
                KeyResult::Captured
            }
            KeyCode::Down => {
                self.move_down();
                KeyResult::Captured
            }
            KeyCode::Home => {
                self.move_home();
                KeyResult::Captured
            }
            KeyCode::End => {
                self.move_end();
                KeyResult::Captured
            }
            KeyCode::Char(ch) => {
                self.insert_char(ch);
                KeyResult::Captured
            }

            _ => KeyResult::NotCaptured,
        }
    }

    /// Take the current text, clear the buffer, and reset the cursor.
    pub fn take_text(&mut self) -> String {
        self.cursor = 0;
        std::mem::take(&mut self.buf)
    }

    pub fn text(&self) -> &str {
        &self.buf
    }

    pub fn set_text(&mut self, s: &str) {
        self.buf = s.to_string();
        self.cursor = self.buf.len();
    }

    pub fn is_empty(&self) -> bool {
        self.buf.trim().is_empty()
    }

    // ── Layout ──────────────────────────────────────────────────────

    /// The total height this widget wants given a particular outer width.
    /// Includes top/bottom padding rows.
    pub fn preferred_height(&self, outer_width: u16) -> u16 {
        let inner_w = outer_width.saturating_sub(PAD_X * 2);
        self.wrap_width.set(inner_w);
        if inner_w == 0 {
            return PAD_Y * 2 + 1;
        }
        let lines = wrap_lines(&self.buf, inner_w as usize);
        let mut text_rows = lines.len().max(1) as u16;

        // If the cursor sits past a full last line, it will render on an
        // extra row — make sure we reserve space for it.
        if let Some(last) = lines.last()
            && last.chars().count() >= inner_w as usize
            && self.cursor == self.buf.len()
        {
            text_rows += 1;
        }

        PAD_Y * 2 + text_rows
    }

    // ── Rendering ───────────────────────────────────────────────────

    /// Render into the given area and return the cursor screen position.
    pub fn render(&self, area: Rect, buf: &mut Buffer) -> RenderResult {
        // Fill background
        for y in area.y..area.bottom() {
            for x in area.x..area.right() {
                buf[(x, y)].set_char(' ').set_bg(BG);
            }
        }

        let inner = Rect::new(
            area.x + PAD_X,
            area.y + PAD_Y,
            area.width.saturating_sub(PAD_X * 2),
            area.height.saturating_sub(PAD_Y * 2),
        );

        self.wrap_width.set(inner.width);

        if inner.width == 0 || inner.height == 0 {
            return RenderResult {
                cursor_position: Position::new(area.x + PAD_X, area.y + PAD_Y),
            };
        }

        let lines = wrap_lines(&self.buf, inner.width as usize);

        // Find which visual line/col the cursor falls on
        let mut cursor_row: u16 = 0;
        let mut cursor_col: u16 = 0;
        let mut found_cursor = false;
        let buf_start = self.buf.as_ptr() as usize;

        for (row_idx, line) in lines.iter().enumerate() {
            let line_byte_start = line.as_ptr() as usize - buf_start;
            let line_byte_end = line_byte_start + line.len();
            if !found_cursor && self.cursor >= line_byte_start && self.cursor <= line_byte_end {
                let offset = self.cursor.saturating_sub(line_byte_start);
                let col = line[..offset.min(line.len())].chars().count();

                // If the cursor is at the end of a line that is exactly
                // max_width chars wide, it should appear at column 0 of
                // the next visual row (like a real text editor).
                let line_full = line.chars().count() >= inner.width as usize;
                if offset == line.len() && line_full {
                    // Let the next iteration (or the fallback below)
                    // place the cursor on the following row.
                    continue;
                }

                cursor_row = row_idx as u16;
                cursor_col = col as u16;
                found_cursor = true;
            }
        }

        // Cursor is past all wrapped content — place it at col 0 of the
        // next row (happens when the last line is exactly full) or at the
        // end of the last line.
        if !found_cursor && let Some(last) = lines.last() {
            let last_full = last.chars().count() >= inner.width as usize;
            if last_full && self.cursor == self.buf.len() {
                cursor_row = lines.len() as u16;
                cursor_col = 0;
            } else {
                cursor_row = lines.len().saturating_sub(1) as u16;
                cursor_col = last.chars().count() as u16;
            }
        }

        // Draw text
        for (row_idx, line) in lines.iter().enumerate() {
            let y = inner.y + row_idx as u16;
            if y >= inner.bottom() {
                break;
            }
            for (col_idx, ch) in line.chars().enumerate() {
                let x = inner.x + col_idx as u16;
                if x >= inner.right() {
                    break;
                }
                buf[(x, y)].set_char(ch).set_fg(FG).set_bg(BG);
            }
        }

        RenderResult {
            cursor_position: Position::new(
                inner.x + cursor_col,
                (inner.y + cursor_row).min(inner.bottom().saturating_sub(1)),
            ),
        }
    }
}

/// Word-wrap `text` into lines of at most `max_width` display columns.
/// Handles explicit newlines and soft-wraps long lines.
/// Returns a vec of string slices (borrowed from `text`).
fn wrap_lines<'a>(text: &'a str, max_width: usize) -> Vec<&'a str> {
    if text.is_empty() {
        // Return a slice of `text` (not a string literal) so pointer
        // arithmetic in the caller stays valid.
        return vec![&text[..0]];
    }
    if max_width == 0 {
        return vec![text];
    }

    let mut lines: Vec<&'a str> = Vec::new();

    // First split on hard newlines, then soft-wrap each segment.
    for segment in text.split('\n') {
        // For empty segments (consecutive newlines), push an empty slice
        // that still points into `text` so pointer arithmetic works.
        if segment.is_empty() {
            // Use the byte position of this segment within text.
            let offset = segment.as_ptr() as usize - text.as_ptr() as usize;
            lines.push(&text[offset..offset]);
            continue;
        }

        let mut remaining = segment;
        while !remaining.is_empty() {
            let char_count = remaining.chars().count();
            if char_count <= max_width {
                lines.push(remaining);
                break;
            }

            // Find the byte index of the max_width-th character
            let break_byte = remaining
                .char_indices()
                .nth(max_width)
                .map(|(i, _)| i)
                .unwrap_or(remaining.len());

            // Try to break at the last space within the first max_width chars
            let seg = &remaining[..break_byte];
            if let Some(space_pos) = seg.rfind(' ') {
                lines.push(&remaining[..space_pos]);
                // Skip the space
                remaining = &remaining[space_pos + 1..];
            } else {
                // No space found — hard break at max_width
                lines.push(seg);
                remaining = &remaining[break_byte..];
            }
        }
    }

    lines
}

impl Component for TextInput {
    fn handle_keystroke(&mut self, key: KeyEvent) -> KeyResult {
        TextInput::handle_keystroke(self, key)
    }
}

/// Widget adapter so `TextInput` can be used with `frame.render_widget`.
/// The cursor position is stashed in the provided `Option`.
pub struct TextInputWidget<'a> {
    input: &'a TextInput,
    cursor_out: &'a mut Option<Position>,
}

impl<'a> TextInputWidget<'a> {
    pub fn new(input: &'a TextInput, cursor_out: &'a mut Option<Position>) -> Self {
        Self { input, cursor_out }
    }
}

impl<'a> Widget for TextInputWidget<'a> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let result = self.input.render(area, buf);
        *self.cursor_out = Some(result.cursor_position);
    }
}
