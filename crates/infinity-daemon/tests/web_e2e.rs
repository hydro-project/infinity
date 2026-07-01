//! End-to-end tests of the bundled web UI, driven through a real browser.
//!
//! The daemon's session machinery runs in-process with a deterministic mock
//! LLM provider (`rig-mock`), serving the bundled web assets and WebSocket
//! protocol on an OS-assigned port (so concurrent tests don't collide). A
//! headless Chromium instance driven by Playwright interacts with the UI,
//! while the test controls every model "response" through the mock's
//! controller.
//!
//! Rendering determinism: the browser context emulates
//! `prefers-reduced-motion: reduce`, which the UI honors (theme.css freezes
//! CSS animations/transitions; the canvas spinner renders a static frame),
//! session/thread ids come from a deterministic sequence, and screenshot
//! assertions additionally disable animations and retry until the page
//! settles.
//!
//! Requires the `e2e-web` feature: builds the web UI via npm (bundled-web)
//! and expects Playwright 1.60 browsers to be installed
//! (`npx playwright@1.60.0 install chromium`). Golden screenshots live in
//! `tests/web_snapshots/`; set `UPDATE_SNAPSHOTS=1` to (re)generate them.
//!
//! If the playwright-rs bundled driver is unusable — its download CDN has
//! been known to 404 at build time, and its bundled node binary needs a
//! newer glibc than some hosts have (browser launch fails with "Server
//! process exited immediately") — set `PLAYWRIGHT_SKIP_DRIVER_DOWNLOAD=1`,
//! `PLAYWRIGHT_NODE_EXE=$(command -v node)`, and
//! `PLAYWRIGHT_CLI_JS=/path/to/node_modules/playwright/cli.js` (from
//! `npm i playwright@1.60.0`) on the `cargo test` invocation — cargo
//! forwards them to both the build script and the test binary.
#![cfg(feature = "e2e-web")]

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use infinity_daemon::ids::SequentialIdSource;
use infinity_daemon::rap_callback;
use infinity_daemon::session::{SessionManager, SessionManagerConfig};
use infinity_daemon::ws_handler;
use infinity_provider_protocol::{ModelEntry, ModelProvider, SingleModelProvider};
use playwright_rs::{
    Animations, Browser, BrowserContext, Page, Playwright, ScreenshotAssertionOptions, expect,
    expect_page,
};
use rig_mock::{MockCompletionModel, MockModelController, mock_model};
use tokio::sync::Mutex;

type BoxError = Box<dyn std::error::Error + Send + Sync>;

// ── In-process daemon ────────────────────────────────────────────────────────

/// The daemon session machinery running in-process, serving the web UI and
/// WebSocket protocol on `port`.
struct TestDaemon {
    #[expect(dead_code, reason = "held so the manager outlives the test")]
    manager: Arc<Mutex<SessionManager>>,
    port: u16,
    /// Working directory for sessions created through the UI.
    cwd: tempfile::TempDir,
    _state_dir: tempfile::TempDir,
}

/// Boot the daemon parts (session manager with a mock provider, RAP callback
/// server, HTTP/WS listener) on temp state. Must run inside a `LocalSet`.
async fn start_daemon(model: MockCompletionModel) -> Result<TestDaemon, BoxError> {
    let entry = ModelEntry {
        model_id: "mock-model".to_owned(),
        display_name: "Mock Model".to_owned(),
        context_window: 100_000,
        max_output_tokens: None,
        supports_image_input: true,
    };
    start_daemon_with_providers(vec![(
        "mock".to_owned(),
        Arc::new(SingleModelProvider::new(entry, model)) as Arc<dyn ModelProvider>,
    )])
    .await
}

