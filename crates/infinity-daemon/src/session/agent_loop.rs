use std::collections::HashMap;
use std::sync::Arc;

use infinity_agent_core::tools::Tool;
use tokio::sync::mpsc;

use super::thread_worker::{SubscribeRequest, thread_worker};
use super::{AgentMessage, SubscriberMap};
use crate::memory_store::{InMemoryConversationStore, InMemoryMessageSender, InMemoryStateStore};
use crate::models::ModelCatalog;
use crate::rap_tools;
use crate::session::ActiveThreads;

struct WorkerChannels {
    input_tx: mpsc::UnboundedSender<(infinity_agent_core::message::InputMessage, String)>,
    subscribe_tx: mpsc::UnboundedSender<SubscribeRequest>,
    model_switch_tx: mpsc::UnboundedSender<infinity_protocol::ModelRef>,
    handle: tokio::task::JoinHandle<()>,
}

#[expect(
    clippy::too_many_arguments,
    reason = "agent loop requires many dependencies"
)]
pub async fn agent_loop(
    session_id: String,
    mut rx: mpsc::UnboundedReceiver<AgentMessage>,
    catalog: Arc<ModelCatalog>,
    conversation_store: InMemoryConversationStore,
    state_store: InMemoryStateStore,
    sender: InMemoryMessageSender,
    callback_url: String,
    tool_impls: Arc<Vec<Box<dyn Tool<InMemoryMessageSender>>>>,
    extra_system_prompt: Arc<Option<String>>,
    rap_notifier: Option<rap_client::notifier::RapNotifier<rap_tools::SimpleHttpClient>>,
    subscriber_map: SubscriberMap,
    active_threads: ActiveThreads,
    idle_tx: mpsc::UnboundedSender<()>,
    shutdown: tokio_util::sync::CancellationToken,
) {
    let mut workers: HashMap<String, WorkerChannels> = HashMap::new();

    loop {
        let msg = tokio::select! {
            biased;
            _ = shutdown.cancelled() => {
                tracing::info!("Agent loop for session {} received shutdown signal", session_id);
                None
            }
            msg = rx.recv() => msg,
        };
        let Some(msg) = msg else { break };
        let thread_id = match &msg {
            AgentMessage::Input(input, _) => input.group_id.clone(),
            AgentMessage::Subscribe { thread_id, .. } => thread_id.clone(),
            AgentMessage::SwitchModel { thread_id, .. } => thread_id.clone(),
        };

        // Check if worker is alive.
        if let Some(w) = workers.get(&thread_id) {
            if !w.input_tx.is_closed() {
                match msg {
                    AgentMessage::Input(input, id) => {
                        let _ = w.input_tx.send((*input, id));
                    }
                    AgentMessage::Subscribe { request, .. } => {
                        tracing::debug!("Worker is already alive, sending subscribe request");
                        let _ = w.subscribe_tx.send(request);
                        // TODO(shadaj): also subscribe to alive children
                    }
                    AgentMessage::SwitchModel { model, .. } => {
                        let _ = w.model_switch_tx.send(model);
                    }
                }
                continue;
            }
            workers.remove(&thread_id);
        }

        // No live worker. A model switch is already persisted in the
        // conversation store, so there is nothing to deliver — the worker
        // resolves the stored selection when it next spawns. Don't spin up a
        // worker just for this (it would immediately idle out again).
        if matches!(msg, AgentMessage::SwitchModel { .. }) {
            continue;
        }

        // Spawn a new worker.
        let parent_subs = {
            let parent_id = conversation_store.get_thread_parent_id(&thread_id);
            let smap = subscriber_map.lock().expect("bug: mutex poisoned");
            let source = parent_id.as_deref().unwrap_or(&thread_id);
            smap.get(source)
                .map(|arc| arc.lock().expect("bug: mutex poisoned").clone())
                .unwrap_or_default()
        };
        let (input_tx, input_rx) = mpsc::unbounded_channel();
        let (subscribe_tx, subscribe_rx) = mpsc::unbounded_channel();
        let (model_switch_tx, model_switch_rx) = mpsc::unbounded_channel();

        let subscribers = subscriber_map
            .lock()
            .expect("bug: mutex poisoned")
            .entry(thread_id.clone())
            .or_insert_with(|| Arc::new(std::sync::Mutex::new(parent_subs)))
            .clone();

        let handle = tokio::task::spawn_local(rap_protocol::log_panic(
            "thread_worker",
            thread_worker(
                thread_id.clone(),
                input_rx,
                subscribe_rx,
                model_switch_rx,
                active_threads.clone(),
                subscribers,
                session_id.clone(),
                catalog.clone(),
                conversation_store.clone(),
                state_store.clone(),
                sender.clone(),
                callback_url.clone(),
                tool_impls.clone(),
                extra_system_prompt.as_ref().clone(),
                rap_notifier.clone(),
                idle_tx.clone(),
            ),
        ));

        match msg {
            AgentMessage::Input(input, id) => {
                let _ = input_tx.send((*input, id));
            }
            AgentMessage::Subscribe { request, .. } => {
                let _ = subscribe_tx.send(request);
            }
            AgentMessage::SwitchModel { .. } => {
                unreachable!("bug: SwitchModel with no live worker is handled above")
            }
        }
        workers.insert(
            thread_id,
            WorkerChannels {
                input_tx,
                subscribe_tx,
                model_switch_tx,
                handle,
            },
        );
    }

    // Wind down: dropping each worker's channels signals it to interrupt any
    // in-flight completion (which flushes pending history items — e.g. tool
    // results — to the store) and exit. Wait for every worker to finish so
    // the session is not torn down underneath them.
    let handles: Vec<(String, tokio::task::JoinHandle<()>)> = workers
        .drain()
        .map(|(thread_id, w)| (thread_id, w.handle))
        .collect();
    for (thread_id, handle) in handles {
        if let Err(e) = handle.await {
            if e.is_panic() {
                tracing::error!("thread worker {thread_id} panicked during shutdown: {e}");
            } else {
                tracing::warn!("thread worker {thread_id} cancelled during shutdown: {e}");
            }
        }
    }
}

#[cfg(test)]
#[expect(clippy::collapsible_if, reason = "readability")]
mod tests {
    use std::collections::HashSet;

    use super::*;
    use infinity_agent_core::message::{InputMessage, InputMessageContent};
    use infinity_agent_core::traits::ConversationStore;
    use infinity_provider_protocol::{ModelEntry, SingleModelProvider};
    use rig::message::UserContent;
    use rig_mock::mock_model;

