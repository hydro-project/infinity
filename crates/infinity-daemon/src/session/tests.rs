//! Daemon-level tests of the session wiring on top of the core agent system:
//! per-round model resolution via [`CatalogModelSource`], model inheritance
//! for spawned threads, and the [`DaemonObserver`]'s state persistence.
//!
//! The generic driver behaviors (batching, interruption, deferral, idling,
//! compaction) are tested in `infinity_agent_core::system::tests`.

use std::rc::Rc;
use std::sync::Arc;

use async_trait::async_trait;

use infinity_agent_core::message::{InputMessage, InputMessageContent, UserChoiceRequired};
use infinity_agent_core::system::local::{ChannelSender, RunningSystem, ThreadLifecycleEvent};
use infinity_agent_core::system::{
    AgentSystemBuilder, NoRapHttp, ThreadConfig, ThreadConfigSource, UserChoice,
};
use infinity_agent_core::tools::Tool;
use infinity_agent_core::traits::{ConversationStore, StateStore};
use infinity_protocol::{DaemonMessage, SessionStatus};
use infinity_provider_protocol::{ModelEntry, ModelProvider, SingleModelProvider};
use rig::message::UserContent;
use rig_mock::mock_model;
use tokio::sync::mpsc;

use super::observer::{DaemonObserver, SubscribeRequest, Subscriber, SubscriberMap};
use super::{SessionManager, SessionManagerConfig};
use crate::ids::SequentialIdSource;
use crate::memory_store::{InMemoryConversationStore, InMemoryStateStore};
use crate::models::{CatalogModelSource, ModelCatalog};

fn model1_ref() -> infinity_protocol::ModelRef {
    infinity_protocol::ModelRef {
        provider_id: "provider1".to_owned(),
        model_id: "model1".to_owned(),
    }
}

fn model2_ref() -> infinity_protocol::ModelRef {
    infinity_protocol::ModelRef {
        provider_id: "provider2".to_owned(),
        model_id: "model2".to_owned(),
    }
}

async fn two_model_catalog(
    model1: rig_mock::MockCompletionModel,
    model2: rig_mock::MockCompletionModel,
) -> Arc<ModelCatalog> {
    let entry = |id: &str| ModelEntry {
        model_id: id.to_owned(),
        display_name: id.to_owned(),
        context_window: 0,
        max_output_tokens: None,
        supports_image_input: false,
    };
    Arc::new(
        ModelCatalog::new(vec![
            (
                "provider1".to_owned(),
                Arc::new(SingleModelProvider::new(entry("model1"), model1)) as _,
            ),
            (
                "provider2".to_owned(),
                Arc::new(SingleModelProvider::new(entry("model2"), model2)) as _,
            ),
        ])
        .await
        .expect("build two-model catalog"),
    )
}

fn tmp_stores(
    default_model: infinity_protocol::ModelRef,
) -> (
    InMemoryConversationStore,
    infinity_agent_core::stores::InMemoryStateStore,
    tempfile::TempDir,
) {
    let dir = tempfile::tempdir().expect("create temp dir");
    let conv = InMemoryConversationStore::new_with_dir(
        dir.path().join("threads"),
        default_model,
        Arc::new(crate::ids::UuidIdSource),
    );
    let state = infinity_agent_core::stores::InMemoryStateStore::new();
    (conv, state, dir)
}

struct TestThreadConfig {
    tools: Vec<Rc<dyn Tool<ChannelSender>>>,
}

#[async_trait(?Send)]
impl ThreadConfigSource<ChannelSender, NoRapHttp> for TestThreadConfig {
    async fn resolve(
        &self,
        _thread_id: &str,
    ) -> Result<ThreadConfig<ChannelSender, NoRapHttp>, Box<dyn std::error::Error + Send + Sync>>
    {
        Ok(ThreadConfig {
            tools: self.tools.clone(),
            extra_system_prompt: None,
            rap_notifier: None,
        })
    }
}

