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
        is_session_stopped: false,
        streaming_message_ts: Vec::new(),
        shortcut: None,
        rewind_user_turn: None,
        fork_source_thread_ts: None,
    }
}

fn devent(thread_ts: &str, message: infinity_protocol::DaemonMessage) -> DaemonEvent {
    DaemonEvent {
        thread_ts: thread_ts.to_owned(),
        message,
    }
}

fn text_chunk(chunk: &str) -> infinity_protocol::DaemonMessage {
    text_chunk_in(None, chunk)
}

fn text_chunk_in(thread_id: Option<&str>, chunk: &str) -> infinity_protocol::DaemonMessage {
    infinity_protocol::DaemonMessage::TextChunk {
        thread_id: thread_id.map(Into::into),
        chunk: chunk.to_owned(),
    }
}

fn tool_call(name: &str) -> infinity_protocol::DaemonMessage {
    tool_call_in(None, name)
}

fn tool_call_in(thread_id: Option<&str>, name: &str) -> infinity_protocol::DaemonMessage {
    infinity_protocol::DaemonMessage::ToolCall {
        name: name.to_owned(),
        args: "{}".to_owned(),
        thread_id: thread_id.map(Into::into),
        display_as: None,
    }
}

fn response_done() -> infinity_protocol::DaemonMessage {
    response_done_in(None)
}

fn response_done_in(thread_id: Option<&str>) -> infinity_protocol::DaemonMessage {
    infinity_protocol::DaemonMessage::ResponseDone {
        thread_id: thread_id.map(Into::into),
        token_usage: None,
    }
}

fn tool_result() -> infinity_protocol::DaemonMessage {
    infinity_protocol::DaemonMessage::ToolResult {
        // The dataflow ignores the segments; completion is paired by queue order.
        segments: Vec::new(),
        thread_id: None,
    }
}

fn error_msg(text: &str) -> infinity_protocol::DaemonMessage {
    infinity_protocol::DaemonMessage::Error {
        thread_id: None,
        text: text.to_owned(),
    }
}

