//! Building an [`AgentSystem`]: the entry point of the high-level API.

use std::rc::Rc;

use async_trait::async_trait;
use tokio::sync::{mpsc, oneshot};

use crate::message::InputMessage;
use crate::traits::{ConversationStore, InputSender, StateStore};
use rap_client::http::HttpClient;

use super::config::{StaticThreadConfig, ThreadConfigSource};
use super::defer::DeferQueue;
use super::local::ChannelSender;
use super::model::ModelSource;
use super::observer::ThreadObserver;
use super::thread::{StepOutcome, Thread};

type BoxError = Box<dyn std::error::Error + Send + Sync>;

type BuildParts<C, S, M, H> = (
    Rc<SystemInner<C, S, M, H>>,
    Option<mpsc::UnboundedReceiver<(InputMessage, String)>>,
);

/// A placeholder [`HttpClient`] used as the default when no RAP notifier is
/// configured. Its methods are never invoked.
#[derive(Clone)]
pub struct NoRapHttp;

#[derive(Debug)]
pub struct NoRapHttpError;

impl std::fmt::Display for NoRapHttpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "no HTTP client configured")
    }
}

impl std::error::Error for NoRapHttpError {}

#[async_trait]
impl HttpClient for NoRapHttp {
    type Error = NoRapHttpError;

    async fn post(&self, _url: &str, _body: &str) -> Result<u16, NoRapHttpError> {
        Err(NoRapHttpError)
    }

    async fn get(&self, _url: &str) -> Result<(u16, Vec<u8>), NoRapHttpError> {
        Err(NoRapHttpError)
    }
}

/// Shared configuration of an agent system, referenced by every [`Thread`].
pub(crate) struct SystemInner<C, S, M, H>
where
    C: ConversationStore,
    S: StateStore,
    M: InputSender,
    H: HttpClient,
{
    pub conversation_store: C,
    pub state_store: S,
    pub model: Box<dyn ModelSource>,
    pub config: Box<dyn ThreadConfigSource<M, H>>,
    pub sender: M,
    pub callback_url: String,
    pub builtin_tools: bool,
    pub tokio_sleep_tools: bool,
}

/// Builder for an [`AgentSystem`].
///
/// Two construction modes decide how messages flow back into the system:
///
/// - [`AgentSystemBuilder::new`] takes the platform's own [`InputSender`]
///   (e.g. an SQS sender on Lambda). The built system exposes the **step
///   API**: the platform delivers each thread's message batches and calls
///   [`AgentSystem::step`] per batch.
/// - [`AgentSystemBuilder::new_local`] creates an internal in-process queue
///   ([`ChannelSender`]).
///   [`LocalAgentSystem::start_with_observer`] then runs the full
///   actor-system-style driver: a router that spawns one worker per thread,
///   batches inputs, handles interruption, deferral, idling, and
///   auto-compaction.
///
/// [`thread_config`](Self::thread_config) resolves a
/// [`ThreadConfig`](super::ThreadConfig) per thread load. The built-in thread
/// and subscription tools (`spawn_thread`,
/// `report_to_parent`, `close_thread`, `send_message_to_child`,
/// `cancel_subscription`, `sleep_until_event_or_input`) are added on top in
/// both cases; disable with
/// [`without_builtin_tools`](Self::without_builtin_tools).
pub struct AgentSystemBuilder<C, S, M, H = NoRapHttp>
where
    C: ConversationStore,
    S: StateStore,
    M: InputSender,
    H: HttpClient,
{
    conversation_store: C,
    state_store: S,
    model: Box<dyn ModelSource>,
    config: Option<Box<dyn ThreadConfigSource<M, H>>>,
    sender: M,
    local_rx: Option<mpsc::UnboundedReceiver<(InputMessage, String)>>,
    callback_url: String,
    builtin_tools: bool,
    tokio_sleep_tools: bool,
}