/// Start an agent system wired exactly like a daemon session: a
/// [`CatalogModelSource`] over the catalog + conversation store, and a
/// [`DaemonObserver`] per thread. Returns the running handle and a subscriber
/// channel receiving the root thread's `DaemonMessage`s.
fn start_daemon_system(
    root: &str,
    conv: InMemoryConversationStore,
    state: impl StateStore + 'static,
    catalog: Arc<ModelCatalog>,
    tools: Vec<Box<dyn Tool<ChannelSender>>>,
) -> (
    RunningSystem<SubscribeRequest>,
    mpsc::UnboundedReceiver<DaemonMessage>,
    SubscriberMap,
) {
    let system = AgentSystemBuilder::new_local(
        conv.clone(),
        state,
        CatalogModelSource {
            catalog,
            conversation_store: conv.clone(),
        },
    )
    .thread_config(TestThreadConfig {
        tools: tools.into_iter().map(Rc::from).collect(),
    })
    .build_local();

    let (client_tx, client_rx) = mpsc::unbounded_channel();
    let subscriber_map: SubscriberMap = Default::default();
    subscriber_map.lock().expect("bug: mutex poisoned").insert(
        root.to_owned(),
        Arc::new(std::sync::Mutex::new(vec![Subscriber {
            tx: client_tx,
            keeps_session_alive: true,
        }])),
    );

    let make_observer = {
        let subscriber_map = subscriber_map.clone();
        move |thread_id: &str| {
            let parent_subs = {
                let parent_id = conv.get_thread_parent_id(thread_id);
                let smap = subscriber_map.lock().expect("bug: mutex poisoned");
                let source = parent_id.as_deref().unwrap_or(thread_id);
                smap.get(source)
                    .map(|arc| arc.lock().expect("bug: mutex poisoned").clone())
                    .unwrap_or_default()
            };
            let subscribers = subscriber_map
                .lock()
                .expect("bug: mutex poisoned")
                .entry(thread_id.to_owned())
                .or_insert_with(|| Arc::new(std::sync::Mutex::new(parent_subs)))
                .clone();
            DaemonObserver {
                subscribers,
                conversation_store: conv.clone(),
            }
        }
    };
    let running = system.start_with_observer(make_observer);
    (running, client_rx, subscriber_map)
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

fn tool_result_input(group_id: &str, id: &str, text: &str) -> InputMessage {
    InputMessage {
        content: InputMessageContent::User(UserContent::ToolResult(rig::message::ToolResult {
            id: id.into(),
            call_id: None,
            content: rig::OneOrMany::one(rig::message::ToolResultContent::Text(rig::agent::Text {
                text: text.into(),
            })),
        })),
        group_id: group_id.into(),
        metadata: None,
        synthetic: None,
        display_as: None,
        subscription: false,
    }
}

/// An async tool whose result is delivered later via the input queue —
/// keeps the driver alive between completion rounds.
struct AsyncStubTool;

#[async_trait::async_trait]
impl Tool<ChannelSender> for AsyncStubTool {
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
        _: &infinity_agent_core::tools::ToolContext<ChannelSender>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        Ok(())
    }
}