fn choice_required(id: &str, prompt: &str) -> infinity_protocol::DaemonMessage {
    infinity_protocol::DaemonMessage::UserChoiceRequired {
        thread_id: None,
        id: id.into(),
        prompt: prompt.to_owned(),
        choices: vec!["Yes".to_owned(), "No".to_owned()],
        default: 0,
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
                root_thread_id: session_id.clone().into(),
                thread_id: infinity_protocol::ThreadRef::from(""),
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
                target_thread,
                text,
            } => {
                assert_eq!(t, thread_ts);
                assert_eq!(s.to_string(), session_id);
                assert!(target_thread.is_none(), "root replies target the root");
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
                ..
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

/// A tool entry whose ToolResult never arrives (e.g. an interrupted tool)
/// must be discarded when the response's stream stops: ToolResult pairs
/// completions to calls by FIFO order only, so a stale entry would otherwise
/// mislabel the next response's first completion with the OLD tool's
/// title/task id (leaving the new tool's card stuck "in progress").
#[test]
fn stale_tool_tasks_discarded_at_response_end() {
    let mut flow = FlowBuilder::new();
    let (slack_tx, daemon_tx, actions_out, commands_out) = build_sim(&mut flow);

    flow.sim().unit_test_fuzz_iterations(256).fuzz(async || {
        let thread_ts = fresh("stale");
        let channel = fresh("C");

        slack_tx.send(chat("U1", "go", &channel, &thread_ts));
        assert!(matches!(
            commands_out.next().await.unwrap(),
            DaemonCommand::CreateSession { .. }
        ));

        daemon_tx.send_many([
            // Response 1: a tool is called but its result never arrives
            // (interrupted). The second ResponseDone ends the response.
            devent(&thread_ts, tool_call("orphaned_tool")),
            devent(&thread_ts, response_done()), // had_tool → stream stays open
            devent(&thread_ts, response_done()), // no tool this round → stop + discard
            // Response 2: a fresh tool call that completes normally.
            devent(&thread_ts, tool_call("fresh_tool")),
            devent(&thread_ts, tool_result()),
            devent(&thread_ts, response_done()), // had_tool → keep open
            devent(&thread_ts, response_done()), // final stop
        ]);

        let actions: Vec<SlackAction> = actions_out.collect().await;

        let core = non_status(&actions);
        match &core[..] {
            [SlackAction::StreamTaskUpdate {
                title: t1,
                status: s1,
                task_id: id1,
                ..
            }, SlackAction::StreamStop { .. }, SlackAction::StreamTaskUpdate {
                title: t2,
                status: s2,
                task_id: id2,
                ..
            }, SlackAction::StreamTaskUpdate {
                title: t3,
                status: s3,
                task_id: id3,
                ..
            }, SlackAction::StreamStop { .. }] => {
                assert_eq!((t1.as_str(), s1.as_str()), ("orphaned_tool", "in_progress"));
                assert_eq!((t2.as_str(), s2.as_str()), ("fresh_tool", "in_progress"));
                // The completion must pair with the FRESH tool, not the stale
                // entry left behind by the orphaned one.
                assert_eq!(
                    (t3.as_str(), s3.as_str()),
                    ("fresh_tool", "complete"),
                    "completion mislabeled — stale task entry was not discarded"
                );
                assert_eq!(id2, id3, "completion must reuse the fresh card's task id");
                assert_ne!(id1, id3, "completion must not reuse the orphaned card's id");
            }
            other => panic!(
                "expected [task(orphaned), stop, task(fresh), task(fresh complete), stop], \
                 got {other:?}"
            ),
        }
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

/// Output from a subagent thread (a `thread_id` different from the session
/// root) is tagged with the subagent id — so the I/O layer routes it into a
/// dedicated Slack thread — and its tool/stream state is tracked separately
/// from the root stream.
/// A native stop-button click (`agent_session_stopped`) halts the thread's
/// stream and posts a confirmation. With no session mapped to the thread
/// (e.g. a stop from before a bot restart) it produces NO daemon command —
/// there is nothing to interrupt, and it must not create a session.
#[test]
fn session_stop_halts_stream_and_confirms() {
    let mut flow = FlowBuilder::new();
    let (slack_tx, _daemon_tx, actions_out, commands_out) = build_sim(&mut flow);

    flow.sim().exhaustive(async || {
        let thread_ts = fresh("stop");
        let channel = fresh("C");

        let mut stop = chat("U1", "", &channel, &thread_ts);
        stop.is_session_stopped = true;
        stop.streaming_message_ts = vec!["111.222".to_owned()];
        slack_tx.send(stop);

        let cmds: Vec<DaemonCommand> = commands_out.collect().await;
        assert!(
            cmds.is_empty(),
            "stop must not produce daemon commands, got {cmds:?}"
        );

        let actions: Vec<SlackAction> = actions_out.collect().await;
        match &actions[..] {
            [SlackAction::HaltStreams {
                channel: c1,
                thread_ts: t1,
                subagent,
                stopped_message_ts,
            }, SlackAction::PostMessage {
                thread_ts: t2,
                subagent: s2,
                text,
                ..
            }] => {
                assert_eq!(c1, &channel);
                assert_eq!(t1, &thread_ts);
                assert!(subagent.is_none(), "root-thread stop has no subagent");
                assert_eq!(stopped_message_ts, &vec!["111.222".to_owned()]);
                assert_eq!(t2.as_deref(), Some(thread_ts.as_str()));
                assert!(s2.is_none());
                assert!(text.contains("Stopped"), "confirmation text: {text}");
            }
            other => panic!("expected [HaltStreams, PostMessage], got {other:?}"),
        }
    });
}

/// A native stop-button click on a thread with a registered session emits an
/// `Interrupt` daemon command targeting the session root, alongside the
/// Slack-side halt + confirmation.
#[test]
fn session_stop_interrupts_daemon_generation() {
    let mut flow = FlowBuilder::new();
    let (slack_tx, daemon_tx, actions_out, commands_out) = build_sim(&mut flow);

    flow.sim().unit_test_fuzz_iterations(256).fuzz(async || {
        let thread_ts = fresh("stopint");
        let channel = fresh("C");
        let session_id = fresh("sess");

        slack_tx.send(chat("U1", "work", &channel, &thread_ts));
        assert!(matches!(
            commands_out.next().await.unwrap(),
            DaemonCommand::CreateSession { .. }
        ));

        // Register the session (the SetThreadTitle action is the
        // synchronization point proving the registration was processed).
        daemon_tx.send(devent(
            &thread_ts,
            infinity_protocol::DaemonMessage::Connected {
                root_thread_id: session_id.clone().into(),
                thread_id: session_id.clone().into(),
                model_name: String::new(),
                context_window: 0,
                title: Some("Root".to_owned()),
                total_tokens_used: 0,
                provider_id: String::new(),
            },
        ));
        loop {
            match actions_out.next().await.unwrap() {
                SlackAction::SetThreadTitle { .. } => break,
                SlackAction::SetStatus { .. } => {}
                other => panic!("expected SetThreadTitle, got {other:?}"),
            }
        }

        let mut stop = chat("U1", "", &channel, &thread_ts);
        stop.is_session_stopped = true;
        stop.streaming_message_ts = vec!["111.222".to_owned()];
        slack_tx.send(stop);

        match commands_out.next().await.unwrap() {
            DaemonCommand::Interrupt {
                thread_ts: t,
                session_id: s,
                target_thread,
            } => {
                assert_eq!(t, thread_ts);
                assert_eq!(s.to_string(), session_id);
                assert!(
                    target_thread.is_none(),
                    "a root-thread stop interrupts the session root"
                );
            }
            other => panic!("expected Interrupt, got {other:?}"),
        }

        // The Slack-side halt + confirmation still happen.
        let actions: Vec<SlackAction> = actions_out.collect().await;
        let core = non_status(&actions);
        match &core[..] {
            [SlackAction::HaltStreams { thread_ts: t1, .. }, SlackAction::PostMessage { text, .. }] =>
            {
                assert_eq!(t1, &thread_ts);
                assert!(text.contains("interrupted"), "confirmation text: {text}");
            }
            other => panic!("expected [HaltStreams, PostMessage], got {other:?}"),
        }
    });
}

#[test]
fn subagent_output_is_tagged_and_tracked_separately() {
    let mut flow = FlowBuilder::new();
    let (slack_tx, daemon_tx, actions_out, commands_out) = build_sim(&mut flow);

    flow.sim().unit_test_fuzz_iterations(256).fuzz(async || {
        let thread_ts = fresh("subagent");
        let channel = fresh("C");
        let session_id = fresh("sess");
        let subagent_id = fresh("agent");

        slack_tx.send(chat("U1", "go", &channel, &thread_ts));
        assert!(matches!(
            commands_out.next().await.unwrap(),
            DaemonCommand::CreateSession { .. }
        ));

        // Register the session root (subagent detection compares against it);
        // the SetThreadTitle action is the synchronization point.
        daemon_tx.send(devent(
            &thread_ts,
            infinity_protocol::DaemonMessage::Connected {
                root_thread_id: session_id.clone().into(),
                thread_id: session_id.clone().into(),
                model_name: String::new(),
                context_window: 0,
                title: Some("Root".to_owned()),
                total_tokens_used: 0,
                provider_id: String::new(),
            },
        ));
        loop {
            match actions_out.next().await.unwrap() {
                SlackAction::SetThreadTitle { .. } => break,
                SlackAction::SetStatus { .. } => {}
                other => panic!("expected SetThreadTitle, got {other:?}"),
            }
        }

        daemon_tx.send_many([
            // Root output (thread_id == session root → not a subagent).
            devent(&thread_ts, text_chunk_in(Some(&session_id), "root text")),
            // Subagent output and tool call.
            devent(&thread_ts, text_chunk_in(Some(&subagent_id), "sub text")),
            devent(&thread_ts, tool_call_in(Some(&subagent_id), "bash")),
            // The subagent's tool call must keep *its* stream open...
            devent(&thread_ts, response_done_in(Some(&subagent_id))),
            // ...while the root (which ran no tool) stops independently.
            devent(&thread_ts, response_done_in(Some(&session_id))),
            // The subagent finishes afterwards.
            devent(&thread_ts, text_chunk_in(Some(&subagent_id), "sub done")),
            devent(&thread_ts, response_done_in(Some(&subagent_id))),
        ]);

        let actions: Vec<SlackAction> = actions_out.collect().await;

        let core = non_status(&actions);
        match &core[..] {
            [SlackAction::StreamAppend {
                text: t1,
                subagent: s1,
                ..
            }, SlackAction::StreamAppend {
                text: t2,
                subagent: s2,
                ..
            }, SlackAction::StreamTaskUpdate {
                title,
                subagent: s3,
                ..
            }, SlackAction::StreamStop {
                subagent: s4,
                thread_ts: stop_ts,
                ..
            }, SlackAction::StreamAppend {
                text: t3,
                subagent: s5,
                ..
            }, SlackAction::StreamStop { subagent: s6, .. }] => {
                assert_eq!(t1, "root text");
                assert!(s1.is_none(), "root output must not be subagent-tagged");
                assert_eq!(t2, "sub text");
                assert_eq!(s2.as_deref(), Some(subagent_id.as_str()));
                assert_eq!(title, "bash");
                assert_eq!(s3.as_deref(), Some(subagent_id.as_str()));
                // The first stop is the root's: the subagent's ResponseDone is
                // swallowed because its tool call is still in flight.
                assert!(s4.is_none(), "first stop must be the root stream");
                assert_eq!(stop_ts, &thread_ts);
                assert_eq!(t3, "sub done");
                assert_eq!(s5.as_deref(), Some(subagent_id.as_str()));
                assert_eq!(s6.as_deref(), Some(subagent_id.as_str()));
            }
            other => panic!(
                "expected [append(root), append(sub), task(sub), stop(root), \
                 append(sub), stop(sub)], got {other:?}"
            ),
        }

        // Subagent activity must not drive the root thread status: the only
        // statuses are "is thinking" (user message + root text chunk).
        for status in status_texts(&actions) {
            assert_eq!(status, "is thinking");
        }
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
                subagent,
            }, SlackAction::StreamStop {
                channel: c2,
                thread_ts: t2,
                subagent: stop_subagent,
            }] => {
                assert_eq!(c1, &channel);
                assert_eq!(t1, &thread_ts);
                assert_eq!(text, "Hello world");
                assert!(
                    subagent.is_none(),
                    "root output must not be subagent-tagged"
                );
                assert_eq!(c2, &channel);
                assert_eq!(t2, &thread_ts);
                assert!(stop_subagent.is_none());
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

/// A discrete block posted into a thread must never leave the stream above it
/// active: an `Error` message finalizes the in-flight stream *before* the
/// error post, so continued generation starts a fresh message below it
/// (instead of editing the message above the block — discontinuity).
#[test]
fn error_stops_stream_before_posting() {
    let mut flow = FlowBuilder::new();
    let (slack_tx, daemon_tx, actions_out, commands_out) = build_sim(&mut flow);

    flow.sim().unit_test_fuzz_iterations(256).fuzz(async || {
        let thread_ts = fresh("err");
        let channel = fresh("C");

        slack_tx.send(chat("U1", "do it", &channel, &thread_ts));
        assert!(matches!(
            commands_out.next().await.unwrap(),
            DaemonCommand::CreateSession { .. }
        ));

        daemon_tx.send_many([
            devent(&thread_ts, text_chunk("working...")),
            devent(&thread_ts, error_msg("boom")),
            devent(&thread_ts, text_chunk("recovered")),
            devent(&thread_ts, response_done()),
        ]);

        let actions: Vec<SlackAction> = actions_out.collect().await;

        let core = non_status(&actions);
        match &core[..] {
            [SlackAction::StreamAppend { text: t1, .. }, SlackAction::StreamStop {
                thread_ts: stop_ts, ..
            }, SlackAction::PostMessage {
                text: err_text,
                thread_ts: post_ts,
                ..
            }, SlackAction::StreamAppend { text: t2, .. }, SlackAction::StreamStop { .. }] => {
                assert_eq!(t1, "working...");
                assert_eq!(stop_ts, &thread_ts);
                assert_eq!(err_text, "⚠️ boom");
                assert_eq!(post_ts.as_deref(), Some(thread_ts.as_str()));
                assert_eq!(t2, "recovered");
            }
            other => panic!(
                "expected [append, stop, post(error), append, stop] — the \
                 stream must be finalized before the error post — got {other:?}"
            ),
        }
    });
}

/// Choice buttons likewise finalize the in-flight stream before being posted,
/// so post-choice output starts a fresh message below the buttons.
#[test]
fn choice_buttons_stop_stream_before_posting() {
    let mut flow = FlowBuilder::new();
    let (slack_tx, daemon_tx, actions_out, commands_out) = build_sim(&mut flow);

    flow.sim().unit_test_fuzz_iterations(256).fuzz(async || {
        let thread_ts = fresh("choice");
        let channel = fresh("C");
        let choice_id = fresh("ch");

        slack_tx.send(chat("U1", "risky op", &channel, &thread_ts));
        assert!(matches!(
            commands_out.next().await.unwrap(),
            DaemonCommand::CreateSession { .. }
        ));

        daemon_tx.send_many([
            devent(&thread_ts, text_chunk("about to ask...")),
            devent(&thread_ts, choice_required(&choice_id, "Allow?")),
            devent(&thread_ts, text_chunk("after choice")),
            devent(&thread_ts, response_done()),
        ]);

        let actions: Vec<SlackAction> = actions_out.collect().await;

        let core = non_status(&actions);
        match &core[..] {
            [SlackAction::StreamAppend { text: t1, .. }, SlackAction::StreamStop {
                thread_ts: stop_ts, ..
            }, SlackAction::PostBlocks {
                fallback_text,
                choice_id: posted_choice,
                ..
            }, SlackAction::StreamAppend { text: t2, .. }, SlackAction::StreamStop { .. }] => {
                assert_eq!(t1, "about to ask...");
                assert_eq!(stop_ts, &thread_ts);
                assert_eq!(fallback_text, "Allow?");
                assert_eq!(
                    posted_choice.as_ref().map(|c| c.as_str()),
                    Some(choice_id.as_str())
                );
                assert_eq!(t2, "after choice");
            }
            other => panic!(
                "expected [append, stop, blocks(choice), append, stop] — the \
                 stream must be finalized before the buttons — got {other:?}"
            ),
        }
    });
}

/// A "Rewind to here" shortcut on a thread with no registered session is
/// answered ephemerally (via the shortcut's response_url) and produces no
/// daemon command and no modal.
#[test]
fn rewind_shortcut_without_session_reports_error() {
    let mut flow = FlowBuilder::new();
    let (slack_tx, _daemon_tx, actions_out, commands_out) = build_sim(&mut flow);

    flow.sim().exhaustive(async || {
        let thread_ts = fresh("rwnone");
        let channel = fresh("C");

        let mut shortcut = chat("U1", "", &channel, &thread_ts);
        shortcut.shortcut = Some("rewind_to_here".to_owned());
        shortcut.message_ts = Some("1111.2222".to_owned());
        shortcut.trigger_id = Some("trig".to_owned());
        shortcut.response_url = Some("https://hooks.slack.com/app/T1/1/x".to_owned());
        slack_tx.send(shortcut);

        let cmds: Vec<DaemonCommand> = commands_out.collect().await;
        assert!(cmds.is_empty(), "no session → no commands, got {cmds:?}");

        let actions: Vec<SlackAction> = actions_out.collect().await;
        match &actions[..] {
            [SlackAction::CommandResponse { response_url, text }] => {
                assert_eq!(response_url, "https://hooks.slack.com/app/T1/1/x");
                assert!(
                    text.contains("no Infinity session"),
                    "error text should explain, got: {text}"
                );
            }
            other => panic!("expected [CommandResponse], got {other:?}"),
        }
    });
}

/// "Fork from here" is non-destructive. The shortcut opens the edit modal;
/// its submission asks the I/O layer to open a NEW Slack thread
/// (`ForkThread`) and touches neither the original thread nor its session.
/// The fork-ready event for the new thread emits `ForkSession` for the
/// source session, the fork's `Connected` maps the new thread to the forked
/// session, and the original thread keeps talking to the original session.
#[test]
fn fork_submission_opens_new_thread_and_keeps_original() {
    let mut flow = FlowBuilder::new();
    let (slack_tx, daemon_tx, actions_out, commands_out) = build_sim(&mut flow);

    flow.sim().unit_test_fuzz_iterations(256).fuzz(async || {
        let thread_ts = fresh("fkdo");
        let fork_ts = fresh("fknew");
        let channel = fresh("C");
        let session_id = fresh("sess");
        let fork_session = fresh("fsess");

        let connected = |sid: &str, title: &str| infinity_protocol::DaemonMessage::Connected {
            root_thread_id: sid.to_owned().into(),
            thread_id: sid.to_owned().into(),
            model_name: String::new(),
            context_window: 0,
            title: Some(title.to_owned()),
            total_tokens_used: 0,
            provider_id: String::new(),
        };
        // The SetThreadTitle action is the synchronization point proving a
        // `Connected` registration was processed.
        let wait_title = async || loop {
            match actions_out.next().await.unwrap() {
                SlackAction::SetThreadTitle { thread_ts, .. } => return thread_ts,
                SlackAction::SetStatus { .. } => {}
                other => panic!("expected SetThreadTitle, got {other:?}"),
            }
        };

        slack_tx.send(chat("U1", "original ask", &channel, &thread_ts));
        assert!(matches!(
            commands_out.next().await.unwrap(),
            DaemonCommand::CreateSession { .. }
        ));
        daemon_tx.send(devent(&thread_ts, connected(&session_id, "Root")));
        assert_eq!(wait_title().await, thread_ts);

        // The shortcut itself only opens the modal (turn resolution happens
        // in the I/O layer, which needs the Slack thread).
        let mut shortcut = chat("U1", "", &channel, &thread_ts);
        shortcut.shortcut = Some("rewind_to_here".to_owned());
        shortcut.message_ts = Some("1111.2222".to_owned());
        shortcut.trigger_id = Some("trig".to_owned());
        shortcut.response_url = Some("https://hooks.slack.com/app/T1/1/x".to_owned());
        slack_tx.send(shortcut);
        loop {
            match actions_out.next().await.unwrap() {
                SlackAction::OpenRewindModal {
                    thread_ts: t,
                    target_ts,
                    ..
                } => {
                    assert_eq!(t, thread_ts);
                    assert_eq!(target_ts, "1111.2222");
                    break;
                }
                SlackAction::SetStatus { .. } => {}
                other => panic!("expected OpenRewindModal, got {other:?}"),
            }
        }

        // Submission: fork from user turn 0 with edited text. Only a
        // ForkThread action — nothing is stopped or posted in the original.
        let mut submit = chat("U1", "edited ask", &channel, &thread_ts);
        submit.rewind_user_turn = Some(0);
        submit.message_ts = Some("1111.2222".to_owned());
        slack_tx.send(submit);
        loop {
            match actions_out.next().await.unwrap() {
                SlackAction::ForkThread {
                    channel: c,
                    source_thread_ts,
                    source_message_ts,
                    user_turn,
                    text,
                } => {
                    assert_eq!(c, channel);
                    assert_eq!(source_thread_ts, thread_ts);
                    assert_eq!(source_message_ts, "1111.2222");
                    assert_eq!(user_turn, 0);
                    assert_eq!(text, "edited ask");
                    break;
                }
                SlackAction::SetStatus { .. } => {}
                other => panic!("expected ForkThread, got {other:?}"),
            }
        }

        // The I/O layer reports the fork's new thread: ForkSession is issued
        // for the SOURCE session, keyed on the new thread.
        let mut ready = chat("", "edited ask", &channel, &fork_ts);
        ready.rewind_user_turn = Some(0);
        ready.fork_source_thread_ts = Some(thread_ts.clone());
        slack_tx.send(ready);
        match commands_out.next().await.unwrap() {
            DaemonCommand::ForkSession {
                thread_ts: t,
                session_id: s,
                user_turn,
            } => {
                assert_eq!(t, fork_ts);
                assert_eq!(s.to_string(), session_id);
                assert_eq!(user_turn, 0);
            }
            other => panic!("expected ForkSession, got {other:?}"),
        }

        // The fork's Connected registers the new thread → forked session.
        daemon_tx.send(devent(&fork_ts, connected(&fork_session, "Fork")));
        assert_eq!(wait_title().await, fork_ts);

        // A reply in the new thread goes to the fork…
        slack_tx.send(chat("U1", "in the fork", &channel, &fork_ts));
        match commands_out.next().await.unwrap() {
            DaemonCommand::SendInput {
                thread_ts: t,
                session_id: s,
                ..
            } => {
                assert_eq!(t, fork_ts);
                assert_eq!(s.to_string(), fork_session);
            }
            other => panic!("expected SendInput to the fork, got {other:?}"),
        }
        // …and the original thread still talks to the original session.
        slack_tx.send(chat("U1", "still original", &channel, &thread_ts));
        match commands_out.next().await.unwrap() {
            DaemonCommand::SendInput {
                thread_ts: t,
                session_id: s,
                ..
            } => {
                assert_eq!(t, thread_ts);
                assert_eq!(s.to_string(), session_id);
            }
            other => panic!("expected SendInput to the original, got {other:?}"),
        }
    });
}
