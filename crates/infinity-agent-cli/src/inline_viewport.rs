use crate::modifier_diff::ModifierDiff;
use crate::term_io::TermOut;
use ratatui::{
    buffer::Buffer,
    crossterm::{
        Command,
        cursor::{self},
        queue,
        style::{
            Attribute as CAttribute, Color as CColor, Colors, Print, SetAttribute,
            SetBackgroundColor, SetColors, SetForegroundColor,
        },
        terminal::{self as cterm},
    },
    layout::{Position, Rect},
    style::{Color, Modifier},
    text::{Line, Span},
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

/// Move cursor down `n` lines and to column 1.
///
/// Emits CUD (`CSI B`) followed by CR rather than CNL (`CSI E`): the two are
/// equivalent, but CUD+CR is more widely supported by terminal
/// implementations (including the `vt100` crate used in tests).
struct MoveToNextLine(u16);
impl Command for MoveToNextLine {
    fn write_ansi(&self, f: &mut impl fmt::Write) -> fmt::Result {
        if self.0 == 0 {
            return Ok(());
        }
        write!(f, "\x1b[{}B\r", self.0)
    }
}

/// A custom inline viewport that lives at the bottom of the terminal screen.
///
/// Unlike ratatui's built-in `Viewport::Inline`, this implementation handles
/// resize correctly by re-deriving its anchor from a single cursor query and
/// local knowledge of what it drew, avoiding terminal round-trips during
/// normal operation entirely.
///
/// # Anchor tracking
///
/// All positioning is relative to the *anchor*: the DECSC saved cursor
/// position, which sits at the end of the scrollback content, one row above
/// the viewport (mid-line while a response is streaming). Reflowing
/// terminals (kitty/alacritty/VTE) translate the **live** cursor through a
/// resize reflow but merely clamp the saved cursor, so after a resize the
/// anchor must be reconstructed from the live cursor:
///
/// * when the cursor is hidden it is parked *on* the anchor, so the queried
///   position is the anchor;
/// * when the cursor is shown in the viewport, we compute how many rows the
///   previously drawn viewport rows rewrap to at the new width (we know
///   exactly what we drew) and subtract;
/// * non-reflowing terminals (xterm-class) keep everything in place — they
///   are detected by the queried position matching the old one, in which
///   case the old anchor remains valid.
///
/// The anchor's column (end of a partially streamed line) is tracked by
/// parsing everything printed through [`InlineViewport::print_above`], so it
/// can be recomputed for any width without asking the terminal.
///
/// All terminal output and queries go through the owned [`TermOut`], so the
/// viewport can be driven against a virtual terminal in tests.
pub struct InlineViewport<T: TermOut> {
    term: T,
    height: u16,
    terminal_size: (u16, u16),
    /// Anchor row + 1: the row the viewport content starts on (unless it is
    /// pinned lower, see `last_effective_viewport_y`).
    viewport_y: u16,
    /// Where the viewport content actually starts: equal to `viewport_y`, or
    /// further down when the viewport is pinned to the bottom of a terminal
    /// that has more room than the scrollback content needs.
    last_effective_viewport_y: u16,
    buffers: [Buffer; 2],
    current: usize,
    request_clear: bool,
    /// The terminal geometry changed since the anchor was last verified; the
    /// next draw or print must re-derive it (one cursor query).
    anchor_stale: bool,
    /// Column of the anchor within its row, in the current geometry.
    anchor_col: u16,
    /// Total display width of the logical line the anchor sits at the end of
    /// (everything printed since the last CR/LF), used to recompute
    /// `anchor_col` after a reflowing resize rewraps the line.
    anchor_line_len: u32,
    /// Where the live terminal cursor was left by the last draw.
    live_cursor: LiveCursor,
    /// Occupied lengths of the viewport rows as last drawn, captured when a
    /// resize invalidates the buffers; used by `re_anchor` to emulate how a
    /// reflowing terminal rewrapped them.
    pre_resize_row_lens: Option<Vec<u16>>,
    /// Terminal width before the pending resize(s), captured with
    /// `pre_resize_row_lens`.
    pre_resize_width: u16,
}

/// Where the live terminal cursor rests between draws.
#[derive(Clone, Copy)]
enum LiveCursor {
    /// Hidden and parked on the anchor, so that a reflowing terminal
    /// translates the anchor for us across resizes.
    Parked,
    /// Visible at this viewport-relative position (the text input cursor).
    Shown { x: u16, y: u16 },
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

pub(crate) struct SetScrollRegion(std::ops::Range<u16>);
impl Command for SetScrollRegion {
    fn write_ansi(&self, f: &mut impl fmt::Write) -> fmt::Result {
        write!(f, "\x1b[{};{}r", self.0.start, self.0.end)
    }
}

pub(crate) struct ResetScrollRegion;
impl Command for ResetScrollRegion {
    fn write_ansi(&self, f: &mut impl fmt::Write) -> fmt::Result {
        write!(f, "\x1b[r")
    }
}

fn write_spans<'a>(
    w: &mut impl Write,
    spans: impl Iterator<Item = &'a Span<'a>>,
) -> io::Result<()> {
    let mut fg = Color::Reset;
    let mut bg = Color::Reset;
    let mut mods = Modifier::empty();

    for span in spans {
        let mut next = mods;
        next.insert(span.style.add_modifier);
        next.remove(span.style.sub_modifier);
        if next != mods {
            ModifierDiff {
                from: mods,
                to: next,
            }
            .queue(w)?;
            mods = next;
        }

        let nfg = span.style.fg.unwrap_or(Color::Reset);
        let nbg = span.style.bg.unwrap_or(Color::Reset);
        if nfg != fg || nbg != bg {
            queue!(w, SetColors(Colors::new(nfg.into(), nbg.into())))?;
            fg = nfg;
            bg = nbg;
        }

        queue!(w, Print(&span.content))?;
    }

    queue!(
        w,
        SetForegroundColor(CColor::Reset),
        SetBackgroundColor(CColor::Reset),
        SetAttribute(CAttribute::Reset),
    )
}

/// Tracks the cursor column and line advances of bytes printed into the
/// scroll region, so the viewport always knows where the anchor ends up
/// without querying the terminal.
///
/// Understands CR/LF/tab stops, skips SGR styling sequences, measures
/// printable text by display width, and accounts for auto-wrap at the
/// terminal width. Anything else printed through
/// [`InlineViewport::print_above`] would silently desynchronize the anchor
/// tracking, so unrecognized control bytes and escape sequences panic.
struct OutputTracker {
    width: u16,
    /// Display width of the logical line printed so far (since last CR/LF).
    line_len: u32,
    /// Number of times the output moved down a row (LF or auto-wrap).
    line_advances: u32,
    state: TrackerState,
}

enum TrackerState {
    Ground,
    Esc,
    Csi,
}

impl OutputTracker {
    fn new(width: u16, line_len: u32) -> Self {
        Self {
            width: width.max(1),
            line_len,
            line_advances: 0,
            state: TrackerState::Ground,
        }
    }

    fn track(&mut self, bytes: &[u8]) {
        // Printable runs are decoded as UTF-8 for display-width measurement;
        // `bytes` always comes from complete formatted strings, so char
        // boundaries are intact within a run.
        let mut run_start: Option<usize> = None;
        let flush_run = |this: &mut Self, start: Option<usize>, end: usize| {
            let Some(start) = start else { return };
            let text = std::str::from_utf8(&bytes[start..end])
                .expect("bug: print_above wrote invalid UTF-8");
            for ch in text.chars() {
                let w = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0) as u32;
                let col = this.line_len % this.width as u32;
                if col + w > this.width as u32 {
                    // Auto-wrap: the char starts on the next row.
                    this.line_advances += 1;
                }
                this.line_len += w;
            }
        };

        for (i, &b) in bytes.iter().enumerate() {
            match self.state {
                TrackerState::Ground => match b {
                    0x1b => {
                        flush_run(self, run_start.take(), i);
                        self.state = TrackerState::Esc;
                    }
                    b'\r' => {
                        flush_run(self, run_start.take(), i);
                        self.line_len = 0;
                    }
                    b'\n' => {
                        flush_run(self, run_start.take(), i);
                        self.line_advances += 1;
                        self.line_len = 0;
                    }
                    b'\t' => {
                        // Advance to the next 8-column tab stop.
                        flush_run(self, run_start.take(), i);
                        self.line_len += 8 - (self.line_len % 8);
                    }
                    0x00..=0x1f | 0x7f => {
                        panic!("bug: print_above wrote untracked control byte {b:#04x}");
                    }
                    _ => {
                        if run_start.is_none() {
                            run_start = Some(i);
                        }
                    }
                },
                TrackerState::Esc => {
                    assert!(
                        b == b'[',
                        "bug: print_above wrote untracked escape sequence ESC {:?}",
                        b as char
                    );
                    self.state = TrackerState::Csi;
                }
                TrackerState::Csi => {
                    if (0x40..=0x7e).contains(&b) {
                        // Only SGR (styling) is cursor-neutral; anything
                        // else would desynchronize the anchor tracking.
                        assert!(
                            b == b'm',
                            "bug: print_above wrote untracked CSI sequence ending in {:?}",
                            b as char
                        );
                        self.state = TrackerState::Ground;
                    }
                }
            }
        }
        flush_run(self, run_start.take(), bytes.len());
    }
}