impl<C, S, M, H> AgentSystemBuilder<C, S, M, H>
where
    C: ConversationStore + 'static,
    S: StateStore + 'static,
    M: InputSender + 'static,
    H: HttpClient + 'static,
{
    /// Shared constructor: `local_rx` is `Some` exactly when the sender is
    /// the internal in-process queue created by [`new_local`].
    ///
    /// [`new_local`]: AgentSystemBuilder::new_local
    fn with_sender(
        conversation_store: C,
        state_store: S,
        model: impl ModelSource + 'static,
        sender: M,
        local_rx: Option<mpsc::UnboundedReceiver<(InputMessage, String)>>,
    ) -> Self {
        AgentSystemBuilder {
            conversation_store,
            state_store,
            model: Box::new(model),
            config: None,
            sender,
            local_rx,
            callback_url: String::new(),
            builtin_tools: true,
            tokio_sleep_tools: false,
        }
    }

    /// Resolve tools, prompt, and notifier dynamically per thread. Use this
    /// when threads need different toolsets, such as one session per working
    /// directory, each with its own tool servers.
    pub fn thread_config<H2: HttpClient + 'static>(
        self,
        source: impl ThreadConfigSource<M, H2> + 'static,
    ) -> AgentSystemBuilder<C, S, M, H2> {
        AgentSystemBuilder {
            conversation_store: self.conversation_store,
            state_store: self.state_store,
            model: self.model,
            config: Some(Box::new(source)),
            sender: self.sender,
            local_rx: self.local_rx,
            callback_url: self.callback_url,
            builtin_tools: self.builtin_tools,
            tokio_sleep_tools: self.tokio_sleep_tools,
        }
    }

    /// The base URL RAP tool servers should POST results to.
    pub fn callback_url(mut self, url: impl Into<String>) -> Self {
        self.callback_url = url.into();
        self
    }

    /// Do not register the built-in thread/subscription tools. Useful for
    /// systems that need a minimal or fully custom toolset.
    pub fn without_builtin_tools(mut self) -> Self {
        self.builtin_tools = false;
        self
    }

    /// Register timed sleep tools backed by in-process tokio timers
    /// (`sleep`, `sleep_until`). Only suitable for resident runtimes; on
    /// serverless platforms register durable-timer equivalents instead.
    pub fn with_tokio_sleep_tools(mut self) -> Self {
        self.tokio_sleep_tools = true;
        self
    }

    fn build_inner(self) -> BuildParts<C, S, M, H> {
        let config: Box<dyn ThreadConfigSource<M, H>> = match self.config {
            Some(source) => source,
            None => Box::new(StaticThreadConfig::default()),
        };
        let model = self.model;
        (
            Rc::new(SystemInner {
                conversation_store: self.conversation_store,
                state_store: self.state_store,
                model,
                config,
                sender: self.sender,
                callback_url: self.callback_url,
                builtin_tools: self.builtin_tools,
                tokio_sleep_tools: self.tokio_sleep_tools,
            }),
            self.local_rx,
        )
    }

    /// Build a step-mode system driven by an external transport.
    pub fn build(self) -> AgentSystem<C, S, M, H> {
        let (inner, _local_rx) = self.build_inner();
        AgentSystem { inner }
    }
}

impl<C, S, M> AgentSystemBuilder<C, S, M, NoRapHttp>
where
    C: ConversationStore + 'static,
    S: StateStore + 'static,
    M: InputSender + 'static,
{
    /// Start building a system whose loopback path is `sender` (the
    /// platform's own transport, e.g. SQS). Use
    /// [`new_local`](AgentSystemBuilder::new_local) for a self-contained
    /// in-process system.
    pub fn new(
        conversation_store: C,
        state_store: S,
        model: impl ModelSource + 'static,
        sender: M,
    ) -> Self {
        Self::with_sender(conversation_store, state_store, model, sender, None)
    }
}

