//! Snapshot tests for the terminal UI, rendered through a `vt100` virtual
//! terminal. See `common/mod.rs` for the harness.
//!
//! All tests run with a paused tokio clock so spinner animations and
//! "settling" of the UI task are fully deterministic. Use
//! `tokio::time::advance` (via `advance_and_redraw`) to move animations.

mod common;

use common::{TuiHarness, advance_and_redraw};
use infinity_agent_core::batch_processor::DisplayEvent;
use ratatui::crossterm::event::KeyCode;
use rig_mock::MockStreamingResponse;
use std::time::Duration;

/// Shorthand for the root-thread display event type under test.
type Evt = DisplayEvent<MockStreamingResponse>;

#[tokio::test(start_paused = true)]
async fn startup_screen() {
    let h = TuiHarness::spawn(80, 16).await;
    insta::assert_snapshot!(h.screen());
}

#[tokio::test(start_paused = true)]
async fn typing_and_slash_autocomplete() {
    let h = TuiHarness::spawn(80, 16).await;

    h.type_str("/m");
    h.settle().await;
    insta::assert_snapshot!("slash_autocomplete_open", h.screen());

    // Tab completes to the highlighted command.
    h.key(KeyCode::Tab);
    h.settle().await;
    insta::assert_snapshot!("slash_autocomplete_tab", h.screen());
}

#[tokio::test(start_paused = true)]
async fn user_message_thinking_then_streaming() {
    let h = TuiHarness::spawn(80, 16).await;

    h.display(Evt::UserInput("hello there".to_owned()));
    h.display(Evt::StartOutput);
    h.display(Evt::ThinkingStart);
    h.display(Evt::ThinkingChunk {
        chunk: "pondering the request".to_owned(),
    });
    h.settle().await;
    insta::assert_snapshot!("thinking_in_progress", h.screen());

    h.display(Evt::ThinkingEnd);
    h.display(Evt::TextChunk {
        chunk: "Hi! ".to_owned(),
    });
    h.display(Evt::TextChunk {
        chunk: "How can I help you today?".to_owned(),
    });
    h.settle().await;
    insta::assert_snapshot!("streaming_text", h.screen());

    h.display(Evt::ResponseDone(None));
    h.settle().await;
    insta::assert_snapshot!("response_done", h.screen());
}

/// A tool call and its result arriving while the thinking display is being
/// replaced — a sequence suspected of producing rendering glitches.
#[tokio::test(start_paused = true)]
async fn tool_call_and_result_replace_thinking() {
    let h = TuiHarness::spawn(80, 16).await;

    h.display(Evt::UserInput("read the config file".to_owned()));
    h.display(Evt::StartOutput);
    h.display(Evt::ThinkingStart);
    h.display(Evt::ThinkingChunk {
        chunk: "I should read the file".to_owned(),
    });
    h.settle().await;
    advance_and_redraw(&h, Duration::from_millis(500)).await;
    insta::assert_snapshot!("thinking_before_tool_call", h.screen());

    // Tool call + result arrive in the same batch, while the thinking
    // spinner row is still up.
    h.display(Evt::ToolCall {
        name: "read_file".to_owned(),
        args: serde_json::json!({"path": "config.toml"}),
        display_as: Some("read_file(config.toml)".to_owned()),
    });
    h.display(Evt::ToolResult {
        segments: vec![rap_protocol::DisplaySegment::Text(
            "[workspace]\nmembers = [\"a\", \"b\"]".to_owned(),
        )],
    });
    h.settle().await;
    insta::assert_snapshot!("tool_call_and_result_same_batch", h.screen());

    h.display(Evt::StartOutput);
    h.display(Evt::TextChunk {
        chunk: "The config defines a workspace.".to_owned(),
    });
    h.display(Evt::ResponseDone(None));
    h.settle().await;
    insta::assert_snapshot!("after_tool_call_response", h.screen());
}

