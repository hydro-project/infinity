//! Shared batch input processing and completion logic.
//!
//! [`process_batch`] processes an iterator of input messages, emits
//! [`DisplayEvent`]s, and when any input is actionable returns a pinned
//! completion future the caller can store (CLI) or immediately `.await`
//! (Lambda).

use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::pin::Pin;

use futures_util::StreamExt;
use rig::completion::{GetTokenUsage, ToolDefinition};
use rig::message::{AssistantContent, Message, ToolResultContent, UserContent};
use tokio::sync::{mpsc, oneshot};

use crate::event_processor::{self, CompletionAction, HistoryManager};
use crate::message::{InputMessage, InputMessageContent, SyntheticKind, TaggedSyntheticKind};
use crate::model_provider::{ModelProvider, ProviderStreamingResponse};
use crate::tools::{Tool, ToolContext};
use crate::traits::{ConversationStore, InputSender, StateStore};
use rap_client::http::HttpClient;
use rap_client::notifier::RapNotifier;

/// Events emitted during batch processing and completion for display purposes.
///
/// Generic over `R`, the model's streaming response type (used only in
/// [`ResponseDone`](DisplayEvent::ResponseDone) to carry usage information).
pub enum DisplayEvent<R> {
    StartOutput,
    TextChunk {
        chunk: String,
    },
    ToolCall {
        name: String,
        args: serde_json::Value,
        display_as: Option<String>,
    },
    ToolResult {
        /// Prioritized display segments. Clients should use the first type they
        /// support. The raw tool output is always included as a trailing `Text`
        /// segment so every client has a fallback.
        segments: Vec<rap_protocol::DisplaySegment>,
    },
    Info(String),
    ResponseDone(Option<R>),
    UserInput(String),
    SubscriptionEvent {
        name: String,
        text: String,
    },
    OAuthRequired {
        auth_url: String,
    },
    UserChoiceRequired {
        id: String,
        prompt: String,
        choices: Vec<String>,
        default: usize,
        response_url: String,
    },
    UserChoiceComplete {
        choice_id: String,
    },
    ThinkingStart,
    ThinkingEnd,
    ThinkingChunk {
        chunk: String,
    },
    CompactionApplied,
}

/// Process a single input message: run prepare_input and emit display events.
/// Returns `Some(message_id)` if the item is ready for completion, `None` otherwise.
async fn process_input_item<C, S, R, M>(
    input_msg: InputMessage,
    message_id: String,
    current_history: &HistoryManager<C, S>,
    conversation_store: &C,
    display_tx: &mpsc::UnboundedSender<DisplayEvent<R>>,
    message_sender: &M,
) -> Option<String>
where
    C: ConversationStore,
    S: StateStore,
    M: InputSender,
{
    let prepare_result = event_processor::prepare_input(
        input_msg.clone(),
        message_id.clone(),
        current_history,
        conversation_store,
        message_sender,
    )
    .await;

    match prepare_result {
        Ok(event_processor::PrepareResult::Handled) => None,
        Ok(event_processor::PrepareResult::CompactionApplied) => {
            let _ = display_tx.send(DisplayEvent::CompactionApplied);
            None
        }
        Ok(event_processor::PrepareResult::OAuthRequired { auth_url }) => {
            let _ = display_tx.send(DisplayEvent::OAuthRequired { auth_url });
            None
        }
        Ok(event_processor::PrepareResult::UserChoiceRequired {
            id,
            prompt,
            choices,
            default,
            response_url,
        }) => {
            let _ = display_tx.send(DisplayEvent::UserChoiceRequired {
                id,
                prompt,
                choices,
                default,
                response_url,
            });
            None
        }
        Err(e) => {
            let _ = display_tx.send(DisplayEvent::Info(format!("Error: {}", e)));
            None
        }
        Ok(event_processor::PrepareResult::Ready) => {
            // Echo to the terminal — subscription events vs user input.
            if let Some(synth) = input_msg.synthetic.as_ref() {
                if let InputMessageContent::User(UserContent::ToolResult(res)) = &input_msg.content
                    && let ToolResultContent::Text(text) = res.content.first()
                {
                    let orig_call = current_history.get_history().into_iter().find(|h| {
                        if let Message::Assistant { content, .. } = h
                            && let AssistantContent::ToolCall(c) = content.first()
                        {
                            c.id == synth.tool_call_id()
                        } else {
                            false
                        }
                    });

                    if let Some(h) = orig_call
                        && let Message::Assistant { content, .. } = h
                        && let AssistantContent::ToolCall(c) = content.first()
                    {
                        let name =
                            if let SyntheticKind::Tagged(TaggedSyntheticKind::ThreadReport {
                                ref child_thread_id,
                                ..
                            }) = *synth
                            {
                                format!("Report from child thread {}", child_thread_id)
                            } else {
                                format!("{}({})", c.function.name, c.function.arguments)
                            };
                        let _ = display_tx.send(DisplayEvent::SubscriptionEvent {
                            name,
                            text: text.text,
                        });
                    }
                }
            } else if let InputMessageContent::User(UserContent::ToolResult(res)) =
                &input_msg.content
                && let ToolResultContent::Text(text) = res.content.first()
            {
                let _ = display_tx.send(DisplayEvent::ToolResult {
                    segments: rap_protocol::build_display_segments(
                        input_msg.display_as.as_deref(),
                        &text.text,
                    ),
                });
            } else if let InputMessageContent::User(UserContent::Text(ref text)) = input_msg.content
            {
                let display_text = text.text.strip_prefix("<interrupt>").unwrap_or(&text.text);
                let _ = display_tx.send(DisplayEvent::UserInput(display_text.to_owned()));
            }

            Some(message_id)
        }
    }
}

