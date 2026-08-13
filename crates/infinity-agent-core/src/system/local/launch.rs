//! Launching threads with their own tools and prompt.
//!
//! Opt in with [`AgentSystemBuilder::with_thread_launcher`], start the system
//! with [`LocalAgentSystem::start_with_launcher`], and create threads through
//! [`LaunchingSystem::thread_builder`]: each launched thread gets a generated
//! ID and runs with the union of the system-wide configuration and the tools
//! and prompt registered at launch.
//!
//! [`AgentSystemBuilder::with_thread_launcher`]: super::AgentSystemBuilder::with_thread_launcher
//! [`LocalAgentSystem::start_with_launcher`]: super::LocalAgentSystem::start_with_launcher

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use async_trait::async_trait;

use crate::tools::Tool;
use crate::traits::{ConversationStore, InputSender, StateStore};
use rap_client::http::HttpClient;

use super::driver::ThreadLifecycleEvent;
use super::handle::{HandleSubscribeRequest, ThreadHandle, attach, handle_observer_factory};
use super::router::RunningSystem;
use super::sender::ChannelSender;
use crate::message::InputMessage;
use crate::system::builder::{AgentSystem, Launcher, LocalAgentSystem};
use crate::system::config::{ThreadConfig, ThreadConfigSource};
use crate::system::model::{ModelSource, ResolvedModel};

type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// The tools, prompt, and model a thread was launched with.
struct LaunchConfig<M: InputSender> {
    tools: Vec<Rc<dyn Tool<M>>>,
    extra_system_prompt: Option<String>,
    model: Option<Rc<dyn ModelSource>>,
}

/// Per-thread launch configurations, shared between the running system's
/// [`UnionConfigSource`] and the [`LaunchingSystem`] that registers entries.
/// Lives (and dies) with the process: launch configurations are not
/// persisted, so a thread resumed after a restart runs with the system-wide
/// configuration only.
pub(crate) struct LaunchRegistry<M: InputSender> {
    threads: RefCell<HashMap<String, LaunchConfig<M>>>,
}

impl<M: InputSender> Default for LaunchRegistry<M> {
    fn default() -> Self {
        Self {
            threads: RefCell::new(HashMap::new()),
        }
    }
}

/// Resolve the root thread of `thread_id`. Launch configurations attach to
/// the launched thread, which is always a root; subagent threads resolve to
/// their root so they inherit its entry.
async fn root_thread_id<C: ConversationStore>(
    store: &C,
    thread_id: &str,
) -> Result<String, BoxError> {
    Ok(store
        .get_ancestor_chain(thread_id)
        .await
        .map_err(|e| Box::new(e) as BoxError)?
        .first()
        .map(|(id, _)| id.clone())
        .unwrap_or_else(|| thread_id.to_owned()))
}

/// Wraps the system-wide [`ModelSource`]: threads launched with their own
/// model (and the subagent threads they spawn, via root resolution) resolve
/// through it; all other threads fall through to the inner source.
pub(crate) struct UnionModelSource<C: ConversationStore, M: InputSender> {
    pub(crate) inner: Box<dyn ModelSource>,
    pub(crate) registry: Rc<LaunchRegistry<M>>,
    pub(crate) conversation_store: C,
}

#[async_trait(?Send)]
impl<C, M> ModelSource for UnionModelSource<C, M>
where
    C: ConversationStore + 'static,
    M: InputSender + 'static,
{
    async fn resolve(&self, thread_id: &str) -> Result<ResolvedModel, BoxError> {
        let root_id = root_thread_id(&self.conversation_store, thread_id).await?;
        let launched = self
            .registry
            .threads
            .borrow()
            .get(&root_id)
            .and_then(|config| config.model.clone());
        match launched {
            Some(model) => model.resolve(thread_id).await,
            None => self.inner.resolve(thread_id).await,
        }
    }
}

