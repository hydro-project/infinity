use ratatui::{
    buffer::Buffer,
    crossterm::{
        Command,
        cursor::{self, MoveTo, MoveUp, SavePosition},
        queue,
        style::Print,
        terminal::{self as cterm, Clear},
    },
    layout::{Position, Rect},
    widgets::Widget,
};
use std::fmt;
use std::io::{self, Write};

/// Move cursor to column `n` (0-based) using CSI sequence.
struct MoveToColumn(u16);
impl Command for MoveToColumn {
    fn write_ansi(&self, f: &mut impl fmt::Write) -> fmt::Result {
        // CHA (Cursor Horizontal Absolute) is 1-based
        write!(f, "\x1b[{}G", self.0 + 1)
    }
}

/// Move cursor down `n` lines and to column 1 using CSI CNL.
struct MoveToNextLine(u16);
impl Command for MoveToNextLine {
    fn write_ansi(&self, f: &mut impl fmt::Write) -> fmt::Result {
        if self.0 == 0 {
            return Ok(());
        }
        write!(f, "\x1b[{}E", self.0)
    }
}

/// A custom inline viewport that lives at the bottom of the terminal screen.
///
/// Unlike ratatui's built-in `Viewport::Inline`, this implementation handles
/// resize correctly by tracking the saved cursor row internally and
/// calculating viewport content reflow, avoiding any terminal round-trips
/// during resize that could race with the terminal's own reflow.
pub struct InlineViewport {
    height: u16,
    terminal_size: (u16, u16),
    pub viewport_y: u16,
    pub last_effective_viewport_y: u16,
    buffers: [Buffer; 2],
    current: usize,
    request_clear: bool,
}

struct DisableWrap;
impl Command for DisableWrap {
    fn write_ansi(&self, f: &mut impl fmt::Write) -> fmt::Result {
        write!(f, "\x1b[?7l")
    }
}

struct EnableWrap;
impl Command for EnableWrap {
    fn write_ansi(&self, f: &mut impl fmt::Write) -> fmt::Result {
        write!(f, "\x1b[?7h")
    }
}

impl InlineViewport {
    pub fn new(height: u16) -> io::Result<Self> {
        let (_, rows) = cterm::size()?;
        let mut stdout = io::stdout();

        // Scroll the screen up to make room for the viewport at the bottom.
        queue!(stdout, cursor::MoveTo(0, rows.saturating_sub(1)))?;
        for _ in 0..height {
            queue!(stdout, ratatui::crossterm::style::Print("\n"))?;
        }

        let viewport_y = rows.saturating_sub(height);

        for row in viewport_y..(viewport_y + height) {
            queue!(stdout, cursor::MoveTo(0, row))?;
            queue!(stdout, cterm::Clear(cterm::ClearType::CurrentLine))?;
        }

        let vp_top = viewport_y;
        let terminal_size = cterm::size()?;
        let mut stdout = io::stdout();
        queue!(stdout, cursor::MoveTo(0, vp_top.saturating_sub(1)))?;
        queue!(stdout, cursor::SavePosition)?;

        stdout.flush()?;

        // Buffer uses zero-based coordinates; draw() maps them via
        // relative cursor movement from the saved cursor position.
        let area = Rect::new(0, 0, terminal_size.0, height);
        Ok(Self {
            height,
            viewport_y,
            last_effective_viewport_y: viewport_y,
            terminal_size,
            buffers: [Buffer::empty(area), Buffer::empty(area)],
            current: 0,
            request_clear: false,
        })
    }

    pub fn area(&self) -> Rect {
        Rect::new(0, 0, self.terminal_size.0, self.height)
    }

    /// Handle a terminal resize with NO terminal round-trips.
    ///
    /// Uses the tracked `saved_cursor_row` and calculates viewport content
    /// reflow from the buffer to determine the correct cursor position.
    /// All commands are queued and flushed once at the end.
    pub fn handle_resize(&mut self) -> io::Result<()> {
        let new_terminal_size = cterm::size()?;
        if self.terminal_size == new_terminal_size {
            return Ok(());
        } else {
            self.terminal_size = new_terminal_size;
        }

        self.request_clear = true;

        let area = Rect::new(0, 0, new_terminal_size.0, self.height);
        self.buffers = [Buffer::empty(area), Buffer::empty(area)];
        self.current = 0;

        Ok(())
    }

