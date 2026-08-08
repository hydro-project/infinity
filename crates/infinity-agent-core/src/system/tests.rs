//! Behavioral tests for the high-level agent system: stepping, driver
//! batching, interruption, deferral, idling, routing, replay, and
//! auto-compaction.

use std::sync::Arc;

use async_trait::async_trait;
use rig::message::UserContent;
use tokio::sync::mpsc;

use crate::message::{InputMessage, InputMessageContent, SyntheticKind, TaggedSyntheticKind};
use crate::stores::{InMemoryConversationStore, InMemoryStateStore};
use crate::tools::{Tool, ToolContext};
use crate::traits::InputSender;
use infinity_provider_protocol::ModelEntry;
use rig_mock::{MockModelController, mock_model};

use super::builder::AgentSystemBuilder;
use super::defer::NoDeferral;
use super::events::{AgentEvent, ReplaySnapshot};
use super::model::StaticModel;
use super::observer::{EventCollector, ThreadObserver};
use super::router::RunningSystem;
use super::sender::ChannelSender;
use super::thread::StepOutcome;

// ── Test observer ──

/// Events seen by attached test clients: live agent events or replays.
#[derive(Debug, Clone)]
enum Evt {
    E(AgentEvent),
    Replay(ReplaySnapshot),
}

/// Broadcasts every event to a channel; replays are sent to the subscriber's
/// own channel (mirroring how a real embedding fans out to clients).
#[derive(Clone)]
struct TestObserver {
    tx: mpsc::UnboundedSender<Evt>,
}

#[async_trait(?Send)]
impl ThreadObserver for TestObserver {
    type SubscribeRequest = mpsc::UnboundedSender<Evt>;

    fn on_event(&self, _thread_id: &str, event: &AgentEvent) {
        let _ = self.tx.send(Evt::E(event.clone()));
    }

    fn on_subscribe(
        &self,
        _thread_id: &str,
        request: Self::SubscribeRequest,
        snapshot: ReplaySnapshot,
    ) {
        let _ = request.send(Evt::Replay(snapshot));
    }
}

// ── Helpers ──

fn model_source(ctrl_entry: Option<ModelEntry>) -> (StaticModel, MockModelController) {
    let (model, ctrl) = mock_model();
    let entry = ctrl_entry.unwrap_or(ModelEntry {
        model_id: "mock".to_owned(),
        display_name: "mock".to_owned(),
        context_window: 0,
        max_output_tokens: None,
        supports_image_input: false,
    });
    let provider = infinity_provider_protocol::SingleModelProvider::new(entry.clone(), model);
    (StaticModel::from_entry(Arc::new(provider), &entry), ctrl)
}

fn user_text_input(group_id: &str, text: &str) -> (InputMessage, String) {
    (
        InputMessage {
            content: InputMessageContent::User(UserContent::text(text)),
            group_id: group_id.into(),
            metadata: None,
            synthetic: None,
            display_as: None,
            subscription: false,
        },
        uuid::Uuid::new_v4().to_string(),
    )
}

fn tool_result_input(group_id: &str, id: &str, text: &str) -> (InputMessage, String) {
    (
        InputMessage {
            content: InputMessageContent::User(UserContent::ToolResult(rig::message::ToolResult {
                id: id.into(),
                call_id: None,
                content: rig::OneOrMany::one(rig::message::ToolResultContent::Text(
                    rig::agent::Text { text: text.into() },
                )),
            })),
            group_id: group_id.into(),
            metadata: None,
            synthetic: None,
            display_as: None,
            subscription: false,
        },
        uuid::Uuid::new_v4().to_string(),
    )
}

fn subscription_event_input(
    group_id: &str,
    tool_call_id: &str,
    text: &str,
) -> (InputMessage, String) {
    (
        InputMessage {
            content: InputMessageContent::User(UserContent::ToolResult(rig::message::ToolResult {
                id: tool_call_id.into(),
                call_id: None,
                content: rig::OneOrMany::one(rig::message::ToolResultContent::Text(
                    rig::agent::Text { text: text.into() },
                )),
            })),
            group_id: group_id.into(),
            metadata: None,
            synthetic: Some(SyntheticKind::Tagged(
                TaggedSyntheticKind::SubscriptionEvent {
                    tool_call_id: tool_call_id.into(),
                    associative: true,
                    r#final: false,
                },
            )),
            display_as: None,
            subscription: false,
        },
        uuid::Uuid::new_v4().to_string(),
    )
}

