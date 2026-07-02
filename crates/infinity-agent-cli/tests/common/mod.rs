//! Shared snapshot-test harness for the terminal UI.
//!
//! Drives [`infinity_agent_cli::terminal::run`] against a virtual terminal
//! and a scripted event queue, so tests can feed display events /
//! keystrokes / resizes and snapshot the resulting screen state with insta.
//!
//! Two emulator backends are available, covering both behavior classes of
//! real-world terminals:
//!
//! * [`Backend::Vt100`] — truncates/pads on resize without reflowing
//!   (like xterm or the Windows console);
//! * [`Backend::Alacritty`] — rewraps wrapped lines through scrollback and
//!   translates the cursor + DECSC saved cursor on resize (like alacritty,
//!   kitty, or VTE-based terminals). Use this to exercise reflow bugs where
//!   scrollback content intersects the inline viewport area.
//!
//! Tests must run on a current-thread runtime with the clock paused
//! (`#[tokio::test(start_paused = true)]`):
//!
//! * the spinner animation uses `tokio::time::Instant`, so frozen time makes
//!   rendering deterministic (use [`tokio::time::advance`] to animate);
//! * [`TuiHarness::settle`] awaits a tiny sleep, which with auto-advance only
//!   completes once the UI task has drained everything it was sent.
#![allow(dead_code, reason = "not all test binaries use every helper")]
#![allow(
    clippy::allow_attributes,
    reason = "need allow(dead_code) for shared test helpers"
)]

use alacritty_terminal::event::{Event as AlacEvent, EventListener};
use alacritty_terminal::grid::Dimensions as _;
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::test::TermSize;
use alacritty_terminal::term::{Config, Term, TermMode};
use alacritty_terminal::vte::ansi::Processor;
use infinity_agent_cli::term_io::{EventSource, TermOut};
use infinity_agent_cli::terminal::{self, DetachResult, SessionChanged};
use infinity_agent_core::batch_processor::DisplayEvent;
use infinity_protocol::{ModelInfo, SessionInfo};
use ratatui::crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use rig_mock::MockStreamingResponse;
use std::collections::HashMap;
use std::io::{self, Write};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// The display-event payload type the harness feeds to the UI.
pub type DisplayItem = (Option<String>, DisplayEvent<MockStreamingResponse>);

// ── Emulator abstraction ────────────────────────────────────────────────────

/// A virtual terminal emulator the UI's output is replayed into.
pub trait Emulator: Send {
    /// Feed raw output bytes (ANSI escape sequences and text).
    fn process(&mut self, bytes: &[u8]);
    /// Current size as `(cols, rows)`.
    fn size(&self) -> (u16, u16);
    /// Cursor position as `(col, row)`, zero-based.
    fn cursor_position(&self) -> (u16, u16);
    fn cursor_hidden(&self) -> bool;
    /// Current window title (set via OSC sequences).
    fn title(&self) -> String;
    /// Resize the terminal, applying backend-specific reflow semantics.
    fn resize(&mut self, cols: u16, rows: u16);
    /// Text of a visible screen row, exactly `cols` display columns wide.
    fn row_text(&self, row: u16) -> String;
    /// Number of rows currently in scrollback history.
    fn history_len(&self) -> usize;
    /// Text of a scrollback row; index 0 is the oldest row.
    fn history_row_text(&self, idx: usize) -> String;
}

pub type SharedEmulator = Arc<Mutex<Box<dyn Emulator>>>;

fn lock_emu(emu: &SharedEmulator) -> std::sync::MutexGuard<'_, Box<dyn Emulator>> {
    emu.lock().expect("bug: emulator lock poisoned")
}

// ── vt100 backend (no reflow) ───────────────────────────────────────────────

pub struct Vt100Emulator {
    parser: vt100::Parser,
}

impl Vt100Emulator {
    pub fn new(cols: u16, rows: u16) -> Self {
        Self {
            parser: vt100::Parser::new(rows, cols, 0),
        }
    }
}