/// Like [`start_daemon`] but with an explicit provider list (in registration
/// order — the first model of the first provider is the default).
async fn start_daemon_with_providers(
    providers: Vec<(String, Arc<dyn ModelProvider>)>,
) -> Result<TestDaemon, BoxError> {
    let state_dir = tempfile::tempdir()?;
    let cwd = tempfile::tempdir()?;

    let (cb_listener, callback_url) = rap_client::callback_server::bind_callback_listener().await?;
    let manager = SessionManager::with_providers(
        SessionManagerConfig {
            state_dir: state_dir.path().to_path_buf(),
            callback_url,
            user_rap_config: None,
            // Deterministic session/thread ids keep rendered ids (sidebar,
            // thread badges) byte-identical across runs for screenshots.
            id_source: Arc::new(SequentialIdSource::new()),
        },
        providers,
        vec![],
    )
    .await?;
    let manager = rap_callback::serve_callbacks(cb_listener, manager);

    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await?;
    let port = listener.local_addr()?.port();
    tokio::task::spawn_local(ws_handler::serve(listener, manager.clone()));

    Ok(TestDaemon {
        manager,
        port,
        cwd,
        _state_dir: state_dir,
    })
}

// ── Browser harness ──────────────────────────────────────────────────────────

struct BrowserHarness {
    #[expect(dead_code, reason = "owns the driver process")]
    playwright: Playwright,
    browser: Browser,
    context: BrowserContext,
}

impl BrowserHarness {
    /// Launch headless Chromium with a deterministic context: fixed
    /// viewport, light theme, `prefers-reduced-motion: reduce`, en-US/UTC.
    async fn launch() -> Result<Self, BoxError> {
        let playwright = Playwright::launch().await?;
        let browser = playwright.chromium().launch().await?;
        let context = browser
            .new_context_with_options(
                playwright_rs::protocol::BrowserContextOptions::builder()
                    .viewport(playwright_rs::Viewport {
                        width: 1280,
                        height: 800,
                    })
                    .device_scale_factor(1.0)
                    .locale("en-US".to_owned())
                    .timezone_id("UTC".to_owned())
                    .reduced_motion("reduce".to_owned())
                    .build(),
            )
            .await?;
        // Fixed theme regardless of the host's color-scheme preference.
        context
            .add_init_script("window.localStorage.setItem('infinity-theme', 'light');")
            .await?;
        Ok(Self {
            playwright,
            browser,
            context,
        })
    }

    async fn open(&self, port: u16) -> Result<Page, BoxError> {
        let page = self.context.new_page().await?;
        page.goto(&format!("http://127.0.0.1:{port}/"), None)
            .await?;
        // Wait for the Google-Fonts webfonts before any assertions:
        // `display=swap` renders fallback fonts first, which would race
        // screenshots. `FontFaceSet::load` also *triggers* loads for
        // weights not yet used by the page, and resolves (never rejects)
        // even on network failure — so verify with `check()` afterwards
        // that the families really became available.
        let fonts_loaded: bool = page
            .evaluate::<(), bool>(
                r#"async () => {
                    const faces = [
                        "400 14px Inter",
                        "500 14px Inter",
                        "600 14px Inter",
                        "700 14px Inter",
                        "400 14px 'JetBrains Mono'",
                        "500 14px 'JetBrains Mono'",
                        "700 14px 'JetBrains Mono'",
                    ];
                    await Promise.all(faces.map((f) => document.fonts.load(f)));
                    await document.fonts.ready;
                    return faces.every((f) => document.fonts.check(f));
                }"#,
                None,
            )
            .await?;
        if !fonts_loaded {
            return Err("webfonts (Inter / JetBrains Mono) failed to load — \
                        is fonts.googleapis.com reachable?"
                .into());
        }
        Ok(page)
    }

    async fn close(self) -> Result<(), BoxError> {
        self.browser.close().await?;
        Ok(())
    }
}

// ── Screenshot snapshots ─────────────────────────────────────────────────────

fn snapshot_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("web_snapshots")
        .join(format!("{name}.png"))
}

