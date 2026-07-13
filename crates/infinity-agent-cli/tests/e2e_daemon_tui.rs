//! End-to-end tests: the real daemon session machinery ([`SessionManager`] +
//! `handle_client_channels`) running with a deterministic mock model
//! provider (rig-mock), driven through the real TUI client loop
//! (`daemon_client::run_client`) rendered into a vt100 virtual terminal.
//!
//! Everything runs in-process on a single current-thread runtime with a
//! paused clock:
//!
//! * the daemon side uses `spawn_local` throughout, so the whole test body
//!   runs inside a [`tokio::task::LocalSet`];
//! * `settle()` awaits a tiny sleep that (via auto-advance) only completes
//!   once every task has drained its queues, making each step
//!   deterministic;
//! * the mock model controller lets the test decide exactly when and what
//!   the "LLM" answers.

mod common;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::{ScriptedEvents, SharedEmulator, VirtualTerm, Vt100Emulator, render_screen};
use infinity_agent_cli::daemon_client;
use infinity_agent_core::model_provider::{ModelEntry, ModelProvider, SingleModelProvider};
use infinity_daemon::client_handler::handle_client_channels;
use infinity_daemon::ids::SequentialIdSource;
use infinity_daemon::session::{SessionManager, SessionManagerConfig};
use infinity_protocol::{ClientMessage, DaemonMessage};
use ratatui::crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use rig::message::UserContent;
use rig_mock::{MockModelController, mock_model};
use tokio::sync::mpsc;

type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// Snapshot the rendered screen. Session/thread ids are deterministic
/// ([`SequentialIdSource`]), so no redaction is needed.
macro_rules! assert_screen {
    ($name:expr, $screen:expr) => {
        insta::assert_snapshot!($name, $screen);
    };
}

/// A full in-process daemon + TUI client under test.
struct E2eHarness {
    emu: SharedEmulator,
    event_tx: mpsc::UnboundedSender<Event>,
    /// The `run_client` task (the TUI client loop).
    client: tokio::task::JoinHandle<Result<(), BoxError>>,
    /// Keeps the daemon state dir alive for the duration of the test.
    _state_dir: tempfile::TempDir,
    /// The (empty) working directory sessions are created in.
    _cwd: tempfile::TempDir,
}

impl E2eHarness {
    /// Wire up the daemon (mock provider, temp state dir) and the TUI client
    /// over in-memory channels, on a `cols`×`rows` vt100 terminal.
    async fn spawn(cols: u16, rows: u16) -> (Self, MockModelController) {
        let (model, ctrl) = mock_model();
        let entry = ModelEntry {
            model_id: "mock-model".to_owned(),
            display_name: "Mock Model".to_owned(),
            context_window: 100_000,
            max_output_tokens: None,
        };
        let providers = vec![(
            "mock".to_owned(),
            Arc::new(SingleModelProvider::new(entry, model)) as Arc<dyn ModelProvider>,
        )];

        let state_dir = tempfile::tempdir().expect("create state temp dir");
        let cwd = tempfile::tempdir().expect("create cwd temp dir");

        let (listener, callback_url) = rap_client::callback_server::bind_callback_listener()
            .await
            .expect("bind callback listener");
        let manager = SessionManager::with_providers(
            SessionManagerConfig {
                state_dir: state_dir.path().to_path_buf(),
                callback_url,
                user_rap_config: None,
                id_source: Arc::new(SequentialIdSource::new()),
            },
            providers,
            vec![],
        )
        .await
        .expect("build session manager");
        let mgr = infinity_daemon::rap_callback::serve_callbacks(listener, manager);

        // In-memory transport between client and daemon.
        let (to_daemon_tx, to_daemon_rx) = mpsc::unbounded_channel::<ClientMessage>();
        let (from_daemon_tx, from_daemon_rx) = mpsc::unbounded_channel::<DaemonMessage>();
        tokio::task::spawn_local(handle_client_channels(
            to_daemon_rx,
            from_daemon_tx,
            mgr.clone(),
        ));

        let emu: SharedEmulator = Arc::new(Mutex::new(Box::new(Vt100Emulator::new(cols, rows))));
        let (event_tx, event_rx) = mpsc::unbounded_channel();

        let client = tokio::task::spawn_local(daemon_client::run_client(
            VirtualTerm::new(Arc::clone(&emu)),
            ScriptedEvents::new(event_rx),
            cwd.path().to_path_buf(),
            from_daemon_rx,
            to_daemon_tx,
            None,
            None,
            None,
        ));

        let harness = Self {
            emu,
            event_tx,
            client,
            _state_dir: state_dir,
            _cwd: cwd,
        };
        harness.settle().await;
        (harness, ctrl)
    }

