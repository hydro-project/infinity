use std::collections::{HashMap, HashSet};

use futures_util::StreamExt;
use rig::{
    OneOrMany,
    completion::{CompletionModel, CompletionRequest, ToolDefinition},
    message::{AssistantContent, Message, ToolResult, ToolResultContent, UserContent},
    streaming::StreamedAssistantContent,
};
use serde::Serialize;
use tracing;

use crate::message::{InputMessage, InputMessageContent};
use crate::tools::{Tool, ToolContext};
use crate::traits::{ConversationStore, InputSender, StateStore};

// ── Public types ──

type BoxError = Box<dyn std::error::Error + Send + Sync>;

#[derive(Debug, Serialize)]
pub struct OutputMessage {
    pub text: String,
    pub metadata: serde_json::Value,
}

#[derive(Debug, Serialize)]
pub struct OAuthOutputMessage {
    #[serde(rename = "type")]
    pub message_type: String,
    pub auth_url: String,
    pub metadata: serde_json::Value,
}

/// The result of preparing an input message before sending it to the model.
#[derive(Debug, PartialEq, Serialize)]
pub enum PrepareResult {
    /// The input was processed and the history manager is ready for completion.
    Ready,
    /// The input was handled without needing a completion (e.g. duplicate, closed thread).
    Handled,
    /// An OAuth challenge must be forwarded to the user.
    OAuthRequired { auth_url: String },
}

/// What the model wants to do after a completion stream finishes.
pub enum CompletionAction<R> {
    /// Model produced text and is done (no tool call).
    Done(R),
    /// Model wants to execute a tool call. Under the RAP protocol tools are
    /// fire-and-forget: the agent loop stops after dispatching the call and
    /// the result arrives as a new input message later.
    ExecuteToolCall {
        tool_name: String,
        tool_args: serde_json::Value,
        tool_call_id: String,
        call_id: Option<String>,
    },
}

/// Items yielded by the completion stream.
pub enum CompletionEvent<R> {
    /// A chunk of text from the model.
    TextChunk(String),
    /// The terminal event — what to do next.
    Action(CompletionAction<R>),
    /// The model has started thinking (reasoning).
    ThinkingStart,
    /// The model has stopped thinking (reasoning).
    ThinkingEnd,
    /// A chunk of thinking/reasoning text from the model.
    ThinkingChunk(String),
    /// A synchronous tool result.
    SyncToolResult(ToolResult),
}

// ── HistoryManager (unchanged from before) ──

struct PendingItem {
    message: Message,
    message_id: String,
}

pub struct HistoryManager<C: ConversationStore, S: StateStore> {
    conversation_store: C,
    state_store: S,
    pub thread_id: String,
    pub root_thread_id: String,
    ancestor_chain: Vec<String>,
    pub history: Vec<Message>,
    processed_message_ids: HashSet<String>,
    processed_tool_calls: HashSet<String>,
    metadata: Option<serde_json::Value>,
    pending_items: Vec<PendingItem>,
    pending_complete_tool_calls: HashSet<String>,
    /// Tool call IDs that were interrupted by a new user message during
    /// `handle_content`. Callers can drain this via `take_interrupted_tool_calls`
    /// to send best-effort cancellation notifications to RAP tool servers.
    interrupted_tool_calls: Vec<String>,
}

impl<C: ConversationStore, S: StateStore> HistoryManager<C, S> {
    pub async fn new_with_history(
        conversation_store: C,
        state_store: S,
        thread_id: String,
    ) -> Result<Self, BoxError> {
        let _ = conversation_store.ensure_root_thread(&thread_id).await;

        let ancestor_chain: Vec<String> = conversation_store
            .get_ancestor_chain(&thread_id)
            .await
            .map(|links| links.iter().map(|(tid, _)| tid.clone()).collect())
            .unwrap_or_default();
        let root_thread_id = ancestor_chain
            .first()
            .cloned()
            .unwrap_or_else(|| thread_id.clone());

        let history = conversation_store
            .load_history_with_ancestors(&thread_id)
            .await
            .map_err(|e| Box::new(e) as BoxError)?;

        let metadata = state_store
            .get_metadata(&root_thread_id)
            .await
            .unwrap_or(None);

        let (processed_message_ids, processed_tool_calls) = state_store
            .get_processed_ids(&thread_id)
            .await
            .unwrap_or_else(|_| (HashSet::new(), HashSet::new()));

        Ok(Self {
            conversation_store,
            state_store,
            thread_id,
            root_thread_id,
            ancestor_chain,
            history,
            processed_message_ids,
            processed_tool_calls,
            metadata,
            pending_items: Vec::new(),
            pending_complete_tool_calls: HashSet::new(),
            interrupted_tool_calls: Vec::new(),
        })
    }

    pub async fn handle_content(
        &mut self,
        message: Message,
        message_id: String,
    ) -> Result<bool, BoxError> {
        if self.processed_message_ids.contains(&message_id) {
            tracing::info!("Message {} already processed, skipping", message_id);
            return Ok(false);
        }

        if let Message::User { content } = &message
            && let UserContent::ToolResult(ref tool_result) = content.first()
        {
            if self.processed_tool_calls.contains(tool_result.id.as_str()) {
                tracing::info!(
                    "Tool call {} already processed, ignoring duplicate",
                    tool_result.id
                );
                self.processed_message_ids.insert(message_id.clone());
                let _ = self
                    .state_store
                    .add_processed_message_ids(&self.thread_id, vec![message_id])
                    .await;
                return Ok(false);
            } else if !self.history.last().is_some_and(|l| {
                if let Message::Assistant { content, .. } = l
                    && let AssistantContent::ToolCall(c) = content.first()
                {
                    c.id == tool_result.id
                } else {
                    false
                }
            }) {
                tracing::info!(
                    "Got tool call result for wrong call, ignoring {}",
                    tool_result.id
                );
                return Ok(false);
            }
        } else if let Some(Message::Assistant { content, .. }) = self.history.last()
            && let AssistantContent::ToolCall(tool_call) = content.first()
            && !self.processed_tool_calls.contains(tool_call.id.as_str())
        {
            tracing::info!("Tool call {} interrupted by new user message", tool_call.id);
            self.interrupted_tool_calls.push(tool_call.id.clone());
            let synthetic_result = Message::User {
                content: OneOrMany::one(UserContent::ToolResult(rig::message::ToolResult {
                    id: tool_call.id.clone(),
                    call_id: tool_call.call_id.clone(),
                    content: OneOrMany::one(ToolResultContent::Text(rig::agent::Text {
                        text: "Tool call interrupted by user".to_string(),
                    })),
                })),
            };
            self.history.push(synthetic_result.clone());
            self.append_pending(synthetic_result, format!("{}-interrupted", tool_call.id));
            self.mark_tool_call_complete(tool_call.id.clone());
        }

        self.history.push(message.clone());
        self.append_pending(message, message_id.clone());
        self.processed_message_ids.insert(message_id);
        Ok(true)
    }