/// Assert the page matches the golden screenshot `tests/web_snapshots/{name}.png`.
///
/// The comparison retries (with animations disabled and any volatile regions
/// masked) until it matches or times out — this absorbs any one-shot
/// third-party animation still settling. A small pixel tolerance covers
/// antialiasing drift. On mismatch, `{name}-actual.png` and
/// `{name}-diff.png` are written next to the golden for review.
///
/// Session/thread ids are deterministic ([`SequentialIdSource`]), so no
/// masking is needed by default; `mask_selectors` covers any per-snapshot
/// volatile regions.
///
/// When the golden does not exist it is written and the test fails, asking
/// for a human review — unless `UPDATE_SNAPSHOTS=1`, which accepts new and
/// changed goldens.
async fn assert_screenshot(
    page: &Page,
    name: &str,
    mask_selectors: &[&str],
) -> Result<(), BoxError> {
    let path = snapshot_path(name);
    let update = std::env::var("UPDATE_SNAPSHOTS").is_ok_and(|v| v == "1");
    let existed = path.exists();

    let mut mask = Vec::new();
    for selector in mask_selectors {
        mask.push(page.locator(selector).await);
    }

    let options = ScreenshotAssertionOptions::builder()
        // Strict comparison: reduced motion, deterministic ids, webfonts
        // (loaded before assertions), and SVG icons make renders
        // byte-stable — even the UA button font is pinned via per-class
        // `font-family`. If cross-platform rasterization drift ever
        // reappears, prefer fixing the source of nondeterminism over
        // adding tolerance; regenerate goldens with UPDATE_SNAPSHOTS=1.
        .max_diff_pixels(0)
        .animations(Animations::Disabled)
        .mask(mask)
        .update_snapshots(update)
        .build();

    expect_page(page)
        .with_timeout(Duration::from_secs(5))
        .to_have_screenshot(&path, Some(options))
        .await
        .map_err(|e| format!("screenshot '{name}' does not match golden: {e}"))?;

    if !existed && !update {
        return Err(format!(
            "golden screenshot '{}' did not exist and was created — review it \
             and re-run (or run with UPDATE_SNAPSHOTS=1 to accept in bulk)",
            path.display()
        )
        .into());
    }
    Ok(())
}

// ── Mock model helpers ───────────────────────────────────────────────────────

/// Wait for the next completion request with a timeout, so a wedged flow
/// fails the test instead of hanging it.
async fn next_request(
    ctrl: &mut MockModelController,
) -> Result<rig::completion::CompletionRequest, BoxError> {
    tokio::time::timeout(Duration::from_secs(30), ctrl.next_request())
        .await
        .map_err(|_| "timed out waiting for the UI to trigger a model request".into())
}

fn history_json(req: &rig::completion::CompletionRequest) -> String {
    serde_json::to_string(&req.chat_history).expect("bug: chat history should serialize")
}

// ── UI flows ─────────────────────────────────────────────────────────────────

/// Create a new local session through the sidebar picker, rooted at `cwd`.
async fn create_session_via_picker(page: &Page, cwd: &str) -> Result<(), BoxError> {
    expect(page.get_by_text("No sessions yet", false).await)
        .to_be_visible()
        .await?;
    page.get_by_text("+ New", true).await.click(None).await?;
    expect(page.get_by_text("New session on", true).await)
        .to_be_visible()
        .await?;
    page.get_by_text("local", true).await.click(None).await?;

    let cwd_input = page
        .get_by_placeholder("Working directory on local (Tab to complete)", true)
        .await;
    cwd_input.fill(cwd, None).await?;
    cwd_input.press("Enter", None).await?;
    Ok(())
}

/// The chat textarea (enabled once the session is connected).
async fn chat_input(page: &Page) -> playwright_rs::protocol::Locator {
    page.get_by_placeholder("Send a message…", true).await
}

