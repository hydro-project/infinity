//! Driver and router behavioral tests for interruption, deferral, idling,
//! routing, replay, and auto-compaction.

use async_trait::async_trait;
use infinity_provider_protocol::StreamChunk;
use infinity_provider_protocol::message::UserContent;
use rap_protocol::ThreadId;
use tokio::sync::mpsc;

use super::events::AgentEvent;
use super::local::{ChannelSender, ThreadLifecycleEvent, ThreadLifecycleState};
use super::test_support::*;
use crate::message::{
    InputMessage, InputMessageContent, SyntheticKind, TaggedSyntheticKind, UserChoiceRequired,
};
use crate::tools::{Tool, ToolContext};

#[tokio::test(flavor = "current_thread")]
async fn pending_choices_are_thread_local_and_replayed() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            use super::{AgentSystemBuilder, EventCollector, NoDeferral};
            use crate::stores::{InMemoryConversationStore, InMemoryStateStore};
            use crate::traits::{ConversationStore, StateStore};

            let (model, _ctrl) = model_source(None);
            let conv = InMemoryConversationStore::new();
            conv.ensure_root_thread(ThreadId::from_ref("t1"))
                .await
                .expect("ensure root");
            conv.ensure_root_thread(ThreadId::from_ref("t2"))
                .await
                .expect("ensure root");
            let state = InMemoryStateStore::new();
            let sender = ChannelSender::new_pair().0;
            let mut system = AgentSystemBuilder::new(conv, state.clone(), model, sender).build();
            let choice_input = |thread_id: &str, choice_id: &str| {
                (
                    InputMessage {
                        content: InputMessageContent::UserChoice(UserChoiceRequired {
                            content_type: "user_choice_required".to_owned(),
                            id: choice_id.to_owned(),
                            call_id: None,
                            prompt: "Pick one".to_owned(),
                            choices: vec!["A".to_owned(), "B".to_owned()],
                            default: 0,
                            response_url: "http://example.test/choice".to_owned(),
                        }),
                        group_id: thread_id.into(),
                        metadata: None,
                        synthetic: None,
                        display_as: None,
                        subscription: false,
                    },
                    format!("message-{choice_id}"),
                )
            };
            let collector = EventCollector::new();
            system
                .step(
                    vec![
                        choice_input("t1", "choice-1"),
                        choice_input("t2", "choice-2"),
                    ],
                    &collector,
                    &mut NoDeferral,
                )
                .await
                .expect("persist choices");

            assert_eq!(
                state
                    .get_pending_user_choices(ThreadId::from_ref("t1"))
                    .await
                    .expect("load first thread choices")[0]
                    .id,
                "choice-1"
            );
            assert_eq!(
                state
                    .get_pending_user_choices(ThreadId::from_ref("t2"))
                    .await
                    .expect("load second thread choices")[0]
                    .id,
                "choice-2"
            );

            let (sub_tx, mut sub_rx) = mpsc::unbounded_channel();
            let replay_conv = InMemoryConversationStore::new();
            replay_conv
                .ensure_root_thread(ThreadId::from_ref("t1"))
                .await
                .expect("ensure replay root");
            let running = AgentSystemBuilder::new_local(replay_conv, state, model_source(None).0)
                .start_with_observer(|_| TestObserver {
                    tx: mpsc::unbounded_channel().0,
                });
            running.subscribe(ThreadId::from_ref("t1"), sub_tx).await;
            let Evt::Replay(snapshot) = next_evt(&mut sub_rx).await else {
                panic!("expected replay");
            };
            assert_eq!(snapshot.pending_choices.len(), 1);
            assert_eq!(snapshot.pending_choices[0].id, "choice-1");
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn driver_idles_after_text_response() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (mut running, mut rx, mut ctrl, _conv) = start_system(vec![], None);
            running
                .send_user_text(ThreadId::from_ref("t1"), "hello")
                .await;
            let _req = ctrl.next_request().await;
            ctrl.send_text("hi there");
            ctrl.finish();
            let texts = collect_until_finished(&mut rx).await;
            assert_eq!(texts, vec!["hi there"]);
            wait_idle(&mut running).await;
            assert!(running.is_idle());
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn driver_stays_alive_waiting_for_tool_result() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (mut running, mut rx, mut ctrl, _conv) =
                start_system(vec![Box::new(AsyncTool)], None);
            running
                .send_user_text(ThreadId::from_ref("t1"), "use tool")
                .await;
            let _req = ctrl.next_request().await;
            ctrl.send_tool_call("tc-1", "async_tool", serde_json::json!({}));
            ctrl.finish();
            collect_until_finished(&mut rx).await;

            assert!(
                !matches!(
                    running.try_next_lifecycle_event(),
                    Ok(ThreadLifecycleEvent {
                        state: ThreadLifecycleState::Idle,
                        ..
                    })
                ),
                "should not idle while a tool call is pending"
            );
            assert!(!running.is_idle());

            // A client attaching while waiting gets a replay whose history
            // ends with the unresolved tool call and no completion in flight.
            let (sub_tx, mut sub_rx) = mpsc::unbounded_channel();
            running.subscribe(ThreadId::from_ref("t1"), sub_tx).await;
            match next_evt(&mut sub_rx).await {
                Evt::Replay(snapshot) => {
                    assert!(!snapshot.in_progress);
                    assert!(matches!(
                        snapshot.history.last(),
                        Some(crate::message::InfinityMessage::ToolCall { call, .. })
                            if call.function.name == "async_tool"
                    ));
                }
                other => panic!("expected replay, got {other:?}"),
            }

            // Deliver the result; the driver wakes and finishes.
            running
                .send(tool_result_input("t1", "tc-1", "tool done").0, "res-1")
                .await;
            let _req2 = ctrl.next_request().await;
            ctrl.send_text("ok");
            ctrl.finish();
            collect_until_finished(&mut rx).await;
            wait_idle(&mut running).await;
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn user_text_interrupts_active_completion() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (running, mut rx, mut ctrl, _conv) = start_system(vec![], None);
            running
                .send_user_text(ThreadId::from_ref("t1"), "first")
                .await;
            let _req = ctrl.next_request().await;
            ctrl.send_text("partial...");
            // Wait until the chunk is observed so the completion is in flight.
            loop {
                if let Evt::E(AgentEvent::TextChunk { .. }) = next_evt(&mut rx).await {
                    break;
                }
            }
            running
                .send_user_text(ThreadId::from_ref("t1"), "stop that")
                .await;
            let req2 = ctrl.next_request().await;
            let has_interrupt = req2.chat_history.into_iter().any(|m| {
                if let infinity_provider_protocol::message::Message::User { content } = &m
                    && let Some(UserContent::Text(t)) = content.first()
                {
                    return t.text.contains("<interrupt>");
                }
                false
            });
            assert!(has_interrupt, "interrupting input should carry the marker");
            ctrl.send_text("ok stopped");
            ctrl.finish();
            collect_until_finished(&mut rx).await;
        })
        .await;
}

