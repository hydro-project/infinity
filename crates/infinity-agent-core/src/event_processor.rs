use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
    time::Duration,
};

use futures_util::StreamExt;
use rig::{
    OneOrMany,
    completion::{CompletionModel, CompletionRequest, ToolDefinition},
    message::{AssistantContent, Message, ToolResult, ToolResultContent, UserContent},
    streaming::{StreamedAssistantContent, ToolCallDeltaContent},
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
    /// A user choice prompt must be surfaced to the user.
    UserChoiceRequired {
        id: String,
        prompt: String,
        choices: Vec<String>,
        default: usize,
        response_url: String,
    },
    /// Compaction was applied to the in-memory history.
    CompactionApplied,
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
    /// A tool call that was synchronously processed.
    SyncToolCall {
        tool_name: String,
        tool_args: serde_json::Value,
    },
    /// The model has started thinking (reasoning).
    ThinkingStart,
    /// The model has stopped thinking (reasoning).
    ThinkingEnd,
    /// A chunk of thinking/reasoning text from the model.
    ThinkingChunk(String),
    /// A synchronous tool result.
    SyncToolResult(ToolResult),
    /// Some piece of information to log to the user.
    Info(String),
}

// ── HistoryManager (unchanged from before) ──

pub struct PendingItem {
    message: Message,
    message_id: String,
}

pub struct HistoryManager<C: ConversationStore, S: StateStore> {
    conversation_store: C,
    state_store: S,
    pub thread_id: String,
    pub root_thread_id: String,
    ancestor_chain: Vec<String>,
    pub history: RefCell<Vec<Message>>,
    processed_message_ids: RefCell<HashSet<String>>,
    processed_tool_calls: RefCell<HashSet<String>>,
    metadata: RefCell<Option<serde_json::Value>>,
    pending_items: RefCell<Vec<PendingItem>>,
    pending_complete_tool_calls: RefCell<HashSet<String>>,
    /// Tool call IDs that were interrupted by a new user message during
    /// `handle_content`. Callers can drain this via `take_interrupted_tool_calls`
    /// to send best-effort cancellation notifications to RAP tool servers.
    interrupted_tool_calls: RefCell<Vec<String>>,
    /// Tracks the absolute store index that the current in-memory compaction
    /// summary covers up to. Used to compute the correct relative split
    /// position when a second compaction is applied on top of an existing one.
    compacted_up_to: RefCell<Option<i64>>,
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