async fn send_chat_message(page: &Page, text: &str) -> Result<(), BoxError> {
    let input = chat_input(page).await;
    expect(input.clone()).to_be_enabled().await?;
    input.fill(text, None).await?;
    input.press("Enter", None).await?;
    Ok(())
}

// ── Stub RAP image tool server (shared, see `rap-test-servers`) ──────────────

use rap_test_servers::{STUB_PNG_BASE64, start_stub_image_server, write_rap_config};

// ── Tests ────────────────────────────────────────────────────────────────────

/// Happy path: create a session, exchange messages with the (mock) model,
/// watch a tool call set the session title, and snapshot the stable states.
#[tokio::test]
async fn chat_round_trip_with_tool_call() -> Result<(), BoxError> {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (model, mut ctrl) = mock_model();
            let daemon = start_daemon(model).await?;
            let harness = BrowserHarness::launch().await?;
            let page = harness.open(daemon.port).await?;

            // ── Session creation ──
            create_session_via_picker(&page, &daemon.cwd.path().to_string_lossy()).await?;

            // ── First exchange: plain streaming text ──
            send_chat_message(&page, "Hello from the e2e test!").await?;
            let req = next_request(&mut ctrl).await?;
            assert!(
                history_json(&req).contains("Hello from the e2e test!"),
                "model request should contain the user's message"
            );
            ctrl.send_text("Hello! I am the ");
            ctrl.send_text("**mock** model.");
            ctrl.finish();

            // The streamed markdown renders as assistant text.
            expect(page.get_by_text("Hello! I am the mock model.", false).await)
                .to_be_visible()
                .await?;
            // The user's message is echoed into the transcript.
            expect(page.get_by_text("Hello from the e2e test!", false).await)
                .to_be_visible()
                .await?;

            // The sidebar item shows the deterministic session id prefix
            // until a tool call sets the title.
            assert_screenshot(&page, "chat-response", &[]).await?;

            // ── Second exchange: tool call sets the session title ──
            send_chat_message(&page, "Please title this session").await?;
            let _req = next_request(&mut ctrl).await?;
            ctrl.send_tool_call(
                "call-1",
                "set_title",
                serde_json::json!({"title": "Mock chat"}),
            );
            ctrl.finish();

            // The tool result comes back as a new model round.
            let req = next_request(&mut ctrl).await?;
            assert!(
                history_json(&req).contains("call-1"),
                "follow-up request should contain the tool result"
            );
            ctrl.send_text("Done — I titled the session.");
            ctrl.finish();

            expect(
                page.get_by_text("Done — I titled the session.", false)
                    .await,
            )
            .to_be_visible()
            .await?;
            // The sidebar shows the new title (SessionsUpdated broadcast).
            expect(page.get_by_text("Mock chat", true).await)
                .to_be_visible()
                .await?;

            assert_screenshot(&page, "chat-title-set", &[]).await?;

            harness.close().await?;
            Ok(())
        })
        .await
}

/// Reloading the page and reconnecting to the session replays the history.
#[tokio::test]
async fn reload_replays_history() -> Result<(), BoxError> {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (model, mut ctrl) = mock_model();
            let daemon = start_daemon(model).await?;
            let harness = BrowserHarness::launch().await?;
            let page = harness.open(daemon.port).await?;

            create_session_via_picker(&page, &daemon.cwd.path().to_string_lossy()).await?;
            send_chat_message(&page, "Remember this message").await?;
            let _req = next_request(&mut ctrl).await?;
            ctrl.send_tool_call(
                "call-title",
                "set_title",
                serde_json::json!({"title": "Replay me"}),
            );
            ctrl.finish();
            let _req = next_request(&mut ctrl).await?;
            ctrl.send_text("A reply worth replaying.");
            ctrl.finish();
            expect(page.get_by_text("A reply worth replaying.", false).await)
                .to_be_visible()
                .await?;

            // Reload: the in-memory client state is gone; the session shows
            // up in the sidebar (by its title) and clicking it replays the
            // transcript.
            page.reload(None).await?;
            let session_item = page.get_by_text("Replay me", true).await;
            expect(session_item.clone()).to_be_visible().await?;
            session_item.click(None).await?;

            expect(page.get_by_text("Remember this message", false).await)
                .to_be_visible()
                .await?;
            expect(page.get_by_text("A reply worth replaying.", false).await)
                .to_be_visible()
                .await?;

            harness.close().await?;
            Ok(())
        })
        .await
}

