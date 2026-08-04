//! Regression test for the resize race that eats scrollback output.
//!
//! When the terminal window is resized *continuously* (e.g. dragging a pane
//! divider in Zed, whose terminal reflows on every drag frame), the terminal
//! keeps reflowing while the viewport's re-anchor cursor query (`CSI 6n`) is
//! in flight. The reply then describes a geometry that is already gone: a
//! reflowing terminal pulls more scrollback rows onto the screen as it grows,
//! moving all content (and the true anchor) further down. Re-saving the
//! anchor at the stale coordinates and clearing from it downwards erases the
//! rows between the stale and the true anchor — the tail of the output.
//!
//! The test models the race by interposing on `cursor_position()`: right
//! after the emulator answers the query (i.e. the moment the reply left the
//! terminal), the emulator is resized again, so all subsequent bytes land on
//! the newer grid — exactly what happens to a real TUI mid-drag.

mod common;

use common::{
    AlacrittyEmulator, Emulator, HarnessOptions, SharedEmulator, TuiHarness, VirtualTerm,
};
use infinity_agent_cli::display::DisplayEvent;
use infinity_agent_cli::term_io::TermOut;
use ratatui::crossterm::event::Event;
use std::io::{self, Write};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

type Evt = DisplayEvent;

/// A [`TermOut`] that resizes the emulator (and queues the matching Resize
/// event) immediately after answering a cursor-position query, modeling a
/// window drag that continues while the query round-trip is in flight.
struct MidQueryResizeTerm {
    inner: VirtualTerm,
    emu: SharedEmulator,
    event_tx: mpsc::UnboundedSender<Event>,
    resize_after_query: Arc<Mutex<Option<(u16, u16)>>>,
}

impl Write for MidQueryResizeTerm {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.inner.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

impl TermOut for MidQueryResizeTerm {
    fn size(&mut self) -> io::Result<(u16, u16)> {
        self.inner.size()
    }

    fn cursor_position(&mut self) -> io::Result<(u16, u16)> {
        let pos = self.inner.cursor_position()?;
        let pending = self
            .resize_after_query
            .lock()
            .expect("bug: resize_after_query lock poisoned")
            .take();
        if let Some((cols, rows)) = pending {
            self.emu
                .lock()
                .expect("bug: emulator lock poisoned")
                .resize(cols, rows);
            self.event_tx
                .send(Event::Resize(cols, rows))
                .expect("bug: UI task dropped event channel");
        }
        Ok(pos)
    }

    fn enable_raw_mode(&mut self) -> io::Result<()> {
        Ok(())
    }

    fn disable_raw_mode(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Stable TUI with deep scrollback; a vertical grow whose cursor query races
/// a further grow must not eat any of the finished assistant output.
#[tokio::test(start_paused = true)]
async fn drag_grow_races_cursor_query() {
    let emu: SharedEmulator = Arc::new(Mutex::new(
        Box::new(AlacrittyEmulator::new(80, 20)) as Box<dyn Emulator>
    ));
    let (event_tx, event_rx) = mpsc::unbounded_channel();
    let resize_after_query = Arc::new(Mutex::new(None));
    let term = MidQueryResizeTerm {
        inner: VirtualTerm::new(Arc::clone(&emu)),
        emu: Arc::clone(&emu),
        event_tx: event_tx.clone(),
        resize_after_query: Arc::clone(&resize_after_query),
    };
    let h = TuiHarness::spawn_with_term(
        term,
        Arc::clone(&emu),
        event_tx,
        event_rx,
        HarnessOptions {
            backend: common::Backend::Alacritty,
            cols: 80,
            rows: 20,
            ..HarnessOptions::default()
        },
    )
    .await;

    // A finished multi-line response, deep enough to fill scrollback.
    h.display(Evt::UserInput("tell me things".to_owned()));
    h.display(Evt::StartOutput);
    let text: String = (1..=30)
        .map(|i| format!("assistant line {i:02}"))
        .collect::<Vec<_>>()
        .join("\n");
    h.display(Evt::TextChunk { chunk: text });
    h.display(Evt::ResponseDone(None));
    h.settle().await;

    // Grow to 24 rows; while the TUI's re-anchor cursor query is in flight,
    // the drag continues to 30 rows (pulling 6 more scrollback rows down).
    *resize_after_query
        .lock()
        .expect("bug: resize_after_query lock poisoned") = Some((80, 30));
    h.resize(80, 24);
    h.settle().await;

    let after = h.screen_with_scrollback();
    for i in 1..=30 {
        assert!(
            after.contains(&format!("assistant line {i:02}")),
            "assistant line {i:02} was eaten by the raced resize\n{after}"
        );
    }
    insta::assert_snapshot!("drag_grow_raced_query", after);
}
