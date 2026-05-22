//! Simulation tests for the Slack bot dataflow using Hydro's test utilities.
#![allow(clippy::unwrap_used, reason = "test assertions")]

use futures::StreamExt;
use hydro_lang::prelude::*;

#[tokio::test]
async fn filter_drops_bot_and_unauthorized() {
    hydro_lang::test_util::stream_transform_test(
        |process| {
            let input = process.source_stream(q!({
                infinity_slack_bot::runtime::ensure_test_init();
                futures_util::stream::iter(vec![
                    // Bot message — should be dropped.
                    infinity_slack_bot::sidecar::SlackEvent {
                        user: "U1".to_owned(),
                        text: "hello".to_owned(),
                        channel: "C1".to_owned(),
                        thread_ts: "1.0".to_owned(),
                        is_button_click: false,
                        button_value: None,
                        action_id: None,
                        is_bot: true,
                        is_unauthorized: false,
                    },
                    // Unauthorized message — should be dropped.
                    infinity_slack_bot::sidecar::SlackEvent {
                        user: "U2".to_owned(),
                        text: "hello".to_owned(),
                        channel: "C1".to_owned(),
                        thread_ts: "2.0".to_owned(),
                        is_button_click: false,
                        button_value: None,
                        action_id: None,
                        is_bot: false,
                        is_unauthorized: true,
                    },
                    // Valid message — should produce a CreateSession command.
                    infinity_slack_bot::sidecar::SlackEvent {
                        user: "U3".to_owned(),
                        text: "hi".to_owned(),
                        channel: "C1".to_owned(),
                        thread_ts: "3.0".to_owned(),
                        is_button_click: false,
                        button_value: None,
                        action_id: None,
                        is_bot: false,
                        is_unauthorized: false,
                    },
                ])
            }));
            let daemon_events = process.source_stream(q!(futures_util::stream::pending::<
                infinity_slack_bot::daemon_sidecar::DaemonEvent,
            >()));
            let (_, daemon_commands) =
                infinity_slack_bot::flow::slack_dataflow(input, daemon_events);
            daemon_commands
        },
        |mut stream| async move {
            let cmd = stream.next().await.unwrap();
            assert!(matches!(
                cmd,
                infinity_slack_bot::daemon_sidecar::DaemonCommand::CreateSession { .. }
            ));
        },
    )
    .await;
}

#[tokio::test]
async fn normal_message_produces_create_session() {
    hydro_lang::test_util::stream_transform_test(
        |process| {
            let input = process.source_stream(q!({
                infinity_slack_bot::runtime::ensure_test_init();
                futures_util::stream::iter(vec![infinity_slack_bot::sidecar::SlackEvent {
                    user: "U1".to_owned(),
                    text: "hello".to_owned(),
                    channel: "C1".to_owned(),
                    thread_ts: "new.1".to_owned(),
                    is_button_click: false,
                    button_value: None,
                    action_id: None,
                    is_bot: false,
                    is_unauthorized: false,
                }])
            }));
            let daemon_events = process.source_stream(q!(futures_util::stream::pending::<
                infinity_slack_bot::daemon_sidecar::DaemonEvent,
            >()));
            let (_, daemon_commands) =
                infinity_slack_bot::flow::slack_dataflow(input, daemon_events);
            daemon_commands
        },
        |mut stream| async move {
            let cmd = stream.next().await.unwrap();
            match cmd {
                infinity_slack_bot::daemon_sidecar::DaemonCommand::CreateSession {
                    thread_ts,
                    ..
                } => {
                    assert_eq!(thread_ts, "new.1");
                }
                other => panic!("expected CreateSession, got {other:?}"),
            }
        },
    )
    .await;
}