/// A model switch persisted while the thread waits for an async tool result
/// is applied to the next completion round.
#[tokio::test(flavor = "current_thread")]
async fn model_switch_applies_to_next_completion() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (conv, state, _dir) = tmp_stores(model1_ref());
            let (model1, mut ctrl1) = mock_model();
            let (model2, mut ctrl2) = mock_model();
            let catalog = two_model_catalog(model1, model2).await;
            conv.ensure_root_thread("t1").await.expect("ensure root");

            let (running, mut display_rx, _smap) = start_daemon_system(
                "t1",
                conv.clone(),
                state,
                catalog,
                vec![Box::new(AsyncStubTool)],
            );

            // First round runs on model1 and leaves an async tool call
            // pending (so the driver stays alive).
            running.send_user_text("t1", "use the tool").await;
            let _req = ctrl1.next_request().await;
            ctrl1.send_tool_call("tc-1", "async_tool", serde_json::json!({}));
            ctrl1.finish();
            collect_until_done(&mut display_rx).await;

            // Switch while the driver waits for the tool result. (The session
            // manager persists the selection; each round resolves it fresh.)
            conv.set_thread_model("t1", model2_ref());

            // The tool result triggers the next round — on model2.
            running
                .send(tool_result_input("t1", "tc-1", "tool done"), "res-1")
                .await;
            let _req2 = ctrl2.next_request().await;
            ctrl2.send_text("hello from model2");
            ctrl2.finish();
            collect_until_done(&mut display_rx).await;
            assert!(
                ctrl1.try_next_request().is_none(),
                "model1 should not receive requests after the switch"
            );
        })
        .await;
}

/// A switch persisted while a completion is in flight does not disturb that
/// completion — it finishes on the old model — but the next round uses the
/// new one.
#[tokio::test(flavor = "current_thread")]
async fn model_switch_during_completion_applies_to_next_round() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (conv, state, _dir) = tmp_stores(model1_ref());
            let (model1, mut ctrl1) = mock_model();
            let (model2, mut ctrl2) = mock_model();
            let catalog = two_model_catalog(model1, model2).await;
            conv.ensure_root_thread("t1").await.expect("ensure root");

            let (running, mut display_rx, _smap) = start_daemon_system(
                "t1",
                conv.clone(),
                state,
                catalog,
                vec![Box::new(AsyncStubTool)],
            );

            // Start a completion on model1 and leave it in flight.
            running.send_user_text("t1", "start").await;
            let _req = ctrl1.next_request().await;
            ctrl1.send_text("streaming on model1...");
            loop {
                match tokio::time::timeout(std::time::Duration::from_secs(2), display_rx.recv())
                    .await
                {
                    Ok(Some(DaemonMessage::TextChunk { .. })) => break,
                    Ok(Some(_)) => {}
                    _ => panic!("timed out waiting for text chunk"),
                }
            }

            // Switch mid-completion: the in-flight round keeps streaming on
            // model1 (it resolved its model at round start).
            conv.set_thread_model("t1", model2_ref());

            // The in-flight completion finishes undisturbed on model1
            // (ending with an async tool call so the driver stays alive).
            ctrl1.send_tool_call("tc-1", "async_tool", serde_json::json!({}));
            ctrl1.finish();
            collect_until_done(&mut display_rx).await;

            // The next round (tool result) goes to model2.
            running
                .send(tool_result_input("t1", "tc-1", "tool done"), "res-1")
                .await;
            let _req2 = ctrl2.next_request().await;
            ctrl2.send_text("hello from model2");
            ctrl2.finish();
            collect_until_done(&mut display_rx).await;
            assert!(
                ctrl1.try_next_request().is_none(),
                "model1 should not receive requests after the switch"
            );
        })
        .await;
}

