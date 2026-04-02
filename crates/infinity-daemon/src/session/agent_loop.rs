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

#[expect(clippy::too_many_arguments)]
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

        tokio::task::spawn_local(thread_worker(
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
#[allow(clippy::collapsible_if)]
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
            subscriber_map.clone(),
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
}