/// An async tool whose result is delivered later through the input queue.
struct AsyncTool;

#[async_trait]
impl Tool<ChannelSender> for AsyncTool {
    fn name(&self) -> &str {
        "async_tool"
    }
    fn description(&self) -> &str {
        "async"
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

/// Start a local system with the given tools, returning the running handle,
/// the observer event stream, the model controller, and the store.
fn start_system(
    tools: Vec<Box<dyn Tool<ChannelSender>>>,
    entry: Option<ModelEntry>,
) -> (
    RunningSystem<mpsc::UnboundedSender<Evt>>,
    mpsc::UnboundedReceiver<Evt>,
    MockModelController,
    InMemoryConversationStore,
) {
    start_system_with(tools, entry, true)
}

fn start_system_with(
    tools: Vec<Box<dyn Tool<ChannelSender>>>,
    entry: Option<ModelEntry>,
    builtin_tools: bool,
) -> (
    RunningSystem<mpsc::UnboundedSender<Evt>>,
    mpsc::UnboundedReceiver<Evt>,
    MockModelController,
    InMemoryConversationStore,
) {
    let (model, ctrl) = model_source(entry);
    let conv = InMemoryConversationStore::new();
    let mut builder =
        AgentSystemBuilder::new_local(conv.clone(), InMemoryStateStore::new(), model).tools(tools);
    if !builtin_tools {
        builder = builder.without_builtin_tools();
    }
    let system = builder.build_local();
    let (tx, rx) = mpsc::unbounded_channel();
    let running = system.start(move |_thread_id| TestObserver { tx: tx.clone() });
    (running, rx, ctrl, conv)
}

async fn next_evt(rx: &mut mpsc::UnboundedReceiver<Evt>) -> Evt {
    tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
        .await
        .expect("timed out waiting for event")
        .expect("event channel closed")
}

async fn collect_until_finished(rx: &mut mpsc::UnboundedReceiver<Evt>) -> Vec<String> {
    let mut texts = Vec::new();
    loop {
        match next_evt(rx).await {
            Evt::E(AgentEvent::TextChunk { text }) => texts.push(text),
            Evt::E(AgentEvent::CompletionFinished { .. }) => break,
            _ => {}
        }
    }
    texts
}

/// Wait until all live drivers have exited (draining thread-exit
/// notifications until the active set is empty).
async fn wait_idle<Sub: Send + 'static>(running: &mut RunningSystem<Sub>) {
    while !running.is_idle() {
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            running.thread_exits.recv(),
        )
        .await
        .expect("timed out waiting for a thread driver to exit")
        .expect("thread exit channel closed");
    }
}

/// The tool-result texts in a completion request's chat history.
fn tool_result_texts(req: &rig::completion::CompletionRequest) -> Vec<String> {
    req.chat_history
        .iter()
        .filter_map(|m| {
            if let rig::message::Message::User { content } = m
                && let UserContent::ToolResult(r) = content.first()
                && let rig::message::ToolResultContent::Text(t) = r.content.first()
            {
                Some(t.text)
            } else {
                None
            }
        })
        .collect()
}

/// Whether a model request is the seed of a compaction child thread.
fn is_compaction_req(req: &rig::completion::CompletionRequest) -> bool {
    tool_result_texts(req)
        .iter()
        .any(|t| t.contains("compaction thread"))
}

/// Extract the compaction child's thread id from its seed instruction.
fn find_compaction_child_id(req: &rig::completion::CompletionRequest) -> String {
    tool_result_texts(req)
        .iter()
        .find_map(|t| {
            let rest = t.split("close_thread with your thread ID (").nth(1)?;
            rest.split(')').next().map(str::to_owned)
        })
        .expect("compaction seed should include the child thread id")
}