/// Display-width position after the last cell of `row` that a reflowing
/// terminal would consider occupied (alacritty/kitty/VTE reflow rows up to
/// their last non-empty cell; a cell is non-empty if it has a visible glyph,
/// any color, or a visible attribute).
fn occupied_row_len(buffer: &Buffer, row: u16) -> u16 {
    let area = buffer.area;
    let mut col: u16 = 0;
    let mut occupied: u16 = 0;
    for x in area.x..area.right() {
        let cell = &buffer[(x, row)];
        let width = unicode_width::UnicodeWidthStr::width(cell.symbol()).max(1) as u16;
        col += width;
        let visible_attrs = Modifier::UNDERLINED
            .union(Modifier::REVERSED)
            .union(Modifier::CROSSED_OUT);
        if cell.symbol() != " "
            || cell.fg != Color::Reset
            || cell.bg != Color::Reset
            || cell.modifier.intersects(visible_attrs)
        {
            occupied = col;
        }
    }
    occupied
}

/// Rows a line of `len` occupied cells takes after rewrapping to `width`
/// columns (at least one).
fn rewrapped_rows(len: u16, width: u16) -> u32 {
    (len.max(1) as u32).div_ceil(width.max(1) as u32)
}

/// Column where a logical line of display width `len` ends after wrapping at
/// `width` columns; an exactly full row pends at the last column.
fn line_end_col(len: u32, width: u16) -> u16 {
    let width = width.max(1);
    let rem = (len % width as u32) as u16;
    if rem == 0 && len > 0 { width - 1 } else { rem }
}

