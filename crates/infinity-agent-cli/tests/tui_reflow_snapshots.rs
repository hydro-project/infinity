//! Snapshot tests for resize/reflow behavior, rendered through
//! `alacritty_terminal` — a virtual terminal that, like alacritty/kitty/VTE
//! terminals, rewraps wrapped scrollback lines and translates the live
//! cursor on resize (but merely clamps the DECSC saved cursor!). This is
//! the behavior class where the inline viewport's resize handling has
//! historically been buggy.
//!
//! See `common/mod.rs` for the harness; `tui_snapshots.rs` covers the
//! non-reflowing (vt100) backend.

mod common;

use common::{HarnessOptions, TuiHarness};
use infinity_agent_core::batch_processor::DisplayEvent;
use infinity_protocol::{SessionInfo, SessionStatus};
use ratatui::crossterm::event::KeyCode;
use rig_mock::MockStreamingResponse;
use std::collections::HashMap;

type Evt = DisplayEvent<MockStreamingResponse>;

/// Sanity check: the simple startup screen matches between backends
/// (compare with `tui_snapshots__startup_screen.snap`).
#[tokio::test(start_paused = true)]
async fn startup_screen_reflowing_backend() {
    let h = TuiHarness::spawn_reflowing(80, 16).await;
    insta::assert_snapshot!(h.screen_with_scrollback());
}

/// A long streamed line wraps in scrollback; narrowing the terminal makes
/// it rewrap to more rows (pushing content up through the viewport area),
/// widening makes it rewrap to fewer.
#[tokio::test(start_paused = true)]
async fn wrapped_scrollback_reflows_on_resize() {
    let h = TuiHarness::spawn_reflowing(80, 16).await;

    h.display(Evt::UserInput("explain the project".to_owned()));
    h.display(Evt::StartOutput);
    let long: String = (1..=30)
        .map(|i| format!("word{i:02}"))
        .collect::<Vec<_>>()
        .join(" ");
    h.display(Evt::TextChunk { chunk: long });
    h.display(Evt::ResponseDone(None));
    h.settle().await;
    insta::assert_snapshot!("reflow_wrapped_80", h.screen_with_scrollback());

    h.resize(40, 16);
    h.settle().await;
    insta::assert_snapshot!("reflow_wrapped_narrow_40", h.screen_with_scrollback());

    h.resize(100, 16);
    h.settle().await;
    insta::assert_snapshot!("reflow_wrapped_wide_100", h.screen_with_scrollback());
}

/// Resize when the scrollback already contains many wrapped lines and the
/// screen is full — the viewport must track how much content reflows above
/// it without leaving artifacts or overlapping output.
#[tokio::test(start_paused = true)]
async fn deep_wrapped_scrollback_narrow_resize() {
    let h = TuiHarness::spawn_reflowing(80, 12).await;

    for i in 1..=10 {
        h.display(Evt::Info(format!(
            "line {i:02} {}",
            "lorem ipsum dolor sit amet ".repeat(3).trim_end()
        )));
    }
    h.settle().await;
    insta::assert_snapshot!("deep_scrollback_80", h.screen_with_scrollback());

    h.resize(50, 12);
    h.settle().await;
    insta::assert_snapshot!("deep_scrollback_narrow_50", h.screen_with_scrollback());

    h.resize(80, 12);
    h.settle().await;
    insta::assert_snapshot!("deep_scrollback_back_80", h.screen_with_scrollback());
}

/// Shrinking the terminal *vertically* moves the viewport area relative to
/// the saved cursor; growing it back should not duplicate or eat lines.
#[tokio::test(start_paused = true)]
async fn vertical_resize_moves_viewport() {
    let h = TuiHarness::spawn_reflowing(80, 16).await;

    h.display(Evt::UserInput("hi".to_owned()));
    h.display(Evt::StartOutput);
    h.display(Evt::TextChunk {
        chunk: "short answer".to_owned(),
    });
    h.display(Evt::ResponseDone(None));
    h.settle().await;
    insta::assert_snapshot!("vertical_before", h.screen_with_scrollback());

    h.resize(80, 8);
    h.settle().await;
    insta::assert_snapshot!("vertical_shrunk_8", h.screen_with_scrollback());

    h.resize(80, 16);
    h.settle().await;
    insta::assert_snapshot!("vertical_grown_16", h.screen_with_scrollback());
}