impl<C, S> AgentSystemBuilder<C, S, ChannelSender, NoRapHttp>
where
    C: ConversationStore + 'static,
    S: StateStore + 'static,
{
    /// Start building a self-contained local system with an internal
    /// in-process input queue. Tools should be registered against
    /// [`ChannelSender`].
    pub fn new_local(
        conversation_store: C,
        state_store: S,
        model: impl ModelSource + 'static,
    ) -> Self {
        let (sender, rx) = ChannelSender::new_pair();
        Self::with_sender(conversation_store, state_store, model, sender, Some(rx))
    }
}

impl<C, S, H> AgentSystemBuilder<C, S, ChannelSender, H>
where
    C: ConversationStore + 'static,
    S: StateStore + 'static,
    H: HttpClient + 'static,
{
    /// Build a local system ready for
    /// [`start_with_observer`](LocalAgentSystem::start_with_observer).
    /// Requires construction via [`new_local`](AgentSystemBuilder::new_local).
    pub fn build_local(self) -> LocalAgentSystem<C, S, H> {
        let (inner, local_rx) = self.build_inner();
        let input_rx = local_rx.expect(
            "bug: build_local requires a builder created with AgentSystemBuilder::new_local",
        );
        LocalAgentSystem {
            system: AgentSystem { inner },
            input_rx,
        }
    }
}

/// A configured system of agents sharing stores, tools, and a model source.
///
/// Analogous to an actor system: threads (root agents and their subagents)
/// are the actors, and the [`InputSender`] is how messages are delivered to
/// them. `AgentSystem` itself holds no running state — each
/// [`step`](Self::step) loads the thread's state from the stores, runs one
/// slice, and drops it, which is what makes the step API fit serverless
/// platforms where nothing survives between slices.
pub struct AgentSystem<C, S, M, H>
where
    C: ConversationStore,
    S: StateStore,
    M: InputSender,
    H: HttpClient,
{
    pub(crate) inner: Rc<SystemInner<C, S, M, H>>,
}

impl<C, S, M, H> AgentSystem<C, S, M, H>
where
    C: ConversationStore + 'static,
    S: StateStore + 'static,
    M: InputSender + 'static,
    H: HttpClient + 'static,
{
    /// Run one step for every thread with messages in `inputs`, concurrently.
    /// This is the whole per-slice job of a step-mode embedding (e.g. one
    /// Lambda invocation).
    ///
    /// The batch may span multiple threads (`InputMessage::group_id`) — e.g.
    /// an SQS FIFO delivery with a batch size above 1 can interleave several
    /// message groups. `step` partitions the batch by thread (preserving
    /// arrival order within each), applies the deferral policy per thread,
    /// and then joins the per-thread steps: each loads the thread's history
    /// and dedup state from the stores, prepares its inputs into history,
    /// runs at most one completion round, commits durably, and dispatches at
    /// most one asynchronous tool call. Returns each thread's
    /// [`StepOutcome`].
    ///
    /// Nothing is cached between calls — the loaded thread state lives only
    /// for the duration of the call, which is what makes this fit serverless
    /// platforms. Steps must still be serialized per thread *across* calls:
    /// within a process the `&mut self` receiver enforces it (embeddings
    /// that want more in-process concurrency build one system per worker
    /// from their cloneable stores); across processes it is the transport's
    /// job — e.g. SQS FIFO message groups.
    ///
    /// `defer` is consulted once per thread in the batch; a durable
    /// [`DeferQueue`] implementation serving multi-thread batches should key
    /// its storage by `InputMessage::group_id`.
    ///
    /// If any thread's step fails, the first error is returned — but only
    /// after every thread's step has run to completion, so one thread's
    /// failure never aborts another's mid-commit.
    pub async fn step<O: ThreadObserver, D: DeferQueue>(
        &mut self,
        inputs: Vec<(InputMessage, String)>,
        observer: &O,
        defer: &mut D,
    ) -> Result<Vec<(String, StepOutcome)>, BoxError> {
        // Partition by thread, preserving arrival order within each thread.
        let mut groups: Vec<(String, Vec<(InputMessage, String)>)> = Vec::new();
        for (msg, id) in inputs {
            match groups.iter_mut().find(|(g, _)| *g == msg.group_id) {
                Some((_, batch)) => batch.push((msg, id)),
                None => groups.push((msg.group_id.clone(), vec![(msg, id)])),
            }
        }

        // Load and filter sequentially (the deferral policy needs exclusive
        // access to the queue), then run the steps concurrently.
        let mut ready = Vec::new();
        for (thread_id, batch) in groups {
            let thread = Thread::load(self.inner.clone(), thread_id.clone()).await?;
            let batch = thread.filter_deferrable(batch, defer).await?;
            ready.push((thread_id, thread, batch));
        }

        let results = futures_util::future::join_all(ready.into_iter().map(
            |(thread_id, thread, batch)| async move {
                // Keep the cancel sender alive for the duration of the step:
                // dropping it would signal cancellation.
                let (_cancel_tx, cancel_rx) = oneshot::channel();
                let outcome = thread.step_no_defer(batch, observer, cancel_rx).await;
                (thread_id, outcome)
            },
        ))
        .await;

        let mut outcomes = Vec::with_capacity(results.len());
        for (thread_id, outcome) in results {
            outcomes.push((thread_id, outcome?));
        }
        Ok(outcomes)
    }
}

