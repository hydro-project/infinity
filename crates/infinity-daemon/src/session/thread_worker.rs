use std::sync::Arc;

use infinity_agent_core::batch_processor::DisplayEvent;
use infinity_agent_core::event_processor;
use infinity_agent_core::message::{InputMessage, InputMessageContent};
use infinity_agent_core::model_provider::ProviderStreamingResponse;
use infinity_agent_core::tools::{Tool, ToolContext};
use infinity_protocol::DaemonMessage;
use rig::message::UserContent;
use tokio::sync::{mpsc, oneshot};

use super::display::{display_event_to_daemon, history_message_to_daemon};
use crate::memory_store::{InMemoryConversationStore, InMemoryMessageSender, InMemoryStateStore};
use crate::models::ModelCatalog;
use crate::rap_tools;
use crate::session::ActiveThreads;
use crate::session_store;
use infinity_agent_core::traits::StateStore;

/// Shared subscriber list for a thread worker.
pub type ThreadSubscribers = Arc<std::sync::Mutex<Vec<mpsc::UnboundedSender<DaemonMessage>>>>;

/// Subscribe request: (client_tx, want_replay).
pub type SubscribeRequest = (mpsc::UnboundedSender<DaemonMessage>, bool);

pub fn is_user_text_input(msg: &InputMessage) -> bool {
    msg.synthetic.is_none()
        && matches!(
            &msg.content,
            InputMessageContent::User(UserContent::Text(_))
        )
}

const SLEEP_TOOL_NAMES: &[&str] = &["sleep", "sleep_until", "sleep_until_event_or_input"];

fn is_deferrable_synthetic_event(msg: &InputMessage) -> bool {
    msg.synthetic.as_ref().is_some_and(|s| {
        matches!(
            s,
            infinity_agent_core::message::SyntheticKind::Tagged(
                infinity_agent_core::message::TaggedSyntheticKind::SubscriptionEvent { .. }
                    | infinity_agent_core::message::TaggedSyntheticKind::ThreadReport { .. }
                    | infinity_agent_core::message::TaggedSyntheticKind::ParentMessage { .. }
            ) | infinity_agent_core::message::SyntheticKind::SubscriptionEvent(_)
        )
    })
}