/// A non-user-text input (e.g. a stale tool result) arriving while a
/// completion is streaming must not interrupt it.
#[tokio::test(flavor = "current_thread")]
async fn non_user_text_during_completion_does_not_interrupt() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (running, mut rx, mut ctrl, _conv) = start_system(vec![Box::new(AsyncTool)], None);
            running
                .send_user_text(ThreadId::from_ref("t1"), "do stuff")
                .await;
            let _req = ctrl.next_request().await;
            ctrl.send_tool_call("tc-1", "async_tool", serde_json::json!({}));
            ctrl.finish();
            collect_until_finished(&mut rx).await;

            running
                .send(tool_result_input("t1", "tc-1", "tool output").0, "res-1")
                .await;
            let _req2 = ctrl.next_request().await;
            ctrl.send_text("processing...");
            loop {
                if let Evt::E(AgentEvent::TextChunk { .. }) = next_evt(&mut rx).await {
                    break;
                }
            }
            // A stale tool result arrives mid-stream — must not interrupt.
            running
                .send(
                    tool_result_input("t1", "tc-other", "stale event").0,
                    "res-stale",
                )
                .await;
            ctrl.send_text(" done");
            ctrl.finish();
            let texts = collect_until_finished(&mut rx).await;
            assert!(
                texts.join("").contains("done"),
                "should not have been interrupted"
            );
        })
        .await;
}

/// An associative subscription event arriving while a completion is streaming
/// is queued and processed in the next round rather than interrupting it.
#[tokio::test(flavor = "current_thread")]
async fn subscription_event_queued_during_completion_processed_after() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (running, mut rx, mut ctrl, _conv) =
                start_system(vec![Box::new(SubscribeTool)], None);
            running
                .send_user_text(ThreadId::from_ref("t1"), "subscribe")
                .await;
            let _req = ctrl.next_request().await;
            ctrl.send_tool_call("tc-sub", "subscribe_tool", serde_json::json!({}));
            ctrl.finish();
            collect_until_finished(&mut rx).await;

            // The subscription result triggers the next round; leave the model
            // mid-stream while the event arrives.
            let _req2 = ctrl.next_request().await;
            ctrl.send_text("got subscription...");
            loop {
                if let Evt::E(AgentEvent::TextChunk { .. }) = next_evt(&mut rx).await {
                    break;
                }
            }
            running
                .send(
                    subscription_event_input("t1", "tc-sub", "event payload xyz").0,
                    "ev-1",
                )
                .await;
            ctrl.send_text(" all good");
            ctrl.finish();
            let texts = collect_until_finished(&mut rx).await;
            assert!(
                texts.join("").contains("all good"),
                "should not have been interrupted"
            );

            // The queued event appears in the next round.
            let req3 = ctrl.next_request().await;
            assert!(
                tool_result_texts(&req3)
                    .iter()
                    .any(|t| t.contains("event payload xyz")),
                "queued event should appear in next round"
            );
            ctrl.send_text("processed");
            ctrl.finish();
            collect_until_finished(&mut rx).await;
        })
        .await;
}
/// An unanswered `close_thread` call never receives a result, so it must not
/// keep the driver resident.
#[tokio::test(flavor = "current_thread")]
async fn driver_idles_after_close_thread_tool_call() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            /// Stub close_thread (the built-in one refuses to close a root
            /// thread); registered with built-ins disabled so it is the only
            /// tool by that name.
            struct CloseThreadStub;
            #[async_trait]
            impl Tool<ChannelSender> for CloseThreadStub {
                fn name(&self) -> &str {
                    "close_thread"
                }
                fn description(&self) -> &str {
                    "close"
                }
                fn parameters(&self) -> serde_json::Value {
                    serde_json::json!({"type":"object","properties":{}})
                }
                async fn execute(
                    &self,
                    _: serde_json::Value,
                    _: String,
                    _: Option<String>,
                    _: &ToolContext<ChannelSender>,
                ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
                    Ok(())
                }
            }
            let (mut running, mut rx, mut ctrl, _conv) =
                start_system_with(vec![Box::new(CloseThreadStub)], None, false);
            running
                .send_user_text(ThreadId::from_ref("t1"), "close")
                .await;
            let _req = ctrl.next_request().await;
            ctrl.send_tool_call(
                "tc-1",
                "close_thread",
                serde_json::json!({"thread_id": "t1"}),
            );
            ctrl.finish();
            collect_until_finished(&mut rx).await;
            wait_idle(&mut running).await;
            assert!(running.is_idle());
        })
        .await;
}

