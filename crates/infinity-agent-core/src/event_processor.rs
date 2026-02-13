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
use crate::traits::{ConversationStore, MessageSender, StateStore};

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
pub enum PrepareResult {
    /// The input was processed and the history manager is ready for completion.
    Ready,
    /// The input was handled without needing a completion (e.g. OAuth, duplicate, closed thread).
    Handled,
}

/// What the model wants to do after a completion stream finishes.
pub enum CompletionAction {
    /// Model produced text and is done (no tool call).
    Done,
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
pub enum CompletionEvent {
    /// A chunk of text from the model.
    TextChunk(String),
    /// The terminal event — what to do next.
    Action(CompletionAction),
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
        } else if let Some(Message::Assistant { content, .. }) = self.history.last() {
            if let AssistantContent::ToolCall(tool_call) = content.first() {
                if !self.processed_tool_calls.contains(tool_call.id.as_str()) {
                    tracing::info!("Tool call {} interrupted by new user message", tool_call.id);
                    let synthetic_result = Message::User {
                        content: OneOrMany::one(UserContent::ToolResult(
                            rig::message::ToolResult {
                                id: tool_call.id.clone(),
                                call_id: tool_call.call_id.clone(),
                                content: OneOrMany::one(ToolResultContent::Text(
                                    rig::agent::Text {
                                        text: "Tool call interrupted by user".to_string(),
                                    },
                                )),
                            },
                        )),
                    };
                    self.history.push(synthetic_result.clone());
                    self.append_pending(synthetic_result, format!("{}-interrupted", tool_call.id));
                    self.mark_tool_call_complete(tool_call.id.clone());
                }
            }
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
            StreamedAssistantContent::ToolCall(call) => Message::Assistant {
                id: None,
                content: OneOrMany::one(AssistantContent::ToolCall(call.clone())),
            },
            StreamedAssistantContent::ToolCallDelta { .. } | StreamedAssistantContent::Final(_) => {
                return;
            }
        };
        self.history.push(message.clone());
        self.append_pending(message, completion_id);
    }

    fn append_pending(&mut self, message: Message, message_id: String) {
        if let Message::User { content } = &message {
            if let UserContent::ToolResult(result) = content.first() {
                self.mark_tool_call_complete(result.id.clone());
            }
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
}

// ═══════════════════════════════════════════════════════════════════════
// (a) prepare_input — process the raw InputMessage into history, handling
//     synthetics, subscription events, OAuth, dedup, closed threads.
// ═══════════════════════════════════════════════════════════════════════

pub async fn prepare_input<C, S, M>(
    input_msg: InputMessage,
    message_id: String,
    current_history: &mut HistoryManager<C, S>,
    conversation_store: &C,
    state_store: &S,
    output_sender: &M,
) -> Result<PrepareResult, BoxError>
where
    C: ConversationStore,
    S: StateStore,
    M: MessageSender,
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

    // Handle OAuth required messages — forward to output, don't add to history
    if let InputMessageContent::OAuth(oauth) = &input_msg.content {
        if oauth.content_type == "oauth_required" {
            tracing::info!("Received OAuth required message, forwarding to output");
            let metadata = current_history
                .get_metadata()
                .unwrap_or(serde_json::json!({}));
            let oauth_msg = OAuthOutputMessage {
                message_type: "oauth_required".to_string(),
                auth_url: oauth.auth_url.clone(),
                metadata,
            };
            output_sender
                .send_to_output(&serde_json::to_string(&oauth_msg)?)
                .await
                .map_err(|e| Box::new(e) as BoxError)?;
            return Ok(PrepareResult::Handled);
        }
    }

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

        if synthetic_kind.is_thread_report() {
            let new_tool_call_id = uuid::Uuid::new_v4().to_string();
            if let UserContent::ToolResult(mut tool_result) = user_content {
                let mut synthetic_args = original_call.function.arguments.clone();
                synthetic_args.as_object_mut().unwrap().insert(
                    "kind".to_string(),
                    serde_json::json!(format!("thread_report:{}", original_tool_call_id)),
                );
                let synthetic_tool_call = Message::Assistant {
                    id: None,
                    content: OneOrMany::one(AssistantContent::ToolCall(rig::message::ToolCall {
                        id: new_tool_call_id.clone(),
                        call_id: None,
                        function: rig::message::ToolFunction {
                            name: original_call.function.name.clone(),
                            arguments: synthetic_args,
                        },
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

            let spawn_order = conversation_store
                .get_current_message_order(&input_msg.group_id)
                .await
                .map_err(|e| Box::new(e) as BoxError)?;

            let sub_thread_id = conversation_store
                .spawn_thread(&input_msg.group_id, spawn_order, &original_tool_call_id)
                .await
                .map_err(|e| Box::new(e) as BoxError)?;

            tracing::info!(
                "Created subthread {} for subscription event in parent {}",
                sub_thread_id,
                input_msg.group_id
            );

            conversation_store
                .mark_as_subscription_event(&sub_thread_id)
                .await
                .map_err(|e| Box::new(e) as BoxError)?;

            *current_history = HistoryManager::new_with_history(
                conversation_store.clone(),
                state_store.clone(),
                sub_thread_id.clone(),
            )
            .await?;

            let event_call_id = uuid::Uuid::new_v4().to_string();
            let spawn_call_id = uuid::Uuid::new_v4().to_string();

            let mut synthetic_args = original_call.function.arguments.clone();
            synthetic_args.as_object_mut().unwrap().insert(
                "kind".to_string(),
                serde_json::json!(format!(
                    "interrupt:{} (subscription remains active)",
                    original_tool_call_id
                )),
            );

            let event_tool_call = Message::Assistant {
                id: None,
                content: OneOrMany::one(AssistantContent::ToolCall(rig::message::ToolCall {
                    id: event_call_id.clone(),
                    call_id: None,
                    function: rig::message::ToolFunction {
                        name: original_call.function.name.clone(),
                        arguments: synthetic_args,
                    },
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
                                    "instructions": "Process the single subscription event above, and close the thread after processing this event with a report to the parent if appropriate. Only your report will be visible to the parent."
                                }),
                            },
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

    Ok(PrepareResult::Ready)
}

// ═══════════════════════════════════════════════════════════════════════
// (b) run_completion — yields CompletionEvent items (text chunks and a
//     terminal Action). Handles stream errors and unknown tools internally.
// ═══════════════════════════════════════════════════════════════════════

pub fn run_completion<'a, Mdl, C, S>(
    model: &'a Mdl,
    history: &'a mut HistoryManager<C, S>,
    tool_names: &'a HashSet<String>,
    tools: &'a [ToolDefinition],
    group_id: &'a str,
    message_id: &'a str,
) -> impl futures_util::Stream<Item = Result<CompletionEvent, BoxError>> + 'a
where
    Mdl: CompletionModel,
    C: ConversationStore,
    S: StateStore,
{
    async_stream::try_stream! {
        let mut completion_counter: usize = 0;

        'outer: loop {
            let stream_result = model
                .stream(CompletionRequest {
                    preamble: None,
                    chat_history: history.get_history(),
                    documents: vec![],
                    tools: tools.to_vec(),
                    temperature: None,
                    max_tokens: None,
                    tool_choice: None,
                    additional_params: Some(serde_json::json!({
                        "anthropic_beta": ["interleaved-thinking-2025-05-14"],
                        "thinking": {
                            "type": "enabled",
                            "budget_tokens": 4096,
                        }
                    })),
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
                if let StreamedAssistantContent::Reasoning(ref r) = chunk {
                    if r.signature.is_none() { continue; }
                }

                let completion_id = format!("{}-{}-completion-{}", group_id, message_id, completion_counter);
                completion_counter += 1;

                history.handle_completion(&chunk, completion_id);

                match chunk {
                    StreamedAssistantContent::Text(text) => {
                        tracing::info!("[Text] {}", &text.text);
                        yield CompletionEvent::TextChunk(text.text);
                    }
                    StreamedAssistantContent::ToolCall(call) => {
                        tracing::info!("[Tool Call: {} with arguments {}]", &call.function.name, &call.function.arguments);

                        // Unknown tool — inject error and retry the whole completion
                        if !tool_names.contains(call.function.name.as_str()) {
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

                        history.sync().await?;

                        yield CompletionEvent::Action(CompletionAction::ExecuteToolCall {
                            tool_name: call.function.name,
                            tool_args: call.function.arguments,
                            tool_call_id: call.id,
                            call_id: call.call_id,
                        });
                        return;
                    }
                    StreamedAssistantContent::ToolCallDelta { .. } => {}
                    StreamedAssistantContent::Reasoning(reasoning) => {
                        tracing::info!("[Reasoning: {:?}]", reasoning.reasoning);
                    }
                    StreamedAssistantContent::Final(_) => {
                        tracing::info!("Received final message");
                        yield CompletionEvent::Action(CompletionAction::Done);
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

pub async fn execute_action<M, C, S>(
    action: CompletionAction,
    history: &mut HistoryManager<C, S>,
    tool_registry: &HashMap<String, &Box<dyn Tool<M>>>,
    tool_context: &ToolContext<M>,
) -> Result<(), BoxError>
where
    M: MessageSender + 'static,
    C: ConversationStore,
    S: StateStore,
{
    match action {
        CompletionAction::Done => {
            history.sync().await?;
        }
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
