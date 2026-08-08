//! Daemon-level tests of the session wiring on top of the core agent system:
//! per-round model resolution via [`CatalogModelSource`], model inheritance
//! for spawned threads, and the [`DaemonObserver`]'s state persistence.
//!
//! The generic driver behaviors (batching, interruption, deferral, idling,
//! compaction) are tested in `infinity_agent_core::system::tests`.

use std::sync::Arc;

use infinity_agent_core::message::{InputMessage, InputMessageContent};
use infinity_agent_core::system::{AgentSystemBuilder, ChannelSender, RunningSystem};
use infinity_agent_core::tools::Tool;
use infinity_agent_core::traits::ConversationStore;
use infinity_protocol::DaemonMessage;
use infinity_provider_protocol::{ModelEntry, SingleModelProvider};
use rig::message::UserContent;
use rig_mock::mock_model;
use tokio::sync::mpsc;

use super::observer::{DaemonObserver, SubscribeRequest, Subscriber, SubscriberMap};
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
    InMemoryStateStore,
    tempfile::TempDir,
) {
    let dir = tempfile::tempdir().expect("create temp dir");
    let conv = InMemoryConversationStore::new_with_dir(
        dir.path().join("threads"),
        default_model,
        Arc::new(crate::ids::UuidIdSource),
    );
    let state = InMemoryStateStore::new(dir.path().join("state"));
    (conv, state, dir)
}

/// Start an agent system wired exactly like a daemon session: a
/// [`CatalogModelSource`] over the catalog + conversation store, and a
/// [`DaemonObserver`] per thread. Returns the running handle and a subscriber
/// channel receiving the root thread's `DaemonMessage`s.
fn start_daemon_system(
    root: &str,
    conv: InMemoryConversationStore,
    state: InMemoryStateStore,
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
    .tools(tools)
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
        let root = root.to_owned();
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
                root_session_id: root.clone(),
                subscribers,
                conversation_store: conv.clone(),
            }
        }
    };
    let running = system.start(make_observer);
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
            running
                .send_user_text("t1", "use the tool")
                .await
                .expect("send");
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
                .await
                .expect("send result");
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
            running.send_user_text("t1", "start").await.expect("send");
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
                .await
                .expect("send result");
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
            running
                .send_user_text("root", "spawn a child")
                .await
                .expect("send");

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
        root_session_id: "t1".to_owned(),
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

/// The daemon observer tracks pending user choices through the awaited
/// durability hooks: stored (and only then broadcast) when required, removed
/// before the step continues when dismissed.
#[tokio::test(flavor = "current_thread")]
async fn observer_tracks_pending_choices() {
    use infinity_agent_core::system::{ThreadObserver, UserChoice};

    let (conv, _state, _dir) = tmp_stores(model1_ref());
    conv.ensure_root_thread("t1").await.expect("ensure root");
    let observer = DaemonObserver {
        root_session_id: "t1".to_owned(),
        subscribers: Default::default(),
        conversation_store: conv.clone(),
    };

    observer
        .on_user_choice_required(
            "t1",
            &UserChoice {
                id: "c1".to_owned(),
                prompt: "Pick".to_owned(),
                choices: vec!["A".to_owned(), "B".to_owned()],
                default: 0,
                response_url: "http://x".to_owned(),
            },
        )
        .await
        .expect("record choice");
    assert!(conv.has_pending_choices("t1"));

    observer
        .on_user_choice_dismissed("t1", "c1")
        .await
        .expect("dismiss choice");
    assert!(!conv.has_pending_choices("t1"));
}
