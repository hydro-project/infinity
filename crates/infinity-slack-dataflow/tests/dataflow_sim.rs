//! Deterministic simulation tests for the Slack bot dataflow.
//!
//! These drive the dataflow through Hydro's simulator (`flow.sim()`) via
//! `sim_input`/`sim_output` ports — no I/O tasks are run. The simulator
//! compiles the flow into a dylib and explores batching/interleaving
//! schedules, so the tests below follow three rules:
//!
//! 1. **State is driven through the dataflow.** The test process cannot
//!    touch `runtime::get()` inside the sim dylib (it has its own statics),
//!    so sessions are registered by sending a `Connected` daemon event and
//!    channel mappings by sending a chat message, exactly like production.
//! 2. **Fresh identifiers per schedule.** Dylib statics persist across
//!    explored schedules, so every iteration uses unique
//!    thread/channel/user ids (see [`fresh`]) to keep per-thread state
//!    disjoint between iterations.
//! 3. **Schedule-independent assertions.** Streams merged with
//!    `merge_ordered` + `nondet!` may interleave arbitrarily, but order
//!    *within* a branch is preserved. Assertions therefore check the exact
//!    sequence of non-`SetStatus` actions (all from the totally-ordered
//!    daemon branch) and treat `SetStatus` separately.
//!
//! The two single-port tests use `exhaustive`; the tests that interleave
//! both input ports use bounded `fuzz`, since the schedule space across two
//! ports and six `merge_ordered` branches is too large to enumerate.
#![allow(clippy::unwrap_used, reason = "test assertions")]

use hydro_lang::live_collections::stream::{ExactlyOnce, TotalOrder};
use hydro_lang::prelude::*;
use hydro_lang::sim::{SimReceiver, SimSender};
use infinity_slack_dataflow::daemon::{DaemonCommand, DaemonEvent};
use infinity_slack_dataflow::slack::{SlackAction, SlackEvent};

// Integration tests are a separate crate, so the `#[cfg(test)]` init hook from
// `hydro_lang::setup!()` in the library does not apply here. Sim compilation
// requires test mode (it inlines the staged crate and enables the
// `hydro___test` feature, which carries `hydro_lang/sim` from dev-deps).
hydro_lang::macro_support::ctor::declarative::ctor!(
    #[ctor(unsafe)]
    fn init() {
        hydro_lang::compile::init_test();
    }
);

/// Unique id source so each explored schedule gets fresh per-thread state
/// (the sim dylib's statics persist across schedules).
static NEXT_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn fresh(prefix: &str) -> String {
    format!(
        "{prefix}.{}",
        NEXT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    )
}

/// Builds the dataflow under sim, returning (slack_in, daemon_in, actions_out, commands_out).
#[expect(clippy::type_complexity, reason = "Hydro sim port signatures")]
fn build_sim<'a>(
    flow: &mut FlowBuilder<'a>,
) -> (
    SimSender<SlackEvent, TotalOrder, ExactlyOnce>,
    SimSender<DaemonEvent, TotalOrder, ExactlyOnce>,
    SimReceiver<SlackAction, TotalOrder, ExactlyOnce>,
    SimReceiver<DaemonCommand, TotalOrder, ExactlyOnce>,
) {
    let process = flow.process::<()>();

    let (slack_tx, slack_events) = process.sim_input::<SlackEvent, TotalOrder, ExactlyOnce>();
    let (daemon_tx, daemon_events) = process.sim_input::<DaemonEvent, TotalOrder, ExactlyOnce>();

    // The flow's closures call `runtime::get()`, which panics unless the
    // runtime is initialized *inside the sim dylib*. Initialize it lazily on
    // every input path before any downstream closure runs.
    let slack_events = slack_events.map(q!(|e| {
        infinity_slack_dataflow::runtime::ensure_test_init();
        e
    }));
    let daemon_events = daemon_events.map(q!(|e| {
        infinity_slack_dataflow::runtime::ensure_test_init();
        e
    }));

    let (slack_actions, daemon_commands) =
        infinity_slack_dataflow::flow::slack_dataflow(slack_events, daemon_events);

    (
        slack_tx,
        daemon_tx,
        slack_actions.sim_output(),
        daemon_commands.sim_output(),
    )
}