    fn test_model_ref() -> infinity_protocol::ModelRef {
        infinity_protocol::ModelRef {
            provider_id: "mock".to_owned(),
            model_id: "mock".to_owned(),
        }
    }

    async fn test_catalog(
        model: rig_mock::MockCompletionModel,
        context_window: usize,
    ) -> Arc<ModelCatalog> {
        let entry = ModelEntry {
            model_id: "mock".to_owned(),
            display_name: "mock".to_owned(),
            context_window,
            max_output_tokens: None,
            supports_image_input: false,
        };
        Arc::new(
            ModelCatalog::new(vec![(
                "mock".to_owned(),
                Arc::new(SingleModelProvider::new(entry, model)) as _,
            )])
            .await
            .expect("build test catalog"),
        )
    }

    async fn two_model_catalog(
        model1: rig_mock::MockCompletionModel,
        model2: rig_mock::MockCompletionModel,
    ) -> Arc<ModelCatalog> {
        Arc::new(
            ModelCatalog::new(vec![
                (
                    "provider1".to_owned(),
                    Arc::new(SingleModelProvider::new(
                        ModelEntry {
                            model_id: "model1".to_owned(),
                            display_name: "model1".to_owned(),
                            context_window: 0,
                            max_output_tokens: None,
                            supports_image_input: false,
                        },
                        model1,
                    )) as _,
                ),
                (
                    "provider2".to_owned(),
                    Arc::new(SingleModelProvider::new(
                        ModelEntry {
                            model_id: "model2".to_owned(),
                            display_name: "model2".to_owned(),
                            context_window: 0,
                            max_output_tokens: None,
                            supports_image_input: false,
                        },
                        model2,
                    )) as _,
                ),
            ])
            .await
            .expect("build two-model catalog"),
        )
    }

    fn tmp_stores() -> (
        InMemoryConversationStore,
        InMemoryStateStore,
        tempfile::TempDir,
    ) {
        let dir = tempfile::tempdir().expect("create temp dir");
        let conv = InMemoryConversationStore::new_with_dir(
            dir.path().join("threads"),
            test_model_ref(),
            Arc::new(crate::ids::UuidIdSource),
        );
        let state = InMemoryStateStore::new(dir.path().join("state"));
        (conv, state, dir)
    }

    fn user_text_input(group_id: &str, text: &str) -> AgentMessage {
        AgentMessage::Input(
            Box::new(InputMessage {
                content: InputMessageContent::User(UserContent::text(text)),
                group_id: group_id.into(),
                metadata: None,
                synthetic: None,
                display_as: None,
                subscription: false,
            }),
            uuid::Uuid::new_v4().to_string(),
        )
    }

    async fn spawn_test_agent_loop(
        session_id: &str,
        conv: InMemoryConversationStore,
        state: InMemoryStateStore,
        model: rig_mock::MockCompletionModel,
    ) -> (
        mpsc::UnboundedSender<AgentMessage>,
        mpsc::UnboundedReceiver<()>,
        ActiveThreads,
        tokio_util::sync::CancellationToken,
    ) {
        let (agent_tx, agent_rx) = mpsc::unbounded_channel();
        let (idle_tx, idle_rx) = mpsc::unbounded_channel();
        let (input_tx, mut input_adapter_rx) = mpsc::unbounded_channel::<(InputMessage, String)>();
        let agent_tx_clone = agent_tx.clone();
        tokio::task::spawn_local(async move {
            while let Some((msg, id)) = input_adapter_rx.recv().await {
                if agent_tx_clone
                    .send(AgentMessage::Input(Box::new(msg), id))
                    .is_err()
                {
                    break;
                }
            }
        });
        let sender = InMemoryMessageSender::new(input_tx);
        let subscriber_map: SubscriberMap = Arc::new(std::sync::Mutex::new(HashMap::new()));
        let active_threads = Arc::new(std::sync::Mutex::new(HashSet::new()));
        let shutdown = tokio_util::sync::CancellationToken::new();

        tokio::task::spawn_local(agent_loop(
            session_id.into(),
            agent_rx,
            test_catalog(model, 0).await,
            conv,
            state,
            sender,
            String::new(),
            Arc::new(vec![]),
            Arc::new(None),
            None,
            subscriber_map,
            active_threads.clone(),
            idle_tx,
            shutdown.clone(),
        ));

        (agent_tx, idle_rx, active_threads, shutdown)
    }

    #[tokio::test(flavor = "current_thread")]
    async fn routes_to_separate_thread_workers() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (conv, state, _dir) = tmp_stores();
                let (model, mut ctrl) = mock_model();
                conv.ensure_root_thread("root")
                    .await
                    .expect("ensure root thread");
                let child_id = conv
                    .spawn_thread("root", "tc-spawn", false, None)
                    .await
                    .expect("spawn child thread");

                let (tx, mut idle_rx, _, _shutdown) =
                    spawn_test_agent_loop("root", conv, state, model).await;

                tx.send(user_text_input("root", "hello root"))
                    .expect("send root user input");
                let _req1 = ctrl.next_request().await;
                ctrl.send_text("root resp");
                ctrl.finish();

                tokio::time::timeout(std::time::Duration::from_secs(2), idle_rx.recv())
                    .await
                    .expect("root should idle");

                tx.send(AgentMessage::Input(
                    Box::new(InputMessage {
                        content: InputMessageContent::User(UserContent::text("hello child")),
                        group_id: child_id.clone(),
                        metadata: None,
                        synthetic: None,
                        display_as: None,
                        subscription: false,
                    }),
                    uuid::Uuid::new_v4().to_string(),
                ))
                .expect("send child user input");