#[tokio::test]
async fn existing_session_produces_send_input() {
    hydro_lang::test_util::stream_transform_test(
        |process| {
            let input = process.source_stream(q!({
                infinity_slack_bot::runtime::ensure_test_init();
                // Pre-populate session store.
                let rt = infinity_slack_bot::runtime::get();
                rt.sessions
                    .lock()
                    .unwrap()
                    .insert_ephemeral("existing.0".to_owned(), "sess-123".to_owned());
                futures_util::stream::iter(vec![infinity_slack_bot::sidecar::SlackEvent {
                    user: "U1".to_owned(),
                    text: "follow up".to_owned(),
                    channel: "C1".to_owned(),
                    thread_ts: "existing.0".to_owned(),
                    is_button_click: false,
                    button_value: None,
                    action_id: None,
                    is_bot: false,
                    is_unauthorized: false,
                }])
            }));
            let daemon_events = process.source_stream(q!(futures_util::stream::pending::<
                infinity_slack_bot::daemon_sidecar::DaemonEvent,
            >()));
            let (_, daemon_commands) =
                infinity_slack_bot::flow::slack_dataflow(input, daemon_events);
            daemon_commands
        },
        |mut stream| async move {
            let cmd = stream.next().await.unwrap();
            match cmd {
                infinity_slack_bot::daemon_sidecar::DaemonCommand::SendInput {
                    thread_ts,
                    session_id,
                    text,
                } => {
                    assert_eq!(thread_ts, "existing.0");
                    assert_eq!(session_id, "sess-123");
                    assert_eq!(text, "follow up");
                }
                other => panic!("expected SendInput, got {other:?}"),
            }
        },
    )
    .await;
}

/// When a ToolCall occurs, the intermediate ResponseDone should NOT produce StreamStop.
/// The stream stays open for post-tool output.
#[tokio::test]
async fn tool_call_keeps_stream_open_across_response_done() {
    hydro_lang::test_util::stream_transform_test(
        |process| {
            let daemon_events = process.source_stream(q!({
                infinity_slack_bot::runtime::ensure_test_init();
                let rt = infinity_slack_bot::runtime::get();
                rt.channels
                    .lock()
                    .unwrap()
                    .insert("tool.1".to_owned(), "C1".to_owned());
                futures_util::stream::iter(vec![
                    // Model produces some text, then a tool call, then ResponseDone.
                    infinity_slack_bot::daemon_sidecar::DaemonEvent {
                        thread_ts: "tool.1".to_owned(),
                        message: infinity_protocol::DaemonMessage::TextChunk {
                            thread_id: None,
                            chunk: "Let me check...".to_owned(),
                        },
                    },
                    infinity_slack_bot::daemon_sidecar::DaemonEvent {
                        thread_ts: "tool.1".to_owned(),
                        message: infinity_protocol::DaemonMessage::ToolCall {
                            name: "read_file".to_owned(),
                            args: "{}".to_owned(),
                            thread_id: None,
                            display_as: None,
                        },
                    },
                    // This ResponseDone should NOT stop the stream.
                    infinity_slack_bot::daemon_sidecar::DaemonEvent {
                        thread_ts: "tool.1".to_owned(),
                        message: infinity_protocol::DaemonMessage::ResponseDone {
                            thread_id: None,
                            token_usage: None,
                        },
                    },
                    // After tool execution, more text arrives and final ResponseDone.
                    infinity_slack_bot::daemon_sidecar::DaemonEvent {
                        thread_ts: "tool.1".to_owned(),
                        message: infinity_protocol::DaemonMessage::TextChunk {
                            thread_id: None,
                            chunk: "Here's the result.".to_owned(),
                        },
                    },
                    // This ResponseDone SHOULD stop the stream.
                    infinity_slack_bot::daemon_sidecar::DaemonEvent {
                        thread_ts: "tool.1".to_owned(),
                        message: infinity_protocol::DaemonMessage::ResponseDone {
                            thread_id: None,
                            token_usage: None,
                        },
                    },
                ])
            }));
            let slack_events = process.source_stream(q!(futures_util::stream::pending::<
                infinity_slack_bot::sidecar::SlackEvent,
            >()));
            let (slack_actions, _) =
                infinity_slack_bot::flow::slack_dataflow(slack_events, daemon_events);
            slack_actions
        },
        |mut stream| async move {
            // 1. StreamAppend for "Let me check..."
            let action = stream.next().await.unwrap();
            match &action {
                infinity_slack_bot::sidecar::SlackAction::StreamAppend { text, .. } => {
                    assert_eq!(text, "Let me check...");
                }
                other => panic!("expected StreamAppend('Let me check...'), got {other:?}"),
            }

            // 2. StreamAppend for the tool call indicator
            let action = stream.next().await.unwrap();
            match &action {
                infinity_slack_bot::sidecar::SlackAction::StreamAppend { text, .. } => {
                    assert!(text.contains("read_file"), "expected tool name, got {text}");
                }
                other => panic!("expected StreamAppend(tool), got {other:?}"),
            }

            // 3. The intermediate ResponseDone produces NOTHING (no StreamStop).
            //    Next action should be StreamAppend for "Here's the result."
            let action = stream.next().await.unwrap();
            match &action {
                infinity_slack_bot::sidecar::SlackAction::StreamAppend { text, .. } => {
                    assert_eq!(text, "Here's the result.");
                }
                other => panic!(
                    "expected StreamAppend('Here\\'s the result.'), got {other:?} — \
                     intermediate ResponseDone should not have produced StreamStop"
                ),
            }

            // 4. Final ResponseDone produces StreamStop.
            let action = stream.next().await.unwrap();
            assert!(
                matches!(
                    action,
                    infinity_slack_bot::sidecar::SlackAction::StreamStop { .. }
                ),
                "expected StreamStop after final ResponseDone, got {action:?}"
            );
        },
    )
    .await;
}

