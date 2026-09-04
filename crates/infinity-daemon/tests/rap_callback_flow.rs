//! Integration tests for the RAP callback path: callbacks flow straight into
//! the agent system's input queue (the router owns admission),
//! the session activity watcher owns the idle flag, and the daemon handles
//! only view updates itself.

use std::sync::Arc;
use std::time::Duration;

use infinity_daemon::client_handler::handle_client_channels;
use infinity_daemon::ids::SequentialIdSource;
use infinity_daemon::rap_callback;
use infinity_daemon::session::{SessionManager, SessionManagerConfig, SharedSessionManager};
use infinity_protocol::{ClientMessage, DaemonMessage, SessionStatus};
use infinity_provider_protocol::mock::{MockModelController, mock_model};
use infinity_provider_protocol::{ModelEntry, ModelProvider, SingleModelProvider};
use tokio::sync::mpsc;

type BoxError = Box<dyn std::error::Error + Send + Sync>;

struct TestHarness {
    manager: SharedSessionManager,
    callback_url: String,
    ctrl: MockModelController,
    client_tx: mpsc::UnboundedSender<ClientMessage>,
    daemon_rx: mpsc::UnboundedReceiver<DaemonMessage>,
    _state_dir: tempfile::TempDir,
    cwd: tempfile::TempDir,
}

/// Boot a session manager with a mock model, serve its RAP callback listener,
/// and connect one client. Must run inside a `LocalSet`.
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

    let bridge = infinity_rap_bridge::RapCallbackBridge::bind().await?;
    let callback_url = bridge.callback_url().to_owned();
    let manager = SessionManager::with_providers(
        SessionManagerConfig {
            state_dir: state_dir.path().to_path_buf(),
            callback_url: callback_url.clone(),
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
    let manager = rap_callback::serve_callbacks(bridge, manager);

    let (client_tx, client_rx) = mpsc::unbounded_channel::<ClientMessage>();
    let (daemon_tx, daemon_rx) = mpsc::unbounded_channel::<DaemonMessage>();
    tokio::task::spawn_local(handle_client_channels(
        client_rx,
        daemon_tx,
        manager.clone(),
    ));

    Ok(TestHarness {
        manager,
        callback_url,
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

/// Create a session, run one chat round, and return the session id.
async fn create_session_and_chat(h: &mut TestHarness) -> infinity_protocol::ThreadRef {
    h.client_tx
        .send(ClientMessage::CreateSession {
            cwd: h.cwd.path().to_path_buf(),
            location: None,
            model: None,
            keeps_session_alive: true,
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

    session_id
}

/// Wait until a `SessionsUpdated` broadcast reports the given status.
async fn wait_for_status(
    rx: &mut mpsc::UnboundedReceiver<DaemonMessage>,
    session_id: &infinity_protocol::ThreadRef,
    status: SessionStatus,
) {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let msg = rx
                .recv()
                .await
                .expect("broadcast channel closed while waiting");
            if let DaemonMessage::SessionsUpdated { sessions } = msg
                && sessions.get(session_id).map(|info| &info.status) == Some(&status)
            {
                return;
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for session status {status:?}"))
}

/// The session activity watcher owns the stored idle flag: it clears it when
/// a thread driver spawns (observed through the core lifecycle channel, with
/// no bookkeeping in the input path) and restores it once the driver exits.
/// Status broadcasts therefore follow thread activity: Running while the
/// model round is in flight, Idle after it completes.
#[tokio::test(flavor = "current_thread")]
async fn session_status_follows_thread_activity() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let mut h = start_harness().await.expect("start harness");

            // Subscribe to status broadcasts before doing anything.
            let (status_tx, mut status_rx) = mpsc::unbounded_channel();
            h.manager.lock().await.list_sessions(Some(status_tx)).await;

            h.client_tx
                .send(ClientMessage::CreateSession {
                    cwd: h.cwd.path().to_path_buf(),
                    location: None,
                    model: None,
                    keeps_session_alive: true,
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

            // The driver spawn flips the session to Running before the model
            // has produced anything.
            wait_for_status(&mut status_rx, &session_id, SessionStatus::Running).await;

            let _req = h.ctrl.next_request().await;
            h.ctrl.send_text("hi there");
            h.ctrl.finish();
            wait_for_message(&mut h.daemon_rx, |m| {
                matches!(m, DaemonMessage::ResponseDone { .. })
            })
            .await;

            // The driver exits once nothing is pending; the idle evaluation
            // restores the stored flag.
            wait_for_status(&mut status_rx, &session_id, SessionStatus::Idle).await;
        })
        .await;
}

/// A view-update callback never enters agent history: it is stored and
/// broadcast to the session's subscribers, and the session stays idle
/// because no driver wakes.
#[tokio::test(flavor = "current_thread")]
async fn view_update_is_broadcast_without_waking_the_session() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let mut h = start_harness().await.expect("start harness");
            let session_id = create_session_and_chat(&mut h).await;

            let callback = serde_json::json!({
                "type": "view_update",
                "group_id": session_id,
                "view_type": "diff",
                "content": { "lines": 3 },
            });
            let status = reqwest_post(&h.callback_url, &callback.to_string()).await;
            assert_eq!(status, 200, "callback must be accepted");

            let update = wait_for_message(&mut h.daemon_rx, |m| {
                matches!(m, DaemonMessage::ViewUpdate { .. })
            })
            .await;
            let DaemonMessage::ViewUpdate {
                thread_id,
                view_type,
                ..
            } = update
            else {
                unreachable!()
            };
            assert_eq!(thread_id.as_ref(), Some(&session_id));
            assert_eq!(view_type, "diff");

            let mgr = h.manager.lock().await;
            assert!(
                mgr.is_session_idle(&session_id.id),
                "a view update must not wake any thread driver"
            );
        })
        .await;
}

/// POST a JSON body and return the HTTP status code.
async fn reqwest_post(url: &str, body: &str) -> u16 {
    use rap_client::http::HttpClient;
    rap_client::http::SimpleHttpClient::new()
        .post(url, body)
        .await
        .expect("POST to the callback listener")
}
