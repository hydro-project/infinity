use std::collections::HashMap;
use std::sync::Arc;

use infinity_agent_core::batch_processor::DisplayEvent;
use infinity_agent_core::message::InputMessage;
use infinity_agent_core::tools::Tool;
use infinity_protocol::DaemonMessage;
use rig::completion::{CompletionModel, GetTokenUsage};
use tokio::sync::mpsc;

use super::display::display_event_to_daemon;
use super::thread_worker::thread_worker;
use super::{ActiveWorkers, ClientTxHandle, SessionStoreHandle};
use crate::memory_store::{InMemoryConversationStore, InMemoryMessageSender, InMemoryStateStore};
use crate::rap_tools;
use crate::session_store;

#[expect(clippy::too_many_arguments)]
pub async fn agent_loop<Mdl>(
    session_id: String,
    session_store: SessionStoreHandle,
    mut rx: mpsc::UnboundedReceiver<(InputMessage, String)>,
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
    client_tx_handle: ClientTxHandle,
    active_workers: ActiveWorkers,
    idle_tx: mpsc::UnboundedSender<()>,
    context_window: usize,
) where
    Mdl: CompletionModel + Send + Sync + 'static,
{
    let (display_tx, mut display_rx) =
        mpsc::unbounded_channel::<DisplayEvent<Mdl::StreamingResponse>>();

    let bridge_handle = client_tx_handle.clone();
    let bridge_session_id = session_id.clone();
    let bridge_session_store = session_store.clone();
    tokio::task::spawn_local(async move {
        while let Some(evt) = display_rx.recv().await {
            if let DisplayEvent::ResponseDone(ref prefix, ref r) = evt
                && prefix.is_none()
                && let Some(r) = r
            {
                let tokens = r.token_usage().map_or(0, |u| u.total_tokens as usize);
                let mut store = bridge_session_store.lock().await;
                store.update(&bridge_session_id, tokens, None);
                let _ = store.save();
            }

            if let DisplayEvent::UserChoiceRequired {
                ref id,
                ref prompt,
                ref choices,
                ref default,
                ref response_url,
            } = evt
            {
                let dm = DaemonMessage::UserChoiceRequired {
                    id: id.clone(),
                    prompt: prompt.clone(),
                    choices: choices.clone(),
                    default: *default,
                };
                let mut store = bridge_session_store.lock().await;
                if let Some(entry) = store.sessions.get_mut(&bridge_session_id) {
                    entry.pending_choices.push(session_store::PendingChoice {
                        id: id.clone(),
                        message: dm,
                        response_url: response_url.clone(),
                    });
                    store.notify(&session_id);
                }
            }

            if let Some(dm) = display_event_to_daemon(evt) {
                let guard = bridge_handle.lock().unwrap();
                if let Some(ref tx) = *guard {
                    let _ = tx.send(dm);
                }
            }
        }
    });

    let mut thread_txs: HashMap<String, mpsc::UnboundedSender<(InputMessage, String)>> =
        HashMap::new();

    while let Some((input_msg, message_id)) = rx.recv().await {
        let group_id = input_msg.group_id.clone();

        if let Some(tx) = thread_txs.get(&group_id) {
            if tx.send((input_msg.clone(), message_id.clone())).is_ok() {
                continue;
            }
            thread_txs.remove(&group_id);
        }

        let (tx, worker_rx) = mpsc::unbounded_channel();
        let aw = active_workers.clone();
        let gid = group_id.clone();
        let itx = idle_tx.clone();
        tokio::task::spawn_local(thread_worker(
            gid,
            worker_rx,
            display_tx.clone(),
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
            aw,
            itx,
            context_window,
        ));
        let _ = tx.send((input_msg, message_id));
        thread_txs.insert(group_id, tx);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use infinity_agent_core::message::InputMessageContent;
    use infinity_agent_core::traits::ConversationStore;
    use rig::message::UserContent;
    use rig_mock::mock_model;
    use std::collections::HashSet;

    fn tmp_stores() -> (
        InMemoryConversationStore,
        InMemoryStateStore,
        tempfile::TempDir,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let conv = InMemoryConversationStore::new_with_dir(dir.path().join("threads"));
        let state = InMemoryStateStore::new(dir.path().join("state"));
        (conv, state, dir)
    }

    fn user_text_input(group_id: &str, text: &str) -> (InputMessage, String) {
        (
            InputMessage {
                content: InputMessageContent::User(UserContent::text(text)),
                group_id: group_id.into(),
                metadata: None,
                synthetic: None,
                display_as: None,
                subscription: false,
            },
            uuid::Uuid::new_v4().to_string(),
        )
    }

    fn spawn_test_agent_loop(
        session_id: &str,
        conv: InMemoryConversationStore,
        state: InMemoryStateStore,
        model: rig_mock::MockCompletionModel,
    ) -> (
        mpsc::UnboundedSender<(InputMessage, String)>,
        mpsc::UnboundedReceiver<()>,
        ActiveWorkers,
    ) {
        let (input_tx, input_rx) = mpsc::unbounded_channel();
        let (idle_tx, idle_rx) = mpsc::unbounded_channel();
        let active_workers: ActiveWorkers = Arc::new(std::sync::Mutex::new(HashSet::new()));
        let sender = InMemoryMessageSender::new(input_tx.clone());
        let client_tx_handle: ClientTxHandle = Arc::new(std::sync::Mutex::new(None));

        let (change_tx, _) = mpsc::unbounded_channel();
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let session_store = Arc::new(tokio::sync::Mutex::new(
            crate::session_store::SessionStore::load(&tmp.path().to_string_lossy(), change_tx),
        ));

        let aw = active_workers.clone();
        tokio::task::spawn_local(agent_loop(
            session_id.into(),
            session_store,
            input_rx,
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
            client_tx_handle,
            aw,
            idle_tx,
            0,
        ));

        (input_tx, idle_rx, active_workers)
    }

    #[tokio::test(flavor = "current_thread")]
    async fn routes_to_separate_thread_workers() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (conv, state, _dir) = tmp_stores();
                let (model, mut ctrl) = mock_model();
                conv.ensure_root_thread("root").await.unwrap();
                let child_id = conv.spawn_thread("root", "tc-spawn", false).await.unwrap();

                let (tx, mut idle_rx, _) = spawn_test_agent_loop("root", conv, state, model);

                tx.send(user_text_input("root", "hello root")).unwrap();
                let _req1 = ctrl.next_request().await;
                ctrl.send_text("root resp");
                ctrl.finish();

                tokio::time::timeout(std::time::Duration::from_secs(2), idle_rx.recv())
                    .await
                    .expect("root should idle");

                tx.send((
                    InputMessage {
                        content: InputMessageContent::User(UserContent::text("hello child")),
                        group_id: child_id.clone(),
                        metadata: None,
                        synthetic: None,
                        display_as: None,
                        subscription: false,
                    },
                    uuid::Uuid::new_v4().to_string(),
                ))
                .unwrap();

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
                conv.ensure_root_thread("t1").await.unwrap();

                let (tx, mut idle_rx, _) = spawn_test_agent_loop("t1", conv, state, model);

                tx.send(user_text_input("t1", "first")).unwrap();
                let _req1 = ctrl.next_request().await;
                ctrl.send_text("resp 1");
                ctrl.finish();

                tokio::time::timeout(std::time::Duration::from_secs(2), idle_rx.recv())
                    .await
                    .expect("should idle");

                tx.send(user_text_input("t1", "second")).unwrap();
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