/// Multiple tool calls in a single turn keep the stream open until the final response.
#[tokio::test]
async fn multiple_tool_calls_keep_stream_open() {
    hydro_lang::test_util::stream_transform_test(
        |process| {
            let daemon_events = process.source_stream(q!({
                infinity_slack_bot::runtime::ensure_test_init();
                let rt = infinity_slack_bot::runtime::get();
                rt.channels
                    .lock()
                    .unwrap()
                    .insert("multi.1".to_owned(), "C1".to_owned());
                futures_util::stream::iter(vec![
                    // Two tool calls in one turn.
                    infinity_slack_bot::daemon_sidecar::DaemonEvent {
                        thread_ts: "multi.1".to_owned(),
                        message: infinity_protocol::DaemonMessage::ToolCall {
                            name: "grep".to_owned(),
                            args: "{}".to_owned(),
                            thread_id: None,
                            display_as: None,
                        },
                    },
                    infinity_slack_bot::daemon_sidecar::DaemonEvent {
                        thread_ts: "multi.1".to_owned(),
                        message: infinity_protocol::DaemonMessage::ToolCall {
                            name: "read_file".to_owned(),
                            args: "{}".to_owned(),
                            thread_id: None,
                            display_as: None,
                        },
                    },
                    // ResponseDone after tools — stream should stay open.
                    infinity_slack_bot::daemon_sidecar::DaemonEvent {
                        thread_ts: "multi.1".to_owned(),
                        message: infinity_protocol::DaemonMessage::ResponseDone {
                            thread_id: None,
                            token_usage: None,
                        },
                    },
                    // Final text and done.
                    infinity_slack_bot::daemon_sidecar::DaemonEvent {
                        thread_ts: "multi.1".to_owned(),
                        message: infinity_protocol::DaemonMessage::TextChunk {
                            thread_id: None,
                            chunk: "Done!".to_owned(),
                        },
                    },
                    infinity_slack_bot::daemon_sidecar::DaemonEvent {
                        thread_ts: "multi.1".to_owned(),
                        message: infinity_protocol::DaemonMessage::ResponseDone {
                            thread_id: None,
                            token_usage: None,
                        },
                    },
                ])
            }));
            let slack_events = process.source_stream(q!(futures_util::stream::pending::<
                infinity_slack_bot::sidecar::SlackEvent,
            >()));
            let (slack_actions, _) =
                infinity_slack_bot::flow::slack_dataflow(slack_events, daemon_events);
            slack_actions
        },
        |mut stream| async move {
            // 1. StreamAppend for grep tool indicator
            let action = stream.next().await.unwrap();
            assert!(
                matches!(&action, infinity_slack_bot::sidecar::SlackAction::StreamAppend { text, .. } if text.contains("grep")),
                "expected grep tool append, got {action:?}"
            );

            // 2. StreamAppend for read_file tool indicator
            let action = stream.next().await.unwrap();
            assert!(
                matches!(&action, infinity_slack_bot::sidecar::SlackAction::StreamAppend { text, .. } if text.contains("read_file")),
                "expected read_file tool append, got {action:?}"
            );

            // 3. No StreamStop here — next should be StreamAppend("Done!")
            let action = stream.next().await.unwrap();
            match &action {
                infinity_slack_bot::sidecar::SlackAction::StreamAppend { text, .. } => {
                    assert_eq!(text, "Done!");
                }
                other => panic!("expected StreamAppend('Done!'), got {other:?}"),
            }

            // 4. Final StreamStop
            let action = stream.next().await.unwrap();
            assert!(
                matches!(action, infinity_slack_bot::sidecar::SlackAction::StreamStop { .. }),
                "expected StreamStop, got {action:?}"
            );
        },
    )
    .await;
}