impl Emulator for Vt100Emulator {
    fn process(&mut self, bytes: &[u8]) {
        self.parser.process(bytes);
    }

    fn size(&self) -> (u16, u16) {
        let (rows, cols) = self.parser.screen().size();
        (cols, rows)
    }

    fn cursor_position(&self) -> (u16, u16) {
        let (row, col) = self.parser.screen().cursor_position();
        (col, row)
    }

    fn cursor_hidden(&self) -> bool {
        self.parser.screen().hide_cursor()
    }

    fn title(&self) -> String {
        self.parser.screen().title().to_owned()
    }

    fn resize(&mut self, cols: u16, rows: u16) {
        self.parser.set_size(rows, cols);
    }

    fn row_text(&self, row: u16) -> String {
        let screen = self.parser.screen();
        let (_, cols) = screen.size();
        let mut out = String::new();
        let mut col = 0;
        while col < cols {
            match screen.cell(row, col) {
                Some(cell) if !cell.contents().is_empty() => {
                    out.push_str(&cell.contents());
                    col += if cell.is_wide() { 2 } else { 1 };
                }
                _ => {
                    out.push(' ');
                    col += 1;
                }
            }
        }
        out
    }

    fn history_len(&self) -> usize {
        0
    }

    fn history_row_text(&self, _idx: usize) -> String {
        String::new()
    }
}

// ── alacritty backend (reflow on resize) ────────────────────────────────────

/// Listener capturing window-title events from the alacritty term.
#[derive(Clone)]
struct TitleProxy(Arc<Mutex<String>>);

impl EventListener for TitleProxy {
    fn send_event(&self, event: AlacEvent) {
        let mut title = self.0.lock().expect("bug: title lock poisoned");
        match event {
            AlacEvent::Title(t) => *title = t,
            AlacEvent::ResetTitle => title.clear(),
            _ => {}
        }
    }
}

pub struct AlacrittyEmulator {
    term: Term<TitleProxy>,
    parser: Processor,
    title: Arc<Mutex<String>>,
}

impl AlacrittyEmulator {
    pub fn new(cols: u16, rows: u16) -> Self {
        let title = Arc::new(Mutex::new(String::new()));
        let term = Term::new(
            Config::default(),
            &TermSize::new(cols as usize, rows as usize),
            TitleProxy(Arc::clone(&title)),
        );
        Self {
            term,
            parser: Processor::new(),
            title,
        }
    }

    fn line_text(&self, line: Line) -> String {
        let grid = self.term.grid();
        let cols = self.term.grid().columns();
        let row = &grid[line];
        let mut out = String::new();
        for col in 0..cols {
            let cell = &row[Column(col)];
            // A leading spacer is the blank cell left at the end of a row
            // when a wide char wraps to the next line: render as a space.
            // A (trailing) wide-char spacer is covered by the preceding
            // 2-column char: skip it.
            if cell.flags.contains(Flags::LEADING_WIDE_CHAR_SPACER) {
                out.push(' ');
            } else if !cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
                out.push(cell.c);
            }
        }
        out
    }
}

impl Emulator for AlacrittyEmulator {
    fn process(&mut self, bytes: &[u8]) {
        self.parser.advance(&mut self.term, bytes);
    }

    fn size(&self) -> (u16, u16) {
        let grid = self.term.grid();
        (grid.columns() as u16, grid.screen_lines() as u16)
    }

    fn cursor_position(&self) -> (u16, u16) {
        let point = self.term.grid().cursor.point;
        (point.column.0 as u16, point.line.0.max(0) as u16)
    }

    fn cursor_hidden(&self) -> bool {
        !self.term.mode().contains(TermMode::SHOW_CURSOR)
    }

    fn title(&self) -> String {
        self.title.lock().expect("bug: title lock poisoned").clone()
    }

    fn resize(&mut self, cols: u16, rows: u16) {
        self.term
            .resize(TermSize::new(cols as usize, rows as usize));
    }