/// Resize arriving while a tool call + thinking spinner are on screen —
/// combines animated viewport content with reflowing scrollback.
#[tokio::test(start_paused = true)]
async fn resize_during_tool_call_spinner() {
    let h = TuiHarness::spawn_reflowing(80, 16).await;

    h.display(Evt::UserInput("run a long command".to_owned()));
    h.display(Evt::StartOutput);
    h.display(Evt::ToolCall {
        name: "execute_command".to_owned(),
        args: serde_json::json!({"command": "cargo build"}),
        display_as: Some(format!(
            "execute_command(cargo build --workspace --all-targets {})",
            "--verbose ".repeat(5).trim_end()
        )),
    });
    h.settle().await;
    insta::assert_snapshot!("tool_spinner_80", h.screen_with_scrollback());

    h.resize(40, 16);
    h.settle().await;
    insta::assert_snapshot!("tool_spinner_narrow_40", h.screen_with_scrollback());
}

/// Vertical shrink while the spinner row is up, then the spinner goes away
/// (viewport height change right after the resize) and a line is printed —
/// scrollback lines must survive both transitions.
#[tokio::test(start_paused = true)]
async fn vertical_shrink_next_to_spinner_change() {
    let h = TuiHarness::spawn_reflowing(80, 14).await;

    for i in 1..=9 {
        h.display(Evt::Info(format!("scroll line {i:02}")));
    }
    h.display(Evt::UserInput("go".to_owned()));
    h.display(Evt::StartOutput);
    h.settle().await;
    insta::assert_snapshot!("v_shrink_spinner_before", h.screen_with_scrollback());

    h.resize(80, 9);
    h.settle().await;
    insta::assert_snapshot!("v_shrink_spinner_after_resize", h.screen_with_scrollback());

    h.display(Evt::ResponseDone(None));
    h.settle().await;
    h.display(Evt::Info("printed after spinner gone".to_owned()));
    h.settle().await;
    insta::assert_snapshot!("v_shrink_spinner_then_print", h.screen_with_scrollback());
}

/// Variant of [`vertical_shrink_next_to_spinner_change`] where the echoed
/// user input is a long line that wraps across several rows: the misplaced
/// redraw after the shrink lands on top of real content instead of blank
/// rows, demonstrating actual overwriting.
#[tokio::test(start_paused = true)]
async fn vertical_shrink_overwrites_wrapped_content() {
    let h = TuiHarness::spawn_reflowing(80, 14).await;

    for i in 1..=9 {
        h.display(Evt::Info(format!("scroll line {i:02}")));
    }
    let long_input: String = (1..=25)
        .map(|i| format!("word{i:02}"))
        .collect::<Vec<_>>()
        .join(" ");
    h.display(Evt::UserInput(long_input));
    h.display(Evt::StartOutput);
    h.settle().await;
    insta::assert_snapshot!("v_shrink_overwrite_before", h.screen_with_scrollback());

    h.resize(80, 10);
    h.settle().await;
    insta::assert_snapshot!(
        "v_shrink_overwrite_after_resize",
        h.screen_with_scrollback()
    );

    h.display(Evt::ResponseDone(None));
    h.settle().await;
    h.display(Evt::Info("printed after spinner gone".to_owned()));
    h.settle().await;
    insta::assert_snapshot!("v_shrink_overwrite_then_print", h.screen_with_scrollback());
}

/// Vertical grow with deep scrollback: the terminal pulls rows back out of
/// history; the viewport must re-anchor at the new bottom without leaving a
/// stale copy mid-screen or eating the pulled-back lines.
#[tokio::test(start_paused = true)]
async fn vertical_grow_pulls_back_history() {
    let h = TuiHarness::spawn_reflowing(80, 8).await;

    for i in 1..=12 {
        h.display(Evt::Info(format!("scroll line {i:02}")));
    }
    h.settle().await;
    insta::assert_snapshot!("v_grow_before", h.screen_with_scrollback());

    h.resize(80, 16);
    h.settle().await;
    insta::assert_snapshot!("v_grow_after", h.screen_with_scrollback());

    h.display(Evt::Info("printed after growing".to_owned()));
    h.settle().await;
    insta::assert_snapshot!("v_grow_then_print", h.screen_with_scrollback());
}