/// Thread reports arriving while waiting for a non-passive async tool result
/// are deferred until the tool result is processed.
#[tokio::test(flavor = "current_thread")]
async fn thread_report_deferred_during_async_tool_wait() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (running, mut rx, mut ctrl, _conv) = start_system(vec![Box::new(AsyncTool)], None);

            // 1. User sends input, model calls async_tool.
            running
                .send_user_text(ThreadId::from_ref("t1"), "do async")
                .await;
            let _req = ctrl.next_request().await;
            ctrl.send_tool_call("tc-async", "async_tool", serde_json::json!({}));
            ctrl.finish();
            collect_until_finished(&mut rx).await;

            // 2. While waiting for the tool result, a thread report arrives.
            running
                .send(
                    InputMessage {
                        content: InputMessageContent::User(UserContent::ToolResult(
                            infinity_provider_protocol::message::ToolResult {
                                id: String::new(),
                                call_id: None,
                                content: vec![
                                    infinity_provider_protocol::message::ToolResultContent::Text(
                                        infinity_provider_protocol::message::Text {
                                            text: "Report from child thread: progress update"
                                                .into(),
                                        },
                                    ),
                                ],
                            },
                        )),
                        group_id: "t1".into(),
                        metadata: None,
                        synthetic: Some(SyntheticKind::Tagged(TaggedSyntheticKind::ThreadReport {
                            tool_call_id: "tc-async".into(),
                            child_thread_id: "child-1".into(),
                        })),
                        display_as: None,
                        subscription: false,
                    },
                    "report-1",
                )
                .await;
            for _ in 0..4 {
                tokio::task::yield_now().await;
            }

            // 3. The thread report must not have triggered a completion.
            assert!(
                ctrl.try_next_request().is_none(),
                "thread report should not trigger completion while waiting for async tool"
            );

            // 4. The real result arrives; the deferred report is included.
            running
                .send(tool_result_input("t1", "tc-async", "tool done").0, "res-1")
                .await;
            let req2 = ctrl.next_request().await;
            ctrl.send_text("all processed");
            ctrl.finish();
            collect_until_finished(&mut rx).await;

            let texts = tool_result_texts(&req2);
            assert!(texts.iter().any(|t| t.contains("tool done")));
            assert!(texts.iter().any(|t| t.contains("progress update")));
        })
        .await;
}

/// Shutting the system down while a completion is in flight interrupts the
/// completion (stripping trailing reasoning) and waits for every thread
/// driver to flush pending history items before the router task returns.
/// A tool result the model has already produced output for is validated and
/// must be persisted together with the partial output.
#[tokio::test(flavor = "current_thread")]
async fn shutdown_persists_model_validated_tool_result() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (running, mut rx, mut ctrl, conv) = start_system(vec![Box::new(AsyncTool)], None);

            // 1. User input → model issues an async tool call.
            running
                .send_user_text(ThreadId::from_ref("t1"), "do something")
                .await;
            let _req = ctrl.next_request().await;
            ctrl.send_tool_call("tc-1", "async_tool", serde_json::json!({}));
            ctrl.finish();
            collect_until_finished(&mut rx).await;

            // 2. The tool result arrives → the driver starts a new
            //    completion, and the model streams some output for it
            //    (validating the result for persistence).
            running
                .send(
                    tool_result_input("t1", "tc-1", "tool execution result").0,
                    "res-1",
                )
                .await;
            let _req2 = ctrl.next_request().await;
            ctrl.send_text("partial reply");
            loop {
                if let Evt::E(AgentEvent::TextChunk { .. }) = next_evt(&mut rx).await {
                    break;
                }
            }

            // 3. Shut down while the model is mid-response.
            let active_threads = running.active_threads();
            running.shutdown().await;
            assert!(
                active_threads
                    .lock()
                    .expect("bug: active_threads mutex poisoned")
                    .is_empty(),
                "no thread drivers should remain after shutdown"
            );

            // 4. The tool result and the partial output must have been
            //    synced to the store.
            let stored = persisted(&conv, "t1");
            assert!(
                stored.contains("tool execution result"),
                "validated in-flight tool result should be persisted on shutdown; history: {stored}"
            );
            assert!(
                stored.contains("partial reply"),
                "partial model output should be persisted on shutdown; history: {stored}"
            );
        })
        .await;
}