fn chat(user: &str, text: &str, channel: &str, thread_ts: &str) -> SlackEvent {
    SlackEvent {
        user: user.to_owned(),
        text: text.to_owned(),
        channel: channel.to_owned(),
        thread_ts: thread_ts.to_owned(),
        is_button_click: false,
        button_value: None,
        action_id: None,
        message_ts: None,
        button_text: None,
        is_bot: false,
        is_unauthorized: false,
        slash_command: None,
        response_url: None,
        trigger_id: None,
        is_app_home_opened: false,
    }
}

fn devent(thread_ts: &str, message: infinity_protocol::DaemonMessage) -> DaemonEvent {
    DaemonEvent {
        thread_ts: thread_ts.to_owned(),
        message,
    }
}

fn text_chunk(chunk: &str) -> infinity_protocol::DaemonMessage {
    infinity_protocol::DaemonMessage::TextChunk {
        thread_id: None,
        chunk: chunk.to_owned(),
    }
}

fn tool_call(name: &str) -> infinity_protocol::DaemonMessage {
    infinity_protocol::DaemonMessage::ToolCall {
        name: name.to_owned(),
        args: "{}".to_owned(),
        thread_id: None,
        display_as: None,
    }
}

fn response_done() -> infinity_protocol::DaemonMessage {
    infinity_protocol::DaemonMessage::ResponseDone {
        thread_id: None,
        token_usage: None,
    }
}

/// The non-`SetStatus` actions, which all originate from totally-ordered
/// branches and therefore have a deterministic sequence.
fn non_status(actions: &[SlackAction]) -> Vec<&SlackAction> {
    actions
        .iter()
        .filter(|a| !matches!(a, SlackAction::SetStatus { .. }))
        .collect()
}

fn status_texts(actions: &[SlackAction]) -> Vec<&str> {
    actions
        .iter()
        .filter_map(|a| match a {
            SlackAction::SetStatus { status, .. } => Some(status.as_str()),
            _ => None,
        })
        .collect()
}

#[test]
fn filter_drops_bot_and_unauthorized() {
    let mut flow = FlowBuilder::new();
    let (slack_tx, _daemon_tx, _actions_out, commands_out) = build_sim(&mut flow);

    flow.sim().exhaustive(async || {
        let (t_bot, t_unauth, t_valid) = (fresh("bot"), fresh("unauth"), fresh("valid"));
        let channel = fresh("C");

        let mut bot_msg = chat("U1", "hello", &channel, &t_bot);
        bot_msg.is_bot = true;
        let mut unauth_msg = chat("U2", "hello", &channel, &t_unauth);
        unauth_msg.is_unauthorized = true;
        let valid_msg = chat("U3", "hi", &channel, &t_valid);

        slack_tx.send(bot_msg);
        slack_tx.send(unauth_msg);
        slack_tx.send(valid_msg);

        // Only the valid message produces a command.
        let cmds: Vec<DaemonCommand> = commands_out.collect().await;
        assert_eq!(cmds.len(), 1, "expected exactly one command, got {cmds:?}");
        match &cmds[0] {
            DaemonCommand::CreateSession { thread_ts, .. } => {
                assert_eq!(*thread_ts, t_valid);
            }
            other => panic!("expected CreateSession, got {other:?}"),
        }
    });
}

#[test]
fn normal_message_produces_create_session() {
    let mut flow = FlowBuilder::new();
    let (slack_tx, _daemon_tx, actions_out, commands_out) = build_sim(&mut flow);

    flow.sim().exhaustive(async || {
        let thread_ts = fresh("new");
        let channel = fresh("C");
        slack_tx.send(chat("U1", "hello", &channel, &thread_ts));

        let cmds: Vec<DaemonCommand> = commands_out.collect().await;
        assert_eq!(cmds.len(), 1, "expected exactly one command, got {cmds:?}");
        match &cmds[0] {
            DaemonCommand::CreateSession {
                thread_ts: t,
                model,
                ..
            } => {
                assert_eq!(*t, thread_ts);
                assert!(model.is_none());
            }
            other => panic!("expected CreateSession, got {other:?}"),
        }

        // The message also sets an "is thinking" status on its thread.
        let actions: Vec<SlackAction> = actions_out.collect().await;
        assert_eq!(status_texts(&actions), ["is thinking"]);
        assert!(non_status(&actions).is_empty(), "unexpected {actions:?}");
    });
}