/// Vertical shrink while the assistant is mid-stream (saved cursor sits on a
/// partial line with no trailing newline), then the stream continues.
#[tokio::test(start_paused = true)]
async fn vertical_shrink_mid_stream_partial_line() {
    let h = TuiHarness::spawn_reflowing(80, 14).await;

    for i in 1..=6 {
        h.display(Evt::Info(format!("scroll line {i:02}")));
    }
    h.display(Evt::UserInput("stream please".to_owned()));
    h.display(Evt::StartOutput);
    h.display(Evt::TextChunk {
        chunk: "this sentence is interrupted by a resize".to_owned(),
    });
    h.settle().await;
    insta::assert_snapshot!("mid_stream_before", h.screen_with_scrollback());

    h.resize(80, 9);
    h.settle().await;
    h.display(Evt::TextChunk {
        chunk: " and then keeps going afterwards".to_owned(),
    });
    h.display(Evt::ResponseDone(None));
    h.settle().await;
    insta::assert_snapshot!("mid_stream_after_shrink", h.screen_with_scrollback());
}

/// Repeated resizes must not corrupt scrollback — including two width
/// shrinks in a row that rewrap a long scrollback line (each shrink
/// re-derives the anchor while the screen still holds viewport rows drawn
/// for the previous geometry).
///
/// Width shrinks may push a few *blank* rows into scrollback: the terminal
/// rewraps the old viewport rows into extra rows (pushing rows out the top)
/// before the Resize event is even delivered. Stale viewport rows must never
/// leak into scrollback, all content must survive verbatim, and the blank
/// gap left on screen must be consumed by subsequent prints (the anchor
/// advances into it) rather than persisting while new content scrolls.
#[tokio::test(start_paused = true)]
async fn repeated_resizes_do_not_accumulate() {
    let h = TuiHarness::spawn_reflowing(80, 12).await;
    for i in 1..=3 {
        h.display(Evt::Info(format!("scroll line {i:02}")));
    }
    // 125 columns: wraps to 2 rows at 80, 3 at 60, 4 at 40.
    let long: String = (1..=18)
        .map(|i| format!("word{i:02}"))
        .collect::<Vec<_>>()
        .join(" ");
    h.display(Evt::Info(long));
    h.settle().await;
    insta::assert_snapshot!("repeated_resizes_before", h.screen_with_scrollback());

    // Two width shrinks in a row, each redrawn separately, then back.
    h.resize(60, 12);
    h.settle().await;
    h.resize(40, 12);
    h.settle().await;
    insta::assert_snapshot!("repeated_resizes_double_shrink", h.screen_with_scrollback());
    h.resize(80, 12);
    h.settle().await;
    insta::assert_snapshot!("repeated_resizes_back_wide", h.screen_with_scrollback());

    // New prints consume the on-screen gap left by the resizes instead of
    // scrolling past it: history must not grow.
    let history_rows = h.screen_with_scrollback().matches("\n~ ").count();
    for i in 1..=3 {
        h.display(Evt::Info(format!("printed after width dance {i}")));
    }
    h.settle().await;
    let after = h.screen_with_scrollback();
    assert_eq!(
        history_rows,
        after.matches("\n~ ").count(),
        "prints after a resize must fill the gap, not push more scrollback"
    );
    insta::assert_snapshot!("repeated_resizes_prints_fill_gap", after);

    // Vertical shrink/grow cycles must not drift: blanks pushed by a shrink
    // are pulled back out of history by the grow.
    for _ in 0..3 {
        h.resize(80, 8);
        h.settle().await;
        h.resize(80, 12);
        h.settle().await;
    }
    insta::assert_snapshot!("repeated_resizes_after_cycles", h.screen_with_scrollback());
}

/// Several resize events coalescing in one poll batch with a keystroke in
/// between (exercises the `got_resize` deduplication path in the event loop).
#[tokio::test(start_paused = true)]
async fn coalesced_resizes_with_key_between() {
    let h = TuiHarness::spawn_reflowing(80, 14).await;

    for i in 1..=6 {
        h.display(Evt::Info(format!("scroll line {i:02}")));
    }
    h.settle().await;

    // All in one batch, no settling in between: shrink, type, shrink again.
    h.resize(60, 14);
    h.type_str("abc");
    h.resize(40, 10);
    h.settle().await;
    insta::assert_snapshot!("coalesced_resizes", h.screen_with_scrollback());
}