/// Process a batch of input messages and, if any are actionable, build a
/// completion future.
///
/// Returns `Some((future, cancel_sender))` when at least one input was
/// [`PrepareResult::Ready`](event_processor::PrepareResult::Ready). The
/// caller should store the future (CLI) or immediately `.await` it (Lambda).
/// The `cancel_sender` can be used to abort the completion early.
///
/// Returns `None` when no inputs were actionable (all handled/skipped).
#[expect(clippy::too_many_arguments, reason = "shared entry point")]
pub async fn process_batch<'a: 'b, 'b, P, C, S, M, H>(
    inputs: impl Iterator<Item = (InputMessage, String)>,
    current_history: &'a HistoryManager<C, S>,
    conversation_store: &'a C,
    display_tx: &'a mpsc::UnboundedSender<DisplayEvent<ProviderStreamingResponse>>,
    active_group_id: &'a str,
    provider: &'a P,
    model_id: &'a str,
    tool_names: &'a HashSet<String>,
    tool_defs: &'a [ToolDefinition],
    tool_registry: &'a HashMap<String, &'a dyn Tool<M>>,
    tool_context: ToolContext<M>,
    extra_system_prompt: &'a Option<String>,
    rap_notifier: Option<&'a RapNotifier<H>>,
    input_tokens_out: Option<&'a Cell<u64>>,
) -> Option<(Pin<Box<dyn Future<Output = ()> + 'b>>, oneshot::Sender<()>)>
where
    P: ModelProvider + ?Sized,
    C: ConversationStore,
    S: StateStore,
    M: InputSender + 'static,
    H: HttpClient,
{
    let mut any_ready = false;
    let mut last_message_id = String::new();

    for (input_msg, message_id) in inputs {
        tracing::trace!(
            "Processing input {:?} in thread {}",
            &input_msg,
            active_group_id
        );
        if let Some(mid) = process_input_item(
            input_msg,
            message_id,
            current_history,
            conversation_store,
            display_tx,
            &tool_context.message_sender,
        )
        .await
        {
            any_ready = true;
            last_message_id = mid;
        }
    }

    // Best-effort: notify RAP tool servers about interrupted tool calls
    // and dismiss any associated pending user choices.
    {
        let interrupted = current_history.take_interrupted_tool_calls();
        if !interrupted.is_empty() {
            if let Some(notifier) = rap_notifier {
                for call_id in &interrupted {
                    notifier
                        .notify_tool_cancelled(active_group_id, call_id)
                        .await;
                }
            }
            for call_id in &interrupted {
                let _ = display_tx.send(DisplayEvent::UserChoiceComplete {
                    choice_id: call_id.clone(),
                });
            }
        }
    }

    if !any_ready {
        return None;
    }

    let (cancel_tx, cancel_rx) = oneshot::channel();

    let active_thread_id = current_history.thread_id.clone();
    let completion_message_id = last_message_id;

    let fut = Box::pin(async move {
        let action = {
            let mut stream = std::pin::pin!(event_processor::run_completion(
                provider,
                model_id,
                current_history,
                tool_names,
                tool_defs,
                tool_registry,
                &tool_context,
                &active_thread_id,
                &completion_message_id,
                extra_system_prompt.as_deref(),
                cancel_rx,
            ));

            let _ = display_tx.send(DisplayEvent::StartOutput);

            let mut action = None;
            let mut resp = None;

            while let Some(ev) = stream.next().await {
                match ev {
                    Ok(event_processor::CompletionEvent::Info(info)) => {
                        let _ = display_tx.send(DisplayEvent::Info(info));
                    }
                    Ok(event_processor::CompletionEvent::TextChunk(chunk)) => {
                        let _ = display_tx.send(DisplayEvent::TextChunk { chunk });
                    }
                    Ok(event_processor::CompletionEvent::ThinkingStart) => {
                        let _ = display_tx.send(DisplayEvent::ThinkingStart);
                    }
                    Ok(event_processor::CompletionEvent::ThinkingEnd) => {
                        let _ = display_tx.send(DisplayEvent::ThinkingEnd);
                    }
                    Ok(event_processor::CompletionEvent::ThinkingChunk(chunk)) => {
                        let _ = display_tx.send(DisplayEvent::ThinkingChunk { chunk });
                    }
                    Ok(event_processor::CompletionEvent::SyncToolResult(res)) => {
                        if let ToolResultContent::Text(text) = res.content.first() {
                            let _ = display_tx.send(DisplayEvent::ToolResult {
                                segments: vec![rap_protocol::DisplaySegment::Text(
                                    text.text.clone(),
                                )],
                            });
                        }
                    }
                    Ok(event_processor::CompletionEvent::Action(CompletionAction::Done(r))) => {
                        // there may be multiple `Done` if the agent synchronously loops back
                        if let Some(input_tokens_out) = input_tokens_out
                            && let Some(usage) = r.token_usage()
                        {
                            input_tokens_out.set(usage.input_tokens);
                        }
                        resp = Some(r);
                    }
                    Ok(event_processor::CompletionEvent::SyncToolCall {
                        ref tool_name,
                        ref tool_args,
                        ref display_as,
                    }) => {
                        let _ = display_tx.send(DisplayEvent::ToolCall {
                            name: tool_name.clone(),
                            args: tool_args.clone(),
                            display_as: display_as.clone(),
                        });
                    }
                    Ok(event_processor::CompletionEvent::Action(a)) => {
                        if let CompletionAction::ExecuteToolCall {
                            ref tool_name,
                            ref tool_args,
                            ref display_as,
                            ..
                        } = a
                        {
                            let _ = display_tx.send(DisplayEvent::ToolCall {
                                name: tool_name.clone(),
                                args: tool_args.clone(),
                                display_as: display_as.clone(),
                            });
                        }

                        assert!(action.is_none());
                        action = Some(a);
                    }
                    Err(e) => {
                        let _ = display_tx.send(DisplayEvent::Info(format!("Error: {}", e)));
                        break;
                    }
                }
            }

            let _ = display_tx.send(DisplayEvent::ResponseDone(resp));

            action
        };

        current_history.sync().await.ok();

        if let Some(action) = action
            && let Err(e) =
                event_processor::execute_action(action, tool_registry, &tool_context).await
        {
            let _ = display_tx.send(DisplayEvent::Info(format!("Error: {}", e)));
        }
    });

    Some((fut, cancel_tx))
}

#[cfg(test)]
mod tests {

    use std::collections::{HashMap, HashSet};

    use super::DisplayEvent;
    use crate::event_processor::HistoryManager;
    use crate::message::{InputMessage, InputMessageContent, OAuthRequired, UserChoiceRequired};
    use crate::model_provider::ProviderStreamingResponse;
    use crate::test_helpers::mock_provider;
    use crate::tools::{Tool, ToolContext};
    use crate::traits::{ConversationStore, InputSender, StateStore};
    use async_trait::async_trait;
    use rap_client::http::HttpClient;
    use rig::OneOrMany;
    use rig::completion::ToolDefinition;
    use rig::message::{Message, ToolResult, ToolResultContent, UserContent};
    use tokio::sync::mpsc;

    #[derive(Debug)]
    struct E;
    impl std::fmt::Display for E {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "stub")
        }
    }
    impl std::error::Error for E {}

    #[derive(Clone)]
    struct StubSender;
    #[async_trait]
    impl InputSender for StubSender {
        type Error = E;
        async fn send_to_input_queue(&self, _: InputMessage, _: &str, _: &str) -> Result<(), E> {
            Ok(())
        }
    }

    #[derive(Clone)]
    struct StubConvo {
        closed: HashSet<String>,
    }
    impl StubConvo {
        fn new() -> Self {
            Self {
                closed: HashSet::new(),
            }
        }
    }
    #[async_trait]
    impl ConversationStore for StubConvo {
        type Error = E;
        async fn ensure_root_thread(&self, _: &str) -> Result<(), E> {
            Ok(())
        }
        async fn load_history_up_to(
            &self,
            _: &str,
            _: Option<i64>,
            _: Option<i64>,
        ) -> Result<Vec<crate::message::InfinityMessage>, E> {
            Ok(vec![])
        }
        async fn append_messages(
            &self,
            _: &str,
            _: Vec<(crate::message::InfinityMessage, String)>,
        ) -> Result<(), E> {
            Ok(())
        }
        async fn spawn_thread(
            &self,
            _: &str,
            _: &str,
            _: bool,
            _: Option<usize>,
        ) -> Result<String, E> {
            Ok("child".into())
        }
        async fn is_thread_closed(&self, id: &str) -> Result<bool, E> {
            Ok(self.closed.contains(id))
        }
        async fn close_thread(&self, _: &str) -> Result<(), E> {
            Ok(())
        }
        async fn is_subscription_event_thread(&self, _: &str) -> Result<bool, E> {
            Ok(false)
        }
        async fn get_thread_parent_info(&self, _: &str) -> Result<Option<(String, String)>, E> {
            Ok(None)
        }
        async fn get_ancestor_chain(&self, _: &str) -> Result<Vec<(String, i64)>, E> {
            Ok(vec![])
        }
        async fn mark_thread_as_compaction(&self, _: &str) -> Result<(), E> {
            Ok(())
        }
        async fn is_compaction_thread(&self, _: &str) -> Result<bool, E> {
            Ok(false)
        }
        async fn get_thread_spawn_order(&self, _: &str) -> Result<Option<i64>, E> {
            Ok(None)
        }
        async fn save_compaction_summary(&self, _: &str, _: &str, _: i64) -> Result<(), E> {
            Ok(())
        }
        async fn load_latest_compaction_summary_up_to(
            &self,
            _: &str,
            _: Option<i64>,
        ) -> Result<Option<(String, i64)>, E> {
            Ok(None)
        }
    }

    #[derive(Clone)]
    struct StubState;
    #[async_trait]
    impl StateStore for StubState {
        type Error = E;
        async fn get_processed_ids(
            &self,
            _: &str,
        ) -> Result<(HashSet<String>, HashSet<String>), E> {
            Ok((HashSet::new(), HashSet::new()))
        }
        async fn add_processed_message_ids(&self, _: &str, _: Vec<String>) -> Result<(), E> {
            Ok(())
        }
        async fn add_processed_tool_calls(&self, _: &str, _: Vec<String>) -> Result<(), E> {
            Ok(())
        }
        async fn get_metadata(&self, _: &str) -> Result<Option<serde_json::Value>, E> {
            Ok(None)
        }
        async fn set_metadata(&self, _: &str, _: serde_json::Value) -> Result<(), E> {
            Ok(())
        }
        async fn get_active_subscriptions(&self, _: &str) -> Result<Vec<String>, E> {
            Ok(vec![])
        }
        async fn add_active_subscription(&self, _: &str, _: &str) -> Result<(), E> {
            Ok(())
        }
        async fn remove_active_subscription(&self, _: &str, _: &str) -> Result<(), E> {
            Ok(())
        }
    }

    #[derive(Clone)]
    struct StubHttp;
    #[async_trait]
    impl HttpClient for StubHttp {
        type Error = E;
        async fn post(&self, _: &str, _: &str) -> Result<u16, E> {
            Ok(200)
        }
        async fn get(&self, _: &str) -> Result<(u16, Vec<u8>), E> {
            Ok((200, vec![]))
        }
    }

    fn ctx() -> ToolContext<StubSender> {
        ToolContext {
            message_sender: StubSender,
            group_id: "t1".into(),
            input_queue_arn: String::new(),
            callback_url: String::new(),
            user_id: None,
            thread_stack: vec!["t1".into()],
        }
    }

    fn user_input(text: &str) -> (InputMessage, String) {
        (
            InputMessage {
                content: InputMessageContent::User(UserContent::text(text)),
                group_id: "t1".into(),
                metadata: None,
                synthetic: None,
                display_as: None,
                subscription: false,
            },
            uuid::Uuid::new_v4().to_string(),
        )
    }

    fn drain(
        rx: &mut mpsc::UnboundedReceiver<DisplayEvent<ProviderStreamingResponse>>,
    ) -> Vec<String> {
        let mut out = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            out.push(match ev {
                DisplayEvent::StartOutput => "StartOutput".into(),
                DisplayEvent::TextChunk { chunk, .. } => format!("Text:{chunk}"),
                DisplayEvent::ToolCall { name, .. } => format!("ToolCall:{name}"),
                DisplayEvent::ToolResult { ref segments, .. } => {
                    let text = segments
                        .iter()
                        .find_map(|s| match s {
                            rap_protocol::DisplaySegment::Text(t) => Some(t.as_str()),
                            _ => None,
                        })
                        .unwrap_or("");
                    format!("ToolResult:{}", &text[..text.len().min(40)])
                }
                DisplayEvent::Info(s) => format!("Info:{}", &s[..s.len().min(40)]),
                DisplayEvent::ResponseDone(_) => "Done".into(),
                DisplayEvent::UserInput(s) => format!("UserInput:{s}"),
                DisplayEvent::OAuthRequired { auth_url } => format!("OAuth:{auth_url}"),
                DisplayEvent::UserChoiceRequired { id, .. } => format!("Choice:{id}"),
                DisplayEvent::SubscriptionEvent { name, .. } => format!("SubEvent:{name}"),
                _ => "Other".into(),
            });
        }
        out
    }

    const NONE_NOTIFIER: Option<&'static rap_client::notifier::RapNotifier<StubHttp>> = None;

    #[tokio::test]
    async fn closed_thread_returns_none() {
        let s = StubConvo {
            closed: HashSet::from(["t1".into()]),
        };
        let hm = HistoryManager::new_with_history(s.clone(), StubState, "t1".into())
            .await
            .expect("create history manager");

        let (provider, _ctrl) = mock_provider();
        let (dtx, _) = mpsc::unbounded_channel();
        let tn = HashSet::new();
        let td: Vec<ToolDefinition> = vec![];
        let tr: HashMap<String, &dyn Tool<StubSender>> = HashMap::new();
        let r = super::process_batch(
            vec![user_input("hi")].into_iter(),
            &hm,
            &s,
            &dtx,
            "t1",
            &provider,
            "mock",
            &tn,
            &td,
            &tr,
            ctx(),
            &None,
            NONE_NOTIFIER,
            None,
        )
        .await;
        assert!(r.is_none());
    }

    #[tokio::test]
    async fn oauth_returns_none_emits_event() {
        let s = StubConvo::new();
        let hm = HistoryManager::new_with_history(s.clone(), StubState, "t1".into())
            .await
            .expect("create history manager");

        let (provider, _ctrl) = mock_provider();
        let (dtx, mut drx) = mpsc::unbounded_channel();
        let tn = HashSet::new();
        let td: Vec<ToolDefinition> = vec![];
        let tr: HashMap<String, &dyn Tool<StubSender>> = HashMap::new();
        let input = (
            InputMessage {
                content: InputMessageContent::OAuth(OAuthRequired {
                    content_type: "oauth_required".into(),
                    id: "o1".into(),
                    call_id: None,
                    auth_url: "https://a.com".into(),
                }),
                group_id: "t1".into(),
                metadata: None,
                synthetic: None,
                display_as: None,
                subscription: false,
            },
            "m1".into(),
        );
        let r = super::process_batch(
            vec![input].into_iter(),
            &hm,
            &s,
            &dtx,
            "t1",
            &provider,
            "mock",
            &tn,
            &td,
            &tr,
            ctx(),
            &None,
            NONE_NOTIFIER,
            None,
        )
        .await;
        assert!(r.is_none());
        assert!(drain(&mut drx).iter().any(|e| e.starts_with("OAuth:")));
    }

    #[tokio::test]
    async fn user_choice_returns_none_emits_event() {
        let s = StubConvo::new();
        let hm = HistoryManager::new_with_history(s.clone(), StubState, "t1".into())
            .await
            .expect("create history manager");

        let (provider, _ctrl) = mock_provider();
        let (dtx, mut drx) = mpsc::unbounded_channel();
        let tn = HashSet::new();
        let td: Vec<ToolDefinition> = vec![];
        let tr: HashMap<String, &dyn Tool<StubSender>> = HashMap::new();
        let input = (
            InputMessage {
                content: InputMessageContent::UserChoice(UserChoiceRequired {
                    content_type: "user_choice_required".into(),
                    id: "c1".into(),
                    call_id: None,
                    prompt: "P".into(),
                    choices: vec!["A".into()],
                    default: 0,
                    response_url: "http://x".into(),
                }),
                group_id: "t1".into(),
                metadata: None,
                synthetic: None,
                display_as: None,
                subscription: false,
            },
            "m1".into(),
        );
        let r = super::process_batch(
            vec![input].into_iter(),
            &hm,
            &s,
            &dtx,
            "t1",
            &provider,
            "mock",
            &tn,
            &td,
            &tr,
            ctx(),
            &None,
            NONE_NOTIFIER,
            None,
        )
        .await;
        assert!(r.is_none());
        assert!(drain(&mut drx).iter().any(|e| e == "Choice:c1"));
    }

    #[tokio::test]
    async fn ready_input_returns_some() {
        let s = StubConvo::new();
        let hm = HistoryManager::new_with_history(s.clone(), StubState, "t1".into())
            .await
            .expect("create history manager");

        let (provider, _ctrl) = mock_provider();
        let (dtx, mut drx) = mpsc::unbounded_channel();
        let tn = HashSet::new();
        let td: Vec<ToolDefinition> = vec![];
        let tr: HashMap<String, &dyn Tool<StubSender>> = HashMap::new();
        let r = super::process_batch(
            vec![user_input("hi")].into_iter(),
            &hm,
            &s,
            &dtx,
            "t1",
            &provider,
            "mock",
            &tn,
            &td,
            &tr,
            ctx(),
            &None,
            NONE_NOTIFIER,
            None,
        )
        .await;
        assert!(r.is_some(), "Should return Some for actionable input");
        // Verify UserInput echo was emitted during input processing
        assert!(drain(&mut drx).contains(&"UserInput:hi".into()));
    }

    #[tokio::test]
    async fn tool_result_echoed_in_display() {
        let s = StubConvo::new();
        let hm = HistoryManager::new_with_history(s.clone(), StubState, "t1".into())
            .await
            .expect("create history manager");

        *hm.history.borrow_mut() = vec![
            crate::message::InfinityMessage::from_rig_message(Message::User {
                content: OneOrMany::one(UserContent::text("go")),
            }),
            crate::message::InfinityMessage::from_rig_message(Message::Assistant {
                id: None,
                content: OneOrMany::one(rig::message::AssistantContent::ToolCall(
                    rig::message::ToolCall {
                        id: "tc-1".into(),
                        call_id: None,
                        function: rig::message::ToolFunction {
                            name: "t".into(),
                            arguments: serde_json::json!({}),
                        },
                        additional_params: None,
                        signature: None,
                    },
                )),
            }),
        ];
        let (provider, _ctrl) = mock_provider();
        let (dtx, mut drx) = mpsc::unbounded_channel();
        let tn = HashSet::new();
        let td: Vec<ToolDefinition> = vec![];
        let tr: HashMap<String, &dyn Tool<StubSender>> = HashMap::new();
        let input = (
            InputMessage {
                content: InputMessageContent::User(UserContent::ToolResult(ToolResult {
                    id: "tc-1".into(),
                    call_id: None,
                    content: OneOrMany::one(ToolResultContent::Text(rig::agent::Text {
                        text: "tool output".into(),
                    })),
                })),
                group_id: "t1".into(),
                metadata: None,
                synthetic: None,
                display_as: None,
                subscription: false,
            },
            "m2".into(),
        );
        let r = super::process_batch(
            vec![input].into_iter(),
            &hm,
            &s,
            &dtx,
            "t1",
            &provider,
            "mock",
            &tn,
            &td,
            &tr,
            ctx(),
            &None,
            NONE_NOTIFIER,
            None,
        )
        .await;
        assert!(r.is_some());
        assert!(
            drain(&mut drx)
                .iter()
                .any(|e| e.starts_with("ToolResult:tool output"))
        );
    }

    #[tokio::test]
    async fn duplicate_input_returns_none() {
        let s = StubConvo::new();
        let hm = HistoryManager::new_with_history(s.clone(), StubState, "t1".into())
            .await
            .expect("create history manager");

        let (provider, _ctrl) = mock_provider();
        let (dtx, _) = mpsc::unbounded_channel();
        let tn = HashSet::new();
        let td: Vec<ToolDefinition> = vec![];
        let tr: HashMap<String, &dyn Tool<StubSender>> = HashMap::new();
        // First call — actionable
        let msg_id = "same-id".to_owned();
        let input1 = (
            InputMessage {
                content: InputMessageContent::User(UserContent::text("hi")),
                group_id: "t1".into(),
                metadata: None,
                synthetic: None,
                display_as: None,
                subscription: false,
            },
            msg_id.clone(),
        );
        let _ = super::process_batch(
            vec![input1].into_iter(),
            &hm,
            &s,
            &dtx,
            "t1",
            &provider,
            "mock",
            &tn,
            &td,
            &tr,
            ctx(),
            &None,
            NONE_NOTIFIER,
            None,
        )
        .await;
        // Second call with same message_id — should be handled (duplicate)
        let input2 = (
            InputMessage {
                content: InputMessageContent::User(UserContent::text("hi")),
                group_id: "t1".into(),
                metadata: None,
                synthetic: None,
                display_as: None,
                subscription: false,
            },
            msg_id,
        );
        let r = super::process_batch(
            vec![input2].into_iter(),
            &hm,
            &s,
            &dtx,
            "t1",
            &provider,
            "mock",
            &tn,
            &td,
            &tr,
            ctx(),
            &None,
            NONE_NOTIFIER,
            None,
        )
        .await;
        assert!(r.is_none(), "Duplicate message should return None");
    }

    #[tokio::test]
    async fn mixed_batch_only_actionable_triggers_completion() {
        let s = StubConvo::new();
        let hm = HistoryManager::new_with_history(s.clone(), StubState, "t1".into())
            .await
            .expect("create history manager");

        let (provider, _ctrl) = mock_provider();
        let (dtx, mut drx) = mpsc::unbounded_channel();
        let tn = HashSet::new();
        let td: Vec<ToolDefinition> = vec![];
        let tr: HashMap<String, &dyn Tool<StubSender>> = HashMap::new();
        // Mix: OAuth (handled) + user text (actionable)
        let oauth = (
            InputMessage {
                content: InputMessageContent::OAuth(OAuthRequired {
                    content_type: "oauth_required".into(),
                    id: "o1".into(),
                    call_id: None,
                    auth_url: "https://a.com".into(),
                }),
                group_id: "t1".into(),
                metadata: None,
                synthetic: None,
                display_as: None,
                subscription: false,
            },
            "m1".into(),
        );
        let text = user_input("hello");
        let r = super::process_batch(
            vec![oauth, text].into_iter(),
            &hm,
            &s,
            &dtx,
            "t1",
            &provider,
            "mock",
            &tn,
            &td,
            &tr,
            ctx(),
            &None,
            NONE_NOTIFIER,
            None,
        )
        .await;
        // Should return Some because the text input is actionable
        assert!(r.is_some());
        let ev = drain(&mut drx);
        // Both events should be emitted
        assert!(ev.iter().any(|e| e.starts_with("OAuth:")));
        assert!(ev.iter().any(|e| e == "UserInput:hello"));
    }
}