/// Answer a compaction child's request by closing it with a summary report.
fn handle_compaction_child(
    ctrl: &mut MockModelController,
    req: &rig::completion::CompletionRequest,
    summary: &str,
) {
    let child_thread_id = find_compaction_child_id(req);
    ctrl.send_tool_call(
        "tc-close",
        "close_thread",
        serde_json::json!({
            "thread_id": child_thread_id,
            "report_to_parent": summary,
        }),
    );
    ctrl.finish();
}

fn high_usage() -> Option<rig::completion::Usage> {
    Some(rig::completion::Usage {
        input_tokens: 76,
        output_tokens: 10,
        total_tokens: 86,
        cached_input_tokens: 0,
    })
}

fn small_context_entry() -> ModelEntry {
    ModelEntry {
        model_id: "mock".to_owned(),
        display_name: "mock".to_owned(),
        context_window: 100,
        max_output_tokens: None,
        supports_image_input: false,
    }
}

// ── Step-mode tests ──

#[tokio::test(flavor = "current_thread")]
async fn step_mode_runs_single_slice() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (model, mut ctrl) = model_source(None);
            let (sender, mut loopback_rx) = ChannelSender::new_pair();
            let system = AgentSystemBuilder::new(
                InMemoryConversationStore::new(),
                InMemoryStateStore::new(),
                model,
                sender,
            )
            .build();

            let step = tokio::task::spawn_local(async move {
                let collector = EventCollector::new();
                let outcome = system
                    .step(
                        "t1",
                        vec![user_text_input("t1", "hello")],
                        &collector,
                        &mut NoDeferral,
                    )
                    .await
                    .expect("step");
                (outcome, collector.take())
            });

            let _req = ctrl.next_request().await;
            ctrl.send_text("hi there");
            ctrl.finish();

            let (outcome, events) = step.await.expect("join");
            assert!(matches!(outcome, StepOutcome::Completed { .. }));
            assert!(
                events
                    .iter()
                    .any(|e| matches!(e, AgentEvent::UserInput { text } if text == "hello"))
            );
            assert!(
                events
                    .iter()
                    .any(|e| matches!(e, AgentEvent::TextChunk { text } if text == "hi there"))
            );
            assert!(
                events
                    .iter()
                    .any(|e| matches!(e, AgentEvent::CompletionFinished { .. }))
            );
            // Nothing was scheduled for later.
            assert!(loopback_rx.try_recv().is_err());
        })
        .await;
}

// ── Driver / router tests ──