    fn row_text(&self, row: u16) -> String {
        self.line_text(Line(row as i32))
    }

    fn history_len(&self) -> usize {
        self.term.grid().history_size()
    }

    fn history_row_text(&self, idx: usize) -> String {
        let history = self.term.grid().history_size() as i32;
        self.line_text(Line(idx as i32 - history))
    }
}

// ── TermOut over a shared emulator ──────────────────────────────────────────

/// A [`TermOut`] that feeds everything written into a shared [`Emulator`].
pub struct VirtualTerm {
    emu: SharedEmulator,
}

impl VirtualTerm {
    pub fn new(emu: SharedEmulator) -> Self {
        Self { emu }
    }
}

impl Write for VirtualTerm {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        lock_emu(&self.emu).process(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl TermOut for VirtualTerm {
    fn size(&mut self) -> io::Result<(u16, u16)> {
        Ok(lock_emu(&self.emu).size())
    }

    fn cursor_position(&mut self) -> io::Result<(u16, u16)> {
        Ok(lock_emu(&self.emu).cursor_position())
    }

    fn enable_raw_mode(&mut self) -> io::Result<()> {
        Ok(())
    }

    fn disable_raw_mode(&mut self) -> io::Result<()> {
        Ok(())
    }
}

// ── Scripted EventSource ────────────────────────────────────────────────────

/// An [`EventSource`] fed from a channel by the test.
pub struct ScriptedEvents {
    rx: mpsc::UnboundedReceiver<Event>,
    /// Event received by `wait_for_event` but not yet consumed.
    pending: Option<Event>,
}

impl ScriptedEvents {
    pub fn new(rx: mpsc::UnboundedReceiver<Event>) -> Self {
        Self { rx, pending: None }
    }
}

impl EventSource for ScriptedEvents {
    async fn wait_for_event(&mut self) {
        if self.pending.is_none() {
            match self.rx.recv().await {
                Some(event) => self.pending = Some(event),
                // No more events will ever arrive; park forever so the UI
                // just keeps serving its other channels.
                None => std::future::pending::<()>().await,
            }
        }
    }

    fn try_read_event(&mut self) -> io::Result<Option<Event>> {
        if let Some(event) = self.pending.take() {
            return Ok(Some(event));
        }
        Ok(self.rx.try_recv().ok())
    }
}

// ── Harness ─────────────────────────────────────────────────────────────────

/// Which virtual terminal to run the UI against.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub enum Backend {
    /// `vt100`: truncates/pads on resize without reflow.
    #[default]
    Vt100,
    /// `alacritty_terminal`: reflows wrapped scrollback on resize.
    Alacritty,
}

/// Optional knobs for [`TuiHarness::spawn_with`].
pub struct HarnessOptions {
    pub backend: Backend,
    pub cols: u16,
    pub rows: u16,
    pub model_name: String,
    pub provider_id: String,
    pub context_window: usize,
    pub initial_sessions: HashMap<String, SessionInfo>,
    pub available_models: Vec<ModelInfo>,
    pub initial_message: Option<String>,
}

impl Default for HarnessOptions {
    fn default() -> Self {
        Self {
            backend: Backend::default(),
            cols: 80,
            rows: 20,
            model_name: "mock-model".to_owned(),
            provider_id: "mock".to_owned(),
            context_window: 100_000,
            initial_sessions: HashMap::new(),
            available_models: vec![
                ModelInfo {
                    display_name: "Mock Model".to_owned(),
                    provider_id: "mock".to_owned(),
                    model_id: "mock-model".to_owned(),
                    context_window: 100_000,
                },
                ModelInfo {
                    display_name: "Mock Mini".to_owned(),
                    provider_id: "mock".to_owned(),
                    model_id: "mock-mini".to_owned(),
                    context_window: 32_000,
                },
            ],
            initial_message: None,
        }
    }
}

/// A running TUI under test, with handles to drive and observe it.
pub struct TuiHarness {
    emu: SharedEmulator,
    pub event_tx: mpsc::UnboundedSender<Event>,
    pub display_tx: mpsc::UnboundedSender<DisplayItem>,
    pub session_tx: mpsc::UnboundedSender<SessionChanged>,
    pub sessions_updated_tx: mpsc::UnboundedSender<HashMap<String, SessionInfo>>,
    pub detach_result_tx: mpsc::UnboundedSender<DetachResult>,
    pub input_rx: mpsc::UnboundedReceiver<String>,
    pub load_session_rx: mpsc::UnboundedReceiver<(Option<String>, bool)>,
    pub model_switch_rx: mpsc::UnboundedReceiver<usize>,
    pub soft_detach_rx: mpsc::UnboundedReceiver<()>,
    pub choice_answered_rx: mpsc::UnboundedReceiver<(String, usize)>,
    pub handle: tokio::task::JoinHandle<Result<bool, BoxError>>,
}

impl TuiHarness {
    /// Spawn the TUI on a `cols`×`rows` vt100 (non-reflowing) terminal.
    pub async fn spawn(cols: u16, rows: u16) -> Self {
        Self::spawn_with(HarnessOptions {
            cols,
            rows,
            ..HarnessOptions::default()
        })
        .await
    }