/// Reloading mid-thinking must revive the thinking state. Streamed reasoning
/// is only committed to history once it completes, so the daemon buffers the
/// in-progress thinking, appends it to the replayed history, and marks the
/// replay `in_progress` — the UI keeps the "Thinking…" spinner alive instead
/// of showing an idle input, and the live stream then finishes on the
/// reconnected page.
#[tokio::test]
async fn reload_mid_thinking_keeps_spinner() -> Result<(), BoxError> {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (model, mut ctrl) = mock_model();
            let daemon = start_daemon(model).await?;
            let harness = BrowserHarness::launch().await?;
            let page = harness.open(daemon.port).await?;

            create_session_via_picker(&page, &daemon.cwd.path().to_string_lossy()).await?;

            // First exchange titles the session so it can be found in the
            // sidebar after the reload.
            send_chat_message(&page, "Please title this session").await?;
            let _req = next_request(&mut ctrl).await?;
            ctrl.send_tool_call(
                "call-title",
                "set_title",
                serde_json::json!({"title": "Deep thinker"}),
            );
            ctrl.finish();
            let _req = next_request(&mut ctrl).await?;
            ctrl.send_text("Titled.");
            ctrl.finish();
            expect(page.get_by_text("Titled.", false).await)
                .to_be_visible()
                .await?;

            // Second turn: the model starts thinking and stays mid-thought
            // (reasoning deltas stream, the completion never finishes).
            send_chat_message(&page, "Think hard about this").await?;
            let _req = next_request(&mut ctrl).await?;
            ctrl.send_chunk(rig::streaming::RawStreamingChoice::ReasoningDelta {
                id: None,
                reasoning: "Pondering the imponderable".into(),
            });
            expect(page.get_by_text("Pondering the imponderable", false).await)
                .to_be_visible()
                .await?;
            assert_screenshot(&page, "mid-thinking-live", &[]).await?;

            // Reload mid-thinking and reconnect: the replayed history must
            // restore the in-progress thinking text and the live spinner.
            page.reload(None).await?;
            let session_item = page.get_by_text("Deep thinker", true).await;
            expect(session_item.clone()).to_be_visible().await?;
            session_item.click(None).await?;

            expect(page.get_by_text("Pondering the imponderable", false).await)
                .to_be_visible()
                .await?;
            expect(page.get_by_text("Thinking…", true).await)
                .to_be_visible()
                .await?;
            assert_screenshot(&page, "mid-thinking-reconnect", &[]).await?;

            // The stream then completes live on the reconnected page: the
            // answer arrives and the spinner clears.
            ctrl.send_text("The answer.");
            ctrl.finish();
            expect(page.get_by_text("The answer.", false).await)
                .to_be_visible()
                .await?;
            expect(page.get_by_text("Thinking…", true).await)
                .to_be_hidden()
                .await?;
            assert_screenshot(&page, "mid-thinking-finished", &[]).await?;

            harness.close().await?;
            Ok(())
        })
        .await
}

// ── Model switching ──────────────────────────────────────────────────────────

/// Named provider list as accepted by [`start_daemon_with_providers`].
type ProviderList = Vec<(String, Arc<dyn ModelProvider>)>;

