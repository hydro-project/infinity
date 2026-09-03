use std::{
    cell::{Cell, RefCell},
    collections::{HashMap, HashSet},
    time::Duration,
};

use futures_util::StreamExt;
use infinity_provider_protocol::{
    CompletionRequest, StreamChunk, ToolCallDeltaContent, ToolDefinition,
    message::{AssistantContent, Message, ToolResult, ToolResultContent, UserContent},
};
use rap_protocol::ThreadId;
use serde::Serialize;
use tracing;

use crate::message::{
    InfinityMessage, InputMessage, InputMessageContent, SyntheticKind, TaggedSyntheticKind,
};
use crate::system::AgentEvent;
use crate::tools::{Tool, ToolContext};
use crate::traits::{ConversationStore, InputSender, StateStore};
use infinity_provider_protocol::{ErrorClass, FinalResponse, ModelProvider};

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

#[derive(Debug, Serialize)]
pub struct UserChoiceOutputMessage {
    #[serde(rename = "type")]
    pub message_type: String,
    pub id: String,
    pub prompt: String,
    pub choices: Vec<String>,
    pub default: usize,
    pub metadata: serde_json::Value,
}

#[derive(Debug, Serialize)]
pub struct UserChoiceCompleteOutputMessage {
    #[serde(rename = "type")]
    pub message_type: String,
    pub choice_id: String,
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
pub enum CompletionAction {
    /// Model produced text and is done (no tool call).
    Done(FinalResponse),
    /// Model wants to execute a tool call. Under the RAP protocol tools are
    /// fire-and-forget: the agent loop stops after dispatching the call and
    /// the result arrives as a new input message later.
    ExecuteToolCall {
        tool_name: String,
        tool_args: serde_json::Value,
        tool_call_id: String,
        call_id: Option<String>,
        display_as: Option<String>,
    },
}

/// Items yielded by the completion stream.
pub enum CompletionEvent {
    /// A chunk of text from the model.
    TextChunk(String),
    /// The terminal event — what to do next.
    Action(CompletionAction),
    /// A tool call that was synchronously processed.
    SyncToolCall {
        tool_name: String,
        tool_args: serde_json::Value,
        display_as: Option<String>,
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

#[derive(Serialize, Clone)]
pub struct PendingItem {
    message: InfinityMessage,
    message_id: String,
}

/// Result of matching an incoming tool result against the history tail (see
/// [`HistoryManager::match_tool_result`]).
enum ToolResultMatch {
    /// A matching, still-unanswered tool call exists: the result is fresh.
    Unanswered,
    /// The matching call already has a result in history: duplicate delivery.
    AlreadyAnswered,
    /// No matching call in the live tail: the result is stale.
    NoPendingCall,
}

pub struct HistoryManager<C: ConversationStore, S: StateStore> {
    conversation_store: C,
    state_store: S,
    pub thread_id: ThreadId,
    pub root_thread_id: ThreadId,
    ancestor_chain: Vec<ThreadId>,
    pub history: RefCell<Vec<InfinityMessage>>,
    processed_message_ids: RefCell<HashSet<String>>,
    metadata: RefCell<Option<serde_json::Value>>,
    // Un-persisted content moves through three phases:
    //
    //   1. `unvalidated_items` — inputs (user text, tool results, injected
    //      synthetic results) that the model has not produced output for
    //      yet. They are part of the in-memory `history` (so completions
    //      include them) but are **not** persisted by [`Self::sync`]: one
    //      of them could be the oversized input that blows up the model's
    //      context window, and persisting it would permanently wedge the
    //      thread on a poison message.
    //   2. `pending_items` — known-safe content awaiting persistence.
    //      Inputs are promoted here by
    //      [`Self::mark_inputs_model_validated`] as soon as the model
    //      streams any output for them (proof the context did not
    //      overflow); model-produced content is appended here directly.
    //   3. Synced — persisted to the conversation store by [`Self::sync`].
    //
    // Sequentiality invariant: known-safe content is never appended while
    // unvalidated items exist (enforced by an assert), so `pending_items`
    // always precedes `unvalidated_items` in history order.
    pending_items: RefCell<Vec<PendingItem>>,
    unvalidated_items: RefCell<Vec<PendingItem>>,
    /// until a turn is complete the data lives here. If errors occur,
    /// it'll get discarded, if the turn completes, then it will get flushed to
    /// _both_ pending_items and history.
    turn_buffer: RefCell<Vec<PendingItem>>,
    /// Tool call IDs that were interrupted by a new user message during
    /// `handle_content`. Callers can drain this via `take_interrupted_tool_calls`
    /// to send best-effort cancellation notifications to RAP tool servers.
    interrupted_tool_calls: RefCell<Vec<String>>,
    /// Tracks the absolute store index that the current in-memory compaction
    /// summary covers up to. Used to compute the correct relative split
    /// position when a second compaction is applied on top of an existing one.
    compacted_up_to: RefCell<Option<i64>>,
    /// Number of ancestor messages prepended to the in-memory history.
    /// These messages are NOT in this thread's own store, so they must be
    /// subtracted when computing absolute store indices. Reset to 0 after
    /// compaction replaces ancestors with a summary.
    ancestor_prefix_len: Cell<usize>,
}

impl<C: ConversationStore, S: StateStore> HistoryManager<C, S> {
    pub async fn new_with_history(
        conversation_store: C,
        state_store: S,
        thread_id: ThreadId,
    ) -> Result<Self, BoxError> {
        let _ = conversation_store.ensure_root_thread(&thread_id).await;

        let ancestor_chain: Vec<ThreadId> = conversation_store
            .get_ancestor_chain(&thread_id)
            .await
            .map(|links| links.iter().map(|(tid, _)| tid.clone()).collect())
            .unwrap_or_default();
        let root_thread_id = ancestor_chain
            .first()
            .cloned()
            .unwrap_or_else(|| thread_id.clone());

        let (history, compacted_up_to, ancestor_prefix_len) = conversation_store
            .load_history_with_ancestors(&thread_id)
            .await
            .map_err(|e| Box::new(e) as BoxError)?;

        let metadata = state_store
            .get_metadata(&root_thread_id)
            .await
            .unwrap_or(None);

        let processed_message_ids = state_store
            .get_processed_ids(&thread_id)
            .await
            .unwrap_or_default();

        Ok(Self {
            conversation_store,
            state_store,
            thread_id,
            root_thread_id,
            ancestor_chain,
            history: RefCell::new(history),
            processed_message_ids: RefCell::new(processed_message_ids),
            metadata: RefCell::new(metadata),
            pending_items: RefCell::new(Vec::new()),
            unvalidated_items: RefCell::new(Vec::new()),
            turn_buffer: RefCell::new(Vec::new()),
            interrupted_tool_calls: RefCell::new(Vec::new()),
            compacted_up_to: RefCell::new(compacted_up_to),
            ancestor_prefix_len: Cell::new(ancestor_prefix_len),
        })
    }

    pub fn handle_content(
        &self,
        message: InfinityMessage,
        message_id: String,
    ) -> Result<bool, BoxError> {
        if self.processed_message_ids.borrow().contains(&message_id) {
            tracing::info!("Message {} already processed, skipping", message_id);
            return Ok(false);
        }

        // SubscriptionEvent with an embedded invocation is self-contained —
        // treat it like a non-tool-result (may interrupt a pending call).
        let is_self_contained_subscription = matches!(
            message,
            InfinityMessage::SubscriptionEvent {
                invocation: Some(_),
                ..
            }
        );

        if !is_self_contained_subscription {
            if let Some(tool_result) = message.tool_result() {
                match self.match_tool_result(&tool_result.id) {
                    ToolResultMatch::Unanswered => {}
                    ToolResultMatch::AlreadyAnswered => {
                        tracing::info!(
                            "Tool call {} already processed, ignoring duplicate",
                            tool_result.id
                        );
                        self.processed_message_ids.borrow_mut().insert(message_id);
                        return Ok(false);
                    }
                    ToolResultMatch::NoPendingCall => {
                        tracing::info!(
                            "Got tool call result for wrong call, ignoring {:?}",
                            tool_result
                        );
                        return Ok(false);
                    }
                }
            } else {
                self.interrupt_pending_tool_call();
            }
        } else {
            self.interrupt_pending_tool_call();
        }

        self.append_unvalidated(message, message_id.clone());
        self.processed_message_ids.borrow_mut().insert(message_id);
        Ok(true)
    }

    /// Match an incoming tool result against the tail of history, walking
    /// backwards just in time instead of maintaining a separate index of
    /// answered tool calls. The walk only crosses tool calls and tool
    /// results (future-proofing for concurrent calls, e.g. `tc tc tr tr`);
    /// anything else — user text, assistant content, subscription events —
    /// is a turn boundary: every call before it is settled, so the result
    /// must be stale.
    fn match_tool_result(&self, result_id: &str) -> ToolResultMatch {
        for msg in self.history.borrow().iter().rev() {
            match msg {
                InfinityMessage::ToolCall { call, .. } => {
                    if call.id == result_id {
                        // Its result would have been seen before the call in
                        // a backwards walk, so this call is unanswered.
                        return ToolResultMatch::Unanswered;
                    }
                }
                InfinityMessage::ToolResult { result, .. } => {
                    if result.id == result_id {
                        return ToolResultMatch::AlreadyAnswered;
                    }
                }
                _ => return ToolResultMatch::NoPendingCall,
            }
        }
        ToolResultMatch::NoPendingCall
    }

    /// If the last history entry is an unanswered tool call, inject a
    /// synthetic "interrupted" result. (A tool call at the tail is unanswered
    /// by construction: its result would have been appended after it.)
    fn interrupt_pending_tool_call(&self) {
        let last_call = self.history.borrow().last().and_then(|m| {
            if let InfinityMessage::ToolCall { call, .. } = m {
                Some(call.clone())
            } else {
                None
            }
        });
        if let Some(tool_call) = last_call {
            tracing::info!("Tool call {} interrupted by incoming message", tool_call.id);
            self.interrupted_tool_calls
                .borrow_mut()
                .push(tool_call.id.clone());
            let synthetic_result = InfinityMessage::ToolResult {
                result: ToolResult {
                    id: tool_call.id.clone(),
                    call_id: tool_call.call_id.clone(),
                    content: vec![ToolResultContent::Text(
                        infinity_provider_protocol::message::Text {
                            text: TOOL_CALL_INTERRUPTED_TEXT.to_owned(),
                        },
                    )],
                },
                display_segments: None,
            };
            self.append_unvalidated(synthetic_result, format!("{}-interrupted", tool_call.id));
        }
    }

    /// Buffer a streamed assistant chunk for the current turn. The chunk is
    /// **not** committed to `history` yet — it accumulates in `turn_buffer`
    /// until the turn reaches a flush point ([`Self::flush_turn`]) or is
    /// discarded on failure ([`Self::discard_turn`]). This keeps partial,
    /// possibly-abandoned turns out of committed history.
    pub fn handle_completion(
        &self,
        completion: &StreamChunk,
        completion_id: String,
        display_as: Option<String>,
    ) {
        if self.processed_message_ids.borrow().contains(&completion_id) {
            return;
        }
        // Coalesce consecutive streamed text chunks into a single buffer entry
        // so that a multi-chunk assistant response is persisted as one message
        // rather than one row per chunk (which blows up disk usage).
        if let StreamChunk::Text(text) = completion
            && self.try_merge_buffer_text(text)
        {
            return;
        }
        let infinity_message = match completion {
            StreamChunk::Text(text) => InfinityMessage::Assistant {
                content: AssistantContent::text(text.clone()),
            },
            StreamChunk::Reasoning(r) => InfinityMessage::Assistant {
                content: AssistantContent::Reasoning(r.clone()),
            },
            StreamChunk::ToolCall(call) => InfinityMessage::ToolCall {
                call: call.clone(),
                display_as,
            },
            StreamChunk::ToolCallDelta { .. }
            | StreamChunk::ReasoningDelta { .. }
            | StreamChunk::Final(_) => {
                return;
            }
        };
        self.turn_buffer.borrow_mut().push(PendingItem {
            message: infinity_message,
            message_id: completion_id,
        });
    }

    /// Commit the buffered current-turn content into `history` and
    /// `pending_items`. Called at a flush point: a completed turn (`Final`) or
    /// a turn-ending tool call. After this, the buffered messages are part of
    /// committed history and will be persisted by the next [`Self::sync`].
    ///
    /// Callers must have promoted any unvalidated inputs first (see
    /// [`Self::mark_inputs_model_validated`]): the buffered content is model
    /// output, and model output for a request that included unvalidated
    /// inputs is exactly the proof required to validate them.
    pub fn flush_turn(&self) {
        let drained = std::mem::take(&mut *self.turn_buffer.borrow_mut());
        for item in drained {
            self.append_known_safe(item.message, item.message_id);
        }
    }

    /// Like [`Self::flush_turn`], but first drops any trailing assistant
    /// reasoning (and empty text) from the buffer so the committed turn does not
    /// end on a reasoning block.
    pub fn flush_turn_trimming_reasoning(&self) {
        {
            let mut buffer = self.turn_buffer.borrow_mut();
            // Pop trailing reasoning and empty text together (an empty text
            // entry between two reasoning blocks must not strand the earlier
            // one), stopping at the first non-empty text or any other content.
            while let Some(PendingItem {
                message: InfinityMessage::Assistant { content },
                ..
            }) = buffer.last()
            {
                match content {
                    AssistantContent::Reasoning(_) => {}
                    AssistantContent::Text(text) if text.text.trim().is_empty() => {}
                    _ => break,
                }
                buffer.pop();
            }
        }
        self.flush_turn();
    }

    /// Drop the buffered current-turn content without committing it. Called
    /// when a turn fails mid-stream (timeout, disconnect) and will be retried:
    /// the retry rebuilds the request from clean committed `history`, so the
    /// abandoned partial turn must not linger.
    pub fn discard_turn(&self) {
        self.turn_buffer.borrow_mut().clear();
    }

    pub fn turn_buffer_is_empty(&self) -> bool {
        self.turn_buffer.borrow().is_empty()
    }

    /// Committed history followed by the in-flight buffered turn. Used for
    /// mid-turn subscriber replay so a client connecting while the model is
    /// streaming still sees the partial assistant message.
    pub fn current_turn_view(&self) -> Vec<InfinityMessage> {
        let history = self.history.borrow();
        let buffer = self.turn_buffer.borrow();
        history
            .iter()
            .cloned()
            .chain(buffer.iter().map(|item| item.message.clone()))
            .collect()
    }

    /// Append an *input* (user text, tool result, injected synthetic
    /// result) to the in-memory history. The item is not persistable yet:
    /// it stays in `unvalidated_items` until the model produces output for
    /// it (see [`Self::mark_inputs_model_validated`]).
    fn append_unvalidated(&self, message: InfinityMessage, message_id: String) {
        assert!(
            self.turn_buffer.borrow().is_empty(),
            "bug: append_unvalidated called with un-flushed turn_buffer content"
        );

        self.history.borrow_mut().push(message.clone());
        self.unvalidated_items.borrow_mut().push(PendingItem {
            message,
            message_id,
        });
    }

    /// Append *model-produced* content (assistant text/reasoning, tool
    /// calls) to the in-memory history and the known-safe persistence
    /// queue.
    fn append_known_safe(&self, message: InfinityMessage, message_id: String) {
        assert!(
            self.turn_buffer.borrow().is_empty(),
            "bug: append_known_safe called with un-flushed turn_buffer content"
        );
        // Sequentiality safeguard: known-safe content must never be
        // committed while unvalidated inputs exist, otherwise sync() would
        // persist the model's output without the inputs it answers.
        assert!(
            self.unvalidated_items.borrow().is_empty(),
            "bug: committing model output while unvalidated inputs exist \
             (mark_inputs_model_validated must run first)"
        );

        self.history.borrow_mut().push(message.clone());
        self.pending_items.borrow_mut().push(PendingItem {
            message,
            message_id,
        });
    }

    /// Promote all unvalidated inputs to the known-safe persistence queue.
    ///
    /// Called as soon as the model streams any output for a request that
    /// included them: output means the request was accepted, i.e. the
    /// context window did not overflow, so the inputs are safe to persist.
    pub fn mark_inputs_model_validated(&self) {
        let drained = std::mem::take(&mut *self.unvalidated_items.borrow_mut());
        self.pending_items.borrow_mut().extend(drained);
    }

    /// Number of inputs still awaiting model validation.
    pub fn unvalidated_len(&self) -> usize {
        self.unvalidated_items.borrow().len()
    }

    /// Drop all unvalidated *user* inputs: items that are neither tool
    /// results nor subscription events (those are placeholder-replaceable,
    /// see [`Self::replace_unvalidated_tool_results`], and answering a tool
    /// call is always better than stranding it). Dropped items are removed
    /// from the in-memory history and their dedup IDs forgotten so a
    /// redelivery is not silently ignored. Returns how many items were
    /// dropped.
    ///
    /// Used when the model reports a context overflow: an oversized user
    /// input has no safe substitute, and it must not be persisted (or kept
    /// in memory) or the thread would be permanently wedged on it.
    pub fn drop_unvalidated_user_inputs(&self) -> usize {
        let mut unvalidated = self.unvalidated_items.borrow_mut();
        if unvalidated.is_empty() {
            return 0;
        }
        let mut history = self.history.borrow_mut();
        // By the sequentiality invariant the unvalidated items are exactly
        // the in-memory history tail.
        let tail_start = history.len() - unvalidated.len();
        let mut processed = self.processed_message_ids.borrow_mut();

        let mut kept = Vec::new();
        let mut dropped = 0;
        for item in unvalidated.drain(..) {
            if matches!(
                item.message,
                InfinityMessage::ToolResult { .. } | InfinityMessage::SubscriptionEvent { .. }
            ) {
                kept.push(item);
            } else {
                processed.remove(&item.message_id);
                dropped += 1;
            }
        }
        history.truncate(tail_start);
        history.extend(kept.iter().map(|item| item.message.clone()));
        *unvalidated = kept;
        dropped
    }

    /// Replace the content of every unvalidated tool result — including the
    /// bodies of subscription events, which carry a tool result — with
    /// `placeholder` (in both the persistence queue and the in-memory
    /// history). Returns `true` if at least one was replaced.
    ///
    /// Used on context overflow: unlike user text, a tool result cannot
    /// simply be dropped without stranding its tool call (and a
    /// subscription event should still record that an event arrived), but
    /// both *can* be answered with a fixed placeholder. Bodies already
    /// equal to the placeholder are not counted, so a second overflow
    /// reports `false` and the caller falls back to dropping the inputs.
    pub fn replace_unvalidated_tool_results(&self, placeholder: &str) -> bool {
        let mut unvalidated = self.unvalidated_items.borrow_mut();
        let mut history = self.history.borrow_mut();
        // By the sequentiality invariant the unvalidated items are exactly
        // the in-memory history tail.
        let tail_start = history.len() - unvalidated.len();
        let mut replaced = false;
        for (i, item) in unvalidated.iter_mut().enumerate() {
            let result = match &mut item.message {
                InfinityMessage::ToolResult {
                    result,
                    display_segments,
                } => {
                    *display_segments = None;
                    result
                }
                InfinityMessage::SubscriptionEvent { result, .. } => result.as_mut(),
                _ => continue,
            };
            if matches!(
                result.content.first(),
                Some(ToolResultContent::Text(t)) if t.text == placeholder
            ) {
                continue;
            }
            result.content = vec![ToolResultContent::Text(
                infinity_provider_protocol::message::Text {
                    text: placeholder.to_owned(),
                },
            )];
            history[tail_start + i] = item.message.clone();
            replaced = true;
        }
        replaced
    }

    /// If the last buffered turn entry is an assistant text message, append
    /// `text` to it and return `true`. Otherwise return `false` so the caller
    /// pushes a new buffer entry. This coalesces consecutive text chunks within
    /// a turn into one message.
    fn try_merge_buffer_text(&self, text: &str) -> bool {
        let mut buffer = self.turn_buffer.borrow_mut();
        let Some(last) = buffer.last_mut() else {
            return false;
        };
        let InfinityMessage::Assistant {
            content: AssistantContent::Text(existing),
        } = &mut last.message
        else {
            return false;
        };
        existing.text.push_str(text);
        true
    }

    pub async fn sync(&self) -> Result<(), BoxError> {
        // `sync` only persists known-safe (`pending_items`) content.
        // Unvalidated inputs deliberately stay in memory (see the field
        // docs); any in-flight turn must have been flushed or discarded
        // before this point.
        assert!(
            self.turn_buffer.borrow().is_empty(),
            "bug: sync() called with un-flushed turn_buffer content"
        );

        let pending_items = std::mem::take(&mut *self.pending_items.borrow_mut());
        if pending_items.is_empty() {
            return Ok(());
        }
        let msgs: Vec<(InfinityMessage, String)> = pending_items
            .iter()
            .map(|item| (item.message.clone(), item.message_id.clone()))
            .collect();
        self.conversation_store
            .append_messages(&self.thread_id, msgs)
            .await
            .map_err(|e| Box::new(e) as BoxError)?;
        // Only inputs that are not naturally idempotent need durable dedup
        // IDs: user text and subscription events (a redelivered subscription
        // event would mint a fresh invocation and be appended again). Tool
        // results are deduplicated against the history tail itself, and
        // assistant/tool-call items only exist downstream of a deduped input.
        let msg_ids: Vec<String> = pending_items
            .iter()
            .filter(|i| {
                matches!(
                    i.message,
                    InfinityMessage::User { .. } | InfinityMessage::SubscriptionEvent { .. }
                )
            })
            .map(|i| i.message_id.clone())
            .collect();
        if !msg_ids.is_empty() {
            let _ = self
                .state_store
                .add_processed_message_ids(&self.thread_id, msg_ids)
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
    /// Build the model-facing chat history. When `supports_image_input` is
    /// `false`, image tool-result content is replaced with a text placeholder
    /// so the history can be sent to models without image support.
    pub fn get_history(&self, supports_image_input: bool) -> Vec<Message> {
        self.history
            .borrow()
            .iter()
            .flat_map(|m| m.clone().into_messages())
            .map(|mut msg| {
                if !supports_image_input {
                    strip_image_tool_results(&mut msg);
                }
                msg
            })
            .collect()
    }

    /// Returns the full thread stack: `[root, ..ancestors, current_thread]`.
    /// For the root thread this is just the root thread ID.
    pub fn get_thread_stack(&self) -> Vec<ThreadId> {
        let mut stack = self.ancestor_chain.clone();
        stack.push(self.thread_id.clone());
        stack
    }

    pub fn conversation_store(&self) -> &C {
        &self.conversation_store
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
            // Add ancestor_prefix_len because those messages occupy the beginning
            // of the in-memory history but are not counted by up_to_order (which
            // is relative to this thread's own store).
            let up_to =
                (up_to_order as usize).saturating_sub(offset) + self.ancestor_prefix_len.get();
            let mut history = self.history.borrow_mut();
            if up_to <= history.len() {
                let remaining = history.split_off(up_to);
                *history = vec![InfinityMessage::Assistant {
                    content: AssistantContent::text(format!(
                        "[Compacted conversation summary]\n{}",
                        summary
                    )),
                }];
                history.extend(remaining);
                *self.compacted_up_to.borrow_mut() = Some(up_to_order);
                // After compaction, ancestors are consumed into the summary.
                self.ancestor_prefix_len.set(0);
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

    /// Compute a safe spawn point that excludes trailing unanswered tool calls
    /// and any unvalidated (not yet persistable) inputs. Returns an absolute
    /// store order (accounting for prior compaction offset and ancestor
    /// prefix) suitable for use as `spawn_order_override`.
    pub fn safe_spawn_point(&self) -> usize {
        let history = self.history.borrow();
        // Walk the trailing run of tool calls / tool results (future-proofing
        // for concurrent calls, e.g. `tc tc tr tr`), tracking which calls
        // have their result. The safe point cuts before the deepest
        // unanswered call — excluding it and everything after it. Anything
        // else (user text, assistant content, subscription events) ends the
        // run: every call before it is settled.
        let mut answered: HashSet<&str> = HashSet::new();
        let mut safe = history.len();
        for (i, msg) in history.iter().enumerate().rev() {
            match msg {
                InfinityMessage::ToolCall { call, .. } => {
                    if !answered.contains(call.id.as_str()) {
                        safe = i;
                    }
                }
                InfinityMessage::ToolResult { result, .. } => {
                    answered.insert(result.id.as_str());
                }
                _ => break,
            }
        }
        // Child threads inherit history *from the store*, so the spawn
        // point must also stay before any unvalidated inputs: they occupy
        // the in-memory history tail but have not been persisted yet.
        let validated_len = history.len() - self.unvalidated_items.borrow().len();
        safe = safe.min(validated_len);
        // Convert in-memory index to absolute store order by adding the offset
        // from any prior compaction. The -1 accounts for the compaction summary
        // message occupying slot 0 in the in-memory history.
        let offset = self
            .compacted_up_to
            .borrow()
            .map_or(0, |prev| prev as usize - 1);
        // Subtract ancestor_prefix_len because those messages are not in this
        // thread's own store (they come from parent/ancestor threads).
        safe.saturating_sub(self.ancestor_prefix_len.get()) + offset
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
    if input_msg
        .synthetic
        .as_ref()
        .is_some_and(SyntheticKind::is_compaction_complete)
    {
        tracing::info!("Applying compaction to thread {}", input_msg.group_id);
        current_history.apply_compaction().await?;
        return Ok(PrepareResult::CompactionApplied);
    }

    // Handle compaction trigger: spawn a compaction child thread
    if input_msg
        .synthetic
        .as_ref()
        .is_some_and(SyntheticKind::is_compaction)
    {
        let spawn_call_id = uuid::Uuid::new_v4().to_string();

        // Compute a safe compaction point: exclude trailing unanswered tool calls
        // from the compaction range so they aren't lost when apply_compaction runs.
        let safe_point = current_history.safe_spawn_point();

        let sub_thread_id = conversation_store
            .spawn_thread(&input_msg.group_id, &spawn_call_id, false, Some(safe_point))
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

        // Write spawn tool call directly to child's store.
        // No need to prepend an "interrupted" result for trailing tool calls
        // because the safe_spawn_point already excludes them from the child's
        // inherited history.
        let spawn_tool_call = InfinityMessage::ToolCall {
            call: infinity_provider_protocol::message::ToolCall {
                id: spawn_call_id.clone(),
                call_id: None,
                function: infinity_provider_protocol::message::ToolFunction {
                    name: "__harness_begin_compaction__".to_owned(),
                    arguments: serde_json::json!({}),
                },
            },
            display_as: None,
        };
        conversation_store
            .append_messages(
                &sub_thread_id,
                vec![(
                    spawn_tool_call,
                    format!("{}-compaction-call", spawn_call_id),
                )],
            )
            .await
            .map_err(|e| Box::new(e) as BoxError)?;

        // Send child its instructions via message sender
        let child_msg = InputMessage {
            content: InputMessageContent::User(UserContent::ToolResult(ToolResult {
                id: spawn_call_id.clone(),
                call_id: None,
                content: vec![ToolResultContent::Text(
                    infinity_provider_protocol::message::Text {
                        text: format!(
                            "This tool call was synthetically injected by the harness. You are now INSIDE a compaction thread. You can see the full conversation history inherited from your parent thread, including all ancestor thread context. \
                        Summarize ALL of this content into a concise but comprehensive summary that preserves: all important context, decisions made, \
                        current task progress, relevant code changes and file paths, and any pending work. \
                        Then call close_thread with your thread ID ({}) and include the summary in report_to_parent.",
                            sub_thread_id
                        ),
                    },
                )],
            })),
            group_id: sub_thread_id.clone(),
            metadata: None,
            synthetic: None,
            display_as: None,
            subscription: false,
        };
        message_sender
            .send_to_input_queue(child_msg, &spawn_call_id)
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
    // Capture metadata for SubscriptionEvent variant before synthetic_kind is consumed.
    let subscription_event_meta: Option<(String, Option<ThreadId>)> =
        input_msg.synthetic.as_ref().and_then(|s| {
            if s.is_thread_report() || s.is_associative() || s.is_parent_message() {
                let child_id = if let SyntheticKind::Tagged(TaggedSyntheticKind::ThreadReport {
                    child_thread_id,
                    ..
                }) = s
                {
                    Some(child_thread_id.clone())
                } else {
                    None
                };
                Some((s.tool_call_id().to_owned(), child_id))
            } else {
                None
            }
        });

    // Will be set to the synthetic invocation ToolCall for inlined subscription events.
    let mut subscription_invocation: Option<infinity_provider_protocol::message::ToolCall> = None;

    let content = if let Some(synthetic_kind) = input_msg.synthetic {
        let original_tool_call_id = synthetic_kind.tool_call_id().to_owned();
        let is_final_subscription = synthetic_kind.is_final();
        tracing::info!(
            "Processing synthetic tool result for tool call: {}",
            original_tool_call_id
        );

        let original_call = current_history.history.borrow().iter().find_map(|msg| {
            if let InfinityMessage::ToolCall { call, .. } = msg
                && call.id == original_tool_call_id
            {
                Some(call.clone())
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
                subscription_invocation = Some(infinity_provider_protocol::message::ToolCall {
                    id: new_tool_call_id.clone(),
                    call_id: None,
                    function: infinity_provider_protocol::message::ToolFunction {
                        name: "receive_event__injected".to_owned(),
                        arguments: serde_json::json!({
                            "original_tool_name": original_call.function.name,
                            "original_tool_call_id": original_tool_call_id,
                            "original_args": original_call.function.arguments,
                        }),
                    },
                });
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

            // Compute safe point excluding trailing unanswered tool calls,
            // so the child doesn't inherit them as "interrupted".
            let safe_point = current_history.safe_spawn_point();

            let sub_thread_id = conversation_store
                .spawn_thread(
                    &input_msg.group_id,
                    &original_tool_call_id,
                    true,
                    Some(safe_point),
                )
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

            // No need to prepend an "interrupted" result for trailing tool calls
            // because the safe_spawn_point already excludes them from the child's
            // inherited history.
            let mut child_messages: Vec<(InfinityMessage, String)> = Vec::new();

            // Write event + spawn tool calls directly to child's store
            let event_tool_call = InfinityMessage::ToolCall {
                call: infinity_provider_protocol::message::ToolCall {
                    id: event_call_id.clone(),
                    call_id: None,
                    function: infinity_provider_protocol::message::ToolFunction {
                        name: "receive_event__injected".to_owned(),
                        arguments: serde_json::json!({
                            "original_tool_name": original_call.function.name,
                            "original_tool_call_id": original_tool_call_id,
                            "original_args": original_call.function.arguments,
                        }),
                    },
                },
                display_as: None,
            };
            let spawn_tool_call = InfinityMessage::ToolCall {
                call: infinity_provider_protocol::message::ToolCall {
                    id: spawn_call_id.clone(),
                    call_id: None,
                    function: infinity_provider_protocol::message::ToolFunction {
                        name: "spawn_thread".to_owned(),
                        arguments: serde_json::json!({
                            "instructions": "Spawning thread to process incoming event."
                        }),
                    },
                },
                display_as: None,
            };
            let spawn_tool_result = InfinityMessage::ToolResult {
                result: ToolResult {
                    id: spawn_call_id.clone(),
                    call_id: None,
                    content: vec![ToolResultContent::Text(
                        infinity_provider_protocol::message::Text {
                            text: format!(
                                "You are now INSIDE the thread for processing the single event above. Your thread ID is {}, the parent which is still subscribing is {}. Process the single subscription event above, report to the parent if appropriate, then close the thread after processing this event. Your outputs are NOT VISIBLE to the user, if you want to show them something, send a report to your parent.",
                                sub_thread_id, input_msg.group_id
                            ),
                        },
                    )],
                },
                display_segments: None,
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
                .send_to_input_queue(child_msg, &event_call_id)
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

    let infinity_msg = if let Some((tool_call_id, child_thread_id)) = subscription_event_meta {
        if let UserContent::ToolResult(result) = content {
            InfinityMessage::SubscriptionEvent {
                result: Box::new(result),
                tool_call_id,
                child_thread_id,
                invocation: subscription_invocation.map(Box::new),
            }
        } else {
            InfinityMessage::User { content }
        }
    } else {
        match content {
            UserContent::ToolResult(result) => InfinityMessage::ToolResult {
                result,
                display_segments: input_msg.display_as.clone(),
            },
            other => InfinityMessage::User { content: other },
        }
    };

    let is_new = current_history.handle_content(infinity_msg, message_id.clone())?;

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

/// Compute the [`AgentEvent`] for an input that was just accepted into
/// history. Returns `None` for inputs with no display representation (e.g. a
/// synthetic event whose originating tool call is no longer in history).
pub fn input_event<C, S>(
    current_history: &HistoryManager<C, S>,
    input_msg: &InputMessage,
) -> Option<AgentEvent>
where
    C: ConversationStore,
    S: StateStore,
{
    if let Some(synth) = input_msg.synthetic.as_ref() {
        if let InputMessageContent::User(UserContent::ToolResult(res)) = &input_msg.content
            && let Some(ToolResultContent::Text(text)) = res.content.first()
        {
            let orig_call = current_history.get_history(true).into_iter().find(|h| {
                if let Message::Assistant { content, .. } = h
                    && let Some(AssistantContent::ToolCall(c)) = content.first()
                {
                    c.id == synth.tool_call_id()
                } else {
                    false
                }
            });

            if let Some(Message::Assistant { content, .. }) = orig_call
                && let Some(AssistantContent::ToolCall(c)) = content.first()
            {
                let name = if let SyntheticKind::Tagged(TaggedSyntheticKind::ThreadReport {
                    ref child_thread_id,
                    ..
                }) = *synth
                {
                    format!("Report from child thread {}", child_thread_id)
                } else {
                    format!("{}({})", c.function.name, c.function.arguments)
                };
                return Some(AgentEvent::SubscriptionEvent {
                    name,
                    text: text.text.clone(),
                });
            }
        }
        None
    } else if let InputMessageContent::User(UserContent::ToolResult(res)) = &input_msg.content
        && let Some(ToolResultContent::Text(text)) = res.content.first()
    {
        Some(AgentEvent::ToolResult {
            segments: rap_protocol::build_display_segments(
                input_msg.display_as.as_deref(),
                &text.text,
            ),
        })
    } else if let InputMessageContent::User(UserContent::Text(ref text)) = input_msg.content {
        let display_text = text.text.strip_prefix("<interrupt>").unwrap_or(&text.text);
        Some(AgentEvent::UserInput {
            text: display_text.to_owned(),
        })
    } else {
        None
    }
}

// ═══════════════════════════════════════════════════════════════════════
// (b) run_completion — yields CompletionEvent items (text chunks and a
//     terminal Action). Handles stream errors and unknown tools internally.
// ═══════════════════════════════════════════════════════════════════════

/// Placeholder text substituted for image tool-result content when the
/// active model does not support image inputs.
pub const IMAGE_OMITTED_PLACEHOLDER: &str =
    "[image omitted: the current model does not support image inputs]";

/// Fixed text substituted for a pending tool result when the model reports
/// a context overflow ([`ErrorClass::ContextOverflow`]): the oversized
/// result cannot be sent, but its tool call must still be answered, so the
/// call is resolved with this placeholder instead.
pub const TOOL_RESULT_TOO_LARGE_PLACEHOLDER: &str =
    "[tool result omitted: it was too large for the model's remaining context window]";

/// Text used to settle a tool call as interrupted. Injected when a user
/// message arrives while the call is unanswered, and also substituted for a
/// pending tool result during overflow recovery when user input had to be
/// dropped: the next thing the model sees is a fresh user message, and
/// "interrupted by user" describes that situation accurately, whereas a
/// "too large" placeholder could mislead the model into re-running the
/// tool.
pub const TOOL_CALL_INTERRUPTED_TEXT: &str = "Tool call interrupted by user";

/// Maximum number of retries for one completion round (transient and
/// throttled provider errors).
const MAX_COMPLETION_RETRIES: u32 = 10;
/// Backoff before retrying a [`ErrorClass::Throttled`] provider error.
const THROTTLED_RETRY_DELAY: Duration = Duration::from_secs(30);
/// Backoff before retrying a [`ErrorClass::Transient`] request-initiation
/// error.
const TRANSIENT_RETRY_DELAY: Duration = Duration::from_secs(5);

/// How [`run_completion`] recovered from a provider-declared context
/// overflow (see [`ErrorClass::ContextOverflow`]).
enum OverflowRecovery {
    /// The not-known-safe queue was exactly one replaceable item (tool
    /// result / subscription event) — the culprit is unambiguous, so it was
    /// replaced with [`TOOL_RESULT_TOO_LARGE_PLACEHOLDER`] and the
    /// completion should be retried.
    RetryWithPlaceholder,
    /// Anything else: replaceable items were settled with
    /// [`TOOL_CALL_INTERRUPTED_TEXT`], user inputs were dropped (count
    /// `usize`, they have no safe substitute), and the completion must
    /// fail. Dropping is never followed by a retry — the user clearly
    /// wanted to say something, and silently re-running the round without
    /// their words would act on stale instructions.
    DroppedInputs(usize),
}

/// Recover from a context overflow.
///
/// * If exactly one unvalidated item is pending and it is replaceable (a
///   tool result or subscription event), it must be what overflowed:
///   replace its body with the "too large" placeholder and retry.
/// * Otherwise the culprit is ambiguous (or is user text, which has no safe
///   substitute): settle replaceable items with the same "interrupted by
///   user" text a user interruption would inject — the next thing the model
///   sees is a fresh user message, and a "too large" note could mislead it
///   into re-running the tool — drop the user inputs, and stop.
///
/// TODO(deferral): when the *committed* history is what overflows (shrinking
/// the fresh inputs does not help), the thread should block on a
/// lower-threshold compaction instead of erroring. That needs deferral
/// support for "wait for the in-flight compaction child", which does not
/// exist yet.
fn recover_from_overflow<C: ConversationStore, S: StateStore>(
    history: &HistoryManager<C, S>,
) -> OverflowRecovery {
    if history.unvalidated_len() == 1
        && history.replace_unvalidated_tool_results(TOOL_RESULT_TOO_LARGE_PLACEHOLDER)
    {
        return OverflowRecovery::RetryWithPlaceholder;
    }
    history.replace_unvalidated_tool_results(TOOL_CALL_INTERRUPTED_TEXT);
    OverflowRecovery::DroppedInputs(history.drop_unvalidated_user_inputs())
}

/// Replace image tool-result content with a text placeholder, in place. Used
/// to sanitize the chat history before invoking a model that does not declare
/// image input support (see `ModelEntry::supports_image_input`).
fn strip_image_tool_results(msg: &mut Message) {
    if let Message::User { content } = msg {
        for c in content.iter_mut() {
            if let UserContent::ToolResult(result) = c {
                for item in result.content.iter_mut() {
                    if matches!(item, ToolResultContent::Image(_)) {
                        *item =
                            ToolResultContent::Text(infinity_provider_protocol::message::Text {
                                text: IMAGE_OMITTED_PLACEHOLDER.to_owned(),
                            });
                    }
                }
            }
        }
    }
}

#[expect(
    clippy::too_many_arguments,
    reason = "completion orchestration requires many parameters"
)]
pub fn run_completion<'a: 'b, 'b, P, C, S, M>(
    provider: &'a P,
    model_id: &'a str,
    // Whether the active model accepts image inputs (from its `ModelEntry`).
    // When `false`, image tool results are replaced with a text placeholder
    // before the model is invoked.
    supports_image_input: bool,
    history: &'a HistoryManager<C, S>,
    tool_names: &'a HashSet<String>,
    tools: &'a [ToolDefinition],
    tool_registry: &'a HashMap<String, &'a dyn Tool<M>>,
    tool_context: &'a ToolContext<M>,
    group_id: &'a ThreadId,
    message_id: &'a str,
    extra_system_prompt: Option<&'a str>,
    cancel_rx: tokio::sync::oneshot::Receiver<()>,
) -> impl futures_util::Stream<Item = Result<CompletionEvent, BoxError>> + 'b
where
    P: ModelProvider + ?Sized,
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
            // The turn buffer should be flushed or discarded at the end of each turn.
            assert!(
                history.turn_buffer_is_empty(),
                "bug: entered completion loop with un-flushed turn buffer"
            );

            let stream_result = provider
                .invoke_model(model_id, CompletionRequest {
                    preamble: Some(preamble.clone()),
                    chat_history: history.get_history(supports_image_input),
                    tools: tools.to_vec(),
                    max_tokens: None,
                    additional_params: None,
                });

            // No initiation timeout here: request timeouts are the
            // provider's responsibility (e.g. infinity-provider-bedrock
            // applies its own 60s initiation timeout and reports it as an
            // `ErrorClass::Transient` error). Only cancellation ends the
            // wait.
            let stream_result = tokio::select! {
                r = stream_result => r,
                _ = &mut cancel_rx => {
                    tracing::info!("Completion cancelled during request initiation");
                    // The model never accepted this request, so this
                    // round's inputs remain unvalidated: they stay in
                    // memory for the next round but are not persisted.
                    return;
                }
            };

            let mut llm_stream = match stream_result {
                Ok(s) => s,
                Err(e) => {
                    tracing::error!(error = %e, class = ?e.class(), "Completion stream initiation failed");

                    match e.class() {
                        ErrorClass::Throttled if retry_count < MAX_COMPLETION_RETRIES => {
                            tracing::warn!("Stream error (rate limit), retrying...");
                            yield CompletionEvent::Info(format!(
                                "Stream error (rate limit), retrying after {} seconds...",
                                THROTTLED_RETRY_DELAY.as_secs()
                            ));
                            tokio::select! {
                                _ = tokio::time::sleep(THROTTLED_RETRY_DELAY) => {}
                                _ = &mut cancel_rx => {
                                    tracing::info!("Completion cancelled during retry wait");
                                    return;
                                }
                            }
                            retry_count += 1;
                            continue 'outer;
                        }
                        ErrorClass::Transient if retry_count < MAX_COMPLETION_RETRIES => {
                            tracing::warn!("Stream error ({e}), retrying...");
                            yield CompletionEvent::Info(format!("Stream error ({e}), retrying..."));
                            tokio::select! {
                                _ = tokio::time::sleep(TRANSIENT_RETRY_DELAY) => {}
                                _ = &mut cancel_rx => {
                                    tracing::info!("Completion cancelled during retry wait");
                                    return;
                                }
                            }
                            retry_count += 1;
                            continue 'outer;
                        }
                        ErrorClass::ContextOverflow => {
                            match recover_from_overflow(history) {
                                OverflowRecovery::RetryWithPlaceholder => {
                                    tracing::warn!("Context overflow: replaced oversized tool result with a placeholder, retrying...");
                                    yield CompletionEvent::Info("Tool result too large for the model's context window; replaced it with a placeholder and retrying...".to_owned());
                                    retry_count += 1;
                                    continue 'outer;
                                }
                                OverflowRecovery::DroppedInputs(dropped) => {
                                    if dropped > 0 {
                                        tracing::warn!("Context overflow: dropped {dropped} oversized input message(s)");
                                        yield CompletionEvent::Info("The last input was too large for the model's context window and has been discarded.".to_owned());
                                    }
                                    Err(Into::<BoxError>::into(e))?;
                                    unreachable!()
                                }
                            }
                        }
                        _ => {
                            Err(Into::<BoxError>::into(e))?;
                            unreachable!()
                        }
                    }
                }
            };

            let mut has_emitted_tool_call = false;
            let mut should_loop_back = false;

            loop {
                // Race between LLM output and cancellation signal.
                // We avoid `yield` inside `select!` (async_stream limitation)
                // by capturing the result into locals first.
                //
                // Deliberately no inactivity timer here: once a stream is
                // live it is never artificially cut off by us. Stall
                // handling, if any, belongs to the provider.
                let cancelled;
                let llm_next = tokio::select! {
                    res = llm_stream.next() => { cancelled = false; res },
                    _ = &mut cancel_rx => { cancelled = true; None },
                };

                if cancelled {
                    tracing::info!("Completion cancelled");
                    // Terminal: keep the visible partial text, but trim trailing
                    // reasoning. This path fires on user interruption, so the next
                    // message is a user turn and must not follow a reasoning block.
                    // If the model produced no output yet, this is a no-op and the
                    // round's inputs stay unvalidated (in memory, unpersisted).
                    history.flush_turn_trimming_reasoning();
                    if is_thinking {
                        yield CompletionEvent::ThinkingEnd;
                    }
                    return;
                }

                let Some(res) = llm_next else {
                    if is_thinking {
                        is_thinking = false;
                        yield CompletionEvent::ThinkingEnd;
                    }
                    if retry_count < MAX_COMPLETION_RETRIES {
                        // Retry rebuilds the request from committed history, so
                        // drop the abandoned partial turn.
                        history.discard_turn();
                        yield CompletionEvent::Info("Stream error (unexpected end), retrying...".to_owned());
                        tracing::warn!("Stream ended unexpectedly, discarding partial turn and retrying...");
                        tokio::time::sleep(Duration::from_secs(1)).await;
                        retry_count += 1;
                        continue 'outer;
                    } else {
                        // Giving up: keep visible text, trim trailing reasoning
                        // (the next appended message is a user turn).
                        history.flush_turn_trimming_reasoning();
                        Err(Into::<BoxError>::into("Stream ended unexpectedly"))?;
                        unreachable!()
                    }
                };

                let chunk = match res {
                    Ok(c) => {
                        retry_count = 0;
                        // The model produced output for this request, so its
                        // context accepted the inputs: they are now safe to
                        // persist.
                        history.mark_inputs_model_validated();
                        c
                    },
                    Err(e) => {
                        if is_thinking {
                            is_thinking = false;
                            yield CompletionEvent::ThinkingEnd;
                        }
                        tracing::error!(error = %e, class = ?e.class(), "Completion stream error");
                        match e.class() {
                            ErrorClass::Transient if retry_count < MAX_COMPLETION_RETRIES => {
                                // Retry rebuilds from committed history.
                                history.discard_turn();
                                yield CompletionEvent::Info(format!("Stream error ({e}), retrying..."));
                                tracing::warn!("Stream error (transient), discarding partial turn and retrying...");
                                tokio::time::sleep(Duration::from_secs(1)).await;
                                retry_count += 1;
                                continue 'outer;
                            }
                            ErrorClass::Throttled if retry_count < MAX_COMPLETION_RETRIES => {
                                history.discard_turn();
                                yield CompletionEvent::Info(format!(
                                    "Stream error (rate limit), retrying after {} seconds...",
                                    THROTTLED_RETRY_DELAY.as_secs()
                                ));
                                tokio::select! {
                                    _ = tokio::time::sleep(THROTTLED_RETRY_DELAY) => {}
                                    _ = &mut cancel_rx => {
                                        tracing::info!("Completion cancelled during retry wait");
                                        return;
                                    }
                                }
                                retry_count += 1;
                                continue 'outer;
                            }
                            ErrorClass::ContextOverflow => {
                                // The request never fit the model's context;
                                // any partial turn is unusable.
                                history.discard_turn();
                                match recover_from_overflow(history) {
                                    OverflowRecovery::RetryWithPlaceholder => {
                                        tracing::warn!("Context overflow: replaced oversized tool result with a placeholder, retrying...");
                                        yield CompletionEvent::Info("Tool result too large for the model's context window; replaced it with a placeholder and retrying...".to_owned());
                                        retry_count += 1;
                                        continue 'outer;
                                    }
                                    OverflowRecovery::DroppedInputs(dropped) => {
                                        if dropped > 0 {
                                            tracing::warn!("Context overflow: dropped {dropped} oversized input message(s)");
                                            yield CompletionEvent::Info("The last input was too large for the model's context window and has been discarded.".to_owned());
                                        }
                                        Err(Into::<BoxError>::into(e))?;
                                        unreachable!()
                                    }
                                }
                            }
                            _ => {
                                // Giving up: keep visible text, trim trailing reasoning
                                // (the next appended message is a user turn).
                                history.flush_turn_trimming_reasoning();
                                Err(Into::<BoxError>::into(e))?;
                                unreachable!()
                            }
                        }
                    }
                };

                // Skip incomplete reasoning chunks
                if let StreamChunk::Reasoning(ref r) = chunk
                    && r.first_signature().is_none() { continue; }

                let completion_id = format!("{}-{}-completion-{}", group_id, message_id, completion_counter);
                completion_counter += 1;

                // After the turn's first tool call, the model may keep
                // streaming: Bedrock emits *concurrent* tool calls in one
                // assistant message (each closed content block yields
                // another `ToolCall`), with interleaved reasoning between
                // them. Only the first call is executed — its result
                // arrives in a later round — so everything streamed after
                // it must be dropped here. Forwarding it would surface the
                // ignored calls' argument deltas and reasoning as
                // "thinking" that reaches clients *before* the executed
                // call's result, and committing trailing reasoning/text to
                // history after the tool call would break the
                // `history.last()` match when the result arrives (dropping
                // the result and stranding the call as unanswered). `Final`
                // still flows through below to flush the turn and finish
                // the round.
                if has_emitted_tool_call && !matches!(chunk, StreamChunk::Final(_)) {
                    tracing::info!("Ignoring post-tool-call stream content: {:?}", chunk);
                } else {
                    // Compute display_as for tool calls before inserting into history.
                    let tool_display_as = if let StreamChunk::ToolCall(ref call) = chunk {
                        let ds = tool_registry
                            .get(call.function.name.as_str())
                            .and_then(|t| t.display_script().map(String::from));
                        crate::tools::eval_display_script(ds.as_deref(), &call.function.arguments)
                    } else {
                        None
                    };
                    history.handle_completion(&chunk, completion_id, tool_display_as.clone());
                    match chunk {
                        StreamChunk::Text(text) => {
                            if is_thinking {
                                is_thinking = false;
                                yield CompletionEvent::ThinkingEnd;
                            }
                            tracing::info!("[Text] {}", &text);
                            yield CompletionEvent::TextChunk(text);
                        }
                        StreamChunk::ToolCall(call) => {
                            if is_thinking {
                                is_thinking = false;
                                yield CompletionEvent::ThinkingEnd;
                            }
                            tracing::info!("[Tool Call: {} with arguments {}]", &call.function.name, &call.function.arguments);

                            has_emitted_tool_call = true;
                            // A tool call ends the turn: commit the buffered
                            // assistant content (any preceding text/reasoning
                            // plus this tool call) so it is in `history` before
                            // the tool-result `handle_content` calls below match
                            // on `history.last()`, and so an async tool call is
                            // persisted by the caller's `sync()` before its
                            // result arrives on a later turn.
                            history.flush_turn();
                            if call.function.name == "receive_event__injected" {
                                let tool_result = InfinityMessage::ToolResult {
                                    result: ToolResult {
                                        id: call.id.clone(),
                                        call_id: call.call_id.clone(),
                                        content: vec![ToolResultContent::Text(infinity_provider_protocol::message::Text {
                                            text: format!("Error: you cannot directly invoke {}, invocations will automatically be injected when events arrive.", call.function.name),
                                        })],
                                    },
                                    display_segments: None,
                                };
                                history.handle_content(tool_result, format!("{}-unknown-tool", call.id))?;
                                should_loop_back = true;
                                continue;
                            } else if !tool_names.contains(call.function.name.as_str()) {
                                // Unknown tool — inject error and retry the whole completion
                                tracing::warn!("Unknown tool '{}' called, injecting error and retrying", call.function.name);
                                let tool_result = InfinityMessage::ToolResult {
                                    result: ToolResult {
                                        id: call.id.clone(),
                                        call_id: call.call_id.clone(),
                                        content: vec![ToolResultContent::Text(infinity_provider_protocol::message::Text {
                                            text: format!("Error: tool '{}' does not exist", call.function.name),
                                        })],
                                    },
                                    display_segments: None,
                                };
                                history.handle_content(tool_result, format!("{}-unknown-tool", call.id))?;
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
                                    tool_args: call.function.arguments.clone(),
                                    display_as: tool_display_as,
                                };
                                yield CompletionEvent::SyncToolResult(res.clone());

                                let sync_id = format!("{}-sync-result-{}", call.id, completion_counter);
                                completion_counter += 1;
                                history.handle_content(
                                    InfinityMessage::ToolResult {
                                        result: res,
                                        display_segments: None,
                                    },
                                    sync_id,
                                )?;
                                should_loop_back = true;
                            } else {
                                yield CompletionEvent::Action(CompletionAction::ExecuteToolCall {
                                    tool_name: call.function.name.clone(),
                                    tool_args: call.function.arguments.clone(),
                                    tool_call_id: call.id.clone(),
                                    call_id: call.call_id.clone(),
                                    display_as: tool_display_as,
                                });
                            }
                        }
                        StreamChunk::ToolCallDelta { content, .. } => {
                            match content {
                                ToolCallDeltaContent::Name(n) => {
                                    yield CompletionEvent::ThinkingChunk(format!("Invoking tool: {}", n));
                                }
                                ToolCallDeltaContent::Delta(d) => {
                                    yield CompletionEvent::ThinkingChunk(d)
                                }
                            }
                        }
                        StreamChunk::Reasoning(reasoning) => {
                            if is_thinking {
                                is_thinking = false;
                                yield CompletionEvent::ThinkingEnd;
                            }
                            tracing::info!("[Reasoning: {:?}]", reasoning.first_text());
                        }
                        StreamChunk::ReasoningDelta { text: reasoning, .. } => {
                            if !is_thinking {
                                is_thinking = true;
                                yield CompletionEvent::ThinkingStart;
                            }
                            yield CompletionEvent::ThinkingChunk(reasoning);
                        }
                        StreamChunk::Final(r) => {
                            if is_thinking {
                                yield CompletionEvent::ThinkingEnd;
                            }
                            tracing::info!("Received final message");
                            // Turn complete
                            history.flush_turn();
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

pub async fn execute_action<M>(
    action: CompletionAction,
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
            display_as: _,
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

/// Dispatch `action`; when asynchronous tool execution fails, enqueue a
/// generic error `ToolResult` (with the original tool/call IDs) so the agent
/// can recover instead of waiting forever, then surface the original error.
pub(crate) async fn execute_action_with_error_result<M>(
    action: CompletionAction,
    tool_registry: &HashMap<String, &dyn Tool<M>>,
    tool_context: &ToolContext<M>,
) -> Result<(), crate::tools::ToolError>
where
    M: InputSender + 'static,
{
    let failed_tool_call = match &action {
        CompletionAction::ExecuteToolCall {
            tool_call_id,
            call_id,
            ..
        } => Some((tool_call_id.clone(), call_id.clone())),
        CompletionAction::Done(_) => None,
    };

    if let Err(error) = execute_action(action, tool_registry, tool_context).await {
        if let Some((tool_call_id, call_id)) = failed_tool_call
            && let Err(send_error) = crate::tools::send_tool_error(
                tool_context,
                &tool_call_id,
                call_id,
                "Tool call failed",
            )
            .await
        {
            tracing::error!(
                "failed to send fallback result after tool execution error: {}",
                send_error
            );
        }
        return Err(error);
    }

    Ok(())
}

#[cfg(test)]
#[expect(
    clippy::collapsible_if,
    clippy::type_complexity,
    reason = "test readability"
)]
mod tests {
    use super::*;
    use crate::message::{
        InputMessage, InputMessageContent, OAuthRequired, SyntheticKind, TaggedSyntheticKind,
    };
    use crate::stores::{InMemoryConversationStore, InMemoryStateStore};
    use crate::traits::{ConversationStore, InputSender};
    use async_trait::async_trait;
    use infinity_provider_protocol::message::{
        AssistantContent, Message, ToolCall, ToolFunction, ToolResult, ToolResultContent,
        UserContent,
    };
    use std::collections::HashSet;

    // ── No-op InputSender ──

    #[derive(Clone)]
    struct StubSender;

    #[async_trait]
    impl InputSender for StubSender {
        type Error = std::io::Error;
        async fn send_to_input_queue(
            &self,
            _message: crate::message::InputMessage,
            _dedup_id: &str,
        ) -> Result<(), std::io::Error> {
            Ok(())
        }
    }

    // ── Helpers ──

    async fn make_history(
        store: &InMemoryConversationStore,
        initial_history: Vec<Message>,
    ) -> HistoryManager<InMemoryConversationStore, InMemoryStateStore> {
        let hm = HistoryManager::new_with_history(
            store.clone(),
            InMemoryStateStore::new(),
            "thread-1".into(),
        )
        .await
        .expect("create history manager");
        *hm.history.borrow_mut() = initial_history
            .into_iter()
            .map(InfinityMessage::from_message)
            .collect();
        hm
    }

    fn user_text_msg(group_id: &str, text: &str) -> InputMessage {
        InputMessage {
            content: InputMessageContent::User(UserContent::text(text)),
            group_id: group_id.into(),
            metadata: None,
            synthetic: None,
            display_as: None,
            subscription: false,
        }
    }

    fn tool_call_msg(id: &str, name: &str, args: serde_json::Value) -> Message {
        Message::Assistant {
            content: vec![AssistantContent::ToolCall(ToolCall {
                id: id.to_owned(),
                call_id: None,
                function: ToolFunction {
                    name: name.to_owned(),
                    arguments: args,
                },
            })],
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
                id: tool_call_id.to_owned(),
                call_id: None,
                content: vec![ToolResultContent::Text(
                    infinity_provider_protocol::message::Text {
                        text: result_text.to_owned(),
                    },
                )],
            })),
            group_id: group_id.into(),
            metadata: None,
            synthetic,
            display_as: None,
            subscription: false,
        }
    }

    // ── Tests ──

    #[tokio::test]
    async fn simple_user_message_on_empty_history() {
        let store = InMemoryConversationStore::new();
        let hm = make_history(&store, vec![]).await;

        let result = prepare_input(
            user_text_msg("thread-1", "hello"),
            "msg-1".to_owned(),
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
    async fn consecutive_text_chunks_are_coalesced() {
        let store = InMemoryConversationStore::new();
        let hm = make_history(&store, vec![]).await;

        let text_chunk = |s: &str| StreamChunk::Text(s.to_owned());

        hm.handle_completion(&text_chunk("Hello"), "c-1".to_owned(), None);
        hm.handle_completion(&text_chunk(", "), "c-2".to_owned(), None);
        hm.handle_completion(&text_chunk("world"), "c-3".to_owned(), None);

        // While streaming, the chunks accumulate in the buffer (not yet
        // committed) and coalesce into a single entry.
        assert_eq!(hm.turn_buffer.borrow().len(), 1);
        assert!(hm.history.borrow().is_empty());

        // Committing the turn moves the single coalesced message into both the
        // pending items (to be persisted) and the in-memory history.
        hm.flush_turn();
        insta::assert_json_snapshot!(hm.pending_items.borrow().clone());
        insta::assert_json_snapshot!(hm.history.borrow().clone());
    }

    #[tokio::test]
    async fn text_chunks_not_coalesced_across_non_text_item() {
        let store = InMemoryConversationStore::new();
        let hm = make_history(&store, vec![]).await;

        let text_chunk = |s: &str| StreamChunk::Text(s.to_owned());
        let tool_call = StreamChunk::ToolCall(ToolCall {
            id: "tc-1".to_owned(),
            call_id: None,
            function: ToolFunction {
                name: "some_tool".to_owned(),
                arguments: serde_json::json!({}),
            },
        });

        hm.handle_completion(&text_chunk("before"), "c-1".to_owned(), None);
        // A non-text item in between breaks the run of text chunks.
        hm.handle_completion(&tool_call, "c-2".to_owned(), None);
        hm.handle_completion(&text_chunk("after"), "c-3".to_owned(), None);

        // The tool call between the two text chunks should prevent them from
        // being coalesced, leaving three distinct buffered items which commit
        // in order on flush.
        hm.flush_turn();
        insta::assert_json_snapshot!(hm.pending_items.borrow().clone());
        insta::assert_json_snapshot!(hm.history.borrow().clone());
    }

    #[tokio::test]
    async fn buffered_turn_is_not_committed_until_flush() {
        let store = InMemoryConversationStore::new();
        let hm = make_history(
            &store,
            vec![Message::User {
                content: vec![UserContent::text("do the thing")],
            }],
        )
        .await;

        let reasoning = StreamChunk::Reasoning(
            infinity_provider_protocol::message::Reasoning::new_with_signature(
                "thinking",
                Some("sig".to_owned()),
            ),
        );
        let text = StreamChunk::Text("partial answer".to_owned());
        hm.handle_completion(&reasoning, "c-1".to_owned(), None);
        hm.handle_completion(&text, "c-2".to_owned(), None);

        // Mid-turn: committed history still ends on the user message; the
        // streamed content lives only in the buffer, so nothing is persisted or
        // sent on a rebuild.
        assert_eq!(hm.history.borrow().len(), 1);
        assert!(hm.pending_items.borrow().is_empty());
        assert_eq!(hm.turn_buffer.borrow().len(), 2);

        hm.flush_turn();

        // After flush the buffered content is committed to history + pending
        // items and the buffer is empty.
        assert_eq!(hm.history.borrow().len(), 3);
        assert_eq!(hm.pending_items.borrow().len(), 2);
        assert!(hm.turn_buffer.borrow().is_empty());
    }

    #[tokio::test]
    async fn discard_turn_drops_buffer_leaving_history_clean() {
        let store = InMemoryConversationStore::new();
        let hm = make_history(
            &store,
            vec![Message::User {
                content: vec![UserContent::text("do the thing")],
            }],
        )
        .await;

        let text = StreamChunk::Text("partial answer".to_owned());
        hm.handle_completion(&text, "c-1".to_owned(), None);
        assert_eq!(hm.turn_buffer.borrow().len(), 1);

        hm.discard_turn();

        // The abandoned partial turn is gone; committed history is untouched, so
        // a rebuilt request ends on the user message.
        assert!(hm.turn_buffer.borrow().is_empty());
        assert_eq!(hm.history.borrow().len(), 1);
        assert!(matches!(
            hm.history.borrow().last(),
            Some(InfinityMessage::User { .. })
        ));
    }

    #[tokio::test]
    async fn flush_turn_trimming_reasoning_keeps_text_drops_trailing_reasoning() {
        let store = InMemoryConversationStore::new();
        let hm = make_history(
            &store,
            vec![Message::User {
                content: vec![UserContent::text("do the thing")],
            }],
        )
        .await;

        // A turn that streamed visible text and then a (complete) reasoning
        // block before being abandoned — e.g. the user interrupted it.
        let text = StreamChunk::Text("here is the answer".to_owned());
        let reasoning = StreamChunk::Reasoning(
            infinity_provider_protocol::message::Reasoning::new_with_signature(
                "still thinking",
                Some("sig".to_owned()),
            ),
        );
        hm.handle_completion(&text, "c-1".to_owned(), None);
        hm.handle_completion(&reasoning, "c-2".to_owned(), None);
        assert_eq!(hm.turn_buffer.borrow().len(), 2);

        hm.flush_turn_trimming_reasoning();

        // The visible text is preserved, but the trailing reasoning is dropped
        // so committed history does not end on a reasoning block (which the next
        // user turn would then illegally follow).
        assert!(hm.turn_buffer.borrow().is_empty());
        let history = hm.history.borrow();
        assert_eq!(history.len(), 2);
        assert!(matches!(
            history.last(),
            Some(InfinityMessage::Assistant {
                content: AssistantContent::Text(t),
            }) if t.text == "here is the answer"
        ));
    }

    #[tokio::test]
    async fn flush_turn_trimming_reasoning_trims_interleaved_empty_text() {
        let store = InMemoryConversationStore::new();
        let hm = make_history(
            &store,
            vec![Message::User {
                content: vec![UserContent::text("do the thing")],
            }],
        )
        .await;

        // reasoning, then an empty-text entry, then more reasoning at the tail:
        // trimming must remove all three (the empty text must not strand the
        // earlier reasoning block), leaving history ending on the user message.
        let reasoning = |s: &str| {
            StreamChunk::Reasoning(
                infinity_provider_protocol::message::Reasoning::new_with_signature(
                    s,
                    Some("sig".to_owned()),
                ),
            )
        };
        let empty_text = StreamChunk::Text("  ".to_owned());
        hm.handle_completion(&reasoning("first"), "c-1".to_owned(), None);
        hm.handle_completion(&empty_text, "c-2".to_owned(), None);
        hm.handle_completion(&reasoning("second"), "c-3".to_owned(), None);

        hm.flush_turn_trimming_reasoning();

        assert!(hm.turn_buffer.borrow().is_empty());
        let history = hm.history.borrow();
        assert_eq!(history.len(), 1);
        assert!(matches!(history.last(), Some(InfinityMessage::User { .. })));
    }

    #[tokio::test]
    async fn current_turn_view_appends_buffer_to_history() {
        let store = InMemoryConversationStore::new();
        let hm = make_history(
            &store,
            vec![Message::User {
                content: vec![UserContent::text("hi")],
            }],
        )
        .await;

        // With an empty buffer the view equals committed history.
        assert_eq!(hm.current_turn_view().len(), 1);

        let text = StreamChunk::Text("streaming...".to_owned());
        hm.handle_completion(&text, "c-1".to_owned(), None);

        // Mid-turn the view includes the buffered partial message even though
        // it is not yet in committed history (for subscriber replay).
        let view = hm.current_turn_view();
        assert_eq!(view.len(), 2);
        assert_eq!(hm.history.borrow().len(), 1);
        assert!(matches!(
            view.last(),
            Some(InfinityMessage::Assistant { .. })
        ));
    }

    #[tokio::test]
    async fn closed_thread_ignores() {
        let store = InMemoryConversationStore::new();
        store
            .ensure_root_thread(ThreadId::from_ref("thread-1"))
            .await
            .expect("testing");
        store
            .close_thread(ThreadId::from_ref("thread-1"))
            .await
            .expect("testing");
        let hm = make_history(&store, vec![]).await;

        let result = prepare_input(
            user_text_msg("thread-1", "hello"),
            "msg-1".to_owned(),
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
        let store = InMemoryConversationStore::new();
        let hm = make_history(&store, vec![]).await;

        let input = InputMessage {
            content: InputMessageContent::OAuth(OAuthRequired {
                content_type: "oauth_required".to_owned(),
                id: "oauth-1".to_owned(),
                call_id: None,
                auth_url: "https://example.com/auth".to_owned(),
            }),
            group_id: "thread-1".into(),
            metadata: None,
            synthetic: None,
            display_as: None,
            subscription: false,
        };

        let result = prepare_input(input, "msg-1".to_owned(), &hm, &store, &StubSender)
            .await
            .expect("prepare input");

        insta::assert_json_snapshot!(result);
        assert!(hm.history.into_inner().is_empty());
    }

    #[tokio::test]
    async fn duplicate_message_returns_handled() {
        let store = InMemoryConversationStore::new();
        let hm = make_history(&store, vec![]).await;

        // First call succeeds
        let r1 = prepare_input(
            user_text_msg("thread-1", "hello"),
            "msg-1".to_owned(),
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
            "msg-1".to_owned(),
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
        let store = InMemoryConversationStore::new();
        // History has a user msg, then an assistant tool call that hasn't been answered
        let initial = vec![
            Message::User {
                content: vec![UserContent::text("do something")],
            },
            tool_call_msg("tc-1", "some_tool", serde_json::json!({"x": 1})),
        ];
        let hm = make_history(&store, initial).await;

        let result = prepare_input(
            user_text_msg("thread-1", "actually, never mind"),
            "msg-2".to_owned(),
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
        let store = InMemoryConversationStore::new();
        let initial = vec![
            Message::User {
                content: vec![UserContent::text("do something")],
            },
            tool_call_msg("tc-1", "some_tool", serde_json::json!({"x": 1})),
        ];
        let hm = make_history(&store, initial).await;

        let input = tool_result_input("thread-1", "tc-1", "tool output", None);

        let result = prepare_input(input, "msg-2".to_owned(), &hm, &store, &StubSender)
            .await
            .expect("prepare input");

        assert_eq!(result, PrepareResult::Ready);
        insta::assert_json_snapshot!(hm.history.into_inner());
    }

    #[tokio::test]
    async fn thread_report_synthetic_event() {
        let store = InMemoryConversationStore::new();
        // Tool call already completed before the thread report arrives
        let initial = vec![
            Message::User {
                content: vec![UserContent::text("subscribe")],
            },
            tool_call_msg(
                "tc-sub",
                "subscribe_tool",
                serde_json::json!({"topic": "events"}),
            ),
            Message::User {
                content: vec![UserContent::ToolResult(ToolResult {
                    id: "tc-sub".to_owned(),
                    call_id: None,
                    content: vec![ToolResultContent::Text(
                        infinity_provider_protocol::message::Text {
                            text: "subscribed successfully".to_owned(),
                        },
                    )],
                })],
            },
        ];
        let hm = make_history(&store, initial).await;

        let input = tool_result_input(
            "thread-1",
            "tc-sub",
            "thread report data",
            Some(SyntheticKind::Tagged(TaggedSyntheticKind::ThreadReport {
                tool_call_id: "tc-sub".to_owned(),
                child_thread_id: "thread-1".into(),
            })),
        );

        let result = prepare_input(input, "msg-2".to_owned(), &hm, &store, &StubSender)
            .await
            .expect("prepare input");

        assert_eq!(result, PrepareResult::Ready);
        // Should have: original user, original tool call, original result, subscription event (with embedded invocation)
        insta::assert_json_snapshot!(
            hm.history.into_inner(),
            { "[3].result.id" => "[uuid]", "[3].invocation.id" => "[uuid]" }
        );
    }

    #[tokio::test]
    async fn thread_report_tool_interruption() {
        let store = InMemoryConversationStore::new();
        // Tool call is still pending when the thread report arrives
        let initial = vec![
            Message::User {
                content: vec![UserContent::text("subscribe")],
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
                tool_call_id: "tc-sub".to_owned(),
                child_thread_id: "thread-1".into(),
            })),
        );

        let result = prepare_input(input, "msg-2".to_owned(), &hm, &store, &StubSender)
            .await
            .expect("prepare input");

        assert_eq!(result, PrepareResult::Ready);
        insta::assert_json_snapshot!(
            hm.history.into_inner(),
            { "[3].result.id" => "[uuid]", "[3].invocation.id" => "[uuid]" }
        );
    }

    #[tokio::test]
    async fn subscription_event_spawned_thread() {
        let store = InMemoryConversationStore::new();
        // Tool call already completed with a result before the event arrives
        let initial = vec![
            Message::User {
                content: vec![UserContent::text("subscribe")],
            },
            tool_call_msg(
                "tc-sub",
                "subscribe_tool",
                serde_json::json!({"topic": "events"}),
            ),
            Message::User {
                content: vec![UserContent::ToolResult(ToolResult {
                    id: "tc-sub".to_owned(),
                    call_id: None,
                    content: vec![ToolResultContent::Text(
                        infinity_provider_protocol::message::Text {
                            text: "subscribed successfully".to_owned(),
                        },
                    )],
                })],
            },
        ];
        let hm = make_history(&store, initial).await;

        let input = tool_result_input(
            "thread-1",
            "tc-sub",
            "event payload",
            Some(SyntheticKind::Tagged(
                TaggedSyntheticKind::SubscriptionEvent {
                    tool_call_id: "tc-sub".to_owned(),
                    associative: false,
                    r#final: false,
                },
            )),
        );

        let result = prepare_input(input, "msg-2".to_owned(), &hm, &store, &StubSender)
            .await
            .expect("prepare input");

        assert_eq!(result, PrepareResult::Handled);
        assert_eq!(hm.thread_id.as_str(), "thread-1");
    }

    #[tokio::test]
    async fn subscription_event_tool_interruption() {
        let store = InMemoryConversationStore::new();
        // Tool call is still pending (no result yet) when the event arrives
        let initial = vec![
            Message::User {
                content: vec![UserContent::text("subscribe")],
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
                    tool_call_id: "tc-sub".to_owned(),
                    associative: false,
                    r#final: false,
                },
            )),
        );

        let result = prepare_input(input, "msg-2".to_owned(), &hm, &store, &StubSender)
            .await
            .expect("prepare input");

        assert_eq!(result, PrepareResult::Handled);
        assert_eq!(hm.thread_id.as_str(), "thread-1");
    }

    #[tokio::test]
    async fn synthetic_with_missing_tool_call_returns_handled() {
        let store = InMemoryConversationStore::new();
        // Empty history — no tool call to match
        let hm = make_history(&store, vec![]).await;

        let input = tool_result_input(
            "thread-1",
            "nonexistent-tc",
            "some data",
            Some(SyntheticKind::Tagged(
                TaggedSyntheticKind::SubscriptionEvent {
                    tool_call_id: "nonexistent-tc".to_owned(),
                    associative: false,
                    r#final: false,
                },
            )),
        );

        let result = prepare_input(input, "msg-1".to_owned(), &hm, &store, &StubSender)
            .await
            .expect("prepare input");

        assert_eq!(result, PrepareResult::Handled);
        assert!(hm.history.into_inner().is_empty());
    }

    #[tokio::test]
    async fn metadata_is_updated_before_processing() {
        let store = InMemoryConversationStore::new();
        let hm = make_history(&store, vec![]).await;
        assert!(hm.get_metadata().is_none());

        let input = InputMessage {
            content: InputMessageContent::User(UserContent::text("hi")),
            group_id: "thread-1".into(),
            metadata: Some(serde_json::json!({"user_id": "u-123"})),
            synthetic: None,
            display_as: None,
            subscription: false,
        };

        let _ = prepare_input(input, "msg-1".to_owned(), &hm, &store, &StubSender)
            .await
            .expect("prepare input");

        insta::assert_json_snapshot!(hm.get_metadata());
    }

    #[tokio::test]
    async fn associative_subscription_event_inlined() {
        let store = InMemoryConversationStore::new();
        // Tool call already completed with a result before the associative event arrives
        let initial = vec![
            Message::User {
                content: vec![UserContent::text("run command")],
            },
            tool_call_msg(
                "tc-cmd",
                "execute_command",
                serde_json::json!({"command": "make build"}),
            ),
            Message::User {
                content: vec![UserContent::ToolResult(ToolResult {
                    id: "tc-cmd".to_owned(),
                    call_id: None,
                    content: vec![ToolResultContent::Text(infinity_provider_protocol::message::Text {
                        text: "Command is still running. Output will be streamed via subscription events.".to_owned(),
                    })],
                })],
            },
        ];
        let hm = make_history(&store, initial).await;

        let input = tool_result_input(
            "thread-1",
            "tc-cmd",
            "build output chunk\n[exit code: 0]",
            Some(SyntheticKind::Tagged(
                TaggedSyntheticKind::SubscriptionEvent {
                    tool_call_id: "tc-cmd".to_owned(),
                    associative: true,
                    r#final: false,
                },
            )),
        );

        let result = prepare_input(input, "msg-2".to_owned(), &hm, &store, &StubSender)
            .await
            .expect("prepare input");

        assert_eq!(result, PrepareResult::Ready);
        // Should NOT spawn a subthread — stays in the same thread
        assert_eq!(hm.thread_id.as_str(), "thread-1");
        // Should have: original user, tool call, original result, subscription event (with embedded invocation)
        insta::assert_json_snapshot!(
            hm.history.into_inner(),
            { "[3].result.id" => "[uuid]", "[3].invocation.id" => "[uuid]" }
        );
    }

    #[tokio::test]
    async fn associative_subscription_event_tool_interruption() {
        let store = InMemoryConversationStore::new();
        // Tool call is still pending (no result yet) when the associative event arrives
        let initial = vec![
            Message::User {
                content: vec![UserContent::text("run command")],
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
                    tool_call_id: "tc-cmd".to_owned(),
                    associative: true,
                    r#final: false,
                },
            )),
        );

        let result = prepare_input(input, "msg-2".to_owned(), &hm, &store, &StubSender)
            .await
            .expect("prepare input");

        assert_eq!(result, PrepareResult::Ready);
        // Should NOT spawn a subthread — stays in the same thread
        assert_eq!(hm.thread_id.as_str(), "thread-1");
        insta::assert_json_snapshot!(
            hm.history.into_inner(),
            { "[3].result.id" => "[uuid]", "[3].invocation.id" => "[uuid]" }
        );
    }

    // `run_completion` tests
    use std::collections::HashMap;

    use super::{CompletionAction, CompletionEvent, HistoryManager};
    use crate::test_helpers::{mock_provider, mock_provider_with_image_support};
    use crate::tools::{Tool, ToolContext};
    use futures_util::StreamExt;
    use infinity_provider_protocol::ToolDefinition;
    use rap_protocol::ThreadId;

    fn tool_context() -> ToolContext<StubSender> {
        ToolContext {
            message_sender: StubSender,
            group_id: "thread-1".into(),
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
                let (provider, mut ctrl) = mock_provider();
                let convo_store = InMemoryConversationStore::new();
                let hm = make_history(
                    &convo_store,
                    vec![Message::User {
                        content: vec![UserContent::text("hello")],
                    }],
                )
                .await;
                let (tool_names, tool_defs, tool_registry) = no_tools();
                let ctx = tool_context();
                let (_cancel_tx, cancel_rx) = tokio::sync::oneshot::channel();

                // Spawn the stream consumer
                let handle = tokio::task::spawn_local(async move {
                    let thread_id = ThreadId::from("thread-1");
                    let stream = run_completion(
                        &provider,
                        "mock",
                        false,
                        &hm,
                        &tool_names,
                        &tool_defs,
                        &tool_registry,
                        &ctx,
                        &thread_id,
                        "msg-1",
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
                let (provider, mut ctrl) = mock_provider();
                let convo_store = InMemoryConversationStore::new();
                let hm = make_history(
                    &convo_store,
                    vec![Message::User {
                        content: vec![UserContent::text("hello")],
                    }],
                )
                .await;
                let (tool_names, tool_defs, tool_registry) = no_tools();
                let ctx = tool_context();
                let (cancel_tx, cancel_rx) = tokio::sync::oneshot::channel();

                let handle = tokio::task::spawn_local(async move {
                    let thread_id = ThreadId::from("thread-1");
                    let stream = run_completion(
                        &provider,
                        "mock",
                        false,
                        &hm,
                        &tool_names,
                        &tool_defs,
                        &tool_registry,
                        &ctx,
                        &thread_id,
                        "msg-1",
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
                let (provider, mut ctrl) = mock_provider();
                let convo_store = InMemoryConversationStore::new();
                let hm = make_history(
                    &convo_store,
                    vec![Message::User {
                        content: vec![UserContent::text("do it")],
                    }],
                )
                .await;
                let (tool_names, tool_defs, tool_registry) = no_tools();
                let ctx = tool_context();
                let (_cancel_tx, cancel_rx) = tokio::sync::oneshot::channel();

                let handle = tokio::task::spawn_local(async move {
                    let thread_id = ThreadId::from("thread-1");
                    let stream = run_completion(
                        &provider,
                        "mock",
                        false,
                        &hm,
                        &tool_names,
                        &tool_defs,
                        &tool_registry,
                        &ctx,
                        &thread_id,
                        "msg-1",
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
                    if let Some(UserContent::ToolResult(res)) = content.first() {
                        if let Some(infinity_provider_protocol::message::ToolResultContent::Text(
                            t,
                        )) = res.content.first()
                        {
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
                let (provider, mut ctrl) = mock_provider();
                let convo_store = InMemoryConversationStore::new();
                let hm = make_history(
                    &convo_store,
                    vec![Message::User {
                        content: vec![UserContent::text("do it")],
                    }],
                )
                .await;
                let (tool_names, tool_defs, tool_registry) = no_tools();
                let ctx = tool_context();
                let (_cancel_tx, cancel_rx) = tokio::sync::oneshot::channel();

                let handle = tokio::task::spawn_local(async move {
                    let thread_id = ThreadId::from("thread-1");
                    let stream = run_completion(
                        &provider,
                        "mock",
                        false,
                        &hm,
                        &tool_names,
                        &tool_defs,
                        &tool_registry,
                        &ctx,
                        &thread_id,
                        "msg-1",
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
                id: id.to_owned(),
                call_id: call_id.map(String::from),
                content: vec![ToolResultContent::Text(
                    infinity_provider_protocol::message::Text {
                        text: format!("echo: {}", text),
                    },
                )],
            })
        }
    }

    static ECHO_TOOL: EchoSyncTool = EchoSyncTool;

    #[tokio::test(flavor = "current_thread")]
    async fn sync_tool_loops_back_without_new_stream() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (provider, mut ctrl) = mock_provider();
                let convo_store = InMemoryConversationStore::new();
                let hm = make_history(
                    &convo_store,
                    vec![Message::User {
                        content: vec![UserContent::text("echo something")],
                    }],
                )
                .await;

                let mut tool_names = HashSet::new();
                tool_names.insert("echo_sync".to_owned());
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
                    let thread_id = ThreadId::from("thread-1");
                    let stream = run_completion(
                        &provider,
                        "mock",
                        false,
                        &hm,
                        &tool_names,
                        &tool_defs,
                        &tool_registry,
                        &ctx,
                        &thread_id,
                        "msg-1",
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
                                if let Some(ToolResultContent::Text(t)) = res.content.first() {
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
                        if let Some(UserContent::ToolResult(res)) = content.first() {
                            if let Some(ToolResultContent::Text(t)) = res.content.first() {
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

    // ── Image tool result handling ──

    /// History ending in a tool result that carries both text and an image.
    fn image_tool_result_history() -> Vec<Message> {
        vec![
            Message::User {
                content: vec![UserContent::text("show me the logo")],
            },
            tool_call_msg(
                "tc-img",
                "read_file",
                serde_json::json!({"path": "logo.png"}),
            ),
            Message::User {
                content: vec![UserContent::ToolResult(ToolResult {
                    id: "tc-img".to_owned(),
                    call_id: None,
                    content: vec![
                        ToolResultContent::text("Read image file \"logo.png\""),
                        ToolResultContent::Image(infinity_provider_protocol::message::Image {
                            data: infinity_provider_protocol::message::ImageSource::Base64(
                                "aGVsbG8=".to_owned(),
                            ),
                            media_type: Some(
                                infinity_provider_protocol::message::ImageMediaType::PNG,
                            ),
                        }),
                    ],
                })],
            },
        ]
    }

    /// Collect the tool-result content items of the `tc-img` tool result in a
    /// request's chat history.
    fn image_result_content(req: &CompletionRequest) -> Vec<ToolResultContent> {
        req.chat_history
            .iter()
            .find_map(|m| {
                if let Message::User { content } = m
                    && let Some(UserContent::ToolResult(res)) = content.first()
                    && res.id == "tc-img"
                {
                    Some(res.content.to_vec())
                } else {
                    None
                }
            })
            .expect("tool result tc-img should be in chat history")
    }

    #[tokio::test(flavor = "current_thread")]
    async fn image_tool_result_sent_to_image_capable_model() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (provider, mut ctrl) = mock_provider_with_image_support(true);
                let convo_store = InMemoryConversationStore::new();
                let hm = make_history(&convo_store, image_tool_result_history()).await;
                let (tool_names, tool_defs, tool_registry) = no_tools();
                let ctx = tool_context();
                let (_cancel_tx, cancel_rx) = tokio::sync::oneshot::channel();

                let thread_id_owned = ThreadId::from("thread-1");
                let handle = tokio::task::spawn_local(async move {
                    let stream = run_completion(
                        &provider,
                        "mock",
                        true,
                        &hm,
                        &tool_names,
                        &tool_defs,
                        &tool_registry,
                        &ctx,
                        &thread_id_owned,
                        "msg-1",
                        None,
                        cancel_rx,
                    );
                    tokio::pin!(stream);
                    while stream.next().await.is_some() {}
                });

                let req = ctrl.next_request().await;
                let content = image_result_content(&req);
                assert_eq!(content.len(), 2);
                assert!(
                    matches!(&content[1], ToolResultContent::Image(img)
                        if img.data == infinity_provider_protocol::message::ImageSource::Base64("aGVsbG8=".to_owned())),
                    "image content should be passed through unchanged, got {content:?}"
                );

                ctrl.send_text("nice logo");
                ctrl.finish();
                handle.await.expect("await task handle");
            })
            .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn image_tool_result_stripped_for_non_image_model() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                // The default mock provider does not declare image support.
                let (provider, mut ctrl) = mock_provider();
                let convo_store = InMemoryConversationStore::new();
                let hm = make_history(&convo_store, image_tool_result_history()).await;
                let (tool_names, tool_defs, tool_registry) = no_tools();
                let ctx = tool_context();
                let (_cancel_tx, cancel_rx) = tokio::sync::oneshot::channel();

                let handle = tokio::task::spawn_local(async move {
                    let thread_id = ThreadId::from("thread-1");
                    let stream = run_completion(
                        &provider,
                        "mock",
                        false,
                        &hm,
                        &tool_names,
                        &tool_defs,
                        &tool_registry,
                        &ctx,
                        &thread_id,
                        "msg-1",
                        None,
                        cancel_rx,
                    );
                    tokio::pin!(stream);
                    while stream.next().await.is_some() {}
                });

                let req = ctrl.next_request().await;
                let content = image_result_content(&req);
                assert_eq!(content.len(), 2);
                match &content[0] {
                    ToolResultContent::Text(t) => {
                        assert_eq!(t.text, "Read image file \"logo.png\"")
                    }
                    other => panic!("expected text content, got {other:?}"),
                }
                match &content[1] {
                    ToolResultContent::Text(t) => assert_eq!(t.text, IMAGE_OMITTED_PLACEHOLDER),
                    other => panic!("image should be replaced with placeholder, got {other:?}"),
                }

                ctrl.send_text("cannot see images");
                ctrl.finish();
                handle.await.expect("await task handle");
            })
            .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn thinking_chunks_emitted() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (provider, mut ctrl) = mock_provider();
                let convo_store = InMemoryConversationStore::new();
                let hm = make_history(
                    &convo_store,
                    vec![Message::User {
                        content: vec![UserContent::text("think hard")],
                    }],
                )
                .await;
                let (tool_names, tool_defs, tool_registry) = no_tools();
                let ctx = tool_context();
                let (_cancel_tx, cancel_rx) = tokio::sync::oneshot::channel();

                let handle = tokio::task::spawn_local(async move {
                    let thread_id = ThreadId::from("thread-1");
                    let stream = run_completion(
                        &provider,
                        "mock",
                        false,
                        &hm,
                        &tool_names,
                        &tool_defs,
                        &tool_registry,
                        &ctx,
                        &thread_id,
                        "msg-1",
                        None,
                        cancel_rx,
                    );
                    tokio::pin!(stream);
                    let mut events = Vec::new();
                    while let Some(ev) = stream.next().await {
                        match ev.expect("receive stream event") {
                            CompletionEvent::ThinkingStart => events.push("start".to_owned()),
                            CompletionEvent::ThinkingEnd => events.push("end".to_owned()),
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
                ctrl.send_chunk(StreamChunk::ReasoningDelta {
                    id: None,
                    text: "hmm".into(),
                });
                ctrl.send_chunk(StreamChunk::ReasoningDelta {
                    id: None,
                    text: "...".into(),
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
                let (provider, mut ctrl) = mock_provider();
                let convo_store = InMemoryConversationStore::new();
                let hm = make_history(
                    &convo_store,
                    vec![Message::User {
                        content: vec![UserContent::text("run tool")],
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
                tool_names.insert("async_tool".to_owned());
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
                    let thread_id = ThreadId::from("thread-1");
                    let stream = run_completion(
                        &provider,
                        "mock",
                        false,
                        &hm,
                        &tool_names,
                        &tool_defs,
                        &tool_registry,
                        &ctx,
                        &thread_id,
                        "msg-1",
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
                assert_eq!(name, Some("async_tool".to_owned()));
            })
            .await;
    }

    /// Regression test: Bedrock streams *concurrent* tool calls in a single
    /// assistant message (each closed content block yields another
    /// `ToolCall`), with interleaved reasoning between them. Only the first
    /// call is executed — its result arrives in a later round — so everything
    /// streamed after it must be suppressed. Previously the ignored second
    /// call's name/argument deltas and the interleaved reasoning were
    /// forwarded as `ThinkingChunk`s, so clients saw "thinking" streaming
    /// *before* the executed call's `ToolResult`. Worse, the trailing
    /// reasoning block was committed to history after the tool call, which
    /// broke the `history.last()` match when the result arrived (the result
    /// was dropped and the call stranded as unanswered).
    #[tokio::test(flavor = "current_thread")]
    async fn concurrent_tool_calls_suppress_trailing_stream_content() {
        use infinity_provider_protocol::StreamChunk;
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (provider, mut ctrl) = mock_provider();
                let convo_store = InMemoryConversationStore::new();
                let hm = make_history(
                    &convo_store,
                    vec![Message::User {
                        content: vec![UserContent::text("run tools")],
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
                static ASYNC_TOOL2: AsyncTool = AsyncTool;

                let mut tool_names = HashSet::new();
                tool_names.insert("async_tool".to_owned());
                let tool_defs = vec![ToolDefinition {
                    name: "async_tool".into(),
                    description: "async".into(),
                    parameters: serde_json::json!({"type": "object"}),
                }];
                let mut tool_registry: HashMap<String, &dyn Tool<StubSender>> = HashMap::new();
                tool_registry.insert("async_tool".into(), &ASYNC_TOOL2);
                let ctx = tool_context();
                let (_cancel_tx, cancel_rx) = tokio::sync::oneshot::channel();

                let handle = tokio::task::spawn_local(async move {
                    let thread_id = ThreadId::from("thread-1");
                    let stream = run_completion(
                        &provider,
                        "mock",
                        false,
                        &hm,
                        &tool_names,
                        &tool_defs,
                        &tool_registry,
                        &ctx,
                        &thread_id,
                        "msg-1",
                        None,
                        cancel_rx,
                    );
                    tokio::pin!(stream);
                    let mut tags: Vec<&'static str> = Vec::new();
                    while let Some(ev) = stream.next().await {
                        tags.push(match ev.expect("receive stream event") {
                            CompletionEvent::Info(_) => "Info",
                            CompletionEvent::TextChunk(_) => "Text",
                            CompletionEvent::ThinkingStart => "ThinkingStart",
                            CompletionEvent::ThinkingEnd => "ThinkingEnd",
                            CompletionEvent::ThinkingChunk(_) => "ThinkingChunk",
                            CompletionEvent::SyncToolCall { .. } => "SyncToolCall",
                            CompletionEvent::SyncToolResult(_) => "SyncToolResult",
                            CompletionEvent::Action(CompletionAction::ExecuteToolCall {
                                ..
                            }) => "ExecuteToolCall",
                            CompletionEvent::Action(CompletionAction::Done(_)) => "Done",
                        });
                    }
                    // Return the final history so the test can check that the
                    // executed tool call stayed the last committed entry.
                    let last = hm.history.borrow().last().cloned();
                    (tags, last)
                });

                let _req = ctrl.next_request().await;
                // Bedrock-style single assistant message with two concurrent
                // tool calls and interleaved reasoning between them.
                ctrl.send_chunk(StreamChunk::ReasoningDelta {
                    id: None,
                    text: "planning the calls".into(),
                });
                ctrl.send_chunk(StreamChunk::Reasoning(
                    infinity_provider_protocol::message::Reasoning::new_with_signature(
                        "planning the calls",
                        Some("sig-1".to_owned()),
                    ),
                ));
                ctrl.send_chunk(StreamChunk::ToolCallDelta {
                    id: "tc-1".into(),
                    content: ToolCallDeltaContent::Name("async_tool".into()),
                });
                ctrl.send_tool_call("tc-1", "async_tool", serde_json::json!({"x": 1}));
                // Second concurrent call: its deltas must not leak out as
                // thinking, and it must not be executed.
                ctrl.send_chunk(StreamChunk::ToolCallDelta {
                    id: "tc-2".into(),
                    content: ToolCallDeltaContent::Name("async_tool".into()),
                });
                ctrl.send_chunk(StreamChunk::ToolCallDelta {
                    id: "tc-2".into(),
                    content: ToolCallDeltaContent::Delta("{\"x\":2}".into()),
                });
                // Interleaved reasoning after the first call.
                ctrl.send_chunk(StreamChunk::ReasoningDelta {
                    id: None,
                    text: "now the second call".into(),
                });
                ctrl.send_chunk(StreamChunk::Reasoning(
                    infinity_provider_protocol::message::Reasoning::new_with_signature(
                        "now the second call",
                        Some("sig-2".to_owned()),
                    ),
                ));
                ctrl.send_tool_call("tc-2", "async_tool", serde_json::json!({"x": 2}));
                ctrl.finish();

                let (tags, last_history) = handle.await.expect("await task handle");

                let action_idx = tags
                    .iter()
                    .position(|t| *t == "ExecuteToolCall")
                    .expect("first tool call should be executed");
                assert_eq!(
                    tags[action_idx + 1..]
                        .iter()
                        .filter(|t| **t != "Done")
                        .count(),
                    0,
                    "no display events may be emitted after the executed tool call \
                     (they would stream as thinking before the tool result); got {tags:?}"
                );

                // The executed call must remain the last committed history
                // entry so its result matches when it arrives.
                match last_history {
                    Some(InfinityMessage::ToolCall { call, .. }) => {
                        assert_eq!(call.id, "tc-1");
                    }
                    other => panic!("history must end with the executed tool call, got {other:?}"),
                }
            })
            .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn stream_drop_triggers_retry() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                // When the model stream ends unexpectedly (None from next()), the loop retries.
                let (provider, mut ctrl) = mock_provider();
                let convo_store = InMemoryConversationStore::new();
                let hm = make_history(
                    &convo_store,
                    vec![Message::User {
                        content: vec![UserContent::text("go")],
                    }],
                )
                .await;
                let (tool_names, tool_defs, tool_registry) = no_tools();
                let ctx = tool_context();
                let (_cancel_tx, cancel_rx) = tokio::sync::oneshot::channel();

                let handle = tokio::task::spawn_local(async move {
                    let thread_id = ThreadId::from("thread-1");
                    let stream = run_completion(
                        &provider,
                        "mock",
                        false,
                        &hm,
                        &tool_names,
                        &tool_defs,
                        &tool_registry,
                        &ctx,
                        &thread_id,
                        "msg-1",
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
                let (provider, mut ctrl) = mock_provider();
                let convo_store = InMemoryConversationStore::new();
                let hm = make_history(
                    &convo_store,
                    vec![Message::User {
                        content: vec![UserContent::text("think")],
                    }],
                )
                .await;
                let (tool_names, tool_defs, tool_registry) = no_tools();
                let ctx = tool_context();
                let (cancel_tx, cancel_rx) = tokio::sync::oneshot::channel();

                let handle = tokio::task::spawn_local(async move {
                    let thread_id = ThreadId::from("thread-1");
                    let stream = run_completion(
                        &provider,
                        "mock",
                        false,
                        &hm,
                        &tool_names,
                        &tool_defs,
                        &tool_registry,
                        &ctx,
                        &thread_id,
                        "msg-1",
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
                ctrl.send_chunk(StreamChunk::ReasoningDelta {
                    id: None,
                    text: "deep thought".into(),
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
    async fn cancellation_after_reasoning_does_not_commit_trailing_reasoning() {
        // A user interrupts the model after it streamed some visible text and a
        // complete reasoning block. On cancel we keep the text but must trim the
        // trailing reasoning: the next input is a user turn, and a user message
        // immediately following a reasoning block is rejected by some providers.
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (provider, mut ctrl) = mock_provider();
                let convo_store = InMemoryConversationStore::new();
                let hm = std::rc::Rc::new(
                    make_history(
                        &convo_store,
                        vec![Message::User {
                            content: vec![UserContent::text("do the thing")],
                        }],
                    )
                    .await,
                );
                let (tool_names, tool_defs, tool_registry) = no_tools();
                let ctx = tool_context();
                let (cancel_tx, cancel_rx) = tokio::sync::oneshot::channel();

                let hm_task = hm.clone();
                let thread_id_owned = ThreadId::from("thread-1");
                let handle = tokio::task::spawn_local(async move {
                    let stream = run_completion(
                        &provider,
                        "mock",
                        false,
                        &hm_task,
                        &tool_names,
                        &tool_defs,
                        &tool_registry,
                        &ctx,
                        &thread_id_owned,
                        "msg-1",
                        None,
                        cancel_rx,
                    );
                    tokio::pin!(stream);
                    while stream.next().await.is_some() {}
                });

                let _req = ctrl.next_request().await;
                ctrl.send_text("here is the answer");
                // A complete reasoning block (has a signature, so it is buffered).
                ctrl.send_chunk(StreamChunk::Reasoning(
                    infinity_provider_protocol::message::Reasoning::new_with_signature(
                        "still thinking",
                        Some("sig".to_owned()),
                    ),
                ));
                tokio::task::yield_now().await;
                tokio::task::yield_now().await;
                cancel_tx.send(()).expect("send cancel signal");

                handle.await.expect("await task handle");

                // Committed history: user + assistant text, with the trailing
                // reasoning trimmed off.
                let history = hm.history.borrow();
                assert!(
                    !matches!(
                        history.last(),
                        Some(InfinityMessage::Assistant {
                            content: AssistantContent::Reasoning(_),
                        })
                    ),
                    "history must not end on a reasoning block, got: {:?}",
                    history.last()
                );
                assert!(matches!(
                    history.last(),
                    Some(InfinityMessage::Assistant {
                        content: AssistantContent::Text(t),
                    }) if t.text == "here is the answer"
                ));
            })
            .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn multiple_sync_tool_calls_chain() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                // Model calls sync tool twice in sequence (two completion rounds), then responds.
                let (provider, mut ctrl) = mock_provider();
                let convo_store = InMemoryConversationStore::new();
                let hm = make_history(
                    &convo_store,
                    vec![Message::User {
                        content: vec![UserContent::text("echo twice")],
                    }],
                )
                .await;

                let mut tool_names = HashSet::new();
                tool_names.insert("echo_sync".to_owned());
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
                    let thread_id = ThreadId::from("thread-1");
                    let stream = run_completion(
                        &provider,
                        "mock",
                        false,
                        &hm,
                        &tool_names,
                        &tool_defs,
                        &tool_registry,
                        &ctx,
                        &thread_id,
                        "msg-1",
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

    #[tokio::test(flavor = "current_thread")]
    async fn broken_stream_discards_partial_turn_before_retry() {
        // End-to-end guard for the original bug: the model streams visible text,
        // then the stream breaks. The retry must rebuild the request from clean
        // committed history — ending on the user message, not the partial
        // assistant text (which Bedrock thinking models reject as "assistant
        // message prefill"). Under the buffering model this holds structurally:
        // the partial turn was never committed, so the discard is implicit.
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (provider, mut ctrl) = mock_provider();
                let convo_store = InMemoryConversationStore::new();
                let hm = make_history(
                    &convo_store,
                    vec![Message::User {
                        content: vec![UserContent::text("do the thing")],
                    }],
                )
                .await;
                let (tool_names, tool_defs, tool_registry) = no_tools();
                let ctx = tool_context();
                let (_cancel_tx, cancel_rx) = tokio::sync::oneshot::channel();

                let handle = tokio::task::spawn_local(async move {
                    let thread_id = ThreadId::from("thread-1");
                    let stream = run_completion(
                        &provider,
                        "mock",
                        false,
                        &hm,
                        &tool_names,
                        &tool_defs,
                        &tool_registry,
                        &ctx,
                        &thread_id,
                        "msg-1",
                        None,
                        cancel_rx,
                    );
                    tokio::pin!(stream);
                    let mut texts = Vec::new();
                    let mut got_done = false;
                    while let Some(ev) = stream.next().await {
                        match ev.expect("receive stream event") {
                            CompletionEvent::TextChunk(t) => texts.push(t),
                            CompletionEvent::Action(CompletionAction::Done(_)) => got_done = true,
                            _ => {}
                        }
                    }
                    (texts, got_done)
                });

                // Round 1: stream text, then break the stream without a Final.
                let _req = ctrl.next_request().await;
                ctrl.send_text("I'll take a look");
                tokio::task::yield_now().await;
                tokio::task::yield_now().await;
                ctrl.drop_stream();

                // Round 2: the retry request must end on the user message.
                let req2 = ctrl.next_request().await;
                let last_msg = req2
                    .chat_history
                    .into_iter()
                    .last()
                    .expect("bug: chat history is empty");
                assert!(
                    matches!(last_msg, Message::User { .. }),
                    "retry request must not end on an assistant message, got: {last_msg:?}"
                );

                ctrl.send_text("done");
                ctrl.finish();

                let (texts, got_done) = handle.await.expect("await task handle");
                assert!(got_done, "completion should finish after a clean retry");
                assert_eq!(texts, vec!["I'll take a look", "done"]);
            })
            .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn async_tool_call_flushes_tool_call_before_turn_ends() {
        // An async tool call ends the turn; its result arrives on a later,
        // separate turn. The assistant tool-call message (and any preceding
        // text) must be committed to history before the turn exits so the
        // caller's sync() persists it and the returning result matches.
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (provider, mut ctrl) = mock_provider();
                let convo_store = InMemoryConversationStore::new();
                let hm = std::rc::Rc::new(
                    make_history(
                        &convo_store,
                        vec![Message::User {
                            content: vec![UserContent::text("use the tool")],
                        }],
                    )
                    .await,
                );

                struct AsyncTool;
                #[async_trait]
                impl Tool<StubSender> for AsyncTool {
                    fn name(&self) -> &str {
                        "do_async"
                    }
                    fn description(&self) -> &str {
                        "async"
                    }
                    fn parameters(&self) -> serde_json::Value {
                        serde_json::json!({"type": "object"})
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
                tool_names.insert("do_async".to_owned());
                let tool_defs = vec![ToolDefinition {
                    name: "do_async".into(),
                    description: "async".into(),
                    parameters: serde_json::json!({"type": "object"}),
                }];
                let mut tool_registry: HashMap<String, &dyn Tool<StubSender>> = HashMap::new();
                tool_registry.insert("do_async".into(), &ASYNC_TOOL);
                let ctx = tool_context();
                let (_cancel_tx, cancel_rx) = tokio::sync::oneshot::channel();

                let hm_task = hm.clone();
                let thread_id_owned = ThreadId::from("thread-1");
                let handle = tokio::task::spawn_local(async move {
                    let stream = run_completion(
                        &provider,
                        "mock",
                        false,
                        &hm_task,
                        &tool_names,
                        &tool_defs,
                        &tool_registry,
                        &ctx,
                        &thread_id_owned,
                        "msg-1",
                        None,
                        cancel_rx,
                    );
                    tokio::pin!(stream);
                    let mut got_action = false;
                    while let Some(ev) = stream.next().await {
                        if let CompletionEvent::Action(CompletionAction::ExecuteToolCall {
                            ..
                        }) = ev.expect("receive stream event")
                        {
                            got_action = true;
                        }
                    }
                    got_action
                });

                let _req = ctrl.next_request().await;
                ctrl.send_text("let me check");
                ctrl.send_tool_call("tc-1", "do_async", serde_json::json!({}));
                ctrl.finish();

                let got_action = handle.await.expect("await task handle");
                assert!(got_action, "async tool call should yield ExecuteToolCall");

                // The text and the tool call are committed to history (and
                // pending_items), so a subsequent tool-result turn will match.
                let history = hm.history.borrow();
                assert!(matches!(
                    history.last(),
                    Some(InfinityMessage::ToolCall { .. })
                ));
                assert!(
                    history
                        .iter()
                        .any(|m| matches!(m, InfinityMessage::Assistant { .. })),
                    "preceding assistant text should be committed too"
                );
                assert!(
                    hm.turn_buffer.borrow().is_empty(),
                    "buffer must be flushed when the turn ends"
                );
            })
            .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn sync_tool_loop_back_includes_flushed_tool_call() {
        // When a sync tool loops back within the same run_completion, the
        // re-sent request must include the assistant tool call and the injected
        // tool result — i.e. the buffer was flushed before the loop-back.
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (provider, mut ctrl) = mock_provider();
                let convo_store = InMemoryConversationStore::new();
                let hm = make_history(
                    &convo_store,
                    vec![Message::User {
                        content: vec![UserContent::text("echo something")],
                    }],
                )
                .await;

                let mut tool_names = HashSet::new();
                tool_names.insert("echo_sync".to_owned());
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
                    let thread_id = ThreadId::from("thread-1");
                    let stream = run_completion(
                        &provider, "mock", false, &hm, &tool_names, &tool_defs, &tool_registry,
                        &ctx, &thread_id, "msg-1", None, cancel_rx,
                    );
                    tokio::pin!(stream);
                    let mut got_done = false;
                    while let Some(ev) = stream.next().await {
                        if let CompletionEvent::Action(CompletionAction::Done(_)) =
                            ev.expect("receive stream event")
                        {
                            got_done = true;
                        }
                    }
                    got_done
                });

                // Round 1: sync tool call.
                let _req = ctrl.next_request().await;
                ctrl.send_tool_call("tc-1", "echo_sync", serde_json::json!({"text": "hi"}));
                ctrl.finish();

                // Round 2 (loop-back): request must contain the assistant tool
                // call and the injected tool result.
                let req2 = ctrl.next_request().await;
                let msgs: Vec<Message> = req2.chat_history.into_iter().collect();
                let has_tool_call = msgs.iter().any(|m| matches!(
                    m,
                    Message::Assistant { content, .. }
                        if matches!(content.first(), Some(AssistantContent::ToolCall(_)))
                ));
                let has_tool_result = msgs.iter().any(|m| matches!(
                    m,
                    Message::User { content }
                        if matches!(content.first(), Some(UserContent::ToolResult(_)))
                ));
                assert!(has_tool_call, "loop-back must include the assistant tool call");
                assert!(has_tool_result, "loop-back must include the injected tool result");

                ctrl.send_text("all done");
                ctrl.finish();

                let got_done = handle.await.expect("await task handle");
                assert!(got_done);
            })
            .await;
    }

    // ── Tool dispatch failure fallbacks (ported from the removed
    //    batch_processor, regression coverage for #88) ──

    /// An `InputSender` that hands sent messages to the test.
    #[derive(Clone)]
    struct CapturingSender(tokio::sync::mpsc::UnboundedSender<InputMessage>);

    #[async_trait]
    impl InputSender for CapturingSender {
        type Error = std::io::Error;
        async fn send_to_input_queue(
            &self,
            message: InputMessage,
            _dedup_id: &str,
        ) -> Result<(), std::io::Error> {
            self.0.send(message).expect("testing");
            Ok(())
        }
    }

    #[derive(Clone)]
    struct StubHttp;

    #[async_trait]
    impl rap_client::http::HttpClient for StubHttp {
        type Error = std::io::Error;
        async fn post(&self, _: &str, _: &str) -> Result<u16, std::io::Error> {
            Ok(200)
        }
        async fn get(&self, _: &str) -> Result<(u16, Vec<u8>), std::io::Error> {
            Ok((200, Vec::new()))
        }
    }

    /// A tool whose dispatch always fails.
    struct FailingTool;

    #[async_trait]
    impl Tool<CapturingSender> for FailingTool {
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
            _: &ToolContext<CapturingSender>,
        ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
            Err("stub".into())
        }
    }

    fn capturing_ctx(
        group_id: &str,
        thread_stack: Vec<ThreadId>,
    ) -> (
        ToolContext<CapturingSender>,
        tokio::sync::mpsc::UnboundedReceiver<InputMessage>,
    ) {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        (
            ToolContext {
                message_sender: CapturingSender(tx),
                group_id: group_id.into(),
                callback_url: String::new(),
                user_id: None,
                thread_stack,
            },
            rx,
        )
    }

    /// Extract the single text tool-result the context's sender captured.
    fn captured_tool_result(
        rx: &mut tokio::sync::mpsc::UnboundedReceiver<InputMessage>,
    ) -> (String, Option<String>, String) {
        let message = rx.try_recv().expect("a tool result should be enqueued");
        let InputMessageContent::User(UserContent::ToolResult(result)) = message.content else {
            panic!("expected a tool result");
        };
        let Some(ToolResultContent::Text(text)) = result.content.first() else {
            panic!("expected text content");
        };
        (result.id.clone(), result.call_id.clone(), text.text.clone())
    }

    /// A tool's own argument validation failure comes back to the agent as an
    /// error tool result (not a propagated error).
    #[tokio::test]
    async fn close_thread_missing_id_enqueues_error_result() {
        let (context, mut rx) = capturing_ctx("child", vec!["root".into(), "child".into()]);
        let tool = crate::tools::thread::CloseThreadTool::<_, StubHttp> {
            conversation_store: InMemoryConversationStore::new(),
            rap_notifier: None,
        };

        tool.execute(
            serde_json::json!({}),
            "tc-close".into(),
            Some("call-close".into()),
            &context,
        )
        .await
        .expect("validation error should be returned as a tool result");

        let (id, call_id, text) = captured_tool_result(&mut rx);
        assert_eq!(id, "tc-close");
        assert_eq!(call_id.as_deref(), Some("call-close"));
        assert_eq!(text, "Error: thread_id is required");
    }

    /// A tool whose dispatch fails still gets a generic error result enqueued
    /// so the agent can recover, and the original error is surfaced.
    #[tokio::test]
    async fn failed_tool_execution_enqueues_fallback_result() {
        let (context, mut rx) = capturing_ctx("t1", vec!["t1".into()]);
        let tool = FailingTool;
        let tools: HashMap<String, &dyn Tool<CapturingSender>> =
            HashMap::from([("failing_tool".into(), &tool as &dyn Tool<CapturingSender>)]);
        let action = CompletionAction::ExecuteToolCall {
            tool_name: "failing_tool".into(),
            tool_args: serde_json::json!({}),
            tool_call_id: "tc-1".into(),
            call_id: Some("call-1".into()),
            display_as: None,
        };

        let error = execute_action_with_error_result(action, &tools, &context)
            .await
            .expect_err("tool execution should fail");
        assert_eq!(error.to_string(), "stub");

        let (id, call_id, text) = captured_tool_result(&mut rx);
        assert_eq!(id, "tc-1");
        assert_eq!(call_id.as_deref(), Some("call-1"));
        assert_eq!(text, "Error: Tool call failed");
    }

    /// A user-choice callback is surfaced out-of-band instead of entering
    /// history.
    #[tokio::test]
    async fn user_choice_input_surfaces_prompt() {
        let store = InMemoryConversationStore::new();
        let hm = make_history(&store, vec![]).await;

        let input = InputMessage {
            content: InputMessageContent::UserChoice(crate::message::UserChoiceRequired {
                content_type: "user_choice_required".to_owned(),
                id: "choice-1".to_owned(),
                call_id: None,
                prompt: "pick one".to_owned(),
                choices: vec!["a".to_owned(), "b".to_owned()],
                default: 0,
                response_url: "https://example.com/choice".to_owned(),
            }),
            group_id: "thread-1".into(),
            metadata: None,
            synthetic: None,
            display_as: None,
            subscription: false,
        };

        let result = prepare_input(input, "msg-1".to_owned(), &hm, &store, &StubSender)
            .await
            .expect("prepare input");

        let PrepareResult::UserChoiceRequired {
            id,
            prompt,
            choices,
            default,
            response_url,
        } = result
        else {
            panic!("expected UserChoiceRequired, got {result:?}");
        };
        assert_eq!(id, "choice-1");
        assert_eq!(prompt, "pick one");
        assert_eq!(choices, vec!["a".to_owned(), "b".to_owned()]);
        assert_eq!(default, 0);
        assert_eq!(response_url, "https://example.com/choice");
        assert!(hm.history.into_inner().is_empty());
    }

    // ═══════════════════════════════════════════════════════════════════
    // Context overflow & input-persistence safety
    //
    // Inputs must not be persisted to the conversation store until the
    // model has produced output for them (which proves the context did not
    // overflow). Provider errors carry an `ErrorClass`; core reacts to the
    // classification instead of parsing message strings, and never applies
    // its own timeouts to model requests.
    // ═══════════════════════════════════════════════════════════════════

    use infinity_provider_protocol::{CompletionError, ErrorClass, ModelProvider};

    /// Feed a user text input through the same path a step uses
    /// (`handle_content`), as opposed to `make_history` which fabricates
    /// already-committed history.
    fn add_user_input(
        hm: &HistoryManager<InMemoryConversationStore, InMemoryStateStore>,
        text: &str,
        message_id: &str,
    ) {
        let accepted = hm
            .handle_content(
                InfinityMessage::User {
                    content: UserContent::text(text),
                },
                message_id.to_owned(),
            )
            .expect("handle user input");
        assert!(accepted, "input should be accepted");
    }

    /// Feed a tool-result input through the same path a step uses.
    fn add_tool_result_input(
        hm: &HistoryManager<InMemoryConversationStore, InMemoryStateStore>,
        tool_call_id: &str,
        text: &str,
        message_id: &str,
    ) {
        let accepted = hm
            .handle_content(
                InfinityMessage::ToolResult {
                    result: ToolResult {
                        id: tool_call_id.to_owned(),
                        call_id: None,
                        content: vec![ToolResultContent::Text(
                            infinity_provider_protocol::message::Text {
                                text: text.to_owned(),
                            },
                        )],
                    },
                    display_segments: None,
                },
                message_id.to_owned(),
            )
            .expect("handle tool result input");
        assert!(accepted, "tool result should be accepted");
    }

    /// Run one completion to termination, summarizing the yielded events as
    /// strings (`text:`, `info:`, `error:`, `done`).
    async fn collect_completion_events<P: ModelProvider>(
        provider: &P,
        hm: &HistoryManager<InMemoryConversationStore, InMemoryStateStore>,
        cancel_rx: tokio::sync::oneshot::Receiver<()>,
    ) -> Vec<String> {
        let (tool_names, tool_defs, tool_registry) = no_tools();
        let ctx = tool_context();
        let thread_id = ThreadId::from("thread-1");
        let stream = run_completion(
            provider,
            "mock",
            false,
            hm,
            &tool_names,
            &tool_defs,
            &tool_registry,
            &ctx,
            &thread_id,
            "msg-1",
            None,
            cancel_rx,
        );
        tokio::pin!(stream);
        let mut events = Vec::new();
        while let Some(ev) = stream.next().await {
            match ev {
                Ok(CompletionEvent::TextChunk(t)) => events.push(format!("text:{t}")),
                Ok(CompletionEvent::Info(t)) => events.push(format!("info:{t}")),
                Ok(CompletionEvent::Action(CompletionAction::Done(_))) => {
                    events.push("done".to_owned());
                }
                Ok(_) => {}
                Err(e) => events.push(format!("error:{e}")),
            }
        }
        events
    }

    /// The messages persisted for `thread-1`, debug-formatted for asserts.
    fn persisted(store: &InMemoryConversationStore) -> String {
        format!(
            "{:?}",
            store
                .thread_messages(&ThreadId::from("thread-1"))
                .unwrap_or_default()
        )
    }

    fn overflow_error() -> CompletionError {
        CompletionError::provider(
            ErrorClass::ContextOverflow,
            "input is too long for the model",
        )
    }

    /// An oversized *user input* cannot be shrunk: on a context-overflow
    /// error it must be dropped from the in-memory history and never
    /// persisted, so the thread does not permanently hang on a poison
    /// message.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn oversized_user_input_is_dropped_and_not_persisted() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (provider, mut ctrl) = mock_provider();
                let convo_store = InMemoryConversationStore::new();
                let hm = make_history(&convo_store, vec![]).await;
                add_user_input(&hm, "HUGE INPUT", "msg-huge");
                let (_cancel_tx, cancel_rx) = tokio::sync::oneshot::channel();

                let handle = tokio::task::spawn_local(async move {
                    let events = collect_completion_events(&provider, &hm, cancel_rx).await;
                    (hm, events)
                });

                let _req = ctrl.next_request().await;
                ctrl.send_error(overflow_error());
                ctrl.drop_stream();

                let (hm, events) = handle.await.expect("join completion task");
                assert!(
                    events.iter().any(|e| e.starts_with("error:")),
                    "the overflow must surface as a terminal error, got {events:?}"
                );
                // The poison input is gone from the in-memory history...
                assert!(
                    !format!("{:?}", hm.history.borrow()).contains("HUGE INPUT"),
                    "oversized input must be dropped from in-memory history"
                );
                // ...and the commit that ends the step persists nothing.
                hm.sync().await.expect("sync");
                assert!(
                    !persisted(&convo_store).contains("HUGE INPUT"),
                    "oversized input must not be persisted"
                );
            })
            .await;
    }

    /// An oversized *tool result* can be shrunk: on a context-overflow
    /// error it is replaced with a fixed placeholder (the tool call must
    /// still be answered) and the completion is retried.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn oversized_tool_result_is_replaced_with_placeholder_and_retried() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (provider, mut ctrl) = mock_provider();
                let convo_store = InMemoryConversationStore::new();
                let hm = make_history(
                    &convo_store,
                    vec![
                        Message::User {
                            content: vec![UserContent::text("run it")],
                        },
                        tool_call_msg("tc-1", "some_tool", serde_json::json!({})),
                    ],
                )
                .await;
                add_tool_result_input(&hm, "tc-1", "HUGE RESULT", "msg-tr");
                let (_cancel_tx, cancel_rx) = tokio::sync::oneshot::channel();

                let handle = tokio::task::spawn_local(async move {
                    let events = collect_completion_events(&provider, &hm, cancel_rx).await;
                    (hm, events)
                });

                let _req1 = ctrl.next_request().await;
                ctrl.send_error(overflow_error());
                ctrl.drop_stream();

                // The retry must present the placeholder instead of the
                // oversized result.
                let req2 = tokio::time::timeout(Duration::from_secs(300), ctrl.next_request())
                    .await
                    .expect("core should retry after replacing the oversized tool result");
                let history_debug = format!("{:?}", req2.chat_history);
                assert!(
                    !history_debug.contains("HUGE RESULT"),
                    "retry must not include the oversized tool result"
                );
                assert!(
                    history_debug.contains(TOOL_RESULT_TOO_LARGE_PLACEHOLDER),
                    "retry must include the placeholder tool result"
                );
                ctrl.send_text("recovered");
                ctrl.finish();

                let (hm, events) = handle.await.expect("join completion task");
                assert!(events.contains(&"done".to_owned()), "events: {events:?}");
                hm.sync().await.expect("sync");
                let stored = persisted(&convo_store);
                assert!(
                    stored.contains(TOOL_RESULT_TOO_LARGE_PLACEHOLDER),
                    "the placeholder result must be persisted"
                );
                assert!(
                    !stored.contains("HUGE RESULT"),
                    "the oversized result must not be persisted"
                );
            })
            .await;
    }

    /// An oversized *subscription event* is also replaced with the
    /// placeholder (its body is a tool result) rather than dropped: the
    /// agent should still learn that an event arrived.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn oversized_subscription_event_is_replaced_with_placeholder_and_retried() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (provider, mut ctrl) = mock_provider();
                let convo_store = InMemoryConversationStore::new();
                let hm = make_history(
                    &convo_store,
                    vec![Message::User {
                        content: vec![UserContent::text("subscribe to things")],
                    }],
                )
                .await;
                let accepted = hm
                    .handle_content(
                        InfinityMessage::SubscriptionEvent {
                            result: Box::new(ToolResult {
                                id: "evt-1".to_owned(),
                                call_id: None,
                                content: vec![ToolResultContent::Text(
                                    infinity_provider_protocol::message::Text {
                                        text: "HUGE EVENT BODY".to_owned(),
                                    },
                                )],
                            }),
                            tool_call_id: "sub-1".to_owned(),
                            child_thread_id: None,
                            invocation: Some(Box::new(ToolCall::new(
                                "evt-1",
                                "receive_event__injected",
                                serde_json::json!({}),
                            ))),
                        },
                        "msg-evt".to_owned(),
                    )
                    .expect("handle subscription event");
                assert!(accepted);
                let (_cancel_tx, cancel_rx) = tokio::sync::oneshot::channel();

                let handle = tokio::task::spawn_local(async move {
                    let events = collect_completion_events(&provider, &hm, cancel_rx).await;
                    (hm, events)
                });

                let _req1 = ctrl.next_request().await;
                ctrl.send_error(overflow_error());
                ctrl.drop_stream();

                let req2 = tokio::time::timeout(Duration::from_secs(300), ctrl.next_request())
                    .await
                    .expect("core should retry after replacing the oversized event body");
                let history_debug = format!("{:?}", req2.chat_history);
                assert!(
                    !history_debug.contains("HUGE EVENT BODY"),
                    "retry must not include the oversized event body"
                );
                assert!(
                    history_debug.contains(TOOL_RESULT_TOO_LARGE_PLACEHOLDER),
                    "retry must include the placeholder event body"
                );
                ctrl.send_text("noted");
                ctrl.finish();

                let (hm, events) = handle.await.expect("join completion task");
                assert!(events.contains(&"done".to_owned()), "events: {events:?}");
                hm.sync().await.expect("sync");
                let stored = persisted(&convo_store);
                assert!(
                    stored.contains(TOOL_RESULT_TOO_LARGE_PLACEHOLDER)
                        && !stored.contains("HUGE EVENT BODY"),
                    "the placeholder event must be persisted, not the oversized body; store: {stored}"
                );
            })
            .await;
    }

    /// If the unvalidated inputs include *user text* alongside a tool
    /// result, an overflow must not trigger a retry: the culprit is
    /// ambiguous and the user's words have to be dropped, so the tool
    /// result is settled with the same "interrupted by user" text a user
    /// interruption would inject (a "too large" note could mislead the
    /// model into re-running the tool) and the round stops so the user can
    /// re-send.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn overflow_with_pending_tool_result_and_user_input_does_not_retry() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (provider, mut ctrl) = mock_provider();
                let convo_store = InMemoryConversationStore::new();
                let hm = make_history(
                    &convo_store,
                    vec![
                        Message::User {
                            content: vec![UserContent::text("run it")],
                        },
                        tool_call_msg("tc-1", "some_tool", serde_json::json!({})),
                    ],
                )
                .await;
                // The tool result arrives first, then a huge user input
                // interrupts before the model produced any output.
                add_tool_result_input(&hm, "tc-1", "normal tool result", "msg-tr");
                add_user_input(&hm, "HUGE USER INPUT", "msg-huge");
                let (_cancel_tx, cancel_rx) = tokio::sync::oneshot::channel();

                let handle = tokio::task::spawn_local(async move {
                    let events = collect_completion_events(&provider, &hm, cancel_rx).await;
                    (hm, events)
                });

                let _req1 = ctrl.next_request().await;
                ctrl.send_error(overflow_error());
                ctrl.drop_stream();

                // Time-boxed so a wrong retry attempt fails fast instead of
                // hanging the test (the consumer would wait forever on the
                // retry round's mock stream).
                let (hm, events) = tokio::time::timeout(Duration::from_secs(600), handle)
                    .await
                    .expect("the round must stop, not retry")
                    .expect("join completion task");
                assert!(
                    events.iter().any(|e| e.starts_with("error:")),
                    "the overflow must surface as a terminal error, got {events:?}"
                );
                assert!(
                    ctrl.try_next_request().is_none(),
                    "dropping user input must stop the round, not retry it"
                );

                let history = format!("{:?}", hm.history.borrow());
                // The user text was dropped...
                assert!(
                    !history.contains("HUGE USER INPUT"),
                    "oversized user input must be dropped; history: {history}"
                );
                // ...and the tool call stays answered — settled as
                // interrupted, not "too large" (the culprit is ambiguous).
                assert!(
                    history.contains(TOOL_CALL_INTERRUPTED_TEXT),
                    "the tool result must be settled as interrupted; history: {history}"
                );
                assert!(
                    !history.contains(TOOL_RESULT_TOO_LARGE_PLACEHOLDER),
                    "ambiguous overflows must not blame the tool result; history: {history}"
                );
                assert!(!history.contains("normal tool result"));

                // Nothing from this round persists (the settled result is
                // still unvalidated).
                hm.sync().await.expect("sync");
                let stored = persisted(&convo_store);
                assert!(
                    !stored.contains("HUGE USER INPUT")
                        && !stored.contains("normal tool result")
                        && !stored.contains(TOOL_CALL_INTERRUPTED_TEXT),
                    "nothing unvalidated may persist; store: {stored}"
                );
            })
            .await;
    }

    /// If the *placeholder* tool result still overflows, give up: surface a
    /// terminal error and persist nothing. Only user input is ever dropped,
    /// so the tool call stays answered in memory — re-settled as
    /// "interrupted by user", since the round is over and the next thing
    /// the model sees will be a fresh user message (a "too large" note
    /// could mislead it into re-running the tool). After a process restart
    /// the store ends on the unanswered call and the next input settles it
    /// the same way (see
    /// `rebooted_session_settles_persisted_unanswered_tool_call`).
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn second_overflow_after_placeholder_gives_up_without_persisting_result() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (provider, mut ctrl) = mock_provider();
                let convo_store = InMemoryConversationStore::new();
                let hm = make_history(
                    &convo_store,
                    vec![
                        Message::User {
                            content: vec![UserContent::text("run it")],
                        },
                        tool_call_msg("tc-1", "some_tool", serde_json::json!({})),
                    ],
                )
                .await;
                add_tool_result_input(&hm, "tc-1", "HUGE RESULT", "msg-tr");
                let (_cancel_tx, cancel_rx) = tokio::sync::oneshot::channel();

                let handle = tokio::task::spawn_local(async move {
                    let events = collect_completion_events(&provider, &hm, cancel_rx).await;
                    (hm, events)
                });

                let _req1 = ctrl.next_request().await;
                ctrl.send_error(overflow_error());
                ctrl.drop_stream();
                let _req2 = tokio::time::timeout(Duration::from_secs(300), ctrl.next_request())
                    .await
                    .expect("core should retry once with the placeholder result");
                ctrl.send_error(overflow_error());
                ctrl.drop_stream();

                let (hm, events) = handle.await.expect("join completion task");
                assert!(
                    events.iter().any(|e| e.starts_with("error:")),
                    "the second overflow must surface as a terminal error, got {events:?}"
                );
                assert!(
                    ctrl.try_next_request().is_none(),
                    "the placeholder round must not be retried again"
                );
                // The tool call stays answered in memory — re-settled as
                // "interrupted" now that the round is abandoned — but
                // nothing about the result persists.
                let last = format!("{:?}", hm.history.borrow().last());
                assert!(
                    last.contains("tc-1") && last.contains(TOOL_CALL_INTERRUPTED_TEXT),
                    "the call must stay answered as interrupted, got {last}"
                );
                assert!(
                    !last.contains(TOOL_RESULT_TOO_LARGE_PLACEHOLDER),
                    "a 'too large' note would mislead the next round, got {last}"
                );
                hm.sync().await.expect("sync");
                let stored = persisted(&convo_store);
                assert!(!stored.contains("HUGE RESULT"), "stored: {stored}");
                assert!(
                    !stored.contains(TOOL_RESULT_TOO_LARGE_PLACEHOLDER)
                        && !stored.contains(TOOL_CALL_INTERRUPTED_TEXT),
                    "stored: {stored}"
                );

                // A later user input does not need to interrupt anything —
                // the call is already answered in memory.
                add_user_input(&hm, "hello again", "msg-next");
                let history = format!("{:?}", hm.history.borrow());
                assert!(history.contains(TOOL_CALL_INTERRUPTED_TEXT));
                assert!(history.contains("hello again"));
            })
            .await;
    }

    /// Interrupting a completion *before the model produced any output*
    /// must not persist the batch's inputs — the model never validated them
    /// (they could be the poison input) — but they stay in memory so the
    /// next round still sends them.
    #[tokio::test(flavor = "current_thread")]
    async fn interrupt_before_model_output_keeps_input_in_memory_unpersisted() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (provider, mut ctrl) = mock_provider();
                let convo_store = InMemoryConversationStore::new();
                let hm = make_history(&convo_store, vec![]).await;
                add_user_input(&hm, "hello there", "msg-1");
                let (cancel_tx, cancel_rx) = tokio::sync::oneshot::channel();

                let handle = tokio::task::spawn_local(async move {
                    let events = collect_completion_events(&provider, &hm, cancel_rx).await;
                    (hm, events)
                });

                // Request is in flight; no output yet. Interrupt.
                let _req = ctrl.next_request().await;
                cancel_tx.send(()).expect("send cancel");

                let (hm, _events) = handle.await.expect("join completion task");
                // The input survives in memory (the next step resends it)...
                assert!(
                    format!("{:?}", hm.history.borrow()).contains("hello there"),
                    "interrupted input must stay in the in-memory history"
                );
                // ...but must not be persisted: the model never accepted it.
                hm.sync().await.expect("sync");
                assert!(
                    !persisted(&convo_store).contains("hello there"),
                    "input must not be persisted before the model produced output"
                );
            })
            .await;
    }

    /// Interrupting *after* the model produced output persists both the
    /// input and the partial output: streamed output proves the context did
    /// not overflow.
    #[tokio::test(flavor = "current_thread")]
    async fn interrupt_after_model_output_persists_input_and_partial_text() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (provider, mut ctrl) = mock_provider();
                let convo_store = InMemoryConversationStore::new();
                let hm = make_history(&convo_store, vec![]).await;
                add_user_input(&hm, "hello there", "msg-1");
                let (cancel_tx, cancel_rx) = tokio::sync::oneshot::channel();

                let handle = tokio::task::spawn_local(async move {
                    let events = collect_completion_events(&provider, &hm, cancel_rx).await;
                    (hm, events)
                });

                let _req = ctrl.next_request().await;
                ctrl.send_text("partial answer");
                tokio::task::yield_now().await;
                tokio::task::yield_now().await;
                cancel_tx.send(()).expect("send cancel");

                let (hm, events) = handle.await.expect("join completion task");
                assert!(events.contains(&"text:partial answer".to_owned()));
                hm.sync().await.expect("sync");
                let stored = persisted(&convo_store);
                assert!(
                    stored.contains("hello there"),
                    "model output validated the input: it must persist"
                );
                assert!(
                    stored.contains("partial answer"),
                    "the partial output must persist"
                );
            })
            .await;
    }

    /// A throttled error retries after a longer backoff — driven by the
    /// provider's classification, not by message-string matching.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn throttled_error_waits_and_retries() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (provider, mut ctrl) = mock_provider();
                let convo_store = InMemoryConversationStore::new();
                let hm = make_history(&convo_store, vec![]).await;
                add_user_input(&hm, "hi", "msg-1");
                let (_cancel_tx, cancel_rx) = tokio::sync::oneshot::channel();

                let handle = tokio::task::spawn_local(async move {
                    collect_completion_events(&provider, &hm, cancel_rx).await
                });

                let started = tokio::time::Instant::now();
                let _req1 = ctrl.next_request().await;
                ctrl.send_error(CompletionError::provider(
                    ErrorClass::Throttled,
                    "simulated throttle",
                ));
                ctrl.drop_stream();

                let _req2 = tokio::time::timeout(Duration::from_secs(600), ctrl.next_request())
                    .await
                    .expect("core should retry after a throttled error");
                assert!(
                    started.elapsed() >= Duration::from_secs(10),
                    "throttled retries should back off"
                );
                ctrl.send_text("ok");
                ctrl.finish();

                let events = handle.await.expect("join completion task");
                assert!(events.contains(&"done".to_owned()), "events: {events:?}");
            })
            .await;
    }

    /// A transient error retries quickly — again from the classification,
    /// regardless of the message text.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn transient_error_discards_turn_and_retries() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (provider, mut ctrl) = mock_provider();
                let convo_store = InMemoryConversationStore::new();
                let hm = make_history(&convo_store, vec![]).await;
                add_user_input(&hm, "hi", "msg-1");
                let (_cancel_tx, cancel_rx) = tokio::sync::oneshot::channel();

                let handle = tokio::task::spawn_local(async move {
                    collect_completion_events(&provider, &hm, cancel_rx).await
                });

                let _req1 = ctrl.next_request().await;
                ctrl.send_error(CompletionError::provider(
                    ErrorClass::Transient,
                    "connection reset by peer",
                ));
                ctrl.drop_stream();

                let _req2 = tokio::time::timeout(Duration::from_secs(600), ctrl.next_request())
                    .await
                    .expect("core should retry after a transient error");
                ctrl.send_text("ok");
                ctrl.finish();

                let events = handle.await.expect("join completion task");
                assert!(events.contains(&"done".to_owned()), "events: {events:?}");
            })
            .await;
    }

    /// A fatal error terminates immediately: no retry request is made.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn fatal_error_gives_up_immediately() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (provider, mut ctrl) = mock_provider();
                let convo_store = InMemoryConversationStore::new();
                let hm = make_history(&convo_store, vec![]).await;
                add_user_input(&hm, "hi", "msg-1");
                let (_cancel_tx, cancel_rx) = tokio::sync::oneshot::channel();

                let handle = tokio::task::spawn_local(async move {
                    collect_completion_events(&provider, &hm, cancel_rx).await
                });

                let _req1 = ctrl.next_request().await;
                ctrl.send_error(CompletionError::provider(
                    ErrorClass::Fatal,
                    "the model does not exist",
                ));
                ctrl.drop_stream();

                let events = handle.await.expect("join completion task");
                assert!(
                    events.iter().any(|e| e.starts_with("error:")),
                    "fatal errors must surface, got {events:?}"
                );
                assert!(
                    ctrl.try_next_request().is_none(),
                    "fatal errors must not be retried"
                );
            })
            .await;
    }

    // ── HistoryManager three-phase pending safeguards ──

    /// Inputs folded into history are not persisted by `sync()` until the
    /// model validates them.
    #[tokio::test]
    async fn sync_does_not_persist_inputs_before_model_output() {
        let store = InMemoryConversationStore::new();
        let hm = make_history(&store, vec![]).await;
        add_user_input(&hm, "not yet validated", "msg-1");
        hm.sync().await.expect("sync");
        assert!(
            !persisted(&store).contains("not yet validated"),
            "unvalidated inputs must not be persisted by sync()"
        );
    }

    /// Sequentiality safeguard: committing model output while unvalidated
    /// inputs exist would let `sync()` persist the output *without* the
    /// input it answers. That is a bug and must panic.
    #[tokio::test]
    #[should_panic(expected = "unvalidated")]
    async fn flushing_model_output_with_unvalidated_inputs_panics() {
        let store = InMemoryConversationStore::new();
        let hm = make_history(&store, vec![]).await;
        add_user_input(&hm, "pending input", "msg-1");
        hm.handle_completion(
            &StreamChunk::Text("model output".to_owned()),
            "completion-1".to_owned(),
            None,
        );
        // Flushing without validating the pending input first violates
        // sequentiality.
        hm.flush_turn();
    }

    /// Child threads inherit history *from the store*, so a spawn point
    /// must not extend past inputs that have not been persisted yet.
    #[tokio::test]
    async fn safe_spawn_point_excludes_unvalidated_inputs() {
        let store = InMemoryConversationStore::new();
        let hm = make_history(&store, vec![]).await;
        add_user_input(&hm, "not persisted yet", "msg-1");
        assert_eq!(
            hm.safe_spawn_point(),
            0,
            "spawn point must not cover unvalidated (unpersisted) inputs"
        );
    }
}