/// Regression test for issue #32: a spawned thread inherits the parent's
/// model, not the global default.
#[tokio::test(flavor = "current_thread")]
async fn spawned_thread_inherits_parent_model() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (conv, state, _dir) = tmp_stores(model1_ref());
            let (model1, mut ctrl1) = mock_model();
            let (model2, mut ctrl2) = mock_model();
            let catalog = two_model_catalog(model1, model2).await;

            conv.ensure_root_thread("root").await.expect("ensure root");
            // Set root thread to use the non-default model (provider2/model2).
            conv.set_thread_model("root", model2_ref());

            // The built-in spawn_thread tool comes from the system builder.
            let (running, mut display_rx, _smap) =
                start_daemon_system("root", conv.clone(), state, catalog, vec![]);

            // Send user input to root thread (which uses model2).
            running.send_user_text("root", "spawn a child").await;

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

            // After spawn_thread, the parent loops back on ctrl2.
            let parent_followup = ctrl2.next_request().await;
            let is_parent = parent_followup.chat_history.iter().any(|m| {
                if let rig::message::Message::User { content } = m
                    && let UserContent::ToolResult(r) = content.first()
                    && let rig::message::ToolResultContent::Text(t) = r.content.first()
                {
                    return t.text.contains("successfully spawned");
                }
                false
            });
            assert!(is_parent, "expected parent follow-up request on ctrl2");
            ctrl2.send_text("ok");
            ctrl2.finish();
            collect_until_done(&mut display_rx).await;

            // The child thread should also use model2 (inherited from the
            // parent). With the bug, the child would use the default model
            // (provider1), so ctrl1 would get the request instead.
            let child_req =
                tokio::time::timeout(std::time::Duration::from_secs(5), ctrl2.next_request())
                    .await
                    .expect(
                        "child thread should use model2 (parent's model), not model1 (default)",
                    );
            let has_instructions = child_req.chat_history.iter().any(|m| {
                if let rig::message::Message::User { content } = m
                    && let UserContent::ToolResult(r) = content.first()
                    && let rig::message::ToolResultContent::Text(t) = r.content.first()
                {
                    return t.text.contains("do something");
                }
                false
            });
            assert!(
                has_instructions,
                "child thread should have received spawn instructions"
            );
            assert!(
                ctrl1.try_next_request().is_none(),
                "default model (provider1/model1) should not have received any requests"
            );

            ctrl2.send_text("child done");
            ctrl2.finish();
        })
        .await;
}

/// The daemon observer persists token usage on `CompletionFinished` and
/// resets it when compaction is applied — synchronously at the emission
/// point, before the event reaches any client.
#[tokio::test(flavor = "current_thread")]
async fn observer_persists_usage_and_resets_on_compaction() {
    use infinity_agent_core::system::{AgentEvent, ThreadObserver};

    let (conv, _state, _dir) = tmp_stores(model1_ref());
    conv.ensure_root_thread("t1").await.expect("ensure root");
    let observer = DaemonObserver {
        subscribers: Default::default(),
        conversation_store: conv.clone(),
    };

    observer.on_event(
        "t1",
        &AgentEvent::CompletionFinished {
            usage: Some(rig::completion::Usage {
                input_tokens: 40,
                output_tokens: 2,
                total_tokens: 42,
                cached_input_tokens: 0,
            }),
        },
    );
    assert_eq!(conv.get_total_tokens_used("t1"), 42);

    // A usage-less response must not reset the stored total.
    observer.on_event("t1", &AgentEvent::CompletionFinished { usage: None });
    assert_eq!(conv.get_total_tokens_used("t1"), 42);

    // Compaction resets the stale pre-compaction total.
    observer.on_event("t1", &AgentEvent::CompactionApplied);
    assert_eq!(conv.get_total_tokens_used("t1"), 0);
}

