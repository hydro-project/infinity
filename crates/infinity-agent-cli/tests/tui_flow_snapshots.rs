//! Snapshot tests for session/command flows: replay bursts after a session
//! switch, detach/quit flows, choice queueing, paste, child-thread
//! lifecycle, and tool-result display variants.
//!
//! The replay test covers the "viewport and scrollback get mixed up
//! during replay" class of corruption seen in terminals like zellij: a
//! session load delivers the whole history as one burst of display events,
//! toggling the spinner row (and thus the viewport height) many times
//! between prints, all drained in a single biased-select pass.

mod common;

use common::TuiHarness;
use infinity_agent_cli::terminal::{DetachResult, SessionChanged};
use infinity_agent_core::batch_processor::DisplayEvent;
use ratatui::crossterm::event::{Event, KeyCode, KeyModifiers};
use rig::completion::Usage;
use rig_mock::MockStreamingResponse;

type Evt = DisplayEvent<MockStreamingResponse>;

fn usage(total: u64) -> MockStreamingResponse {
    MockStreamingResponse {
        usage: Some(Usage {
            total_tokens: total,
            ..Usage::default()
        }),
    }
}

fn send_session(h: &TuiHarness, id: &str, title: &str, tokens: usize) {
    h.session_tx
        .send(SessionChanged {
            session_id: id.to_owned(),
            title: Some(title.to_owned()),
            total_tokens_used: tokens,
            model_name: "mock-model".to_owned(),
            context_window: 100_000,
            provider_id: "mock".to_owned(),
        })
        .expect("bug: UI task dropped session channel");
}

/// Load a session while another one already has content on screen: the
/// daemon replays the whole history as one burst — user inputs, thinking,
/// tool calls/results, child-thread events, response boundaries — with the
/// spinner row appearing and disappearing many times between prints.
#[tokio::test(start_paused = true)]
async fn session_replay_burst() {
    let h = TuiHarness::spawn_reflowing(80, 18).await;

    // Some live conversation first, so the replay lands on a used screen.
    h.display(Evt::UserInput("old session question".to_owned()));
    h.display(Evt::StartOutput);
    h.display(Evt::TextChunk {
        chunk: "old session answer".to_owned(),
    });
    h.display(Evt::ResponseDone(Some(usage(1_000))));
    h.settle().await;

    // Switch sessions: SessionChanged followed immediately by the replayed
    // history, exactly as daemon_client forwards a Replay message.
    send_session(&h, "replayed-session-0001", "Replayed session", 42_000);
    h.display(Evt::UserInput("replayed question one".to_owned()));
    h.display(Evt::StartOutput);
    h.display(Evt::ThinkingStart);
    h.display(Evt::ThinkingChunk {
        chunk: "replayed thinking".to_owned(),
    });
    h.display(Evt::ThinkingEnd);
    h.display(Evt::TextChunk {
        chunk: "replayed answer one".to_owned(),
    });
    h.display(Evt::ToolCall {
        name: "read_file".to_owned(),
        args: serde_json::json!({"path": "a.txt"}),
        display_as: Some("read_file(a.txt)".to_owned()),
    });
    h.display(Evt::ToolResult {
        segments: vec![rap_protocol::DisplaySegment::Text("contents".to_owned())],
    });
    h.display(Evt::TextChunk {
        chunk: "more of answer one".to_owned(),
    });
    h.display(Evt::ResponseDone(Some(usage(10_000))));
    h.display(Evt::UserInput("replayed question two".to_owned()));
    h.display(Evt::StartOutput);
    h.display_for_thread("replay-child", Evt::StartOutput);
    h.display_for_thread(
        "replay-child",
        Evt::ThinkingChunk {
            chunk: "replayed child activity".to_owned(),
        },
    );
    h.display(Evt::TextChunk {
        chunk: "replayed answer two".to_owned(),
    });
    // Final ResponseDone the daemon appends after the history.
    h.display(Evt::ResponseDone(Some(usage(42_000))));
    h.settle().await;
    insta::assert_snapshot!("replay_burst", h.screen_with_scrollback());

    // Live traffic after the replay must render normally.
    h.display(Evt::UserInput("live question after replay".to_owned()));
    h.display(Evt::StartOutput);
    h.display(Evt::TextChunk {
        chunk: "live answer after replay".to_owned(),
    });
    h.display(Evt::ResponseDone(Some(usage(43_000))));
    h.settle().await;
    insta::assert_snapshot!("replay_then_live", h.screen_with_scrollback());
}