                let req2 = ctrl.next_request().await;
                let has_child_msg = req2.chat_history.into_iter().any(|m| {
                    if let rig::message::Message::User { content } = &m {
                        if let UserContent::Text(t) = content.first() {
                            return t.text.contains("hello child");
                        }
                    }
                    false
                });
                assert!(has_child_msg);
                ctrl.send_text("child resp");
                ctrl.finish();
            })
            .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn respawns_worker_after_idle() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (conv, state, _dir) = tmp_stores();
                let (model, mut ctrl) = mock_model();
                conv.ensure_root_thread("t1")
                    .await
                    .expect("ensure root thread");

                let (tx, mut idle_rx, _, _shutdown) =
                    spawn_test_agent_loop("t1", conv, state, model).await;

                tx.send(user_text_input("t1", "first"))
                    .expect("send first user input");
                let _req1 = ctrl.next_request().await;
                ctrl.send_text("resp 1");
                ctrl.finish();

                tokio::time::timeout(std::time::Duration::from_secs(2), idle_rx.recv())
                    .await
                    .expect("should idle");

                tx.send(user_text_input("t1", "second"))
                    .expect("send second user input");
                let req2 = ctrl.next_request().await;
                let has_second = req2.chat_history.into_iter().any(|m| {
                    if let rig::message::Message::User { content } = &m {
                        if let UserContent::Text(t) = content.first() {
                            return t.text.contains("second");
                        }
                    }
                    false
                });
                assert!(has_second, "respawned worker should see second message");
                ctrl.send_text("resp 2");
                ctrl.finish();
            })
            .await;
    }

    /// Regression test: a tool result that arrived and is being processed by
    /// the model (completion in flight) must be persisted to the conversation
    /// store when the session is shut down. Shutdown interrupts the in-flight
    /// completion (stripping trailing reasoning) and waits for every thread
    /// worker to flush pending history items before the agent loop returns.
    #[tokio::test(flavor = "current_thread")]
    async fn shutdown_persists_in_flight_tool_result() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (conv, state, _dir) = tmp_stores();
                let (model, mut ctrl) = mock_model();
                conv.ensure_root_thread("t1")
                    .await
                    .expect("ensure root thread");

                use async_trait::async_trait;
                struct AsyncTool;
                #[async_trait]
                impl Tool<InMemoryMessageSender> for AsyncTool {
                    fn name(&self) -> &str {
                        "async_tool"
                    }
                    fn description(&self) -> &str {
                        "a"
                    }
                    fn parameters(&self) -> serde_json::Value {
                        serde_json::json!({"type":"object","properties":{}})
                    }
                    async fn execute(
                        &self,
                        _: serde_json::Value,
                        _: String,
                        _: Option<String>,
                        _: &infinity_agent_core::tools::ToolContext<InMemoryMessageSender>,
                    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
                        Ok(())
                    }
                }

                let (tx, mut idle_rx, active_threads, shutdown) = spawn_test_agent_loop_with_tools(
                    "t1",
                    conv.clone(),
                    state,
                    model,
                    vec![Box::new(AsyncTool)],
                    0,
                )
                .await;

                // 1. User input → model issues an async tool call.
                tx.send(user_text_input("t1", "do something"))
                    .expect("send user input");
                let _req = ctrl.next_request().await;
                ctrl.send_tool_call("tc-1", "async_tool", serde_json::json!({}));
                ctrl.finish();

                // 2. The tool result arrives → the worker starts a new
                //    completion. Waiting for the model request guarantees the
                //    completion is in flight and the tool result is sitting in
                //    the history manager's pending (unsynced) items.
                tx.send(AgentMessage::Input(
                    Box::new(InputMessage {
                        content: InputMessageContent::User(UserContent::ToolResult(
                            rig::message::ToolResult {
                                id: "tc-1".into(),
                                call_id: None,
                                content: rig::OneOrMany::one(
                                    rig::message::ToolResultContent::Text(rig::agent::Text {
                                        text: "tool execution result".into(),
                                    }),
                                ),
                            },
                        )),
                        group_id: "t1".into(),
                        metadata: None,
                        synthetic: None,
                        display_as: None,
                        subscription: false,
                    }),
                    uuid::Uuid::new_v4().to_string(),
                ))
                .expect("send tool result");
                let _req2 = ctrl.next_request().await;

                // 3. Shut down the session while the model is mid-response.
                shutdown.cancel();

                // 4. The worker interrupts the completion and exits; the
                //    WorkerGuard pings idle when the last worker is done.
                tokio::time::timeout(std::time::Duration::from_secs(5), idle_rx.recv())
                    .await
                    .expect("workers should wind down after shutdown");
                assert!(
                    active_threads
                        .lock()
                        .expect("bug: active_threads mutex poisoned")
                        .is_empty(),
                    "no thread workers should remain after shutdown"
                );

                // 5. The tool result must have been synced to the store.
                let history = conv
                    .load_history_up_to("t1", None, None)
                    .await
                    .expect("load history");
                let has_tool_result = history.iter().any(|m| {
                    if let infinity_agent_core::message::InfinityMessage::ToolResult {
                        result, ..
                    } = m
                        && let rig::message::ToolResultContent::Text(t) = result.content.first()
                    {
                        result.id == "tc-1" && t.text.contains("tool execution result")
                    } else {
                        false
                    }
                });
                assert!(
                    has_tool_result,
                    "in-flight tool result should be persisted on shutdown; history: {history:#?}"
                );
            })
            .await;
    }

    async fn spawn_test_agent_loop_with_tools(
        session_id: &str,
        conv: InMemoryConversationStore,
        state: InMemoryStateStore,
        model: rig_mock::MockCompletionModel,
        tools: Vec<Box<dyn Tool<InMemoryMessageSender>>>,
        context_window: usize,
    ) -> (
        mpsc::UnboundedSender<AgentMessage>,
        mpsc::UnboundedReceiver<()>,
        ActiveThreads,
        tokio_util::sync::CancellationToken,
    ) {
        spawn_test_agent_loop_with_catalog(
            session_id,
            conv,
            state,
            test_catalog(model, context_window).await,
            tools,
        )
    }

    fn spawn_test_agent_loop_with_catalog(
        session_id: &str,
        conv: InMemoryConversationStore,
        state: InMemoryStateStore,
        catalog: Arc<ModelCatalog>,
        tools: Vec<Box<dyn Tool<InMemoryMessageSender>>>,
    ) -> (
        mpsc::UnboundedSender<AgentMessage>,
        mpsc::UnboundedReceiver<()>,
        ActiveThreads,
        tokio_util::sync::CancellationToken,
    ) {
        let (agent_tx, agent_rx) = mpsc::unbounded_channel();
        let (idle_tx, idle_rx) = mpsc::unbounded_channel();
        let (input_tx, mut input_adapter_rx) = mpsc::unbounded_channel::<(InputMessage, String)>();
        let agent_tx_clone = agent_tx.clone();
        tokio::task::spawn_local(async move {
            while let Some((msg, id)) = input_adapter_rx.recv().await {
                if agent_tx_clone
                    .send(AgentMessage::Input(Box::new(msg), id))
                    .is_err()
                {
                    break;
                }
            }
        });
        let sender = InMemoryMessageSender::new(input_tx);
        let subscriber_map: SubscriberMap = Arc::new(std::sync::Mutex::new(HashMap::new()));
        let active_threads = Arc::new(std::sync::Mutex::new(HashSet::new()));
        let shutdown = tokio_util::sync::CancellationToken::new();

        tokio::task::spawn_local(agent_loop(
            session_id.into(),
            agent_rx,
            catalog,
            conv,
            state,
            sender,
            String::new(),
            Arc::new(tools),
            Arc::new(None),
            None,
            subscriber_map,
            active_threads.clone(),
            idle_tx,
            shutdown.clone(),
        ));

        (agent_tx, idle_rx, active_threads, shutdown)
    }

    /// Regression test for issue #32: spawned threads should inherit the parent's
    /// model, not the global default.
    #[tokio::test(flavor = "current_thread")]
    async fn spawned_thread_inherits_parent_model() {
        use infinity_agent_core::tools::thread::SpawnThreadTool;
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let dir = tempfile::tempdir().expect("create temp dir");
                let default_model = infinity_protocol::ModelRef {
                    provider_id: "provider1".to_owned(),
                    model_id: "model1".to_owned(),
                };
                let non_default_model = infinity_protocol::ModelRef {
                    provider_id: "provider2".to_owned(),
                    model_id: "model2".to_owned(),
                };
                let conv = InMemoryConversationStore::new_with_dir(
                    dir.path().join("threads"),
                    default_model, // default is provider1/model1 (via catalog)
                    Arc::new(crate::ids::UuidIdSource),
                );
                let state = InMemoryStateStore::new(dir.path().join("state"));

                let (model1, mut ctrl1) = mock_model();
                let (model2, mut ctrl2) = mock_model();
                let catalog = two_model_catalog(model1, model2).await;

                conv.ensure_root_thread("root")
                    .await
                    .expect("ensure root thread");
                // Set root thread to use the non-default model (provider2/model2).
                conv.set_thread_model("root", non_default_model.clone());

                let tools: Vec<Box<dyn Tool<InMemoryMessageSender>>> =
                    vec![Box::new(SpawnThreadTool {
                        conversation_store: conv.clone(),
                    })];

                let (tx, _idle_rx, _, _shutdown) =
                    spawn_test_agent_loop_with_catalog("root", conv.clone(), state, catalog, tools);

                // Send user input to root thread (which uses model2).
                tx.send(user_text_input("root", "spawn a child"))
                    .expect("send user input");

                // Root thread uses model2, so ctrl2 gets the request.
                let _req = ctrl2.next_request().await;
                ctrl2.send_tool_call(
                    "tc-spawn",
                    "spawn_thread",
                    serde_json::json!({
                        "instructions": "do something",
                        "child_of": ["root"]
                    }),
                );
                ctrl2.finish();

                // After spawn_thread, the parent gets the tool result which
                // triggers another model call on ctrl2. Handle it.
                let parent_followup = ctrl2.next_request().await;
                let is_parent = parent_followup.chat_history.iter().any(|m| {
                    if let rig::message::Message::User { content } = m {
                        if let UserContent::ToolResult(r) = content.first() {
                            if let rig::message::ToolResultContent::Text(t) = r.content.first() {
                                return t.text.contains("successfully spawned");
                            }
                        }
                    }
                    false
                });
                assert!(is_parent, "expected parent follow-up request on ctrl2");
                ctrl2.send_text("ok");
                ctrl2.finish();

                // The child thread worker should also use model2 (inherited from parent).
                // Due to the bug (issue #32), the child uses the default model (provider1),
                // so ctrl1 would get the request instead of ctrl2.
                let child_req =
                    tokio::time::timeout(std::time::Duration::from_secs(5), ctrl2.next_request())
                        .await
                        .expect(
                            "child thread should use model2 (parent's model), not model1 (default)",
                        );

                // Verify the child received the spawn instructions.
                let has_instructions = child_req.chat_history.iter().any(|m| {
                    if let rig::message::Message::User { content } = m {
                        if let UserContent::ToolResult(r) = content.first() {
                            if let rig::message::ToolResultContent::Text(t) = r.content.first() {
                                return t.text.contains("do something");
                            }
                        }
                    }
                    false
                });
                assert!(
                    has_instructions,
                    "child thread should have received spawn instructions"
                );

                // Verify ctrl1 (default model) did NOT get any requests.
                assert!(
                    ctrl1.try_next_request().is_none(),
                    "default model (provider1/model1) should not have received any requests"
                );

                ctrl2.send_text("child done");
                ctrl2.finish();
            })
            .await;
    }

    /// Reproduces corruption when compaction triggers mid-tool-call:
    /// 1. User sends input, model responds with async tool call + high token usage
    /// 2. Compaction triggers, spawns child thread
    /// 3. Child thread summarizes and calls close_thread (saves summary, sends CompactionComplete)
    /// 4. Tool result arrives, apply_compaction truncates history
    /// 5. History has [summary, tool_result] with no matching tool_call
    #[tokio::test(flavor = "current_thread")]
    async fn compaction_during_tool_call_corrupts_history() {
        use infinity_agent_core::tools::thread::CloseThreadTool;
        use rig::completion::Usage;
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (conv, state, _dir) = tmp_stores();
                let (model, mut ctrl) = mock_model();
                conv.ensure_root_thread("t1")
                    .await
                    .expect("ensure root thread");

                use async_trait::async_trait;
                struct AsyncTool;
                #[async_trait]
                impl Tool<InMemoryMessageSender> for AsyncTool {
                    fn name(&self) -> &str {
                        "async_tool"
                    }
                    fn description(&self) -> &str {
                        "a"
                    }
                    fn parameters(&self) -> serde_json::Value {
                        serde_json::json!({"type":"object","properties":{}})
                    }
                    async fn execute(
                        &self,
                        _: serde_json::Value,
                        _: String,
                        _: Option<String>,
                        _: &infinity_agent_core::tools::ToolContext<InMemoryMessageSender>,
                    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
                        Ok(())
                    }
                }

                let tools: Vec<Box<dyn Tool<InMemoryMessageSender>>> =
                    vec![
                        Box::new(AsyncTool),
                        Box::new(CloseThreadTool::<_, rap_client::http::SimpleHttpClient> {
                            conversation_store: conv.clone(),
                            rap_notifier: None,
                        }),
                    ];

                // context_window = 100, so 76 input tokens triggers compaction
                let (tx, _idle_rx, _, _shutdown) =
                    spawn_test_agent_loop_with_tools("t1", conv.clone(), state, model, tools, 100)
                        .await;

                // 1. User sends input, model responds with async tool call
                tx.send(user_text_input("t1", "do something"))
                    .expect("send user input");
                let _req = ctrl.next_request().await;
                ctrl.send_tool_call("tc-1", "async_tool", serde_json::json!({}));
                ctrl.finish_with_usage(Some(Usage {
                    input_tokens: 76,
                    output_tokens: 10,
                    total_tokens: 86,
                    cached_input_tokens: 0,
                }));

                // 2. Compaction triggers. Send the tool result BEFORE the compaction
                //    child finishes (simulating a fast tool execution).
                tx.send(AgentMessage::Input(
                    Box::new(InputMessage {
                        content: InputMessageContent::User(UserContent::ToolResult(
                            rig::message::ToolResult {
                                id: "tc-1".into(),
                                call_id: None,
                                content: rig::OneOrMany::one(
                                    rig::message::ToolResultContent::Text(rig::agent::Text {
                                        text: "tool execution result".into(),
                                    }),
                                ),
                            },
                        )),
                        group_id: "t1".into(),
                        metadata: None,
                        synthetic: None,
                        display_as: None,
                        subscription: false,
                    }),
                    uuid::Uuid::new_v4().to_string(),
                ))
                .expect("send tool result");

                // The parent processes the tool result and calls the model.
                // But first, the compaction child also gets a model request.
                // We need to handle both — order depends on scheduling.
                // Handle whichever comes first.
                let req2 = ctrl.next_request().await;

                // Determine if this is the compaction child or the parent's tool result
                let is_compaction_child = req2.chat_history.iter().any(|m| {
                    if let rig::message::Message::User { content } = m
                        && let UserContent::ToolResult(r) = content.first()
                        && let rig::message::ToolResultContent::Text(t) = r.content.first()
                    {
                        t.text.contains("compaction thread")
                    } else {
                        false
                    }
                });

                let find_child_thread_id =
                    |req: &rig::completion::CompletionRequest| -> String {
                        req.chat_history
                            .iter()
                            .find_map(|m| {
                                if let rig::message::Message::User { content } = m
                                    && let UserContent::ToolResult(r) = content.first()
                                    && let rig::message::ToolResultContent::Text(t) =
                                        r.content.first()
                                    && t.text.contains("close_thread with your thread ID")
                                {
                                    let start = t.text.find('(')? + 1;
                                    let end = t.text.find(')')?;
                                    Some(t.text[start..end].to_owned())
                                } else {
                                    None
                                }
                            })
                            .expect("should find child thread ID")
                    };

                let handle_compaction_child =
                    |ctrl: &mut rig_mock::MockModelController,
                     req: &rig::completion::CompletionRequest| {
                        let child_thread_id = find_child_thread_id(req);
                        ctrl.send_tool_call(
                            "tc-close",
                            "close_thread",
                            serde_json::json!({
                                "thread_id": child_thread_id,
                                "report_to_parent": "Summary of conversation so far"
                            }),
                        );
                        ctrl.finish();
                    };

                if is_compaction_child {
                    handle_compaction_child(&mut ctrl, &req2);
                    let _req3 = ctrl.next_request().await;
                    ctrl.send_text("processed tool result");
                    ctrl.finish();
                } else {
                    ctrl.send_text("processed tool result");
                    ctrl.finish();
                    let compaction_req = ctrl.next_request().await;
                    handle_compaction_child(&mut ctrl, &compaction_req);
                }

                // 3. After CompactionComplete is applied, send a user message to
                //    trigger a model call so we can inspect the history.

                tx.send(user_text_input("t1", "what happened?"))
                    .expect("send follow-up");

                // 4. Inspect the history for corruption.
                let req_final = tokio::time::timeout(
                    std::time::Duration::from_secs(5),
                    ctrl.next_request(),
                )
                .await
                .expect("timed out waiting for final model request");

                let history: Vec<_> = req_final.chat_history.into_iter().collect();

                // BUG: After apply_compaction, history has tool_result("tc-1")
                // but no matching tool_call (it was compacted away).
                let has_orphaned_tool_result = history.iter().enumerate().any(|(i, m)| {
                    if let rig::message::Message::User { content } = m
                        && let UserContent::ToolResult(r) = content.first()
                        && let rig::message::ToolResultContent::Text(t) = r.content.first()
                        && t.text.contains("tool execution result")
                    {
                        !history[..i].iter().any(|prev| {
                            if let rig::message::Message::Assistant { content, .. } = prev {
                                content.iter().any(|c| {
                                    matches!(c, rig::message::AssistantContent::ToolCall(tc) if tc.id == "tc-1")
                                })
                            } else {
                                false
                            }
                        })
                    } else {
                        false
                    }
                });

                assert!(
                    !has_orphaned_tool_result,
                    "History is corrupted: tool result has no matching tool_call after compaction. History: {:#?}",
                    history
                );
            })
            .await;
    }

    /// Regression test for issue #64: after compaction is applied, the tracked
    /// context usage must be reset so auto-compaction does not immediately
    /// re-trigger on the stale pre-compaction token count.
    ///
    /// The worker must stay alive across the compaction round trip for the
    /// stale count to survive (a respawned worker starts from zero), so the
    /// model responds with an async tool call: the unanswered call keeps the
    /// worker from idling out while the compaction child runs.
    #[tokio::test(flavor = "current_thread")]
    async fn compaction_does_not_retrigger_after_applied() {
        use infinity_agent_core::tools::thread::CloseThreadTool;
        use rig::completion::Usage;
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (conv, state, _dir) = tmp_stores();
                let (model, mut ctrl) = mock_model();
                conv.ensure_root_thread("t1")
                    .await
                    .expect("ensure root thread");

                use async_trait::async_trait;
                struct AsyncTool;
                #[async_trait]
                impl Tool<InMemoryMessageSender> for AsyncTool {
                    fn name(&self) -> &str {
                        "async_tool"
                    }
                    fn description(&self) -> &str {
                        "a"
                    }
                    fn parameters(&self) -> serde_json::Value {
                        serde_json::json!({"type":"object","properties":{}})
                    }
                    async fn execute(
                        &self,
                        _: serde_json::Value,
                        _: String,
                        _: Option<String>,
                        _: &infinity_agent_core::tools::ToolContext<InMemoryMessageSender>,
                    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
                        Ok(())
                    }
                }

                let tools: Vec<Box<dyn Tool<InMemoryMessageSender>>> = vec![
                    Box::new(AsyncTool),
                    Box::new(CloseThreadTool::<_, rap_client::http::SimpleHttpClient> {
                        conversation_store: conv.clone(),
                        rap_notifier: None,
                    }),
                ];

                // context_window = 100, so 76 input tokens triggers compaction
                let (tx, mut idle_rx, _, _shutdown) =
                    spawn_test_agent_loop_with_tools("t1", conv.clone(), state, model, tools, 100)
                        .await;

                // 1. User input → async tool call + high usage → compaction triggers.
                tx.send(user_text_input("t1", "do something"))
                    .expect("send user input");
                let _req = ctrl.next_request().await;
                ctrl.send_tool_call("tc-1", "async_tool", serde_json::json!({}));
                ctrl.finish_with_usage(Some(Usage {
                    input_tokens: 76,
                    output_tokens: 10,
                    total_tokens: 86,
                    cached_input_tokens: 0,
                }));

                let is_compaction_req = |req: &rig::completion::CompletionRequest| -> bool {
                    req.chat_history.iter().any(|m| {
                        if let rig::message::Message::User { content } = m
                            && let UserContent::ToolResult(r) = content.first()
                            && let rig::message::ToolResultContent::Text(t) = r.content.first()
                        {
                            t.text.contains("compaction thread")
                        } else {
                            false
                        }
                    })
                };

                // 2. The compaction child asks for a summary; close it. This
                //    sends CompactionComplete to the parent worker, which is
                //    still alive waiting on the tc-1 tool result.
                let compaction_req = ctrl.next_request().await;
                assert!(is_compaction_req(&compaction_req));
                let child_thread_id = compaction_req
                    .chat_history
                    .iter()
                    .find_map(|m| {
                        if let rig::message::Message::User { content } = m
                            && let UserContent::ToolResult(r) = content.first()
                            && let rig::message::ToolResultContent::Text(t) = r.content.first()
                            && t.text.contains("close_thread with your thread ID")
                        {
                            let start = t.text.find('(')? + 1;
                            let end = t.text.find(')')?;
                            Some(t.text[start..end].to_owned())
                        } else {
                            None
                        }
                    })
                    .expect("should find child thread ID");
                ctrl.send_tool_call(
                    "tc-close",
                    "close_thread",
                    serde_json::json!({
                        "thread_id": child_thread_id,
                        "report_to_parent": "Summary of conversation so far"
                    }),
                );
                ctrl.finish();

                // 3. The tool result arrives. The parent applies the compaction
                //    (CompactionComplete) and then processes the tool result.
                //    With the bug, the stale 86-token count immediately
                //    re-triggers compaction, spawning a second compaction child
                //    whose model request would show up here.
                tx.send(AgentMessage::Input(
                    Box::new(InputMessage {
                        content: InputMessageContent::User(UserContent::ToolResult(
                            rig::message::ToolResult {
                                id: "tc-1".into(),
                                call_id: None,
                                content: rig::OneOrMany::one(
                                    rig::message::ToolResultContent::Text(rig::agent::Text {
                                        text: "tool execution result".into(),
                                    }),
                                ),
                            },
                        )),
                        group_id: "t1".into(),
                        metadata: None,
                        synthetic: None,
                        display_as: None,
                        subscription: false,
                    }),
                    uuid::Uuid::new_v4().to_string(),
                ))
                .expect("send tool result");

                let req = ctrl.next_request().await;
                assert!(
                    !is_compaction_req(&req),
                    "compaction must not re-trigger after being applied"
                );
                // Finish without usage so the tracked context usage stays at
                // its post-compaction value.
                ctrl.send_text("processed tool result");
                ctrl.finish();

                // 4. With the fix, all workers wind down. With the bug, a
                //    second compaction child sits waiting on the mock model
                //    forever and the session never idles.
                tokio::time::timeout(std::time::Duration::from_secs(5), idle_rx.recv())
                    .await
                    .expect("session should idle after compaction (no re-trigger)");

                // 5. Yield so the display forwarder drains its queue and any
                //    erroneously-spawned second compaction child would have
                //    issued its model request.
                for _ in 0..100 {
                    tokio::task::yield_now().await;
                }
                assert!(
                    ctrl.try_next_request().is_none(),
                    "no further model requests should be made after compaction is applied"
                );

                // The persisted context usage should have been reset when the
                // compaction was applied (the final response reported no usage,
                // so it must not have overwritten the reset).
                assert_eq!(
                    conv.get_total_tokens_used("t1"),
                    0,
                    "persisted context usage should be reset after compaction"
                );
            })
            .await;
    }

    /// Same as above but triggers compaction TWICE on the same worker without
    /// it reloading from the store. Exposes the mismatch between in-memory
    /// indices and absolute store orders after a prior compaction.
    #[tokio::test(flavor = "current_thread")]
    async fn second_compaction_during_tool_call_after_prior_compaction() {
        use infinity_agent_core::tools::thread::CloseThreadTool;
        use rig::completion::Usage;
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (conv, state, _dir) = tmp_stores();
                let (model, mut ctrl) = mock_model();
                conv.ensure_root_thread("t1")
                    .await
                    .expect("ensure root thread");

                use async_trait::async_trait;
                struct AsyncTool;
                #[async_trait]
                impl Tool<InMemoryMessageSender> for AsyncTool {
                    fn name(&self) -> &str {
                        "async_tool"
                    }
                    fn description(&self) -> &str {
                        "a"
                    }
                    fn parameters(&self) -> serde_json::Value {
                        serde_json::json!({"type":"object","properties":{}})
                    }
                    async fn execute(
                        &self,
                        _: serde_json::Value,
                        _: String,
                        _: Option<String>,
                        _: &infinity_agent_core::tools::ToolContext<InMemoryMessageSender>,
                    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
                        Ok(())
                    }
                }

                let tools: Vec<Box<dyn Tool<InMemoryMessageSender>>> = vec![
                    Box::new(AsyncTool),
                    Box::new(CloseThreadTool::<_, rap_client::http::SimpleHttpClient> {
                        conversation_store: conv.clone(),
                        rap_notifier: None,
                    }),
                ];

                // context_window = 100
                let (tx, _idle_rx, _, _shutdown) =
                    spawn_test_agent_loop_with_tools("t1", conv.clone(), state, model, tools, 100)
                        .await;

                let find_child_thread_id =
                    |req: &rig::completion::CompletionRequest| -> String {
                        req.chat_history
                            .iter()
                            .find_map(|m| {
                                if let rig::message::Message::User { content } = m
                                    && let UserContent::ToolResult(r) = content.first()
                                    && let rig::message::ToolResultContent::Text(t) =
                                        r.content.first()
                                    && t.text.contains("close_thread with your thread ID")
                                {
                                    let start = t.text.find('(')? + 1;
                                    let end = t.text.find(')')?;
                                    Some(t.text[start..end].to_owned())
                                } else {
                                    None
                                }
                            })
                            .expect("should find child thread ID")
                    };

                let is_compaction_req = |req: &rig::completion::CompletionRequest| -> bool {
                    req.chat_history.iter().any(|m| {
                        if let rig::message::Message::User { content } = m
                            && let UserContent::ToolResult(r) = content.first()
                            && let rig::message::ToolResultContent::Text(t) = r.content.first()
                        {
                            t.text.contains("compaction thread")
                        } else {
                            false
                        }
                    })
                };

                let handle_compaction_child =
                    |ctrl: &mut rig_mock::MockModelController,
                     req: &rig::completion::CompletionRequest,
                     summary: &str| {
                        let child_thread_id = find_child_thread_id(req);
                        ctrl.send_tool_call(
                            "tc-close",
                            "close_thread",
                            serde_json::json!({
                                "thread_id": child_thread_id,
                                "report_to_parent": summary
                            }),
                        );
                        ctrl.finish();
                    };

                let high_usage = Some(Usage {
                    input_tokens: 76,
                    output_tokens: 10,
                    total_tokens: 86,
                    cached_input_tokens: 0,
                });

                // ── FIRST ROUND: text response + compaction (no tool call) ──
                // This creates a compaction with up_to_order = 2 (user + assistant),
                // so the offset will be non-zero for the second round.

                tx.send(user_text_input("t1", "first message"))
                    .expect("send");
                let _req1 = ctrl.next_request().await;
                ctrl.send_text("first response");
                ctrl.finish_with_usage(high_usage);

                // Compaction child spawns (no pending tool call, safe_point = history.len() = 2)
                let compaction_req1 = ctrl.next_request().await;
                assert!(is_compaction_req(&compaction_req1));
                handle_compaction_child(&mut ctrl, &compaction_req1, "Summary of first round");

                // Worker idles after first compaction. Send more messages to build history.
                tx.send(user_text_input("t1", "second message"))
                    .expect("send");
                let _req2 = ctrl.next_request().await;
                ctrl.send_text("second response");
                ctrl.finish();

                tx.send(user_text_input("t1", "third message"))
                    .expect("send");
                let _req3 = ctrl.next_request().await;
                ctrl.send_text("third response");
                ctrl.finish();

                // ── SECOND ROUND: tool call + compaction after prior compaction ──
                // After reload, compacted_up_to = Some(2), in-memory history:
                //   [summary, second_msg, second_resp, third_msg, third_resp, fourth_msg, tc-2]
                // safe_spawn_point without offset = 6 (in-memory index)
                // safe_spawn_point with offset = 6 + (2-1) = 7 (absolute store order)
                // Store has 8 messages (0..8). Cutoff 6 misses "fourth message",
                // cutoff 7 includes it.

                tx.send(user_text_input("t1", "fourth message"))
                    .expect("send");
                let _req4 = ctrl.next_request().await;
                ctrl.send_tool_call("tc-2", "async_tool", serde_json::json!({}));
                ctrl.finish_with_usage(high_usage);

                // Send tool result before compaction child finishes
                tx.send(AgentMessage::Input(
                    Box::new(InputMessage {
                        content: InputMessageContent::User(UserContent::ToolResult(
                            rig::message::ToolResult {
                                id: "tc-2".into(),
                                call_id: None,
                                content: rig::OneOrMany::one(
                                    rig::message::ToolResultContent::Text(rig::agent::Text {
                                        text: "second tool result".into(),
                                    }),
                                ),
                            },
                        )),
                        group_id: "t1".into(),
                        metadata: None,
                        synthetic: None,
                        display_as: None,
                        subscription: false,
                    }),
                    uuid::Uuid::new_v4().to_string(),
                ))
                .expect("send tool result 2");

                // Handle both requests for second round
                let req_c = ctrl.next_request().await;
                let compaction_child_req = if is_compaction_req(&req_c) {
                    let r = req_c;
                    handle_compaction_child(&mut ctrl, &r, "Summary of second round");
                    let _req_d = ctrl.next_request().await;
                    ctrl.send_text("processed second tool");
                    ctrl.finish();
                    r
                } else {
                    ctrl.send_text("processed second tool");
                    ctrl.finish();
                    let req_d = ctrl.next_request().await;
                    handle_compaction_child(&mut ctrl, &req_d, "Summary of second round");
                    req_d
                };

                // Snapshot the compaction child's inherited history to verify:
                // - It includes "fourth message" (the last msg before the tool call)
                // - It does NOT include tc-2 (excluded by safe point)
                insta::assert_json_snapshot!(
                    "second_compaction_child_history",
                    compaction_child_req.chat_history,
                    {
                        "[].content[].id" => "[id]",
                        "[].content[].content[].text" => insta::dynamic_redaction(|value, _| {
                            let s = value.as_str().unwrap_or("");
                            if s.contains("compaction thread") {
                                insta::internals::Content::String("[compaction_instructions]".into())
                            } else {
                                value
                            }
                        })
                    }
                );
            })
            .await;
    }

    /// Regression test for issue #31: compaction spawned inside a child thread
    /// uses safe_spawn_point() which includes ancestor messages in the index,
    /// causing a panic ("range end index X out of range for slice of length Y")
    /// when the grandchild tries to load history.
    #[tokio::test(flavor = "current_thread")]
    async fn compaction_inside_child_thread_does_not_panic() {
        use infinity_agent_core::tools::thread::{CloseThreadTool, SpawnThreadTool};
        use rig::completion::Usage;
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (conv, state, _dir) = tmp_stores();
                let (model, mut ctrl) = mock_model();
                conv.ensure_root_thread("root")
                    .await
                    .expect("ensure root thread");

                use async_trait::async_trait;
                struct AsyncTool;
                #[async_trait]
                impl Tool<InMemoryMessageSender> for AsyncTool {
                    fn name(&self) -> &str {
                        "async_tool"
                    }
                    fn description(&self) -> &str {
                        "a"
                    }
                    fn parameters(&self) -> serde_json::Value {
                        serde_json::json!({"type":"object","properties":{}})
                    }
                    async fn execute(
                        &self,
                        _: serde_json::Value,
                        _: String,
                        _: Option<String>,
                        _: &infinity_agent_core::tools::ToolContext<InMemoryMessageSender>,
                    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
                        Ok(())
                    }
                }

                let tools: Vec<Box<dyn Tool<InMemoryMessageSender>>> = vec![
                    Box::new(AsyncTool),
                    Box::new(SpawnThreadTool {
                        conversation_store: conv.clone(),
                    }),
                    Box::new(CloseThreadTool::<_, rap_client::http::SimpleHttpClient> {
                        conversation_store: conv.clone(),
                        rap_notifier: None,
                    }),
                ];

                // context_window = 100, so 76 input tokens triggers compaction
                let (tx, _idle_rx, _, _shutdown) = spawn_test_agent_loop_with_tools(
                    "root",
                    conv.clone(),
                    state,
                    model,
                    tools,
                    100,
                )
                .await;

                // ── Build root history ──
                tx.send(user_text_input("root", "root message one"))
                    .expect("send");
                let _req = ctrl.next_request().await;
                ctrl.send_text("root response one");
                ctrl.finish();

                tx.send(user_text_input("root", "root message two"))
                    .expect("send");
                let _req = ctrl.next_request().await;
                ctrl.send_text("root response two");
                ctrl.finish();

                // ── Spawn a child thread from root ──
                tx.send(user_text_input("root", "spawn a child"))
                    .expect("send");
                let _req = ctrl.next_request().await;
                ctrl.send_tool_call(
                    "tc-spawn",
                    "spawn_thread",
                    serde_json::json!({
                        "instructions": "do child work",
                        "child_of": ["root"]
                    }),
                );
                ctrl.finish();

                // Parent gets tool result after spawn — extract child thread ID
                let parent_followup = ctrl.next_request().await;
                let child_thread_id = parent_followup
                    .chat_history
                    .iter()
                    .find_map(|m| {
                        if let rig::message::Message::User { content } = m
                            && let UserContent::ToolResult(r) = content.first()
                            && let rig::message::ToolResultContent::Text(t) = r.content.first()
                            && t.text.contains("successfully spawned")
                        {
                            let after = t.text.strip_prefix(
                                "Child thread is successfully spawned and has ID: ",
                            )?;
                            Some(after.split('.').next()?.to_owned())
                        } else {
                            None
                        }
                    })
                    .expect("should find child thread ID in spawn result");
                ctrl.send_text("ok, child spawned");
                ctrl.finish();

                // ── Child thread gets its first model call ──
                let _child_req = ctrl.next_request().await;
                ctrl.send_text("child first response");
                ctrl.finish();

                // ── Send another round to child ──
                tx.send(user_text_input(&child_thread_id, "child follow-up"))
                    .expect("send");
                let _req = ctrl.next_request().await;
                // Child responds with a TOOL CALL + high usage → triggers compaction.
                // The tool call is after safe_spawn_point, so it survives compaction.
                let high_usage = Some(Usage {
                    input_tokens: 76,
                    output_tokens: 10,
                    total_tokens: 86,
                    cached_input_tokens: 0,
                });
                ctrl.send_tool_call("tc-child", "async_tool", serde_json::json!({}));
                ctrl.finish_with_usage(high_usage);

                // Send tool result before compaction completes
                tx.send(AgentMessage::Input(
                    Box::new(InputMessage {
                        content: InputMessageContent::User(UserContent::ToolResult(
                            rig::message::ToolResult {
                                id: "tc-child".into(),
                                call_id: None,
                                content: rig::OneOrMany::one(
                                    rig::message::ToolResultContent::Text(rig::agent::Text {
                                        text: "async tool result".into(),
                                    }),
                                ),
                            },
                        )),
                        group_id: child_thread_id.clone(),
                        metadata: None,
                        synthetic: None,
                        display_as: None,
                        subscription: false,
                    }),
                    uuid::Uuid::new_v4().to_string(),
                ))
                .expect("send tool result");

                // Two model requests arrive: compaction grandchild + child processing
                // the tool result. Handle in whichever order they come.
                let req_a = ctrl.next_request().await;
                let is_compaction_req = |req: &rig::completion::CompletionRequest| -> bool {
                    req.chat_history.iter().any(|m| {
                        if let rig::message::Message::User { content } = m
                            && let UserContent::ToolResult(r) = content.first()
                            && let rig::message::ToolResultContent::Text(t) = r.content.first()
                        {
                            t.text.contains("compaction thread")
                        } else {
                            false
                        }
                    })
                };

                let find_grandchild_id =
                    |req: &rig::completion::CompletionRequest| -> String {
                        req.chat_history
                            .iter()
                            .find_map(|m| {
                                if let rig::message::Message::User { content } = m
                                    && let UserContent::ToolResult(r) = content.first()
                                    && let rig::message::ToolResultContent::Text(t) =
                                        r.content.first()
                                    && t.text.contains("close_thread with your thread ID")
                                {
                                    let start = t.text.find('(')? + 1;
                                    let end = t.text.find(')')?;
                                    Some(t.text[start..end].to_owned())
                                } else {
                                    None
                                }
                            })
                            .expect("should find grandchild thread ID")
                    };

                let handle_compaction =
                    |ctrl: &mut rig_mock::MockModelController,
                     req: &rig::completion::CompletionRequest| {
                        let id = find_grandchild_id(req);
                        ctrl.send_tool_call(
                            "tc-close",
                            "close_thread",
                            serde_json::json!({
                                "thread_id": id,
                                "report_to_parent": "Summary of child work"
                            }),
                        );
                        ctrl.finish();
                    };

                let compaction_req;
                if is_compaction_req(&req_a) {
                    compaction_req = req_a;
                    handle_compaction(&mut ctrl, &compaction_req);
                    // Now handle the child's tool result processing
                    let _req_b = ctrl.next_request().await;
                    ctrl.send_text("processed tool result");
                    ctrl.finish();
                } else {
                    // Child's tool result came first
                    ctrl.send_text("processed tool result");
                    ctrl.finish();
                    compaction_req = ctrl.next_request().await;
                    handle_compaction(&mut ctrl, &compaction_req);
                }

                // Snapshot the history sent to the compaction thread's model call.
                insta::assert_json_snapshot!(
                    "issue31_compaction_child_history",
                    compaction_req.chat_history,
                    {
                        "[].content[].id" => "[id]",
                        "[].content[].content[].text" => insta::dynamic_redaction(|value, _| {
                            let s = value.as_str().unwrap_or("");
                            if s.contains("compaction thread") {
                                insta::internals::Content::String("[compaction_instructions]".into())
                            } else if s.contains("INSIDE the thread") {
                                insta::internals::Content::String("[spawn_instructions]".into())
                            } else {
                                value
                            }
                        })
                    }
                );

                // ── After compaction applies, send a message to inspect the history ──
                tx.send(user_text_input(
                    &child_thread_id,
                    "message after compaction",
                ))
                .expect("send post-compaction message");

                let post_compaction_req = tokio::time::timeout(
                    std::time::Duration::from_secs(5),
                    ctrl.next_request(),
                )
                .await
                .expect("child should respond after compaction");

                // Post-compaction history should have: [summary, tool_call (survived
                // because it was after safe_spawn_point), tool_result, model response,
                // new user message]
                insta::assert_json_snapshot!(
                    "issue31_post_compaction_history",
                    post_compaction_req.chat_history,
                    {
                        "[].content[].id" => "[id]",
                    }
                );
            })
            .await;
    }
}