#[tokio::test(flavor = "current_thread")]
async fn driver_idles_after_text_response() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (mut running, mut rx, mut ctrl, _conv) = start_system(vec![], None);
            running
                .send_user_text("t1", "hello")
                .await
                .expect("send input");
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
                .send_user_text("t1", "use tool")
                .await
                .expect("send input");
            let _req = ctrl.next_request().await;
            ctrl.send_tool_call("tc-1", "async_tool", serde_json::json!({}));
            ctrl.finish();
            collect_until_finished(&mut rx).await;

            assert!(
                running.thread_exits.try_recv().is_err(),
                "should not idle while a tool call is pending"
            );
            assert!(!running.is_idle());

            // A client attaching while waiting gets a replay whose history
            // ends with the unresolved tool call and no completion in flight.
            let (sub_tx, mut sub_rx) = mpsc::unbounded_channel();
            running.subscribe("t1", sub_tx).await;
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
                .await
                .expect("send result");
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
                .send_user_text("t1", "first")
                .await
                .expect("send input");
            let _req = ctrl.next_request().await;
            ctrl.send_text("partial...");
            // Wait until the chunk is observed so the completion is in flight.
            loop {
                if let Evt::E(AgentEvent::TextChunk { .. }) = next_evt(&mut rx).await {
                    break;
                }
            }
            running
                .send_user_text("t1", "stop that")
                .await
                .expect("send interrupt");
            let req2 = ctrl.next_request().await;
            let has_interrupt = req2.chat_history.into_iter().any(|m| {
                if let rig::message::Message::User { content } = &m
                    && let UserContent::Text(t) = content.first()
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
                .send_user_text("t1", "do stuff")
                .await
                .expect("send");
            let _req = ctrl.next_request().await;
            ctrl.send_tool_call("tc-1", "async_tool", serde_json::json!({}));
            ctrl.finish();
            collect_until_finished(&mut rx).await;

            running
                .send(tool_result_input("t1", "tc-1", "tool output").0, "res-1")
                .await
                .expect("send result");
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
                .await
                .expect("send stale");
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
            /// A tool that starts a subscription: its result is delivered
            /// through the input queue with `subscription: true`.
            struct SubTool;
            #[async_trait]
            impl Tool<ChannelSender> for SubTool {
                fn name(&self) -> &str {
                    "subscribe_tool"
                }
                fn description(&self) -> &str {
                    "s"
                }
                fn parameters(&self) -> serde_json::Value {
                    serde_json::json!({"type":"object","properties":{}})
                }
                async fn execute(
                    &self,
                    _: serde_json::Value,
                    id: String,
                    call_id: Option<String>,
                    ctx: &ToolContext<ChannelSender>,
                ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
                    let msg = InputMessage {
                        content: InputMessageContent::User(UserContent::ToolResult(
                            rig::message::ToolResult {
                                id: id.clone(),
                                call_id,
                                content: rig::OneOrMany::one(
                                    rig::message::ToolResultContent::Text(rig::agent::Text {
                                        text: "subscribed".into(),
                                    }),
                                ),
                            },
                        )),
                        group_id: ctx.group_id.clone(),
                        metadata: None,
                        synthetic: None,
                        display_as: None,
                        subscription: true,
                    };
                    ctx.message_sender
                        .send_to_input_queue(msg, &ctx.group_id, &id)
                        .await?;
                    Ok(())
                }
            }

            let (running, mut rx, mut ctrl, _conv) = start_system(vec![Box::new(SubTool)], None);
            running
                .send_user_text("t1", "subscribe")
                .await
                .expect("send");
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
                .await
                .expect("send event");
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
            running.send_user_text("t1", "close").await.expect("send");
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
                .send_user_text("t1", "do async")
                .await
                .expect("send");
            let _req = ctrl.next_request().await;
            ctrl.send_tool_call("tc-async", "async_tool", serde_json::json!({}));
            ctrl.finish();
            collect_until_finished(&mut rx).await;

            // 2. While waiting for the tool result, a thread report arrives.
            running
                .send(
                    InputMessage {
                        content: InputMessageContent::User(UserContent::ToolResult(
                            rig::message::ToolResult {
                                id: String::new(),
                                call_id: None,
                                content: rig::OneOrMany::one(
                                    rig::message::ToolResultContent::Text(rig::agent::Text {
                                        text: "Report from child thread: progress update".into(),
                                    }),
                                ),
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
                .await
                .expect("send report");
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
                .await
                .expect("send result");
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
#[tokio::test(flavor = "current_thread")]
async fn shutdown_persists_in_flight_tool_result() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (running, mut rx, mut ctrl, conv) = start_system(vec![Box::new(AsyncTool)], None);

            // 1. User input → model issues an async tool call.
            running
                .send_user_text("t1", "do something")
                .await
                .expect("send");
            let _req = ctrl.next_request().await;
            ctrl.send_tool_call("tc-1", "async_tool", serde_json::json!({}));
            ctrl.finish();
            collect_until_finished(&mut rx).await;

            // 2. The tool result arrives → the driver starts a new
            //    completion. Waiting for the model request guarantees the
            //    completion is in flight and the tool result is sitting in
            //    the history manager's pending (unsynced) items.
            running
                .send(
                    tool_result_input("t1", "tc-1", "tool execution result").0,
                    "res-1",
                )
                .await
                .expect("send result");
            let _req2 = ctrl.next_request().await;

            // 3. Shut down while the model is mid-response.
            let active_threads = running.active_threads();
            running.begin_shutdown();
            running.task.await.expect("router join");
            assert!(
                active_threads
                    .lock()
                    .expect("bug: active_threads mutex poisoned")
                    .is_empty(),
                "no thread drivers should remain after shutdown"
            );

            // 4. The tool result must have been synced to the store.
            use crate::traits::ConversationStore;
            let history = conv
                .load_history_up_to("t1", None, None)
                .await
                .expect("load history");
            let has_tool_result = history.iter().any(|m| {
                if let crate::message::InfinityMessage::ToolResult { result, .. } = m
                    && let rig::message::ToolResultContent::Text(t) = result.content.first()
                {
                    result.id == "tc-1" && t.text.contains("tool execution result")
                } else {
                    false
                }
            });
            assert!(
                has_tool_result,
                "in-flight tool result should be persisted on shutdown; history: {history:#?}"
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
                .send_user_text("t1", "do async")
                .await
                .expect("send input");
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
                .await
                .expect("send event");
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
                .await
                .expect("send result");
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
async fn stale_result_does_not_flush_deferred_events() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (running, mut rx, mut ctrl, _conv) = start_system(vec![Box::new(AsyncTool)], None);
            running
                .send_user_text("t1", "do async")
                .await
                .expect("send input");
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
                .await
                .expect("send event");
            running
                .send(
                    tool_result_input("t1", "tc-stale", "stale result").0,
                    "res-stale",
                )
                .await
                .expect("send stale");
            for _ in 0..6 {
                tokio::task::yield_now().await;
            }
            assert!(
                ctrl.try_next_request().is_none(),
                "stale result must not flush the deferred event"
            );

            running
                .send(tool_result_input("t1", "tc-async", "tool done").0, "res-1")
                .await
                .expect("send result");
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
                .send_user_text("t1", "think hard")
                .await
                .expect("send input");
            let _req = ctrl.next_request().await;
            ctrl.send_chunk(rig::streaming::RawStreamingChoice::ReasoningDelta {
                id: None,
                reasoning: "deep ".into(),
            });
            ctrl.send_chunk(rig::streaming::RawStreamingChoice::ReasoningDelta {
                id: None,
                reasoning: "thought".into(),
            });
            // Wait until both chunks have been observed.
            let mut seen = String::new();
            while seen != "deep thought" {
                if let Evt::E(AgentEvent::ThinkingChunk { text }) = next_evt(&mut rx).await {
                    seen.push_str(&text);
                }
            }

            let (sub_tx, mut sub_rx) = mpsc::unbounded_channel();
            running.subscribe("t1", sub_tx).await;
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
            running.subscribe("t1", sub_tx2).await;
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
            running.send_user_text("t1", "one").await.expect("send");
            let _r1 = ctrl.next_request().await;
            ctrl.send_text("first");
            ctrl.finish();
            collect_until_finished(&mut rx).await;

            running.send_user_text("t2", "two").await.expect("send");
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
            running.send_user_text("t1", "hello").await.expect("send");
            let _req = ctrl.next_request().await;
            ctrl.send_text("hi");
            ctrl.finish();
            collect_until_finished(&mut rx).await;
            wait_idle(&mut running).await;
            assert!(running.is_idle());

            // A new message respawns the driver; the history is intact.
            running.send_user_text("t1", "again").await.expect("send");
            let req2 = ctrl.next_request().await;
            assert!(req2.chat_history.into_iter().any(|m| {
                if let rig::message::Message::User { content } = &m
                    && let UserContent::Text(t) = content.first()
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

#[tokio::test(flavor = "current_thread")]
async fn shutdown_flushes_in_flight_turn() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (running, mut rx, mut ctrl, conv) = start_system(vec![], None);
            running.send_user_text("t1", "hello").await.expect("send");
            let _req = ctrl.next_request().await;
            ctrl.send_text("partial answer");
            loop {
                if let Evt::E(AgentEvent::TextChunk { .. }) = next_evt(&mut rx).await {
                    break;
                }
            }

            // Shut down mid-completion: the driver cancels, flushes, syncs.
            running.begin_shutdown();
            running.task.await.expect("router join");

            use crate::traits::ConversationStore;
            let history = conv
                .load_history_up_to("t1", None, None)
                .await
                .expect("load history");
            assert!(
                history
                    .iter()
                    .any(|m| matches!(m, crate::message::InfinityMessage::Assistant { .. })),
                "partial assistant text must be persisted on shutdown, got {history:?}"
            );
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn auto_compaction_triggers_and_applies() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            // Tiny context window so the usage report crosses the threshold.
            let (mut running, mut rx, mut ctrl, _conv) =
                start_system(vec![], Some(small_context_entry()));
            running.send_user_text("t1", "hello").await.expect("send");
            let _req = ctrl.next_request().await;
            ctrl.send_text("hi there");
            ctrl.finish_with_usage(high_usage());
            collect_until_finished(&mut rx).await;

            // The driver spawns a compaction child; close it with a summary.
            let creq = ctrl.next_request().await;
            assert!(is_compaction_req(&creq));
            handle_compaction_child(&mut ctrl, &creq, "summary of everything");

            // The parent eventually applies the compaction.
            loop {
                if let Evt::E(AgentEvent::CompactionApplied) = next_evt(&mut rx).await {
                    break;
                }
            }
            wait_idle(&mut running).await;
        })
        .await;
}

/// Regression test: a tool result arriving while a compaction child is
/// summarizing must not leave the history with an orphaned tool result whose
/// matching tool call was compacted away. The safe spawn point excludes the
/// trailing unanswered call from the compaction range.
#[tokio::test(flavor = "current_thread")]
async fn compaction_during_tool_call_preserves_history() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (running, mut _rx, mut ctrl, _conv) =
                start_system(vec![Box::new(AsyncTool)], Some(small_context_entry()));

            // 1. User sends input, model responds with an async tool call and
            //    usage above the compaction threshold.
            running
                .send_user_text("t1", "do something")
                .await
                .expect("send");
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
                .await
                .expect("send result");

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
                .send_user_text("t1", "what happened?")
                .await
                .expect("send follow-up");
            let req_final =
                tokio::time::timeout(std::time::Duration::from_secs(5), ctrl.next_request())
                    .await
                    .expect("timed out waiting for final model request");

            let history: Vec<_> = req_final.chat_history.into_iter().collect();
            let has_orphaned_tool_result = history.iter().enumerate().any(|(i, m)| {
                if let rig::message::Message::User { content } = m
                    && let UserContent::ToolResult(r) = content.first()
                    && let rig::message::ToolResultContent::Text(t) = r.content.first()
                    && t.text.contains("tool execution result")
                {
                    !history[..i].iter().any(|prev| {
                        if let rig::message::Message::Assistant { content, .. } = prev {
                            content.iter().any(|c| {
                                matches!(c, rig::message::AssistantContent::ToolCall(tc) if tc.id == "tc-1")
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
                .send_user_text("t1", "do something")
                .await
                .expect("send");
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
                .await
                .expect("send result");
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
                .send_user_text("t1", "first message")
                .await
                .expect("send");
            let _req1 = ctrl.next_request().await;
            ctrl.send_text("first response");
            ctrl.finish_with_usage(high_usage());

            let compaction_req1 = ctrl.next_request().await;
            assert!(is_compaction_req(&compaction_req1));
            handle_compaction_child(&mut ctrl, &compaction_req1, "Summary of first round");

            // Build more history after the first compaction.
            running
                .send_user_text("t1", "second message")
                .await
                .expect("send");
            let _req2 = ctrl.next_request().await;
            ctrl.send_text("second response");
            ctrl.finish();

            running
                .send_user_text("t1", "third message")
                .await
                .expect("send");
            let _req3 = ctrl.next_request().await;
            ctrl.send_text("third response");
            ctrl.finish();

            // ── SECOND ROUND: tool call + compaction after prior compaction ──
            running
                .send_user_text("t1", "fourth message")
                .await
                .expect("send");
            let _req4 = ctrl.next_request().await;
            ctrl.send_tool_call("tc-2", "async_tool", serde_json::json!({}));
            ctrl.finish_with_usage(high_usage());

            // Send tool result before the compaction child finishes.
            running
                .send(
                    tool_result_input("t1", "tc-2", "second tool result").0,
                    "res-2",
                )
                .await
                .expect("send result");

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
                .send_user_text("root", "root message one")
                .await
                .expect("send");
            let _req = ctrl.next_request().await;
            ctrl.send_text("root response one");
            ctrl.finish();

            running
                .send_user_text("root", "root message two")
                .await
                .expect("send");
            let _req = ctrl.next_request().await;
            ctrl.send_text("root response two");
            ctrl.finish();

            // ── Spawn a child thread from root ──
            running
                .send_user_text("root", "spawn a child")
                .await
                .expect("send");
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
                .send_user_text(&child_thread_id, "child follow-up")
                .await
                .expect("send");
            let _req = ctrl.next_request().await;
            ctrl.send_tool_call("tc-child", "async_tool", serde_json::json!({}));
            ctrl.finish_with_usage(high_usage());

            // Send tool result before compaction completes.
            running
                .send(
                    tool_result_input(&child_thread_id, "tc-child", "async tool result").0,
                    "res-child",
                )
                .await
                .expect("send result");

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
                .send_user_text(&child_thread_id, "message after compaction")
                .await
                .expect("send post-compaction message");
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

// ── Thread handles ──

/// Start a local system with the built-in handle observer.
fn start_handle_system() -> (
    RunningSystem<super::handle::HandleSubscribeRequest>,
    MockModelController,
) {
    let (model, ctrl) = model_source(None);
    let system = AgentSystemBuilder::new_local(
        InMemoryConversationStore::new(),
        InMemoryStateStore::new(),
        model,
    )
    .build_local();
    (system.start_with_handles(), ctrl)
}

/// Drain a handle's events until the completion finishes, returning the text
/// chunks seen along the way.
async fn handle_texts_until_finished(handle: &mut super::handle::ThreadHandle) -> Vec<String> {
    let mut texts = Vec::new();
    loop {
        let event = tokio::time::timeout(std::time::Duration::from_secs(5), handle.recv())
            .await
            .expect("timed out waiting for a handle event")
            .expect("handle event channel closed");
        match event {
            AgentEvent::TextChunk { text } => texts.push(text),
            AgentEvent::CompletionFinished { .. } => break,
            _ => {}
        }
    }
    texts
}

#[tokio::test(flavor = "current_thread")]
async fn thread_handle_sends_inputs_and_receives_events() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (mut running, mut ctrl) = start_handle_system();

            let mut handle = running
                .thread_handle("t1")
                .await
                .expect("system is running");
            assert_eq!(handle.thread_id(), "t1");
            assert!(handle.replay().history.is_empty(), "fresh thread");
            assert!(!handle.replay().in_progress);

            handle.send_user_text("hello").await.expect("send input");
            let _req = ctrl.next_request().await;
            ctrl.send_text("hi there");
            ctrl.finish();
            assert_eq!(handle_texts_until_finished(&mut handle).await, ["hi there"]);

            // A handle attached after the exchange replays the history.
            wait_idle(&mut running).await;
            let late = running
                .thread_handle("t1")
                .await
                .expect("system is running");
            assert!(
                !late.replay().history.is_empty(),
                "late handle sees committed history"
            );
            assert!(!late.replay().in_progress);
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn thread_handle_survives_driver_respawn() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (mut running, mut ctrl) = start_handle_system();

            // Attaching to a thread that does not exist yet sets up the
            // subscription; the driver spawned for the attach idles right
            // back out.
            let mut handle = running
                .thread_handle("fresh")
                .await
                .expect("system is running");
            assert!(handle.replay().history.is_empty());
            wait_idle(&mut running).await;
            assert!(running.is_idle(), "attach alone does not keep a driver");

            // The first message respawns the driver; the handle still
            // receives every event.
            handle.send_user_text("hello").await.expect("send input");
            let _req = ctrl.next_request().await;
            ctrl.send_text("first");
            ctrl.finish();
            assert_eq!(handle_texts_until_finished(&mut handle).await, ["first"]);

            // And again across another idle/respawn cycle.
            wait_idle(&mut running).await;
            handle.send_user_text("more").await.expect("send input");
            let _req = ctrl.next_request().await;
            ctrl.send_text("second");
            ctrl.finish();
            assert_eq!(handle_texts_until_finished(&mut handle).await, ["second"]);
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn dropped_thread_handle_is_pruned_and_others_keep_receiving() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (running, mut ctrl) = start_handle_system();

            let dropped = running
                .thread_handle("t1")
                .await
                .expect("system is running");
            let mut kept = running
                .thread_handle("t1")
                .await
                .expect("system is running");
            drop(dropped);

            kept.send_user_text("hello").await.expect("send input");
            let _req = ctrl.next_request().await;
            ctrl.send_text("still here");
            ctrl.finish();
            assert_eq!(handle_texts_until_finished(&mut kept).await, ["still here"]);
        })
        .await;
}