#[tokio::test(flavor = "current_thread")]
async fn answered_user_choice_emits_complete_and_disappears_from_replay() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (conv, state, _dir) = tmp_stores(model1_ref());
            let (model, mut ctrl) = mock_model();
            let catalog = two_model_catalog(model.clone(), model).await;
            conv.ensure_root_thread("t1").await.expect("ensure root");
            let (running, mut display_rx, _) = start_daemon_system(
                "t1",
                conv,
                state.clone(),
                catalog,
                vec![Box::new(AsyncStubTool)],
            );

            running.send_user_text("t1", "use the tool").await;
            let _request = ctrl.next_request().await;
            ctrl.send_tool_call("tc-choice", "async_tool", serde_json::json!({}));
            ctrl.finish();
            collect_until_done(&mut display_rx).await;

            running
                .send(
                    InputMessage {
                        content: InputMessageContent::UserChoice(UserChoiceRequired {
                            content_type: "user_choice_required".to_owned(),
                            id: "tc-choice".to_owned(),
                            call_id: None,
                            prompt: "Pick one".to_owned(),
                            choices: vec!["A".to_owned(), "B".to_owned()],
                            default: 0,
                            response_url: "http://example.test/choice".to_owned(),
                        }),
                        group_id: "t1".to_owned(),
                        metadata: None,
                        synthetic: None,
                        display_as: None,
                        subscription: false,
                    },
                    "choice-message",
                )
                .await;
            assert!(matches!(
                display_rx.recv().await,
                Some(DaemonMessage::UserChoiceRequired { id, .. }) if id == "tc-choice"
            ));

            running
                .send(
                    tool_result_input("t1", "tc-choice", "selected A"),
                    "choice-result",
                )
                .await;
            let mut saw_complete = false;
            while !saw_complete {
                match tokio::time::timeout(std::time::Duration::from_secs(5), display_rx.recv())
                    .await
                    .expect("complete event timeout")
                    .expect("display channel closed")
                {
                    DaemonMessage::UserChoiceComplete { choice_id } => {
                        assert_eq!(choice_id, "tc-choice");
                        saw_complete = true;
                    }
                    DaemonMessage::StartOutput { .. } => {
                        ctrl.send_text("done");
                        ctrl.finish();
                    }
                    _ => {}
                }
            }
            assert!(
                state
                    .get_pending_user_choices("t1")
                    .await
                    .expect("load choices")
                    .is_empty()
            );

            let (replay_tx, mut replay_rx) = mpsc::unbounded_channel();
            running
                .subscribe(
                    "t1",
                    SubscribeRequest {
                        tx: replay_tx,
                        wants_replay: true,
                        keeps_session_alive: true,
                    },
                )
                .await;
            match tokio::time::timeout(std::time::Duration::from_secs(5), replay_rx.recv())
                .await
                .expect("replay timeout")
                .expect("replay channel closed")
            {
                DaemonMessage::Replay {
                    pending_choices, ..
                } => assert!(pending_choices.is_empty()),
                other => panic!("expected replay, got {other:?}"),
            }
        })
        .await;
}

/// A pending choice on any child thread represents session-wide waiting state,
/// even though choice lookup and response routing remain thread-scoped.
#[tokio::test(flavor = "current_thread")]
async fn child_pending_choice_updates_root_session_status() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (model, _ctrl) = mock_model();
            let entry = ModelEntry {
                model_id: "model1".to_owned(),
                display_name: "model1".to_owned(),
                context_window: 100_000,
                max_output_tokens: None,
                supports_image_input: false,
            };
            let state_dir = tempfile::tempdir().expect("create daemon state dir");
            let cwd = tempfile::tempdir().expect("create session cwd");
            let manager = SessionManager::with_providers(
                SessionManagerConfig {
                    state_dir: state_dir.path().to_path_buf(),
                    callback_url: "http://127.0.0.1:0".to_owned(),
                    user_rap_config: None,
                    id_source: Arc::new(SequentialIdSource::new()),
                },
                vec![(
                    "provider1".to_owned(),
                    Arc::new(SingleModelProvider::new(entry, model)) as Arc<dyn ModelProvider>,
                )],
                vec![],
            )
            .await
            .expect("build session manager");

            let mut emit = |_message| async {};
            let session_id = manager
                .create_session(cwd.path(), model1_ref(), &mut emit)
                .await
                .expect("create session");
            let child_id = manager
                .conversation_store
                .spawn_thread(&session_id, "spawn-call", false, None)
                .await
                .expect("spawn child thread");
            let choice = UserChoice {
                id: "choice-1".to_owned(),
                prompt: "Choose".to_owned(),
                choices: vec!["one".to_owned(), "two".to_owned()],
                default: 0,
                response_url: "http://127.0.0.1:0/choice".to_owned(),
            };

            manager
                .state_store
                .add_pending_user_choice(&child_id, choice.clone())
                .await
                .expect("add pending choice to child");
            assert!(manager.state_store.has_pending_choices(&child_id));
            assert!(!manager.state_store.has_pending_choices(&session_id));
            assert_eq!(
                manager
                    .list_sessions(None)
                    .await
                    .get(&session_id)
                    .expect("created session is listed")
                    .status,
                SessionStatus::WaitingForChoice
            );

            manager
                .state_store
                .remove_pending_user_choice(&child_id, &choice.id)
                .await
                .expect("dismiss child pending choice");
            assert_eq!(
                manager
                    .list_sessions(None)
                    .await
                    .get(&session_id)
                    .expect("created session is still listed")
                    .status,
                SessionStatus::Running
            );

            manager
                .state_store
                .add_pending_user_choice(&child_id, choice)
                .await
                .expect("restore pending choice before cleanup");
            manager.cleanup_session(&session_id).await;
            assert!(!manager.state_store.has_pending_choices(&child_id));
            assert_eq!(
                manager
                    .list_sessions(None)
                    .await
                    .get(&session_id)
                    .expect("cleaned-up session is listed")
                    .status,
                SessionStatus::Stopped
            );
        })
        .await;
}

