use std::collections::HashMap;
use std::sync::Arc;

use infinity_agent_core::tools::Tool;
use rig::completion::CompletionModel;
use tokio::sync::mpsc;

use super::thread_worker::{SubscribeRequest, thread_worker};
use super::{AgentMessage, SubscriberMap};
use crate::memory_store::{InMemoryConversationStore, InMemoryMessageSender, InMemoryStateStore};
use crate::rap_tools;
use crate::session::ActiveThreads;

struct WorkerChannels {
    input_tx: mpsc::UnboundedSender<(infinity_agent_core::message::InputMessage, String)>,
    subscribe_tx: mpsc::UnboundedSender<SubscribeRequest>,
}

#[expect(
    clippy::too_many_arguments,
    reason = "agent loop requires many dependencies"
)]
pub async fn agent_loop<Mdl>(
    session_id: String,
    mut rx: mpsc::UnboundedReceiver<AgentMessage>,
    model: Arc<Mdl>,
    conversation_store: InMemoryConversationStore,
    state_store: InMemoryStateStore,
    sender: InMemoryMessageSender,
    callback_url: String,
    tool_impls: Arc<Vec<Box<dyn Tool<InMemoryMessageSender>>>>,
    extra_system_prompt: Arc<Option<String>>,
    rap_notifier: Option<rap_client::notifier::RapNotifier<rap_tools::SimpleHttpClient>>,
    additional_request_params: Arc<std::sync::RwLock<Option<serde_json::Value>>>,
    active_model_id: Arc<std::sync::RwLock<Option<String>>>,
    subscriber_map: SubscriberMap,
    active_threads: ActiveThreads,
    idle_tx: mpsc::UnboundedSender<()>,
    context_window: usize,
) where
    Mdl: CompletionModel + Send + Sync + 'static,
{
    let mut workers: HashMap<String, WorkerChannels> = HashMap::new();

    while let Some(msg) = rx.recv().await {
        let thread_id = match &msg {
            AgentMessage::Input(input, _) => input.group_id.clone(),
            AgentMessage::Subscribe { thread_id, .. } => thread_id.clone(),
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
                }
                continue;
            }
            workers.remove(&thread_id);
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

        let subscribers = subscriber_map
            .lock()
            .expect("bug: mutex poisoned")
            .entry(thread_id.clone())
            .or_insert_with(|| Arc::new(std::sync::Mutex::new(parent_subs)))
            .clone();

        tokio::task::spawn_local(rap_protocol::log_panic(
            "thread_worker",
            thread_worker(
                thread_id.clone(),
                input_rx,
                subscribe_rx,
                active_threads.clone(),
                subscribers,
                session_id.clone(),
                model.clone(),
                conversation_store.clone(),
                state_store.clone(),
                sender.clone(),
                callback_url.clone(),
                tool_impls.clone(),
                extra_system_prompt.as_ref().clone(),
                rap_notifier.clone(),
                additional_request_params.clone(),
                active_model_id.clone(),
                idle_tx.clone(),
                context_window,
            ),
        ));

        match msg {
            AgentMessage::Input(input, id) => {
                let _ = input_tx.send((*input, id));
            }
            AgentMessage::Subscribe { request, .. } => {
                let _ = subscribe_tx.send(request);
            }
        }
        workers.insert(
            thread_id,
            WorkerChannels {
                input_tx,
                subscribe_tx,
            },
        );
    }
}

#[cfg(test)]
#[expect(clippy::collapsible_if, reason = "readability")]
mod tests {
    use std::collections::HashSet;

    use super::*;
    use infinity_agent_core::message::{InputMessage, InputMessageContent};
    use infinity_agent_core::traits::ConversationStore;
    use rig::message::UserContent;
    use rig_mock::mock_model;

    fn tmp_stores() -> (
        InMemoryConversationStore,
        InMemoryStateStore,
        tempfile::TempDir,
    ) {
        let dir = tempfile::tempdir().expect("create temp dir");
        let conv = InMemoryConversationStore::new_with_dir(dir.path().join("threads"));
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

    fn spawn_test_agent_loop(
        session_id: &str,
        conv: InMemoryConversationStore,
        state: InMemoryStateStore,
        model: rig_mock::MockCompletionModel,
    ) -> (
        mpsc::UnboundedSender<AgentMessage>,
        mpsc::UnboundedReceiver<()>,
        ActiveThreads,
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

        tokio::task::spawn_local(agent_loop(
            session_id.into(),
            agent_rx,
            Arc::new(model),
            conv,
            state,
            sender,
            String::new(),
            Arc::new(vec![]),
            Arc::new(None),
            None,
            Arc::new(std::sync::RwLock::new(None)),
            Arc::new(std::sync::RwLock::new(None)),
            subscriber_map,
            active_threads.clone(),
            idle_tx,
            0,
        ));

        (agent_tx, idle_rx, active_threads)
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
                    .spawn_thread("root", "tc-spawn", false)
                    .await
                    .expect("spawn child thread");

                let (tx, mut idle_rx, _) = spawn_test_agent_loop("root", conv, state, model);

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

                let (tx, mut idle_rx, _) = spawn_test_agent_loop("t1", conv, state, model);

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

    fn spawn_test_agent_loop_with_tools(
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

        tokio::task::spawn_local(agent_loop(
            session_id.into(),
            agent_rx,
            Arc::new(model),
            conv,
            state,
            sender,
            String::new(),
            Arc::new(tools),
            Arc::new(None),
            None,
            Arc::new(std::sync::RwLock::new(None)),
            Arc::new(std::sync::RwLock::new(None)),
            subscriber_map,
            active_threads.clone(),
            idle_tx,
            context_window,
        ));

        (agent_tx, idle_rx, active_threads)
    }

    /// Reproduces corruption when compaction triggers mid-tool-call:
    /// 1. User sends input, model responds with async tool call + high token usage
    /// 2. Compaction triggers, spawns child thread
    /// 3. Child thread summarizes and calls close_thread (saves summary, sends CompactionComplete)
    /// 4. Tool result arrives, apply_compaction truncates history
    /// 5. History has [summary, tool_result] with no matching tool_call
    #[tokio::test(flavor = "current_thread")]
    #[ignore = "known bug: compaction truncates pending tool call from history"]
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
                let (tx, _idle_rx, _) =
                    spawn_test_agent_loop_with_tools("t1", conv.clone(), state, model, tools, 100);

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
}