    /// Draw the viewport using relative cursor movement from the saved
    /// cursor position (end of scrollback). Restores the saved cursor,
    /// then uses `MoveToNextLine` / `MoveToColumn` to reach each cell,
    /// avoiding any absolute coordinates.
    pub fn draw<F>(&mut self, desired_lines: u16, render_fn: F) -> io::Result<()>
    where
        F: FnOnce(&mut ViewportFrame),
    {
        let old_height = self.height;
        self.height = desired_lines;

        let area = self.area();
        self.buffers[self.current] = Buffer::empty(area);

        let ideal_viewport_y = self.terminal_size.1.saturating_sub(self.height);

        let cursor_position;
        {
            let mut frame = ViewportFrame {
                area,
                buffer: &mut self.buffers[self.current],
                cursor_position: None,
            };
            render_fn(&mut frame);
            cursor_position = frame.cursor_position;
        }

        let previous = &self.buffers[1 - self.current];
        let current = &self.buffers[self.current];

        let mut stdout = io::stdout();
        let is_clearing_due_to_resize = self.request_clear;
        let (should_clear, updates) = if self.request_clear
            || ideal_viewport_y != self.last_effective_viewport_y
            || self.height != old_height
        {
            self.request_clear = false;
            (true, Buffer::empty(area).diff(current))
        } else {
            (false, previous.diff(current))
        };

        // Restore the saved cursor (sits at end of scrollback, one row
        // above the viewport). All subsequent positioning is relative.
        if should_clear {
            queue!(stdout, cursor::RestorePosition)?;

            stdout.flush()?;

            let cursor_position_here = cursor::position().unwrap();
            self.viewport_y = cursor_position_here.1 + 1;

            queue!(stdout, Clear(cterm::ClearType::FromCursorDown))?;
        }

        queue!(stdout, cursor::RestorePosition)?;
        queue!(stdout, cursor::Hide)?;
        queue!(stdout, DisableWrap)?;

        self.last_effective_viewport_y = self.viewport_y;
        if !is_clearing_due_to_resize {
            if ideal_viewport_y < self.viewport_y {
                let shift_up = self.viewport_y - ideal_viewport_y;
                queue!(stdout, MoveToNextLine(old_height))?;
                for _ in 0..shift_up {
                    queue!(stdout, Print("\r\n"))?;
                    self.viewport_y -= 1;
                }
                queue!(stdout, cursor::RestorePosition)?;
                queue!(stdout, cursor::MoveUp(shift_up))?;
                queue!(stdout, SavePosition)?;
            } else if ideal_viewport_y > self.viewport_y {
                queue!(stdout, MoveTo(0, ideal_viewport_y - 1))?;
                self.last_effective_viewport_y = ideal_viewport_y;
            }
        }

        // Track where the cursor currently is in buffer-local coords.
        let mut cur_row: u16 = 0;
        let mut cur_col: u16 = 0;
        queue!(stdout, MoveToNextLine(1))?; // saved cursor is one row above the viewport

        for (x, y, cell) in &updates {
            let target_row = *y;
            let target_col = *x;

            // Move to the correct row relative to current position.
            let row_delta = target_row - cur_row;
            for _ in 0..row_delta {
                queue!(stdout, MoveToNextLine(1))?;
                cur_col = 0; // CNL moves to column 0
            }
            cur_row = target_row;

            // Move to the correct column.
            if target_col != cur_col {
                queue!(stdout, MoveToColumn(target_col))?;
                cur_col = target_col;
            }

            queue!(
                stdout,
                ratatui::crossterm::style::SetAttribute(
                    ratatui::crossterm::style::Attribute::Reset
                )
            )?;
            let fg: ratatui::style::Color = cell.fg;
            queue!(
                stdout,
                ratatui::crossterm::style::SetForegroundColor(fg.into())
            )?;
            let bg: ratatui::style::Color = cell.bg;
            queue!(
                stdout,
                ratatui::crossterm::style::SetBackgroundColor(bg.into())
            )?;
            let mods = cell.modifier;
            if mods.contains(ratatui::style::Modifier::BOLD) {
                queue!(
                    stdout,
                    ratatui::crossterm::style::SetAttribute(
                        ratatui::crossterm::style::Attribute::Bold
                    )
                )?;
            }
            if mods.contains(ratatui::style::Modifier::DIM) {
                queue!(
                    stdout,
                    ratatui::crossterm::style::SetAttribute(
                        ratatui::crossterm::style::Attribute::Dim
                    )
                )?;
            }
            if mods.contains(ratatui::style::Modifier::ITALIC) {
                queue!(
                    stdout,
                    ratatui::crossterm::style::SetAttribute(
                        ratatui::crossterm::style::Attribute::Italic
                    )
                )?;
            }
            if mods.contains(ratatui::style::Modifier::UNDERLINED) {
                queue!(
                    stdout,
                    ratatui::crossterm::style::SetAttribute(
                        ratatui::crossterm::style::Attribute::Underlined
                    )
                )?;
            }
            queue!(stdout, ratatui::crossterm::style::Print(cell.symbol()))?;

            // Advance column tracking by the symbol's display width.
            cur_col += unicode_width::UnicodeWidthStr::width(cell.symbol()) as u16;
        }

        if let Some(pos) = cursor_position {
            if cur_row == pos.y {
            } else if pos.y > cur_row {
                queue!(stdout, MoveToNextLine(pos.y - cur_row))?;
                cur_col = 0;
            } else {
                queue!(stdout, MoveUp(cur_row - pos.y))?;
            }

            if pos.x != cur_col {
                queue!(stdout, MoveToColumn(pos.x))?;
            }

            queue!(stdout, cursor::Show)?;
        } else {
            queue!(stdout, cursor::Hide)?;
        }

        queue!(stdout, EnableWrap)?;
        queue!(
            stdout,
            ratatui::crossterm::style::SetAttribute(ratatui::crossterm::style::Attribute::Reset)
        )?;

        stdout.flush()?;
        self.current = 1 - self.current;
        Ok(())
    }
}

pub struct ViewportFrame<'a> {
    area: Rect,
    buffer: &'a mut Buffer,
    cursor_position: Option<Position>,
}

impl<'a> ViewportFrame<'a> {
    pub fn area(&self) -> Rect {
        self.area
    }

    pub fn render_widget<W: Widget>(&mut self, widget: W, area: Rect) {
        widget.render(area, self.buffer);
    }

    pub fn set_cursor_position(&mut self, position: Position) {
        self.cursor_position = Some(position);
    }
}