/// A callback-style event (a tool result with no live driver) for a session
/// the user shut down is refused by the stopped-thread policy inside the
/// router, while the same event for a live session wakes its thread. This
/// keeps stale RAP callbacks from reviving stopped sessions regardless of
/// which input surface delivered them.
#[tokio::test(flavor = "current_thread")]
async fn shut_down_session_events_do_not_wake_threads() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (model, mut ctrl) = mock_model();
            let (model2, _ctrl2) = mock_model();
            let catalog = two_model_catalog(model, model2).await;
            let (conv, _state, dir) = tmp_stores(model1_ref());
            conv.ensure_root_thread("live").await.expect("ensure root");
            conv.ensure_root_thread("stopped")
                .await
                .expect("ensure root");

            // Session registry: both sessions exist; one is shut down.
            let (change_tx, _change_rx) = mpsc::unbounded_channel();
            let session_store = Arc::new(tokio::sync::Mutex::new(
                crate::session_store::SessionStore::load(
                    &dir.path().join("sessions.json").to_string_lossy(),
                    change_tx,
                ),
            ));
            {
                let mut store = session_store.lock().await;
                store.create("live", dir.path().to_path_buf());
                store.create("stopped", dir.path().to_path_buf());
                store.mark_shut_down("stopped");
            }
            let state =
                InMemoryStateStore::new(dir.path().join("state"), conv.clone(), session_store);

            let (mut running, mut display_rx, _smap) =
                start_daemon_system("live", conv.clone(), state, catalog, vec![]);

            // The stopped session's event is dropped: no driver wakes, so no
            // thread exit is ever reported for it.
            running
                .send(
                    tool_result_input("stopped", "tc-stale", "late result"),
                    "cb-stale",
                )
                .await;

            // The live session processes user text normally afterwards,
            // proving the router did not stall on the dropped event.
            running.send_user_text("live", "hello").await;
            let _req = ctrl.next_request().await;
            ctrl.send_text("hi");
            ctrl.finish();
            collect_until_done(&mut display_rx).await;

            // Every lifecycle transition must belong to the live session;
            // the stopped session's event may never wake a driver.
            let mut saw_live_idle = false;
            while !saw_live_idle {
                let event = tokio::time::timeout(
                    std::time::Duration::from_secs(5),
                    running.thread_lifecycle.recv(),
                )
                .await
                .expect("timed out waiting for the live thread to idle")
                .expect("thread lifecycle channel closed");
                match event {
                    ThreadLifecycleEvent::Live { thread_id } => {
                        assert_eq!(thread_id, "live", "only the live session may wake");
                    }
                    ThreadLifecycleEvent::Idle { thread_id } => {
                        assert_eq!(thread_id, "live", "only the live session's driver may run");
                        saw_live_idle = true;
                    }
                }
            }
            assert!(
                running.is_idle(),
                "the stopped session must not have a live driver"
            );
        })
        .await;
}