/// A built local system: an [`AgentSystem`] plus the receiving end of its
/// internal input queue. Use
/// [`start_with_observer`](Self::start_with_observer) to run it.
pub struct LocalAgentSystem<C, S, H>
where
    C: ConversationStore,
    S: StateStore,
    H: HttpClient,
{
    pub(crate) system: AgentSystem<C, S, ChannelSender, H>,
    pub(crate) input_rx: mpsc::UnboundedReceiver<(InputMessage, String)>,
}

impl<C, S, H> LocalAgentSystem<C, S, H>
where
    C: ConversationStore + 'static,
    S: StateStore + 'static,
    H: HttpClient + 'static,
{
    /// The system's [`InputSender`] (for callback servers and external
    /// injectors).
    pub fn sender(&self) -> ChannelSender {
        self.system.inner.sender.clone()
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::{InputMessage, InputMessageContent, OAuthRequired};
    use crate::stores::{InMemoryConversationStore, InMemoryStateStore};
    use crate::system::defer::NoDeferral;
    use crate::system::events::AgentEvent;
    use crate::system::local::ChannelSender;
    use crate::system::observer::EventCollector;
    use crate::system::test_support::*;
    use crate::system::thread::StepOutcome;

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
                    let mut system = system;
                    let collector = EventCollector::new();
                    let outcomes = system
                        .step(
                            vec![user_text_input("t1", "hello")],
                            &collector,
                            &mut NoDeferral,
                        )
                        .await
                        .expect("step");
                    (outcomes, collector.take())
                });

                let _req = ctrl.next_request().await;
                ctrl.send_text("hi there");
                ctrl.finish();

                let (outcomes, events) = step.await.expect("join");
                assert_eq!(outcomes.len(), 1);
                assert_eq!(outcomes[0].0, "t1");
                assert!(matches!(outcomes[0].1, StepOutcome::Completed { .. }));
                assert!(events.iter().any(|(t, e)| t == "t1"
                    && matches!(e, AgentEvent::UserInput { text } if text == "hello")));
                assert!(events.iter().any(|(t, e)| t == "t1"
                    && matches!(e, AgentEvent::TextChunk { text } if text == "hi there")));
                assert!(
                    events
                        .iter()
                        .any(|(_, e)| matches!(e, AgentEvent::CompletionFinished { .. }))
                );
                // Nothing was scheduled for later.
                assert!(loopback_rx.try_recv().is_err());
            })
            .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn step_mode_runs_multiple_threads_from_one_batch() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (model, mut ctrl) = model_source(None);
                let (sender, _loopback_rx) = ChannelSender::new_pair();
                let system = AgentSystemBuilder::new(
                    InMemoryConversationStore::new(),
                    InMemoryStateStore::new(),
                    model,
                    sender,
                )
                .build();

                // One delivered batch interleaving two message groups.
                let step = tokio::task::spawn_local(async move {
                    let mut system = system;
                    let collector = EventCollector::new();
                    let outcomes = system
                        .step(
                            vec![
                                user_text_input("t1", "hello from one"),
                                user_text_input("t2", "hello from two"),
                            ],
                            &collector,
                            &mut NoDeferral,
                        )
                        .await
                        .expect("step");
                    (outcomes, collector.take())
                });

                // The controller serves rounds in FIFO order (responding to one
                // round at a time), while the system runs both threads' steps
                // joined on the same task.
                let _req1 = ctrl.next_request().await;
                ctrl.send_text("one");
                ctrl.finish();
                let _req2 = ctrl.next_request().await;
                ctrl.send_text("two");
                ctrl.finish();

                let (outcomes, events) = step.await.expect("join");
                let mut ids: Vec<&str> = outcomes.iter().map(|(t, _)| t.as_str()).collect();
                ids.sort_unstable();
                assert_eq!(ids, ["t1", "t2"]);
                assert!(
                    outcomes
                        .iter()
                        .all(|(_, o)| matches!(o, StepOutcome::Completed { .. }))
                );
                assert!(events.iter().any(|(t, e)| t == "t1"
                    && matches!(e, AgentEvent::TextChunk { text } if text == "one")));
                assert!(events.iter().any(|(t, e)| t == "t2"
                    && matches!(e, AgentEvent::TextChunk { text } if text == "two")));
            })
            .await;
    }

    /// A batch mixing an out-of-band input (an OAuth challenge, which never
    /// enters history) with actionable user text: the challenge is surfaced as an
    /// event, and only the text triggers a completion.
    #[tokio::test(flavor = "current_thread")]
    async fn step_mode_surfaces_oauth_and_completes_actionable_input() {
        let local = tokio::task::LocalSet::new();
        local
        .run_until(async {
            let (model, mut ctrl) = model_source(None);
            let (sender, _loopback_rx) = ChannelSender::new_pair();
            let system = AgentSystemBuilder::new(
                InMemoryConversationStore::new(),
                InMemoryStateStore::new(),
                model,
                sender,
            )
            .build();

            let oauth = (
                InputMessage {
                    content: InputMessageContent::OAuth(OAuthRequired {
                        content_type: "oauth_required".into(),
                        id: "o1".into(),
                        call_id: None,
                        auth_url: "https://example.com/auth".into(),
                    }),
                    group_id: "t1".into(),
                    metadata: None,
                    synthetic: None,
                    display_as: None,
                    subscription: false,
                },
                uuid::Uuid::new_v4().to_string(),
            );

            let step = tokio::task::spawn_local(async move {
                let mut system = system;
                let collector = EventCollector::new();
                let outcomes = system
                    .step(
                        vec![oauth, user_text_input("t1", "hello")],
                        &collector,
                        &mut NoDeferral,
                    )
                    .await
                    .expect("step");
                (outcomes, collector.take())
            });

            let _req = ctrl.next_request().await;
            ctrl.send_text("hi there");
            ctrl.finish();

            let (outcomes, events) = step.await.expect("join");
            assert_eq!(outcomes.len(), 1);
            assert!(matches!(outcomes[0].1, StepOutcome::Completed { .. }));
            assert!(events.iter().any(|(t, e)| t == "t1"
                && matches!(e, AgentEvent::OAuthRequired { auth_url } if auth_url == "https://example.com/auth")));
            assert!(events.iter().any(|(t, e)| t == "t1"
                && matches!(e, AgentEvent::UserInput { text } if text == "hello")));
        })
        .await;
    }
}
