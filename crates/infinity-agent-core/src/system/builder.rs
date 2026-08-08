//! Building an [`AgentSystem`]: the entry point of the high-level API.

use std::rc::Rc;

use async_trait::async_trait;
use tokio::sync::{mpsc, oneshot};

use crate::message::InputMessage;
use crate::tools::Tool;
use crate::traits::{ConversationStore, InputSender, StateStore};
use rap_client::http::HttpClient;
use rap_client::notifier::RapNotifier;

use super::config::{StaticThreadConfig, ThreadConfigSource};
use super::defer::DeferQueue;
use super::model::ModelSource;
use super::observer::ThreadObserver;
use super::sender::ChannelSender;
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
///   ([`ChannelSender`]). [`LocalAgentSystem::start`] then runs the full
///   actor-system-style driver: a router that spawns one worker per thread,
///   batches inputs, handles interruption, deferral, idling, and
///   auto-compaction.
///
/// Tool/prompt configuration is either **static** (the `tools`,
/// `extra_system_prompt`, and `rap_notifier` methods — every thread sees the
/// same set) or **dynamic** via [`thread_config`](Self::thread_config), which
/// resolves a [`ThreadConfig`](super::ThreadConfig) per thread load. The
/// built-in thread and subscription tools (`spawn_thread`,
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
    tools: Vec<Rc<dyn Tool<M>>>,
    config: Option<Box<dyn ThreadConfigSource<M, H>>>,
    sender: M,
    local_rx: Option<mpsc::UnboundedReceiver<(InputMessage, String)>>,
    extra_system_prompt: Option<String>,
    callback_url: String,
    rap_notifier: Option<RapNotifier<H>>,
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
    /// Register an additional tool (static configuration).
    pub fn tool(mut self, tool: Box<dyn Tool<M>>) -> Self {
        self.tools.push(Rc::from(tool));
        self
    }

    /// Register additional tools (static configuration).
    pub fn tools(mut self, tools: impl IntoIterator<Item = Box<dyn Tool<M>>>) -> Self {
        self.tools.extend(tools.into_iter().map(Rc::from));
        self
    }

    /// Append text to the built-in system prompt (static configuration).
    pub fn extra_system_prompt(mut self, prompt: impl Into<String>) -> Self {
        self.extra_system_prompt = Some(prompt.into());
        self
    }

    /// Resolve tools, prompt, and notifier dynamically per thread instead of
    /// using the static `tools`/`extra_system_prompt`/`rap_notifier`
    /// configuration (which must not be combined with a source). Use this
    /// when threads need different toolsets — e.g. one session per working
    /// directory, each with its own tool servers.
    pub fn thread_config<H2: HttpClient + 'static>(
        self,
        source: impl ThreadConfigSource<M, H2> + 'static,
    ) -> AgentSystemBuilder<C, S, M, H2> {
        assert!(
            self.tools.is_empty()
                && self.extra_system_prompt.is_none()
                && self.rap_notifier.is_none(),
            "thread_config replaces the static tools/extra_system_prompt/rap_notifier configuration; do not combine them"
        );
        AgentSystemBuilder {
            conversation_store: self.conversation_store,
            state_store: self.state_store,
            model: self.model,
            tools: Vec::new(),
            config: Some(Box::new(source)),
            sender: self.sender,
            local_rx: self.local_rx,
            extra_system_prompt: None,
            callback_url: self.callback_url,
            rap_notifier: None,
            builtin_tools: self.builtin_tools,
            tokio_sleep_tools: self.tokio_sleep_tools,
        }
    }

    /// The base URL RAP tool servers should POST results to.
    pub fn callback_url(mut self, url: impl Into<String>) -> Self {
        self.callback_url = url.into();
        self
    }

    /// Do not register the built-in thread/subscription tools.
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

    /// Configure the notifier that sends best-effort lifecycle notifications
    /// (tool cancellation, thread closure) to RAP tool servers (static
    /// configuration; with [`thread_config`](Self::thread_config) the
    /// notifier comes from each thread's `ThreadConfig` instead).
    pub fn rap_notifier<H2: HttpClient + 'static>(
        self,
        notifier: RapNotifier<H2>,
    ) -> AgentSystemBuilder<C, S, M, H2> {
        assert!(
            self.config.is_none(),
            "rap_notifier cannot be combined with thread_config; put the notifier in the ThreadConfig instead"
        );
        AgentSystemBuilder {
            conversation_store: self.conversation_store,
            state_store: self.state_store,
            model: self.model,
            tools: self.tools,
            config: None,
            sender: self.sender,
            local_rx: self.local_rx,
            extra_system_prompt: self.extra_system_prompt,
            callback_url: self.callback_url,
            rap_notifier: Some(notifier),
            builtin_tools: self.builtin_tools,
            tokio_sleep_tools: self.tokio_sleep_tools,
        }
    }

    fn build_inner(self) -> BuildParts<C, S, M, H> {
        let config: Box<dyn ThreadConfigSource<M, H>> = match self.config {
            Some(source) => source,
            None => Box::new(StaticThreadConfig {
                tools: self.tools,
                extra_system_prompt: self.extra_system_prompt,
                rap_notifier: self.rap_notifier,
            }),
        };
        (
            Rc::new(SystemInner {
                conversation_store: self.conversation_store,
                state_store: self.state_store,
                model: self.model,
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
        AgentSystemBuilder {
            conversation_store,
            state_store,
            model: Box::new(model),
            tools: Vec::new(),
            config: None,
            sender,
            local_rx: None,
            extra_system_prompt: None,
            callback_url: String::new(),
            rap_notifier: None,
            builtin_tools: true,
            tokio_sleep_tools: false,
        }
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
        AgentSystemBuilder {
            conversation_store,
            state_store,
            model: Box::new(model),
            tools: Vec::new(),
            config: None,
            sender,
            local_rx: Some(rx),
            extra_system_prompt: None,
            callback_url: String::new(),
            rap_notifier: None,
            builtin_tools: true,
            tokio_sleep_tools: false,
        }
    }
}

impl<C, S, H> AgentSystemBuilder<C, S, ChannelSender, H>
where
    C: ConversationStore + 'static,
    S: StateStore + 'static,
    H: HttpClient + 'static,
{
    /// Build a local system ready to [`start`](LocalAgentSystem::start).
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
/// them. `AgentSystem` itself holds no running state — [`thread`](Self::thread)
/// loads a [`Thread`] handle on demand, which is what makes the step API fit
/// serverless platforms where nothing survives between slices.
pub struct AgentSystem<C, S, M, H>
where
    C: ConversationStore,
    S: StateStore,
    M: InputSender,
    H: HttpClient,
{
    pub(crate) inner: Rc<SystemInner<C, S, M, H>>,
}

impl<C, S, M, H> Clone for AgentSystem<C, S, M, H>
where
    C: ConversationStore,
    S: StateStore,
    M: InputSender,
    H: HttpClient,
{
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

impl<C, S, M, H> AgentSystem<C, S, M, H>
where
    C: ConversationStore + 'static,
    S: StateStore + 'static,
    M: InputSender + 'static,
    H: HttpClient + 'static,
{
    /// The sender used for the system's loopback path.
    pub fn sender(&self) -> &M {
        &self.inner.sender
    }

    /// Load a handle to one conversation thread (restoring its history and
    /// state from the stores, and resolving its per-thread configuration).
    pub async fn thread(&self, thread_id: &str) -> Result<Thread<C, S, M, H>, BoxError> {
        Thread::load(self.inner.clone(), thread_id.to_owned()).await
    }

    /// Convenience: load `thread_id`, apply the deferral policy, and run one
    /// step over `inputs`. This is the whole per-slice job of a step-mode
    /// embedding (e.g. one Lambda invocation).
    pub async fn step<O: ThreadObserver, D: DeferQueue>(
        &self,
        thread_id: &str,
        inputs: Vec<(InputMessage, String)>,
        observer: &O,
        defer: &mut D,
    ) -> Result<StepOutcome, BoxError> {
        let thread = self.thread(thread_id).await?;
        // Keep the cancel sender alive for the duration of the step: dropping
        // it would signal cancellation.
        let (_cancel_tx, cancel_rx) = oneshot::channel();
        thread.step(inputs, observer, defer, cancel_rx).await
    }
}

/// A built local system: an [`AgentSystem`] plus the receiving end of its
/// internal input queue. Call [`start`](Self::start) to run it.
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

    /// The underlying system, for direct [`Thread`] access.
    pub fn system(&self) -> &AgentSystem<C, S, ChannelSender, H> {
        &self.system
    }
}