    /// Spawn the TUI on a `cols`×`rows` alacritty (reflowing) terminal.
    pub async fn spawn_reflowing(cols: u16, rows: u16) -> Self {
        Self::spawn_with(HarnessOptions {
            backend: Backend::Alacritty,
            cols,
            rows,
            ..HarnessOptions::default()
        })
        .await
    }

    pub async fn spawn_with(opts: HarnessOptions) -> Self {
        let emu: SharedEmulator = Arc::new(Mutex::new(match opts.backend {
            Backend::Vt100 => Box::new(Vt100Emulator::new(opts.cols, opts.rows)),
            Backend::Alacritty => {
                Box::new(AlacrittyEmulator::new(opts.cols, opts.rows)) as Box<dyn Emulator>
            }
        }));
        let term = VirtualTerm {
            emu: Arc::clone(&emu),
        };

        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let events = ScriptedEvents {
            rx: event_rx,
            pending: None,
        };

        let (input_tx, input_rx) = mpsc::unbounded_channel();
        let (display_tx, display_rx) = mpsc::unbounded_channel::<DisplayItem>();
        let (load_session_tx, load_session_rx) = mpsc::unbounded_channel();
        let (model_switch_tx, model_switch_rx) = mpsc::unbounded_channel();
        let (session_tx, session_rx) = mpsc::unbounded_channel();
        let (sessions_updated_tx, sessions_updated_rx) = mpsc::unbounded_channel();
        let (soft_detach_tx, soft_detach_rx) = mpsc::unbounded_channel();
        let (detach_result_tx, detach_result_rx) = mpsc::unbounded_channel();
        let (choice_answered_tx, choice_answered_rx) = mpsc::unbounded_channel();

        let handle = tokio::spawn(terminal::run(
            term,
            events,
            input_tx,
            display_rx,
            opts.model_name,
            opts.provider_id,
            opts.context_window,
            opts.initial_sessions,
            load_session_tx,
            model_switch_tx,
            opts.available_models,
            opts.initial_message,
            session_rx,
            sessions_updated_rx,
            soft_detach_tx,
            detach_result_rx,
            choice_answered_tx,
        ));

        let harness = Self {
            emu,
            event_tx,
            display_tx,
            session_tx,
            sessions_updated_tx,
            detach_result_tx,
            input_rx,
            load_session_rx,
            model_switch_rx,
            soft_detach_rx,
            choice_answered_rx,
            handle,
        };
        harness.settle().await;
        harness
    }

    /// Let the UI task process everything queued so far.
    ///
    /// Relies on the paused clock: this sleep only completes via
    /// auto-advance, i.e. once no other task (the UI loop) can make progress.
    pub async fn settle(&self) {
        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
    }