/// Loading a session populates the context indicator from the session info
/// in the Connected message; the replayed history carries no usage data and
/// ends with a usage-less ResponseDone marker. Neither that marker nor a
/// live response without usage metadata may reset the indicator to zero.
#[tokio::test(start_paused = true)]
async fn replay_keeps_context_usage() {
    let h = TuiHarness::spawn(80, 14).await;

    send_session(&h, "replayed-session-0001", "Replayed session", 42_000);
    h.display(Evt::UserInput("replayed question".to_owned()));
    h.display(Evt::StartOutput);
    h.display(Evt::TextChunk {
        chunk: "replayed answer".to_owned(),
    });
    // The end-of-replay marker daemon_client appends after the history.
    h.display(Evt::ResponseDone(None));
    // A response whose provider reported no usage must not reset it either.
    h.display(Evt::ResponseDone(Some(MockStreamingResponse {
        usage: None,
    })));
    h.settle().await;
    insta::assert_snapshot!("replay_keeps_context_usage", h.screen());
}

/// Tool results rendered as a unified diff (Diff segment branch) and with
/// no segments at all (bare " ✓" branch).
#[tokio::test(start_paused = true)]
async fn diff_and_empty_tool_results() {
    let h = TuiHarness::spawn_reflowing(80, 18).await;

    h.display(Evt::UserInput("edit the file".to_owned()));
    h.display(Evt::StartOutput);
    h.display(Evt::ToolCall {
        name: "edit_file".to_owned(),
        args: serde_json::json!({"path": "main.rs"}),
        display_as: Some("edit_file(main.rs)".to_owned()),
    });
    h.display(Evt::ToolResult {
        segments: vec![rap_protocol::DisplaySegment::Diff(
            rap_protocol::DiffContent {
                path: "main.rs".to_owned(),
                patch: "@@ -1,3 +1,3 @@\n fn main() {\n-    println!(\"old\");\n+    println!(\"new\");\n }".to_owned(),
            },
        )],
    });
    h.settle().await;
    insta::assert_snapshot!("diff_tool_result", h.screen_with_scrollback());

    h.display(Evt::ToolCall {
        name: "set_title".to_owned(),
        args: serde_json::json!({"title": "t"}),
        display_as: None,
    });
    h.display(Evt::ToolResult { segments: vec![] });
    h.display(Evt::ResponseDone(None));
    h.settle().await;
    insta::assert_snapshot!("empty_tool_result", h.screen_with_scrollback());
}

/// Child-thread lifecycle specifics: a `close_thread` tool call removes the
/// row immediately (it never gets a result), and child subscription events
/// print with a `[thread-id] ` prefix.
#[tokio::test(start_paused = true)]
async fn child_close_thread_and_subscription_prefix() {
    let h = TuiHarness::spawn_reflowing(80, 14).await;

    h.display(Evt::UserInput("spawn and close".to_owned()));
    h.display(Evt::StartOutput);
    h.display_for_thread("child-abcdef12", Evt::StartOutput);
    h.display_for_thread(
        "child-abcdef12",
        Evt::ThinkingChunk {
            chunk: "child working".to_owned(),
        },
    );
    h.display_for_thread(
        "child-abcdef12",
        Evt::SubscriptionEvent {
            name: "timer".to_owned(),
            text: "tick from the child".to_owned(),
        },
    );
    h.settle().await;
    insta::assert_snapshot!("child_subscription_prefix", h.screen_with_scrollback());

    h.display_for_thread(
        "child-abcdef12",
        Evt::ToolCall {
            name: "close_thread".to_owned(),
            args: serde_json::json!({"thread_id": "child-abcdef12"}),
            display_as: None,
        },
    );
    h.settle().await;
    insta::assert_snapshot!("child_closed_row_removed", h.screen_with_scrollback());
}

/// Bracketed paste delivers multi-line text as a single Event::Paste; it
/// must land in the input box without submitting on the embedded newlines.
#[tokio::test(start_paused = true)]
async fn bracketed_paste_multiline() {
    let mut h = TuiHarness::spawn(80, 14).await;

    h.event_tx
        .send(Event::Paste(
            "pasted one\npasted two\npasted three".to_owned(),
        ))
        .expect("bug: UI task dropped event channel");
    h.settle().await;
    insta::assert_snapshot!("paste_in_input", h.screen());

    h.key(KeyCode::Enter);
    h.settle().await;
    let sent = h.input_rx.try_recv().expect("paste should submit on Enter");
    assert_eq!(sent, "pasted one\npasted two\npasted three");
}

/// Ctrl+C with an active non-idle session: soft detach is requested, the
/// daemon answers NotIdle, and the quit picker overlay appears; Esc returns
/// to normal mode.
#[tokio::test(start_paused = true)]
async fn quit_picker_when_not_idle() {
    let mut h = TuiHarness::spawn(80, 14).await;

    send_session(&h, "busy-session-0001", "Busy session", 5_000);
    h.settle().await;

    h.key_with(KeyCode::Char('c'), KeyModifiers::CONTROL);
    h.settle().await;
    h.soft_detach_rx
        .try_recv()
        .expect("Ctrl+C with a session should request soft detach");
    h.detach_result_tx
        .send(DetachResult::NotIdle)
        .expect("bug: UI task dropped detach channel");
    h.settle().await;
    insta::assert_snapshot!("quit_picker_open", h.screen());

    h.key(KeyCode::Esc);
    h.settle().await;
    insta::assert_snapshot!("quit_picker_cancelled", h.screen());
}