#[tokio::test(start_paused = true)]
async fn resize_narrower_mid_stream() {
    let h = TuiHarness::spawn(80, 16).await;

    h.display(Evt::UserInput("tell me something long".to_owned()));
    h.display(Evt::StartOutput);
    h.display(Evt::TextChunk {
        chunk: "This is a fairly long line of streaming text that approaches the width.".to_owned(),
    });
    h.settle().await;
    insta::assert_snapshot!("before_resize", h.screen());

    h.resize(40, 16);
    h.settle().await;
    insta::assert_snapshot!("after_resize_narrow", h.screen());

    h.resize(80, 16);
    h.settle().await;
    insta::assert_snapshot!("after_resize_back_wide", h.screen());
}

#[tokio::test(start_paused = true)]
async fn help_overlay() {
    let h = TuiHarness::spawn(80, 40).await;

    h.type_str("/help");
    h.key(KeyCode::Enter);
    h.settle().await;
    insta::assert_snapshot!(h.screen());
}

#[tokio::test(start_paused = true)]
async fn child_thread_activity_rows() {
    let h = TuiHarness::spawn(80, 16).await;

    h.display(Evt::UserInput("do things in parallel".to_owned()));
    h.display(Evt::StartOutput);
    h.display_for_thread("thread-aaaa1111", Evt::StartOutput);
    h.display_for_thread(
        "thread-aaaa1111",
        Evt::ThinkingChunk {
            chunk: "child one is thinking about its task".to_owned(),
        },
    );
    h.display_for_thread("thread-bbbb2222", Evt::StartOutput);
    h.display_for_thread(
        "thread-bbbb2222",
        Evt::ToolCall {
            name: "execute_command".to_owned(),
            args: serde_json::json!({"command": "ls"}),
            display_as: Some("execute_command(ls)".to_owned()),
        },
    );
    h.settle().await;
    insta::assert_snapshot!(h.screen());
}

#[tokio::test(start_paused = true)]
async fn typed_message_submits_to_agent() {
    let mut h = TuiHarness::spawn(80, 16).await;

    h.type_str("hello agent");
    h.key(KeyCode::Enter);
    h.settle().await;

    // The message is handed to the agent…
    let sent = h.input_rx.try_recv().expect("input should be forwarded");
    assert_eq!(sent, "hello agent");
    // …and the input box is cleared again.
    insta::assert_snapshot!(h.screen());
}

#[tokio::test(start_paused = true)]
async fn session_loaded_updates_title_and_status() {
    let h = TuiHarness::spawn(80, 16).await;

    h.session_tx
        .send(infinity_agent_cli::terminal::SessionChanged {
            session_id: "f00dcafe-1234-5678-9abc-def012345678".to_owned(),
            title: Some("My fancy session".to_owned()),
            total_tokens_used: 42_000,
            model_name: "mock-model".to_owned(),
            context_window: 100_000,
        })
        .expect("UI task dropped session channel");
    h.settle().await;
    insta::assert_snapshot!(h.screen());
}

#[tokio::test(start_paused = true)]
async fn session_loaded_updates_model_name() {
    let h = TuiHarness::spawn(80, 16).await;

    // Load a session whose thread uses a different model than the default.
    h.session_tx
        .send(infinity_agent_cli::terminal::SessionChanged {
            session_id: "aaa-bbb".to_owned(),
            title: Some("Other model session".to_owned()),
            total_tokens_used: 10_000,
            model_name: "Mock Mini".to_owned(),
            context_window: 32_000,
        })
        .expect("UI task dropped session channel");
    h.settle().await;
    insta::assert_snapshot!(h.screen());
}

#[tokio::test(start_paused = true)]
async fn spinner_states_animate_over_time() {
    let h = TuiHarness::spawn(80, 16).await;

    h.display(Evt::UserInput("hi".to_owned()));
    h.display(Evt::StartOutput);
    h.settle().await;
    insta::assert_snapshot!("spinner_loading_context_t0", h.screen());

    advance_and_redraw(&h, Duration::from_millis(300)).await;
    insta::assert_snapshot!("spinner_loading_context_t300ms", h.screen());

    h.display(Evt::ToolCall {
        name: "sleep".to_owned(),
        args: serde_json::json!({"seconds": 5}),
        display_as: None,
    });
    h.settle().await;
    advance_and_redraw(&h, Duration::from_millis(700)).await;
    insta::assert_snapshot!("spinner_waiting_tool_call", h.screen());
}