    /// Send a display event attributed to the root thread.
    pub fn display(&self, event: DisplayEvent<MockStreamingResponse>) {
        self.display_tx
            .send((None, event))
            .expect("bug: UI task dropped display channel");
    }

    /// Send a display event attributed to a child thread.
    pub fn display_for_thread(&self, thread_id: &str, event: DisplayEvent<MockStreamingResponse>) {
        self.display_tx
            .send((Some(thread_id.to_owned()), event))
            .expect("bug: UI task dropped display channel");
    }

    /// Send a key press (no modifiers).
    pub fn key(&self, code: KeyCode) {
        self.key_with(code, KeyModifiers::NONE);
    }

    /// Send a key press with modifiers.
    pub fn key_with(&self, code: KeyCode, modifiers: KeyModifiers) {
        self.event_tx
            .send(Event::Key(KeyEvent::new(code, modifiers)))
            .expect("bug: UI task dropped event channel");
    }

    /// Type a string one character at a time.
    pub fn type_str(&self, text: &str) {
        for ch in text.chars() {
            self.key(KeyCode::Char(ch));
        }
    }

    /// Send a no-op event that forces the UI loop through a redraw pass
    /// (mirrors the periodic wakeup of the real crossterm event source).
    pub fn tick(&self) {
        self.event_tx
            .send(Event::FocusGained)
            .expect("bug: UI task dropped event channel");
    }

    /// Resize the virtual terminal and notify the UI.
    ///
    /// With [`Backend::Vt100`] content is truncated/padded in place; with
    /// [`Backend::Alacritty`] wrapped lines are reflowed through scrollback
    /// and the cursor/saved-cursor are translated, like a real reflowing
    /// terminal would before delivering SIGWINCH.
    pub fn resize(&self, cols: u16, rows: u16) {
        lock_emu(&self.emu).resize(cols, rows);
        self.event_tx
            .send(Event::Resize(cols, rows))
            .expect("bug: UI task dropped event channel");
    }

    /// Render the current screen state for snapshotting: the character grid
    /// in a frame, plus cursor state and the terminal title.
    pub fn screen(&self) -> String {
        let emu = lock_emu(&self.emu);
        render_screen(&**emu, false)
    }

    /// Like [`TuiHarness::screen`], but with scrollback history rendered
    /// above the screen frame (`~ ` prefixed, right-trimmed). Useful for
    /// reflow tests to see what was pushed into or pulled out of history.
    pub fn screen_with_scrollback(&self) -> String {
        let emu = lock_emu(&self.emu);
        render_screen(&**emu, true)
    }
}

pub fn render_screen(emu: &dyn Emulator, with_scrollback: bool) -> String {
    let (cols, rows) = emu.size();
    let mut out = String::new();

    if with_scrollback {
        let history = emu.history_len();
        for idx in 0..history {
            out.push_str("~ ");
            out.push_str(emu.history_row_text(idx).trim_end());
            out.push('\n');
        }
    }

    out.push('┌');
    out.push_str(&"─".repeat(cols as usize));
    out.push_str("┐\n");
    for row in 0..rows {
        out.push('│');
        out.push_str(&emu.row_text(row));
        out.push_str("│\n");
    }
    out.push('└');
    out.push_str(&"─".repeat(cols as usize));
    out.push_str("┘\n");

    let (cursor_col, cursor_row) = emu.cursor_position();
    out.push_str(&format!(
        "cursor: row={cursor_row} col={cursor_col}{}\n",
        if emu.cursor_hidden() { " (hidden)" } else { "" }
    ));
    let title = emu.title();
    if !title.is_empty() {
        out.push_str(&format!("title: {title}\n"));
    }
    out
}

/// Advance the paused clock (animations) and force a redraw pass.
pub async fn advance_and_redraw(harness: &TuiHarness, duration: std::time::Duration) {
    tokio::time::advance(duration).await;
    harness.tick();
    harness.settle().await;
}