    pub fn handle_completion<R>(
        &mut self,
        completion: &StreamedAssistantContent<R>,
        completion_id: String,
    ) {
        if self.processed_message_ids.contains(&completion_id) {
            return;
        }
        let message = match completion {
            StreamedAssistantContent::Text(text) => Message::Assistant {
                id: None,
                content: OneOrMany::one(AssistantContent::Text(text.clone())),
            },
            StreamedAssistantContent::Reasoning(r) => Message::Assistant {
                id: None,
                content: OneOrMany::one(AssistantContent::Reasoning(r.clone())),
            },
            StreamedAssistantContent::ToolCall {
                tool_call: call, ..
            } => Message::Assistant {
                id: None,
                content: OneOrMany::one(AssistantContent::ToolCall(call.clone())),
            },
            StreamedAssistantContent::ToolCallDelta { .. }
            | StreamedAssistantContent::ReasoningDelta { .. }
            | StreamedAssistantContent::Final(_) => {
                return;
            }
        };
        self.history.push(message.clone());
        self.append_pending(message, completion_id);
    }

    fn append_pending(&mut self, message: Message, message_id: String) {
        if let Message::User { content } = &message
            && let UserContent::ToolResult(result) = content.first()
        {
            self.mark_tool_call_complete(result.id.clone());
        }
        self.pending_items.push(PendingItem {
            message,
            message_id,
        });
    }

    fn mark_tool_call_complete(&mut self, call_id: String) {
        self.processed_tool_calls.insert(call_id.clone());
        self.pending_complete_tool_calls.insert(call_id);
    }

    pub async fn sync(&mut self) -> Result<(), BoxError> {
        if self.pending_items.is_empty() && self.pending_complete_tool_calls.is_empty() {
            return Ok(());
        }
        if !self.pending_items.is_empty() {
            let msgs: Vec<(Message, String)> = self
                .pending_items
                .iter()
                .map(|item| (item.message.clone(), item.message_id.clone()))
                .collect();
            self.conversation_store
                .append_messages(&self.thread_id, msgs)
                .await
                .map_err(|e| Box::new(e) as BoxError)?;
        }
        let msg_ids: Vec<String> = self
            .pending_items
            .iter()
            .map(|i| i.message_id.clone())
            .collect();
        let tc_ids: Vec<String> = self.pending_complete_tool_calls.iter().cloned().collect();
        if !msg_ids.is_empty() {
            let _ = self
                .state_store
                .add_processed_message_ids(&self.thread_id, msg_ids)
                .await;
        }
        if !tc_ids.is_empty() {
            let _ = self
                .state_store
                .add_processed_tool_calls(&self.thread_id, tc_ids)
                .await;
        }
        self.pending_items.clear();
        self.pending_complete_tool_calls.clear();
        Ok(())
    }

    pub async fn update_metadata(&mut self, metadata: serde_json::Value) -> Result<(), BoxError> {
        self.metadata = Some(metadata.clone());
        self.state_store
            .set_metadata(&self.root_thread_id, metadata)
            .await
            .map_err(|e| Box::new(e) as BoxError)
    }

    pub fn get_metadata(&self) -> Option<serde_json::Value> {
        self.metadata.clone()
    }
    pub fn get_history(&self) -> OneOrMany<Message> {
        OneOrMany::many(self.history.clone()).unwrap()
    }

    pub fn remove_trailing_reasoning(&mut self) {
        while let Some(Message::Assistant { content, .. }) = self.history.last() {
            if matches!(content.first(), AssistantContent::Reasoning(_)) {
                self.history.pop();
            } else {
                break;
            }
        }
    }

    pub fn get_thread_nesting_prefix(&self) -> Option<String> {
        if self.ancestor_chain.is_empty() {
            return None;
        }
        let mut labels: Vec<String> = self
            .ancestor_chain
            .iter()
            .skip(1)
            .map(|id| {
                if id.len() > 8 {
                    id[..8].to_string()
                } else {
                    id.clone()
                }
            })
            .collect();
        let short = if self.thread_id.len() > 8 {
            &self.thread_id[..8]
        } else {
            &self.thread_id
        };
        labels.push(short.to_string());
        Some(format!("[{}]", labels.join(":")))
    }

    pub fn conversation_store(&self) -> &C {
        &self.conversation_store
    }
    pub fn state_store(&self) -> &S {
        &self.state_store
    }

    /// Drain and return tool call IDs that were interrupted by new user messages.
    /// Callers use this to send best-effort cancellation notifications to RAP
    /// tool servers so they can abort in-flight operations.
    pub fn take_interrupted_tool_calls(&mut self) -> Vec<String> {
        std::mem::take(&mut self.interrupted_tool_calls)
    }

    /// Record a subscription in the current thread's metadata. The
    /// `tool_call_id` is the ID of the tool call whose result had
    /// `subscription: true`. Ownership is implicit — a subscription is
    /// stored in the thread that created it.
    pub async fn track_subscription(&mut self, tool_call_id: &str) -> Result<(), BoxError> {
        self.state_store
            .add_active_subscription(&self.thread_id, tool_call_id)
            .await
            .map_err(|e| Box::new(e) as BoxError)
    }