#[tokio::test]
async fn daemon_text_chunk_produces_stream_append() {
    hydro_lang::test_util::stream_transform_test(
        |process| {
            let daemon_events = process.source_stream(q!({
                infinity_slack_bot::runtime::ensure_test_init();
                // Pre-populate channel mapping so the action has the channel.
                let rt = infinity_slack_bot::runtime::get();
                rt.channels
                    .lock()
                    .unwrap()
                    .insert("resp.1".to_owned(), "C1".to_owned());
                futures_util::stream::iter(vec![
                    infinity_slack_bot::daemon_sidecar::DaemonEvent {
                        thread_ts: "resp.1".to_owned(),
                        message: infinity_protocol::DaemonMessage::TextChunk {
                            thread_id: None,
                            chunk: "Hello world".to_owned(),
                        },
                    },
                    infinity_slack_bot::daemon_sidecar::DaemonEvent {
                        thread_ts: "resp.1".to_owned(),
                        message: infinity_protocol::DaemonMessage::ResponseDone {
                            thread_id: None,
                            token_usage: None,
                        },
                    },
                ])
            }));
            let slack_events = process.source_stream(q!(futures_util::stream::pending::<
                infinity_slack_bot::sidecar::SlackEvent,
            >()));
            let (slack_actions, _) =
                infinity_slack_bot::flow::slack_dataflow(slack_events, daemon_events);
            slack_actions
        },
        |mut stream| async move {
            let action = stream.next().await.unwrap();
            match action {
                infinity_slack_bot::sidecar::SlackAction::StreamAppend {
                    channel,
                    thread_ts,
                    text,
                } => {
                    assert_eq!(channel, "C1");
                    assert_eq!(thread_ts, "resp.1");
                    assert_eq!(text, "Hello world");
                }
                other => panic!("expected StreamAppend, got {other:?}"),
            }
            let action = stream.next().await.unwrap();
            match action {
                infinity_slack_bot::sidecar::SlackAction::StreamStop { channel, thread_ts } => {
                    assert_eq!(channel, "C1");
                    assert_eq!(thread_ts, "resp.1");
                }
                other => panic!("expected StreamStop, got {other:?}"),
            }
        },
    )
    .await;
}