/// Shutting down while the model has produced *no* output for a freshly
/// arrived tool result must not persist that result: the model never
/// validated it, and it could be the oversized input that permanently
/// wedges the thread. The durable state keeps the tool call unanswered
/// (a later user input settles it with a synthetic "interrupted" result).
#[tokio::test(flavor = "current_thread")]
async fn shutdown_does_not_persist_unvalidated_tool_result() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (running, mut rx, mut ctrl, conv) = start_system(vec![Box::new(AsyncTool)], None);

            running
                .send_user_text(ThreadId::from_ref("t1"), "do something")
                .await;
            let _req = ctrl.next_request().await;
            ctrl.send_tool_call("tc-1", "async_tool", serde_json::json!({}));
            ctrl.finish();
            collect_until_finished(&mut rx).await;

            // The tool result arrives; the completion is in flight but the
            // model has not produced any output yet.
            running
                .send(
                    tool_result_input("t1", "tc-1", "tool execution result").0,
                    "res-1",
                )
                .await;
            let _req2 = ctrl.next_request().await;

            running.shutdown().await;

            let stored = persisted(&conv, "t1");
            assert!(
                stored.contains("tc-1"),
                "the tool call itself is model output and must be persisted; history: {stored}"
            );
            assert!(
                !stored.contains("tool execution result"),
                "an unvalidated tool result must not be persisted on shutdown; history: {stored}"
            );
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn subscription_event_deferred_during_async_tool_wait() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (running, mut rx, mut ctrl, _conv) = start_system(vec![Box::new(AsyncTool)], None);
            running
                .send_user_text(ThreadId::from_ref("t1"), "do async")
                .await;
            let _req = ctrl.next_request().await;
            ctrl.send_tool_call("tc-async", "async_tool", serde_json::json!({}));
            ctrl.finish();
            collect_until_finished(&mut rx).await;

            // A subscription event arrives while the tool result is pending.
            running
                .send(
                    subscription_event_input("t1", "tc-async", "sub event data").0,
                    "sub-1",
                )
                .await;
            for _ in 0..4 {
                tokio::task::yield_now().await;
            }
            assert!(
                ctrl.try_next_request().is_none(),
                "deferred event must not trigger a completion"
            );

            // The real result arrives; the deferred event is included after it.
            running
                .send(tool_result_input("t1", "tc-async", "tool done").0, "res-1")
                .await;
            let req2 = ctrl.next_request().await;
            ctrl.send_text("all processed");
            ctrl.finish();
            collect_until_finished(&mut rx).await;

            let texts = tool_result_texts(&req2);
            assert!(texts.iter().any(|t| t.contains("tool done")));
            assert!(texts.iter().any(|t| t.contains("sub event data")));
            assert!(
                !texts.iter().any(|t| t.contains("interrupted")),
                "in-flight tool call must not have been interrupted: {texts:?}"
            );
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn stale_result_does_not_flush_deferred_events_during_async_tool_wait() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (running, mut rx, mut ctrl, _conv) = start_system(vec![Box::new(AsyncTool)], None);
            running
                .send_user_text(ThreadId::from_ref("t1"), "do async")
                .await;
            let _req = ctrl.next_request().await;
            ctrl.send_tool_call("tc-async", "async_tool", serde_json::json!({}));
            ctrl.finish();
            collect_until_finished(&mut rx).await;

            // A deferrable event and a stale tool result arrive together.
            running
                .send(
                    subscription_event_input("t1", "tc-async", "sub event data").0,
                    "sub-1",
                )
                .await;
            running
                .send(
                    tool_result_input("t1", "tc-stale", "stale result").0,
                    "res-stale",
                )
                .await;
            for _ in 0..6 {
                tokio::task::yield_now().await;
            }
            assert!(
                ctrl.try_next_request().is_none(),
                "stale result must not flush the deferred event"
            );

            running
                .send(tool_result_input("t1", "tc-async", "tool done").0, "res-1")
                .await;
            let req2 = ctrl.next_request().await;
            ctrl.send_text("all processed");
            ctrl.finish();
            collect_until_finished(&mut rx).await;

            let texts = tool_result_texts(&req2);
            assert!(texts.iter().any(|t| t.contains("tool done")));
            assert!(texts.iter().any(|t| t.contains("sub event data")));
            assert!(
                !texts.iter().any(|t| t.contains("interrupted")),
                "in-flight tool call must not have been interrupted: {texts:?}"
            );
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn subscribe_mid_thinking_replays_current_thinking() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (running, mut rx, mut ctrl, _conv) = start_system(vec![], None);
            running
                .send_user_text(ThreadId::from_ref("t1"), "think hard")
                .await;
            let _req = ctrl.next_request().await;
            ctrl.send_chunk(StreamChunk::ReasoningDelta {
                id: None,
                text: "deep ".into(),
            });
            ctrl.send_chunk(StreamChunk::ReasoningDelta {
                id: None,
                text: "thought".into(),
            });
            // Wait until both chunks have been observed.
            let mut seen = String::new();
            while seen != "deep thought" {
                if let Evt::E(AgentEvent::ThinkingChunk { text }) = next_evt(&mut rx).await {
                    seen.push_str(&text);
                }
            }

            let (sub_tx, mut sub_rx) = mpsc::unbounded_channel();
            running.subscribe(ThreadId::from_ref("t1"), sub_tx).await;
            match next_evt(&mut sub_rx).await {
                Evt::Replay(snapshot) => {
                    assert!(snapshot.in_progress, "completion is in flight");
                    assert_eq!(snapshot.current_thinking.as_deref(), Some("deep thought"));
                }
                other => panic!("expected replay, got {other:?}"),
            }

            // Past the thinking chain, a new subscriber sees no stale thinking.
            ctrl.send_text("the answer");
            loop {
                if let Evt::E(AgentEvent::TextChunk { .. }) = next_evt(&mut rx).await {
                    break;
                }
            }
            let (sub_tx2, mut sub_rx2) = mpsc::unbounded_channel();
            running.subscribe(ThreadId::from_ref("t1"), sub_tx2).await;
            match next_evt(&mut sub_rx2).await {
                Evt::Replay(snapshot) => {
                    assert!(snapshot.in_progress);
                    assert!(snapshot.current_thinking.is_none());
                    // The partial text is visible via the in-flight turn.
                    assert!(matches!(
                        snapshot.history.last(),
                        Some(crate::message::InfinityMessage::Assistant { .. })
                    ));
                }
                other => panic!("expected replay, got {other:?}"),
            }

            ctrl.finish();
            collect_until_finished(&mut rx).await;
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn routes_to_separate_threads() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (mut running, mut rx, mut ctrl, _conv) = start_system(vec![], None);
            running
                .send_user_text(ThreadId::from_ref("t1"), "one")
                .await;
            let _r1 = ctrl.next_request().await;
            ctrl.send_text("first");
            ctrl.finish();
            collect_until_finished(&mut rx).await;

            running
                .send_user_text(ThreadId::from_ref("t2"), "two")
                .await;
            let _r2 = ctrl.next_request().await;
            ctrl.send_text("second");
            ctrl.finish();
            collect_until_finished(&mut rx).await;

            wait_idle(&mut running).await;
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn respawns_driver_after_idle() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (mut running, mut rx, mut ctrl, _conv) = start_system(vec![], None);
            running
                .send_user_text(ThreadId::from_ref("t1"), "hello")
                .await;
            let _req = ctrl.next_request().await;
            ctrl.send_text("hi");
            ctrl.finish();
            collect_until_finished(&mut rx).await;
            wait_idle(&mut running).await;
            assert!(running.is_idle());

            // A new message respawns the driver; the history is intact.
            running
                .send_user_text(ThreadId::from_ref("t1"), "again")
                .await;
            let req2 = ctrl.next_request().await;
            assert!(req2.chat_history.into_iter().any(|m| {
                if let infinity_provider_protocol::message::Message::User { content } = &m
                    && let Some(UserContent::Text(t)) = content.first()
                {
                    return t.text.contains("hello");
                }
                false
            }));
            ctrl.send_text("welcome back");
            ctrl.finish();
            collect_until_finished(&mut rx).await;
        })
        .await;
}
/// Regression test: a tool result arriving while a compaction child is
/// summarizing must not leave the history with an orphaned tool result whose
/// matching tool call was compacted away. The safe spawn point excludes the
/// trailing unanswered call from the compaction range.
#[tokio::test(flavor = "current_thread")]
async fn compaction_during_tool_call_corrupts_history() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (running, mut _rx, mut ctrl, _conv) =
                start_system(vec![Box::new(AsyncTool)], Some(small_context_entry()));

            // 1. User sends input, model responds with an async tool call and
            //    usage above the compaction threshold.
            running
                .send_user_text(ThreadId::from_ref("t1"), "do something")
                .await;
            let _req = ctrl.next_request().await;
            ctrl.send_tool_call("tc-1", "async_tool", serde_json::json!({}));
            ctrl.finish_with_usage(high_usage());

            // 2. Compaction triggers. Send the tool result BEFORE the
            //    compaction child finishes (simulating a fast tool execution).
            running
                .send(
                    tool_result_input("t1", "tc-1", "tool execution result").0,
                    "res-1",
                )
                .await;

            // Two model requests arrive (compaction child + parent processing
            // the tool result) in scheduling-dependent order.
            let req2 = ctrl.next_request().await;
            if is_compaction_req(&req2) {
                handle_compaction_child(&mut ctrl, &req2, "Summary of conversation so far");
                let _req3 = ctrl.next_request().await;
                ctrl.send_text("processed tool result");
                ctrl.finish();
            } else {
                ctrl.send_text("processed tool result");
                ctrl.finish();
                let compaction_req = ctrl.next_request().await;
                handle_compaction_child(&mut ctrl, &compaction_req, "Summary of conversation so far");
            }

            // 3. After CompactionComplete is applied, trigger a model call so
            //    we can inspect the history.
            running
                .send_user_text(ThreadId::from_ref("t1"), "what happened?")
                .await;
            let req_final =
                tokio::time::timeout(std::time::Duration::from_secs(5), ctrl.next_request())
                    .await
                    .expect("timed out waiting for final model request");

            let history: Vec<_> = req_final.chat_history.into_iter().collect();
            let has_orphaned_tool_result = history.iter().enumerate().any(|(i, m)| {
                if let infinity_provider_protocol::message::Message::User { content } = m
                    && let Some(UserContent::ToolResult(r)) = content.first()
                    && let Some(infinity_provider_protocol::message::ToolResultContent::Text(t)) = r.content.first()
                    && t.text.contains("tool execution result")
                {
                    !history[..i].iter().any(|prev| {
                        if let infinity_provider_protocol::message::Message::Assistant { content, .. } = prev {
                            content.iter().any(|c| {
                                matches!(c, infinity_provider_protocol::message::AssistantContent::ToolCall(tc) if tc.id == "tc-1")
                            })
                        } else {
                            false
                        }
                    })
                } else {
                    false
                }
            });
            assert!(
                !has_orphaned_tool_result,
                "History is corrupted: tool result has no matching tool_call after compaction. History: {history:#?}"
            );

            ctrl.send_text("all good");
            ctrl.finish();
        })
        .await;
}

