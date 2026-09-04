//! [`Thread`]: a handle to one conversation thread, with a step-oriented
//! execution API.

use rap_protocol::ThreadId;
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use futures_util::StreamExt;
use infinity_provider_protocol::message::{ToolResultContent, UserContent};
use infinity_provider_protocol::{ToolDefinition, Usage};
use tokio::sync::oneshot;

use crate::event_processor::{self, CompletionAction, CompletionEvent, HistoryManager};
use crate::message::{InputMessage, InputMessageContent, SyntheticKind, TaggedSyntheticKind};
use crate::tools::{Tool, ToolContext};
use crate::traits::{ConversationStore, InputSender, StateStore};
use rap_client::http::HttpClient;

use super::builder::SystemInner;
use super::defer::DeferQueue;
use super::events::{AgentEvent, ReplaySnapshot, UserChoice};
use super::observer::ThreadObserver;

type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// The name of the built-in thread-closing tool. An unanswered call to it
/// never receives a result (the thread is closed), so it does not count as
/// "waiting for a tool result".
const CLOSE_THREAD_TOOL: &str = "close_thread";

/// The result of one step (see [`AgentSystem::step`](super::AgentSystem::step)).
#[derive(Debug)]
pub enum StepOutcome {
    /// No input was actionable — no completion ran. (Deferred, duplicate, or
    /// out-of-band inputs such as OAuth challenges land here.)
    Skipped,
    /// A completion round ran (it may have dispatched a tool call, which will
    /// deliver its result as a later input).
    Completed {
        /// Token usage reported by the provider, if any.
        usage: Option<Usage>,
        /// The context window of the model that ran the round (`0` when
        /// unknown). Useful for compaction thresholds.
        context_window: usize,
    },
}

/// Whether this input is a plain user text message (as opposed to a tool
/// result or synthetic event). User text deliberately interrupts pending
/// work.
pub fn is_user_text_input(msg: &InputMessage) -> bool {
    msg.synthetic.is_none()
        && matches!(
            &msg.content,
            InputMessageContent::User(UserContent::Text(_))
        )
}

/// Whether this input is a synthetic event that can be deferred while the
/// thread waits on a pending tool call (subscription events, thread reports,
/// parent messages).
pub fn is_deferrable_synthetic_event(msg: &InputMessage) -> bool {
    msg.synthetic.as_ref().is_some_and(|s| {
        matches!(
            s,
            SyntheticKind::Tagged(
                TaggedSyntheticKind::SubscriptionEvent { .. }
                    | TaggedSyntheticKind::ThreadReport { .. }
                    | TaggedSyntheticKind::ParentMessage { .. }
            ) | SyntheticKind::SubscriptionEvent(_)
        )
    })
}

/// A thread's resolved configuration: [`ThreadConfig`] tools plus the
/// built-in thread/subscription tools.
struct ResolvedConfig<M: InputSender, H: HttpClient> {
    tools: Vec<Rc<dyn Tool<M>>>,
    extra_system_prompt: Option<String>,
    rap_notifier: Option<rap_client::notifier::RapNotifier<H>>,
}

/// A handle to one conversation thread of an [`AgentSystem`](super::AgentSystem).
///
/// A `Thread` owns the thread's in-memory [`HistoryManager`] and executes
/// **steps**: one step prepares a batch of input messages, runs at most one
/// completion round, commits the results durably, and dispatches at most one
/// asynchronous tool call. Between steps the thread holds no in-flight work,
/// which is what lets serverless embeddings drop it entirely (a later slice
/// reloads it from the stores) and resident embeddings park it on a channel.
///
/// The thread's configuration (tools, prompt, notifier) is resolved lazily
/// on the first step rather than at load, so loading a thread for replay or
/// subscription does not touch (or boot) its tool servers.
pub struct Thread<C: ConversationStore, S: StateStore, M: InputSender, H: HttpClient> {
    inner: Rc<SystemInner<C, S, M, H>>,
    history: HistoryManager<C, S>,
    /// Lazily resolved per-thread configuration (see
    /// [`resolved_config`](Self::resolved_config)).
    config: RefCell<Option<Rc<ResolvedConfig<M, H>>>>,
    /// In-progress (uncommitted) reasoning text, for replay snapshots.
    current_thinking: RefCell<Option<String>>,
    /// Whether a completion round is currently streaming.
    in_flight: Cell<bool>,
    /// Pending choices loaded from and kept in sync with this thread's state entry.
    pending_choices: RefCell<Vec<UserChoice>>,
}