// ── Resizes overlapping picker overlays, streams, and prints ───────────────

/// Horizontal resize while the model picker overlay is open.
#[tokio::test(start_paused = true)]
async fn resize_while_model_picker_open() {
    let h = TuiHarness::spawn_reflowing(80, 14).await;
    for i in 1..=5 {
        h.display(Evt::Info(format!("scroll line {i:02}")));
    }
    h.settle().await;

    h.type_str("/model");
    h.key(KeyCode::Enter);
    h.settle().await;
    insta::assert_snapshot!("picker_before_resize", h.screen_with_scrollback());

    h.resize(50, 14);
    h.settle().await;
    insta::assert_snapshot!("picker_resized_narrow", h.screen_with_scrollback());

    // Close it after the resize and print — does the viewport recover?
    h.key(KeyCode::Esc);
    h.settle().await;
    h.display(Evt::Info("printed after picker resize".to_owned()));
    h.settle().await;
    insta::assert_snapshot!("picker_closed_after_resize", h.screen_with_scrollback());
}

/// Vertical shrink while the session picker overlay is open, smaller than
/// the picker wants to be.
#[tokio::test(start_paused = true)]
async fn vertical_shrink_under_session_picker() {
    let mut sessions = HashMap::new();
    for i in 1..=8 {
        sessions.insert(
            format!("session-{i:04}aaaa-bbbb-cccc"),
            SessionInfo {
                title: Some(format!("Session number {i}")),
                last_updated: format!("2025-01-{:02}T00:00:00Z", i),
                total_tokens_used: i * 1000,
                status: SessionStatus::Idle,
                threads: vec![],
                remote: None,
            },
        );
    }
    let h = TuiHarness::spawn_with(HarnessOptions {
        cols: 80,
        rows: 14,
        backend: common::Backend::Alacritty,
        initial_sessions: sessions,
        ..HarnessOptions::default()
    })
    .await;

    h.type_str("/load");
    h.key(KeyCode::Enter);
    h.settle().await;
    insta::assert_snapshot!("session_picker_open", h.screen_with_scrollback());

    h.resize(80, 6);
    h.settle().await;
    insta::assert_snapshot!("session_picker_shrunk_6", h.screen_with_scrollback());
}

/// Widening while mid-stream on a wrapped partial line: the wrapped rows
/// merge back into one, and the next chunk must continue at the right spot.
#[tokio::test(start_paused = true)]
async fn widen_mid_stream_wrapped_line() {
    let h = TuiHarness::spawn_reflowing(40, 12).await;

    h.display(Evt::UserInput("stream".to_owned()));
    h.display(Evt::StartOutput);
    h.display(Evt::TextChunk {
        chunk: "this is a long sentence that wraps across multiple rows at forty columns"
            .to_owned(),
    });
    h.settle().await;
    insta::assert_snapshot!("widen_mid_stream_before", h.screen_with_scrollback());

    h.resize(80, 12);
    h.settle().await;
    h.display(Evt::TextChunk {
        chunk: " AND THE CONTINUATION AFTER WIDENING".to_owned(),
    });
    h.display(Evt::ResponseDone(None));
    h.settle().await;
    insta::assert_snapshot!("widen_mid_stream_after", h.screen_with_scrollback());
}

/// The terminal has already resized (and reflowed) but the UI prints a line
/// before it reads the Resize event — the scroll region is set from stale
/// coordinates. The display channel is polled before input events
/// (biased select), so sending both before settling reproduces the race.
#[tokio::test(start_paused = true)]
async fn print_races_resize_notification() {
    let h = TuiHarness::spawn_reflowing(80, 14).await;
    for i in 1..=8 {
        h.display(Evt::Info(format!("scroll line {i:02}")));
    }
    h.settle().await;

    // Resize the emulator (reflow happens now), then a print arrives and is
    // processed before the Resize event.
    h.resize(80, 8);
    h.display(Evt::Info("printed during resize race".to_owned()));
    h.settle().await;
    insta::assert_snapshot!("resize_race_vertical", h.screen_with_scrollback());

    h.display(Evt::Info("printed after race settled".to_owned()));
    h.settle().await;
    insta::assert_snapshot!("resize_race_vertical_after", h.screen_with_scrollback());
}