/// Regression test for issue #64: after compaction is applied, the tracked
/// context usage must be reset so auto-compaction does not immediately
/// re-trigger on the stale pre-compaction token count.
#[tokio::test(flavor = "current_thread")]
async fn compaction_does_not_retrigger_after_applied() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (mut running, mut _rx, mut ctrl, _conv) =
                start_system(vec![Box::new(AsyncTool)], Some(small_context_entry()));

            // 1. User input → async tool call + high usage → compaction triggers.
            running
                .send_user_text(ThreadId::from_ref("t1"), "do something")
                .await;
            let _req = ctrl.next_request().await;
            ctrl.send_tool_call("tc-1", "async_tool", serde_json::json!({}));
            ctrl.finish_with_usage(high_usage());

            // 2. The compaction child asks for a summary; close it. This sends
            //    CompactionComplete to the parent driver, which is still alive
            //    waiting on the tc-1 tool result.
            let compaction_req = ctrl.next_request().await;
            assert!(is_compaction_req(&compaction_req));
            handle_compaction_child(&mut ctrl, &compaction_req, "Summary of conversation so far");

            // 3. The tool result arrives. The parent applies the compaction
            //    and then processes the tool result. With the bug, the stale
            //    86-token count immediately re-triggers compaction, spawning a
            //    second compaction child whose model request would show up
            //    here.
            running
                .send(
                    tool_result_input("t1", "tc-1", "tool execution result").0,
                    "res-1",
                )
                .await;
            let req = ctrl.next_request().await;
            assert!(
                !is_compaction_req(&req),
                "compaction must not re-trigger after being applied"
            );
            // Finish without usage so the tracked context usage stays at its
            // post-compaction value.
            ctrl.send_text("processed tool result");
            ctrl.finish();

            // 4. With the fix, all drivers wind down. With the bug, a second
            //    compaction child sits waiting on the mock model forever and
            //    the system never idles.
            wait_idle(&mut running).await;
            for _ in 0..100 {
                tokio::task::yield_now().await;
            }
            assert!(
                ctrl.try_next_request().is_none(),
                "no further model requests should be made after compaction is applied"
            );
        })
        .await;
}