    /// Let all tasks process everything queued so far (paused-clock
    /// auto-advance completes this sleep only once nothing can make
    /// progress).
    async fn settle(&self) {
        tokio::time::sleep(Duration::from_millis(1)).await;
    }

    fn key_with(&self, code: KeyCode, modifiers: KeyModifiers) {
        self.event_tx
            .send(Event::Key(KeyEvent::new(code, modifiers)))
            .expect("bug: UI task dropped event channel");
    }

    fn key(&self, code: KeyCode) {
        self.key_with(code, KeyModifiers::NONE);
    }

    fn type_str(&self, text: &str) {
        for ch in text.chars() {
            self.key(KeyCode::Char(ch));
        }
    }

    fn screen(&self) -> String {
        let emu = self.emu.lock().expect("bug: emulator lock poisoned");
        render_screen(&**emu, false)
    }
}

/// Wait for the next model request. The generous timeout fails fast when the
/// pipeline is stuck (paused clock auto-advances) instead of hanging.
async fn next_request(ctrl: &mut MockModelController) -> rig::completion::CompletionRequest {
    tokio::time::timeout(Duration::from_secs(120), ctrl.next_request())
        .await
        .expect("timed out waiting for a model request")
}

/// True if any user message in the chat history contains `needle`.
fn history_contains_user_text(req: &rig::completion::CompletionRequest, needle: &str) -> bool {
    req.chat_history.iter().any(|m| {
        if let rig::message::Message::User { content } = m {
            content.iter().any(|c| {
                if let UserContent::Text(t) = c {
                    t.text.contains(needle)
                } else {
                    false
                }
            })
        } else {
            false
        }
    })
}

/// True if any tool result in the chat history contains `needle`.
fn history_contains_tool_result(req: &rig::completion::CompletionRequest, needle: &str) -> bool {
    req.chat_history.iter().any(|m| {
        if let rig::message::Message::User { content } = m {
            content.iter().any(|c| {
                if let UserContent::ToolResult(tr) = c {
                    tr.content.iter().any(|seg| {
                        if let rig::message::ToolResultContent::Text(t) = seg {
                            t.text.contains(needle)
                        } else {
                            false
                        }
                    })
                } else {
                    false
                }
            })
        } else {
            false
        }
    })
}

/// Happy path: create a session from the first user input, stream a
/// response, run a `set_title` tool round-trip, then a second turn, and
/// finally quit cleanly via Ctrl+C.
#[tokio::test(start_paused = true)]
async fn session_lifecycle_with_mock_daemon() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (h, mut ctrl) = E2eHarness::spawn(80, 20).await;
            assert_screen!("e2e_welcome", h.screen());

            // First input creates the session lazily, then flushes the text.
            h.type_str("hello daemon");
            h.key(KeyCode::Enter);
            h.settle().await;

            let req = next_request(&mut ctrl).await;
            assert!(
                history_contains_user_text(&req, "hello daemon"),
                "model request should contain the user's message"
            );

            ctrl.send_text("Hello! ");
            h.settle().await;
            assert_screen!("e2e_streaming", h.screen());

            ctrl.send_text("I am a mock model.");
            ctrl.finish();
            h.settle().await;
            assert_screen!("e2e_response_done", h.screen());

            // Second turn: tool-call round-trip that titles the session.
            h.type_str("please set a title");
            h.key(KeyCode::Enter);
            h.settle().await;

            let req = next_request(&mut ctrl).await;
            assert!(
                history_contains_user_text(&req, "please set a title"),
                "second turn should reach the model"
            );
            ctrl.send_tool_call(
                "tc-1",
                "set_title",
                serde_json::json!({"title": "Greeting session"}),
            );
            ctrl.finish();

            // The tool result comes back as a new model round.
            let req = next_request(&mut ctrl).await;
            assert!(
                history_contains_tool_result(&req, "Title set"),
                "follow-up request should contain the set_title tool result"
            );
            ctrl.send_text("Done, titled.");
            ctrl.finish();
            h.settle().await;
            assert_screen!("e2e_title_set", h.screen());

            // Quit: Ctrl+C soft-detaches; the agent is idle, so the client
            // exits (keep_running) and disconnects.
            h.key_with(KeyCode::Char('c'), KeyModifiers::CONTROL);
            h.settle().await;

            let result = tokio::time::timeout(Duration::from_secs(120), h.client)
                .await
                .expect("client should exit after Ctrl+C")
                .expect("client task should not panic");
            assert!(result.is_ok(), "run_client should exit cleanly: {result:?}");
        })
        .await;
}

