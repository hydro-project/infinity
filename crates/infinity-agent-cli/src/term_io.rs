//! Terminal I/O abstraction for the TUI.
//!
//! The TUI talks to the terminal exclusively through two traits: [`TermOut`]
//! (an ANSI byte sink plus the few queries/mode switches we need) and
//! [`EventSource`] (an async stream of crossterm input events). Production
//! code uses the crossterm/stdout-backed implementations in this module;
//! tests substitute a `vt100`-backed virtual terminal and scripted events so
//! the rendered screen state can be snapshot-tested.

use ratatui::crossterm::{
    cursor,
    event::{self, Event},
    terminal as cterm,
};
use std::io::{self, Write};

/// Output half of a terminal: an ANSI escape-sequence sink plus the small
/// set of queries and mode switches the TUI needs.
///
/// Writes may be buffered; implementations must apply all previously written
/// bytes when [`Write::flush`] is called.
pub trait TermOut: Write {
    /// Current terminal size as `(cols, rows)`.
    fn size(&mut self) -> io::Result<(u16, u16)>;

    /// Current cursor position as `(col, row)`, zero-based.
    ///
    /// Implementations must flush pending output before answering so the
    /// reported position reflects every previous write.
    fn cursor_position(&mut self) -> io::Result<(u16, u16)>;

    fn enable_raw_mode(&mut self) -> io::Result<()>;

    fn disable_raw_mode(&mut self) -> io::Result<()>;
}

/// Input half of a terminal: an async source of crossterm [`Event`]s.
pub trait EventSource {
    /// Wait until an event might be available from
    /// [`try_read_event`](EventSource::try_read_event), or until an
    /// implementation-defined animation tick elapses.
    ///
    /// Must be cancel-safe: dropping the returned future must not lose
    /// events.
    fn wait_for_event(&mut self) -> impl Future<Output = ()>;

    /// Non-blocking read of the next pending event, if any.
    fn try_read_event(&mut self) -> io::Result<Option<Event>>;
}

/// The real terminal: writes to stdout and queries via crossterm.
pub struct CrosstermTerm {
    stdout: io::Stdout,
}

impl CrosstermTerm {
    pub fn new() -> Self {
        Self {
            stdout: io::stdout(),
        }
    }
}

impl Default for CrosstermTerm {
    fn default() -> Self {
        Self::new()
    }
}

impl Write for CrosstermTerm {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.stdout.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.stdout.flush()
    }
}

impl TermOut for CrosstermTerm {
    fn size(&mut self) -> io::Result<(u16, u16)> {
        cterm::size()
    }

    fn cursor_position(&mut self) -> io::Result<(u16, u16)> {
        self.stdout.flush()?;
        cursor::position()
    }

    fn enable_raw_mode(&mut self) -> io::Result<()> {
        cterm::enable_raw_mode()
    }

    fn disable_raw_mode(&mut self) -> io::Result<()> {
        cterm::disable_raw_mode()
    }
}

/// The real event source: polls crossterm events on the blocking thread
/// pool, waking up every 16ms even without input so animations (e.g. the
/// thinking spinner) keep redrawing.
pub struct CrosstermEvents;

impl EventSource for CrosstermEvents {
    async fn wait_for_event(&mut self) {
        tokio::task::spawn_blocking(|| {
            let _ = event::poll(std::time::Duration::from_millis(16));
        })
        .await
        .ok();
    }

    fn try_read_event(&mut self) -> io::Result<Option<Event>> {
        if event::poll(std::time::Duration::ZERO)? {
            Ok(Some(event::read()?))
        } else {
            Ok(None)
        }
    }
}