    /// Mutate this HistoryManager in place to become a child thread.
    /// Keeps the current history and metadata (the child inherits the parent context),
    /// updates the thread_id and ancestor chain, and clears pending/processed state
    /// since the child is brand new. No store round-trip needed.
    pub fn fork_new(&mut self, sub_thread_id: String) {
        // Parent becomes part of the ancestor chain
        self.ancestor_chain.push(self.thread_id.clone());
        self.thread_id = sub_thread_id;
        self.processed_message_ids.clear();
        self.processed_tool_calls.clear();
        self.pending_items.clear();
        self.pending_complete_tool_calls.clear();
        self.interrupted_tool_calls.clear();
    }
}

// ═══════════════════════════════════════════════════════════════════════
// (a) prepare_input — process the raw InputMessage into history, handling
//     synthetics, subscription events, OAuth, dedup, closed threads.
// ═══════════════════════════════════════════════════════════════════════

pub async fn prepare_input<C, S>(
    input_msg: InputMessage,
    message_id: String,
    current_history: &mut HistoryManager<C, S>,
    conversation_store: &C,
) -> Result<PrepareResult, BoxError>
where
    C: ConversationStore,
    S: StateStore,
{
    // Skip messages for closed threads
    if conversation_store
        .is_thread_closed(&input_msg.group_id)
        .await
        .unwrap_or(false)
    {
        tracing::warn!(
            "Received message for closed thread {}, skipping",
            input_msg.group_id
        );
        return Ok(PrepareResult::Handled);
    }

    // Update metadata if provided
    if let Some(metadata) = input_msg.metadata {
        current_history.update_metadata(metadata).await?;
    }

    // Handle OAuth required messages — return to caller, don't add to history
    if let InputMessageContent::OAuth(oauth) = &input_msg.content {
        assert!(oauth.content_type == "oauth_required");
        tracing::info!("Received OAuth required message, returning to caller");
        return Ok(PrepareResult::OAuthRequired {
            auth_url: oauth.auth_url.clone(),
        });
    }

    let is_subscription = input_msg.subscription;

    let user_content = match input_msg.content {
        InputMessageContent::User(content) => content,
        InputMessageContent::OAuth(_) => return Ok(PrepareResult::Handled),
    };

    // Handle synthetic tool results (subscription events / thread reports)
    let content = if let Some(synthetic_kind) = input_msg.synthetic {
        let original_tool_call_id = synthetic_kind.tool_call_id().to_string();
        tracing::info!(
            "Processing synthetic tool result for tool call: {}",
            original_tool_call_id
        );

        let original_call = current_history.history.iter().find_map(|msg| {
            if let Message::Assistant { content, .. } = msg {
                content.iter().find_map(|c| {
                    if let AssistantContent::ToolCall(call) = c {
                        if call.id == original_tool_call_id {
                            Some(call.clone())
                        } else {
                            None
                        }
                    } else {
                        None
                    }
                })
            } else {
                None
            }
        });

        let Some(original_call) = original_call else {
            tracing::warn!(
                "Could not find original tool call for synthetic message: {}, dropping",
                original_tool_call_id
            );
            return Ok(PrepareResult::Handled);
        };

        if synthetic_kind.is_thread_report() || synthetic_kind.is_associative() {
            let new_tool_call_id = uuid::Uuid::new_v4().to_string();
            if let UserContent::ToolResult(mut tool_result) = user_content {
                let synthetic_tool_call = Message::Assistant {
                    id: None,
                    content: OneOrMany::one(AssistantContent::ToolCall(rig::message::ToolCall {
                        id: new_tool_call_id.clone(),
                        call_id: None,
                        function: rig::message::ToolFunction {
                            name: "receive_event__injected".to_string(),
                            arguments: serde_json::json!({
                                "original_tool_name": original_call.function.name,
                                "original_tool_call_id": original_tool_call_id,
                                "original_args": original_call.function.arguments,
                            }),
                        },
                        additional_params: None,
                        signature: None,
                    })),
                };
                current_history
                    .handle_content(
                        synthetic_tool_call,
                        format!("{}-synthetic-call", new_tool_call_id),
                    )
                    .await?;
                tool_result.id = new_tool_call_id;
                UserContent::ToolResult(tool_result)
            } else {
                return Err("Synthetic message is not a tool result".into());
            }
        } else {
            // Subscription events spawn a new subthread
            tracing::info!(
                "Spawning subthread for subscription event from tool call: {}",
                original_tool_call_id
            );

            let sub_thread_id = conversation_store
                .spawn_thread(&input_msg.group_id, &original_tool_call_id, true)
                .await
                .map_err(|e| Box::new(e) as BoxError)?;

            tracing::info!(
                "Created subthread {} for subscription event in parent {}",
                sub_thread_id,
                input_msg.group_id
            );

            current_history.fork_new(sub_thread_id.clone());

            let event_call_id = uuid::Uuid::new_v4().to_string();
            let spawn_call_id = uuid::Uuid::new_v4().to_string();

            let event_tool_call = Message::Assistant {
                id: None,
                content: OneOrMany::one(AssistantContent::ToolCall(rig::message::ToolCall {
                    id: event_call_id.clone(),
                    call_id: None,
                    function: rig::message::ToolFunction {
                        name: "receive_event__injected".to_string(),
                        arguments: serde_json::json!({
                            "original_tool_name": original_call.function.name,
                            "original_tool_call_id": original_tool_call_id,
                            "original_args": original_call.function.arguments,
                        }),
                    },
                    additional_params: None,
                    signature: None,
                })),
            };

            let event_content = if let UserContent::ToolResult(mut tool_result) = user_content {
                tool_result.id = event_call_id.clone();
                tool_result.call_id = None;
                tool_result
            } else {
                return Err("Synthetic subscription event is not a tool result".into());
            };

            current_history
                .handle_content(event_tool_call, format!("{}-event-call", event_call_id))
                .await?;
            current_history
                .handle_content(
                    Message::User {
                        content: OneOrMany::one(UserContent::ToolResult(event_content)),
                    },
                    format!("{}-event-result", event_call_id),
                )
                .await?;
            current_history
                .handle_content(
                    Message::Assistant {
                        id: None,
                        content: OneOrMany::one(AssistantContent::ToolCall(rig::message::ToolCall {
                            id: spawn_call_id.clone(),
                            call_id: None,
                            function: rig::message::ToolFunction {
                                name: "spawn_thread".to_string(),
                                arguments: serde_json::json!({
                                    "instructions": "Process the single subscription event above, report to the parent if appropriate, then close the thread after processing this event. Only your report will be visible to the parent."
                                }),
                            },
                            additional_params: None,
                            signature: None,
                        })),
                    },
                    format!("{}-spawn-call", spawn_call_id),
                )
                .await?;

            UserContent::ToolResult(ToolResult {
                id: spawn_call_id,
                call_id: None,
                content: OneOrMany::one(ToolResultContent::Text(rig::agent::Text {
                    text: format!(
                        "You are now INSIDE the thread for processing the single event above. Your thread ID is {}, the parent which is still subscribing is {}.",
                        sub_thread_id, input_msg.group_id
                    ),
                })),
            })
        }
    } else {
        user_content
    };

    // Capture tool call ID before `content` is moved, so we can track
    // subscriptions after the message is added to history.
    let subscription_tool_call_id = if is_subscription {
        match &content {
            UserContent::ToolResult(result) => Some(result.id.clone()),
            _ => None,
        }
    } else {
        None
    };

    let is_new = current_history
        .handle_content(
            Message::User {
                content: OneOrMany::one(content),
            },
            message_id.clone(),
        )
        .await?;

    if !is_new {
        tracing::info!("Message was duplicate or ignored, skipping agent processing");
        return Ok(PrepareResult::Handled);
    }

    // Track subscription if this tool result started one
    if let Some(ref tool_call_id) = subscription_tool_call_id {
        tracing::info!(
            "Tracking subscription {} in thread {}",
            tool_call_id,
            current_history.thread_id
        );
        current_history.track_subscription(tool_call_id).await?;
    }

    Ok(PrepareResult::Ready)
}