/// Wraps the system-wide [`ThreadConfigSource`], unioning each launched
/// thread's registered tools and prompt on top of whatever the inner source
/// resolves. Launch configurations attach to the launched (root) thread, so
/// threads a launched thread spawns resolve their root's entry and inherit
/// its tools and prompt. Threads whose root has no registry entry see the
/// inner configuration unchanged.
pub(crate) struct UnionConfigSource<C: ConversationStore, M: InputSender, H: HttpClient> {
    pub(crate) inner: Box<dyn ThreadConfigSource<M, H>>,
    pub(crate) registry: Rc<LaunchRegistry<M>>,
    pub(crate) conversation_store: C,
}

#[async_trait(?Send)]
impl<C, M, H> ThreadConfigSource<M, H> for UnionConfigSource<C, M, H>
where
    C: ConversationStore + 'static,
    M: InputSender + 'static,
    H: HttpClient + 'static,
{
    async fn resolve(&self, thread_id: &str) -> Result<ThreadConfig<M, H>, BoxError> {
        let mut config = self.inner.resolve(thread_id).await?;
        let root_id = root_thread_id(&self.conversation_store, thread_id).await?;
        if let Some(local) = self.registry.threads.borrow().get(&root_id) {
            config.tools.extend(local.tools.iter().cloned());
            config.extra_system_prompt = match (
                config.extra_system_prompt.take(),
                local.extra_system_prompt.clone(),
            ) {
                (Some(system_wide), Some(local)) => Some(format!("{system_wide}\n\n{local}")),
                (system_wide, local) => system_wide.or(local),
            };
        }
        Ok(config)
    }
}

impl<C, S, H> LocalAgentSystem<C, S, H, Launcher>
where
    C: ConversationStore + 'static,
    S: StateStore + 'static,
    H: HttpClient + 'static,
{
    /// Run the system in launcher mode: create threads through
    /// [`LaunchingSystem::thread_builder`], each with its own tools and
    /// prompt unioned onto the system-wide configuration, and re-attach to
    /// existing threads with [`LaunchingSystem::thread_handle`].
    pub fn start(self) -> LaunchingSystem<C, S, H> {
        let registry = self.registry.clone();
        let system = AgentSystem {
            inner: self.system.inner.clone(),
        };
        let running = self.start_inner(handle_observer_factory());
        LaunchingSystem {
            system,
            running,
            registry,
        }
    }
}

/// A running local system whose threads are created through
/// [`thread_builder`](Self::thread_builder), each with its own tools and
/// prompt unioned onto the system-wide configuration.
///
/// Unlike [`RunningSystem::thread_handle`], which attaches to any thread ID
/// (creating its subscription on the spot), a launching system separates the
/// two intents: [`thread_builder`](Self::thread_builder) creates new threads
/// (generating their IDs), and [`thread_handle`](Self::thread_handle)
/// re-attaches to threads that already exist.
pub struct LaunchingSystem<C, S, H>
where
    C: ConversationStore,
    S: StateStore,
    H: HttpClient,
{
    pub(crate) system: AgentSystem<C, S, ChannelSender, H>,
    pub(crate) running: RunningSystem<HandleSubscribeRequest>,
    pub(crate) registry: Rc<LaunchRegistry<ChannelSender>>,
}