/// End-to-end session lifecycle: a first message creates a session, the
/// daemon's `Connected` registers it, and a follow-up on the same thread is
/// routed to the existing session via `SendInput`.
#[test]
fn existing_session_produces_send_input() {
    let mut flow = FlowBuilder::new();
    let (slack_tx, daemon_tx, actions_out, commands_out) = build_sim(&mut flow);

    flow.sim().unit_test_fuzz_iterations(256).fuzz(async || {
        let thread_ts = fresh("existing");
        let channel = fresh("C");
        let session_id = fresh("sess");

        // First message on a fresh thread → CreateSession.
        slack_tx.send(chat("U1", "hi", &channel, &thread_ts));
        match commands_out.next().await.unwrap() {
            DaemonCommand::CreateSession { thread_ts: t, .. } => assert_eq!(t, thread_ts),
            other => panic!("expected CreateSession, got {other:?}"),
        }

        // Daemon confirms the session; the title gives us a synchronization
        // point (SetThreadTitle) proving the registration was processed.
        daemon_tx.send(devent(
            &thread_ts,
            infinity_protocol::DaemonMessage::Connected {
                session_id: session_id.clone(),
                thread_id: String::new(),
                model_name: String::new(),
                context_window: 0,
                title: Some("My Thread".to_owned()),
                total_tokens_used: 0,
                provider_id: String::new(),
            },
        ));
        loop {
            match actions_out.next().await.unwrap() {
                SlackAction::SetThreadTitle { title, .. } => {
                    assert_eq!(title, "My Thread");
                    break;
                }
                // The first message's "is thinking" status may interleave.
                SlackAction::SetStatus { .. } => {}
                other => panic!("expected SetThreadTitle, got {other:?}"),
            }
        }

        // Follow-up on the same thread → SendInput to the stored session.
        slack_tx.send(chat("U1", "follow up", &channel, &thread_ts));
        match commands_out.next().await.unwrap() {
            DaemonCommand::SendInput {
                thread_ts: t,
                session_id: s,
                text,
            } => {
                assert_eq!(t, thread_ts);
                assert_eq!(s, session_id);
                assert_eq!(text, "follow up");
            }
            other => panic!("expected SendInput, got {other:?}"),
        }
    });
}

/// When a ToolCall occurs, the intermediate ResponseDone should NOT produce
/// StreamStop. The stream stays open for post-tool output.
#[test]
fn tool_call_keeps_stream_open_across_response_done() {
    let mut flow = FlowBuilder::new();
    let (slack_tx, daemon_tx, actions_out, commands_out) = build_sim(&mut flow);

    flow.sim().unit_test_fuzz_iterations(256).fuzz(async || {
        let thread_ts = fresh("tool");
        let channel = fresh("C");

        // Establish the thread → channel mapping through the dataflow.
        slack_tx.send(chat("U1", "run it", &channel, &thread_ts));
        assert!(matches!(
            commands_out.next().await.unwrap(),
            DaemonCommand::CreateSession { .. }
        ));

        // Model produces text, then a tool call, then an intermediate
        // ResponseDone, then post-tool text and the final ResponseDone.
        daemon_tx.send_many([
            devent(&thread_ts, text_chunk("Let me check...")),
            devent(&thread_ts, tool_call("read_file")),
            devent(&thread_ts, response_done()),
            devent(&thread_ts, text_chunk("Here's the result.")),
            devent(&thread_ts, response_done()),
        ]);

        let actions: Vec<SlackAction> = actions_out.collect().await;

        // The daemon-driven actions are totally ordered; the intermediate
        // ResponseDone must not have produced a StreamStop.
        let core = non_status(&actions);
        match &core[..] {
            [SlackAction::StreamAppend {
                text: t1,
                channel: c1,
                ..
            }, SlackAction::StreamTaskUpdate {
                title,
                status,
                details,
                ..
            }, SlackAction::StreamAppend { text: t2, .. }, SlackAction::StreamStop {
                channel: c2,
                thread_ts: t,
            }] => {
                assert_eq!(t1, "Let me check...");
                assert_eq!(c1, &channel);
                assert_eq!(title, "read_file");
                assert_eq!(status, "in_progress");
                assert_eq!(details, "{}");
                assert_eq!(t2, "Here's the result.");
                assert_eq!(c2, &channel);
                assert_eq!(t, &thread_ts);
            }
            other => panic!(
                "expected [append, task, append, stop] — the intermediate \
                 ResponseDone must not stop the stream — got {other:?}"
            ),
        }

        // The tool call is reflected in the thread status.
        assert!(
            status_texts(&actions).contains(&"is running read_file"),
            "missing tool status in {actions:?}"
        );
    });
}

