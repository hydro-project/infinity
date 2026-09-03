//! Integration tests for the `keeps_session_alive` connection flag.
//!
//! A connection that declares `keeps_session_alive: false` (e.g. the Slack
//! bot's persistent per-thread connections) must not be the reason a session
//! is kept warm: once the agent finishes its work, the session should idle
//! out and its agent task should exit even though the client is still
//! connected. A normal (keep-alive) connection must keep the session warm.

use infinity_agent_core::ThreadId;
use std::sync::Arc;
use std::time::Duration;

use infinity_daemon::client_handler::handle_client_channels;
use infinity_daemon::ids::SequentialIdSource;
use infinity_daemon::session::{SessionManager, SessionManagerConfig, SharedSessionManager};
use infinity_protocol::{ClientMessage, DaemonMessage};
use infinity_provider_protocol::mock::{MockModelController, mock_model};
use infinity_provider_protocol::{ModelEntry, ModelProvider, SingleModelProvider};
use tokio::sync::{Mutex, mpsc};

type BoxError = Box<dyn std::error::Error + Send + Sync>;

struct TestHarness {
    manager: SharedSessionManager,
    ctrl: MockModelController,
    client_tx: mpsc::UnboundedSender<ClientMessage>,
    daemon_rx: mpsc::UnboundedReceiver<DaemonMessage>,
    _state_dir: tempfile::TempDir,
    cwd: tempfile::TempDir,
}

/// Boot an in-process session manager with a mock model and connect one
/// client to it through `handle_client_channels`. Must run inside a LocalSet.
async fn start_harness() -> Result<TestHarness, BoxError> {
    let (model, ctrl) = mock_model();
    let entry = ModelEntry {
        model_id: "mock-model".to_owned(),
        display_name: "Mock Model".to_owned(),
        context_window: 100_000,
        max_output_tokens: None,
        supports_image_input: false,
    };
    let state_dir = tempfile::tempdir()?;
    let cwd = tempfile::tempdir()?;
    let manager = SessionManager::with_providers(
        SessionManagerConfig {
            state_dir: state_dir.path().to_path_buf(),
            callback_url: "http://127.0.0.1:0".to_owned(),
            user_rap_config: None,
            id_source: Arc::new(SequentialIdSource::new()),
        },
        vec![(
            "mock".to_owned(),
            Arc::new(SingleModelProvider::new(entry, model)) as Arc<dyn ModelProvider>,
        )],
        vec![],
    )
    .await?;
    let manager = SharedSessionManager::new(Mutex::new(manager));

    let (client_tx, client_rx) = mpsc::unbounded_channel::<ClientMessage>();
    let (daemon_tx, daemon_rx) = mpsc::unbounded_channel::<DaemonMessage>();
    tokio::task::spawn_local(handle_client_channels(
        client_rx,
        daemon_tx,
        manager.clone(),
    ));

    Ok(TestHarness {
        manager,
        ctrl,
        client_tx,
        daemon_rx,
        _state_dir: state_dir,
        cwd,
    })
}

/// Receive daemon messages until the predicate matches, with a timeout.
async fn wait_for_message(
    rx: &mut mpsc::UnboundedReceiver<DaemonMessage>,
    mut pred: impl FnMut(&DaemonMessage) -> bool,
) -> DaemonMessage {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let msg = rx
                .recv()
                .await
                .expect("daemon channel closed while waiting");
            if pred(&msg) {
                return msg;
            }
        }
    })
    .await
    .expect("timed out waiting for daemon message")
}

