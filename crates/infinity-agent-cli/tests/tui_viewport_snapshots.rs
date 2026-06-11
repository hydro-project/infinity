//! Snapshot tests for inline-viewport placement when the viewport itself
//! changes height — spinner rows appearing/disappearing, multiline input,
//! autocomplete hints, pickers, and child-thread rows — while scrollback
//! content exists above it.
//!
//! The viewport grows by scrolling the screen and re-saving the cursor, but
//! the shrink path repositions with absolute coordinates without re-saving,
//! so these tests look for eaten/overwritten scrollback lines and misplaced
//! prints around height changes. Rendered through the reflowing
//! (`alacritty_terminal`) backend so scrollback is visible in snapshots.
//!
//! These tests document current (possibly buggy) behavior — inspect the
//! snapshots before trusting them as "correct".

mod common;

use common::TuiHarness;
use infinity_agent_core::batch_processor::DisplayEvent;
use ratatui::crossterm::event::{KeyCode, KeyModifiers};
use rig_mock::MockStreamingResponse;

type Evt = DisplayEvent<MockStreamingResponse>;

/// Print `n` numbered lines so eaten/duplicated scrollback is obvious.
async fn fill_scrollback(h: &TuiHarness, n: usize) {
    for i in 1..=n {
        h.display(Evt::Info(format!("scroll line {i:02}")));
    }
    h.settle().await;
}

#[tokio::test(start_paused = true)]
async fn spinner_appear_disappear_with_scrollback() {
    let h = TuiHarness::spawn_reflowing(80, 12).await;
    fill_scrollback(&h, 8).await;
    insta::assert_snapshot!("scrollback_baseline", h.screen_with_scrollback());

    // Spinner appears → viewport grows by one row.
    h.display(Evt::UserInput("go".to_owned()));
    h.display(Evt::StartOutput);
    h.settle().await;
    insta::assert_snapshot!("spinner_grew_viewport", h.screen_with_scrollback());

    // Spinner disappears → viewport shrinks again.
    h.display(Evt::ResponseDone(None));
    h.settle().await;
    insta::assert_snapshot!("spinner_gone_viewport_shrunk", h.screen_with_scrollback());

    // A print after the shrink shows where the saved cursor ended up.
    h.display(Evt::Info("printed after viewport shrank".to_owned()));
    h.settle().await;
    insta::assert_snapshot!("print_after_shrink", h.screen_with_scrollback());
}

#[tokio::test(start_paused = true)]
async fn multiline_input_grow_then_submit() {
    let h = TuiHarness::spawn_reflowing(80, 12).await;
    fill_scrollback(&h, 8).await;

    h.type_str("first line");
    h.key_with(KeyCode::Char('j'), KeyModifiers::CONTROL);
    h.type_str("second line");
    h.key_with(KeyCode::Char('j'), KeyModifiers::CONTROL);
    h.type_str("third line");
    h.settle().await;
    insta::assert_snapshot!("multiline_input_grown", h.screen_with_scrollback());

    // Submit: input clears (viewport shrinks) and the daemon echoes the
    // message back as a UserInput display event.
    h.key(KeyCode::Enter);
    h.settle().await;
    h.display(Evt::UserInput(
        "first line\nsecond line\nthird line".to_owned(),
    ));
    h.settle().await;
    insta::assert_snapshot!("multiline_input_submitted", h.screen_with_scrollback());
}

#[tokio::test(start_paused = true)]
async fn autocomplete_grow_shrink_with_scrollback() {
    let h = TuiHarness::spawn_reflowing(80, 12).await;
    fill_scrollback(&h, 8).await;

    // Typing "/" opens the autocomplete table → viewport grows.
    h.type_str("/");
    h.settle().await;
    insta::assert_snapshot!("autocomplete_grown", h.screen_with_scrollback());

    // Deleting it closes the table → viewport shrinks; then print.
    h.key(KeyCode::Backspace);
    h.settle().await;
    h.display(Evt::Info("printed after autocomplete closed".to_owned()));
    h.settle().await;
    insta::assert_snapshot!("autocomplete_closed_then_print", h.screen_with_scrollback());
}

#[tokio::test(start_paused = true)]
async fn model_picker_open_close_with_scrollback() {
    let h = TuiHarness::spawn_reflowing(80, 12).await;
    fill_scrollback(&h, 8).await;

    h.type_str("/model");
    h.key(KeyCode::Enter);
    h.settle().await;
    insta::assert_snapshot!("model_picker_open", h.screen_with_scrollback());

    h.key(KeyCode::Esc);
    h.settle().await;
    h.display(Evt::Info("printed after picker closed".to_owned()));
    h.settle().await;
    insta::assert_snapshot!("model_picker_closed_then_print", h.screen_with_scrollback());
}

