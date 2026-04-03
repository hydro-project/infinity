use std::sync::Arc;

use infinity_agent_core::batch_processor::DisplayEvent;
use infinity_agent_core::event_processor;
use infinity_agent_core::message::{InputMessage, InputMessageContent};
use infinity_agent_core::tools::{Tool, ToolContext};
use infinity_protocol::DaemonMessage;
use rig::completion::CompletionModel;
use rig::message::UserContent;
use tokio::sync::{mpsc, oneshot};

use super::display::{display_event_to_daemon, history_message_to_daemon};
use crate::memory_store::{InMemoryConversationStore, InMemoryMessageSender, InMemoryStateStore};
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

#[expect(clippy::too_many_arguments)]
pub async fn thread_worker<Mdl>(
    active_group_id: String,
    mut rx: mpsc::UnboundedReceiver<(InputMessage, String)>,
    subscribe_rx: mpsc::UnboundedReceiver<SubscribeRequest>,
    active_threads: ActiveThreads,
    subscribers: ThreadSubscribers,
    root_session_id: String,
    model: Arc<Mdl>,
    conversation_store: InMemoryConversationStore,
    state_store: InMemoryStateStore,
    sender: InMemoryMessageSender,
    callback_url: String,
    tool_impls: Arc<Vec<Box<dyn Tool<InMemoryMessageSender>>>>,
    extra_system_prompt: Option<String>,
    rap_notifier: Option<rap_client::notifier::RapNotifier<rap_tools::SimpleHttpClient>>,
    additional_request_params: Arc<std::sync::RwLock<Option<serde_json::Value>>>,
    active_model_id: Arc<std::sync::RwLock<Option<String>>>,
    idle_tx: mpsc::UnboundedSender<()>,
    context_window: usize,
) where
    Mdl: CompletionModel + Send + Sync + 'static,
{
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
        mpsc::unbounded_channel::<DisplayEvent<Mdl::StreamingResponse>>();
    let fwd_group_id = active_group_id.clone();
    let fwd_subscribers = subscribers.clone();
    let fwd_conversation_store = conversation_store.clone();
    let fwd_root_session_id = root_session_id.clone();
    tokio::task::spawn_local(async move {
        while let Some(evt) = display_fwd_rx.recv().await {
            // Update token usage for root thread responses.
            if let DisplayEvent::ResponseDone(ref r) = evt
                && let Some(r) = r
            {
                use rig::completion::GetTokenUsage;
                let tokens = r.token_usage().map_or(0, |u| u.total_tokens as usize);
                fwd_conversation_store.set_total_tokens_used(&fwd_group_id, tokens);
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

            if let Some(dm) = display_event_to_daemon(&fwd_group_id, evt) {
                let mut subs = fwd_subscribers.lock().expect("bug: mutex poisoned");
                subs.retain(|tx| tx.send(dm.clone()).is_ok());
            }
        }
    });

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

    let tool_names: std::collections::HashSet<String> =
        tool_impls.iter().map(|t| t.name().to_string()).collect();
    let tool_defs: Vec<rig::completion::ToolDefinition> = tool_impls
        .iter()
        .map(|t| rig::completion::ToolDefinition {
            name: t.name().to_string(),
            description: t.description().to_string(),
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
            .map(|t| (t.name().to_string(), t.as_ref()))
            .collect();

    let input_tokens_cell = std::cell::Cell::new(0u64);
    let mut compaction_triggered = false;
    let mut pending_non_interrupt_items = vec![];
    let mut completion_fut = None;
    let mut completion_cancel_tx: Option<oneshot::Sender<()>> = None;

    let handle_subscribe = async |tx: mpsc::UnboundedSender<DaemonMessage>, want_replay: bool| {
        if want_replay {
            let history: Vec<DaemonMessage> = {
                current_history
                    .history
                    .borrow()
                    .iter()
                    .filter_map(|m| {
                        history_message_to_daemon(m, &active_group_id, &conversation_store)
                    })
                    .collect()
            };
            let choices = conversation_store.get_pending_choice_messages(&root_session_id);
            if !history.is_empty() || !choices.is_empty() {
                let _ = tx.send(DaemonMessage::Replay {
                    history,
                    pending_choices: choices,
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

                    // Background compaction: trigger if input tokens > 75% of context window
                    let input_tokens = input_tokens_cell.get() as usize;
                    if !compaction_triggered && context_window > 0 && input_tokens > context_window * 3 / 4 {
                        compaction_triggered = true;
                        tracing::info!(
                            "Auto-compaction for thread {}: {} input tokens > 75% of {} context window",
                            &active_group_id, input_tokens, context_window
                        );
                        let _ = display_tx.send(DisplayEvent::Info(
                            "✦ Auto-compaction triggered (context > 75%)".to_string(),
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
                    let Some(first) = first else { return };
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

            if pending_non_interrupt_items.is_empty() {
                let first_res = rx.try_recv();
                let mut first = if let Ok(first_res) = first_res {
                    Some(first_res)
                } else {
                    let last_is_tool_call = {
                        current_history.history.borrow().last().is_some_and(|msg| matches!(
                            msg,
                            rig::message::Message::Assistant { content, .. }
                                if matches!(content.first(), rig::message::AssistantContent::ToolCall(c) if c.function.name != "close_thread")
                        ))
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

            batch
        };

        let all_inputs: Vec<_> = inputs_before_pending
            .into_iter()
            .chain(pending_non_interrupt_items.drain(..))
            .collect();

        for (m, _) in &all_inputs {
            if let (Some(da), InputMessageContent::User(UserContent::ToolResult(res))) =
                (&m.display_as, &m.content)
            {
                conversation_store.save_display_as(&active_group_id, &res.id, da);
            }
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

        let params = additional_request_params
            .read()
            .expect("bug: rwlock poisoned")
            .clone();
        let mid = active_model_id
            .read()
            .expect("bug: rwlock poisoned")
            .clone();

        let result = infinity_agent_core::batch_processor::process_batch(
            all_inputs.into_iter(),
            &current_history,
            &conversation_store,
            &display_tx,
            &active_group_id,
            model.as_ref(),
            &tool_names,
            &tool_defs,
            &tool_registry,
            tool_context.clone(),
            &extra_system_prompt,
            params,
            mid,
            rap_notifier.as_ref(),
            Some(&input_tokens_cell),
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
#[allow(clippy::collapsible_if, clippy::type_complexity)]
mod tests {
    use super::*;
    use infinity_agent_core::traits::InputSender;
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

    fn spawn_worker(
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

        tokio::task::spawn_local(thread_worker(
            group_id.into(),
            input_rx,
            subscribe_rx,
            active_threads.clone(),
            subscribers,
            group_id.into(),
            Arc::new(model),
            conv,
            state,
            sender,
            String::new(),
            Arc::new(tools),
            None,
            None,
            Arc::new(std::sync::RwLock::new(None)),
            Arc::new(std::sync::RwLock::new(None)),
            idle_tx,
            0,
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
                    spawn_worker("t1", conv, state, model, vec![]);
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
                    spawn_worker("t1", conv, state, model, vec![Box::new(DummyTool)]);
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
                let (tx, mut display_rx, _, _) = spawn_worker("t1", conv, state, model, vec![]);
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
                    spawn_worker("t1", conv, state, model, vec![Box::new(DummyTool)]);
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
                    spawn_worker("t1", conv, state, model, vec![Box::new(SubTool)]);
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
                    spawn_worker("t1", conv, state, model, vec![Box::new(CloseThreadStub)]);
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
}