/// Two mock providers ("mock"/Mock Model and "mock2"/Second Model) plus their
/// controllers, for tests that exercise model switching. The first provider's
/// model is the daemon default.
fn two_mock_providers() -> (ProviderList, MockModelController, MockModelController) {
    let (model1, ctrl1) = mock_model();
    let (model2, ctrl2) = mock_model();
    let providers = vec![
        (
            "mock".to_owned(),
            Arc::new(SingleModelProvider::new(
                ModelEntry {
                    model_id: "mock-model".to_owned(),
                    display_name: "Mock Model".to_owned(),
                    context_window: 100_000,
                    max_output_tokens: None,
                    supports_image_input: true,
                },
                model1,
            )) as Arc<dyn ModelProvider>,
        ),
        (
            "mock2".to_owned(),
            Arc::new(SingleModelProvider::new(
                ModelEntry {
                    model_id: "second-model".to_owned(),
                    display_name: "Second Model".to_owned(),
                    context_window: 200_000,
                    max_output_tokens: None,
                    supports_image_input: true,
                },
                model2,
            )) as Arc<dyn ModelProvider>,
        ),
    ];
    (providers, ctrl1, ctrl2)
}

/// Hovering the model pill drops down the list of available models; clicking
/// one switches the session's model mid-session: the pill and transcript
/// confirm the switch, and the next request goes to the new model's provider.
#[tokio::test]
async fn switch_model_mid_session() -> Result<(), BoxError> {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (providers, mut ctrl1, mut ctrl2) = two_mock_providers();
            let daemon = start_daemon_with_providers(providers).await?;
            let harness = BrowserHarness::launch().await?;
            let page = harness.open(daemon.port).await?;

            // ── First exchange runs on the default model (provider "mock") ──
            create_session_via_picker(&page, &daemon.cwd.path().to_string_lossy()).await?;
            send_chat_message(&page, "hello").await?;
            let req = next_request(&mut ctrl1).await?;
            assert!(
                history_json(&req).contains("hello"),
                "first request should reach the default model"
            );
            ctrl1.send_text("from model one");
            ctrl1.finish();
            expect(page.get_by_text("from model one", false).await)
                .to_be_visible()
                .await?;

            // ── Hover the pill: the dropdown lists both models ──
            let pill = page.get_by_test_id("model-pill").await;
            expect(pill.clone())
                .to_contain_text("mock: Mock Model")
                .await?;
            pill.hover(None).await?;
            expect(page.locator("[data-testid='model-option']").await)
                .to_have_count(2)
                .await?;
            expect(page.get_by_text("Second Model", true).await)
                .to_be_visible()
                .await?;
            expect(page.get_by_text("100k ctx", true).await)
                .to_be_visible()
                .await?;
            expect(page.get_by_text("200k ctx", true).await)
                .to_be_visible()
                .await?;
            // The active model is marked as selected.
            expect(
                page.locator("[data-testid='model-option'][data-selected]")
                    .await,
            )
            .to_contain_text("Mock Model")
            .await?;

            assert_screenshot(&page, "model-dropdown-open", &[]).await?;

            // ── Click the second model: the daemon confirms the switch ──
            page.get_by_text("Second Model", true)
                .await
                .click(None)
                .await?;
            expect(pill.clone())
                .to_contain_text("mock2: Second Model")
                .await?;
            expect(
                page.get_by_text("Switched model to mock2: Second Model", false)
                    .await,
            )
            .to_be_visible()
            .await?;

            // ── The next exchange runs on the new model (provider "mock2") ──
            send_chat_message(&page, "again").await?;
            let req = next_request(&mut ctrl2).await?;
            assert!(
                history_json(&req).contains("again"),
                "post-switch request should reach the second model"
            );
            assert!(
                ctrl1.try_next_request().is_none(),
                "the old model must not receive requests after the switch"
            );
            ctrl2.send_text("from model two");
            ctrl2.finish();
            expect(page.get_by_text("from model two", false).await)
                .to_be_visible()
                .await?;

            // Park the mouse so no hover styling leaks into the screenshot.
            page.mouse().move_to(0, 0, None).await?;
            assert_screenshot(&page, "model-switched", &[]).await?;

            harness.close().await?;
            Ok(())
        })
        .await
}