// ═══════════════════════════════════════════════════════════════════════
// (b) run_completion — yields CompletionEvent items (text chunks and a
//     terminal Action). Handles stream errors and unknown tools internally.
// ═══════════════════════════════════════════════════════════════════════

pub fn run_completion<'a, Mdl, C, S, M>(
    model: &'a Mdl,
    history: &'a mut HistoryManager<C, S>,
    tool_names: &'a HashSet<String>,
    tools: &'a [ToolDefinition],
    tool_registry: &'a HashMap<String, &'a dyn Tool<M>>,
    tool_context: &'a ToolContext<M>,
    group_id: &'a str,
    message_id: &'a str,
    extra_system_prompt: Option<&'a str>,
    additional_request_params: Option<&'a serde_json::Value>,
) -> impl futures_util::Stream<Item = Result<CompletionEvent<Mdl::StreamingResponse>, BoxError>> + 'a
where
    Mdl: CompletionModel,
    C: ConversationStore,
    S: StateStore,
    M: InputSender + 'static,
{
    async_stream::try_stream! {
        let mut completion_counter: usize = 0;
        let mut is_thinking = false;

        let preamble = match extra_system_prompt {
            Some(extra) => format!("{}\n\n{}", include_str!("default_prompt.md"), extra),
            None => include_str!("default_prompt.md").to_string(),
        };

        'outer: loop {
            let stream_result = model
                .stream(CompletionRequest {
                    model: None,
                    preamble: Some(preamble.clone()),
                    chat_history: history.get_history(),
                    documents: vec![],
                    tools: tools.to_vec(),
                    temperature: None,
                    max_tokens: None,
                    tool_choice: None,
                    additional_params: {
                        let mut base = serde_json::json!({
                            "thinking": {
                                "type": "adaptive"
                            }
                        });
                        if let Some(extra) = additional_request_params {
                            if let (Some(base_obj), Some(extra_obj)) = (base.as_object_mut(), extra.as_object()) {
                                for (k, v) in extra_obj {
                                    base_obj.insert(k.clone(), v.clone());
                                }
                            }
                        }
                        Some(base)
                    },
                    output_schema: None,
                })
                .await;

            let mut llm_stream = match stream_result {
                Ok(s) => s,
                Err(e) => {
                    Err(Into::<BoxError>::into(e))?;
                    unreachable!()
                }
            };

            loop {
                let res = match llm_stream.next().await {
                    Some(r) => r,
                    None => {
                        tracing::warn!("Stream ended unexpectedly, removing trailing reasoning and retrying...");
                        history.remove_trailing_reasoning();
                        continue 'outer;
                    }
                };

                let chunk = match res {
                    Ok(c) => c,
                    Err(e) => {
                        let err_str = format!("{}", e);
                        if err_str.contains("unexpected end of stream") {
                            tracing::warn!("Stream error (unexpected end), retrying...");
                            history.remove_trailing_reasoning();
                            continue 'outer;
                        }
                        Err(Into::<BoxError>::into(e))?;
                        unreachable!()
                    }
                };

                // Skip incomplete reasoning chunks
                if let StreamedAssistantContent::Reasoning(ref r) = chunk
                    && r.first_signature().is_none() { continue; }

                let completion_id = format!("{}-{}-completion-{}", group_id, message_id, completion_counter);
                completion_counter += 1;

                history.handle_completion(&chunk, completion_id);

                match chunk {
                    StreamedAssistantContent::Text(text) => {
                        if is_thinking {
                            is_thinking = false;
                            yield CompletionEvent::ThinkingEnd;
                        }
                        tracing::info!("[Text] {}", &text.text);
                        yield CompletionEvent::TextChunk(text.text);
                    }
                    StreamedAssistantContent::ToolCall { tool_call: call, .. } => {
                        if is_thinking {
                            is_thinking = false;
                            yield CompletionEvent::ThinkingEnd;
                        }
                        tracing::info!("[Tool Call: {} with arguments {}]", &call.function.name, &call.function.arguments);

                        // Unknown tool — inject error and retry the whole completion
                        if call.function.name == "receive_event__injected" {
                            let tool_result = Message::User {
                                content: OneOrMany::one(UserContent::ToolResult(rig::message::ToolResult {
                                    id: call.id.clone(),
                                    call_id: call.call_id.clone(),
                                    content: OneOrMany::one(ToolResultContent::Text(rig::agent::Text {
                                        text: format!("Error: you cannot directly invoke {}, invocations will automatically be injected when events arrive.", call.function.name),
                                    })),
                                })),
                            };
                            history.handle_content(tool_result, format!("{}-unknown-tool", call.id)).await?;
                            continue 'outer;
                        } else if !tool_names.contains(call.function.name.as_str()) {
                            tracing::warn!("Unknown tool '{}' called, injecting error and retrying", call.function.name);
                            let tool_result = Message::User {
                                content: OneOrMany::one(UserContent::ToolResult(rig::message::ToolResult {
                                    id: call.id.clone(),
                                    call_id: call.call_id.clone(),
                                    content: OneOrMany::one(ToolResultContent::Text(rig::agent::Text {
                                        text: format!("Error: tool '{}' does not exist", call.function.name),
                                    })),
                                })),
                            };
                            history.handle_content(tool_result, format!("{}-unknown-tool", call.id)).await?;
                            continue 'outer;
                        }

                        yield CompletionEvent::Action(CompletionAction::ExecuteToolCall {
                            tool_name: call.function.name.clone(),
                            tool_args: call.function.arguments.clone(),
                            tool_call_id: call.id.clone(),
                            call_id: call.call_id.clone(),
                        });

                        // Check for synchronous execution — if the tool provides
                        // synchronous results, inject into history immediately and
                        // continue the completion loop instead of returning. This
                        // prevents race conditions where a concurrent event makes
                        // the tool call appear cancelled.
                        if let Some(tool) = tool_registry.get(call.function.name.as_str()) && tool.supports_sync() {
                            history.sync().await?; // we must sync the history so that thread spawning uses the correct state

                            if let Some(res) = tool.execute_synchronous(
                                &call.function.arguments,
                                &call.id,
                                call.call_id.as_deref(),
                                tool_context,
                            ).await {
                                yield CompletionEvent::SyncToolResult(res.clone());

                                let sync_id = format!("{}-sync-result-{}", call.id, completion_counter);
                                completion_counter += 1;
                                history.handle_content(
                                    Message::User { content: OneOrMany::one(UserContent::ToolResult(res)) },
                                    sync_id,
                                ).await?;
                                continue 'outer;
                            }
                        }
                    }
                    StreamedAssistantContent::ToolCallDelta { .. } => {}
                    StreamedAssistantContent::Reasoning(reasoning) => {
                        if is_thinking {
                            is_thinking = false;
                            yield CompletionEvent::ThinkingEnd;
                        }
                        tracing::info!("[Reasoning: {:?}]", reasoning.first_text());
                    }
                    StreamedAssistantContent::ReasoningDelta { reasoning, .. } => {
                        if !is_thinking {
                            is_thinking = true;
                            yield CompletionEvent::ThinkingStart;
                        }
                        yield CompletionEvent::ThinkingChunk(reasoning);
                    }
                    StreamedAssistantContent::Final(r) => {
                        if is_thinking {
                            yield CompletionEvent::ThinkingEnd;
                        }
                        tracing::info!("Received final message");
                        yield CompletionEvent::Action(CompletionAction::Done(r));
                        return;
                    }
                }
            }
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════
// (c) execute_action — dispatch the CompletionAction (execute tool call
//     or emit output).
// ═══════════════════════════════════════════════════════════════════════

pub async fn execute_action<M, R>(
    action: CompletionAction<R>,
    tool_registry: &HashMap<String, &dyn Tool<M>>,
    tool_context: &ToolContext<M>,
) -> Result<(), BoxError>
where
    M: InputSender + 'static,
{
    match action {
        CompletionAction::Done(_) => {}
        CompletionAction::ExecuteToolCall {
            tool_name,
            tool_args,
            tool_call_id,
            call_id,
        } => {
            let tool = tool_registry
                .get(&tool_name)
                .expect("tool must exist after run_completion");
            tool.execute(tool_args, tool_call_id, call_id, tool_context)
                .await?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::{
        InputMessage, InputMessageContent, OAuthRequired, SyntheticKind, TaggedSyntheticKind,
    };
    use crate::traits::{ConversationStore, StateStore};
    use async_trait::async_trait;
    use rig::OneOrMany;
    use rig::message::{
        AssistantContent, Message, ToolCall, ToolFunction, ToolResult, ToolResultContent,
        UserContent,
    };
    use std::collections::HashSet;

    // ── Minimal error type ──

    #[derive(Debug)]
    struct TestError;
    impl std::fmt::Display for TestError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "test error")
        }
    }
    impl std::error::Error for TestError {}

    // ── No-op ConversationStore ──

    #[derive(Clone)]
    struct StubConversationStore {
        closed_threads: HashSet<String>,
    }

    impl StubConversationStore {
        fn new() -> Self {
            Self {
                closed_threads: HashSet::new(),
            }
        }
    }

    #[async_trait]
    impl ConversationStore for StubConversationStore {
        type Error = TestError;

        async fn ensure_root_thread(&self, _thread_id: &str) -> Result<(), TestError> {
            Ok(())
        }
        async fn load_history(&self, _session_id: &str) -> Result<Vec<Message>, TestError> {
            Ok(vec![])
        }
        async fn load_history_up_to(
            &self,
            _session_id: &str,
            _up_to_order: i64,
        ) -> Result<Vec<Message>, TestError> {
            Ok(vec![])
        }
        async fn load_history_with_ancestors(
            &self,
            _thread_id: &str,
        ) -> Result<Vec<Message>, TestError> {
            Ok(vec![])
        }
        async fn append_messages(
            &self,
            _session_id: &str,
            _messages: Vec<(Message, String)>,
        ) -> Result<(), TestError> {
            Ok(())
        }
        async fn spawn_thread(
            &self,
            _parent_thread_id: &str,
            _spawn_tool_call_id: &str,
            _is_for_subscription_event: bool,
        ) -> Result<String, TestError> {
            Ok("sub-thread-1".to_string())
        }
        async fn is_thread_closed(&self, thread_id: &str) -> Result<bool, TestError> {
            Ok(self.closed_threads.contains(thread_id))
        }
        async fn close_thread(&self, _thread_id: &str) -> Result<(), TestError> {
            Ok(())
        }
        async fn is_subscription_event_thread(&self, _thread_id: &str) -> Result<bool, TestError> {
            Ok(false)
        }
        async fn get_thread_parent_info(
            &self,
            _thread_id: &str,
        ) -> Result<Option<(String, String)>, TestError> {
            Ok(None)
        }
        async fn get_ancestor_chain(
            &self,
            _thread_id: &str,
        ) -> Result<Vec<(String, i64)>, TestError> {
            Ok(vec![])
        }
    }

    // ── No-op StateStore ──

    #[derive(Clone)]
    struct StubStateStore;

    #[async_trait]
    impl StateStore for StubStateStore {
        type Error = TestError;

        async fn get_processed_ids(
            &self,
            _thread_id: &str,
        ) -> Result<(HashSet<String>, HashSet<String>), TestError> {
            Ok((HashSet::new(), HashSet::new()))
        }
        async fn add_processed_message_ids(
            &self,
            _thread_id: &str,
            _message_ids: Vec<String>,
        ) -> Result<(), TestError> {
            Ok(())
        }
        async fn add_processed_tool_calls(
            &self,
            _thread_id: &str,
            _tool_call_ids: Vec<String>,
        ) -> Result<(), TestError> {
            Ok(())
        }
        async fn get_metadata(
            &self,
            _root_thread_id: &str,
        ) -> Result<Option<serde_json::Value>, TestError> {
            Ok(None)
        }
        async fn set_metadata(
            &self,
            _root_thread_id: &str,
            _metadata: serde_json::Value,
        ) -> Result<(), TestError> {
            Ok(())
        }
        async fn get_active_subscriptions(
            &self,
            _thread_id: &str,
        ) -> Result<Vec<String>, TestError> {
            Ok(vec![])
        }
        async fn add_active_subscription(
            &self,
            _thread_id: &str,
            _tool_call_id: &str,
        ) -> Result<(), TestError> {
            Ok(())
        }
        async fn remove_active_subscription(
            &self,
            _thread_id: &str,
            _tool_call_id: &str,
        ) -> Result<(), TestError> {
            Ok(())
        }
    }

    // ── Helpers ──

    async fn make_history(
        store: &StubConversationStore,
        initial_history: Vec<Message>,
    ) -> HistoryManager<StubConversationStore, StubStateStore> {
        let mut hm =
            HistoryManager::new_with_history(store.clone(), StubStateStore, "thread-1".to_string())
                .await
                .unwrap();
        hm.history = initial_history;
        hm
    }

    fn user_text_msg(group_id: &str, text: &str) -> InputMessage {
        InputMessage {
            content: InputMessageContent::User(UserContent::text(text)),
            group_id: group_id.to_string(),
            metadata: None,
            synthetic: None,
            display_as: None,
            subscription: false,
        }
    }

    fn tool_call_msg(id: &str, name: &str, args: serde_json::Value) -> Message {
        Message::Assistant {
            id: None,
            content: OneOrMany::one(AssistantContent::ToolCall(ToolCall {
                id: id.to_string(),
                call_id: None,
                function: ToolFunction {
                    name: name.to_string(),
                    arguments: args,
                },
                additional_params: None,
                signature: None,
            })),
        }
    }

    fn tool_result_input(
        group_id: &str,
        tool_call_id: &str,
        result_text: &str,
        synthetic: Option<SyntheticKind>,
    ) -> InputMessage {
        InputMessage {
            content: InputMessageContent::User(UserContent::ToolResult(ToolResult {
                id: tool_call_id.to_string(),
                call_id: None,
                content: OneOrMany::one(ToolResultContent::Text(rig::agent::Text {
                    text: result_text.to_string(),
                })),
            })),
            group_id: group_id.to_string(),
            metadata: None,
            synthetic,
            display_as: None,
            subscription: false,
        }
    }

    // ── Tests ──

    #[tokio::test]
    async fn simple_user_message_on_empty_history() {
        let store = StubConversationStore::new();
        let mut hm = make_history(&store, vec![]).await;

        let result = prepare_input(
            user_text_msg("thread-1", "hello"),
            "msg-1".to_string(),
            &mut hm,
            &store,
        )
        .await
        .unwrap();

        assert_eq!(result, PrepareResult::Ready);
        insta::assert_json_snapshot!(hm.history);
    }

    #[tokio::test]
    async fn closed_thread_ignores() {
        let store = StubConversationStore {
            closed_threads: HashSet::from(["thread-1".to_string()]),
        };
        let mut hm = make_history(&store, vec![]).await;

        let result = prepare_input(
            user_text_msg("thread-1", "hello"),
            "msg-1".to_string(),
            &mut hm,
            &store,
        )
        .await
        .unwrap();

        assert_eq!(result, PrepareResult::Handled);
        assert!(hm.history.is_empty());
    }

    #[tokio::test]
    async fn oauth_required_returns_auth_url() {
        let store = StubConversationStore::new();
        let mut hm = make_history(&store, vec![]).await;

        let input = InputMessage {
            content: InputMessageContent::OAuth(OAuthRequired {
                content_type: "oauth_required".to_string(),
                id: "oauth-1".to_string(),
                call_id: None,
                auth_url: "https://example.com/auth".to_string(),
            }),
            group_id: "thread-1".to_string(),
            metadata: None,
            synthetic: None,
            display_as: None,
            subscription: false,
        };

        let result = prepare_input(input, "msg-1".to_string(), &mut hm, &store)
            .await
            .unwrap();

        insta::assert_json_snapshot!(result);
        assert!(hm.history.is_empty());
    }

    #[tokio::test]
    async fn duplicate_message_returns_handled() {
        let store = StubConversationStore::new();
        let mut hm = make_history(&store, vec![]).await;

        // First call succeeds
        let r1 = prepare_input(
            user_text_msg("thread-1", "hello"),
            "msg-1".to_string(),
            &mut hm,
            &store,
        )
        .await
        .unwrap();
        assert!(matches!(r1, PrepareResult::Ready));

        // Same message_id again
        let r2 = prepare_input(
            user_text_msg("thread-1", "hello"),
            "msg-1".to_string(),
            &mut hm,
            &store,
        )
        .await
        .unwrap();

        assert_eq!(r2, PrepareResult::Handled);
        // History should still have only one user message
        insta::assert_json_snapshot!(hm.history);
    }

    #[tokio::test]
    async fn user_message_interrupts_pending_tool_call() {
        let store = StubConversationStore::new();
        // History has a user msg, then an assistant tool call that hasn't been answered
        let initial = vec![
            Message::User {
                content: OneOrMany::one(UserContent::text("do something")),
            },
            tool_call_msg("tc-1", "some_tool", serde_json::json!({"x": 1})),
        ];
        let mut hm = make_history(&store, initial).await;

        let result = prepare_input(
            user_text_msg("thread-1", "actually, never mind"),
            "msg-2".to_string(),
            &mut hm,
            &store,
        )
        .await
        .unwrap();

        assert_eq!(result, PrepareResult::Ready);
        // Should have: original user, tool call, synthetic interrupted result, new user msg
        insta::assert_json_snapshot!(hm.history);
    }

    #[tokio::test]
    async fn tool_result_appended_to_history() {
        let store = StubConversationStore::new();
        let initial = vec![
            Message::User {
                content: OneOrMany::one(UserContent::text("do something")),
            },
            tool_call_msg("tc-1", "some_tool", serde_json::json!({"x": 1})),
        ];
        let mut hm = make_history(&store, initial).await;

        let input = tool_result_input("thread-1", "tc-1", "tool output", None);

        let result = prepare_input(input, "msg-2".to_string(), &mut hm, &store)
            .await
            .unwrap();

        assert_eq!(result, PrepareResult::Ready);
        insta::assert_json_snapshot!(hm.history);
    }

    #[tokio::test]
    async fn thread_report_synthetic_event() {
        let store = StubConversationStore::new();
        // Tool call already completed before the thread report arrives
        let initial = vec![
            Message::User {
                content: OneOrMany::one(UserContent::text("subscribe")),
            },
            tool_call_msg(
                "tc-sub",
                "subscribe_tool",
                serde_json::json!({"topic": "events"}),
            ),
            Message::User {
                content: OneOrMany::one(UserContent::ToolResult(ToolResult {
                    id: "tc-sub".to_string(),
                    call_id: None,
                    content: OneOrMany::one(ToolResultContent::Text(rig::agent::Text {
                        text: "subscribed successfully".to_string(),
                    })),
                })),
            },
        ];
        let mut hm = make_history(&store, initial).await;
        hm.processed_tool_calls.insert("tc-sub".to_string());

        let input = tool_result_input(
            "thread-1",
            "tc-sub",
            "thread report data",
            Some(SyntheticKind::Tagged(TaggedSyntheticKind::ThreadReport {
                tool_call_id: "tc-sub".to_string(),
            })),
        );

        let result = prepare_input(input, "msg-2".to_string(), &mut hm, &store)
            .await
            .unwrap();

        assert_eq!(result, PrepareResult::Ready);
        // Should have: original user, original tool call, original result, synthetic tool call, synthetic result
        insta::assert_json_snapshot!(
            hm.history,
            { "[3].content[0].id" => "[uuid]", "[4].content[0].id" => "[uuid]" }
        );
    }

    #[tokio::test]
    async fn thread_report_tool_interruption() {
        let store = StubConversationStore::new();
        // Tool call is still pending when the thread report arrives
        let initial = vec![
            Message::User {
                content: OneOrMany::one(UserContent::text("subscribe")),
            },
            tool_call_msg(
                "tc-sub",
                "subscribe_tool",
                serde_json::json!({"topic": "events"}),
            ),
        ];
        let mut hm = make_history(&store, initial).await;

        let input = tool_result_input(
            "thread-1",
            "tc-sub",
            "thread report data",
            Some(SyntheticKind::Tagged(TaggedSyntheticKind::ThreadReport {
                tool_call_id: "tc-sub".to_string(),
            })),
        );

        let result = prepare_input(input, "msg-2".to_string(), &mut hm, &store)
            .await
            .unwrap();

        assert_eq!(result, PrepareResult::Ready);
        insta::assert_json_snapshot!(
            hm.history,
            { "[3].content[0].id" => "[uuid]", "[4].content[0].id" => "[uuid]" }
        );
    }

    #[tokio::test]
    async fn subscription_event_spawned_thread() {
        let store = StubConversationStore::new();
        // Tool call already completed with a result before the event arrives
        let initial = vec![
            Message::User {
                content: OneOrMany::one(UserContent::text("subscribe")),
            },
            tool_call_msg(
                "tc-sub",
                "subscribe_tool",
                serde_json::json!({"topic": "events"}),
            ),
            Message::User {
                content: OneOrMany::one(UserContent::ToolResult(ToolResult {
                    id: "tc-sub".to_string(),
                    call_id: None,
                    content: OneOrMany::one(ToolResultContent::Text(rig::agent::Text {
                        text: "subscribed successfully".to_string(),
                    })),
                })),
            },
        ];
        let mut hm = make_history(&store, initial).await;

        let input = tool_result_input(
            "thread-1",
            "tc-sub",
            "event payload",
            Some(SyntheticKind::Tagged(
                TaggedSyntheticKind::SubscriptionEvent {
                    tool_call_id: "tc-sub".to_string(),
                    associative: false,
                },
            )),
        );

        let result = prepare_input(input, "msg-2".to_string(), &mut hm, &store)
            .await
            .unwrap();

        assert_eq!(result, PrepareResult::Ready);
        assert_eq!(hm.thread_id, "sub-thread-1");
        insta::assert_json_snapshot!(
            hm.history,
            {
                "[3].content[0].id" => "[uuid]",
                "[4].content[0].id" => "[uuid]",
                "[5].content[0].id" => "[uuid]",
                "[6].content[0].id" => "[uuid]",
            }
        );
    }

    #[tokio::test]
    async fn subscription_event_tool_interruption() {
        let store = StubConversationStore::new();
        // Tool call is still pending (no result yet) when the event arrives
        let initial = vec![
            Message::User {
                content: OneOrMany::one(UserContent::text("subscribe")),
            },
            tool_call_msg(
                "tc-sub",
                "subscribe_tool",
                serde_json::json!({"topic": "events"}),
            ),
        ];
        let mut hm = make_history(&store, initial).await;

        let input = tool_result_input(
            "thread-1",
            "tc-sub",
            "event payload",
            Some(SyntheticKind::Tagged(
                TaggedSyntheticKind::SubscriptionEvent {
                    tool_call_id: "tc-sub".to_string(),
                    associative: false,
                },
            )),
        );

        let result = prepare_input(input, "msg-2".to_string(), &mut hm, &store)
            .await
            .unwrap();

        assert_eq!(result, PrepareResult::Ready);
        assert_eq!(hm.thread_id, "sub-thread-1");
        insta::assert_json_snapshot!(
            hm.history,
            {
                "[3].content[0].id" => "[uuid]",
                "[4].content[0].id" => "[uuid]",
                "[5].content[0].id" => "[uuid]",
                "[6].content[0].id" => "[uuid]",
            }
        );
    }

    #[tokio::test]
    async fn synthetic_with_missing_tool_call_returns_handled() {
        let store = StubConversationStore::new();
        // Empty history — no tool call to match
        let mut hm = make_history(&store, vec![]).await;

        let input = tool_result_input(
            "thread-1",
            "nonexistent-tc",
            "some data",
            Some(SyntheticKind::Tagged(
                TaggedSyntheticKind::SubscriptionEvent {
                    tool_call_id: "nonexistent-tc".to_string(),
                    associative: false,
                },
            )),
        );

        let result = prepare_input(input, "msg-1".to_string(), &mut hm, &store)
            .await
            .unwrap();

        assert_eq!(result, PrepareResult::Handled);
        assert!(hm.history.is_empty());
    }

    #[tokio::test]
    async fn metadata_is_updated_before_processing() {
        let store = StubConversationStore::new();
        let mut hm = make_history(&store, vec![]).await;
        assert!(hm.get_metadata().is_none());

        let input = InputMessage {
            content: InputMessageContent::User(UserContent::text("hi")),
            group_id: "thread-1".to_string(),
            metadata: Some(serde_json::json!({"user_id": "u-123"})),
            synthetic: None,
            display_as: None,
            subscription: false,
        };

        let _ = prepare_input(input, "msg-1".to_string(), &mut hm, &store)
            .await
            .unwrap();

        insta::assert_json_snapshot!(hm.get_metadata());
    }

    #[tokio::test]
    async fn associative_subscription_event_inlined() {
        let store = StubConversationStore::new();
        // Tool call already completed with a result before the associative event arrives
        let initial = vec![
            Message::User {
                content: OneOrMany::one(UserContent::text("run command")),
            },
            tool_call_msg(
                "tc-cmd",
                "execute_command",
                serde_json::json!({"command": "make build"}),
            ),
            Message::User {
                content: OneOrMany::one(UserContent::ToolResult(ToolResult {
                    id: "tc-cmd".to_string(),
                    call_id: None,
                    content: OneOrMany::one(ToolResultContent::Text(rig::agent::Text {
                        text: "Command is still running. Output will be streamed via subscription events.".to_string(),
                    })),
                })),
            },
        ];
        let mut hm = make_history(&store, initial).await;
        hm.processed_tool_calls.insert("tc-cmd".to_string());

        let input = tool_result_input(
            "thread-1",
            "tc-cmd",
            "build output chunk\n[exit code: 0]",
            Some(SyntheticKind::Tagged(
                TaggedSyntheticKind::SubscriptionEvent {
                    tool_call_id: "tc-cmd".to_string(),
                    associative: true,
                },
            )),
        );

        let result = prepare_input(input, "msg-2".to_string(), &mut hm, &store)
            .await
            .unwrap();

        assert_eq!(result, PrepareResult::Ready);
        // Should NOT spawn a subthread — stays in the same thread
        assert_eq!(hm.thread_id, "thread-1");
        // Should have: original user, tool call, original result, synthetic tool call, event result
        insta::assert_json_snapshot!(
            hm.history,
            { "[3].content[0].id" => "[uuid]", "[4].content[0].id" => "[uuid]" }
        );
    }

    #[tokio::test]
    async fn associative_subscription_event_tool_interruption() {
        let store = StubConversationStore::new();
        // Tool call is still pending (no result yet) when the associative event arrives
        let initial = vec![
            Message::User {
                content: OneOrMany::one(UserContent::text("run command")),
            },
            tool_call_msg(
                "tc-cmd",
                "execute_command",
                serde_json::json!({"command": "make build"}),
            ),
        ];
        let mut hm = make_history(&store, initial).await;

        let input = tool_result_input(
            "thread-1",
            "tc-cmd",
            "build output chunk\n[exit code: 0]",
            Some(SyntheticKind::Tagged(
                TaggedSyntheticKind::SubscriptionEvent {
                    tool_call_id: "tc-cmd".to_string(),
                    associative: true,
                },
            )),
        );

        let result = prepare_input(input, "msg-2".to_string(), &mut hm, &store)
            .await
            .unwrap();

        assert_eq!(result, PrepareResult::Ready);
        // Should NOT spawn a subthread — stays in the same thread
        assert_eq!(hm.thread_id, "thread-1");
        insta::assert_json_snapshot!(
            hm.history,
            { "[3].content[0].id" => "[uuid]", "[4].content[0].id" => "[uuid]" }
        );
    }
}