impl<C, S, H> LaunchingSystem<C, S, H>
where
    C: ConversationStore + 'static,
    S: StateStore + 'static,
    H: HttpClient + 'static,
{
    /// Start configuring a new thread. Finish with
    /// [`ThreadBuilder::launch`].
    pub fn thread_builder(&self) -> ThreadBuilder<'_, C, S, H> {
        ThreadBuilder {
            system: self,
            tools: Vec::new(),
            extra_system_prompt: None,
            model: None,
        }
    }

    /// Attach to an existing thread, returning `None` if no such thread
    /// exists. A thread exists if it was launched in this process or has
    /// history in the conversation store. New threads are created with
    /// [`thread_builder`](Self::thread_builder) instead.
    pub async fn thread_handle(&self, thread_id: &str) -> Option<ThreadHandle> {
        if !self.thread_exists(thread_id).await {
            return None;
        }
        Some(attach(&self.running, thread_id).await)
    }

    async fn thread_exists(&self, thread_id: &str) -> bool {
        if self.registry.threads.borrow().contains_key(thread_id) {
            return true;
        }
        match self
            .system
            .inner
            .conversation_store
            .load_history_with_ancestors(thread_id)
            .await
        {
            Ok((history, _, _)) => !history.is_empty(),
            Err(_) => false,
        }
    }

    /// The system's [`InputSender`](crate::traits::InputSender), for callback
    /// servers and any other external message injectors.
    pub fn sender(&self) -> ChannelSender {
        self.running.sender()
    }

    /// Deliver an input message to its thread (`message.group_id`). See
    /// [`RunningSystem::send`].
    pub async fn send(&self, message: InputMessage, dedup_id: &str) {
        self.running.send(message, dedup_id).await
    }

    /// Receives a [`ThreadLifecycleEvent`] each time a thread's driver
    /// spawns or idles out. See
    /// [`RunningSystem::thread_lifecycle`](RunningSystem#structfield.thread_lifecycle).
    pub fn thread_lifecycle(
        &mut self,
    ) -> &mut tokio::sync::mpsc::UnboundedReceiver<ThreadLifecycleEvent> {
        &mut self.running.thread_lifecycle
    }

    /// Whether no thread driver is currently live. Threads with active
    /// subscriptions but no pending work do not count as live; their events
    /// respawn a driver when they arrive.
    pub fn is_idle(&self) -> bool {
        self.running.is_idle()
    }

    /// Wind the whole system down (process exit): every driver flushes its
    /// in-flight turn and exits; resolves when the wind-down is complete.
    pub async fn shutdown(self) {
        self.running.shutdown().await;
    }
}

/// Configures and launches one new thread on a [`LaunchingSystem`]. The
/// thread's ID is generated at [`launch`](Self::launch); the registered tools
/// and prompt are unioned onto the system-wide configuration for every
/// completion the thread runs.
pub struct ThreadBuilder<'a, C, S, H>
where
    C: ConversationStore,
    S: StateStore,
    H: HttpClient,
{
    system: &'a LaunchingSystem<C, S, H>,
    tools: Vec<Rc<dyn Tool<ChannelSender>>>,
    extra_system_prompt: Option<String>,
    model: Option<Rc<dyn ModelSource>>,
}

impl<C, S, H> ThreadBuilder<'_, C, S, H>
where
    C: ConversationStore + 'static,
    S: StateStore + 'static,
    H: HttpClient + 'static,
{
    /// Give the thread an additional tool (on top of the system-wide tools).
    pub fn tool(mut self, tool: Box<dyn Tool<ChannelSender>>) -> Self {
        self.tools.push(Rc::from(tool));
        self
    }

    /// Give the thread additional tools (on top of the system-wide tools).
    pub fn tools(mut self, tools: impl IntoIterator<Item = Box<dyn Tool<ChannelSender>>>) -> Self {
        self.tools.extend(tools.into_iter().map(Rc::from));
        self
    }

    /// Append thread-specific text to the system prompt (after any
    /// system-wide extra prompt).
    pub fn extra_system_prompt(mut self, prompt: impl Into<String>) -> Self {
        self.extra_system_prompt = Some(prompt.into());
        self
    }

    /// Give the thread its own [`ModelSource`] in place of the system-wide
    /// one. Like the system-wide source, it resolves at the start of every
    /// completion round, and threads this thread spawns inherit it.
    pub fn model(mut self, model: impl ModelSource + 'static) -> Self {
        self.model = Some(Rc::new(model));
        self
    }

    /// Create the thread: generate its ID, register the launch
    /// configuration, and attach. Returns the thread's [`ThreadHandle`]
    /// (its ID is [`ThreadHandle::thread_id`]).
    ///
    /// The launch configuration lives for the lifetime of the process; a
    /// thread resumed after a restart runs with the system-wide
    /// configuration only.
    pub async fn launch(self) -> ThreadHandle {
        let thread_id = uuid::Uuid::new_v4().to_string();
        self.system.registry.threads.borrow_mut().insert(
            thread_id.clone(),
            LaunchConfig {
                tools: self.tools,
                extra_system_prompt: self.extra_system_prompt,
                model: self.model,
            },
        );
        attach(&self.system.running, &thread_id).await
    }
}
#[cfg(test)]
mod tests {
    use crate::message::InputMessage;
    use crate::system::test_support::*;