/// Regression test: a second compaction on the same (still-loaded) thread
/// after a prior compaction must compute the safe spawn point against
/// absolute store orders, not in-memory indices. The compaction child's
/// inherited history must include everything before the pending tool call and
/// exclude the call itself.
#[tokio::test(flavor = "current_thread")]
async fn second_compaction_during_tool_call_after_prior_compaction() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (running, mut _rx, mut ctrl, _conv) =
                start_system(vec![Box::new(AsyncTool)], Some(small_context_entry()));

            // ── FIRST ROUND: text response + compaction (no tool call) ──
            running
                .send_user_text(ThreadId::from_ref("t1"), "first message")
                .await;
            let _req1 = ctrl.next_request().await;
            ctrl.send_text("first response");
            ctrl.finish_with_usage(high_usage());

            let compaction_req1 = ctrl.next_request().await;
            assert!(is_compaction_req(&compaction_req1));
            handle_compaction_child(&mut ctrl, &compaction_req1, "Summary of first round");

            // Build more history after the first compaction.
            running
                .send_user_text(ThreadId::from_ref("t1"), "second message")
                .await;
            let _req2 = ctrl.next_request().await;
            ctrl.send_text("second response");
            ctrl.finish();

            running
                .send_user_text(ThreadId::from_ref("t1"), "third message")
                .await;
            let _req3 = ctrl.next_request().await;
            ctrl.send_text("third response");
            ctrl.finish();

            // ── SECOND ROUND: tool call + compaction after prior compaction ──
            running
                .send_user_text(ThreadId::from_ref("t1"), "fourth message")
                .await;
            let _req4 = ctrl.next_request().await;
            ctrl.send_tool_call("tc-2", "async_tool", serde_json::json!({}));
            ctrl.finish_with_usage(high_usage());

            // Send tool result before the compaction child finishes.
            running
                .send(
                    tool_result_input("t1", "tc-2", "second tool result").0,
                    "res-2",
                )
                .await;

            let req_c = ctrl.next_request().await;
            let compaction_child_req = if is_compaction_req(&req_c) {
                let r = req_c;
                handle_compaction_child(&mut ctrl, &r, "Summary of second round");
                let _req_d = ctrl.next_request().await;
                ctrl.send_text("processed second tool");
                ctrl.finish();
                r
            } else {
                ctrl.send_text("processed second tool");
                ctrl.finish();
                let req_d = ctrl.next_request().await;
                handle_compaction_child(&mut ctrl, &req_d, "Summary of second round");
                req_d
            };

            // The compaction child inherits everything before the pending
            // tool call ("fourth message" included, tc-2 excluded).
            insta::assert_json_snapshot!(
                "second_compaction_child_history",
                compaction_child_req.chat_history,
                {
                    "[].content[].id" => "[id]",
                    "[].content[].content[].text" => insta::dynamic_redaction(|value, _| {
                        let s = value.as_str().unwrap_or("");
                        if s.contains("compaction thread") {
                            insta::internals::Content::String("[compaction_instructions]".into())
                        } else {
                            value
                        }
                    })
                }
            );
        })
        .await;
}

/// Regression test for issue #31: compaction spawned inside a child thread
/// must account for ancestor messages when computing the safe spawn point;
/// otherwise the grandchild panics loading history.
#[tokio::test(flavor = "current_thread")]
async fn compaction_inside_child_thread_does_not_panic() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (running, mut _rx, mut ctrl, _conv) =
                start_system(vec![Box::new(AsyncTool)], Some(small_context_entry()));

            // ── Build root history ──
            running
                .send_user_text(ThreadId::from_ref("root"), "root message one")
                .await;
            let _req = ctrl.next_request().await;
            ctrl.send_text("root response one");
            ctrl.finish();

            running
                .send_user_text(ThreadId::from_ref("root"), "root message two")
                .await;
            let _req = ctrl.next_request().await;
            ctrl.send_text("root response two");
            ctrl.finish();

            // ── Spawn a child thread from root ──
            running
                .send_user_text(ThreadId::from_ref("root"), "spawn a child")
                .await;
            let _req = ctrl.next_request().await;
            ctrl.send_tool_call(
                "tc-spawn",
                "spawn_thread",
                serde_json::json!({
                    "instructions": "do child work",
                    "child_of": ["root"]
                }),
            );
            ctrl.finish();

            // Parent loops back after the sync spawn — extract the child id.
            let parent_followup = ctrl.next_request().await;
            let child_thread_id = tool_result_texts(&parent_followup)
                .iter()
                .find_map(|t| {
                    let after =
                        t.strip_prefix("Child thread is successfully spawned and has ID: ")?;
                    Some(after.split('.').next()?.to_owned())
                })
                .expect("should find child thread ID in spawn result");
            ctrl.send_text("ok, child spawned");
            ctrl.finish();

            // ── Child thread gets its first model call ──
            let _child_req = ctrl.next_request().await;
            ctrl.send_text("child first response");
            ctrl.finish();

            // ── Another round in the child: tool call + high usage triggers
            //    compaction inside the child thread. ──
            running
                .send_user_text(ThreadId::from_ref(&child_thread_id), "child follow-up")
                .await;
            let _req = ctrl.next_request().await;
            ctrl.send_tool_call("tc-child", "async_tool", serde_json::json!({}));
            ctrl.finish_with_usage(high_usage());

            // Send tool result before compaction completes.
            running
                .send(
                    tool_result_input(&child_thread_id, "tc-child", "async tool result").0,
                    "res-child",
                )
                .await;

            // Two model requests arrive (compaction grandchild + child
            // processing the tool result) in scheduling-dependent order.
            let req_a = ctrl.next_request().await;
            let compaction_req = if is_compaction_req(&req_a) {
                handle_compaction_child(&mut ctrl, &req_a, "Summary of child work");
                let _req_b = ctrl.next_request().await;
                ctrl.send_text("processed tool result");
                ctrl.finish();
                req_a
            } else {
                ctrl.send_text("processed tool result");
                ctrl.finish();
                let req_b = ctrl.next_request().await;
                handle_compaction_child(&mut ctrl, &req_b, "Summary of child work");
                req_b
            };

            insta::assert_json_snapshot!(
                "issue31_compaction_child_history",
                compaction_req.chat_history,
                {
                    "[].content[].id" => "[id]",
                    "[].content[].content[].text" => insta::dynamic_redaction(|value, _| {
                        let s = value.as_str().unwrap_or("");
                        if s.contains("compaction thread") {
                            insta::internals::Content::String("[compaction_instructions]".into())
                        } else if s.contains("INSIDE the thread") {
                            insta::internals::Content::String("[spawn_instructions]".into())
                        } else {
                            value
                        }
                    })
                }
            );

            // ── After compaction applies, inspect the child's history ──
            running
                .send_user_text(
                    ThreadId::from_ref(&child_thread_id),
                    "message after compaction",
                )
                .await;
            let post_compaction_req =
                tokio::time::timeout(std::time::Duration::from_secs(5), ctrl.next_request())
                    .await
                    .expect("child should respond after compaction");

            insta::assert_json_snapshot!(
                "issue31_post_compaction_history",
                post_compaction_req.chat_history,
                {
                    "[].content[].id" => "[id]",
                }
            );

            ctrl.send_text("done");
            ctrl.finish();
        })
        .await;
}