/// The model dropdown also works on a freshly loaded client with no session
/// selected: hovering the pill lists the models, and selecting one applies to
/// the next session created — the pill updates immediately and the new
/// session's first request goes to the chosen model's provider.
#[tokio::test]
async fn select_model_before_first_session() -> Result<(), BoxError> {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (providers, mut ctrl1, mut ctrl2) = two_mock_providers();
            let daemon = start_daemon_with_providers(providers).await?;
            let harness = BrowserHarness::launch().await?;
            let page = harness.open(daemon.port).await?;

            // ── Fresh client, no session: the pill shows the default model ──
            expect(page.get_by_text("No sessions yet", false).await)
                .to_be_visible()
                .await?;
            let pill = page.get_by_test_id("model-pill").await;
            expect(pill.clone())
                .to_contain_text("mock: Mock Model")
                .await?;

            // ── Hover the pill: the dropdown opens with both models, the
            // default marked as selected ──
            pill.hover(None).await?;
            expect(page.locator("[data-testid='model-option']").await)
                .to_have_count(2)
                .await?;
            expect(
                page.locator("[data-testid='model-option'][data-selected]")
                    .await,
            )
            .to_contain_text("Mock Model")
            .await?;

            assert_screenshot(&page, "model-dropdown-no-session", &[]).await?;

            // ── Select the second model: the pill reflects the choice
            // immediately (there is no session to confirm through) ──
            page.get_by_text("Second Model", true)
                .await
                .click(None)
                .await?;
            expect(pill.clone())
                .to_contain_text("mock2: Second Model")
                .await?;

            // ── Create a session: it boots on the selected model — the first
            // request reaches the second provider, never the default ──
            create_session_via_picker(&page, &daemon.cwd.path().to_string_lossy()).await?;
            send_chat_message(&page, "hello new session").await?;
            let req = next_request(&mut ctrl2).await?;
            assert!(
                history_json(&req).contains("hello new session"),
                "the new session's first request should reach the selected model"
            );
            assert!(
                ctrl1.try_next_request().is_none(),
                "the default model must not receive requests for the new session"
            );
            ctrl2.send_text("hello from model two");
            ctrl2.finish();
            expect(page.get_by_text("hello from model two", false).await)
                .to_be_visible()
                .await?;
            // After Connected, the pill still shows the selected model.
            expect(pill).to_contain_text("mock2: Second Model").await?;

            harness.close().await?;
            Ok(())
        })
        .await
}

/// A RAP tool returns an image: the transcript renders the image display
/// segment as an inline `<img>` (instead of the text fallback), and the
/// image content reaches the (image-capable) mock model.
#[tokio::test]
async fn image_tool_result_renders_inline() -> Result<(), BoxError> {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (model, mut ctrl) = mock_model();
            let daemon = start_daemon(model).await?;
            let stub_port = start_stub_image_server().await?;

            // Point sessions created in this cwd at the stub RAP server.
            write_rap_config(daemon.cwd.path(), stub_port)?;

            let harness = BrowserHarness::launch().await?;
            let page = harness.open(daemon.port).await?;

            create_session_via_picker(&page, &daemon.cwd.path().to_string_lossy()).await?;

            // The model answers with a call to the stub's image tool.
            send_chat_message(&page, "Show me the logo").await?;
            let _req = next_request(&mut ctrl).await?;
            ctrl.send_tool_call(
                "call-img",
                "read_image",
                serde_json::json!({"path": "logo.png"}),
            );
            ctrl.finish();

            // The stub's tool result comes back as a new model round with the
            // image content intact (the mock model declares image support).
            let req = next_request(&mut ctrl).await?;
            assert!(
                history_json(&req).contains(STUB_PNG_BASE64),
                "follow-up request should contain the base64 image tool-result content"
            );
            ctrl.send_text("A lovely indigo rectangle.");
            ctrl.finish();

            expect(page.get_by_text("A lovely indigo rectangle.", false).await)
                .to_be_visible()
                .await?;

            // The tool result renders as an inline image (the image display
            // segment wins over the text fallback).
            let img = page.get_by_test_id("tool-result-image").await;
            expect(img.clone()).to_be_visible().await?;
            let src = img
                .get_attribute("src")
                .await?
                .ok_or("tool result image should have a src")?;
            assert_eq!(src, format!("data:image/png;base64,{STUB_PNG_BASE64}"));

            assert_screenshot(&page, "chat-image-result", &[]).await?;

            harness.close().await?;
            Ok(())
        })
        .await
}