/// `/new` with an active session: soft detach, daemon answers Idle, the UI
/// requests a lazy new session and resets its per-session state.
#[tokio::test(start_paused = true)]
async fn lazy_new_session_after_detach() {
    let mut h = TuiHarness::spawn(80, 14).await;

    send_session(&h, "current-session-01", "Current", 9_000);
    h.display(Evt::UserInput("hi".to_owned()));
    h.display(Evt::StartOutput);
    h.settle().await;

    h.type_str("/new");
    h.key(KeyCode::Enter);
    h.settle().await;
    h.soft_detach_rx
        .try_recv()
        .expect("/new with a session should request soft detach");
    h.detach_result_tx
        .send(DetachResult::Idle)
        .expect("bug: UI task dropped detach channel");
    h.settle().await;
    let (target, stop) = h
        .load_session_rx
        .try_recv()
        .expect("idle detach should request session load");
    assert_eq!((target, stop), (None, false));
    insta::assert_snapshot!("lazy_new_session", h.screen());
}

/// Two choice requests queue up; answering the first reveals the second,
/// and an external UserChoiceComplete dismisses it without an answer.
#[tokio::test(start_paused = true)]
async fn queued_choices_and_external_complete() {
    let mut h = TuiHarness::spawn(80, 16).await;

    h.display(Evt::UserChoiceRequired {
        id: "choice-1".to_owned(),
        prompt: "First question?".to_owned(),
        choices: vec!["Yes".to_owned(), "No".to_owned()],
        default: 0,
        response_url: String::new(),
    });
    h.display(Evt::UserChoiceRequired {
        id: "choice-2".to_owned(),
        prompt: "Second question?".to_owned(),
        choices: vec!["Red".to_owned(), "Green".to_owned(), "Blue".to_owned()],
        default: 1,
        response_url: String::new(),
    });
    h.settle().await;
    insta::assert_snapshot!("first_choice_shown", h.screen());

    h.key(KeyCode::Enter);
    h.settle().await;
    let (id, idx) = h
        .choice_answered_rx
        .try_recv()
        .expect("first choice should be answered");
    assert_eq!((id.as_str(), idx), ("choice-1", 0));
    insta::assert_snapshot!("second_choice_shown", h.screen());

    h.display(Evt::UserChoiceComplete {
        choice_id: "choice-2".to_owned(),
    });
    h.settle().await;
    insta::assert_snapshot!("choices_all_dismissed", h.screen());
}

/// Sessions-updated notifications arriving while the session picker is open
/// must refresh the visible list.
#[tokio::test(start_paused = true)]
async fn sessions_updated_while_picker_open() {
    let mut sessions = std::collections::HashMap::new();
    sessions.insert(
        "session-aaaa".to_owned(),
        infinity_protocol::SessionInfo {
            title: Some("Old title".to_owned()),
            last_updated: "2025-01-01T00:00:00Z".to_owned(),
            total_tokens_used: 100,
            status: infinity_protocol::SessionStatus::Idle,
            threads: vec![],
            remote: None,
        },
    );
    let h = TuiHarness::spawn_with(common::HarnessOptions {
        cols: 80,
        rows: 14,
        initial_sessions: sessions,
        ..common::HarnessOptions::default()
    })
    .await;

    h.type_str("/load");
    h.key(KeyCode::Enter);
    h.settle().await;
    insta::assert_snapshot!("picker_initial_list", h.screen());

    let mut updates = std::collections::HashMap::new();
    updates.insert(
        "session-aaaa".to_owned(),
        infinity_protocol::SessionInfo {
            title: Some("Renamed title".to_owned()),
            last_updated: "2025-01-02T00:00:00Z".to_owned(),
            total_tokens_used: 200,
            status: infinity_protocol::SessionStatus::Idle,
            threads: vec![],
            remote: None,
        },
    );
    updates.insert(
        "session-bbbb".to_owned(),
        infinity_protocol::SessionInfo {
            title: Some("Brand new session".to_owned()),
            last_updated: "2025-01-03T00:00:00Z".to_owned(),
            total_tokens_used: 300,
            status: infinity_protocol::SessionStatus::Running,
            threads: vec![],
            remote: None,
        },
    );
    h.sessions_updated_tx
        .send(updates)
        .expect("bug: UI task dropped sessions-updated channel");
    h.settle().await;
    insta::assert_snapshot!("picker_after_update", h.screen());
}

/// Feedback lines for commands that act without a session: /compact prints
/// its trigger, /stop and /archive print "no active session" notes.
#[tokio::test(start_paused = true)]
async fn slash_command_feedback_without_session() {
    let h = TuiHarness::spawn(80, 14).await;

    h.type_str("/compact");
    h.key(KeyCode::Enter);
    h.type_str("/stop");
    h.key(KeyCode::Enter);
    h.type_str("/archive");
    h.key(KeyCode::Enter);
    h.settle().await;
    insta::assert_snapshot!("command_feedback", h.screen());
}