#[expect(
    clippy::too_many_arguments,
    reason = "thread worker requires many dependencies"
)]
pub async fn thread_worker(
    active_group_id: String,
    mut rx: mpsc::UnboundedReceiver<(InputMessage, String)>,
    subscribe_rx: mpsc::UnboundedReceiver<SubscribeRequest>,
    active_threads: ActiveThreads,
    subscribers: ThreadSubscribers,
    root_session_id: String,
    catalog: Arc<ModelCatalog>,
    conversation_store: InMemoryConversationStore,
    state_store: InMemoryStateStore,
    sender: InMemoryMessageSender,
    callback_url: String,
    tool_impls: Arc<Vec<Box<dyn Tool<InMemoryMessageSender>>>>,
    extra_system_prompt: Option<String>,
    rap_notifier: Option<rap_client::notifier::RapNotifier<rap_tools::SimpleHttpClient>>,
    idle_tx: mpsc::UnboundedSender<()>,
) {
    let mut subscribe_rx = subscribe_rx;
    active_threads
        .lock()
        .expect("bug: mutex poisoned")
        .insert(active_group_id.clone());
    let _guard = WorkerGuard {
        active_group_id: active_group_id.clone(),
        active_threads,
        idle_tx,
    };

    // Create a local display channel; a forwarding task converts events and broadcasts to subscribers.
    let (display_tx, mut display_fwd_rx) =
        mpsc::unbounded_channel::<DisplayEvent<ProviderStreamingResponse>>();
    let fwd_group_id = active_group_id.clone();
    let fwd_subscribers = subscribers.clone();
    let fwd_conversation_store = conversation_store.clone();
    let fwd_root_session_id = root_session_id.clone();
    tokio::task::spawn_local(rap_protocol::log_panic(
        "display_event_forwarder",
        async move {
            while let Some(evt) = display_fwd_rx.recv().await {
                // Update token usage for root thread responses.
                if let DisplayEvent::ResponseDone(ref r) = evt
                    && let Some(r) = r
                {
                    use rig::completion::GetTokenUsage;
                    // Only persist usage the provider actually reported; a
                    // response without usage metadata must not reset the
                    // stored total to zero.
                    if let Some(usage) = r.token_usage() {
                        fwd_conversation_store
                            .set_total_tokens_used(&fwd_group_id, usage.total_tokens as usize);
                    }
                    fwd_conversation_store
                        .set_last_updated(&fwd_group_id, &chrono::Utc::now().to_rfc3339());
                }

                // Store pending choices.
                if let DisplayEvent::UserChoiceRequired {
                    ref id,
                    ref prompt,
                    ref choices,
                    ref default,
                    ref response_url,
                } = evt
                {
                    let dm = DaemonMessage::UserChoiceRequired {
                        thread_id: Some(fwd_group_id.clone()),
                        id: id.clone(),
                        prompt: prompt.clone(),
                        choices: choices.clone(),
                        default: *default,
                    };
                    fwd_conversation_store.add_pending_choice(
                        &fwd_root_session_id,
                        session_store::PendingChoice {
                            id: id.clone(),
                            message: dm,
                            response_url: response_url.clone(),
                        },
                    );
                }

                // Remove pending choices when completed/cancelled.
                if let DisplayEvent::UserChoiceComplete { ref choice_id } = evt {
                    fwd_conversation_store.remove_pending_choice(&fwd_root_session_id, choice_id);
                }

                if let Some(dm) = display_event_to_daemon(&fwd_group_id, evt) {
                    let mut subs = fwd_subscribers.lock().expect("bug: mutex poisoned");
                    subs.retain(|tx| tx.send(dm.clone()).is_ok());
                }
            }
        },
    ));

    let current_history = match event_processor::HistoryManager::new_with_history(
        conversation_store.clone(),
        state_store.clone(),
        active_group_id.clone(),
    )
    .await
    {
        Ok(h) => h,
        Err(e) => {
            let _ = display_tx.send(DisplayEvent::Info(format!("Error: {e}")));
            return;
        }
    };

    // Resolve this thread's model. Every thread stores its own selection (no
    // parent fallback); selections that are no longer available fall back to
    // the global default.
    let selected = conversation_store.get_thread_model(&active_group_id);
    let (model_ref, model_entry, fell_back) = catalog.resolve(&selected);
    if fell_back {
        let _ = display_tx.send(DisplayEvent::Info(format!(
            "Warning: model {}/{} is no longer available; using default {}/{}",
            selected.provider_id, selected.model_id, model_ref.provider_id, model_ref.model_id
        )));
    }
    let provider = catalog
        .provider(&model_ref.provider_id)
        .expect("bug: resolved model's provider missing from catalog")
        .clone();
    let context_window = model_entry.context_window;

    let tool_names: std::collections::HashSet<String> =
        tool_impls.iter().map(|t| t.name().to_owned()).collect();
    let tool_defs: Vec<rig::completion::ToolDefinition> = tool_impls
        .iter()
        .map(|t| rig::completion::ToolDefinition {
            name: t.name().to_owned(),
            description: t.description().to_owned(),
            parameters: t.parameters(),
        })
        .collect();

    let tool_context = ToolContext {
        message_sender: sender.clone(),
        group_id: active_group_id.clone(),
        input_queue_arn: String::new(),
        callback_url,
        user_id: None,
        thread_stack: current_history.get_thread_stack(),
    };
    let tool_registry: std::collections::HashMap<String, &dyn Tool<InMemoryMessageSender>> =
        tool_impls
            .iter()
            .map(|t| (t.name().to_owned(), t.as_ref()))
            .collect();

    let total_tokens_cell = std::cell::Cell::new(0u64);
    let mut compaction_triggered = false;
    let mut pending_non_interrupt_items = vec![];
    let mut completion_fut = None;
    let mut completion_cancel_tx: Option<oneshot::Sender<()>> = None;

    let handle_subscribe = async |tx: mpsc::UnboundedSender<DaemonMessage>, want_replay: bool| {
        if want_replay {
            let history: Vec<DaemonMessage> = {
                let hist = current_history.history.borrow();
                hist.iter()
                    .filter_map(|m| history_message_to_daemon(m, &active_group_id, &hist))
                    .collect()
            };
            let choices = conversation_store.get_pending_choice_messages(&root_session_id);
            let views = conversation_store.get_views(&active_group_id);
            if !history.is_empty() || !choices.is_empty() || !views.is_empty() {
                let _ = tx.send(DaemonMessage::Replay {
                    history,
                    pending_choices: choices,
                    views,
                });
            }
        }
        subscribers.lock().expect("bug: mutex poisoned").push(tx);
    };

    loop {
        let inputs_before_pending = if let Some(mut_fut) = completion_fut.as_mut() {
            tokio::select! {
                _ = mut_fut => {
                    #[expect(clippy::let_underscore_future, reason = "dropping completed future")]
                    let _ = completion_fut.take().expect("bug: completion_fut missing after poll");

                    // Background compaction: trigger if total tokens > 75% of context window
                    let total_tokens = total_tokens_cell.get() as usize;
                    if !compaction_triggered && context_window > 0 && total_tokens > context_window * 3 / 4 {
                        compaction_triggered = true;
                        tracing::info!(
                            "Auto-compaction for thread {}: {} total tokens > 75% of {} context window",
                            &active_group_id, total_tokens, context_window
                        );
                        let _ = display_tx.send(DisplayEvent::Info(
                            "✦ Auto-compaction triggered (context > 75%)".to_owned(),
                        ));
                        pending_non_interrupt_items.push((InputMessage {
                            content: InputMessageContent::User(UserContent::text("")),
                            group_id: active_group_id.clone(),
                            metadata: None,
                            synthetic: Some(infinity_agent_core::message::SyntheticKind::Tagged(
                                infinity_agent_core::message::TaggedSyntheticKind::Compaction,
                            )),
                            display_as: None,
                            subscription: false,
                        }, uuid::Uuid::new_v4().to_string()));
                    }

                    continue;
                },
                first = rx.recv() => {
                    let Some(first) = first else {
                        // Input channel closed — the session is shutting down.
                        // Interrupt the in-flight completion and wait for it to
                        // wind down so pending history items (e.g. a tool result
                        // that was being processed) are synced to the store. The
                        // cancellation path strips trailing reasoning before the
                        // sync, so no partial thinking is persisted.
                        let _ = completion_cancel_tx.take().expect("bug: cancel_tx missing during shutdown").send(());
                        completion_fut.take().expect("bug: completion_fut missing during shutdown").await;
                        return;
                    };
                    let mut batch = vec![first];
                    while let Ok(item) = rx.try_recv() {
                        batch.push(item);
                    }

                    if batch.iter().any(|(msg, _)| is_user_text_input(msg))
                    {
                        let _ = completion_cancel_tx.take().expect("bug: cancel_tx missing during interrupt").send(());
                        let completion_fut_taken = completion_fut.take().expect("bug: completion_fut missing during interrupt");
                        completion_fut_taken.await;

                        let (mut user_inputs, non_user_inputs): (Vec<_>, Vec<_>) = batch
                            .into_iter()
                            .partition(|(msg, _)| is_user_text_input(msg));

                        if let InputMessageContent::User(UserContent::Text(text)) = &mut user_inputs[0].0.content {
                            text.text = format!("<interrupt>{}", text.text);
                        } else {
                            panic!("user_inputs should only have user text");
                        }

                        pending_non_interrupt_items.extend(non_user_inputs);
                        user_inputs
                    } else {
                        pending_non_interrupt_items.extend(batch);
                        continue;
                    }
                },
                req = subscribe_rx.recv() => {
                    if let Some((tx, want_replay)) = req {
                        handle_subscribe(tx, want_replay).await;
                    }
                    continue;
                }
            }
        } else {
            let mut batch = vec![];

            // Check if we're waiting for a non-sleep async tool result.
            let waiting_for_non_sleep_tool = {
                current_history.history.borrow().last().is_some_and(|msg| {
                    if let infinity_agent_core::message::InfinityMessage::ToolCall {
                        call, ..
                    } = msg
                    {
                        !SLEEP_TOOL_NAMES.contains(&call.function.name.as_str())
                    } else {
                        false
                    }
                })
            };

            // Treat pending items as empty if they're all deferred synthetic events.
            let has_actionable_pending = !pending_non_interrupt_items.is_empty()
                && (!waiting_for_non_sleep_tool
                    || pending_non_interrupt_items
                        .iter()
                        .any(|(msg, _)| !is_deferrable_synthetic_event(msg)));

            if !has_actionable_pending {
                let first_res = rx.try_recv();
                let mut first = if let Ok(first_res) = first_res {
                    Some(first_res)
                } else {
                    let last_is_tool_call = {
                        current_history.history.borrow().last().is_some_and(|msg| {
                            if let infinity_agent_core::message::InfinityMessage::ToolCall {
                                call,
                                ..
                            } = msg
                            {
                                call.function.name != "close_thread"
                            } else {
                                false
                            }
                        })
                    };
                    let has_subs = state_store
                        .get_active_subscriptions(&active_group_id)
                        .await
                        .map(|s| !s.is_empty())
                        .unwrap_or(false);

                    while let Ok((tx, want_replay)) = subscribe_rx.try_recv() {
                        // handle replays before idling
                        handle_subscribe(tx, want_replay).await;
                    }

                    if !last_is_tool_call && !has_subs {
                        tracing::info!("Thread {} going to idle", &active_group_id);
                        return;
                    } else {
                        None
                    }
                };

                if first.is_none() {
                    loop {
                        tokio::select! {
                            msg = rx.recv() => {
                                first = msg;
                                break;
                            }
                            req = subscribe_rx.recv() => {
                                if let Some((tx, want_replay)) = req {
                                    handle_subscribe(tx, want_replay).await;
                                }
                            }
                        }
                    }
                }
                let Some(first) = first else {
                    return;
                };
                batch.push(first);
            }

            while let Ok(item) = rx.try_recv() {
                batch.push(item);
            }

            // Defer synthetic events arriving from rx when waiting for a
            // non-sleep async tool result.
            if waiting_for_non_sleep_tool && !batch.is_empty() {
                let (sub_events, rest): (Vec<_>, Vec<_>) = batch
                    .into_iter()
                    .partition(|(msg, _)| is_deferrable_synthetic_event(msg));
                if !sub_events.is_empty() {
                    pending_non_interrupt_items.extend(sub_events);
                }
                batch = rest;
                if batch.is_empty() {
                    continue;
                }
            }

            batch
        };

        let all_inputs: Vec<_> = inputs_before_pending
            .into_iter()
            .chain(pending_non_interrupt_items.drain(..))
            .collect();

        for (m, _) in &all_inputs {
            if m.synthetic.as_ref().is_some_and(|s| {
                matches!(
                    s,
                    infinity_agent_core::message::SyntheticKind::Tagged(
                        infinity_agent_core::message::TaggedSyntheticKind::CompactionComplete
                    )
                )
            }) {
                compaction_triggered = false;
            }
        }

        let result = infinity_agent_core::batch_processor::process_batch(
            all_inputs.into_iter(),
            &current_history,
            &conversation_store,
            &display_tx,
            &active_group_id,
            provider.as_ref(),
            &model_ref.model_id,
            &tool_names,
            &tool_defs,
            &tool_registry,
            tool_context.clone(),
            &extra_system_prompt,
            rap_notifier.as_ref(),
            Some(&total_tokens_cell),
        )
        .await;

        if let Some((fut, ct)) = result {
            completion_cancel_tx = Some(ct);
            completion_fut = Some(fut);
        }
    }
}

