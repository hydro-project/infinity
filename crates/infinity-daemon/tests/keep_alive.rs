//! Integration tests for the `keeps_session_alive` connection flag.
//!
//! A connection that declares `keeps_session_alive: false` (e.g. the Slack
//! bot's persistent per-thread connections) must not be the reason a session
//! is kept warm: once the agent finishes its work, the session should idle
//! out and its agent task should exit even though the client is still
//! connected. A normal (keep-alive) connection must keep the session warm.

use std::sync::Arc;
use std::time::Duration;

use infinity_daemon::client_handler::handle_client_channels;
use infinity_daemon::ids::SequentialIdSource;
use infinity_daemon::session::{SessionManager, SessionManagerConfig, SharedSessionManager};
use infinity_protocol::{ClientMessage, DaemonMessage};
use infinity_provider_protocol::{ModelEntry, ModelProvider, SingleModelProvider};
use rig_mock::{MockModelController, mock_model};
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
async fn create_session_and_chat(h: &mut TestHarness, keeps_session_alive: bool) -> String {
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
    let DaemonMessage::Connected { session_id, .. } = connected else {
        unreachable!()
    };

    h.client_tx
        .send(ClientMessage::UserInput {
            session_id: session_id.clone(),
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

/// Poll until the session's RAP servers have been stopped (the session-idle
/// teardown path ran), or time out.
async fn wait_for_server_teardown(manager: &SharedSessionManager, session_id: &str) -> bool {
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