impl<T: TermOut> InlineViewport<T> {
    pub fn new(mut term: T, height: u16) -> io::Result<Self> {
        let (_, rows) = term.size()?;

        // Scroll the screen up to make room for the viewport at the bottom.
        queue!(term, cursor::MoveTo(0, rows.saturating_sub(1)))?;
        for _ in 0..height {
            queue!(term, Print("\n"))?;
        }

        let viewport_y = rows.saturating_sub(height);

        for row in viewport_y..(viewport_y + height) {
            queue!(term, cursor::MoveTo(0, row))?;
            queue!(term, cterm::Clear(cterm::ClearType::CurrentLine))?;
        }

        let vp_top = viewport_y;
        let terminal_size = term.size()?;
        queue!(term, cursor::MoveTo(0, vp_top.saturating_sub(1)))?;
        queue!(term, cursor::SavePosition)?;

        term.flush()?;

        // Buffer uses zero-based coordinates; draw() maps them via
        // relative cursor movement from the saved cursor position.
        let area = Rect::new(0, 0, terminal_size.0, height);
        Ok(Self {
            term,
            height,
            viewport_y,
            last_effective_viewport_y: viewport_y,
            terminal_size,
            buffers: [Buffer::empty(area), Buffer::empty(area)],
            current: 0,
            request_clear: false,
            anchor_stale: false,
            anchor_col: 0,
            anchor_line_len: 0,
            live_cursor: LiveCursor::Parked,
            pre_resize_row_lens: None,
            pre_resize_width: terminal_size.0,
        })
    }

    /// Direct access to the underlying terminal, e.g. for cleanup sequences
    /// or setting the terminal title.
    pub fn term_mut(&mut self) -> &mut T {
        &mut self.term
    }

    pub fn area(&self) -> Rect {
        Rect::new(0, 0, self.terminal_size.0, self.height)
    }