impl<C, S, M, H> Thread<C, S, M, H>
where
    C: ConversationStore + 'static,
    S: StateStore + 'static,
    M: InputSender + 'static,
    H: HttpClient + 'static,
{
    pub(crate) async fn load(
        inner: Rc<SystemInner<C, S, M, H>>,
        thread_id: ThreadId,
    ) -> Result<Self, BoxError> {
        let history = HistoryManager::new_with_history(
            inner.conversation_store.clone(),
            inner.state_store.clone(),
            thread_id,
        )
        .await?;
        let pending_choices = inner
            .state_store
            .get_pending_user_choices(&history.thread_id)
            .await?;
        Ok(Self {
            inner,
            history,
            config: RefCell::new(None),
            current_thinking: RefCell::new(None),
            in_flight: Cell::new(false),
            pending_choices: RefCell::new(pending_choices),
        })
    }

    /// Resolve (once) this thread's configuration and append the built-in
    /// tools to it.
    async fn resolved_config(&self) -> Result<Rc<ResolvedConfig<M, H>>, BoxError> {
        if let Some(config) = self.config.borrow().clone() {
            return Ok(config);
        }
        let config = self.inner.config.resolve(&self.history.thread_id).await?;
        let mut tools = config.tools;
        if self.inner.builtin_tools {
            tools.push(Rc::new(crate::tools::thread::SpawnThreadTool {
                conversation_store: self.inner.conversation_store.clone(),
            }));
            tools.push(Rc::new(crate::tools::thread::ReportToParentTool {
                conversation_store: self.inner.conversation_store.clone(),
            }));
            tools.push(Rc::new(crate::tools::thread::CloseThreadTool {
                conversation_store: self.inner.conversation_store.clone(),
                rap_notifier: config.rap_notifier.clone(),
            }));
            tools.push(Rc::new(crate::tools::thread::SendMessageToChildTool {
                conversation_store: self.inner.conversation_store.clone(),
            }));
            tools.push(Rc::new(
                crate::tools::cancel_subscription::CancelSubscriptionTool {
                    state_store: self.inner.state_store.clone(),
                    rap_notifier: config.rap_notifier.clone(),
                },
            ));
            tools.push(Rc::new(crate::tools::sleep::SleepUntilEventOrInputTool));
        }
        if self.inner.tokio_sleep_tools {
            tools.push(Rc::new(crate::tools::sleep::TokioSleepTool));
            tools.push(Rc::new(crate::tools::sleep::TokioSleepUntilTool));
        }
        let resolved = Rc::new(ResolvedConfig {
            tools,
            extra_system_prompt: config.extra_system_prompt,
            rap_notifier: config.rap_notifier,
        });
        *self.config.borrow_mut() = Some(resolved.clone());
        Ok(resolved)
    }

    /// A live view of the thread for bringing a new subscriber up to date:
    /// committed history plus the in-flight turn, the in-progress reasoning
    /// text, and whether a completion is currently streaming.
    pub fn replay_snapshot(&self) -> ReplaySnapshot {
        ReplaySnapshot {
            history: self.history.current_turn_view(),
            current_thinking: self.current_thinking.borrow().clone(),
            in_progress: self.in_flight.get(),
            pending_choices: self.pending_choices.borrow().clone(),
        }
    }

    /// If the last history entry is an unanswered tool call, its
    /// `(tool_call_id, tool_name)`.
    pub fn pending_tool_call(&self) -> Option<(String, String)> {
        self.history.history.borrow().last().and_then(|msg| {
            if let crate::message::InfinityMessage::ToolCall { call, .. } = msg {
                Some((call.id.clone(), call.function.name.clone()))
            } else {
                None
            }
        })
    }

    /// The thread is waiting for the result of a dispatched tool call whose
    /// tool represents in-flight work (i.e. it is not a passive tool like the
    /// sleep family). While this is `Some`, deferrable synthetic events must
    /// not be processed — doing so would inject an "interrupted" result for
    /// a call that is actually still running.
    ///
    /// If the thread's configuration has not been resolved yet (no step has
    /// run), the call is conservatively treated as active.
    pub fn pending_active_tool_call(&self) -> Option<String> {
        let (id, name) = self.pending_tool_call()?;
        let passive = self.config.borrow().as_ref().is_some_and(|config| {
            config
                .tools
                .iter()
                .find(|t| t.name() == name)
                .is_some_and(|t| t.is_passive())
        });
        if passive { None } else { Some(id) }
    }

    /// Whether the thread must stay resident: the result of an unanswered
    /// tool call (other than `close_thread`, whose result never arrives) is
    /// still on its way. Active subscriptions do not keep a thread resident;
    /// their events wake it through the router when they arrive.
    ///
    /// Synchronous so a driver can make its exit decision atomically with
    /// respect to the router on the shared `LocalSet`.
    pub fn awaiting_tool_result(&self) -> bool {
        self.pending_tool_call()
            .is_some_and(|(_, name)| name != CLOSE_THREAD_TOOL)
    }

    /// Apply the deferral policy to a batch of inputs.
    ///
    /// While the thread waits on a pending active tool call, deferrable
    /// synthetic events are pushed into `defer` (unless the batch itself
    /// settles the pending call — via its actual result or an interrupting
    /// user message). When the thread can process deferred events, they are
    /// drained and appended after the incoming batch. Returns the batch that
    /// should be passed to [`step`](Self::step).
    pub async fn filter_deferrable<D: DeferQueue>(
        &self,
        inputs: Vec<(InputMessage, String)>,
        defer: &mut D,
    ) -> Result<Vec<(InputMessage, String)>, BoxError> {
        // Deferral decisions depend on tool passivity, so resolve the
        // thread's configuration if it hasn't been yet.
        self.resolved_config().await?;
        let Some(pending_call_id) = self.pending_active_tool_call() else {
            let mut batch = inputs;
            batch.extend(defer.drain().await?);
            return Ok(batch);
        };

        // Flushing is safe when the batch itself settles the pending call
        // first: either it contains the call's actual tool result, or a user
        // text input (which deliberately interrupts the pending call). Batch
        // items are processed before drained deferred items, so ordering is
        // preserved.
        let settles_pending = inputs.iter().any(|(msg, _)| {
            is_user_text_input(msg)
                || (msg.synthetic.is_none()
                    && matches!(
                        &msg.content,
                        InputMessageContent::User(UserContent::ToolResult(r))
                            if r.id == pending_call_id
                    ))
        });

        if settles_pending {
            let mut batch = inputs;
            batch.extend(defer.drain().await?);
            return Ok(batch);
        }

        let mut batch = Vec::new();
        for (msg, id) in inputs {
            if is_deferrable_synthetic_event(&msg) && defer.push(msg.clone(), id.clone()).await? {
                continue;
            }
            batch.push((msg, id));
        }
        Ok(batch)
    }

    /// Prepare a single input message into history, emitting the
    /// corresponding events. Returns `true` when the input is actionable
    /// (a completion should run). Errors from the observer's durability
    /// hooks fail the step; errors from input preparation itself are
    /// surfaced as [`AgentEvent::Info`].
    async fn prepare_one<O: ThreadObserver>(
        &self,
        input_msg: InputMessage,
        message_id: String,
        observer: &O,
    ) -> Result<bool, BoxError> {
        let thread_id = self.history.thread_id.clone();
        let prepare_result = event_processor::prepare_input(
            input_msg.clone(),
            message_id,
            &self.history,
            &self.inner.conversation_store,
            &self.inner.sender,
        )
        .await;

        match prepare_result {
            Ok(event_processor::PrepareResult::Handled) => Ok(false),
            Ok(event_processor::PrepareResult::CompactionApplied) => {
                observer.on_event(&thread_id, &AgentEvent::CompactionApplied);
                Ok(false)
            }
            Ok(event_processor::PrepareResult::OAuthRequired { auth_url }) => {
                observer.on_event(&thread_id, &AgentEvent::OAuthRequired { auth_url });
                Ok(false)
            }
            Ok(event_processor::PrepareResult::UserChoiceRequired {
                id,
                prompt,
                choices,
                default,
                response_url,
            }) => {
                let choice = UserChoice {
                    id,
                    prompt,
                    choices,
                    default,
                    response_url,
                };
                self.inner
                    .state_store
                    .add_pending_user_choice(&thread_id, choice.clone())
                    .await?;
                {
                    let mut pending = self.pending_choices.borrow_mut();
                    if let Some(existing) = pending.iter_mut().find(|c| c.id == choice.id) {
                        *existing = choice.clone();
                    } else {
                        pending.push(choice.clone());
                    }
                }
                observer.on_event(&thread_id, &AgentEvent::UserChoiceRequired { choice });
                Ok(false)
            }
            Err(e) => {
                observer.on_event(
                    &thread_id,
                    &AgentEvent::Info {
                        text: format!("Error: {}", e),
                    },
                );
                Ok(false)
            }
            Ok(event_processor::PrepareResult::Ready) => {
                self.emit_input_echo(&input_msg, observer, &thread_id);
                if let InputMessageContent::User(UserContent::ToolResult(result)) =
                    &input_msg.content
                    && self
                        .pending_choices
                        .borrow()
                        .iter()
                        .any(|choice| choice.id == result.id)
                {
                    self.inner
                        .state_store
                        .remove_pending_user_choice(&thread_id, &result.id)
                        .await?;
                    self.pending_choices
                        .borrow_mut()
                        .retain(|choice| choice.id != result.id);
                    observer.on_event(
                        &thread_id,
                        &AgentEvent::UserChoiceDismissed {
                            choice_id: result.id.clone(),
                        },
                    );
                }
                Ok(true)
            }
        }
    }

    /// Emit the event describing an accepted input: a subscription event or
    /// thread report, an incoming tool result, or user text. The event itself
    /// is computed by the shared [`event_processor::input_event`].
    fn emit_input_echo<O: ThreadObserver>(
        &self,
        input_msg: &InputMessage,
        observer: &O,
        thread_id: &ThreadId<str>,
    ) {
        if let Some(event) = event_processor::input_event(&self.history, input_msg) {
            observer.on_event(thread_id, &event);
        }
    }

    /// Execute one step over a batch that has **not** been run through the
    /// deferral policy: prepare `inputs` into history, run at most one
    /// completion round, commit durably, and dispatch at most one
    /// asynchronous tool call. Callers are expected to apply
    /// [`filter_deferrable`](Self::filter_deferrable) first.
    ///
    /// Events are delivered to `observer` synchronously as they occur; the
    /// turn is synced to the conversation store before the tool call (if
    /// any) is dispatched.
    ///
    /// `cancel` aborts the completion stream early (user interruption). The
    /// cancelled step still flushes and commits whatever streamed before the
    /// cancellation. **Note:** dropping the paired sender counts as
    /// cancellation, so callers that don't intend to cancel must keep the
    /// sender alive for the duration of the step.
    pub async fn step_no_defer<O: ThreadObserver>(
        &self,
        inputs: Vec<(InputMessage, String)>,
        observer: &O,
        cancel: oneshot::Receiver<()>,
    ) -> Result<StepOutcome, BoxError> {
        let thread_id = self.history.thread_id.clone();
        // Resolve the thread's configuration before anything observable
        // happens (interruption notifications below already need the
        // notifier; a completion needs the tools).
        let config = self.resolved_config().await?;

        // ── Prepare phase: fold each input into history ──
        let mut any_ready = false;
        let mut last_message_id = String::new();
        for (input_msg, message_id) in inputs {
            if self
                .prepare_one(input_msg, message_id.clone(), observer)
                .await?
            {
                any_ready = true;
                last_message_id = message_id;
            }
        }

        // Best-effort: notify RAP tool servers about interrupted tool calls.
        // Dismissing the associated pending user choices is a durable state
        // transition: it is awaited before the step continues, so the agent
        // cannot proceed past an interruption whose dismissal has not been
        // persisted.
        let interrupted = self.history.take_interrupted_tool_calls();
        if !interrupted.is_empty() {
            if let Some(notifier) = config.rap_notifier.as_ref() {
                for call_id in &interrupted {
                    notifier.notify_tool_cancelled(&thread_id, call_id).await;
                }
            }
            for call_id in interrupted {
                self.inner
                    .state_store
                    .remove_pending_user_choice(&thread_id, &call_id)
                    .await?;
                self.pending_choices
                    .borrow_mut()
                    .retain(|choice| choice.id != call_id);
                observer.on_event(
                    &thread_id,
                    &AgentEvent::UserChoiceDismissed { choice_id: call_id },
                );
            }
        }

        if !any_ready {
            // Commit anything already known-safe (e.g. processed IDs from
            // deduped inputs). Interruption results and other fresh inputs
            // stay unvalidated — and unpersisted — until a completion
            // produces model output for them.
            self.history.sync().await?;
            return Ok(StepOutcome::Skipped);
        }

        // ── Completion phase ──
        let model = self.inner.model.resolve(&thread_id).await?;

        let tool_names: HashSet<String> =
            config.tools.iter().map(|t| t.name().to_owned()).collect();
        let tool_defs: Vec<ToolDefinition> = config
            .tools
            .iter()
            .map(|t| ToolDefinition {
                name: t.name().to_owned(),
                description: t.description().to_owned(),
                parameters: t.parameters(),
            })
            .collect();
        let tool_registry: HashMap<String, &dyn Tool<M>> = config
            .tools
            .iter()
            .map(|t| (t.name().to_owned(), t.as_ref()))
            .collect();

        let user_id = self
            .history
            .get_metadata()
            .and_then(|m| m.get("user_id").and_then(|v| v.as_str()).map(String::from));
        let tool_context = ToolContext {
            message_sender: self.inner.sender.clone(),
            group_id: thread_id.clone(),
            callback_url: self.inner.callback_url.clone(),
            user_id,
            thread_stack: self.history.get_thread_stack(),
        };

        observer.on_event(&thread_id, &AgentEvent::CompletionStarted);
        self.in_flight.set(true);

        let mut action = None;
        let mut usage: Option<Usage> = None;
        {
            let mut stream = std::pin::pin!(event_processor::run_completion(
                model.provider.as_ref(),
                &model.model_id,
                model.supports_image_input,
                &self.history,
                &tool_names,
                &tool_defs,
                &tool_registry,
                &tool_context,
                &thread_id,
                &last_message_id,
                config.extra_system_prompt.as_deref(),
                cancel,
            ));

            while let Some(ev) = stream.next().await {
                match ev {
                    Ok(event) => self.handle_completion_event(
                        event,
                        observer,
                        &thread_id,
                        &mut action,
                        &mut usage,
                    ),
                    Err(e) => {
                        observer.on_event(
                            &thread_id,
                            &AgentEvent::Info {
                                text: format!("Error: {}", e),
                            },
                        );
                        break;
                    }
                }
            }
        }
        self.in_flight.set(false);
        *self.current_thinking.borrow_mut() = None;

        // ── Commit phase: the turn becomes durable before anything external
        // can react to it. ──
        self.history.sync().await?;
        observer.on_event(&thread_id, &AgentEvent::CompletionFinished { usage });

        // ── Dispatch phase: fire the tool call (if any) after the commit
        // barrier, so the call is durable in history before its result can
        // possibly arrive. A failed dispatch enqueues an error tool result
        // so the agent recovers instead of waiting forever. ──
        if let Some(action) = action
            && let Err(e) = event_processor::execute_action_with_error_result(
                action,
                &tool_registry,
                &tool_context,
            )
            .await
        {
            observer.on_event(
                &thread_id,
                &AgentEvent::Info {
                    text: format!("Error: {}", e),
                },
            );
        }

        Ok(StepOutcome::Completed {
            usage,
            context_window: model.context_window,
        })
    }

    fn handle_completion_event(
        &self,
        event: CompletionEvent,
        observer: &impl ThreadObserver,
        thread_id: &ThreadId<str>,
        action: &mut Option<CompletionAction>,
        usage: &mut Option<Usage>,
    ) {
        match event {
            CompletionEvent::Info(text) => {
                observer.on_event(thread_id, &AgentEvent::Info { text });
            }
            CompletionEvent::TextChunk(text) => {
                *self.current_thinking.borrow_mut() = None;
                observer.on_event(thread_id, &AgentEvent::TextChunk { text });
            }
            CompletionEvent::ThinkingStart => {
                *self.current_thinking.borrow_mut() = None;
                observer.on_event(thread_id, &AgentEvent::ThinkingStarted);
            }
            CompletionEvent::ThinkingEnd => {
                *self.current_thinking.borrow_mut() = None;
                observer.on_event(thread_id, &AgentEvent::ThinkingEnded);
            }
            CompletionEvent::ThinkingChunk(text) => {
                self.current_thinking
                    .borrow_mut()
                    .get_or_insert_default()
                    .push_str(&text);
                observer.on_event(thread_id, &AgentEvent::ThinkingChunk { text });
            }
            CompletionEvent::SyncToolCall {
                tool_name,
                tool_args,
                display_as,
            } => {
                *self.current_thinking.borrow_mut() = None;
                observer.on_event(
                    thread_id,
                    &AgentEvent::ToolCall {
                        name: tool_name,
                        args: tool_args,
                        display_as,
                    },
                );
            }
            CompletionEvent::SyncToolResult(res) => {
                *self.current_thinking.borrow_mut() = None;
                if let Some(ToolResultContent::Text(text)) = res.content.first() {
                    observer.on_event(
                        thread_id,
                        &AgentEvent::ToolResult {
                            segments: vec![rap_protocol::DisplaySegment::Text(text.text.clone())],
                        },
                    );
                }
            }
            CompletionEvent::Action(CompletionAction::Done(r)) => {
                // There may be multiple `Done` if the agent synchronously
                // loops back; the last one carries the round's final usage.
                *self.current_thinking.borrow_mut() = None;
                if let Some(u) = r.usage {
                    *usage = Some(u);
                }
            }
            CompletionEvent::Action(a) => {
                if let CompletionAction::ExecuteToolCall {
                    ref tool_name,
                    ref tool_args,
                    ref display_as,
                    ..
                } = a
                {
                    *self.current_thinking.borrow_mut() = None;
                    observer.on_event(
                        thread_id,
                        &AgentEvent::ToolCall {
                            name: tool_name.clone(),
                            args: tool_args.clone(),
                            display_as: display_as.clone(),
                        },
                    );
                }
                assert!(action.is_none(), "bug: multiple terminal actions");
                *action = Some(a);
            }
        }
    }
}
#[cfg(test)]
mod tests {
    use crate::system::test_support::*;