/// While a completion is in flight the agent is not idle: Ctrl+C shows the
/// quit picker instead of exiting, and "Keep running" (first option) returns
/// to the session.
#[tokio::test(start_paused = true)]
async fn quit_picker_when_agent_busy() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (h, mut ctrl) = E2eHarness::spawn(80, 20).await;

            h.type_str("long task");
            h.key(KeyCode::Enter);
            h.settle().await;
            let _req = next_request(&mut ctrl).await;

            // Completion in flight → not idle → quit picker.
            h.key_with(KeyCode::Char('c'), KeyModifiers::CONTROL);
            h.settle().await;
            assert_screen!("e2e_quit_picker_busy", h.screen());

            // Escape returns to the input; the stream then completes.
            h.key(KeyCode::Esc);
            h.settle().await;
            ctrl.send_text("done");
            ctrl.finish();
            h.settle().await;
            assert_screen!("e2e_after_busy_finish", h.screen());
        })
        .await;
}

/// Reconnecting to a session whose model is mid-thinking must revive the
/// thinking spinner. Streamed reasoning is only committed to history once it
/// completes, so the daemon buffers the in-progress thinking, appends it to
/// the replayed history, and marks the replay `in_progress` (suppressing the
/// client's end-of-replay ResponseDone). Switching away and back with a
/// single client exercises the same detach/replay path as a fresh attach.
#[tokio::test(start_paused = true)]
async fn switch_back_mid_thinking_revives_spinner() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (h, mut ctrl) = E2eHarness::spawn(80, 20).await;

            h.type_str("think hard");
            h.key(KeyCode::Enter);
            h.settle().await;
            let req = next_request(&mut ctrl).await;
            assert!(
                history_contains_user_text(&req, "think hard"),
                "model request should contain the user's message"
            );

            // Stream reasoning deltas; the completion stays in flight.
            ctrl.send_chunk(rig::streaming::RawStreamingChoice::ReasoningDelta {
                id: None,
                reasoning: "Deep thought ".into(),
            });
            ctrl.send_chunk(rig::streaming::RawStreamingChoice::ReasoningDelta {
                id: None,
                reasoning: "in progress".into(),
            });
            h.settle().await;
            assert_screen!("e2e_mid_thinking_live", h.screen());

            // Switch away to a lazy new session (soft detach answers NotIdle
            // because the completion is still running)…
            h.type_str("/new");
            h.key(KeyCode::Enter);
            h.settle().await;

            // …and back via the session picker: the replay ends with the
            // buffered thinking and is marked in-progress, so the spinner
            // comes back alive instead of an idle prompt.
            h.type_str("/load");
            h.key(KeyCode::Enter);
            h.settle().await;
            h.key(KeyCode::Enter); // select the (only) running session
            h.settle().await;
            assert_screen!("e2e_mid_thinking_after_reconnect", h.screen());

            // The stream then completes live on the re-attached client: the
            // answer streams in and the spinner clears.
            ctrl.send_text("The answer.");
            ctrl.finish();
            h.settle().await;
            assert_screen!("e2e_mid_thinking_finished", h.screen());
        })
        .await;
}