    /// Handle a terminal resize with NO terminal output or queries.
    ///
    /// Marks the anchor stale; the next draw or print re-derives it with a
    /// single cursor-position query (see [`InlineViewport::re_anchor`]).
    pub fn handle_resize(&mut self) -> io::Result<()> {
        let new_terminal_size = self.term.size()?;
        if self.terminal_size == new_terminal_size {
            return Ok(());
        }

        // Capture how the viewport rows were drawn before dropping the
        // buffers, so `re_anchor` can emulate how the terminal rewraps them.
        // On coalesced resizes only the first capture matters: rewrapping is
        // a function of the logical lines, which don't change.
        if !self.anchor_stale {
            let frame = &self.buffers[1 - self.current];
            let lens: Vec<u16> = (0..frame.area.height)
                .map(|row| occupied_row_len(frame, row))
                .collect();
            self.pre_resize_row_lens = Some(lens);
            self.pre_resize_width = self.terminal_size.0;
        }

        self.terminal_size = new_terminal_size;
        self.request_clear = true;
        self.anchor_stale = true;

        let area = Rect::new(0, 0, new_terminal_size.0, self.height);
        self.buffers = [Buffer::empty(area), Buffer::empty(area)];
        self.current = 0;

        Ok(())
    }

    /// Re-derive the anchor after a resize, with a single cursor query.
    ///
    /// See the type-level docs for the strategy. Ends with the live cursor
    /// parked on the (re-saved) anchor.
    fn re_anchor(&mut self) -> io::Result<()> {
        let (cols, rows) = self.terminal_size;
        if cols == 0 || rows == 0 {
            self.anchor_stale = false;
            return Ok(());
        }

        self.term.flush()?;
        let (live_col, live_row) = self.term.cursor_position()?;
        let pre_resize_row_lens = self.pre_resize_row_lens.take();

        // Where the live cursor would be if the terminal moved nothing
        // (xterm-class: truncate/pad in place). If the live cursor isn't
        // exactly there — including when it merely got clamped to the new
        // bounds — the terminal moved content and the anchor must be
        // reconstructed from the live cursor.
        let (prev_col, prev_row) = match self.live_cursor {
            LiveCursor::Parked => (self.anchor_col, self.viewport_y.saturating_sub(1)),
            LiveCursor::Shown { x, y } => (x, self.last_effective_viewport_y + y),
        };
        let unmoved = live_col == prev_col && live_row == prev_row;
        let width_changed = self.pre_resize_width != cols;

        let (anchor_row, anchor_col) = match self.live_cursor {
            _ if unmoved => {
                // The viewport rows did not move, so the anchor row is where
                // it was. The anchor *column* is still ambiguous when its
                // line is wrapped: on a width grow, a reflowing terminal
                // merges the line without moving anything below it. Use the
                // reflowed column — on a non-reflowing terminal that leaves
                // at most a cosmetic gap within the row, whereas assuming
                // truncation on a reflowing terminal would overwrite the
                // merged tail (data loss).
                let row = self.viewport_y.saturating_sub(1).min(rows - 1);
                if width_changed && cols < self.pre_resize_width {
                    // A width shrink that moved nothing only happens on
                    // non-reflowing terminals, which truncate the line.
                    let col = self.anchor_col.min(cols - 1);
                    self.anchor_line_len = col as u32;
                    (row, col)
                } else {
                    (row, line_end_col(self.anchor_line_len, cols))
                }
            }
            LiveCursor::Parked => {
                // The parked cursor *is* the anchor; the reflow tracked it.
                (live_row, live_col)
            }
            LiveCursor::Shown { x, y } => {
                // Count the rows between the anchor and the live cursor in
                // the new geometry: the blank rows between the anchor and
                // the viewport top never rewrap; each drawn viewport row
                // above the cursor rewraps to ceil(len / cols) rows; the
                // cursor's own row contributes the rows above the cursor
                // cell.
                let gap_rows = (self.last_effective_viewport_y - self.viewport_y) as u32 + 1;
                let mut offset = gap_rows + (x / cols) as u32;
                let lens = pre_resize_row_lens
                    .as_ref()
                    .expect("bug: resize with a shown cursor must capture the drawn rows");
                for len in lens.iter().take(y as usize) {
                    offset += rewrapped_rows(*len, cols);
                }
                let row = (live_row as u32).saturating_sub(offset) as u16;
                let col = line_end_col(self.anchor_line_len, cols);
                (row.min(rows - 1), col)
            }
        };

        self.viewport_y = anchor_row + 1;
        self.last_effective_viewport_y = self.viewport_y;
        self.anchor_col = anchor_col;
        queue!(self.term, cursor::Hide)?;
        queue!(self.term, cursor::MoveTo(anchor_col, anchor_row))?;
        queue!(self.term, cursor::SavePosition)?;
        self.live_cursor = LiveCursor::Parked;
        self.anchor_stale = false;
        Ok(())
    }