    #[tokio::test(flavor = "current_thread")]
    async fn launched_threads_union_their_own_tools_and_prompt() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (system, mut ctrl) = start_launcher_system(
                    vec![Box::new(NamedTool("global_tool"))],
                    "Global prompt.",
                );

                let mut alpha = system
                    .thread_builder()
                    .tool(Box::new(NamedTool("alpha_tool")))
                    .extra_system_prompt("You are alpha.")
                    .launch()
                    .await;
                let mut beta = system
                    .thread_builder()
                    .tool(Box::new(NamedTool("beta_tool")))
                    .launch()
                    .await;
                assert_ne!(alpha.thread_id(), beta.thread_id());

                alpha.send_user_text("hi").await.expect("send input");
                let req = ctrl.next_request().await;
                let names = tool_names(&req);
                assert!(names.contains(&"global_tool".to_owned()));
                assert!(names.contains(&"alpha_tool".to_owned()));
                assert!(!names.contains(&"beta_tool".to_owned()));
                let preamble = req.preamble.clone().expect("system prompt present");
                assert!(preamble.contains("Global prompt."));
                assert!(preamble.contains("You are alpha."));
                ctrl.send_text("hello alpha");
                ctrl.finish();
                assert_eq!(
                    handle_texts_until_finished(&mut alpha).await,
                    ["hello alpha"]
                );

