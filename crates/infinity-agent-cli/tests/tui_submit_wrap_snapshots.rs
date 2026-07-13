//! Regression test for viewport corruption when a long (wrapping) input
//! was submitted while the agent was actively thinking: the input box was
//! drawn one row too low, its background leaked into the row below, and
//! the status bar landed on the terminal's bottom row on top of the box
//! background.
//!
//! Mechanism of the (fixed) bug:
//!
//! 1. Submitting a 3+-row input shrinks the viewport, opening a gap
//!    between the anchor and the bottom-pinned viewport.
//! 2. The `> `-prefixed echo wraps in scrollback. `OutputTracker` counted
//!    a wrap only when `col + w > width` with `col = line_len % width`,
//!    which is never true for single-width chars — soft wraps went
//!    uncounted (the terminal defers the wrap of an exactly-full row to
//!    the next printable char, which then starts at `col == 0`), so the
//!    *real* anchor ended up below the *tracked* one.
//! 3. The divergence stayed latent while `ideal_viewport_y > viewport_y`:
//!    `draw()` takes the pin branch, which positions with an absolute
//!    `MoveTo`. (This is also why spinner-row toggles never triggered the
//!    bug — the pin branch doesn't update `viewport_y`.)
//! 4. The first draw whose viewport *growth* made
//!    `ideal_viewport_y <= viewport_y` positioned relatively from the
//!    real (lower) anchor — painting the whole frame one row too low. Any
//!    growth qualified: input wrapping, autocomplete rows, or (as used
//!    here, requiring no typing at all) a child-thread row.
//!
//! Snapshots include a background-marker frame (see
//! `TuiHarness::screen_with_bg`) since the corruption was partly
//! invisible in the character grid alone.

mod common;

use common::TuiHarness;
use infinity_agent_core::batch_processor::DisplayEvent;
use ratatui::crossterm::event::KeyCode;
use rig_mock::MockStreamingResponse;

type Evt = DisplayEvent<MockStreamingResponse>;

/// Long enough to wrap to three rows in the input box (78 inner columns at
/// width 80) and to three rows in the `> `-prefixed scrollback echo.
const LONG_INPUT: &str = "please refactor the parser module to support incremental \
     reparsing and then add regression tests covering the new behavior end to end, \
     including the error recovery paths and the checkpoint serialization logic";

/// Agent mid-thinking on a full screen; type a wrapping input, submit it
/// (interrupting the thinking), receive the wrapped echo, then a
/// child-thread event adds a thread row — a one-row viewport growth with
/// no typing. Before the fix, the final frame was corrupted: box one row
/// too low, background under the status row, cursor desynced.
///
/// (The bug reproduced identically on the reflowing/alacritty backend;
/// the vt100 one suffices since no resizes are involved.)
#[tokio::test(start_paused = true)]
async fn submit_wrapping_input_during_thinking() {
    let mut h = TuiHarness::spawn(80, 20).await;
    let h = &mut h;
    // Fill the screen with prior conversation so the viewport sits at the
    // bottom with no slack, like a real session in progress.
    for i in 1..=20 {
        h.display(Evt::Info(format!("history line {i:02}")));
    }
    h.display(Evt::UserInput("warm up the session".to_owned()));
    h.display(Evt::StartOutput);
    h.display(Evt::ThinkingStart);
    h.display(Evt::ThinkingChunk {
        chunk: "pondering the request".to_owned(),
    });
    h.settle().await;

    // Type the long message (wraps to 3 rows in the box) and submit it,
    // interrupting the active thinking.
    h.type_str(LONG_INPUT);
    h.settle().await;
    h.key(KeyCode::Enter);
    h.settle().await;
    let sent = h
        .input_rx
        .try_recv()
        .expect("input should be submitted on Enter");
    assert_eq!(sent, LONG_INPUT);

    // The daemon echoes the submitted input (the `> ` line wraps in
    // scrollback) and a new turn starts.
    h.display(Evt::UserInput(LONG_INPUT.to_owned()));
    h.display(Evt::StartOutput);
    h.settle().await;

    // A child thread reports activity: one more viewport row — the growth
    // that used to turn the latent anchor mistracking into visible
    // corruption. The box must sit directly under the border with a clean
    // status row below it.
    h.display_for_thread("child-a1", Evt::StartOutput);
    h.display_for_thread(
        "child-a1",
        Evt::ThinkingChunk {
            chunk: "child working".to_owned(),
        },
    );
    h.settle().await;
    insta::assert_snapshot!("after_child_row", h.screen_with_bg());
}
