use ratatui::{
    buffer::Buffer,
    layout::{Position, Rect},
    style::Color,
    widgets::Widget,
};

const PAD_X: u16 = 1;
const PAD_Y: u16 = 1;
const BG: Color = Color::Rgb(40, 40, 40);
const FG: Color = Color::White;

/// A multi-line text input with its own wrapping, cursor tracking, and rendering.
pub struct TextInput {
    buf: String,
    /// Byte-offset cursor position within `buf`.
    cursor: usize,
}

/// Snapshot returned by `render_state` so the caller can place the terminal cursor.
pub struct RenderResult {
    /// Absolute screen position where the blinking cursor should be placed.
    pub cursor_position: Position,
}

impl TextInput {
    pub fn new() -> Self {
        Self {
            buf: String::new(),
            cursor: 0,
        }
    }

    // ── Public mutation API ──────────────────────────────────────────

    pub fn insert_char(&mut self, ch: char) {
        self.buf.insert(self.cursor, ch);
        self.cursor += ch.len_utf8();
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
            let next = self.cursor + self.buf[self.cursor..].chars().next().unwrap().len_utf8();
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
            self.cursor += self.buf[self.cursor..].chars().next().unwrap().len_utf8();
        }
    }

    pub fn move_home(&mut self) {
        self.cursor = 0;
    }

    pub fn move_end(&mut self) {
        self.cursor = self.buf.len();
    }

    /// Take the current text, clear the buffer, and reset the cursor.
    pub fn take_text(&mut self) -> String {
        self.cursor = 0;
        std::mem::take(&mut self.buf)
    }

    pub fn is_empty(&self) -> bool {
        self.buf.trim().is_empty()
    }

    // ── Layout ──────────────────────────────────────────────────────

    /// The total height this widget wants given a particular outer width.
    /// Includes top/bottom padding rows.
    pub fn preferred_height(&self, outer_width: u16) -> u16 {
        let inner_w = outer_width.saturating_sub(PAD_X * 2);
        if inner_w == 0 {
            return PAD_Y * 2 + 1;
        }
        let lines = wrap_lines(&self.buf, inner_w as usize);
        let mut text_rows = lines.len().max(1) as u16;

        // If the cursor sits past a full last line, it will render on an
        // extra row — make sure we reserve space for it.
        if let Some(last) = lines.last() {
            if last.chars().count() >= inner_w as usize && self.cursor == self.buf.len() {
                text_rows += 1;
            }
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
        if !found_cursor {
            if let Some(last) = lines.last() {
                let last_full = last.chars().count() >= inner.width as usize;
                if last_full && self.cursor == self.buf.len() {
                    cursor_row = lines.len() as u16;
                    cursor_col = 0;
                } else {
                    cursor_row = lines.len().saturating_sub(1) as u16;
                    cursor_col = last.chars().count() as u16;
                }
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
    let mut remaining = text;

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
        let segment = &remaining[..break_byte];
        if let Some(space_pos) = segment.rfind(' ') {
            lines.push(&remaining[..space_pos]);
            // Skip the space
            remaining = &remaining[space_pos + 1..];
        } else {
            // No space found — hard break at max_width
            lines.push(segment);
            remaining = &remaining[break_byte..];
        }
    }

    lines
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