    /// A tool call whose dispatch fails must feed an error result back to the
    /// agent (as a new input through the loopback queue) instead of leaving the
    /// thread waiting forever on a result that will never come.
    #[tokio::test(flavor = "current_thread")]
    async fn failed_tool_dispatch_returns_error_result_to_agent() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (mut running, mut rx, mut ctrl, _conv) =
                    start_system(vec![Box::new(FailingTool)], None);
                running
                    .send_user_text(rap_protocol::ThreadId::from_ref("t1"), "use tool")
                    .await;
                let _req = ctrl.next_request().await;
                ctrl.send_tool_call("call-1", "failing_tool", serde_json::json!({}));
                ctrl.finish();

                // The dispatch failure enqueues a fallback error result, which
                // wakes the thread for a second completion round.
                let req2 = ctrl.next_request().await;
                assert!(
                    tool_result_texts(&req2)
                        .iter()
                        .any(|t| t.contains("Error: Tool call failed")),
                    "second round should see the fallback error tool result"
                );
                ctrl.send_text("recovered");
                ctrl.finish();

                collect_until_finished(&mut rx).await; // round 1
                let texts = collect_until_finished(&mut rx).await; // round 2
                assert_eq!(texts, vec!["recovered"]);
                wait_idle(&mut running).await;
            })
            .await;
    }
}