                beta.send_user_text("hi").await.expect("send input");
                let req = ctrl.next_request().await;
                let names = tool_names(&req);
                assert!(names.contains(&"global_tool".to_owned()));
                assert!(names.contains(&"beta_tool".to_owned()));
                assert!(!names.contains(&"alpha_tool".to_owned()));
                let preamble = req.preamble.clone().expect("system prompt present");
                assert!(preamble.contains("Global prompt."));
                assert!(!preamble.contains("You are alpha."));
                ctrl.send_text("hello beta");
                ctrl.finish();
                assert_eq!(handle_texts_until_finished(&mut beta).await, ["hello beta"]);
            })
            .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn launcher_thread_handle_attaches_to_existing_threads_only() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (mut system, mut ctrl) = start_launcher_system(vec![], "");

                // Unknown threads cannot be attached to.
                assert!(system.thread_handle("no-such-thread").await.is_none());

                // Launched threads can, even before their first message.
                let launched = system.thread_builder().launch().await;
                let launched_id = launched.thread_id().to_owned();
                assert!(system.thread_handle(&launched_id).await.is_some());

                // Threads with history (created by direct sends, e.g. RAP
                // callbacks) exist too.
                system
                    .send(
                        InputMessage::user_text("history-thread", "hello"),
                        "dedup-1",
                    )
                    .await;
                let _req = ctrl.next_request().await;
                ctrl.send_text("hi");
                ctrl.finish();
                // Wait for the round to settle (drivers idle out).
                while !system.is_idle() {
                    tokio::time::timeout(
                        std::time::Duration::from_secs(5),
                        system.thread_lifecycle().recv(),
                    )
                    .await
                    .expect("timed out waiting for a driver exit")
                    .expect("thread lifecycle channel closed");
                }
                let mut attached = system
                    .thread_handle("history-thread")
                    .await
                    .expect("thread has history");
                assert!(
                    !attached.replay().history.is_empty(),
                    "replay includes the thread's history"
                );
                attached.send_user_text("again").await.expect("send input");
                let _req = ctrl.next_request().await;
                ctrl.send_text("still here");
                ctrl.finish();
                assert_eq!(
                    handle_texts_until_finished(&mut attached).await,
                    ["still here"]
                );
            })
            .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn launched_thread_children_inherit_launch_config() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (system, mut ctrl) = start_launcher_system(
                    vec![Box::new(NamedTool("global_tool"))],
                    "Global prompt.",
                );

                let alpha = system
                    .thread_builder()
                    .tool(Box::new(NamedTool("alpha_tool")))
                    .extra_system_prompt("You are alpha.")
                    .launch()
                    .await;
                let alpha_id = alpha.thread_id().to_owned();

                alpha
                    .send_user_text("spawn a child")
                    .await
                    .expect("send input");
                let _req = ctrl.next_request().await;
                ctrl.send_tool_call(
                    "tc-spawn",
                    "spawn_thread",
                    serde_json::json!({
                        "instructions": "do child work",
                        "child_of": [alpha_id],
                    }),
                );
                ctrl.finish();

                // The spawn is synchronous: the parent loops back with the
                // spawn result first.
                let parent_followup = ctrl.next_request().await;
                assert!(tool_names(&parent_followup).contains(&"alpha_tool".to_owned()));
                ctrl.send_text("spawned");
                ctrl.finish();

                // The child's seed message produces its first completion. Launch
                // configurations attach to the launched (root) thread, so the
                // child runs with alpha's tools and prompt.
                let child_req = ctrl.next_request().await;
                let names = tool_names(&child_req);
                assert!(names.contains(&"global_tool".to_owned()));
                assert!(
                    names.contains(&"alpha_tool".to_owned()),
                    "child threads inherit the launched root's tools"
                );
                let preamble = child_req.preamble.expect("system prompt present");
                assert!(
                    preamble.contains("You are alpha."),
                    "child threads inherit the launched root's prompt"
                );
                ctrl.send_text("child done");
                ctrl.finish();
            })
            .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn launched_threads_use_their_own_model_and_children_inherit_it() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (system, mut system_ctrl) = start_launcher_system(vec![], "");
                let (alpha_model, mut alpha_ctrl) = model_source(None);

                let alpha = system.thread_builder().model(alpha_model).launch().await;
                let alpha_id = alpha.thread_id().to_owned();
                let other = system.thread_builder().launch().await;

                // The launched model serves alpha's rounds; the system-wide
                // model serves everyone else.
                alpha.send_user_text("hi").await.expect("send input");
                let _req = alpha_ctrl.next_request().await;
                alpha_ctrl.send_text("from alpha model");
                alpha_ctrl.finish();

                other.send_user_text("hi").await.expect("send input");
                let _req = system_ctrl.next_request().await;
                system_ctrl.send_text("from system model");
                system_ctrl.finish();

                // A child spawned by alpha inherits alpha's model.
                alpha
                    .send_user_text("spawn a child")
                    .await
                    .expect("send input");
                let _req = alpha_ctrl.next_request().await;
                alpha_ctrl.send_tool_call(
                    "tc-spawn",
                    "spawn_thread",
                    serde_json::json!({
                        "instructions": "do child work",
                        "child_of": [alpha_id],
                    }),
                );
                alpha_ctrl.finish();

                // Parent follow-up (spawn is synchronous), then the child's
                // seed completion: both on alpha's model.
                let _parent_followup = alpha_ctrl.next_request().await;
                alpha_ctrl.send_text("spawned");
                alpha_ctrl.finish();
                let child_req = alpha_ctrl.next_request().await;
                assert!(
                    tool_result_texts(&child_req)
                        .iter()
                        .any(|t| t.contains("do child work")),
                    "the request on alpha's model is the child's seed"
                );
                alpha_ctrl.send_text("child done");
                alpha_ctrl.finish();
            })
            .await;
    }
}