        let (history, compacted_up_to) = conversation_store
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
            history: RefCell::new(history),
            processed_message_ids: RefCell::new(processed_message_ids),
            processed_tool_calls: RefCell::new(processed_tool_calls),
            metadata: RefCell::new(metadata),
            pending_items: RefCell::new(Vec::new()),
            pending_complete_tool_calls: RefCell::new(HashSet::new()),
            interrupted_tool_calls: RefCell::new(Vec::new()),
            compacted_up_to: RefCell::new(compacted_up_to),
        })
    }

    pub async fn handle_content(
        &self,
        message: Message,
        message_id: String,
    ) -> Result<bool, BoxError> {
        if self.processed_message_ids.borrow().contains(&message_id) {
            tracing::info!("Message {} already processed, skipping", message_id);
            return Ok(false);
        }

        if let Message::User { content } = &message
            && let UserContent::ToolResult(ref tool_result) = content.first()
        {
            if self
                .processed_tool_calls
                .borrow()
                .contains(tool_result.id.as_str())
            {
                tracing::info!(
                    "Tool call {} already processed, ignoring duplicate",
                    tool_result.id
                );
                self.processed_message_ids
                    .borrow_mut()
                    .insert(message_id.clone());
                let _ = self
                    .state_store
                    .add_processed_message_ids(&self.thread_id, vec![message_id])
                    .await;
                return Ok(false);
            } else if !self.history.borrow().last().is_some_and(|l| {
                if let Message::Assistant { content, .. } = l
                    && let AssistantContent::ToolCall(c) = content.first()
                {
                    c.id == tool_result.id
                } else {
                    false
                }
            }) {
                tracing::info!(
                    "Got tool call result for wrong call, ignoring {:?}",
                    tool_result
                );
                return Ok(false);
            }
        } else {
            let last_msg = self.history.borrow().last().cloned();
            if let Some(Message::Assistant { content, .. }) = last_msg
                && let AssistantContent::ToolCall(tool_call) = content.first()
                && !self
                    .processed_tool_calls
                    .borrow()
                    .contains(tool_call.id.as_str())
            {
                tracing::info!("Tool call {} interrupted by new user message", tool_call.id);
                self.interrupted_tool_calls
                    .borrow_mut()
                    .push(tool_call.id.clone());
                let synthetic_result = Message::User {
                    content: OneOrMany::one(UserContent::ToolResult(rig::message::ToolResult {
                        id: tool_call.id.clone(),
                        call_id: tool_call.call_id.clone(),
                        content: OneOrMany::one(ToolResultContent::Text(rig::agent::Text {
                            text: "Tool call interrupted by user".to_string(),
                        })),
                    })),
                };
                self.append_pending(synthetic_result, format!("{}-interrupted", tool_call.id));
                self.mark_tool_call_complete(tool_call.id.clone());
            }
        }

        self.append_pending(message, message_id.clone());
        self.processed_message_ids.borrow_mut().insert(message_id);
        Ok(true)
    }

    pub fn handle_completion<R>(
        &self,
        completion: &StreamedAssistantContent<R>,
        completion_id: String,
    ) {
        if self.processed_message_ids.borrow().contains(&completion_id) {
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
        self.append_pending(message, completion_id);
    }

    fn append_pending(&self, message: Message, message_id: String) {
        self.history.borrow_mut().push(message.clone());
        if let Message::User { content } = &message
            && let UserContent::ToolResult(result) = content.first()
        {
            self.mark_tool_call_complete(result.id.clone());
        }
        self.pending_items.borrow_mut().push(PendingItem {
            message,
            message_id,
        });
    }

    fn mark_tool_call_complete(&self, call_id: String) {
        self.processed_tool_calls
            .borrow_mut()
            .insert(call_id.clone());
        self.pending_complete_tool_calls
            .borrow_mut()
            .insert(call_id);
    }

    pub async fn sync(&self) -> Result<(), BoxError> {
        let pending_items = std::mem::take(&mut *self.pending_items.borrow_mut());
        let pending_complete_tool_calls =
            std::mem::take(&mut *self.pending_complete_tool_calls.borrow_mut());
        if pending_items.is_empty() && pending_complete_tool_calls.is_empty() {
            return Ok(());
        }
        if !pending_items.is_empty() {
            let msgs: Vec<(Message, String)> = pending_items
                .iter()
                .map(|item| (item.message.clone(), item.message_id.clone()))
                .collect();
            self.conversation_store
                .append_messages(&self.thread_id, msgs)
                .await
                .map_err(|e| Box::new(e) as BoxError)?;
        }
        let msg_ids: Vec<String> = pending_items.iter().map(|i| i.message_id.clone()).collect();
        let tc_ids: Vec<String> = pending_complete_tool_calls.iter().cloned().collect();
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
        Ok(())
    }

    pub async fn update_metadata(&self, metadata: serde_json::Value) -> Result<(), BoxError> {
        *self.metadata.borrow_mut() = Some(metadata.clone());
        self.state_store
            .set_metadata(&self.root_thread_id, metadata)
            .await
            .map_err(|e| Box::new(e) as BoxError)
    }

    pub fn get_metadata(&self) -> Option<serde_json::Value> {
        self.metadata.borrow().clone()
    }
    pub fn get_history(&self) -> OneOrMany<Message> {
        OneOrMany::many(self.history.borrow().clone()).expect("bug: history should never be empty")
    }

    pub fn remove_trailing_reasoning(&self) {
        let mut history = self.history.borrow_mut();
        let mut pending_items = self.pending_items.borrow_mut();
        while let Some(Message::Assistant { content, .. }) = history.last() {
            match content.first() {
                AssistantContent::Reasoning(_) => {
                    history.pop();
                    pending_items.pop();
                }
                AssistantContent::Text(text) if text.text.trim().is_empty() => {
                    history.pop();
                    pending_items.pop();
                }
                _ => break,
            }
        }
    }

    /// Returns the full thread stack: [root, ..ancestors, current_thread].
    /// For the root thread this is just [root_thread_id].
    pub fn get_thread_stack(&self) -> Vec<String> {
        let mut stack = self.ancestor_chain.clone();
        stack.push(self.thread_id.clone());
        stack
    }

    pub fn conversation_store(&self) -> &C {
        &self.conversation_store
    }
    pub fn state_store(&self) -> &S {
        &self.state_store
    }

    /// Apply the latest compaction summary: reload from store, truncate
    /// in-memory history up to the compaction point, and prepend the summary.
    pub async fn apply_compaction(&self) -> Result<bool, BoxError> {
        if let Ok(Some((summary, up_to_order))) = self
            .conversation_store
            .load_latest_compaction_summary_up_to(&self.thread_id, None)
            .await
        {
            // Compute the relative split position in the in-memory history.
            // If a previous compaction already replaced indices 0..prev with a
            // single summary message, the in-memory index 0 corresponds to
            // absolute index (prev - 1) in the store (the -1 accounts for the
            // summary message itself occupying slot 0).
            let offset = self
                .compacted_up_to
                .borrow()
                .map_or(0, |prev| prev as usize - 1);
            let up_to = (up_to_order as usize).saturating_sub(offset);
            let mut history = self.history.borrow_mut();
            if up_to <= history.len() {
                let remaining = history.split_off(up_to);
                *history = vec![Message::Assistant {
                    id: None,
                    content: OneOrMany::one(AssistantContent::text(format!(
                        "[Compacted conversation summary]\n{}",
                        summary
                    ))),
                }];
                history.extend(remaining);
                *self.compacted_up_to.borrow_mut() = Some(up_to_order);
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Drain and return tool call IDs that were interrupted by new user messages.
    /// Callers use this to send best-effort cancellation notifications to RAP
    /// tool servers so they can abort in-flight operations.
    pub fn take_interrupted_tool_calls(&self) -> Vec<String> {
        std::mem::take(&mut *self.interrupted_tool_calls.borrow_mut())
    }

    /// Record a subscription in the current thread's metadata. The
    /// `tool_call_id` is the ID of the tool call whose result had
    /// `subscription: true`. Ownership is implicit — a subscription is
    /// stored in the thread that created it.
    pub async fn track_subscription(&self, tool_call_id: &str) -> Result<(), BoxError> {
        self.state_store
            .add_active_subscription(&self.thread_id, tool_call_id)
            .await
            .map_err(|e| Box::new(e) as BoxError)
    }

    /// Remove a subscription from the current thread's active tracking.
    pub async fn remove_subscription(&self, tool_call_id: &str) -> Result<(), BoxError> {
        self.state_store
            .remove_active_subscription(&self.thread_id, tool_call_id)
            .await
            .map_err(|e| Box::new(e) as BoxError)
    }

    /// Check if this thread has any active subscriptions.
    pub async fn has_active_subscriptions(&self) -> bool {
        self.state_store
            .get_active_subscriptions(&self.thread_id)
            .await
            .map(|s| !s.is_empty())
            .unwrap_or(false)
    }
}

// ═══════════════════════════════════════════════════════════════════════
// (a) prepare_input — process the raw InputMessage into history, handling
//     synthetics, subscription events, OAuth, dedup, closed threads.
// ═══════════════════════════════════════════════════════════════════════

pub async fn prepare_input<C, S, M>(
    input_msg: InputMessage,
    message_id: String,
    current_history: &HistoryManager<C, S>,
    conversation_store: &C,
    message_sender: &M,
) -> Result<PrepareResult, BoxError>
where
    C: ConversationStore,
    S: StateStore,
    M: InputSender,
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

    // Handle compaction complete: apply compaction to in-memory history, no LLM needed
    if input_msg.synthetic.as_ref().is_some_and(|s| {
        matches!(
            s,
            crate::message::SyntheticKind::Tagged(
                crate::message::TaggedSyntheticKind::CompactionComplete
            )
        )
    }) {
        tracing::info!("Applying compaction to thread {}", input_msg.group_id);
        current_history.apply_compaction().await?;
        return Ok(PrepareResult::CompactionApplied);
    }

    // Handle compaction trigger: spawn a compaction child thread
    if input_msg.synthetic.as_ref().is_some_and(|s| {
        matches!(
            s,
            crate::message::SyntheticKind::Tagged(crate::message::TaggedSyntheticKind::Compaction)
        )
    }) {
        let spawn_call_id = uuid::Uuid::new_v4().to_string();
        let sub_thread_id = conversation_store
            .spawn_thread(&input_msg.group_id, &spawn_call_id, false)
            .await
            .map_err(|e| Box::new(e) as BoxError)?;
        conversation_store
            .mark_thread_as_compaction(&sub_thread_id)
            .await
            .map_err(|e| Box::new(e) as BoxError)?;

        tracing::info!(
            "Spawned compaction thread {} for parent {}",
            sub_thread_id,
            input_msg.group_id
        );

        // Write spawn tool call + result directly to child's store.
        // If the parent's history ends with an unanswered tool call, prepend
        // a synthetic "interrupted" result so the child doesn't have two
        // consecutive assistant tool calls without a tool_result in between.
        let mut child_messages: Vec<(Message, String)> = Vec::new();
        if let Some(Message::Assistant { content, .. }) =
            current_history.history.borrow().last().cloned()
            && let AssistantContent::ToolCall(pending_call) = content.first()
            && !current_history
                .processed_tool_calls
                .borrow()
                .contains(pending_call.id.as_str())
        {
            let interrupted_result = Message::User {
                content: OneOrMany::one(UserContent::ToolResult(ToolResult {
                    id: pending_call.id.clone(),
                    call_id: pending_call.call_id.clone(),
                    content: OneOrMany::one(ToolResultContent::Text(rig::agent::Text {
                        text: "Tool call interrupted by compaction".to_string(),
                    })),
                })),
            };
            child_messages.push((
                interrupted_result,
                format!("{}-interrupted", pending_call.id),
            ));
        }

        let spawn_tool_call = Message::Assistant {
            id: None,
            content: OneOrMany::one(AssistantContent::ToolCall(rig::message::ToolCall {
                id: spawn_call_id.clone(),
                call_id: None,
                function: rig::message::ToolFunction {
                    name: "__harness_begin_compaction__".to_string(),
                    arguments: serde_json::json!({}),
                },
                additional_params: None,
                signature: None,
            })),
        };
        child_messages.push((
            spawn_tool_call,
            format!("{}-compaction-call", spawn_call_id),
        ));
        conversation_store
            .append_messages(&sub_thread_id, child_messages)
            .await
            .map_err(|e| Box::new(e) as BoxError)?;

        // Send child its instructions via message sender
        let child_msg = InputMessage {
            content: InputMessageContent::User(UserContent::ToolResult(ToolResult {
                id: spawn_call_id.clone(),
                call_id: None,
                content: OneOrMany::one(ToolResultContent::Text(rig::agent::Text {
                    text: format!(
                        "This tool call was synthetically injected by the harness. You are now INSIDE a compaction thread. You can see the full conversation history inherited from your parent thread, including all ancestor thread context. \
                        Summarize ALL of this content into a concise but comprehensive summary that preserves: all important context, decisions made, \
                        current task progress, relevant code changes and file paths, and any pending work. \
                        Then call close_thread with your thread ID ({}) and include the summary in report_to_parent.",
                        sub_thread_id
                    ),
                })),
            })),
            group_id: sub_thread_id.clone(),
            metadata: None,
            synthetic: None,
            display_as: None,
            subscription: false,
        };
        message_sender
            .send_to_input_queue(child_msg, &sub_thread_id, &spawn_call_id)
            .await
            .map_err(|e| Box::new(e) as BoxError)?;

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

    // Handle user choice required messages — return to caller, don't add to history
    if let InputMessageContent::UserChoice(choice) = &input_msg.content {
        assert!(choice.content_type == "user_choice_required");
        tracing::info!("Received user choice required message, returning to caller");
        return Ok(PrepareResult::UserChoiceRequired {
            id: choice.id.clone(),
            prompt: choice.prompt.clone(),
            choices: choice.choices.clone(),
            default: choice.default,
            response_url: choice.response_url.clone(),
        });
    }

    let is_subscription = input_msg.subscription;

    let user_content = match input_msg.content {
        InputMessageContent::User(content) => content,
        InputMessageContent::OAuth(_) | InputMessageContent::UserChoice(_) => {
            return Ok(PrepareResult::Handled);
        }
    };

    // Handle synthetic tool results (subscription events / thread reports)
    let content = if let Some(synthetic_kind) = input_msg.synthetic {
        let original_tool_call_id = synthetic_kind.tool_call_id().to_string();
        let is_final_subscription = synthetic_kind.is_final();
        tracing::info!(
            "Processing synthetic tool result for tool call: {}",
            original_tool_call_id
        );

        let original_call = current_history.history.borrow().iter().find_map(|msg| {
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

        if synthetic_kind.is_thread_report()
            || synthetic_kind.is_associative()
            || synthetic_kind.is_parent_message()
        {
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
                // Remove subscription if this is the final event
                if is_final_subscription {
                    current_history
                        .remove_subscription(&original_tool_call_id)
                        .await
                        .ok();
                }
                UserContent::ToolResult(tool_result)
            } else {
                return Err("Synthetic message is not a tool result".into());
            }
        } else {
            // Subscription events spawn a new subthread via message sender
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

            let event_call_id = uuid::Uuid::new_v4().to_string();
            let spawn_call_id = uuid::Uuid::new_v4().to_string();

            let event_content = if let UserContent::ToolResult(mut tool_result) = user_content {
                tool_result.id = event_call_id.clone();
                tool_result.call_id = None;
                tool_result
            } else {
                return Err("Synthetic subscription event is not a tool result".into());
            };

            // If the parent's history ends with an unanswered tool call (e.g.
            // sleep_until_event_or_input), the child will inherit that via
            // load_history_with_ancestors. We must prepend a synthetic
            // "interrupted" result so the child's conversation doesn't have
            // two consecutive assistant tool calls without a tool_result in
            // between, which would cause a 400 Bad Request from the API.
            let mut child_messages: Vec<(Message, String)> = Vec::new();
            if let Some(Message::Assistant { content, .. }) =
                current_history.history.borrow().last().cloned()
                && let AssistantContent::ToolCall(pending_call) = content.first()
                && !current_history
                    .processed_tool_calls
                    .borrow()
                    .contains(pending_call.id.as_str())
            {
                let interrupted_result = Message::User {
                    content: OneOrMany::one(UserContent::ToolResult(ToolResult {
                        id: pending_call.id.clone(),
                        call_id: pending_call.call_id.clone(),
                        content: OneOrMany::one(ToolResultContent::Text(rig::agent::Text {
                            text: "Tool call interrupted by subscription event".to_string(),
                        })),
                    })),
                };
                child_messages.push((
                    interrupted_result,
                    format!("{}-interrupted", pending_call.id),
                ));
            }

            // Write event + spawn tool calls directly to child's store
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
            let spawn_tool_call = Message::Assistant {
                id: None,
                content: OneOrMany::one(AssistantContent::ToolCall(rig::message::ToolCall {
                    id: spawn_call_id.clone(),
                    call_id: None,
                    function: rig::message::ToolFunction {
                        name: "spawn_thread".to_string(),
                        arguments: serde_json::json!({
                            "instructions": "Spawning thread to process incoming event."
                        }),
                    },
                    additional_params: None,
                    signature: None,
                })),
            };
            let spawn_tool_result = Message::User {
                content: OneOrMany::one(UserContent::ToolResult(ToolResult {
                    id: spawn_call_id.clone(),
                    call_id: None,
                    content: OneOrMany::one(ToolResultContent::Text(rig::agent::Text {
                        text: format!(
                            "You are now INSIDE the thread for processing the single event above. Your thread ID is {}, the parent which is still subscribing is {}. Process the single subscription event above, report to the parent if appropriate, then close the thread after processing this event. Your outputs are NOT VISIBLE to the user, if you want to show them something, send a report to your parent.",
                            sub_thread_id, input_msg.group_id
                        ),
                    })),
                })),
            };
            child_messages.extend(vec![
                (spawn_tool_call, format!("{}-spawn-call", spawn_call_id)),
                (spawn_tool_result, format!("{}-spawn-result", spawn_call_id)),
                (event_tool_call, format!("{}-event-call", event_call_id)),
            ]);
            conversation_store
                .append_messages(&sub_thread_id, child_messages)
                .await
                .map_err(|e| Box::new(e) as BoxError)?;

            // Send child its instructions via message sender
            let child_msg = InputMessage {
                content: InputMessageContent::User(UserContent::ToolResult(event_content)),
                group_id: sub_thread_id.clone(),
                metadata: None,
                synthetic: None,
                display_as: None,
                subscription: false,
            };
            message_sender
                .send_to_input_queue(child_msg, &sub_thread_id, &event_call_id)
                .await
                .map_err(|e| Box::new(e) as BoxError)?;

            // Remove subscription if this is the final event
            if is_final_subscription {
                current_history
                    .remove_subscription(&original_tool_call_id)
                    .await
                    .ok();
            }

            return Ok(PrepareResult::Handled);
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

#[allow(clippy::too_many_arguments)]
pub fn run_completion<'a: 'b, 'b, Mdl, C, S, M>(
    model: &'a Mdl,
    history: &'a HistoryManager<C, S>,
    tool_names: &'a HashSet<String>,
    tools: &'a [ToolDefinition],
    tool_registry: &'a HashMap<String, &'a dyn Tool<M>>,
    tool_context: &'a ToolContext<M>,
    group_id: &'a str,
    message_id: &'a str,
    extra_system_prompt: Option<&'a str>,
    additional_request_params: Option<&'a serde_json::Value>,
    model_id_override: Option<&'a str>,
    cancel_rx: tokio::sync::oneshot::Receiver<()>,
) -> impl futures_util::Stream<Item = Result<CompletionEvent<Mdl::StreamingResponse>, BoxError>> + 'b
where
    Mdl: CompletionModel,
    C: ConversationStore,
    S: StateStore,
    M: InputSender + 'static,
{
    async_stream::try_stream! {
        let mut cancel_rx = cancel_rx;
        let mut completion_counter: usize = 0;
        let mut is_thinking = false;
        let mut retry_count = 0;

        let preamble = {
            let base = include_str!("default_prompt.md");
            let thread_info = format!("\n\nYour current thread ID is `{}`. The root thread ID is `{}`.", history.thread_id, history.root_thread_id);
            match extra_system_prompt {
                Some(extra) => format!("{}{}\n\n{}", base, thread_info, extra),
                None => format!("{}{}", base, thread_info),
            }
        };

        'outer: loop {
            let stream_result = model
                .stream(CompletionRequest {
                    model: model_id_override.map(|s| s.to_string()),
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
                        if let Some(extra) = additional_request_params
                            && let (Some(base_obj), Some(extra_obj)) = (base.as_object_mut(), extra.as_object()) {
                                for (k, v) in extra_obj {
                                    base_obj.insert(k.clone(), v.clone());
                                }
                            }
                        Some(base)
                    },
                    output_schema: None,
                });

            let stream_result = tokio::select! {
                r = stream_result => {
                    Ok(r)
                }
                _ = &mut cancel_rx => {
                    tracing::info!("Completion cancelled during request initiation");
                    // there must be no trailing reasoning because we drop it when retrying post-initiation
                    return;
                }
                _ = tokio::time::sleep(Duration::from_secs(30)) => {
                    if retry_count < 10 {
                        yield CompletionEvent::Info("Stream error (timeout initiating request), retrying...".to_string());
                        retry_count += 1;
                        continue 'outer;
                    } else {
                        Err(Into::<BoxError>::into("Timed out initiating request"))
                    }
                }
            }?;

            let mut llm_stream = match stream_result {
                Ok(s) => s,
                Err(e) => {
                    // there must be no trailing reasoning because we drop it when retrying post-initiation
                    let err_str = format!("{}", e);
                    tracing::error!(error = %e, "Completion stream initiation failed");

                    if (err_str.contains("please wait before trying again") || err_str.contains("please try again")) && retry_count < 10 {
                        tracing::warn!("Stream error (rate limit), retrying...");

                        yield CompletionEvent::Info("Stream error (rate limit), retrying after 30 seconds...".to_string());
                        tokio::select! {
                            _ = tokio::time::sleep(Duration::from_secs(30)) => {}
                            _ = &mut cancel_rx => {
                                tracing::info!("Completion cancelled during retry wait");
                                return;
                            }
                        }
                        retry_count += 1;
                        continue 'outer;
                    } else if (err_str.contains("unexpected end of stream") || err_str.contains("unexpected error when processing the request")) && retry_count < 10 {
                        tracing::warn!("Stream error (unexpected end), retrying...");

                        yield CompletionEvent::Info("Stream error (unexpected end), retrying...".to_string());
                        tokio::select! {
                            _ = tokio::time::sleep(Duration::from_secs(5)) => {}
                            _ = &mut cancel_rx => {
                                tracing::info!("Completion cancelled during retry wait");
                                return;
                            }
                        }
                        retry_count += 1;
                        continue 'outer;
                    } else {
                        Err(Into::<BoxError>::into(e))?;
                        unreachable!()
                    }
                }
            };

            let mut has_emitted_tool_call = false;
            let mut should_loop_back = false;

            loop {
                // Race between LLM output and cancellation signal.
                // We avoid `yield` inside `select!` (async_stream limitation)
                // by capturing the result into locals first.
                let cancelled;
                let llm_next = tokio::select! {
                    res = llm_stream.next() => { cancelled = false; Ok(res) },
                    _ = &mut cancel_rx => { cancelled = true; Ok(None) },
                    _ = tokio::time::sleep(Duration::from_secs(120)) => {
                        cancelled = false;
                        if retry_count < 10 {
                            yield CompletionEvent::Info("Stream error (timeout), retrying...".to_string());
                            tracing::warn!("Stream ended unexpectedly, removing trailing reasoning and retrying...");
                            history.remove_trailing_reasoning();
                            if is_thinking {
                                is_thinking = false;
                                yield CompletionEvent::ThinkingEnd;
                            }
                            retry_count += 1;
                            continue 'outer;
                        } else {
                            Err(Into::<BoxError>::into("Stream timed out"))
                        }
                    },
                }?;

                if cancelled {
                    tracing::info!("Completion cancelled");
                    history.remove_trailing_reasoning();
                    if is_thinking {
                        yield CompletionEvent::ThinkingEnd;
                    }
                    return;
                }

                let res = match llm_next {
                    Some(r) => r,
                    None => {
                        history.remove_trailing_reasoning();
                        if is_thinking {
                            is_thinking = false;
                            yield CompletionEvent::ThinkingEnd;
                        }
                        if retry_count < 10 {
                            yield CompletionEvent::Info("Stream error (unexpected end), retrying...".to_string());
                            tracing::warn!("Stream ended unexpectedly, removing trailing reasoning and retrying...");
                            tokio::time::sleep(Duration::from_secs(1)).await;
                            retry_count += 1;
                            continue 'outer;
                        } else {
                            Err(Into::<BoxError>::into("Stream timed out"))?;
                            unreachable!()
                        }
                    }
                };

                let chunk = match res {
                    Ok(c) => {
                        retry_count = 0;
                        c
                    },
                    Err(e) => {
                        history.remove_trailing_reasoning();
                        if is_thinking {
                            is_thinking = false;
                            yield CompletionEvent::ThinkingEnd;
                        }
                        let err_str = format!("{}", e);
                        if (err_str.contains("unexpected end of stream") || err_str.contains("unexpected error when processing the request")) && retry_count < 10 {
                            yield CompletionEvent::Info("Stream error (unexpected end), retrying...".to_string());
                            tracing::warn!("Stream error (unexpected end), retrying...");
                            tokio::time::sleep(Duration::from_secs(1)).await;
                            retry_count += 1;
                            continue 'outer;
                        } else {
                            Err(Into::<BoxError>::into(e))?;
                            unreachable!()
                        }
                    }
                };

                // Skip incomplete reasoning chunks
                if let StreamedAssistantContent::Reasoning(ref r) = chunk
                    && r.first_signature().is_none() { continue; }

                let completion_id = format!("{}-{}-completion-{}", group_id, message_id, completion_counter);
                completion_counter += 1;

                if let StreamedAssistantContent::ToolCall { .. } = chunk && has_emitted_tool_call {

                } else {
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

                            if has_emitted_tool_call {
                                tracing::info!("Ignoring batched tool call");
                            } else {
                                has_emitted_tool_call = true;
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
                                    should_loop_back = true;
                                    continue;
                                } else if !tool_names.contains(call.function.name.as_str()) {
                                    // Unknown tool — inject error and retry the whole completion
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
                                    should_loop_back = true;
                                    continue;
                                }

                                // Check for synchronous execution — if the tool provides
                                // synchronous results, inject into history immediately and
                                // continue the completion loop instead of returning. This
                                // prevents race conditions where a concurrent event makes
                                // the tool call appear cancelled.
                                let tool = tool_registry.get(call.function.name.as_str()).expect("bug: tool not found in registry after call");
                                if tool.supports_sync() {
                                    history.sync().await?; // we must sync the history so that thread spawning uses the correct state

                                    let res = tool.execute_synchronous(
                                        &call.function.arguments,
                                        &call.id,
                                        call.call_id.as_deref(),
                                        tool_context,
                                    ).await.expect("bug: synchronous tool execution failed");

                                    yield CompletionEvent::SyncToolCall {
                                        tool_name: call.function.name.clone(),
                                        tool_args: call.function.arguments.clone()
                                    };
                                    yield CompletionEvent::SyncToolResult(res.clone());

                                    let sync_id = format!("{}-sync-result-{}", call.id, completion_counter);
                                    completion_counter += 1;
                                    history.handle_content(
                                        Message::User { content: OneOrMany::one(UserContent::ToolResult(res)) },
                                        sync_id,
                                    ).await?;
                                    should_loop_back = true;
                                } else {
                                    yield CompletionEvent::Action(CompletionAction::ExecuteToolCall {
                                        tool_name: call.function.name.clone(),
                                        tool_args: call.function.arguments.clone(),
                                        tool_call_id: call.id.clone(),
                                        call_id: call.call_id.clone(),
                                    });
                                }
                            }
                        }
                        StreamedAssistantContent::ToolCallDelta { content, .. } => {
                            match content {
                                ToolCallDeltaContent::Name(n) => {
                                    yield CompletionEvent::ThinkingChunk(format!("Invoking tool: {}", n));
                                }
                                ToolCallDeltaContent::Delta(d) => {
                                    yield CompletionEvent::ThinkingChunk(d)
                                }
                            }
                        }
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

                            if should_loop_back {
                                continue 'outer;
                            } else {
                                return;
                            }
                        }
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
#[allow(clippy::collapsible_if, clippy::type_complexity)]
mod tests {
    use super::*;
    use crate::message::{
        InputMessage, InputMessageContent, OAuthRequired, SyntheticKind, TaggedSyntheticKind,
    };
    use crate::traits::{ConversationStore, InputSender, StateStore};
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

    // ── No-op InputSender ──

    #[derive(Clone)]
    struct StubSender;

    #[async_trait]
    impl InputSender for StubSender {
        type Error = TestError;
        async fn send_to_input_queue(
            &self,
            _message: crate::message::InputMessage,
            _group_id: &str,
            _dedup_id: &str,
        ) -> Result<(), TestError> {
            Ok(())
        }
    }

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
        async fn load_history_up_to(
            &self,
            _session_id: &str,
            _start_from: Option<i64>,
            _up_to: Option<i64>,
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
        async fn mark_thread_as_compaction(&self, _thread_id: &str) -> Result<(), TestError> {
            Ok(())
        }
        async fn is_compaction_thread(&self, _thread_id: &str) -> Result<bool, TestError> {
            Ok(false)
        }
        async fn get_thread_spawn_order(&self, _thread_id: &str) -> Result<Option<i64>, TestError> {
            Ok(None)
        }
        async fn save_compaction_summary(
            &self,
            _thread_id: &str,
            _summary: &str,
            _up_to_order: i64,
        ) -> Result<(), TestError> {
            Ok(())
        }
        async fn load_latest_compaction_summary_up_to(
            &self,
            _thread_id: &str,
            _up_to_order: Option<i64>,
        ) -> Result<Option<(String, i64)>, TestError> {
            Ok(None)
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
        let hm =
            HistoryManager::new_with_history(store.clone(), StubStateStore, "thread-1".to_string())
                .await
                .expect("create history manager");
        *hm.history.borrow_mut() = initial_history;
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
        let hm = make_history(&store, vec![]).await;

        let result = prepare_input(
            user_text_msg("thread-1", "hello"),
            "msg-1".to_string(),
            &hm,
            &store,
            &StubSender,
        )
        .await
        .expect("prepare input");

        assert_eq!(result, PrepareResult::Ready);
        insta::assert_json_snapshot!(hm.history.into_inner());
    }

    #[tokio::test]
    async fn closed_thread_ignores() {
        let store = StubConversationStore {
            closed_threads: HashSet::from(["thread-1".to_string()]),
        };
        let hm = make_history(&store, vec![]).await;

        let result = prepare_input(
            user_text_msg("thread-1", "hello"),
            "msg-1".to_string(),
            &hm,
            &store,
            &StubSender,
        )
        .await
        .expect("prepare input");

        assert_eq!(result, PrepareResult::Handled);
        assert!(hm.history.into_inner().is_empty());
    }

    #[tokio::test]
    async fn oauth_required_returns_auth_url() {
        let store = StubConversationStore::new();
        let hm = make_history(&store, vec![]).await;

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

        let result = prepare_input(input, "msg-1".to_string(), &hm, &store, &StubSender)
            .await
            .expect("prepare input");

        insta::assert_json_snapshot!(result);
        assert!(hm.history.into_inner().is_empty());
    }

    #[tokio::test]
    async fn duplicate_message_returns_handled() {
        let store = StubConversationStore::new();
        let hm = make_history(&store, vec![]).await;

        // First call succeeds
        let r1 = prepare_input(
            user_text_msg("thread-1", "hello"),
            "msg-1".to_string(),
            &hm,
            &store,
            &StubSender,
        )
        .await
        .expect("prepare input");
        assert!(matches!(r1, PrepareResult::Ready));

        // Same message_id again
        let r2 = prepare_input(
            user_text_msg("thread-1", "hello"),
            "msg-1".to_string(),
            &hm,
            &store,
            &StubSender,
        )
        .await
        .expect("prepare input");

        assert_eq!(r2, PrepareResult::Handled);
        // History should still have only one user message
        insta::assert_json_snapshot!(hm.history.into_inner());
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
        let hm = make_history(&store, initial).await;

        let result = prepare_input(
            user_text_msg("thread-1", "actually, never mind"),
            "msg-2".to_string(),
            &hm,
            &store,
            &StubSender,
        )
        .await
        .expect("prepare input");

        assert_eq!(result, PrepareResult::Ready);
        // Should have: original user, tool call, synthetic interrupted result, new user msg
        insta::assert_json_snapshot!(hm.history.into_inner());
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
        let hm = make_history(&store, initial).await;

        let input = tool_result_input("thread-1", "tc-1", "tool output", None);

        let result = prepare_input(input, "msg-2".to_string(), &hm, &store, &StubSender)
            .await
            .expect("prepare input");

        assert_eq!(result, PrepareResult::Ready);
        insta::assert_json_snapshot!(hm.history.into_inner());
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
        let hm = make_history(&store, initial).await;
        hm.processed_tool_calls
            .borrow_mut()
            .insert("tc-sub".to_string());

        let input = tool_result_input(
            "thread-1",
            "tc-sub",
            "thread report data",
            Some(SyntheticKind::Tagged(TaggedSyntheticKind::ThreadReport {
                tool_call_id: "tc-sub".to_string(),
                child_thread_id: "thread-1".to_string(),
            })),
        );

        let result = prepare_input(input, "msg-2".to_string(), &hm, &store, &StubSender)
            .await
            .expect("prepare input");

        assert_eq!(result, PrepareResult::Ready);
        // Should have: original user, original tool call, original result, synthetic tool call, synthetic result
        insta::assert_json_snapshot!(
            hm.history.into_inner(),
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
        let hm = make_history(&store, initial).await;

        let input = tool_result_input(
            "thread-1",
            "tc-sub",
            "thread report data",
            Some(SyntheticKind::Tagged(TaggedSyntheticKind::ThreadReport {
                tool_call_id: "tc-sub".to_string(),
                child_thread_id: "thread-1".to_string(),
            })),
        );

        let result = prepare_input(input, "msg-2".to_string(), &hm, &store, &StubSender)
            .await
            .expect("prepare input");

        assert_eq!(result, PrepareResult::Ready);
        insta::assert_json_snapshot!(
            hm.history.into_inner(),
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
        let hm = make_history(&store, initial).await;

        let input = tool_result_input(
            "thread-1",
            "tc-sub",
            "event payload",
            Some(SyntheticKind::Tagged(
                TaggedSyntheticKind::SubscriptionEvent {
                    tool_call_id: "tc-sub".to_string(),
                    associative: false,
                    r#final: false,
                },
            )),
        );

        let result = prepare_input(input, "msg-2".to_string(), &hm, &store, &StubSender)
            .await
            .expect("prepare input");

        assert_eq!(result, PrepareResult::Handled);
        assert_eq!(hm.thread_id, "thread-1");
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
        let hm = make_history(&store, initial).await;

        let input = tool_result_input(
            "thread-1",
            "tc-sub",
            "event payload",
            Some(SyntheticKind::Tagged(
                TaggedSyntheticKind::SubscriptionEvent {
                    tool_call_id: "tc-sub".to_string(),
                    associative: false,
                    r#final: false,
                },
            )),
        );

        let result = prepare_input(input, "msg-2".to_string(), &hm, &store, &StubSender)
            .await
            .expect("prepare input");

        assert_eq!(result, PrepareResult::Handled);
        assert_eq!(hm.thread_id, "thread-1");
    }

    #[tokio::test]
    async fn synthetic_with_missing_tool_call_returns_handled() {
        let store = StubConversationStore::new();
        // Empty history — no tool call to match
        let hm = make_history(&store, vec![]).await;

        let input = tool_result_input(
            "thread-1",
            "nonexistent-tc",
            "some data",
            Some(SyntheticKind::Tagged(
                TaggedSyntheticKind::SubscriptionEvent {
                    tool_call_id: "nonexistent-tc".to_string(),
                    associative: false,
                    r#final: false,
                },
            )),
        );

        let result = prepare_input(input, "msg-1".to_string(), &hm, &store, &StubSender)
            .await
            .expect("prepare input");

        assert_eq!(result, PrepareResult::Handled);
        assert!(hm.history.into_inner().is_empty());
    }

    #[tokio::test]
    async fn metadata_is_updated_before_processing() {
        let store = StubConversationStore::new();
        let hm = make_history(&store, vec![]).await;
        assert!(hm.get_metadata().is_none());

        let input = InputMessage {
            content: InputMessageContent::User(UserContent::text("hi")),
            group_id: "thread-1".to_string(),
            metadata: Some(serde_json::json!({"user_id": "u-123"})),
            synthetic: None,
            display_as: None,
            subscription: false,
        };

        let _ = prepare_input(input, "msg-1".to_string(), &hm, &store, &StubSender)
            .await
            .expect("prepare input");

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
        let hm = make_history(&store, initial).await;
        hm.processed_tool_calls
            .borrow_mut()
            .insert("tc-cmd".to_string());

        let input = tool_result_input(
            "thread-1",
            "tc-cmd",
            "build output chunk\n[exit code: 0]",
            Some(SyntheticKind::Tagged(
                TaggedSyntheticKind::SubscriptionEvent {
                    tool_call_id: "tc-cmd".to_string(),
                    associative: true,
                    r#final: false,
                },
            )),
        );

        let result = prepare_input(input, "msg-2".to_string(), &hm, &store, &StubSender)
            .await
            .expect("prepare input");

        assert_eq!(result, PrepareResult::Ready);
        // Should NOT spawn a subthread — stays in the same thread
        assert_eq!(hm.thread_id, "thread-1");
        // Should have: original user, tool call, original result, synthetic tool call, event result
        insta::assert_json_snapshot!(
            hm.history.into_inner(),
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
        let hm = make_history(&store, initial).await;

        let input = tool_result_input(
            "thread-1",
            "tc-cmd",
            "build output chunk\n[exit code: 0]",
            Some(SyntheticKind::Tagged(
                TaggedSyntheticKind::SubscriptionEvent {
                    tool_call_id: "tc-cmd".to_string(),
                    associative: true,
                    r#final: false,
                },
            )),
        );

        let result = prepare_input(input, "msg-2".to_string(), &hm, &store, &StubSender)
            .await
            .expect("prepare input");

        assert_eq!(result, PrepareResult::Ready);
        // Should NOT spawn a subthread — stays in the same thread
        assert_eq!(hm.thread_id, "thread-1");
        insta::assert_json_snapshot!(
            hm.history.into_inner(),
            { "[3].content[0].id" => "[uuid]", "[4].content[0].id" => "[uuid]" }
        );
    }

    // `run_completion` tests
    use std::collections::HashMap;

    use super::{CompletionAction, CompletionEvent, HistoryManager};
    use crate::tools::{Tool, ToolContext};
    use futures_util::StreamExt;
    use rig::completion::ToolDefinition;
    use rig_mock::mock_model;

    fn tool_context() -> ToolContext<StubSender> {
        ToolContext {
            message_sender: StubSender,
            group_id: "thread-1".into(),
            input_queue_arn: String::new(),
            callback_url: String::new(),
            user_id: None,
            thread_stack: vec!["thread-1".into()],
        }
    }

    fn no_tools() -> (
        HashSet<String>,
        Vec<ToolDefinition>,
        HashMap<String, &'static dyn Tool<StubSender>>,
    ) {
        (HashSet::new(), vec![], HashMap::new())
    }

    // ── Tests ──

    #[tokio::test(flavor = "current_thread")]
    async fn basic_text_completion() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (model, mut ctrl) = mock_model();
                let convo_store = StubConversationStore::new();
                let hm = make_history(
                    &convo_store,
                    vec![Message::User {
                        content: OneOrMany::one(UserContent::text("hello")),
                    }],
                )
                .await;
                let (tool_names, tool_defs, tool_registry) = no_tools();
                let ctx = tool_context();
                let (_cancel_tx, cancel_rx) = tokio::sync::oneshot::channel();

                // Spawn the stream consumer
                let handle = tokio::task::spawn_local(async move {
                    let stream = crate::event_processor::run_completion(
                        &model,
                        &hm,
                        &tool_names,
                        &tool_defs,
                        &tool_registry,
                        &ctx,
                        "thread-1",
                        "msg-1",
                        None,
                        None,
                        None,
                        cancel_rx,
                    );
                    tokio::pin!(stream);
                    let mut texts = Vec::new();
                    let mut got_done = false;
                    while let Some(ev) = stream.next().await {
                        match ev.expect("receive stream event") {
                            CompletionEvent::TextChunk(t) => texts.push(t),
                            CompletionEvent::Action(CompletionAction::Done(_)) => {
                                got_done = true;
                            }
                            _ => {}
                        }
                    }
                    (texts, got_done)
                });

                // Feed the model
                let _req = ctrl.next_request().await;
                ctrl.send_text("Hello ");
                ctrl.send_text("world!");
                ctrl.finish();

                let (texts, got_done) = handle.await.expect("await task handle");
                assert_eq!(texts, vec!["Hello ", "world!"]);
                assert!(got_done);
            })
            .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn cancellation_mid_stream() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (model, mut ctrl) = mock_model();
                let convo_store = StubConversationStore::new();
                let hm = make_history(
                    &convo_store,
                    vec![Message::User {
                        content: OneOrMany::one(UserContent::text("hello")),
                    }],
                )
                .await;
                let (tool_names, tool_defs, tool_registry) = no_tools();
                let ctx = tool_context();
                let (cancel_tx, cancel_rx) = tokio::sync::oneshot::channel();

                let handle = tokio::task::spawn_local(async move {
                    let stream = super::run_completion(
                        &model,
                        &hm,
                        &tool_names,
                        &tool_defs,
                        &tool_registry,
                        &ctx,
                        "thread-1",
                        "msg-1",
                        None,
                        None,
                        None,
                        cancel_rx,
                    );
                    tokio::pin!(stream);
                    let mut texts = Vec::new();
                    let mut got_done = false;
                    while let Some(ev) = stream.next().await {
                        match ev.expect("receive stream event") {
                            CompletionEvent::TextChunk(t) => texts.push(t),
                            CompletionEvent::Action(CompletionAction::Done(_)) => {
                                got_done = true;
                            }
                            _ => {}
                        }
                    }
                    (texts, got_done)
                });

                let _req = ctrl.next_request().await;
                ctrl.send_text("partial");
                // Give the stream a moment to process the chunk
                tokio::task::yield_now().await;
                tokio::task::yield_now().await;
                // Cancel before finishing
                cancel_tx.send(()).expect("send cancel signal");

                let (texts, got_done) = handle.await.expect("await task handle");
                assert_eq!(texts, vec!["partial"]);
                // Should NOT get Done — stream was cancelled
                assert!(!got_done);
            })
            .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn unknown_tool_injects_error_and_retries() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (model, mut ctrl) = mock_model();
                let convo_store = StubConversationStore::new();
                let hm = make_history(
                    &convo_store,
                    vec![Message::User {
                        content: OneOrMany::one(UserContent::text("do it")),
                    }],
                )
                .await;
                let (tool_names, tool_defs, tool_registry) = no_tools();
                let ctx = tool_context();
                let (_cancel_tx, cancel_rx) = tokio::sync::oneshot::channel();

                let handle = tokio::task::spawn_local(async move {
                    let stream = super::run_completion(
                        &model,
                        &hm,
                        &tool_names,
                        &tool_defs,
                        &tool_registry,
                        &ctx,
                        "thread-1",
                        "msg-1",
                        None,
                        None,
                        None,
                        cancel_rx,
                    );
                    tokio::pin!(stream);
                    let mut texts = Vec::new();
                    let mut got_done = false;
                    while let Some(ev) = stream.next().await {
                        match ev.expect("receive stream event") {
                            CompletionEvent::TextChunk(t) => texts.push(t),
                            CompletionEvent::Action(CompletionAction::Done(_)) => {
                                got_done = true;
                            }
                            _ => {}
                        }
                    }
                    (texts, got_done)
                });

                // Round 1: model calls unknown tool
                let _req = ctrl.next_request().await;
                ctrl.send_tool_call("tc-1", "nonexistent_tool", serde_json::json!({}));
                ctrl.finish();

                // Round 2: after error injection, model retries and returns text
                let req2 = ctrl.next_request().await;
                // The history should now contain the error tool result
                let last_msg = req2
                    .chat_history
                    .into_iter()
                    .last()
                    .expect("bug: chat history is empty");
                if let Message::User { content } = &last_msg {
                    if let UserContent::ToolResult(res) = content.first() {
                        if let rig::message::ToolResultContent::Text(t) = res.content.first() {
                            assert!(
                                t.text.contains("does not exist"),
                                "Expected error about nonexistent tool, got: {}",
                                t.text
                            );
                        }
                    }
                }
                ctrl.send_text("ok, done");
                ctrl.finish();

                let (texts, got_done) = handle.await.expect("await task handle");
                assert_eq!(texts, vec!["ok, done"]);
                assert!(got_done);
            })
            .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn receive_event_injected_tool_rejected() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (model, mut ctrl) = mock_model();
                let convo_store = StubConversationStore::new();
                let hm = make_history(
                    &convo_store,
                    vec![Message::User {
                        content: OneOrMany::one(UserContent::text("do it")),
                    }],
                )
                .await;
                let (tool_names, tool_defs, tool_registry) = no_tools();
                let ctx = tool_context();
                let (_cancel_tx, cancel_rx) = tokio::sync::oneshot::channel();

                let handle = tokio::task::spawn_local(async move {
                    let stream = super::run_completion(
                        &model,
                        &hm,
                        &tool_names,
                        &tool_defs,
                        &tool_registry,
                        &ctx,
                        "thread-1",
                        "msg-1",
                        None,
                        None,
                        None,
                        cancel_rx,
                    );
                    tokio::pin!(stream);
                    let mut got_done = false;
                    while let Some(ev) = stream.next().await {
                        if let Ok(CompletionEvent::Action(CompletionAction::Done(_))) = ev {
                            got_done = true;
                        }
                    }
                    got_done
                });

                // Round 1: model tries to call the injected-only tool
                let _req = ctrl.next_request().await;
                ctrl.send_tool_call("tc-1", "receive_event__injected", serde_json::json!({}));
                ctrl.finish();

                // Round 2: model should get error and retry
                let _req2 = ctrl.next_request().await;
                ctrl.send_text("understood");
                ctrl.finish();

                let got_done = handle.await.expect("await task handle");
                assert!(got_done);
            })
            .await;
    }

    // ── Sync tool for testing ──

    struct EchoSyncTool;

    #[async_trait]
    impl Tool<StubSender> for EchoSyncTool {
        fn name(&self) -> &str {
            "echo_sync"
        }
        fn description(&self) -> &str {
            "echoes args"
        }
        fn parameters(&self) -> serde_json::Value {
            serde_json::json!({"type": "object", "properties": {"text": {"type": "string"}}, "required": ["text"]})
        }
        async fn execute(
            &self,
            _: serde_json::Value,
            _: String,
            _: Option<String>,
            _: &ToolContext<StubSender>,
        ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
            Ok(())
        }
        fn supports_sync(&self) -> bool {
            true
        }
        async fn execute_synchronous(
            &self,
            args: &serde_json::Value,
            id: &str,
            call_id: Option<&str>,
            _ctx: &ToolContext<StubSender>,
        ) -> Option<ToolResult> {
            let text = args["text"].as_str().unwrap_or("?");
            Some(ToolResult {
                id: id.to_string(),
                call_id: call_id.map(String::from),
                content: OneOrMany::one(ToolResultContent::Text(rig::agent::Text {
                    text: format!("echo: {}", text),
                })),
            })
        }
    }

    static ECHO_TOOL: EchoSyncTool = EchoSyncTool;

    #[tokio::test(flavor = "current_thread")]
    async fn sync_tool_loops_back_without_new_stream() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (model, mut ctrl) = mock_model();
                let convo_store = StubConversationStore::new();
                let hm = make_history(
                    &convo_store,
                    vec![Message::User {
                        content: OneOrMany::one(UserContent::text("echo something")),
                    }],
                )
                .await;

                let mut tool_names = HashSet::new();
                tool_names.insert("echo_sync".to_string());
                let tool_defs = vec![ToolDefinition {
                    name: "echo_sync".into(),
                    description: "echoes".into(),
                    parameters: serde_json::json!({"type": "object", "properties": {"text": {"type": "string"}}}),
                }];
                let mut tool_registry: HashMap<String, &dyn Tool<StubSender>> = HashMap::new();
                tool_registry.insert("echo_sync".into(), &ECHO_TOOL);
                let ctx = tool_context();
                let (_cancel_tx, cancel_rx) = tokio::sync::oneshot::channel();

                let handle = tokio::task::spawn_local(async move {
                    let stream = super::run_completion(
                        &model,
                        &hm,
                        &tool_names,
                        &tool_defs,
                        &tool_registry,
                        &ctx,
                        "thread-1",
                        "msg-1",
                        None,
                        None,
                        None,
                        cancel_rx,
                    );
                    tokio::pin!(stream);
                    let mut sync_calls = Vec::new();
                    let mut sync_results = Vec::new();
                    let mut texts = Vec::new();
                    while let Some(ev) = stream.next().await {
                        match ev.expect("receive stream event") {
                            CompletionEvent::SyncToolCall { tool_name, .. } => sync_calls.push(tool_name),
                            CompletionEvent::SyncToolResult(res) => {
                                if let ToolResultContent::Text(t) = res.content.first() {
                                    sync_results.push(t.text.clone());
                                }
                            }
                            CompletionEvent::TextChunk(t) => texts.push(t),
                            _ => {}
                        }
                    }
                    (sync_calls, sync_results, texts)
                });

                // Round 1: model calls sync tool
                let _req = ctrl.next_request().await;
                ctrl.send_tool_call("tc-1", "echo_sync", serde_json::json!({"text": "hi"}));
                ctrl.finish();

                // Round 2: model sees the tool result in history and responds with text
                let req2 = ctrl.next_request().await;
                // Verify the tool result is in the history
                let has_echo = req2.chat_history.into_iter().any(|m| {
                    if let Message::User { content } = &m {
                        if let UserContent::ToolResult(res) = content.first() {
                            if let ToolResultContent::Text(t) = res.content.first() {
                                return t.text.contains("echo: hi");
                            }
                        }
                    }
                    false
                });
                assert!(has_echo, "Tool result should be in history for round 2");
                ctrl.send_text("done");
                ctrl.finish();

                let (sync_calls, sync_results, texts) = handle.await.expect("await task handle");
                assert_eq!(sync_calls, vec!["echo_sync"]);
                assert_eq!(sync_results, vec!["echo: hi"]);
                assert_eq!(texts, vec!["done"]);
            })
            .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn thinking_chunks_emitted() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (model, mut ctrl) = mock_model();
                let convo_store = StubConversationStore::new();
                let hm = make_history(
                    &convo_store,
                    vec![Message::User {
                        content: OneOrMany::one(UserContent::text("think hard")),
                    }],
                )
                .await;
                let (tool_names, tool_defs, tool_registry) = no_tools();
                let ctx = tool_context();
                let (_cancel_tx, cancel_rx) = tokio::sync::oneshot::channel();

                let handle = tokio::task::spawn_local(async move {
                    let stream = super::run_completion(
                        &model,
                        &hm,
                        &tool_names,
                        &tool_defs,
                        &tool_registry,
                        &ctx,
                        "thread-1",
                        "msg-1",
                        None,
                        None,
                        None,
                        cancel_rx,
                    );
                    tokio::pin!(stream);
                    let mut events = Vec::new();
                    while let Some(ev) = stream.next().await {
                        match ev.expect("receive stream event") {
                            CompletionEvent::ThinkingStart => events.push("start".to_string()),
                            CompletionEvent::ThinkingEnd => events.push("end".to_string()),
                            CompletionEvent::ThinkingChunk(c) => {
                                events.push(format!("think:{}", c))
                            }
                            CompletionEvent::TextChunk(t) => events.push(format!("text:{}", t)),
                            _ => {}
                        }
                    }
                    events
                });

                let _req = ctrl.next_request().await;
                ctrl.send_chunk(rig::streaming::RawStreamingChoice::ReasoningDelta {
                    id: None,
                    reasoning: "hmm".into(),
                });
                ctrl.send_chunk(rig::streaming::RawStreamingChoice::ReasoningDelta {
                    id: None,
                    reasoning: "...".into(),
                });
                ctrl.send_text("answer");
                ctrl.finish();

                let events = handle.await.expect("await task handle");
                assert_eq!(
                    events,
                    vec!["start", "think:hmm", "think:...", "end", "text:answer"]
                );
            })
            .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn async_tool_call_yields_execute_action() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (model, mut ctrl) = mock_model();
                let convo_store = StubConversationStore::new();
                let hm = make_history(
                    &convo_store,
                    vec![Message::User {
                        content: OneOrMany::one(UserContent::text("run tool")),
                    }],
                )
                .await;

                struct AsyncTool;
                #[async_trait]
                impl Tool<StubSender> for AsyncTool {
                    fn name(&self) -> &str {
                        "async_tool"
                    }
                    fn description(&self) -> &str {
                        "async"
                    }
                    fn parameters(&self) -> serde_json::Value {
                        serde_json::json!({"type": "object", "properties": {}})
                    }
                    async fn execute(
                        &self,
                        _: serde_json::Value,
                        _: String,
                        _: Option<String>,
                        _: &ToolContext<StubSender>,
                    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
                        Ok(())
                    }
                }
                static ASYNC_TOOL: AsyncTool = AsyncTool;

                let mut tool_names = HashSet::new();
                tool_names.insert("async_tool".to_string());
                let tool_defs = vec![ToolDefinition {
                    name: "async_tool".into(),
                    description: "async".into(),
                    parameters: serde_json::json!({"type": "object"}),
                }];
                let mut tool_registry: HashMap<String, &dyn Tool<StubSender>> = HashMap::new();
                tool_registry.insert("async_tool".into(), &ASYNC_TOOL);
                let ctx = tool_context();
                let (_cancel_tx, cancel_rx) = tokio::sync::oneshot::channel();

                let handle = tokio::task::spawn_local(async move {
                    let stream = super::run_completion(
                        &model,
                        &hm,
                        &tool_names,
                        &tool_defs,
                        &tool_registry,
                        &ctx,
                        "thread-1",
                        "msg-1",
                        None,
                        None,
                        None,
                        cancel_rx,
                    );
                    tokio::pin!(stream);
                    let mut tool_call_name = None;
                    while let Some(ev) = stream.next().await {
                        if let Ok(CompletionEvent::Action(CompletionAction::ExecuteToolCall {
                            tool_name,
                            ..
                        })) = ev
                        {
                            tool_call_name = Some(tool_name);
                        }
                    }
                    tool_call_name
                });

                let _req = ctrl.next_request().await;
                ctrl.send_tool_call("tc-1", "async_tool", serde_json::json!({"x": 1}));
                ctrl.finish();

                let name = handle.await.expect("await task handle");
                assert_eq!(name, Some("async_tool".to_string()));
            })
            .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn stream_drop_triggers_retry() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                // When the model stream ends unexpectedly (None from next()), the loop retries.
                let (model, mut ctrl) = mock_model();
                let convo_store = StubConversationStore::new();
                let hm = make_history(
                    &convo_store,
                    vec![Message::User {
                        content: OneOrMany::one(UserContent::text("go")),
                    }],
                )
                .await;
                let (tool_names, tool_defs, tool_registry) = no_tools();
                let ctx = tool_context();
                let (_cancel_tx, cancel_rx) = tokio::sync::oneshot::channel();

                let handle = tokio::task::spawn_local(async move {
                    let stream = super::run_completion(
                        &model,
                        &hm,
                        &tool_names,
                        &tool_defs,
                        &tool_registry,
                        &ctx,
                        "thread-1",
                        "msg-1",
                        None,
                        None,
                        None,
                        cancel_rx,
                    );
                    tokio::pin!(stream);
                    let mut texts = Vec::new();
                    let mut info_count = 0;
                    while let Some(ev) = stream.next().await {
                        match ev.expect("receive stream event") {
                            CompletionEvent::TextChunk(t) => texts.push(t),
                            CompletionEvent::Info(_) => info_count += 1,
                            _ => {}
                        }
                    }
                    (texts, info_count)
                });

                // Round 1: drop the stream without sending Final (simulates unexpected end)
                let _req = ctrl.next_request().await;
                ctrl.drop_stream();

                // Round 2: retry should happen, model responds normally
                let _req2 = ctrl.next_request().await;
                ctrl.send_text("recovered");
                ctrl.finish();

                let (texts, info_count) = handle.await.expect("await task handle");
                assert_eq!(texts, vec!["recovered"]);
                assert!(
                    info_count >= 1,
                    "Should have emitted at least one Info about retry"
                );
            })
            .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn cancellation_during_thinking_emits_thinking_end() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (model, mut ctrl) = mock_model();
                let convo_store = StubConversationStore::new();
                let hm = make_history(
                    &convo_store,
                    vec![Message::User {
                        content: OneOrMany::one(UserContent::text("think")),
                    }],
                )
                .await;
                let (tool_names, tool_defs, tool_registry) = no_tools();
                let ctx = tool_context();
                let (cancel_tx, cancel_rx) = tokio::sync::oneshot::channel();

                let handle = tokio::task::spawn_local(async move {
                    let stream = super::run_completion(
                        &model,
                        &hm,
                        &tool_names,
                        &tool_defs,
                        &tool_registry,
                        &ctx,
                        "thread-1",
                        "msg-1",
                        None,
                        None,
                        None,
                        cancel_rx,
                    );
                    tokio::pin!(stream);
                    let mut events = Vec::new();
                    while let Some(ev) = stream.next().await {
                        match ev.expect("receive stream event") {
                            CompletionEvent::ThinkingStart => events.push("start"),
                            CompletionEvent::ThinkingEnd => events.push("end"),
                            CompletionEvent::ThinkingChunk(_) => events.push("chunk"),
                            _ => {}
                        }
                    }
                    events
                });

                let _req = ctrl.next_request().await;
                ctrl.send_chunk(rig::streaming::RawStreamingChoice::ReasoningDelta {
                    id: None,
                    reasoning: "deep thought".into(),
                });
                tokio::task::yield_now().await;
                tokio::task::yield_now().await;
                cancel_tx.send(()).expect("send cancel signal");

                let events = handle.await.expect("await task handle");
                // Should have: start, chunk, end (end emitted on cancellation)
                assert!(events.contains(&"start"));
                assert!(
                    events.last() == Some(&"end"),
                    "ThinkingEnd should be emitted on cancel, got: {:?}",
                    events
                );
            })
            .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn multiple_sync_tool_calls_chain() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                // Model calls sync tool twice in sequence (two completion rounds), then responds.
                let (model, mut ctrl) = mock_model();
                let convo_store = StubConversationStore::new();
                let hm = make_history(
                    &convo_store,
                    vec![Message::User {
                        content: OneOrMany::one(UserContent::text("echo twice")),
                    }],
                )
                .await;

                let mut tool_names = HashSet::new();
                tool_names.insert("echo_sync".to_string());
                let tool_defs = vec![ToolDefinition {
                    name: "echo_sync".into(),
                    description: "echoes".into(),
                    parameters: serde_json::json!({"type": "object", "properties": {"text": {"type": "string"}}}),
                }];
                let mut tool_registry: HashMap<String, &dyn Tool<StubSender>> = HashMap::new();
                tool_registry.insert("echo_sync".into(), &ECHO_TOOL);
                let ctx = tool_context();
                let (_cancel_tx, cancel_rx) = tokio::sync::oneshot::channel();

                let handle = tokio::task::spawn_local(async move {
                    let stream = super::run_completion(
                        &model,
                        &hm,
                        &tool_names,
                        &tool_defs,
                        &tool_registry,
                        &ctx,
                        "thread-1",
                        "msg-1",
                        None,
                        None,
                        None,
                        cancel_rx,
                    );
                    tokio::pin!(stream);
                    let mut sync_calls = 0;
                    let mut texts = Vec::new();
                    while let Some(ev) = stream.next().await {
                        match ev.expect("receive stream event") {
                            CompletionEvent::SyncToolCall { .. } => sync_calls += 1,
                            CompletionEvent::TextChunk(t) => texts.push(t),
                            _ => {}
                        }
                    }
                    (sync_calls, texts)
                });

                // Round 1: first sync tool call
                let _req = ctrl.next_request().await;
                ctrl.send_tool_call("tc-1", "echo_sync", serde_json::json!({"text": "first"}));
                ctrl.finish();

                // Round 2: second sync tool call
                let _req2 = ctrl.next_request().await;
                ctrl.send_tool_call("tc-2", "echo_sync", serde_json::json!({"text": "second"}));
                ctrl.finish();

                // Round 3: final text response
                let _req3 = ctrl.next_request().await;
                ctrl.send_text("all done");
                ctrl.finish();

                let (sync_calls, texts) = handle.await.expect("await task handle");
                assert_eq!(sync_calls, 2);
                assert_eq!(texts, vec!["all done"]);
            })
            .await;
    }
}