    pub fn print_above(
        &mut self,
        writer: impl FnOnce(&mut Vec<u8>) -> io::Result<()>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        // A print can race the Resize event: the terminal has already
        // resized (and reflowed) but the event hasn't been read yet. The
        // size query is a cheap ioctl (not a terminal round-trip), so check
        // on every print and repair the anchor first if geometry changed.
        self.handle_resize()?;
        if self.anchor_stale {
            self.re_anchor()?;
        }

        let mut buf = Vec::new();
        writer(&mut buf)?;

        let mut tracker = OutputTracker::new(self.terminal_size.0, self.anchor_line_len);
        tracker.track(&buf);

        // A scroll region needs at least two rows (`CSI 1;1r` is invalid and
        // ignored). On terminals too small for one, print without a region —
        // overwriting viewport rows — and repaint the viewport right after.
        let degenerate_region = self.last_effective_viewport_y < 2;

        let term = &mut self.term;
        queue!(term, cursor::Hide)?;
        if !degenerate_region {
            queue!(term, SetScrollRegion(1..self.last_effective_viewport_y))?;
        }
        queue!(term, cursor::RestorePosition)?;
        term.write_all(&buf)?;
        queue!(term, cursor::SavePosition)?;
        if !degenerate_region {
            queue!(term, ResetScrollRegion)?;
        }
        // DECSTBM homes the cursor; park it back on the anchor.
        queue!(term, cursor::RestorePosition)?;
        self.live_cursor = LiveCursor::Parked;

        // Track where the print left the anchor. Its column follows the
        // printed text; its row advances with each printed line until it
        // reaches the scroll-region bottom, after which LF scrolls the
        // region content instead of moving the cursor.
        self.anchor_line_len = tracker.line_len;
        self.anchor_col = line_end_col(tracker.line_len, self.terminal_size.0);
        let advanced = self.viewport_y as u32 + tracker.line_advances;
        if degenerate_region {
            // Without a region the anchor advances until the screen bottom,
            // and the print may have overwritten viewport rows.
            self.viewport_y = advanced.min(self.terminal_size.1 as u32) as u16;
            self.request_clear = true;
        } else {
            self.viewport_y = advanced.min(self.last_effective_viewport_y as u32) as u16;
        }

        Ok(())
    }