// ── WaitingForChoice test ────────────────────────────────────────────────────

use rap_test_servers::start_choice_server;

/// A session with a pending user choice shows the WaitingForChoice status dot
/// (pulsing orange) and sorts above idle sessions in the sidebar. The focused
/// session is a *different* one so the waiting session's highlight is visible
/// as an inactive sidebar item.
#[tokio::test]
async fn waiting_for_choice_status_in_sidebar() -> Result<(), BoxError> {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (model, mut ctrl) = mock_model();
            let daemon = start_daemon(model).await?;

            // Start a RAP server whose tool sends a user_choice callback.
            let rap_port = start_choice_server()
                .await
                .expect("start stub choice server");
            write_rap_config(daemon.cwd.path(), rap_port).expect("write rap.json");

            let harness = BrowserHarness::launch().await?;
            let page = harness.open(daemon.port).await?;

            // ── First session: will end up WaitingForChoice ──
            create_session_via_picker(&page, &daemon.cwd.path().to_string_lossy()).await?;
            send_chat_message(&page, "Do something that needs permission").await?;
            let _req = next_request(&mut ctrl).await?;
            ctrl.send_tool_call(
                "call-title",
                "set_title",
                serde_json::json!({"title": "Waiting session"}),
            );
            ctrl.finish();
            let _req = next_request(&mut ctrl).await?;
            // Call the tool that sends user_choice → WaitingForChoice.
            ctrl.send_tool_call(
                "call-perm",
                "ask_permission",
                serde_json::json!({"action": "deploy"}),
            );
            ctrl.finish();

            // Wait for the choice prompt to appear.
            expect(page.get_by_text("Allow \"deploy\"?", false).await)
                .to_be_visible()
                .await?;

            // ── Second session: create and switch to it ──
            page.get_by_text("+ New", true).await.click(None).await?;
            expect(page.get_by_text("New session on", true).await)
                .to_be_visible()
                .await?;
            page.get_by_text("local", true).await.click(None).await?;
            let cwd_input = page
                .get_by_placeholder("Working directory on local (Tab to complete)", true)
                .await;
            cwd_input
                .fill(&daemon.cwd.path().to_string_lossy(), None)
                .await?;
            cwd_input.press("Enter", None).await?;

            send_chat_message(&page, "Second session").await?;
            let _req = next_request(&mut ctrl).await?;
            ctrl.send_tool_call(
                "call-title2",
                "set_title",
                serde_json::json!({"title": "Active session"}),
            );
            ctrl.finish();
            let _req = next_request(&mut ctrl).await?;
            ctrl.send_text("Done.");
            ctrl.finish();
            expect(page.get_by_text("Active session", true).await)
                .to_be_visible()
                .await?;

            // The sidebar should show both sessions; the WaitingForChoice one
            // should sort to the top with a highlighted background/border
            // even though we are focused on the second session.
            let highlighted_item = page
                .locator("[class*='item'][data-status='WaitingForChoice']")
                .await;
            expect(highlighted_item).to_be_visible().await?;

            assert_screenshot(&page, "waiting-for-choice-sidebar", &[]).await?;

            harness.close().await?;
            Ok(())
        })
        .await
}