/// Run one full round: create a session with the given keep-alive flag, send
/// input, complete a mock model response, and return the session id.
async fn create_session_and_chat(h: &mut TestHarness, keeps_session_alive: bool) -> ThreadId {
    h.client_tx
        .send(ClientMessage::CreateSession {
            cwd: h.cwd.path().to_path_buf(),
            location: None,
            model: None,
            keeps_session_alive,
        })
        .expect("send CreateSession");

    let connected = wait_for_message(&mut h.daemon_rx, |m| {
        matches!(m, DaemonMessage::Connected { .. })
    })
    .await;
    let DaemonMessage::Connected {
        root_thread_id: session_id,
        ..
    } = connected
    else {
        unreachable!()
    };

    h.client_tx
        .send(ClientMessage::UserInput {
            thread_id: session_id.clone(),
            text: "hello".to_owned(),
        })
        .expect("send UserInput");

    let _req = h.ctrl.next_request().await;
    h.ctrl.send_text("hi there");
    h.ctrl.finish();

    wait_for_message(&mut h.daemon_rx, |m| {
        matches!(m, DaemonMessage::ResponseDone { .. })
    })
    .await;

    session_id.id
}

/// Poll until the session's RAP servers have been stopped (the session-idle
/// teardown path ran), or time out.
async fn wait_for_server_teardown(
    manager: &SharedSessionManager,
    session_id: &ThreadId<str>,
) -> bool {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        {
            let mgr = manager.lock().await;
            if mgr.rap_manager.times_shut_down(session_id) > 0 {
                assert!(
                    mgr.is_session_idle(session_id),
                    "servers must only be stopped once the session is idle"
                );
                return true;
            }
        }
        if tokio::time::Instant::now() > deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// A connection with `keeps_session_alive: false` (like the Slack bot) must
/// not keep the session warm: after the response completes, the session goes
/// idle and its RAP servers are stopped even though the client is still
/// connected. (The agent system itself keeps running — servers reboot lazily
/// on the next input.)
#[tokio::test(flavor = "current_thread")]
async fn non_keep_alive_client_does_not_keep_session_warm() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let mut h = start_harness().await.expect("start harness");
            let session_id = create_session_and_chat(&mut h, false).await;

            // The client is still connected (client_tx alive), but the
            // session must still wind down because the only subscriber is
            // non-keep-alive.
            assert!(
                wait_for_server_teardown(&h.manager, &session_id).await,
                "session should idle out despite the connected non-keep-alive client"
            );
            let mgr = h.manager.lock().await;
            assert!(
                mgr.session_store.lock().await.is_idle(&session_id),
                "session should be marked idle in the store"
            );
        })
        .await;
}

/// User text is admitted for a stopped session, and the driver's `Live`
/// transition reactivates the session before the response completes.
#[tokio::test(flavor = "current_thread")]
async fn user_input_restarts_stopped_session() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let mut h = start_harness().await.expect("start harness");
            let session_id = create_session_and_chat(&mut h, true).await;

            {
                let mgr = h.manager.lock().await;
                mgr.cleanup_session(&session_id).await;
                assert!(mgr.session_store.lock().await.is_shut_down(&session_id));
            }

            h.client_tx
                .send(ClientMessage::UserInput {
                    thread_id: infinity_protocol::ThreadRef::local(session_id.clone()),
                    text: "resume".to_owned(),
                })
                .expect("send user input");

            let _req = h.ctrl.next_request().await;
            {
                let mgr = h.manager.lock().await;
                assert!(
                    !mgr.session_store.lock().await.is_shut_down(&session_id),
                    "the Live transition must reactivate the stopped session"
                );
            }
            h.ctrl.send_text("resumed");
            h.ctrl.finish();
            wait_for_message(&mut h.daemon_rx, |message| {
                matches!(message, DaemonMessage::ResponseDone { .. })
            })
            .await;
        })
        .await;
}

/// A normal connection (`keeps_session_alive: true`) keeps the session warm
/// while it stays connected: its RAP servers are never stopped.
#[tokio::test(flavor = "current_thread")]
async fn keep_alive_client_keeps_session_warm() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let mut h = start_harness().await.expect("start harness");
            let session_id = create_session_and_chat(&mut h, true).await;

            // Give the idle machinery ample time to (incorrectly) tear the
            // session down; its servers must still be untouched afterwards.
            tokio::time::sleep(Duration::from_millis(500)).await;
            let mgr = h.manager.lock().await;
            assert_eq!(
                mgr.rap_manager.times_shut_down(&session_id),
                0,
                "session servers should stay warm while a keep-alive client is connected"
            );
        })
        .await;
}