struct WorkerGuard {
    active_group_id: String,
    active_threads: ActiveThreads,
    idle_tx: mpsc::UnboundedSender<()>,
}

impl Drop for WorkerGuard {
    fn drop(&mut self) {
        let mut threads = self.active_threads.lock().expect("bug: mutex poisoned");
        threads.remove(&self.active_group_id);
        if threads.is_empty() {
            let _ = self.idle_tx.send(());
        }
    }
}

#[cfg(test)]
#[expect(clippy::collapsible_if, reason = "test readability")]
mod tests {
    use super::*;
    use infinity_agent_core::model_provider::{ModelEntry, SingleModelProvider};
    use infinity_agent_core::traits::{ConversationStore, InputSender};
    use rig_mock::mock_model;

    fn test_model_ref() -> infinity_protocol::ModelRef {
        infinity_protocol::ModelRef {
            provider_id: "mock".to_owned(),
            model_id: "mock".to_owned(),
        }
    }

    async fn test_catalog(model: rig_mock::MockCompletionModel) -> Arc<ModelCatalog> {
        let entry = ModelEntry {
            model_id: "mock".to_owned(),
            display_name: "mock".to_owned(),
            context_window: 0,
            max_output_tokens: None,
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

    fn tmp_stores() -> (
        InMemoryConversationStore,
        InMemoryStateStore,
        tempfile::TempDir,
    ) {
        let dir = tempfile::tempdir().expect("create temp dir");
        let conv =
            InMemoryConversationStore::new_with_dir(dir.path().join("threads"), test_model_ref());
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

    fn tool_result_input(group_id: &str, id: &str, text: &str) -> (InputMessage, String) {
        (
            InputMessage {
                content: InputMessageContent::User(UserContent::ToolResult(
                    rig::message::ToolResult {
                        id: id.into(),
                        call_id: None,
                        content: rig::OneOrMany::one(rig::message::ToolResultContent::Text(
                            rig::agent::Text { text: text.into() },
                        )),
                    },
                )),
                group_id: group_id.into(),
                metadata: None,
                synthetic: None,
                display_as: None,
                subscription: false,
            },
            uuid::Uuid::new_v4().to_string(),
        )
    }

    async fn spawn_worker(
        group_id: &str,
        conv: InMemoryConversationStore,
        state: InMemoryStateStore,
        model: rig_mock::MockCompletionModel,
        tools: Vec<Box<dyn Tool<InMemoryMessageSender>>>,
    ) -> (
        mpsc::UnboundedSender<(InputMessage, String)>,
        mpsc::UnboundedReceiver<DaemonMessage>,
        mpsc::UnboundedReceiver<()>,
        ActiveThreads,
    ) {
        let (input_tx, input_rx) = mpsc::unbounded_channel();
        let (client_tx, client_rx) = mpsc::unbounded_channel();
        let (idle_tx, idle_rx) = mpsc::unbounded_channel();
        let (_subscribe_tx, subscribe_rx) = mpsc::unbounded_channel();
        let active_threads: ActiveThreads =
            Arc::new(std::sync::Mutex::new(std::collections::HashSet::new()));
        let sender = InMemoryMessageSender::new(input_tx.clone());
        let subscribers: ThreadSubscribers = Arc::new(std::sync::Mutex::new(vec![client_tx]));

        // Thread metadata (including the selected model) must exist before a
        // worker starts.
        conv.ensure_root_thread(group_id)
            .await
            .expect("ensure root thread");

        tokio::task::spawn_local(thread_worker(
            group_id.into(),
            input_rx,
            subscribe_rx,
            active_threads.clone(),
            subscribers,
            group_id.into(),
            test_catalog(model).await,
            conv,
            state,
            sender,
            String::new(),
            Arc::new(tools),
            None,
            None,
            idle_tx,
        ));
        (input_tx, client_rx, idle_rx, active_threads)
    }

    async fn collect_until_done(rx: &mut mpsc::UnboundedReceiver<DaemonMessage>) -> Vec<String> {
        let mut texts = Vec::new();
        loop {
            match tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv()).await {
                Ok(Some(DaemonMessage::TextChunk { chunk, .. })) => texts.push(chunk),
                Ok(Some(DaemonMessage::ResponseDone { .. })) => break,
                Ok(Some(_)) => {}
                Ok(None) => break,
                Err(_) => panic!("timed out waiting for ResponseDone"),
            }
        }
        texts
    }

    #[tokio::test(flavor = "current_thread")]
    async fn worker_idles_after_text_response() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (conv, state, _dir) = tmp_stores();
                let (model, mut ctrl) = mock_model();
                let (tx, mut display_rx, mut idle_rx, workers) =
                    spawn_worker("t1", conv, state, model, vec![]).await;
                tx.send(user_text_input("t1", "hello"))
                    .expect("send user input");
                let _req = ctrl.next_request().await;
                ctrl.send_text("hi there");
                ctrl.finish();
                collect_until_done(&mut display_rx).await;
                tokio::time::timeout(std::time::Duration::from_secs(2), idle_rx.recv())
                    .await
                    .expect("should idle");
                assert!(
                    workers
                        .lock()
                        .expect("bug: workers mutex poisoned")
                        .is_empty()
                );
            })
            .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn worker_stays_alive_waiting_for_tool_result() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (conv, state, _dir) = tmp_stores();
                let (model, mut ctrl) = mock_model();
                use async_trait::async_trait;
                struct DummyTool;
                #[async_trait]
                impl Tool<InMemoryMessageSender> for DummyTool {
                    fn name(&self) -> &str {
                        "dummy"
                    }
                    fn description(&self) -> &str {
                        "d"
                    }
                    fn parameters(&self) -> serde_json::Value {
                        serde_json::json!({"type":"object","properties":{}})
                    }
                    async fn execute(
                        &self,
                        _: serde_json::Value,
                        _: String,
                        _: Option<String>,
                        _: &ToolContext<InMemoryMessageSender>,
                    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
                        Ok(())
                    }
                }
                let (tx, mut display_rx, mut idle_rx, workers) =
                    spawn_worker("t1", conv, state, model, vec![Box::new(DummyTool)]).await;
                tx.send(user_text_input("t1", "use tool"))
                    .expect("send user input");
                let _req = ctrl.next_request().await;
                ctrl.send_tool_call("tc-1", "dummy", serde_json::json!({}));
                ctrl.finish();
                loop {
                    match tokio::time::timeout(std::time::Duration::from_secs(2), display_rx.recv())
                        .await
                    {
                        Ok(Some(DaemonMessage::ResponseDone { .. })) => break,
                        Ok(Some(_)) => {}
                        _ => panic!("timed out"),
                    }
                }
                assert!(
                    idle_rx.try_recv().is_err(),
                    "should not be idle while tool call pending"
                );
                assert!(
                    workers
                        .lock()
                        .expect("bug: workers mutex poisoned")
                        .contains("t1")
                );
                tx.send(tool_result_input("t1", "tc-1", "tool done"))
                    .expect("send tool result");
                let _req2 = ctrl.next_request().await;
                ctrl.send_text("ok");
                ctrl.finish();
                collect_until_done(&mut display_rx).await;
                tokio::time::timeout(std::time::Duration::from_secs(2), idle_rx.recv())
                    .await
                    .expect("should idle");
            })
            .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn user_text_interrupts_active_completion() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (conv, state, _dir) = tmp_stores();
                let (model, mut ctrl) = mock_model();
                let (tx, mut display_rx, _, _) =
                    spawn_worker("t1", conv, state, model, vec![]).await;
                tx.send(user_text_input("t1", "first"))
                    .expect("send first user input");
                let _req = ctrl.next_request().await;
                ctrl.send_text("partial...");
                loop {
                    match tokio::time::timeout(std::time::Duration::from_secs(2), display_rx.recv())
                        .await
                    {
                        Ok(Some(DaemonMessage::TextChunk { .. })) => break,
                        Ok(Some(_)) => {}
                        _ => panic!("timed out"),
                    }
                }
                ctrl.send_text(" more");
                loop {
                    match tokio::time::timeout(std::time::Duration::from_secs(2), display_rx.recv())
                        .await
                    {
                        Ok(Some(DaemonMessage::TextChunk { .. })) => break,
                        Ok(Some(_)) => {}
                        _ => panic!("timed out"),
                    }
                }
                tx.send(user_text_input("t1", "stop that"))
                    .expect("send interrupt input");
                let req2 = ctrl.next_request().await;
                let has_interrupt = req2.chat_history.into_iter().any(|m| {
                    if let rig::message::Message::User { content } = &m {
                        if let UserContent::Text(t) = content.first() {
                            return t.text.contains("<interrupt>");
                        }
                    }
                    false
                });
                assert!(has_interrupt, "should have <interrupt> prefix");
                ctrl.send_text("ok stopped");
                ctrl.finish();
                collect_until_done(&mut display_rx).await;
            })
            .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn non_user_text_during_completion_does_not_interrupt() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (conv, state, _dir) = tmp_stores();
                let (model, mut ctrl) = mock_model();
                use async_trait::async_trait;
                struct DummyTool;
                #[async_trait]
                impl Tool<InMemoryMessageSender> for DummyTool {
                    fn name(&self) -> &str {
                        "dummy"
                    }
                    fn description(&self) -> &str {
                        "d"
                    }
                    fn parameters(&self) -> serde_json::Value {
                        serde_json::json!({"type":"object","properties":{}})
                    }
                    async fn execute(
                        &self,
                        _: serde_json::Value,
                        _: String,
                        _: Option<String>,
                        _: &ToolContext<InMemoryMessageSender>,
                    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
                        Ok(())
                    }
                }
                let (tx, mut display_rx, _, _) =
                    spawn_worker("t1", conv, state, model, vec![Box::new(DummyTool)]).await;
                tx.send(user_text_input("t1", "do stuff"))
                    .expect("send user input");
                let _req = ctrl.next_request().await;
                ctrl.send_tool_call("tc-1", "dummy", serde_json::json!({}));
                ctrl.finish();
                loop {
                    match tokio::time::timeout(std::time::Duration::from_secs(2), display_rx.recv())
                        .await
                    {
                        Ok(Some(DaemonMessage::ResponseDone { .. })) => break,
                        Ok(Some(_)) => {}
                        _ => panic!("timed out"),
                    }
                }
                tx.send(tool_result_input("t1", "tc-1", "tool output"))
                    .expect("send tool result");
                let _req2 = ctrl.next_request().await;
                ctrl.send_text("processing...");
                loop {
                    match tokio::time::timeout(std::time::Duration::from_secs(2), display_rx.recv())
                        .await
                    {
                        Ok(Some(DaemonMessage::TextChunk { .. })) => break,
                        Ok(Some(_)) => {}
                        _ => panic!("timed out"),
                    }
                }
                tx.send(tool_result_input("t1", "tc-other", "stale event"))
                    .expect("send stale tool result");
                ctrl.send_text(" done");
                ctrl.finish();
                let texts = collect_until_done(&mut display_rx).await;
                assert!(
                    texts.join("").contains("done"),
                    "should not have been interrupted"
                );
            })
            .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn subscription_event_queued_during_completion_processed_after() {
        use infinity_agent_core::message::{SyntheticKind, TaggedSyntheticKind};
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (conv, state, _dir) = tmp_stores();
                let (model, mut ctrl) = mock_model();
                use async_trait::async_trait;
                struct SubTool;
                #[async_trait]
                impl Tool<InMemoryMessageSender> for SubTool {
                    fn name(&self) -> &str {
                        "subscribe_tool"
                    }
                    fn description(&self) -> &str {
                        "s"
                    }
                    fn parameters(&self) -> serde_json::Value {
                        serde_json::json!({"type":"object","properties":{}})
                    }
                    async fn execute(
                        &self,
                        _: serde_json::Value,
                        id: String,
                        call_id: Option<String>,
                        ctx: &ToolContext<InMemoryMessageSender>,
                    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
                        let msg = InputMessage {
                            content: InputMessageContent::User(UserContent::ToolResult(
                                rig::message::ToolResult {
                                    id: id.clone(),
                                    call_id,
                                    content: rig::OneOrMany::one(
                                        rig::message::ToolResultContent::Text(rig::agent::Text {
                                            text: "subscribed".into(),
                                        }),
                                    ),
                                },
                            )),
                            group_id: ctx.group_id.clone(),
                            metadata: None,
                            synthetic: None,
                            display_as: None,
                            subscription: true,
                        };
                        ctx.message_sender
                            .send_to_input_queue(msg, &ctx.group_id, &id)
                            .await?;
                        Ok(())
                    }
                }
                let (tx, mut display_rx, _, _) =
                    spawn_worker("t1", conv, state, model, vec![Box::new(SubTool)]).await;
                tx.send(user_text_input("t1", "subscribe"))
                    .expect("send user input");
                let _req = ctrl.next_request().await;
                ctrl.send_tool_call("tc-sub", "subscribe_tool", serde_json::json!({}));
                ctrl.finish();
                loop {
                    match tokio::time::timeout(std::time::Duration::from_secs(2), display_rx.recv())
                        .await
                    {
                        Ok(Some(DaemonMessage::ResponseDone { .. })) => break,
                        Ok(Some(_)) => {}
                        _ => panic!("timed out"),
                    }
                }
                let _req2 = ctrl.next_request().await;
                ctrl.send_text("got subscription...");
                loop {
                    match tokio::time::timeout(std::time::Duration::from_secs(2), display_rx.recv())
                        .await
                    {
                        Ok(Some(DaemonMessage::TextChunk { .. })) => break,
                        Ok(Some(_)) => {}
                        _ => panic!("timed out"),
                    }
                }
                tx.send((
                    InputMessage {
                        content: InputMessageContent::User(UserContent::ToolResult(
                            rig::message::ToolResult {
                                id: "tc-sub".into(),
                                call_id: None,
                                content: rig::OneOrMany::one(
                                    rig::message::ToolResultContent::Text(rig::agent::Text {
                                        text: "event payload xyz".into(),
                                    }),
                                ),
                            },
                        )),
                        group_id: "t1".into(),
                        metadata: None,
                        synthetic: Some(SyntheticKind::Tagged(
                            TaggedSyntheticKind::SubscriptionEvent {
                                tool_call_id: "tc-sub".into(),
                                associative: true,
                                r#final: false,
                            },
                        )),
                        display_as: None,
                        subscription: false,
                    },
                    uuid::Uuid::new_v4().to_string(),
                ))
                .expect("send subscription event");
                ctrl.send_text(" all good");
                ctrl.finish();
                let texts = collect_until_done(&mut display_rx).await;
                assert!(
                    texts.join("").contains("all good"),
                    "should not have been interrupted"
                );
                let req3 = ctrl.next_request().await;
                let has_event = req3.chat_history.into_iter().any(|m| {
                    if let rig::message::Message::User { content } = &m {
                        if let UserContent::ToolResult(r) = content.first() {
                            if let rig::message::ToolResultContent::Text(t) = r.content.first() {
                                return t.text.contains("event payload xyz");
                            }
                        }
                    }
                    false
                });
                assert!(has_event, "queued event should appear in next round");
                ctrl.send_text("processed");
                ctrl.finish();
                collect_until_done(&mut display_rx).await;
            })
            .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn worker_idles_after_close_thread_tool_call() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (conv, state, _dir) = tmp_stores();
                let (model, mut ctrl) = mock_model();
                use async_trait::async_trait;
                struct CloseThreadStub;
                #[async_trait]
                impl Tool<InMemoryMessageSender> for CloseThreadStub {
                    fn name(&self) -> &str {
                        "close_thread"
                    }
                    fn description(&self) -> &str {
                        "close"
                    }
                    fn parameters(&self) -> serde_json::Value {
                        serde_json::json!({"type":"object","properties":{}})
                    }
                    async fn execute(
                        &self,
                        _: serde_json::Value,
                        _: String,
                        _: Option<String>,
                        _: &ToolContext<InMemoryMessageSender>,
                    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
                        Ok(())
                    }
                }
                let (tx, mut display_rx, mut idle_rx, workers) =
                    spawn_worker("t1", conv, state, model, vec![Box::new(CloseThreadStub)]).await;
                tx.send(user_text_input("t1", "close"))
                    .expect("send user input");
                let _req = ctrl.next_request().await;
                ctrl.send_tool_call(
                    "tc-1",
                    "close_thread",
                    serde_json::json!({"thread_id": "t1"}),
                );
                ctrl.finish();
                loop {
                    match tokio::time::timeout(std::time::Duration::from_secs(2), display_rx.recv())
                        .await
                    {
                        Ok(Some(DaemonMessage::ResponseDone { .. })) => break,
                        Ok(Some(_)) => {}
                        _ => panic!("timed out"),
                    }
                }
                tokio::time::timeout(std::time::Duration::from_secs(2), idle_rx.recv())
                    .await
                    .expect("should idle after close_thread");
                assert!(
                    workers
                        .lock()
                        .expect("bug: workers mutex poisoned")
                        .is_empty()
                );
            })
            .await;
    }

    /// Subscription events arriving while waiting for a non-sleep async tool
    /// result are deferred until the tool result is processed.
    #[tokio::test(flavor = "current_thread")]
    async fn subscription_event_deferred_during_async_tool_wait() {
        use infinity_agent_core::message::{SyntheticKind, TaggedSyntheticKind};
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (conv, state, _dir) = tmp_stores();
                let (model, mut ctrl) = mock_model();
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
                        _: &ToolContext<InMemoryMessageSender>,
                    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
                        // Async tool — result delivered later via input queue.
                        Ok(())
                    }
                }
                let (tx, mut display_rx, _, _) =
                    spawn_worker("t1", conv, state, model, vec![Box::new(AsyncTool)]).await;

                // 1. User sends input, model calls async_tool.
                tx.send(user_text_input("t1", "do async"))
                    .expect("send user input");
                let _req = ctrl.next_request().await;
                ctrl.send_tool_call("tc-async", "async_tool", serde_json::json!({}));
                ctrl.finish();
                loop {
                    match tokio::time::timeout(std::time::Duration::from_secs(2), display_rx.recv())
                        .await
                    {
                        Ok(Some(DaemonMessage::ResponseDone { .. })) => break,
                        Ok(Some(_)) => {}
                        _ => panic!("timed out waiting for first ResponseDone"),
                    }
                }

                // 2. While waiting for tool result, a subscription event arrives.
                tx.send((
                    InputMessage {
                        content: InputMessageContent::User(UserContent::ToolResult(
                            rig::message::ToolResult {
                                id: "tc-async".into(),
                                call_id: None,
                                content: rig::OneOrMany::one(
                                    rig::message::ToolResultContent::Text(rig::agent::Text {
                                        text: "sub event data".into(),
                                    }),
                                ),
                            },
                        )),
                        group_id: "t1".into(),
                        metadata: None,
                        synthetic: Some(SyntheticKind::Tagged(
                            TaggedSyntheticKind::SubscriptionEvent {
                                tool_call_id: "tc-async".into(),
                                associative: true,
                                r#final: false,
                            },
                        )),
                        display_as: None,
                        subscription: false,
                    },
                    uuid::Uuid::new_v4().to_string(),
                ))
                .expect("send subscription event");

                // Yield so the worker can process the subscription event.
                tokio::task::yield_now().await;
                tokio::task::yield_now().await;

                // 3. The subscription event should NOT have triggered a
                //    completion request yet (it should be deferred).
                assert!(
                    ctrl.try_next_request().is_none(),
                    "subscription event should not trigger completion while waiting for async tool"
                );

                // 4. Now deliver the actual tool result.
                tx.send(tool_result_input("t1", "tc-async", "tool done"))
                    .expect("send tool result");

                // 5. The tool result triggers a completion. The deferred
                //    subscription event should be included in this batch.
                let req2 = ctrl.next_request().await;
                ctrl.send_text("all processed");
                ctrl.finish();
                collect_until_done(&mut display_rx).await;

                // req2 should contain both the tool result and the
                // deferred subscription event (transformed into a
                // receive_event__injected tool call by prepare_input).
                let has_tool_result = req2.chat_history.iter().any(|m| {
                    if let rig::message::Message::User { content } = m {
                        if let UserContent::ToolResult(r) = content.first() {
                            if let rig::message::ToolResultContent::Text(t) = r.content.first() {
                                return t.text.contains("tool done");
                            }
                        }
                    }
                    false
                });
                let has_injected_event = req2.chat_history.iter().any(|m| {
                    if let rig::message::Message::User { content } = m {
                        if let UserContent::ToolResult(r) = content.first() {
                            if let rig::message::ToolResultContent::Text(t) = r.content.first() {
                                return t.text.contains("sub event data");
                            }
                        }
                    }
                    false
                });
                assert!(has_tool_result, "tool result should be in completion");
                assert!(
                    has_injected_event,
                    "deferred subscription event should be in completion"
                );
            })
            .await;
    }

    /// Thread reports arriving while waiting for a non-sleep async tool
    /// result are deferred until the tool result is processed.
    #[tokio::test(flavor = "current_thread")]
    async fn thread_report_deferred_during_async_tool_wait() {
        use infinity_agent_core::message::{SyntheticKind, TaggedSyntheticKind};
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (conv, state, _dir) = tmp_stores();
                let (model, mut ctrl) = mock_model();
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
                        _: &ToolContext<InMemoryMessageSender>,
                    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
                        Ok(())
                    }
                }
                let (tx, mut display_rx, _, _) =
                    spawn_worker("t1", conv, state, model, vec![Box::new(AsyncTool)]).await;

                // 1. User sends input, model calls async_tool.
                tx.send(user_text_input("t1", "do async"))
                    .expect("send user input");
                let _req = ctrl.next_request().await;
                ctrl.send_tool_call("tc-async", "async_tool", serde_json::json!({}));
                ctrl.finish();
                loop {
                    match tokio::time::timeout(std::time::Duration::from_secs(2), display_rx.recv())
                        .await
                    {
                        Ok(Some(DaemonMessage::ResponseDone { .. })) => break,
                        Ok(Some(_)) => {}
                        _ => panic!("timed out waiting for first ResponseDone"),
                    }
                }

                // 2. While waiting for tool result, a thread report arrives.
                tx.send((
                    InputMessage {
                        content: InputMessageContent::User(UserContent::ToolResult(
                            rig::message::ToolResult {
                                id: String::new(),
                                call_id: None,
                                content: rig::OneOrMany::one(
                                    rig::message::ToolResultContent::Text(rig::agent::Text {
                                        text: "Report from child thread: progress update".into(),
                                    }),
                                ),
                            },
                        )),
                        group_id: "t1".into(),
                        metadata: None,
                        synthetic: Some(SyntheticKind::Tagged(TaggedSyntheticKind::ThreadReport {
                            tool_call_id: "tc-async".into(),
                            child_thread_id: "child-1".into(),
                        })),
                        display_as: None,
                        subscription: false,
                    },
                    uuid::Uuid::new_v4().to_string(),
                ))
                .expect("send thread report");

                // Yield so the worker can process the thread report.
                tokio::task::yield_now().await;
                tokio::task::yield_now().await;

                // 3. The thread report should NOT have triggered a
                //    completion request yet (it should be deferred).
                assert!(
                    ctrl.try_next_request().is_none(),
                    "thread report should not trigger completion while waiting for async tool"
                );

                // 4. Now deliver the actual tool result.
                tx.send(tool_result_input("t1", "tc-async", "tool done"))
                    .expect("send tool result");

                // 5. The tool result triggers a completion. The deferred
                //    thread report should be included in this batch.
                let req2 = ctrl.next_request().await;
                ctrl.send_text("all processed");
                ctrl.finish();
                collect_until_done(&mut display_rx).await;

                let has_tool_result = req2.chat_history.iter().any(|m| {
                    if let rig::message::Message::User { content } = m {
                        if let UserContent::ToolResult(r) = content.first() {
                            if let rig::message::ToolResultContent::Text(t) = r.content.first() {
                                return t.text.contains("tool done");
                            }
                        }
                    }
                    false
                });
                let has_thread_report = req2.chat_history.iter().any(|m| {
                    if let rig::message::Message::User { content } = m {
                        if let UserContent::ToolResult(r) = content.first() {
                            if let rig::message::ToolResultContent::Text(t) = r.content.first() {
                                return t.text.contains("progress update");
                            }
                        }
                    }
                    false
                });
                assert!(has_tool_result, "tool result should be in completion");
                assert!(
                    has_thread_report,
                    "deferred thread report should be in completion"
                );
            })
            .await;
    }
}