// ═══════════════════════════════════════════════════════════════════════
// Oversized-input / persistence-safety driver tests
// ═══════════════════════════════════════════════════════════════════════

/// The messages persisted for a thread, debug-formatted for asserts.
fn persisted(conv: &crate::stores::InMemoryConversationStore, thread_id: &str) -> String {
    format!(
        "{:?}",
        conv.thread_messages(ThreadId::from_ref(thread_id))
            .unwrap_or_default()
    )
}

/// Interrupting a request that produced no model output yet must not
/// persist the interrupted input — but the input stays in memory and is
/// re-sent (together with the interrupting message) on the next round.
/// Everything persists once the model produces output for it.
#[tokio::test(flavor = "current_thread")]
async fn interrupt_before_output_persists_nothing_until_model_output() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (mut running, mut rx, mut ctrl, conv) = start_system(vec![], None);

            // Message A: request goes out, model never answers.
            running
                .send_user_text(ThreadId::from_ref("t1"), "first message")
                .await;
            let _req1 = ctrl.next_request().await;

            // Message B interrupts before any output.
            running
                .send_user_text(ThreadId::from_ref("t1"), "second message")
                .await;
            let req2 = ctrl.next_request().await;

            // Nothing persisted yet: the model never validated A (it could
            // be the poison input blowing up the context window).
            let stored = persisted(&conv, "t1");
            assert!(
                !stored.contains("first message"),
                "interrupted input must not be persisted before model output; store: {stored}"
            );

            // But A is still in memory: the retry request carries A and B.
            let history = format!("{:?}", req2.chat_history);
            assert!(
                history.contains("first message") && history.contains("second message"),
                "the retry must include both inputs; history: {history}"
            );

            ctrl.send_text("model reply");
            ctrl.finish();
            collect_until_finished(&mut rx).await; // interrupted round 1
            collect_until_finished(&mut rx).await; // round 2
            wait_idle(&mut running).await;

            // Model output validated everything: now it all persists.
            let stored = persisted(&conv, "t1");
            assert!(
                stored.contains("first message")
                    && stored.contains("second message")
                    && stored.contains("model reply"),
                "validated inputs and output must be persisted; store: {stored}"
            );
        })
        .await;
}

/// End-to-end oversized tool result recovery: the result is replaced with a
/// placeholder and retried; if the placeholder still overflows, the step
/// errors out *without* persisting the poison result, keeping the call
/// answered in memory as "interrupted by user" — so a later user input just
/// works and the thread never hangs.
#[tokio::test(flavor = "current_thread")]
async fn oversized_tool_result_never_wedges_the_thread() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            use infinity_provider_protocol::{CompletionError, ErrorClass};

            let (mut running, mut rx, mut ctrl, conv) =
                start_system(vec![Box::new(AsyncTool)], None);

            // Round 1: the model calls an async tool.
            running
                .send_user_text(ThreadId::from_ref("t1"), "use the tool")
                .await;
            let _req1 = ctrl.next_request().await;
            ctrl.send_tool_call("tc-1", "async_tool", serde_json::json!({}));
            ctrl.finish();
            collect_until_finished(&mut rx).await;

            // The tool result is enormous: the model reports an overflow.
            running
                .send(
                    tool_result_input("t1", "tc-1", "HUGE TOOL RESULT").0,
                    "res-1",
                )
                .await;
            let _req2 = ctrl.next_request().await;
            ctrl.send_error(CompletionError::provider(
                ErrorClass::ContextOverflow,
                "input is too long for the model",
            ));
            ctrl.drop_stream();

            // Core retries with the placeholder result...
            let req3 = tokio::time::timeout(std::time::Duration::from_secs(5), ctrl.next_request())
                .await
                .expect("core should retry with a placeholder tool result");
            let history = format!("{:?}", req3.chat_history);
            assert!(
                !history.contains("HUGE TOOL RESULT"),
                "retry must not resend the oversized result; history: {history}"
            );
            assert!(
                history.contains(crate::event_processor::TOOL_RESULT_TOO_LARGE_PLACEHOLDER),
                "retry must answer the call with the placeholder; history: {history}"
            );

            // ...which still overflows: the step gives up.
            ctrl.send_error(CompletionError::provider(
                ErrorClass::ContextOverflow,
                "input is too long for the model",
            ));
            ctrl.drop_stream();
            collect_until_finished(&mut rx).await;

            // The poison result must not have been persisted.
            let stored = persisted(&conv, "t1");
            assert!(
                !stored.contains("HUGE TOOL RESULT"),
                "the oversized result must not be persisted; store: {stored}"
            );

            // A later user input settles the stranded call and the thread
            // keeps working.
            running
                .send_user_text(ThreadId::from_ref("t1"), "are you alive?")
                .await;
            let req4 = tokio::time::timeout(std::time::Duration::from_secs(5), ctrl.next_request())
                .await
                .expect("a later user input must reach the model (no permanent hang)");
            let history = format!("{:?}", req4.chat_history);
            assert!(
                history.contains(crate::event_processor::TOOL_CALL_INTERRUPTED_TEXT),
                "the call must stay answered (settled as interrupted); history: {history}"
            );
            assert!(
                !history.contains(crate::event_processor::TOOL_RESULT_TOO_LARGE_PLACEHOLDER),
                "a 'too large' note would mislead the model; history: {history}"
            );
            assert!(
                !history.contains("HUGE TOOL RESULT"),
                "the poison result must be gone from the model-facing history"
            );
            ctrl.send_text("alive and well");
            ctrl.finish();
            let texts = collect_until_finished(&mut rx).await;
            assert_eq!(texts, vec!["alive and well"]);
            wait_idle(&mut running).await;
        })
        .await;
}