/// A thread holding an active subscription keeps its session's RAP servers
/// warm even though its driver idles out: the events the subscription will
/// deliver need the servers to still be running. The client is
/// non-keep-alive, so the subscription is the only thing keeping the session
/// active.
#[tokio::test(flavor = "current_thread")]
async fn active_subscription_keeps_rap_servers_warm() {
    use infinity_agent_core::message::{InputMessage, InputMessageContent};
    use infinity_provider_protocol::message::{ToolResult, ToolResultContent, UserContent};

    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let mut h = start_harness().await.expect("start harness");

            h.client_tx
                .send(ClientMessage::CreateSession {
                    cwd: h.cwd.path().to_path_buf(),
                    location: None,
                    model: None,
                    keeps_session_alive: false,
                })
                .expect("send CreateSession");
            let connected = wait_for_message(&mut h.daemon_rx, |m| {
                matches!(m, DaemonMessage::Connected { .. })
            })
            .await;
            let DaemonMessage::Connected {
                root_thread_id: session_id,
                ..
            } = connected
            else {
                unreachable!()
            };
            let session_id = session_id.id;

            // Round 1 leaves a pending tool call for the subscription setup.
            h.client_tx
                .send(ClientMessage::UserInput {
                    thread_id: infinity_protocol::ThreadRef::local(session_id.clone()),
                    text: "watch the build".to_owned(),
                })
                .expect("send UserInput");
            let _req = h.ctrl.next_request().await;
            h.ctrl
                .send_tool_call("tc-sub", "sleep", serde_json::json!({"seconds": 600}));
            h.ctrl.finish();

            // The setup result arrives with `subscription: true`, settling
            // the call and recording the active subscription.
            let result = InputMessage {
                content: InputMessageContent::User(UserContent::ToolResult(ToolResult {
                    id: "tc-sub".to_owned(),
                    call_id: None,
                    content: vec![ToolResultContent::Text(
                        infinity_provider_protocol::message::Text {
                            text: "subscribed to build events".to_owned(),
                        },
                    )],
                })),
                group_id: session_id.clone(),
                metadata: None,
                synthetic: None,
                display_as: None,
                subscription: true,
            };
            assert!(
                h.manager
                    .lock()
                    .await
                    .send_input((result, Some("sub-result".to_owned())))
                    .await,
                "subscription result must be admitted"
            );
            let _req2 = h.ctrl.next_request().await;
            h.ctrl.send_text("watching");
            // The driver is mid-completion here, so any teardown so far
            // predates the subscription (the non-keep-alive attach at
            // session creation idles once before any input).
            let teardowns_before = h
                .manager
                .lock()
                .await
                .rap_manager
                .times_shut_down(&session_id);
            h.ctrl.finish();
            wait_for_message(&mut h.daemon_rx, |m| {
                matches!(m, DaemonMessage::ResponseDone { .. })
            })
            .await;

            // Let the driver exit and the activity watcher process it
            // (everything runs on this LocalSet, so yielding drains the
            // pending wakeups).
            for _ in 0..20 {
                tokio::task::yield_now().await;
            }

            // The driver idled, but the subscription keeps the session
            // active: its RAP servers stay up.
            let mgr = h.manager.lock().await;
            assert!(
                !mgr.is_session_idle(&session_id),
                "an active subscription counts as session activity"
            );
            assert_eq!(
                mgr.rap_manager.times_shut_down(&session_id),
                teardowns_before,
                "servers must stay warm while a subscription can deliver events"
            );
        })
        .await;
}
