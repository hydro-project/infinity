//! [`Thread`]: a handle to one conversation thread, with a step-oriented
//! execution API.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use futures_util::StreamExt;
use rig::completion::{GetTokenUsage, ToolDefinition, Usage};
use rig::message::{ToolResultContent, UserContent};
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

/// The result of one [`Thread::step`].
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
        thread_id: String,
    ) -> Result<Self, BoxError> {
        let history = HistoryManager::new_with_history(
            inner.conversation_store.clone(),
            inner.state_store.clone(),
            thread_id,
        )
        .await?;
        Ok(Self {
            inner,
            history,
            config: RefCell::new(None),
            current_thinking: RefCell::new(None),
            in_flight: Cell::new(false),
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

    pub fn thread_id(&self) -> &str {
        &self.history.thread_id
    }

    /// The thread's low-level history manager, for advanced integrations.
    pub fn history(&self) -> &HistoryManager<C, S> {
        &self.history
    }

    /// A live view of the thread for bringing a new subscriber up to date:
    /// committed history plus the in-flight turn, the in-progress reasoning
    /// text, and whether a completion is currently streaming.
    pub fn replay_snapshot(&self) -> ReplaySnapshot {
        ReplaySnapshot {
            history: self.history.current_turn_view(),
            current_thinking: self.current_thinking.borrow().clone(),
            in_progress: self.in_flight.get(),
        }
    }

    /// Whether a completion round is currently streaming.
    pub fn in_flight(&self) -> bool {
        self.in_flight.get()
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

    /// Whether the thread should stay resident waiting for something: an
    /// unanswered tool call (other than `close_thread`, whose result never
    /// arrives) or an active subscription.
    pub async fn expects_wakeup(&self) -> bool {
        let awaiting_call = self
            .pending_tool_call()
            .is_some_and(|(_, name)| name != CLOSE_THREAD_TOOL);
        awaiting_call || self.history.has_active_subscriptions().await
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
                // Awaited durability hook: the pending choice must be
                // persisted (and surfaced) before the step continues.
                observer
                    .on_user_choice_required(
                        &thread_id,
                        &UserChoice {
                            id,
                            prompt,
                            choices,
                            default,
                            response_url,
                        },
                    )
                    .await?;
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
                Ok(true)
            }
        }
    }

    /// Emit the event describing an accepted input: a subscription event or
    /// thread report, an incoming tool result, or user text.
    fn emit_input_echo<O: ThreadObserver>(
        &self,
        input_msg: &InputMessage,
        observer: &O,
        thread_id: &str,
    ) {
        use rig::message::{AssistantContent, Message};

        if let Some(synth) = input_msg.synthetic.as_ref() {
            if let InputMessageContent::User(UserContent::ToolResult(res)) = &input_msg.content
                && let ToolResultContent::Text(text) = res.content.first()
            {
                let orig_call = self.history.get_history(true).into_iter().find(|h| {
                    if let Message::Assistant { content, .. } = h
                        && let AssistantContent::ToolCall(c) = content.first()
                    {
                        c.id == synth.tool_call_id()
                    } else {
                        false
                    }
                });

                if let Some(Message::Assistant { content, .. }) = orig_call
                    && let AssistantContent::ToolCall(c) = content.first()
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
                    observer.on_event(
                        thread_id,
                        &AgentEvent::SubscriptionEvent {
                            name,
                            text: text.text,
                        },
                    );
                }
            }
        } else if let InputMessageContent::User(UserContent::ToolResult(res)) = &input_msg.content
            && let ToolResultContent::Text(text) = res.content.first()
        {
            observer.on_event(
                thread_id,
                &AgentEvent::ToolResult {
                    segments: rap_protocol::build_display_segments(
                        input_msg.display_as.as_deref(),
                        &text.text,
                    ),
                },
            );
        } else if let InputMessageContent::User(UserContent::Text(ref text)) = input_msg.content {
            let display_text = text.text.strip_prefix("<interrupt>").unwrap_or(&text.text);
            observer.on_event(
                thread_id,
                &AgentEvent::UserInput {
                    text: display_text.to_owned(),
                },
            );
        }
    }

    /// Execute one step: apply the deferral policy to `inputs`, prepare the
    /// remainder into history, run at most one completion round, commit
    /// durably, and dispatch at most one asynchronous tool call.
    ///
    /// This is [`filter_deferrable`](Self::filter_deferrable) followed by
    /// [`step_no_defer`](Self::step_no_defer); see the latter for the
    /// step's semantics (observer delivery, commit barrier, cancellation).
    /// Callers that need to act between the two phases (e.g. to avoid
    /// starting a step at all when every input was deferred, as the local
    /// driver does) compose the pieces themselves.
    pub async fn step<O: ThreadObserver, D: DeferQueue>(
        &self,
        inputs: Vec<(InputMessage, String)>,
        observer: &O,
        defer: &mut D,
        cancel: oneshot::Receiver<()>,
    ) -> Result<StepOutcome, BoxError> {
        let batch = self.filter_deferrable(inputs, defer).await?;
        self.step_no_defer(batch, observer, cancel).await
    }

    /// Execute one step over a batch that has **not** been run through the
    /// deferral policy: prepare `inputs` into history, run at most one
    /// completion round, commit durably, and dispatch at most one
    /// asynchronous tool call. Most callers want [`step`](Self::step), which
    /// applies [`filter_deferrable`](Self::filter_deferrable) first.
    ///
    /// Events are delivered to `observer` synchronously as they occur; the
    /// observer's `on_commit` is awaited after the turn is synced to the
    /// conversation store and before the tool call (if any) is dispatched.
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
                observer
                    .on_user_choice_dismissed(&thread_id, &call_id)
                    .await?;
            }
        }

        if !any_ready {
            // Commit anything prepare persisted (processed IDs, interruption
            // results) even though no completion runs.
            self.history.sync().await?;
            observer.on_commit(&thread_id).await?;
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
        let _flight_guard = FlightGuard {
            in_flight: &self.in_flight,
            thinking: &self.current_thinking,
        };

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
        drop(_flight_guard);

        // ── Commit phase: the turn becomes durable before anything external
        // can react to it. ──
        self.history.sync().await?;
        observer.on_event(&thread_id, &AgentEvent::CompletionFinished { usage });
        observer.on_commit(&thread_id).await?;

        // ── Dispatch phase: fire the tool call (if any) after the commit
        // barrier, so the call is durable in history before its result can
        // possibly arrive. ──
        if let Some(action) = action
            && let Err(e) =
                event_processor::execute_action(action, &tool_registry, &tool_context).await
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
        event: CompletionEvent<infinity_provider_protocol::ProviderStreamingResponse>,
        observer: &impl ThreadObserver,
        thread_id: &str,
        action: &mut Option<
            CompletionAction<infinity_provider_protocol::ProviderStreamingResponse>,
        >,
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
                if let ToolResultContent::Text(text) = res.content.first() {
                    observer.on_event(
                        thread_id,
                        &AgentEvent::ToolResult {
                            segments: vec![rap_protocol::DisplaySegment::Text(text.text)],
                        },
                    );
                }
            }
            CompletionEvent::Action(CompletionAction::Done(r)) => {
                // There may be multiple `Done` if the agent synchronously
                // loops back; the last one carries the round's final usage.
                *self.current_thinking.borrow_mut() = None;
                if let Some(u) = r.token_usage() {
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

/// Resets the in-flight flag and thinking buffer even if the step future is
/// dropped mid-stream.
struct FlightGuard<'a> {
    in_flight: &'a Cell<bool>,
    thinking: &'a RefCell<Option<String>>,
}

impl Drop for FlightGuard<'_> {
    fn drop(&mut self) {
        self.in_flight.set(false);
        *self.thinking.borrow_mut() = None;
    }
}