/// Reboot recovery: a session dies while a tool result was still
/// unvalidated, so the durable store ends on the unanswered tool *call*.
/// When a fresh session boots from that store and user input arrives, the
/// stranded call must be settled with a synthetic "interrupted" result and
/// the thread must keep working.
#[tokio::test(flavor = "current_thread")]
async fn rebooted_session_settles_persisted_unanswered_tool_call() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            use super::AgentSystemBuilder;
            use crate::stores::{InMemoryConversationStore, InMemoryStateStore};

            let conv = InMemoryConversationStore::new();
            let state = InMemoryStateStore::new();

            // ── Session 1: the model calls a tool; its (oversized) result
            // arrives but the session shuts down before the model produces
            // any output for it. ──
            let (model1, mut ctrl1) = model_source(None);
            let (tx1, mut rx1) = mpsc::unbounded_channel();
            let running1 = AgentSystemBuilder::new_local(conv.clone(), state.clone(), model1)
                .tools(vec![Box::new(AsyncTool) as Box<dyn Tool<ChannelSender>>])
                .start_with_observer(move |_thread_id| TestObserver { tx: tx1.clone() });
            running1
                .send_user_text(ThreadId::from_ref("t1"), "do something")
                .await;
            let _req1 = ctrl1.next_request().await;
            ctrl1.send_tool_call("tc-1", "async_tool", serde_json::json!({}));
            ctrl1.finish();
            collect_until_finished(&mut rx1).await;

            running1
                .send(
                    tool_result_input("t1", "tc-1", "HUGE TOOL RESULT").0,
                    "res-1",
                )
                .await;
            let _req2 = ctrl1.next_request().await;
            running1.shutdown().await;

            // Durable state ends on the unanswered tool call; the
            // unvalidated result was not persisted.
            let stored = persisted(&conv, "t1");
            assert!(
                stored.contains("tc-1") && !stored.contains("HUGE TOOL RESULT"),
                "store must end on the unanswered call; store: {stored}"
            );

            // ── Session 2: boots from the same stores. ──
            let (model2, mut ctrl2) = model_source(None);
            let (tx2, mut rx2) = mpsc::unbounded_channel();
            let mut running2 = AgentSystemBuilder::new_local(conv.clone(), state.clone(), model2)
                .tools(vec![Box::new(AsyncTool) as Box<dyn Tool<ChannelSender>>])
                .start_with_observer(move |_thread_id| TestObserver { tx: tx2.clone() });
            running2
                .send_user_text(ThreadId::from_ref("t1"), "hello again")
                .await;

            // The stranded call is settled before the new input reaches the
            // model.
            let req = ctrl2.next_request().await;
            let history = format!("{:?}", req.chat_history);
            assert!(
                history.contains("Tool call interrupted by user"),
                "reboot + user input must settle the stranded call; history: {history}"
            );
            assert!(history.contains("hello again"), "history: {history}");

            ctrl2.send_text("back online");
            ctrl2.finish();
            let texts = collect_until_finished(&mut rx2).await;
            assert_eq!(texts, vec!["back online"]);
            wait_idle(&mut running2).await;

            // Everything from the recovered round is validated + persisted.
            let stored = persisted(&conv, "t1");
            assert!(
                stored.contains("Tool call interrupted by user")
                    && stored.contains("hello again")
                    && stored.contains("back online"),
                "recovered round must persist; store: {stored}"
            );
        })
        .await;
}

/// Variant of the interrupt test where the retry round overflows the
/// context. User text has no safe placeholder substitute, so the only
/// possible reaction is to drop the pending inputs — and when inputs are
/// dropped the step must *stop* (no blind retry: the user clearly wants to
/// say something, and silently re-running without their words would be
/// worse). The thread stays usable for the next input.
#[tokio::test(flavor = "current_thread")]
async fn overflow_after_interrupt_drops_user_inputs_and_stops() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            use infinity_provider_protocol::{CompletionError, ErrorClass};

            let (mut running, mut rx, mut ctrl, conv) = start_system(vec![], None);

            // Message A: request goes out, model never answers.
            running
                .send_user_text(ThreadId::from_ref("t1"), "first message")
                .await;
            let _req1 = ctrl.next_request().await;

            // Message B interrupts; the retry carries A and B...
            running
                .send_user_text(ThreadId::from_ref("t1"), "second message")
                .await;
            let req2 = ctrl.next_request().await;
            let history = format!("{:?}", req2.chat_history);
            assert!(history.contains("first message") && history.contains("second message"));

            // ...and overflows the context.
            ctrl.send_error(CompletionError::provider(
                ErrorClass::ContextOverflow,
                "input is too long for the model",
            ));
            ctrl.drop_stream();

            // The step surfaces the drop and stops — dropping inputs must
            // never be followed by a silent retry. (The first
            // CompletionFinished belongs to the interrupted round; the
            // overflow happens in the second.)
            let mut saw_drop_info = false;
            let mut finished_rounds = 0;
            while finished_rounds < 2 {
                match next_evt(&mut rx).await {
                    Evt::E(AgentEvent::Info { text }) if text.contains("too large") => {
                        saw_drop_info = true;
                    }
                    Evt::E(AgentEvent::CompletionFinished { .. }) => finished_rounds += 1,
                    _ => {}
                }
            }
            assert!(
                saw_drop_info,
                "the dropped input must be surfaced to the user"
            );
            wait_idle(&mut running).await;
            assert!(
                ctrl.try_next_request().is_none(),
                "dropping user input must stop the round, not retry it"
            );

            // Nothing was persisted.
            let stored = persisted(&conv, "t1");
            assert!(
                !stored.contains("first message") && !stored.contains("second message"),
                "dropped inputs must not be persisted; store: {stored}"
            );

            // The thread is not wedged: a fresh (smaller) message works, and
            // the dropped inputs are gone from the model-facing history too.
            running
                .send_user_text(ThreadId::from_ref("t1"), "third message")
                .await;
            let req3 = ctrl.next_request().await;
            let history = format!("{:?}", req3.chat_history);
            assert!(
                history.contains("third message")
                    && !history.contains("first message")
                    && !history.contains("second message"),
                "dropped inputs must not be resent; history: {history}"
            );
            ctrl.send_text("hello!");
            ctrl.finish();
            let texts = collect_until_finished(&mut rx).await;
            assert_eq!(texts, vec!["hello!"]);
            wait_idle(&mut running).await;

            let stored = persisted(&conv, "t1");
            assert!(
                stored.contains("third message") && stored.contains("hello!"),
                "the recovered round must persist; store: {stored}"
            );
        })
        .await;
}