    pub fn print_line_above(
        &mut self,
        line: Line<'_>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        self.print_above(|w| {
            write!(w, "\r\n")?;
            write_spans(w, line.iter())
        })
    }

    pub fn print_spans_above(
        &mut self,
        line: Line<'_>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        self.print_above(|w| write_spans(w, line.iter()))
    }

    /// Draw the viewport using relative cursor movement from the saved
    /// cursor position (end of scrollback). Restores the saved cursor,
    /// then uses `MoveToNextLine` / `MoveToColumn` to reach each cell,
    /// avoiding any absolute coordinates.
    pub fn draw<F>(&mut self, desired_lines: u16, render_fn: F) -> io::Result<()>
    where
        F: FnOnce(&mut ViewportFrame),
    {
        // Keep one row above the viewport free for the anchor, even on tiny
        // terminals.
        let desired_lines = desired_lines
            .min(self.terminal_size.1.saturating_sub(1))
            .max(1);
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

        // A resize since the last draw invalidated the anchor; re-derive it
        // (the only cursor query in the entire viewport) before positioning.
        if self.anchor_stale {
            self.re_anchor()?;
        }

        let previous = &self.buffers[1 - self.current];
        let current = &self.buffers[self.current];

        let term = &mut self.term;
        let (should_clear, updates) = if self.request_clear
            || ideal_viewport_y != self.last_effective_viewport_y
            || self.height != old_height
        {
            self.request_clear = false;
            (true, Buffer::empty(area).diff(current))
        } else {
            (false, previous.diff(current))
        };

        // Restore the saved cursor (the anchor, at end of scrollback one
        // row above the viewport). All subsequent positioning is relative.
        //
        // A clear + full repaint could be observed half-done by the
        // terminal; wrap it in a synchronized update (`CSI ?2026`) so
        // supporting terminals apply it atomically (others ignore it).
        if should_clear {
            queue!(term, cterm::BeginSynchronizedUpdate)?;
            queue!(term, cursor::RestorePosition)?;
            queue!(term, cterm::Clear(cterm::ClearType::FromCursorDown))?;
        }

        queue!(term, cursor::RestorePosition)?;
        queue!(term, cursor::Hide)?;
        queue!(term, DisableWrap)?;

        self.last_effective_viewport_y = self.viewport_y;
        if ideal_viewport_y < self.viewport_y {
            // Not enough rows below the anchor: scroll the screen up.
            let shift_up = self.viewport_y - ideal_viewport_y;
            queue!(term, MoveToNextLine(self.terminal_size.1))?; // clamps at the bottom row
            for _ in 0..shift_up {
                queue!(term, Print("\r\n"))?;
                self.viewport_y -= 1;
            }
            queue!(term, cursor::RestorePosition)?;
            queue!(term, cursor::MoveUp(shift_up))?;
            queue!(term, cursor::SavePosition)?;
            self.last_effective_viewport_y = self.viewport_y;
        } else if ideal_viewport_y > self.viewport_y {
            // More room than the scrollback needs: pin the viewport to the
            // bottom, leaving the anchor and a blank gap above it. The gap
            // is not wasted: prints consume it (the anchor advances into it)
            // before any scrolling resumes. Keeping the viewport at the
            // bottom also means a later reflow pushes at most blank gap rows
            // into scrollback — never stale viewport rows.
            queue!(term, cursor::MoveTo(0, ideal_viewport_y - 1))?;
            self.last_effective_viewport_y = ideal_viewport_y;
        }

        // Track where the cursor currently is in buffer-local coords.
        let mut cur_row: u16 = 0;
        let mut cur_col: u16 = 0;
        queue!(term, MoveToNextLine(1))?; // saved cursor is one row above the viewport

        for (x, y, cell) in &updates {
            let target_row = *y;
            let target_col = *x;

            // Move to the correct row relative to current position.
            let row_delta = target_row - cur_row;
            for _ in 0..row_delta {
                queue!(term, MoveToNextLine(1))?;
                cur_col = 0; // CNL moves to column 0
            }
            cur_row = target_row;

            // Move to the correct column.
            if target_col != cur_col {
                queue!(term, MoveToColumn(target_col))?;
                cur_col = target_col;
            }

            queue!(
                term,
                SetAttribute(ratatui::crossterm::style::Attribute::Reset)
            )?;
            let fg: Color = cell.fg;
            queue!(term, SetForegroundColor(fg.into()))?;
            let bg: Color = cell.bg;
            queue!(term, SetBackgroundColor(bg.into()))?;
            let mods = cell.modifier;
            if mods.contains(Modifier::BOLD) {
                queue!(
                    term,
                    SetAttribute(ratatui::crossterm::style::Attribute::Bold)
                )?;
            }
            if mods.contains(Modifier::DIM) {
                queue!(
                    term,
                    SetAttribute(ratatui::crossterm::style::Attribute::Dim)
                )?;
            }
            if mods.contains(Modifier::ITALIC) {
                queue!(
                    term,
                    SetAttribute(ratatui::crossterm::style::Attribute::Italic)
                )?;
            }
            if mods.contains(Modifier::UNDERLINED) {
                queue!(
                    term,
                    SetAttribute(ratatui::crossterm::style::Attribute::Underlined)
                )?;
            }
            queue!(term, Print(cell.symbol()))?;

            // Advance column tracking by the symbol's display width.
            cur_col += unicode_width::UnicodeWidthStr::width(cell.symbol()) as u16;
        }

        if let Some(pos) = cursor_position {
            if cur_row == pos.y {
            } else if pos.y > cur_row {
                queue!(term, MoveToNextLine(pos.y - cur_row))?;
                cur_col = 0;
            } else {
                queue!(term, cursor::MoveUp(cur_row - pos.y))?;
            }

            if pos.x != cur_col {
                queue!(term, MoveToColumn(pos.x))?;
            }

            queue!(term, cursor::Show)?;
            self.live_cursor = LiveCursor::Shown { x: pos.x, y: pos.y };
        } else {
            queue!(term, cursor::Hide)?;
            // Park the hidden cursor on the anchor so reflowing terminals
            // translate the anchor for us across resizes.
            queue!(term, cursor::RestorePosition)?;
            self.live_cursor = LiveCursor::Parked;
        }

        queue!(term, EnableWrap)?;
        queue!(
            term,
            SetAttribute(ratatui::crossterm::style::Attribute::Reset)
        )?;

        if should_clear {
            queue!(term, cterm::EndSynchronizedUpdate)?;
        }

        term.flush()?;
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
