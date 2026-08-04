//! Shared fixtures for system unit tests.

use std::rc::Rc;
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

use super::builder::{AgentSystemBuilder, NoRapHttp};
use super::config::{ThreadConfig, ThreadConfigSource};
use super::events::{AgentEvent, ReplaySnapshot};
use super::local::{ChannelSender, RunningSystem};
use super::model::StaticModel;
use super::observer::ThreadObserver;

// ── Test observer ──

/// Events seen by attached test clients: live agent events or replays.
#[derive(Debug, Clone)]
pub(crate) enum Evt {
    E(AgentEvent),
    Replay(ReplaySnapshot),
}

/// Broadcasts every event to a channel; replays are sent to the subscriber's
/// own channel (mirroring how a real embedding fans out to clients).
#[derive(Clone)]
pub(crate) struct TestObserver {
    pub(crate) tx: mpsc::UnboundedSender<Evt>,
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

pub(crate) fn model_source(ctrl_entry: Option<ModelEntry>) -> (StaticModel, MockModelController) {
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

pub(crate) fn user_text_input(group_id: &str, text: &str) -> (InputMessage, String) {
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

pub(crate) fn tool_result_input(group_id: &str, id: &str, text: &str) -> (InputMessage, String) {
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

pub(crate) fn subscription_event_input(
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
pub(crate) struct AsyncTool;

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

/// A tool whose dispatch always fails.
pub(crate) struct FailingTool;

#[async_trait]
impl Tool<ChannelSender> for FailingTool {
    fn name(&self) -> &str {
        "failing_tool"
    }
    fn description(&self) -> &str {
        "always fails"
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
        Err("boom".into())
    }
}

/// Fixed tool configuration used by system tests without exposing the
/// application-facing static builder conveniences.
pub(crate) struct TestThreadConfig {
    tools: Vec<Rc<dyn Tool<ChannelSender>>>,
}

impl TestThreadConfig {
    pub(crate) fn new(tools: Vec<Box<dyn Tool<ChannelSender>>>) -> Self {
        Self {
            tools: tools.into_iter().map(Rc::from).collect(),
        }
    }
}

#[async_trait(?Send)]
impl ThreadConfigSource<ChannelSender, NoRapHttp> for TestThreadConfig {
    async fn resolve(
        &self,
        _thread_id: &str,
    ) -> Result<ThreadConfig<ChannelSender, NoRapHttp>, Box<dyn std::error::Error + Send + Sync>>
    {
        Ok(ThreadConfig {
            tools: self.tools.clone(),
            extra_system_prompt: None,
            rap_notifier: None,
        })
    }
}

/// Start a local system with the given tools, returning the running handle,
/// the observer event stream, the model controller, and the store.
pub(crate) fn start_system(
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

pub(crate) fn start_system_with(
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
    let state = InMemoryStateStore::new();
    let mut builder = AgentSystemBuilder::new_local(conv.clone(), state, model)
        .thread_config(TestThreadConfig::new(tools));
    if !builtin_tools {
        builder = builder.without_builtin_tools();
    }
    let system = builder.build_local();
    let (tx, rx) = mpsc::unbounded_channel();
    let running = system.start_with_observer(move |_thread_id| TestObserver { tx: tx.clone() });
    (running, rx, ctrl, conv)
}

pub(crate) async fn next_evt(rx: &mut mpsc::UnboundedReceiver<Evt>) -> Evt {
    tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
        .await
        .expect("timed out waiting for event")
        .expect("event channel closed")
}

pub(crate) async fn collect_until_finished(rx: &mut mpsc::UnboundedReceiver<Evt>) -> Vec<String> {
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

/// Wait until all live drivers have exited (draining lifecycle
/// notifications until the active set is empty).
pub(crate) async fn wait_idle<Sub: Send + 'static>(running: &mut RunningSystem<Sub>) {
    while !running.is_idle() {
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            running.thread_lifecycle.recv(),
        )
        .await
        .expect("timed out waiting for a thread driver to exit")
        .expect("thread lifecycle channel closed");
    }
}

/// The tool-result texts in a completion request's chat history.
pub(crate) fn tool_result_texts(req: &rig::completion::CompletionRequest) -> Vec<String> {
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
pub(crate) fn is_compaction_req(req: &rig::completion::CompletionRequest) -> bool {
    tool_result_texts(req)
        .iter()
        .any(|t| t.contains("compaction thread"))
}

/// Extract the compaction child's thread id from its seed instruction.
pub(crate) fn find_compaction_child_id(req: &rig::completion::CompletionRequest) -> String {
    tool_result_texts(req)
        .iter()
        .find_map(|t| {
            let rest = t.split("close_thread with your thread ID (").nth(1)?;
            rest.split(')').next().map(str::to_owned)
        })
        .expect("compaction seed should include the child thread id")
}

/// Answer a compaction child's request by closing it with a summary report.
pub(crate) fn handle_compaction_child(
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

pub(crate) fn high_usage() -> Option<rig::completion::Usage> {
    Some(rig::completion::Usage {
        input_tokens: 76,
        output_tokens: 10,
        total_tokens: 86,
        cached_input_tokens: 0,
    })
}

pub(crate) fn small_context_entry() -> ModelEntry {
    ModelEntry {
        model_id: "mock".to_owned(),
        display_name: "mock".to_owned(),
        context_window: 100,
        max_output_tokens: None,
        supports_image_input: false,
    }
}

/// A tool that starts a subscription: its result is delivered through the
/// input queue with `subscription: true`.
pub(crate) struct SubscribeTool;
#[async_trait]
impl Tool<ChannelSender> for SubscribeTool {
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
            content: InputMessageContent::User(UserContent::ToolResult(rig::message::ToolResult {
                id: id.clone(),
                call_id,
                content: rig::OneOrMany::one(rig::message::ToolResultContent::Text(
                    rig::agent::Text {
                        text: "subscribed".into(),
                    },
                )),
            })),
            group_id: ctx.group_id.clone(),
            metadata: None,
            synthetic: None,
            display_as: None,
            subscription: true,
        };
        ctx.message_sender.send_to_input_queue(msg, &id).await?;
        Ok(())
    }
}
