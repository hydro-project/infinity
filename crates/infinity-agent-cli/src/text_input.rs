use crate::component::{Component, KeyResult};
use ratatui::{
    buffer::Buffer,
    crossterm::event::{KeyCode, KeyEvent, KeyModifiers},
    layout::{Position, Rect},
    style::Color,
    widgets::Widget,
};
use std::cell::Cell;
use unicode_width::UnicodeWidthChar;

const PAD_X: u16 = 1;
const PAD_Y: u16 = 1;
const BG: Color = Color::Rgb(40, 40, 40);
const FG: Color = Color::White;

/// Display width of a single character as the renderer will actually draw it.
///
/// `unicode-width` is inconsistent for control characters: `char::width()`
/// returns `None` while `str::width()` counts them as one column. The
/// renderer skips zero-width chars, so everything that measures text for
/// cursor placement or wrapping must go through this helper (and never
/// `str::width()`) or the cursor drifts away from the drawn glyphs.
fn char_width(ch: char) -> usize {
    ch.width().unwrap_or(0)
}

/// Display width of a string, summed per-char via [`char_width`].
fn display_width(s: &str) -> usize {
    s.chars().map(char_width).sum()
}

/// Normalize line breaks in text arriving from the clipboard.
///
/// Office apps (notably PowerPoint) separate paragraphs with `\r` and soft
/// line breaks with `\x0b` (vertical tab); Windows uses `\r\n`; `\x0c` (form
/// feed) shows up as a page break. All of these become `\n` so they render as
/// real line breaks instead of invisible characters glued between words.
fn normalize_line_breaks(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '\r' => {
                // Collapse `\r\n` into a single break.
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                out.push('\n');
            }
            '\x0b' | '\x0c' => out.push('\n'),
            other => out.push(other),
        }
    }
    out
}

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
    ///
    /// Line breaks are normalized to `\n` (see [`normalize_line_breaks`]).
    pub fn insert_str(&mut self, s: &str) {
        let s = normalize_line_breaks(s);
        self.buf.insert_str(self.cursor, &s);
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

    /// Returns `(visual_row, display_column)` for the current cursor position
    /// given a wrapping width.
    fn cursor_visual_pos(&self, wrap_w: usize) -> (usize, usize) {
        let lines = wrap_lines(&self.buf, wrap_w);
        let buf_start = self.buf.as_ptr() as usize;

        for (row_idx, line) in lines.iter().enumerate() {
            let line_byte_start = line.as_ptr() as usize - buf_start;
            let line_byte_end = line_byte_start + line.len();

            if self.cursor >= line_byte_start && self.cursor <= line_byte_end {
                let offset = self.cursor.saturating_sub(line_byte_start);
                let col = display_width(&line[..offset.min(line.len())]);

                // If the cursor is at the end of a line that fills the full
                // width, it logically sits at col 0 of the next row (matching
                // the same logic used in `render`).
                let line_full = display_width(line) >= wrap_w;
                if offset == line.len() && line_full {
                    continue;
                }

                return (row_idx, col);
            }
        }

        // Fallback: cursor is past all content.
        if let Some(last) = lines.last() {
            let last_full = display_width(last) >= wrap_w;
            if last_full && self.cursor == self.buf.len() {
                return (lines.len(), 0);
            }
            return (lines.len().saturating_sub(1), display_width(last));
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
            .map(|l| display_width(l) >= wrap_w && self.cursor == self.buf.len())
            .unwrap_or(false);
        lines.len() + extra as usize
    }

    /// Move the cursor to the given visual `(row, col)`, clamping the column
    /// to the actual width of the target line.
    fn set_cursor_to_visual_pos(&mut self, target_row: usize, target_col: usize, wrap_w: usize) {
        let new_cursor = {
            let lines = wrap_lines(&self.buf, wrap_w);
            let buf_start = self.buf.as_ptr() as usize;

            if target_row >= lines.len() {
                self.buf.len()
            } else {
                let line = lines[target_row];
                let line_byte_start = line.as_ptr() as usize - buf_start;

                // Walk the line accumulating display widths to find the byte
                // offset corresponding to the target column.
                let mut byte_offset = line.len();
                let mut acc = 0;
                for (i, ch) in line.char_indices() {
                    if acc >= target_col {
                        byte_offset = i;
                        break;
                    }
                    acc += char_width(ch);
                }

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
                if self.cursor == 0 {
                    KeyResult::NotCaptured
                } else {
                    self.move_up();
                    KeyResult::Captured
                }
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
        self.buf = s.to_owned();
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
            && display_width(last) >= inner_w as usize
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

        // Find which visual line/col the cursor falls on (col in display
        // columns, so wide characters count as 2).
        let mut cursor_row: u16 = 0;
        let mut cursor_col: u16 = 0;
        let mut found_cursor = false;
        let buf_start = self.buf.as_ptr() as usize;

        for (row_idx, line) in lines.iter().enumerate() {
            let line_byte_start = line.as_ptr() as usize - buf_start;
            let line_byte_end = line_byte_start + line.len();
            if !found_cursor && self.cursor >= line_byte_start && self.cursor <= line_byte_end {
                let offset = self.cursor.saturating_sub(line_byte_start);
                let col = display_width(&line[..offset.min(line.len())]);

                // If the cursor is at the end of a line that is exactly
                // max_width columns wide, it should appear at column 0 of
                // the next visual row (like a real text editor).
                let line_full = display_width(line) >= inner.width as usize;
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
            let last_full = display_width(last) >= inner.width as usize;
            if last_full && self.cursor == self.buf.len() {
                cursor_row = lines.len() as u16;
                cursor_col = 0;
            } else {
                cursor_row = lines.len().saturating_sub(1) as u16;
                cursor_col = display_width(last) as u16;
            }
        }

        // Draw text, advancing by display width so wide characters occupy
        // two columns (the cell after a wide char is skipped).
        for (row_idx, line) in lines.iter().enumerate() {
            let y = inner.y + row_idx as u16;
            if y >= inner.bottom() {
                break;
            }
            let mut x = inner.x;
            for ch in line.chars() {
                let w = char_width(ch) as u16;
                if w == 0 {
                    continue;
                }
                if x + w > inner.right() {
                    break;
                }
                buf[(x, y)].set_char(ch).set_fg(FG).set_bg(BG);
                x += w;
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
            if display_width(remaining) <= max_width {
                lines.push(remaining);
                break;
            }

            // Find the byte index where the accumulated display width
            // would exceed max_width.
            let mut break_byte = remaining.len();
            let mut acc = 0;
            for (i, ch) in remaining.char_indices() {
                let w = char_width(ch);
                if acc + w > max_width {
                    break_byte = i;
                    break;
                }
                acc += w;
            }
            // Guard against a single char wider than the wrap width.
            if break_byte == 0 {
                break_byte = remaining
                    .chars()
                    .next()
                    .expect("bug: remaining is non-empty")
                    .len_utf8();
            }

            // Try to break at the last space within the first max_width columns
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

#[cfg(test)]
mod tests {
    use super::*;
    use unicode_width::UnicodeWidthStr;

    /// Render `input` into a fresh buffer of the given outer width and
    /// return the buffer, the inner text area, and the cursor position.
    fn render(input: &TextInput, outer_width: u16) -> (Buffer, Rect, Position) {
        let height = input.preferred_height(outer_width);
        let area = Rect::new(0, 0, outer_width, height);
        let mut buf = Buffer::empty(area);
        let result = input.render(area, &mut buf);
        let inner = Rect::new(
            area.x + PAD_X,
            area.y + PAD_Y,
            area.width - PAD_X * 2,
            area.height - PAD_Y * 2,
        );
        (buf, inner, result.cursor_position)
    }

    /// The glyphs drawn on inner row `row`, with trailing padding trimmed.
    fn row_text(buf: &Buffer, inner: Rect, row: u16) -> String {
        let y = inner.y + row;
        let s: String = (inner.x..inner.right())
            .map(|x| buf[(x, y)].symbol().to_owned())
            .collect();
        s.trim_end().to_owned()
    }

    /// Pastes from PowerPoint (and other Office apps) use `\r` for paragraph
    /// breaks and `\x0b` (vertical tab) for soft line breaks; Windows
    /// clipboards hand us `\r\n`. All of these must become plain `\n` so the
    /// text wraps onto separate lines instead of being glued together.
    #[test]
    fn normalize_line_breaks_handles_office_and_windows_endings() {
        assert_eq!(normalize_line_breaks("a\rb"), "a\nb");
        assert_eq!(normalize_line_breaks("a\r\nb"), "a\nb");
        assert_eq!(normalize_line_breaks("a\x0bb"), "a\nb");
        assert_eq!(normalize_line_breaks("a\x0cb"), "a\nb");
        // `\n` and `\r\r\n` keep their line count.
        assert_eq!(normalize_line_breaks("a\nb"), "a\nb");
        assert_eq!(normalize_line_breaks("a\r\r\nb"), "a\n\nb");
        assert_eq!(normalize_line_breaks("a\n\rb"), "a\n\nb");
        // Trailing `\r` at end of input.
        assert_eq!(normalize_line_breaks("a\r"), "a\n");
    }

    #[test]
    fn insert_str_normalizes_office_line_breaks() {
        let mut input = TextInput::new();
        input.insert_str("Slide title\rFirst point\x0bSecond point\r\nThird point\x0c");
        assert_eq!(
            input.text(),
            "Slide title\nFirst point\nSecond point\nThird point\n"
        );
        assert_eq!(input.cursor, input.buf.len());
    }

    /// Regression: after pasting PowerPoint text, the blinking cursor sat to
    /// the right of the end of the text. `\r` renders as nothing, but the
    /// cursor column counted it as one cell.
    #[test]
    fn cursor_sits_at_end_of_pasted_powerpoint_text() {
        let mut input = TextInput::new();
        input.insert_str("Slide title\rFirst point\x0bSecond point");

        let (buf, inner, cursor) = render(&input, 40);
        assert_eq!(row_text(&buf, inner, 0), "Slide title");
        assert_eq!(row_text(&buf, inner, 1), "First point");
        assert_eq!(row_text(&buf, inner, 2), "Second point");
        assert_eq!(
            cursor,
            Position::new(inner.x + "Second point".width() as u16, inner.y + 2)
        );
    }

    /// Even for control characters we don't normalize away, the cursor
    /// column must agree with what the renderer actually draws: a char the
    /// renderer skips must also count as zero columns for the cursor.
    #[test]
    fn cursor_column_matches_drawn_glyphs_with_control_chars() {
        for ctrl in ['\t', '\x7f', '\x1b', '\x01'] {
            let mut input = TextInput::new();
            // Bypass `insert_str` normalization to exercise the raw path.
            input.buf = format!("ab{ctrl}cd");
            input.cursor = input.buf.len();

            let (buf, inner, cursor) = render(&input, 40);
            let drawn = row_text(&buf, inner, 0);
            assert_eq!(
                cursor.x,
                inner.x + drawn.width() as u16,
                "cursor desynced from drawn text {drawn:?} for control char {ctrl:?}"
            );
        }
    }

    /// Moving the cursor left over a zero-width (skipped) control char must
    /// keep the cursor aligned with the glyph to its right.
    #[test]
    fn cursor_column_matches_mid_line_with_control_chars() {
        let mut input = TextInput::new();
        input.buf = "ab\x7fcd".to_owned();
        input.cursor = input.buf.len();
        input.move_left(); // before 'd'
        input.move_left(); // before 'c'

        let (_, inner, cursor) = render(&input, 40);
        assert_eq!(cursor.x, inner.x + 2, "cursor should sit before 'c'");
    }
}