#[tokio::test(start_paused = true)]
async fn choice_picker_during_stream() {
    let h = TuiHarness::spawn_reflowing(80, 14).await;
    fill_scrollback(&h, 6).await;

    h.display(Evt::UserInput("pick something".to_owned()));
    h.display(Evt::StartOutput);
    h.display(Evt::TextChunk {
        chunk: "Working on it...".to_owned(),
    });
    h.display(Evt::UserChoiceRequired {
        id: "choice-1".to_owned(),
        prompt: "Allow the tool to run?".to_owned(),
        choices: vec!["Allow".to_owned(), "Deny".to_owned()],
        default: 0,
        response_url: String::new(),
    });
    h.settle().await;
    insta::assert_snapshot!("choice_picker_open", h.screen_with_scrollback());

    // Answer it → picker collapses, viewport shrinks mid-stream.
    h.key(KeyCode::Enter);
    h.settle().await;
    h.display(Evt::TextChunk {
        chunk: " continuing after the choice".to_owned(),
    });
    h.display(Evt::ResponseDone(None));
    h.settle().await;
    insta::assert_snapshot!(
        "choice_answered_stream_continues",
        h.screen_with_scrollback()
    );
}

#[tokio::test(start_paused = true)]
async fn thread_rows_appear_and_clear() {
    let h = TuiHarness::spawn_reflowing(80, 12).await;
    fill_scrollback(&h, 8).await;

    h.display(Evt::UserInput("spawn children".to_owned()));
    h.display(Evt::StartOutput);
    h.display_for_thread("child-1", Evt::StartOutput);
    h.display_for_thread(
        "child-1",
        Evt::ThinkingChunk {
            chunk: "child one working".to_owned(),
        },
    );
    h.display_for_thread("child-2", Evt::StartOutput);
    h.display_for_thread(
        "child-2",
        Evt::ThinkingChunk {
            chunk: "child two working".to_owned(),
        },
    );
    h.settle().await;
    insta::assert_snapshot!("thread_rows_grown", h.screen_with_scrollback());

    // Children finish → their rows vanish, viewport shrinks by two.
    h.display_for_thread("child-1", Evt::ResponseDone(None));
    h.display_for_thread("child-2", Evt::ResponseDone(None));
    h.settle().await;
    h.display(Evt::Info("printed after threads finished".to_owned()));
    h.settle().await;
    insta::assert_snapshot!("thread_rows_cleared_then_print", h.screen_with_scrollback());
}

/// Multi-line text printed through `print_line_above` (e.g. multi-line Info
/// or OAuth messages) contains raw `\n` without carriage returns — in raw
/// mode LF alone does not return to column 0, so continuation lines may
/// start at the wrong column ("staircase" output).
#[tokio::test(start_paused = true)]
async fn multiline_info_raw_newlines() {
    let h = TuiHarness::spawn_reflowing(80, 12).await;

    h.display(Evt::Info(
        "first info line\nsecond info line\nthird info line".to_owned(),
    ));
    h.settle().await;
    // BUG: the newlines are stripped entirely — all three lines are crammed
    // onto one row ("first info linesecond info linethird info line");
    // ratatui's Line normalizes embedded \n away.
    insta::assert_snapshot!("multiline_info", h.screen_with_scrollback());
}

/// OAuth-required messages embed the URL after a `\n` inside a single
/// styled span — same raw-LF concern as multi-line Info.
#[tokio::test(start_paused = true)]
async fn oauth_required_message() {
    let h = TuiHarness::spawn_reflowing(80, 12).await;

    h.display(Evt::OAuthRequired {
        auth_url: "https://example.com/oauth/authorize?client_id=abc123".to_owned(),
    });
    h.settle().await;
    // BUG: staircase — the URL line starts at column 33 instead of column 0
    // because the embedded \n does LF without CR in raw mode.
    insta::assert_snapshot!("oauth_required", h.screen_with_scrollback());
}

/// Subscription events with multi-line payloads print a header plus
/// continuation lines while also restarting the spinner.
#[tokio::test(start_paused = true)]
async fn multiline_subscription_event() {
    let h = TuiHarness::spawn_reflowing(80, 14).await;
    fill_scrollback(&h, 4).await;

    h.display(Evt::SubscriptionEvent {
        name: "github_pr_comment".to_owned(),
        text: "new comment on PR #42\nauthor: octocat\nbody: please fix the tests".to_owned(),
    });
    h.settle().await;
    insta::assert_snapshot!("subscription_event", h.screen_with_scrollback());
}