/// Multiple tool calls in a single turn keep the stream open until the final
/// response.
#[test]
fn multiple_tool_calls_keep_stream_open() {
    let mut flow = FlowBuilder::new();
    let (slack_tx, daemon_tx, actions_out, commands_out) = build_sim(&mut flow);

    flow.sim().unit_test_fuzz_iterations(256).fuzz(async || {
        let thread_ts = fresh("multi");
        let channel = fresh("C");

        slack_tx.send(chat("U1", "run them", &channel, &thread_ts));
        assert!(matches!(
            commands_out.next().await.unwrap(),
            DaemonCommand::CreateSession { .. }
        ));

        daemon_tx.send_many([
            devent(&thread_ts, tool_call("grep")),
            devent(&thread_ts, tool_call("read_file")),
            devent(&thread_ts, response_done()),
            devent(&thread_ts, text_chunk("Done!")),
            devent(&thread_ts, response_done()),
        ]);

        let actions: Vec<SlackAction> = actions_out.collect().await;

        let core = non_status(&actions);
        match &core[..] {
            [SlackAction::StreamTaskUpdate {
                title: title1,
                status: status1,
                task_id: task1,
                ..
            }, SlackAction::StreamTaskUpdate {
                title: title2,
                status: status2,
                task_id: task2,
                ..
            }, SlackAction::StreamAppend { text, .. }, SlackAction::StreamStop { .. }] => {
                assert_eq!(title1, "grep");
                assert_eq!(status1, "in_progress");
                assert_eq!(title2, "read_file");
                assert_eq!(status2, "in_progress");
                assert_ne!(task1, task2, "tool tasks must have distinct ids");
                assert_eq!(text, "Done!");
            }
            other => panic!(
                "expected [task, task, append, stop] — the intermediate \
                 ResponseDone must not stop the stream — got {other:?}"
            ),
        }

        let statuses = status_texts(&actions);
        assert!(statuses.contains(&"is running grep"), "got {statuses:?}");
        assert!(
            statuses.contains(&"is running read_file"),
            "got {statuses:?}"
        );
    });
}

#[test]
fn daemon_text_chunk_produces_stream_append() {
    let mut flow = FlowBuilder::new();
    let (slack_tx, daemon_tx, actions_out, commands_out) = build_sim(&mut flow);

    flow.sim().unit_test_fuzz_iterations(256).fuzz(async || {
        let thread_ts = fresh("resp");
        let channel = fresh("C");

        slack_tx.send(chat("U1", "hello", &channel, &thread_ts));
        assert!(matches!(
            commands_out.next().await.unwrap(),
            DaemonCommand::CreateSession { .. }
        ));

        daemon_tx.send_many([
            devent(&thread_ts, text_chunk("Hello world")),
            devent(&thread_ts, response_done()),
        ]);

        let actions: Vec<SlackAction> = actions_out.collect().await;

        let core = non_status(&actions);
        match &core[..] {
            [SlackAction::StreamAppend {
                channel: c1,
                thread_ts: t1,
                text,
            }, SlackAction::StreamStop {
                channel: c2,
                thread_ts: t2,
            }] => {
                assert_eq!(c1, &channel);
                assert_eq!(t1, &thread_ts);
                assert_eq!(text, "Hello world");
                assert_eq!(c2, &channel);
                assert_eq!(t2, &thread_ts);
            }
            other => panic!("expected [append, stop], got {other:?}"),
        }

        // Every status on this thread is "is thinking" (from the user message
        // and the text chunk), routed to the mapped channel.
        for action in &actions {
            if let SlackAction::SetStatus {
                channel: c, status, ..
            } = action
            {
                assert_eq!(c, &channel);
                assert_eq!(status, "is thinking");
            }
        }
    });
}