/// A terminal shorter than the viewport wants to be: 80x5 cannot fit the
/// gap+border+input+status plus a spinner row.
#[tokio::test(start_paused = true)]
async fn tiny_terminal_height() {
    let h = TuiHarness::spawn_reflowing(80, 5).await;
    // BUG: the text input row is missing entirely and the cursor is parked
    // on the border row — the layout collapses when the terminal is shorter
    // than the viewport's desired height.
    insta::assert_snapshot!("tiny_5_rows_startup", h.screen_with_scrollback());

    h.display(Evt::UserInput("hello".to_owned()));
    h.display(Evt::StartOutput);
    h.settle().await;
    // BUG: still no input row; the spinner pushes the layout so the cursor
    // sits on the bottom border/status area.
    insta::assert_snapshot!("tiny_5_rows_spinner", h.screen_with_scrollback());

    h.display(Evt::TextChunk {
        chunk: "a response that streams".to_owned(),
    });
    h.display(Evt::ResponseDone(None));
    h.settle().await;
    // BUG: input row still missing; the cursor floats mid-viewport.
    insta::assert_snapshot!("tiny_5_rows_response", h.screen_with_scrollback());
}

/// The /help box is ~57 columns wide; on a 40-column terminal each row
/// wraps, and on a reflowing terminal the wrapped rows interact with the
/// scroll region.
#[tokio::test(start_paused = true)]
async fn help_wider_than_terminal() {
    let h = TuiHarness::spawn_reflowing(40, 20).await;

    h.type_str("/help");
    h.key(KeyCode::Enter);
    h.settle().await;
    // BUG: the ~57-column help box wraps on the 40-column terminal and
    // corrupts scrollback — box rows are interleaved with misaligned "│"
    // fragments from the wrapped right edge.
    insta::assert_snapshot!("help_narrow_40", h.screen_with_scrollback());
}

/// Display events keep arriving while the model picker overlay is open —
/// prints go into the scroll region above while the picker is drawn.
#[tokio::test(start_paused = true)]
async fn prints_while_picker_open() {
    let h = TuiHarness::spawn_reflowing(80, 14).await;
    fill_scrollback(&h, 4).await;

    h.type_str("/model");
    h.key(KeyCode::Enter);
    h.settle().await;

    h.display(Evt::Info("info arriving while picker open 1".to_owned()));
    h.display(Evt::Info("info arriving while picker open 2".to_owned()));
    h.settle().await;
    insta::assert_snapshot!("picker_open_with_prints", h.screen_with_scrollback());

    h.key(KeyCode::Esc);
    h.settle().await;
    insta::assert_snapshot!("picker_closed_after_prints", h.screen_with_scrollback());
}

/// User submits a message while the previous response is still streaming —
/// the echoed `> input` and the unfinished stream contend for the
/// mid-stream state.
#[tokio::test(start_paused = true)]
async fn input_submitted_mid_stream() {
    let h = TuiHarness::spawn_reflowing(80, 14).await;

    h.display(Evt::UserInput("first question".to_owned()));
    h.display(Evt::StartOutput);
    h.display(Evt::TextChunk {
        chunk: "answering the first question".to_owned(),
    });
    h.settle().await;

    h.type_str("second question");
    h.key(KeyCode::Enter);
    h.settle().await;
    h.display(Evt::UserInput("second question".to_owned()));
    h.settle().await;
    h.display(Evt::TextChunk {
        chunk: " ...still streaming the first answer".to_owned(),
    });
    h.settle().await;
    insta::assert_snapshot!("input_mid_stream", h.screen_with_scrollback());
}

/// Spinner + child-thread rows + autocomplete all visible at once, then
/// everything collapses at the same time.
#[tokio::test(start_paused = true)]
async fn spinner_threads_autocomplete_combined() {
    let h = TuiHarness::spawn_reflowing(80, 16).await;
    for i in 1..=4 {
        h.display(Evt::Info(format!("scroll line {i:02}")));
    }

    h.display(Evt::UserInput("busy".to_owned()));
    h.display(Evt::StartOutput);
    h.display_for_thread("child-1", Evt::StartOutput);
    h.display_for_thread(
        "child-1",
        Evt::ThinkingChunk {
            chunk: "child working".to_owned(),
        },
    );
    h.type_str("/");
    h.settle().await;
    insta::assert_snapshot!("combined_grown", h.screen_with_scrollback());

    // Collapse everything in one batch: autocomplete away, thread done,
    // spinner done.
    h.key(KeyCode::Backspace);
    h.display_for_thread("child-1", Evt::ResponseDone(None));
    h.display(Evt::ResponseDone(None));
    h.settle().await;
    h.display(Evt::Info("printed after combined collapse".to_owned()));
    h.settle().await;
    insta::assert_snapshot!("combined_collapsed", h.screen_with_scrollback());
}
