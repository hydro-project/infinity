

## v0.1.0 (2026-09-03)

### Chore

 - <csr-id-e2e0719faebbffc72ec7bd8a8b3b02223da8ba0e/> add automated THIRD-PARTY file generation with license enforcement
   Generates a plaintext THIRD-PARTY attribution file combining:
   - Rust dependencies via `cargo-about` (grouped by license, full text)
   - npm dependencies from `infinity-ui/` and `agent/` via `license-checker`
   
   Both ecosystems enforce an allowlist of permissive licenses:
   - Rust (about.toml): Apache-2.0, MIT, ISC, BSD-3-Clause, CC0-1.0,
   CDLA-Permissive-2.0, MPL-2.0, Unicode-3.0, Zlib
   - npm (script): MIT, Apache-2.0, ISC, BSD-2-Clause, BSD-3-Clause, 0BSD,
   CC0-1.0, Unlicense, BlueOak-1.0.0, MPL-2.0
   
   The script fails if any dependency uses a license not in the allowlist.
   
   New files:
   - `about.toml` — cargo-about config
   - `about.hbs` — plaintext Handlebars template
   - `scripts/generate-third-party.sh` — orchestration script
   - `scripts/license-checker-format.json` — tells license-checker to include text
   - `THIRD-PARTY` — the generated file
 - <csr-id-b92b7a17f4b69e2652f5cce813320eca851717e4/> add workspace lints and fix all lint violations

### New Features

 - <csr-id-cbe6f1766a2d73aaad47a02cfe5dcc8ce063b0c0/> Show a placeholder in the session picker when there are no sessions
   Previously, opening the session picker (/load or Ctrl+L) with no sessions
   rendered a blank area with no indication of what was happening.
   
   - session_picker.rs: render a dim italic placeholder line ("No sessions to
   load — press esc to dismiss and start a new one.") when the session list
   is empty; reserve one visible row for it via visible_rows(); make Enter
   dismiss the empty picker instead of doing nothing.
   - terminal.rs: guard against an out-of-bounds panic when a sessions-updated
   notification arrives while the picker is open with an empty list (was
   indexing sessions[selected] unconditionally).
   - tests: new snapshot test empty_session_picker_shows_placeholder covering
   both the placeholder rendering and Enter-to-dismiss behavior.
 - <csr-id-1935c387d806a1da271e15078b26e06f228737c6/> multimodal (image) tool results end-to-end, with image display + review fixes
   Models can declare image input support, RAP tools can return images in tool
   results (both as model-facing content and as inline display segments), the
   sandbox `read_file` returns image files as image attachments, the web UI
   renders them inline, and the terminal falls back gracefully. Includes a
   shared test-server crate and mock-model / sandbox / TUI / web e2e coverage.
   
   ## Provider protocol (`infinity-provider-protocol`)
   
   * `ModelEntry` gains `supports_image_input: bool` (`#[serde(default)]`, so
   the remote provider socket protocol stays backward compatible).
   * Bedrock provider: all Claude models declare `supports_image_input: true`.
   * The capability is threaded from the resolved `ModelEntry` into
   `run_completion`/`process_batch` (rather than a trait method that re-lists
   models each turn): the daemon passes `catalog.find(&round_model)
   .supports_image_input` per round (following mid-session model switches);
   the Lambda resolves it once from `list_models()`.
   
   ## Agent core (`infinity-agent-core`)
   
   * `HistoryManager::get_history(supports_image_input)` replaces image
   tool-result content in place with `IMAGE_OMITTED_PLACEHOLDER` when the
   model can't accept images (no extra allocation pass). Images kept in
   history become visible again after switching to an image-capable model.
   
   ## RAP protocol (`rap-protocol`)
   
   * `RapToolResult` carries either `text` **or** structured `content`
   (`RapToolResultContent::{Text, Image{data, mediaType}}`, base64); `text` is
   now optional. When `content` is present it supersedes `text`.
   * New `DisplaySegment::Image(ImageContent { data, mediaType })` for
   human-facing UIs.
   * Spec docs (`tool-result.md`) and provider docs updated.
   
   ## Daemon (`infinity-daemon`)
   
   * RAP callbacks build the rig tool result from structured `content` when
   present (images → `ToolResultContent::Image`), else fall back to `text`.
   
   ## Sandbox (`sandbox-core` / `sandbox-local`)
   
   * `read_file` detects images by content (magic bytes, not extension) so
   mislabeled/extension-less files are classified correctly; returns a
   describing text plus base64 image content with `display_as: [image,
   text-summary]`. Tool output modeled as a named `ToolOutput` struct.
   
   ## Clients
   
   * Web (`infinity-ui`): `MessageItem` renders images as an inline bordered
   `<img>` (`data:` URL, `data-testid="tool-result-image"`).
   * TUI / ACP (`infinity-agent-cli`): renderers pick the first *supported*
   display segment; image-only results show `✓ [image — not displayable in
   terminal]`, otherwise the text summary.
   
   ## Shared test crate (`rap-test-servers`, unpublished)
   
   * `start_stub_image_server()` serves a `read_image` RAP tool returning a
   fixed indigo PNG; `write_rap_config(cwd, port)` points sessions at it.
   Dev-dependency of the CLI and daemon e2e suites.
   
   ## Tests
   
   * agent-core: image tool results reach image-capable models and are replaced
   with the placeholder otherwise.
   * daemon: RAP→rig content conversion (fallbacks, media types).
   * provider-protocol: remote transport round-trips `supports_image_input`.
   * sandbox-local: PNG content + display segments, content-based detection
   (PNG named `.txt`), text reads unchanged.
   * TUI e2e: image content reaches the model; terminal renders the text
   fallback (insta snapshot).
   * Web e2e: follow-up request carries the base64 image; transcript renders the
   inline `<img>`; `chat-image-result.png` golden.
 - <csr-id-440752df0e987e0eaaf7adda2dfaf23f9a2955db/> add `infinity daemon restart` and post-update daemon check
   ## `infinity daemon restart`
   
   * New `DaemonCommands::Restart` subcommand backed by `daemon_client::restart_daemon()`:
   stops the running daemon (if any), then launches a fresh instance via the existing
   `launch_daemon()` readiness handshake; starts one directly if none was running
   
   ## Shared, blocking stop logic
   
   * New `daemon_client::running_daemon_pid()`: reads the pid file and probes liveness
   with signal 0, so stale pid files (e.g. after a crash) are treated as not running
   * New async `daemon_client::stop_daemon()`: sends SIGTERM and waits (up to 30s) for
   the process to fully exit, so its shutdown cleanup (socket/pid file removal) cannot
   race a subsequently launched daemon; returns `Ok(Some(pid))` or `Ok(None)` when
   not running
   * `infinity daemon stop` and `restart_daemon()` both use it — the hand-rolled `nix`
   kill code is gone from `main.rs`, and `daemon stop` now reports
   "daemon stopped (pid N)" only once the daemon is actually gone
   
   ## Post-update daemon check
   
   * New `install::ensure_fresh_daemon()` runs after `infinity update`,
   `infinity rap update`, and `infinity provider update` (all update binaries the
   daemon executes or spawns):
   * daemon not running → boots it, so the freshly installed version is live
   * daemon running → prints a yellow warning that it's still on the previous
   version and to run `infinity daemon restart`
   * `launch_daemon()` now resolves its executable via `daemon_exe_path()`, stripping
   the Linux `/proc/self/exe` " (deleted)" suffix — after a self-update the old
   process would otherwise try to re-exec its replaced inode's path; this ensures
   the freshly installed binary is spawned. Made `pub` for reuse from `install.rs`.
   
   Verified with `./check.bash` (all checks pass, including web e2e).
 - <csr-id-1c4f71a611507dc7575c20b724faef680cbde2c7/> mid-session model switching per thread, with TUI + desktop UI and e2e tests
 - <csr-id-4368bde7e4be240e52932067702ccad333c17a08/> show provider ID in CLI model picker
   Added provider_id as a left column (10 chars wide) in the model picker.
   Updated snapshots to match.
   
   Row format: ` {provider_id:<10} {display_name:<24} {ctx:>10}`
 - <csr-id-66ddd8ff3797df0284b0658382249133361b55d9/> add Claude Fable 5 to Bedrock models list
 - <csr-id-a20554d63a64440f1ac7d2aa810697e035712832/> pass CLI-selected model to new sessions
 - <csr-id-c308202b8b11a8092399eefbf7c087ddab11971a/> /stop pauses agent instead of resetting session
   Changed `/stop` to preserve the session identity so the next user input
   resumes the same session instead of creating a new one.
   
   * `terminal.rs`: Don't clear `thread_id` or `total_tokens_used` on `/stop`.
   Updated the status message to hint that the session can be resumed.
   * `daemon_client.rs`: When `/stop` fires (`shut_down_old=true, maybe_target=None`),
   keep `active_session` set so the next `UserInput` routes to the same session.
   The daemon's `send_input()` already handles restarting shut-down sessions
   (re-boots RAP servers, starts new agent loop, re-attaches client).
 - <csr-id-dba5f88e924e5bf8f2d512b6ce6485230c56110f/> support prefix matching for --session arg
   The `--session` flag now uses prefix matching instead of requiring an exact
   session ID. If exactly one session ID starts with the given prefix, it connects
   to that session. If zero match, it errors with "no session found matching
   prefix". If multiple match, it errors with "ambiguous session prefix" and lists
   the matching IDs.
 - <csr-id-5bccd1dd693778407114f7af182e7339ab0b8420/> add /archive slash command
   Wire up the `ArchiveSession` protocol message (added in the previous commit)
   as a CLI slash command:
   
   * Add `/archive` (alias `/a`) to the autocomplete hints and help text
   * Parse the command from both Enter input and slash-command matching
   * In the terminal handler, reset local UI state (tokens, thread buffers,
   spinner) and send the `__archive__` sentinel through the input channel
   * In `daemon_client.rs`, intercept `__archive__` and send
   `ClientMessage::ArchiveSession`, then clear `active_session` so the
   CLI returns to the idle/new-session state
 - <csr-id-59d331491087ef43aa3cea9215a94c2089675b30/> show choice picker alongside input and cancel choices on tool interruption
   ## Choice picker coexists with text input
   
   Previously, a pending user choice replaced the text input entirely in both
   the CLI and web UI. Now the choice picker renders above the input so users
   can still type while a choice is visible.
   
   ### CLI
   
   - Removed `UiMode::ChoicePicker`; choice state is tracked via
   `UiMode::Normal { choice_focused: bool }`.
   - `draw_viewport` renders both choice picker and text input when a choice
   is active (choice above, input below).
   - Arrow-key focus transitions: Down past last choice → input, Up at top
   of input → choice picker. Cursor only shown when input is focused.
   
   ### Web UI
   
   - `MessageList` always renders `InputBar`; renders `ChoicePicker` above it
   when a pending choice exists.
   - `ChoicePicker` calls `onFocusInput` when ArrowDown is pressed at the
   last choice, shifting focus to the textarea.
   - Both components converted to `forwardRef` to support programmatic focus.
   
   ## Cancel pending choices on tool call interruption
   
   When a user sends input while a tool-initiated choice is pending, the tool
   call is interrupted but the choice was left dangling in the UI.
   
   - `batch_processor`: After notifying RAP servers of interrupted tool calls,
   emit `UserChoiceComplete` for each interrupted ID to dismiss associated
   choices.
   - `thread_worker`: Handle `UserChoiceComplete` in the display event
   forwarder by removing from `pending_choices` in the memory store.
 - <csr-id-4059c548b91070d2885215957ec45e381fce7565/> add `--session` flag to connect to a session by name or ID
   - Add `-s`/`--session` CLI argument to `Cli` struct that accepts a session
   name (title substring, case-insensitive) or exact session ID
   - In `run_client`, resolve the `--session` target against the Welcome
   message's session list: exact ID match first, then case-insensitive
   title substring match. Errors on zero matches or ambiguous matches.
   - Auto-sends `ClientMessage::Connect` when a session is resolved, so the
   TUI opens directly into that session
   - Thread the `session` parameter through `run_with_daemon`, `run_in_memory`,
   `run_client`, and `run_direct`
   - Update `run_headless` to print the reconnect command:
   `To connect: infinity --session '<session_id>'`
 - <csr-id-b5d3cac9bd75ca35d65d77db97fbe6da548bf0f3/> add -H/--headless flag, rename -m to -i/--initial-message
   - Rename `-m/--message` to `-i/--initial-message` for the TUI initial message
   - Add `-H/--headless <MESSAGE>` that sends a task to the daemon and exits
   without opening the TUI
   - New `run_headless()` in `daemon_client.rs` does the minimal protocol
   handshake: connect → Welcome → CreateSession → Connected → UserInput →
   wait for StartOutput → Disconnect
   - Waits for `StartOutput` before disconnecting so the agent is in
   `active_threads` and won't be killed by the daemon's idle check
   - Initialization errors are surfaced to the user before exit
 - <csr-id-b026adc49c8997cff35b88d181b202b0209ca477/> accept Ctrl+C as cancel in TUI menu popups
   Added an `is_cancel` helper to `component.rs` that recognizes both Esc and
   Ctrl+C as cancel gestures, and wired it into the picker popups:
   
   * choice_picker, model_picker, session_picker: Ctrl+C now cancels/dismisses
   the popup, same as Esc, using the shared `is_cancel` helper.
   * quit_picker: reordered options so "Continue running agent in background" is
   the default (first) selection. Ctrl+C directly selects KeepRunning, while
   Esc still cancels the picker — these are handled as separate match arms
   since they have different behavior.
 - <csr-id-bc19ab28e1ff552a4992812b75ffefd796973948/> serve web UI from daemon with `bundled-web` feature
 - <csr-id-0297d743512c02edd25a8ede1ee551ea65d878dc/> add directory tab completion in session picker
 - <csr-id-ea42de747adccf033b0be82f6d5131bd68e6408d/> require explicit `ssh` keyword after `--` in `remote add`
 - <csr-id-947b37af6289db10485ee7e0a4267333edc4bcef/> new session button uses location picker instead of local-only CWD picker
 - <csr-id-4169bdceccae28a77d664b9942758651defe8a0b/> add UserChoiceComplete daemon-to-client message
 - <csr-id-7085405bbfa8d07f6a69bc0e418761a56d108a67/> add RAP view_update protocol + diff view in web UI
 - <csr-id-ba10ffd62644a4c86c31a7fb6d5eaaca8c403b55/> add remote host migration UI and daemon orchestration
 - <csr-id-34145d9927397efd29a50a41f7839e965c9c6475/> show current session in session switcher UI
 - <csr-id-73708c07ed08acfd388bdf26654e71f9ab3184bd/> use user.name/email with fallback

### Bug Fixes

 - <csr-id-d5d3defcb1df5e0b88d466566e621cbffcb5f411/> don't eat scrollback when a resize races the re-anchor cursor query
   When the terminal is resized continuously (e.g. dragging a pane divider in
   Zed), the terminal keeps reflowing while the viewport's re-anchor cursor
   query (`CSI 6n`) is in flight. Acting on the stale reply re-saved the anchor
   too high (a growing reflow pulls scrollback rows down, moving the true anchor
   with them) and the subsequent clear-from-anchor-down erased the tail of the
   assistant output.
   
   * `InlineViewport::re_anchor` now re-checks the terminal size after every
   cursor-query round-trip and, if the geometry moved again, absorbs the new
   resize and re-queries until stable
   * `InlineViewport::draw` runs `re_anchor` before computing any
   geometry-dependent values (frame area, ideal viewport position), so a draw
   that absorbs a raced resize is fully consistent with the newest geometry
   * Regression test `tui_resize_race.rs` reproduces the race with a `TermOut`
   interposer that resizes the emulator right after answering the cursor
   query (before the fix, 6 of 30 output lines were erased)
   * Test harness: `TuiHarness::spawn_with_term` allows spawning the TUI
   against a caller-provided `TermOut` and event channel
 - <csr-id-1b20fdac512ea534ee24006b95903f3961ff5179/> add `keeps_session_alive` flag to prevent non-interactive clients from blocking idle shutdown
   Add a `keeps_session_alive` boolean field to `ClientMessage::CreateSession` and
   `ClientMessage::Connect` (defaulting to `true` via serde for backward compat).
   Connections that set this to `false` are tracked but do not prevent the session
   from idling out — enabling persistent but passive client connections (e.g. bots)
   that should not keep sessions warm indefinitely.
   
   Key changes:
   
   * `infinity-protocol`: new `keeps_session_alive` field on `CreateSession` and
   `Connect`, with `#[serde(default = "default_true")]`.
   
   * `client_handler.rs`: tracks `connection_keeps_alive` per connection and
   threads it through `attach_client` and `send_input`.
   
   * `session/thread_worker.rs`: replaces bare `UnboundedSender` subscriber lists
   with a `Subscriber` struct carrying `tx` + `keeps_session_alive`. Idle-exit
   and has-clients checks now only consider keep-alive subscribers.
   
   * `infinity-agent-cli`: all existing call sites pass `keeps_session_alive: true`
   (preserving current behavior for interactive CLI clients).
   
   * `tests/keep_alive.rs`: integration tests covering non-keep-alive idle exit
   and keep-alive warmth.
 - <csr-id-a536c9fa6d51bd2eaf0d5cf88af237ea1cce0e65/> reset context usage when compaction is applied so it does not re-trigger
 - <csr-id-45f198eec86493474ba48f812f397a4dbc113321/> strip ANSI escapes from external text instead of panicking
   The inline viewport's `OutputTracker` panics on any escape sequence it
   doesn't understand, so tool results containing ANSI codes (e.g. `cat`ing a
   file with captured TUI output) crashed the CLI with
   `bug: print_above wrote untracked escape sequence ESC ']'`.
   
   * Add `sanitize::strip_ansi`, a small VT-parser-shaped state machine that
   removes CSI sequences, OSC/DCS/SOS/PM/APC strings (BEL- or ST-terminated),
   all other escape sequences, and non-whitespace control characters (keeps
   `\t`, `\n`, `\r`), returning `Cow::Borrowed` for the common clean case.
   * Proactively sanitize all externally sourced text in `terminal.rs` before
   it reaches the viewport: tool results (text and diff segments), tool call
   display text, model text/thinking chunks, info lines, subscription events,
   and user input; also sanitize cargo output lines in `install.rs` (which
   carry colors when `CARGO_TERM_COLOR=always` is inherited).
   * Tighten `OutputTracker` to exactly what the TUI itself emits through
   `print_above`: CSI parameter bytes are now restricted to digits and `;`
   with `m` (SGR) as the only accepted final byte — anything else still
   panics as a bug, per the "internal output only" contract.
   * Add unit tests for `strip_ansi` and a `tui_flow_snapshots` regression test
   feeding OSC/SGR/cursor-movement/DCS sequences and stray control bytes
   through a tool result.
 - <csr-id-78bd1b5524346a9cc786cdf12f0e6e7a0b2ea085/> count deferred soft wraps in the inline viewport's output tracker
   Fixes TUI corruption where, after submitting a long (wrapping) input while
   the agent was actively thinking, the input box was drawn one row too low —
   its background leaked into the row below and the status bar was painted on
   the terminal's bottom row on top of the box background.
   
   ## Root cause
 - <csr-id-7a6e9715a7b602d0a04bc527a3c76f4c6a1ccd80/> replay in-progress thinking and response state to clients attaching mid-response
   A client connecting to a running agent got a history `Replay` that always ended
   in an idle state: streamed reasoning is only committed to history once complete,
   and the CLI unconditionally appended an end-of-replay `ResponseDone` marker that
   cleared the spinner. The client would appear idle even though the model was
   actively thinking.
   
   ## Daemon
   
   * `thread_worker` keeps the in-progress thinking text in a per-worker
   `Rc<RefCell<Option<String>>>`: the display-event forwarder accumulates
   `ThinkingChunk`s and clears on anything that moves the stream past the chain
   (thinking end, text, tool call/result, response done)
   * On subscribe, the replayed history is extended with `ThinkingStart` +
   `ThinkingChunk` for the buffered thinking, so clients recompute a live
   "thinking" state from the end of the replay
   * `DaemonMessage::Replay` gains `#[serde(default)] in_progress: bool` — true iff
   a completion is currently in flight. A pending async tool result is *not*
   flagged; clients already derive "waiting for tool result" from the trailing
   unresolved `ToolCall` in the history
   * Dead-session replays (`attach_client`) send `in_progress: false`; remote
   message prefixing passes the field through
   
   ## Clients
   
   * CLI `daemon_client` only synthesizes the end-of-replay `ResponseDone` marker
   when `!in_progress`, so the spinner state implied by the end of the history
   stays live; `ResponseDone` continues to preserve `WaitingToolCall` for
   trailing tool calls
   * Terminal: `ThinkingStart` now sets the spinner to `Thinking` unconditionally
   (replays have no preceding `StartOutput`)
   * Web: end of replay mirrors a live `ResponseDone` (`tool` survives, everything
   else clears) instead of always clearing, and skips the implicit done entirely
   when `in_progress`; `ToolResult` now switches the spinner back to thinking
   like the terminal does
   
   ## Tests
   
   * `thread_worker` unit tests: subscribing mid-thinking yields a replay ending
   with the in-progress thinking and `in_progress: true`; after the chain closes
   no stale thinking is replayed; waiting-for-tool-result replays have
   `in_progress: false` with the trailing unresolved `ToolCall` last in history
   * e2e TUI (`e2e_daemon_tui`): `switch_back_mid_thinking_revives_spinner` runs
   the real daemon + TUI client, streams reasoning deltas mid-completion,
   switches away (`/new`) and back (`/load`) with a single client, and snapshots
   the live, post-reconnect (spinner revived with buffered thought), and
   finished screens
   * e2e web (`web_e2e`, Playwright): `reload_mid_thinking_keeps_spinner` reloads
   the page mid-thinking, reconnects, and asserts the thinking text and
   "Thinking…" spinner are restored and clear once the stream finishes live —
   with golden screenshots of the live, reconnected, and finished states
 - <csr-id-8ad86d850d761c58669dffb906ef389654e4990d/> increase LengthDelimitedCodec max frame size to 256 MiB
   The "Failed to send daemon message to client: frame size too big" error
   occurs because `LengthDelimitedCodec::new()` defaults to an 8 MiB max
   frame length. When a DaemonMessage exceeds this (e.g. replaying a long
   conversation, large tool outputs, or file contents), the codec rejects
   it.
 - <csr-id-a9594657afe733d50c9cecb61ac29f8066faaf01/> render subscription event body in gray instead of orange
   * Keep the `⚡name:` label orange (`Color::Indexed(208)`) for visual distinction
   * Single-line events: split into two spans (orange label + `DarkGray` body)
   * Multi-line events: continuation lines use `DarkGray` instead of orange
   * Matches the style used for tool call result bodies
 - <csr-id-b7a980585d981b1ae22f1bb4fad12b739202b524/> use total_tokens for context usage and compaction trigger
   With Bedrock prompt caching, `input_tokens` only reflects uncached (new) input
   tokens. This caused two bugs:
   
   1. **Web UI context percentage showed ~1%** even at 800k tokens: the
   `TokenUsage` protocol struct lacked `total_tokens`, so the web UI computed
   `input_tokens + output_tokens` which only captured the small uncached
   portion.
   
   2. **Auto-compaction never triggered**: the compaction check compared
   `input_tokens` (uncached only) against 75% of the context window, so it
   never fired when most input was cached.
 - <csr-id-ba08e40ae829ed59bd2d08f1b986ce4d7b1e71e3/> fix test compilation after merge
   * Added missing `provider_id: "mock".to_owned()` field to the
   `SessionChanged` initializer in `session_loaded_updates_model_name` test
   * Updated the corresponding insta snapshot to match the new status bar format
   (`session_id | /help for commands` left, `provider: model · N% context` right)
 - <csr-id-c49fc7d0e78a22c2f0f8f6c84878e4e6a3dcfe35/> context usage resets to zero after session replay
   ## Root cause
   
   When loading a session, the CLI sets the context counter from the
   `Connected` message (`total_tokens_used` from session info). But after
   forwarding the replayed history, `daemon_client` appended a synthesized
   `ResponseDone(Some(DaemonTokenUsage(None)))` marker, and the terminal's
   `ResponseDone` handler did:
   
   ```rust
   if let Some(r) = r {
   total_tokens_used = r.token_usage().map_or(0, ...);
   }
   ```
   
   Since the marker carries no usage, `map_or(0, ...)` reset the counter to
   zero right after the replay. (Not a daemon or Bedrock bug — though the
   same pattern existed daemon-side, see below.)
   
   ## Changes
   
   * `terminal.rs`: only update `total_tokens_used` when the response
   actually reports usage; a usage-less `ResponseDone` (post-replay
   marker, or a provider that omits usage metadata) keeps the last known
   value instead of zeroing it.
   * `daemon_client.rs`: send the end-of-replay marker as
   `ResponseDone(None)` instead of wrapping an empty usage, making the
   "no usage info" intent explicit.
   * `infinity-daemon/thread_worker.rs` (defensive, same bug class): the
   display-event forwarder no longer persists `total_tokens_used = 0`
   when a response has no usage metadata (e.g. a Bedrock stream that ends
   without a usage `Metadata` event); `last_updated` is still refreshed.
   
   ## Tests
   
   * New snapshot test `replay_keeps_context_usage` modeling the real load
   flow: `SessionChanged` with 42k tokens → replayed history →
   `ResponseDone(None)` → `ResponseDone` with `usage: None`. Status bar
   must still show `42% context used` (previously showed `0%`).
 - <csr-id-bb92400080d4b85fcc34a51b6a54b8959cabade3/> update displayed model when loading an existing session
   The daemon's `Connected` message already carries the resolved `model_name` and
   `context_window` for the session's root thread (resolved via
   `get_thread_model` in `build_connected`), but the CLI client dropped both
   fields when translating it into the terminal's `SessionChanged` event. As a
   result, the TUI status bar kept showing the previous/default model after
   loading an existing session, and the token-usage gauge used the wrong context
   window.
   
   * Add `model_name` and `context_window` to `terminal::SessionChanged`
   * Forward both fields from `DaemonMessage::Connected` in `daemon_client.rs`
   * Apply them to the terminal's `model_name` / `context_window` state when a
   session change arrives, so the status bar and context gauge reflect the
   loaded session's model
 - <csr-id-cdcf0933658551227caa38def229e734fb0b0e42/> fix all 34 snapshot-documented TUI bugs (reflow, wide chars, spinner state, tiny terminals)
   Fixes every `// BUG:` annotated in the snapshot suite from the previous
   commit, healing all baselines (53 tests, +1 new). Round-trip discipline got
   *stricter*: the cursor is now queried exactly once per resize and never
   otherwise (the old code queried on every viewport height change). All
   rendering remains diff-based and clear+repaints are wrapped in synchronized
   updates — no new flicker — and redraws never emit LF, so they can't push the
   screen.
   
   ## Inline viewport: reflow-safe anchoring (`inline_viewport.rs`)
   
   The root cause of the ~17 resize-corruption bugs: reflowing terminals
   (kitty/alacritty/VTE) translate the **live** cursor through a resize reflow
   but merely *clamp* the DECSC saved cursor that the viewport anchored
   everything on. The viewport now re-derives its anchor from the live cursor:
   
   * hidden cursors are *parked on the anchor*, so the terminal itself tracks
   the anchor through reflows for free;
   * with the cursor shown in the input box, the anchor is reconstructed by
   computing how many rows the previously drawn viewport rows rewrap to at
   the new width (occupied lengths captured from the last frame, matching
   alacritty's cell-emptiness rules);
   * non-reflowing terminals (xterm-class) are detected by the queried cursor
   matching its old position exactly, keeping the old anchor; the ambiguous
   width-grow case resolves in favor of reflow (cosmetic gap on xterm vs.
   data loss on kitty).
   * The anchor's mid-line column (streaming) is recomputed for any width from
   a tracked logical-line length: `print_above` buffers its output and parses
   it (`OutputTracker`: CR/LF/tabs, SGR, display widths, auto-wrap), panicking
   on any escape sequence it doesn't know how to track rather than silently
   desynchronizing.
   * `print_above` checks `size()` (cheap ioctl, no round trip) on every print,
   repairing the anchor before printing — fixes prints racing the Resize
   event eating lines or corrupting scrollback.
   * After a resize the viewport stays pinned to the bottom: a later reflow can
   then push at most *blank* gap rows into scrollback — never stale viewport
   rows — and prints consume the gap (the anchor advances into it) before any
   scrolling resumes. The new `repeated_resizes_do_not_accumulate` test
   asserts both properties through a double width-shrink over wrapped
   scrollback and vertical shrink/grow cycles. (The alternative — drawing
   tight under the anchor after resizes, as the old code did — was probed and
   rejected: it leaks stale border rows into scrollback during resize storms.)
   * Clear + full repaints (resize, viewport height changes) are wrapped in
   `CSI ?2026` synchronized updates so supporting terminals apply them
   atomically; unsupporting terminals ignore the markers.
   * Degenerate scroll regions (`CSI 1;1r` on tiny terminals) are skipped and
   followed by a repaint instead of being silently ignored by the terminal.
   * Viewport height is clamped to rows−1 (the anchor needs a row above).
   
   ## Terminal UI logic (`terminal.rs`)
   
   * `ToolResult` clears "waiting for tool call result" once all pending root
   tool calls (counted) have resolved and switches the spinner back to
   Thinking — the model continues its turn immediately; `StartOutput`
   preserves the Thinking state instead of demoting it to LoadingContext.
   * Session-change and sessions-updated arms now redraw (status/% context,
   cursor restore, live session-picker list refresh).
   * Switching to a lazily-created session (and `/archive`) resets the terminal
   title left over from the previous session.
   * Multi-line `Info` and OAuth messages print line-by-line (ratatui `Line`
   drops `\n`; raw LF staircases in raw mode).
   * `/help` box clamps to the terminal width and truncates rows instead of
   wrapping into scroll-region corruption.
   * Status row measures display width (emoji/CJK), keeps the right side whole,
   and truncates the left with an ellipsis and a guaranteed 2-space gap.
   * `wrap_tail` (thread rows, thinking text) trims by display width so the
   freshest CJK/emoji text stays visible.
   * Tiny terminals shed gap → spinner → border rows before squeezing the
   input/status rows.
   
   ## Text input (`text_input.rs`)
   
   Width-aware wrapping, cursor math, and rendering: wide chars occupy two
   columns (no more swallowed characters after emoji), cursor lands on the
   correct column.
 - <csr-id-eb9404f66f01aadc31270571e4e0008ad59ea234/> log communication failures instead of swallowing
 - <csr-id-698ff544df0bea23b3087794dd869976368a8528/> remove easy-to-trigger compaction shortcuts from CLI
   Removed the Ctrl+K keyboard shortcut and the /k slash command alias for
   compaction to prevent accidental destructive compaction. Only the full
   `/compact` slash command remains.
   
   Changes in crates/infinity-agent-cli/src/terminal.rs:
   - Removed `(KeyCode::Char('k'), m) if m.contains(KeyModifiers::CONTROL)` keybinding
   - Removed "Ctrl+K  Trigger compaction" from keyboard shortcuts help text
   - Changed `"/compact" | "/k"` match arm to just `"/compact"`
   - Updated slash commands help text from "/compact, /k" to "/compact"
 - <csr-id-9c44d23fad385e6f326d7cbdb957669cb1de8bb1/> add clap conflicts_with to prevent --headless with --local
   Added `conflicts_with = "local"` to the `--headless` arg in the Cli struct.
   Clap will now emit an error at parse time if both `--headless` and `--local`
   are provided, since headless mode requires the daemon which --local disables.
 - <csr-id-44fcca250a44029e36b49df6013a049d33bc985f/> log panics from fire-and-forget spawned tasks instead of silently swallowing them
 - <csr-id-b40442e37ac91b884f51fcabb018a3735bdf612f/> hanging caused by `sh -c` intercepting SIGINT, improved config error handling
   Fixes a (the?) hanging issue encountered on Ubuntu 24.04
 - <csr-id-4bf24625f33c47202209052bd7b743c775dbd1b7/> remove unused `find` and `has_sessions` methods from SessionStore
   Both methods were flagged as dead code. `has_sessions` is redundant
   since callers check `!sessions.is_empty()` directly, and `find` was
   never called anywhere in the codebase.

### Other

 - <csr-id-ffc27d0bf5d964a655fedab9460bf5017971e6b6/> set up cargo-smart-release release workflow (mirroring hydro)
   * chore: set up cargo-smart-release release workflow (mirroring hydro)
   
   Sets up the release tooling for this workspace following the same
   cargo-smart-release setup as hydro-project/hydro (per its RELEASING.md).
   
   * `.github/workflows/release.yml`: new manually-dispatched Release workflow,
   adapted from hydro's. Supports major/minor/patch/keep/auto bumps, optional
   pre-release ids, and a dry-run mode (execute unchecked). Uses the
   hydro-project-bot GitHub App token to push past branch protection, and the
   pinned hydro-project fork of cargo-smart-release (rev e6f3368337a0).
   * `RELEASING.md`: releasing guide adapted from hydro's, including which crates
   are published and why the others are not, plus an addendum explaining why
   `[patch.crates-io]` on `rig-bedrock` blocks publishing the bedrock provider.
   * Crate manifests, 14 publishable crates (rap-protocol, rap-client,
   rap-steering-server, rap-github-event-poller, infinity-protocol,
   infinity-provider-protocol, infinity-agent-core, infinity-mcp-bridge,
   infinity-rap-bridge, infinity-daemon, infinity-agent-cli, sandbox-core,
   sandbox-local, sandbox-remote):
   - `publish = true`, `description`, `documentation` (docs.rs), and
   `repository = { workspace = true }` (new `[workspace.package]` in the root
   `Cargo.toml`).
   - `version = "^0.1.0"` added to all intra-workspace path dependencies
   (including dev-deps between publishable crates), as required for
   publishing.
   - New empty `CHANGELOG.md` per crate so cargo-smart-release will generate
   and track changelogs.
   * `publish = false` added to crates that must not be published:
   - `rig-mock`, `rap-test-servers` (test-only; left as path-only dev-deps so
   cargo strips them at publish time),
   - `rig-bedrock-patched` (vendored fork of crates-io `rig-bedrock`),
   - `infinity-provider-bedrock` + `infinity-agent-lambda` (depend on the
   patched rig-bedrock; publishing would silently drop the patches),
   - `infinity-slack-bot` (deployment artifact).
 - <csr-id-ea6b62e7b00f2a6b7e7338fa12e60fb3a46bb012/> add GitHub Actions workflows for lints, tests, conventional commits, and docs
   Added four workflow/action files modeled after hydro-project/hydro:
   
   - `.github/actions/use-sccache/action.yml` — composite action enabling sccache
   with GHA cache backend for Rust compilation caching.
   
   - `.github/workflows/ci.yml` — runs on push to main, PRs, and manual dispatch.
   Two jobs: `lint` (fmt, clippy, license/THIRD-PARTY check via
   generate-third-party.sh) and `test` (cargo test). Both use sccache and
   skip-duplicate-actions. Installs libcap-dev and Node.js for the license checker.
   
   - `.github/workflows/conventional_commits.yml` — validates PR titles match
   conventional commit types (feat, fix, docs, refactor, perf, test, chore, ci,
   revert) using amannn/action-semantic-pull-request.
   
   - `.github/workflows/docs.yml` — builds the documentation site by installing
   infinity-ui deps then docs deps and running `npm run build` in docs/.
   Uses Node 22 (current LTS) since the docs SSG requires `navigator.userAgent`
   which was added in Node 21+.
   
   Also ran `cargo fmt --all` to fix minor formatting issues.

### Refactor

 - <csr-id-24fa6cbf5564d4df2297451bdc76c9619ec741fe/> drop "Using provider" info message, show provider_id in status displays
 - <csr-id-ff1b8ee3af55c3fa816d507a7634012d2cc1fdad/> rewrite ACP server with per-session daemon connections
   Restructure the ACP server to use one daemon socket connection per active
   session, plus a background control connection for session listing. This
   eliminates the Disconnect/Connect dance, active_session tracking, and
   race conditions from the previous single-connection design.
 - <csr-id-7634b823ad70378e666379a9a8e8a7935a06026f/> replace all .unwrap() with .expect() and fix clippy warnings
 - <csr-id-9757071818663cefb8e6a12438071d95000379a8/> add precheck script, lints
 - <csr-id-51406e4dfab243a4400027507f446862b26ce8d3/> extract rap-client crate and unify RAP protocol types
   - Unify all duplicate RAP protocol types into rap-protocol crate:
   RapInvocation (3 copies), ToolsetManifest/ToolDef, and callback
   types (RapToolResult, RapSubscriptionEvent, RapUserChoice) with
   new RapOAuth struct and RapCallback tagged enum
   
   - Create rap-client crate with HttpClient trait, ToolsetCache trait,
   SimpleHttpClient, InMemoryToolsetCache, ToolsetLoader, RapNotifier,
   and a generic callback server accepting an async closure
   
   - Update infinity-agent-core, infinity-daemon, infinity-agent-lambda,
   and infinity-agent-cli to import directly from rap-client and
   rap-protocol instead of local duplicates
   
   - Rewrite daemon callback server to wrap rap-client's generic server,
   routing callbacks directly to SessionManager without mpsc indirection
   
   - Fix Send lifetime error in send_input's async closure

### Style

 - <csr-id-1e1ae380ae017878c224f8633b0ea7c95993ec82/> collapse `if` blocks into match guards to satisfy clippy
   Only matters for newer rust releases--
   
   - `crates/infinity-agent-lambda/src/event_handler.rs`: Collapsed `if name != "sleep_until_event_or_input"` into a match guard on the `DisplayEvent::ToolCall` arm.
   - `crates/infinity-agent-cli/src/choice_picker.rs`: Collapsed `if self.selected > 0` and `if self.selected < self.choices.len().saturating_sub(1)` into match guards on `KeyCode::Up` and `KeyCode::Down`.
   - `crates/infinity-agent-cli/src/quit_picker.rs`: Collapsed `if self.selected > 0` and `if self.selected < 1` into match guards on `KeyCode::Up` and `KeyCode::Down`.
   
   All five `clippy::collapsible_match` warnings are resolved. Formatting, clippy, compilation, and all 82 tests pass.

### Test

 - <csr-id-646c8f3dfcbb352369e70022cab1292cbbc49384/> add deterministic e2e tests for TUI↔daemon and web UI (Playwright)
   ## Daemon: injectable providers & configurable paths
   
   * `SessionManager::with_providers(SessionManagerConfig, providers, processes)` —
   generic constructor taking explicit `(provider_id, provider)` pairs;
   `SessionManager::new` keeps the production defaults (spawn providers from
   `~/.infinity/providers.json`, user RAP config at `~/.infinity/rap.json`).
   * `SessionManagerConfig { state_dir, callback_url, user_rap_config, id_source }`
   reduces hardcoded paths: `user_rap_config: None` makes sessions hermetic.
   * `rap_callback::serve_callbacks(listener, manager)` — callback accept loop for a
   pre-built manager; `start_callback_server` now composes it.
   * `ws_handler::serve(listener, mgr)` — reusable HTTP/WS accept loop (used by
   `run_daemon` and by tests binding an OS-assigned port).
   * `boot_rap_servers` takes the user-level config path as a parameter.
   
   ## Deterministic ids (`infinity_daemon::ids::IdSource`)
   
   User-visible ids (session ids from `create_session`, thread ids from the
   conversation store) now come from an injectable `IdSource`. Production uses
   random v4 UUIDs; tests use `IdSource::sequential()`
   (`00000000-0000-4000-8000-000000000001`, …) so ids rendered in UIs are stable
   across runs — no snapshot redaction or screenshot masking needed.
   
   ## TUI ↔ daemon e2e (`crates/infinity-agent-cli/tests/e2e_daemon_tui.rs`)
   
   * `daemon_client` moved from the binary into the lib; `run_client` is now public
   and generic over `TermOut`/`EventSource` + explicit cwd (crossterm /
   `current_dir()` defaults stay in `run_with_daemon`/`run_in_memory`).
   * Single-process test on a paused-clock current-thread runtime: real
   `SessionManager` (mock rig provider) + `handle_client_channels` + real
   `run_client` rendered into a vt100 virtual terminal. Covers lazy session
   creation from first input, streamed chunks, `set_title` tool round-trip
   (terminal title), multi-turn, quit-picker-when-busy, and clean Ctrl+C
   shutdown, with 6 insta screen snapshots (fully deterministic, no filters).
   
   ## Web UI e2e (`crates/infinity-daemon/tests/web_e2e.rs`, feature `e2e-web`)
   
   * New `e2e-web` feature = `bundled-web` + optional `playwright-rs` dep (optional
   regular dep because Cargo has no optional dev-dependencies; test gated with
   `#![cfg(feature = "e2e-web")]` so plain builds/tests are unaffected).
   * In-process daemon on an OS-assigned port (concurrent-test safe) + headless
   Chromium via playwright-rs with a deterministic context (1280×800, light
   theme, `prefers-reduced-motion: reduce`, en-US/UTC). The harness waits for
   the Google-Fonts webfonts (`document.fonts.load` + `check`) before any
   assertions so screenshots never race font loading. Tests:
   `chat_round_trip_with_tool_call` and `reload_replays_history`.
   * Screenshot goldens in `tests/web_snapshots/` via playwright's screenshot
   assertions: animations disabled, auto-retry, `max_diff_pixels(25)` (absorbs
   the sub-pixel text rasterization drift observed between this host and
   GitHub's ubuntu runners); `UPDATE_SNAPSHOTS=1` regenerates, mismatches write
   `-actual`/`-diff` PNGs. **The two golden PNGs (light mode) need human
   visual review.**
   
   ## Web UI: deterministic rendering & fonts
   
   * `theme.css`: global `@media (prefers-reduced-motion: reduce)` rule freezing
   CSS animations/transitions; `Spinner.tsx` honors the media query (static
   frame instead of a rAF loop).
   * Google Fonts request now includes weight 700 (bold markdown text was
   synthesized differently per platform from the 400–600 set).
   * New `infinity-ui` `icons.tsx` (PinIcon, Sun/Moon/MonitorIcon, Copy/CheckIcon,
   Chevron icons): all emoji / font-dependent symbol glyphs (📌 💻 ☀ ☾ ⧉ ✓ ▾ ▸)
   replaced with inline stroke SVGs — emoji rendering depends on platform fonts
   (hosts without a color-emoji font drop 📌 entirely).
   
   ## CI (`.github/workflows/ci.yml`)
   
   The web e2e runs in the existing test job on ubuntu only: setup-node +
   `npx playwright@1.60.0 install --with-deps chromium` + the feature-gated test,
   with a `web-e2e-screenshots` artifact uploaded on failure (goldens +
   `-actual`/`-diff` PNGs) for reviewing rendering drift.
   
   ## check.bash
   
   New guarded "Web UI e2e (Playwright)" step: runs only when npm and the
   Playwright 1.60 chromium build are installed (skip note otherwise) and falls
   back to a temp npm cache when `~/.npm` isn't writable. For old-glibc hosts
   where the driver's bundled node can't run, a comment documents the env-var
   override (`PLAYWRIGHT_SKIP_DRIVER_DOWNLOAD=1` + `PLAYWRIGHT_NODE_EXE` +
   `PLAYWRIGHT_CLI_JS`, forwarded by cargo to both the build script and the test
   binary) — verified working end-to-end on this host.
 - <csr-id-5b9f795b75a820cad2a13f73c84fb377f8fd95dd/> add snapshot test for model update on session load
 - <csr-id-16c123c012d0a73e88a35cbd6b3b53df0cb282f5/> TUI snapshot-test infrastructure with vt100 + alacritty, documenting ~25 viewport bugs
   Refactor the terminal UI's I/O onto traits so it can be driven against
   virtual terminals, and add a deterministic insta snapshot-test suite
   (52 tests / 97 snapshots) that reproduces and documents existing TUI
   bugs — no fixes included; buggy baselines are committed as-is and
   annotated with greppable `// BUG:` comments (34) for a later fixing pass.
   
   ## Terminal I/O abstraction (`src/term_io.rs`)
   
   * `TermOut` trait: ANSI byte sink (`Write`) + `size()`,
   `cursor_position()`, raw-mode toggles — everything the TUI queries
   from the terminal.
   * `EventSource` trait: cancel-safe async `wait_for_event()` +
   non-blocking `try_read_event()` for crossterm input events.
   * Production impls `CrosstermTerm` (stdout) and `CrosstermEvents` (16ms
   spawn_blocking poll, preserving spinner-animation wakeups; replaces
   `terminal::poll_crossterm_event`).
   
   ## Refactors (no behavior change)
   
   * `InlineViewport<T: TermOut>` owns its terminal instead of grabbing
   `io::stdout()` ad hoc; size/cursor queries go through the trait.
   * `terminal::run(term, events, ...)` is generic over both traits;
   `cleanup()` / `set_terminal_title()` take the terminal explicitly.
   * `std::time::Instant` → `tokio::time::Instant` in spinner paths so
   paused-clock tests are deterministic.
   * `MoveToNextLine` emits `CSI B` + CR instead of the equivalent but
   less-supported `CSI E` (vt100 lacks CNL; vt100 0.16 is blocked by
   ratatui's `unicode-width =0.2.0` pin).
   * `install.rs` / `daemon_client.rs` updated to the new APIs.
   
   ## Test harness (`tests/common/mod.rs`)
   
   * `Emulator` trait with two backends covering both real-world terminal
   behavior classes:
   * `Vt100Emulator` — truncates on resize, no reflow (xterm-like);
   * `AlacrittyEmulator` (via `alacritty_terminal`) — rewraps wrapped
   lines through scrollback and translates cursor + DECSC saved cursor
   on resize (alacritty/kitty/VTE-like), the class where inline-viewport
   resize bugs live. Wide-char spacer cells rendered correctly.
   * `VirtualTerm` (TermOut → shared emulator) + `ScriptedEvents`
   (channel-fed EventSource).
   * `TuiHarness`: spawns the real `terminal::run` loop with mock channels
   (`rig_mock::MockStreamingResponse` as the response type); helpers
   `display()`, `display_for_thread()`, `key()`, `type_str()`,
   `resize()`, `tick()`, `advance_and_redraw()`, `screen()` and
   `screen_with_scrollback()` (framed character grid + scrollback +
   cursor state + terminal title).
   * Determinism: `#[tokio::test(start_paused = true)]`; `settle()` is a
   1ms sleep completing only via tokio auto-advance once the UI task has
   drained its queues; animations advanced explicitly with
   `tokio::time::advance`. Verified flake-free across repeated locked
   (INSTA_UPDATE=no) runs.
   
   ## Test suites
   
   * `tui_snapshots.rs` (10, vt100): startup, slash autocomplete + Tab,
   thinking → streaming → done, tool call + result replacing the thinking
   display, resize mid-stream, help overlay, child-thread rows, message
   submission, session load, spinner animation over time.
   * `tui_reflow_snapshots.rs` (14, alacritty): wrapped-scrollback reflow on
   narrow/wide resize, deep scrollback, vertical shrink/grow (incl. next
   to spinner-height changes and mid-stream partial lines), coalesced
   resizes, resizes with pickers open, widen mid-stream, print racing the
   Resize notification.
   * `tui_viewport_snapshots.rs` (14, alacritty): viewport height changes
   next to scrollback (spinner/autocomplete/pickers/choice/thread rows,
   multiline input), multiline Info / OAuth / subscription events, tiny
   terminals, /help on narrow terminals, prints while picker open,
   submit mid-stream, compound grow/collapse.
   * `tui_widechar_snapshots.rs` (5, alacritty): CJK/emoji display-width vs
   char-count issues in wrap_tail, the input box, and the status row.
   * `tui_flow_snapshots.rs` (9, mixed): zellij-style session-replay burst
   (session switch + full history replayed undrained, spinner toggling
   between prints — renders clean, locked as regression guard),
   diff/empty tool results, child close_thread + subscription prefixes,
   multiline bracketed paste, quit picker via soft detach, lazy new
   session, queued choices + external completion, sessions updated while
   picker open, slash-command feedback.
   
   ## Bugs documented (34 `// BUG:` annotated assertions)
   
   Each buggy baseline has a greppable `// BUG: <explanation>` comment above
   its assertion. Highlights:
   
   * data loss: widening mid-stream overwrites reflowed text; a print
   racing the Resize event is eaten by a stale scroll region (and its
   aftermath bakes a duplicate status row into scrollback)
   * vertical/horizontal resize corruption: ghost spinners and stale or
   duplicated borders that subsequent prints scroll into permanent
   history; picker-resize artifacts; session-picker shrink merging
   leftovers into the status row
   * wide chars: wrap_tail clips CJK/emoji tails (chars vs columns); emoji
   eat the following space in the input box; emoji unbalance status-row
   padding
   * text/layout: multi-line Info loses newlines; OAuth URL staircases
   (LF without CR); tiny terminals lose the input row; /help corrupts
   scrollback on narrow terminals; status-row collision at narrow widths
   * stale state: session change never redraws (hidden cursor, stale
   context %); "waiting for tool call result" spinner text persists after
   the result rendered; lazy new session keeps the old terminal title;
   sessions_updated never repaints an open session picker
   
   Verified-correct behaviors (spinner grow/shrink, vertical grow pulling
   history, prints while picker open, streaming CJK, replay burst, compound
   collapse, etc.) are locked in as regression guards for the upcoming
   fixes.

### New Features (BREAKING)

 - <csr-id-84f7aff103f885169f4a6f4ba34aca3af9111a91/> run model providers as configurable separate processes over Unix sockets
   The Bedrock provider is no longer hardcoded and linked into the daemon.
   Providers now run as standalone processes that serve the `ModelProvider`
   trait over a Unix domain socket, configured in `~/.infinity/providers.json`
   and managed with new `infinity provider` CLI commands. The CLI also gained
   a readiness handshake so daemon startup failures surface directly.
   
   ## `infinity-agent-core`: `model_provider::remote` submodule
   
   New Unix-socket transport for any `ModelProvider` implementor
   (`model_provider.rs` moved to `model_provider/mod.rs` to host it):
   
   * **Protocol**: one JSON value per line, framed with tokio-util's
   `LinesCodec`; one request per connection (concurrent invocations =
   concurrent connections). `ProviderRequest::{ListModels, InvokeModel}` →
   `ProviderResponse::{Models, InvokeStarted, Chunk…, StreamEnd, Error}`.
   `WireCompletionRequest` / `WireStreamItem` are serializable mirrors of
   rig's `CompletionRequest` and `RawStreamingChoice`.
   * **Server**: `serve_provider(provider)` binds a fresh temp socket path and
   returns `(path, server_future)` for provider binaries to run.
   * **Client**: `RemoteModelProvider` implements the full trait over the
   socket, including streaming with mid-stream error forwarding.
   * Tests: socket round trip (list + streamed invocation with usage) against
   a mock model; clean failure on a missing socket.
   
   ## `infinity-provider-bedrock`: new binary
   
   Serves `BedrockProvider::from_env()` via `serve_provider` and prints the
   socket path as its only stdout line (logs go to stderr).
   
   ## `infinity-daemon`: config-driven provider registry
   
   * **BREAKING**: the daemon no longer links the Bedrock provider. Providers
   come from `~/.infinity/providers.json` — a JSON object mapping provider
   id to `{ "command": [...], "crate_name"?, "git"?, "path"? }`. There is no
   implicit default: a missing/empty config is a startup error pointing at
   `infinity provider install`.
   * `ProvidersConfig` preserves the JSON document's entry order via custom
   serde impls backed by a `Vec` (config order = registration order; the
   first model of the first provider is the global default). Duplicate ids,
   empty ids, and empty commands are rejected.
   * `models::spawn_provider` launches each command (normal `PATH` lookup; no
   special resolution) with piped stdout, waits (30s timeout) for the socket
   path line, forwards later stdout to the log, and kills the process on
   drop. `SessionManager` builds the `ModelCatalog` from
   `RemoteModelProvider`s and keeps the child handles alive for the
   daemon's lifetime.
   * `run_daemon(announce_ready: bool)` prints the new `DAEMON_READY_LINE` to
   stdout after all initialization succeeds (passed as true by the
   `infinity daemon` subcommand; the standalone binary keeps it off).
   
   ## `infinity-agent-cli`: provider management + launch supervision
   
   * `infinity provider install <id> --crate <name> [--git URL | --path DIR]`
   — cargo-installs the provider crate (sharing the `run_cargo_install` TUI
   plumbing with `rap install`) and registers it in providers.json,
   replacing existing entries in place to preserve ordering.
   * `infinity provider update` re-installs all providers with recorded
   sources; full `infinity update` now also updates providers.
   * `launch_daemon` spawns `{bin} daemon` with piped stdout/stderr and races
   (`tokio::select!`) the ready line against process exit (60s outer
   timeout). If the daemon exits during startup, the CLI reports everything
   it printed to stdout/stderr — previously discarded for the detached
   process. No post-launch connect retries: the socket is bound before
   readiness is announced, so a single connect suffices.
   * CLI `main` returns `ExitCode` and prints errors with Display formatting,
   so multi-line failure reports keep real newlines instead of Debug-escaped
   `\n`s.
   
   ## Docs
   
   * Quickstarts (README, Infinity Code overview, runtime getting-started)
   now include installing the Bedrock provider.
   * New `infinity-code/model-providers.md`: installing / configuring /
   switching / updating providers, providers.json reference, and
   troubleshooting based on the captured startup output.
   * New `infinity-runtime/model-providers.md`: the `ModelProvider` trait and
   `ModelEntry` semantics, writing a provider, with the Unix-socket process
   transport (stdout contract, line-delimited JSON protocol,
   `RemoteModelProvider`) covered at the end.
   
   The lambda crate intentionally keeps linking `BedrockProvider` in-process.

### Refactor (BREAKING)

 - <csr-id-49ad32e467d92f82cdac76095b6cb0a3daf2f964/> rig-free provider stack, native Bedrock, minimal deps; refreshed scale claims
   Remove `rig` from the core provider/agent stack. `infinity-provider-protocol` now owns a
   minimal model API; the Bedrock provider talks to the official AWS SDK directly
   (eliminating the maintained `rig-bedrock` patch); rig survives only as an optional
   bridge crate. Re-measured the memory benchmark and refreshed the landing/README claims.
 - <csr-id-9c921fde280b50c89c3e5b9caadccf83a46078a4/> extract shared agent system engine
   Extract the daemon's agent execution machinery into a shared `infinity_agent_core::system` engine, migrate the daemon and Lambda runtimes onto it, and extract the protocol components those embeddings share.
   
   This PR intentionally stops at the engine and embedding boundary. Static builder conveniences, local MCP/RAP tool-set adapters, `ThreadHandle`, launcher mode, per-thread launch configuration, and the new usage guides are introduced by #92.
   
   ## Breaking changes
   
   - Removes `batch_processor` (`process_batch`, `process_input_item`, and core `DisplayEvent`) in favor of `AgentSystem::step` and observer-based local execution.
   - `Thread` is internal and `AgentSystem` is not `Clone`.
   - `ThreadObserver` replaces inline daemon persistence/display hooks; it has no `on_commit`.
   - `EventCollector::take` returns `(thread_id, event)` pairs.
   - `ConversationStore` gains `thread_exists`, which checks exact root or child records without creating them.
   - `StateStore` gains the provided `is_thread_stopped` policy hook. User text may resume stopped threads; event-style input may not.
   - Builder tools are stored as `Rc`; `Tool` gains defaulted `is_passive`.
   - `ToolContext` and its builder lose `input_queue_arn`.
   - Resident runtime types such as `RunningSystem`, `SubscribeHandle`, `ChannelSender`, and `ChannelSendError` live under `system::local`.
   - The daemon no longer exports its old worker/loop/session implementation modules, sleep/RAP tool wrappers, or `boot_rap_servers`.
   - `SessionManager::send_input` no longer accepts a thread ID or `user_driven` flag and performs no status admission; router admission and lifecycle events own those concerns.
   - `rap_callback::start_callback_server` is replaced by `infinity_daemon::launch_session_manager`; callback serving accepts a `RapCallbackBridge`.
   - `SessionManager::switch_model` accepts the requester's sender and returns `Result<(), String>`; `SharedSessionManager` is `Rc`.
   - CLI `DisplayEvent` lives in `infinity_agent_cli::display`.
 - <csr-id-27b40fed6c5fd1fad5ebfabb1a2a909b7018a0cf/> extract provider protocol into `infinity-provider-protocol` crate
   * Move `infinity_agent_core::model_provider` (the `ModelProvider` trait, `ModelEntry`,
   `erase_streaming_response`, `SingleModelProvider`, and the `remote` Unix-socket wire
   protocol) into a new lightweight `infinity-provider-protocol` crate whose dependency
   surface is just rig-core + serde/schemars + async plumbing — no rap-client,
   rap-protocol, rhai, chrono, etc.
   * `infinity-provider-bedrock` now depends only on `infinity-provider-protocol`, dropping
   the entire `infinity-agent-core`/rap dependency tree from provider builds.
   * No legacy path: all consumers (`infinity-agent-core` internals, `infinity-daemon`,
   `infinity-agent-cli` tests) import `infinity_provider_protocol::…` directly instead
   of going through a re-export.
   * Trim now-unused deps from `infinity-agent-core` (`schemars`, `tokio-util`, tokio `net`
   feature).
   * Update `docs/docs/infinity-runtime/model-providers.md` to reference the new crate.
   * Regenerate the Rust section of `THIRD-PARTY` (adds `infinity-provider-protocol` to the
   Apache-2.0 "used by" list); the npm sections are unchanged since no npm deps changed.
 - <csr-id-b4a31e2925c371f38b85b8b2e878fdd226566766/> make model providers extensible via a dyn-compatible `ModelProvider` trait
   ## `infinity-agent-core`
   * New `model_provider` module:
   * `ModelProvider` trait (via `async_trait`) exposing `list_models()` and `invoke_model(model_id, CompletionRequest)`, which returns the same `StreamingCompletionResponse` as rig's `CompletionModel::stream` with the provider-specific final response erased to `ProviderStreamingResponse` (carries token usage only).
   * Providers have no identity of their own — ids are assigned at registration by callers that manage multiple providers.
   * `erase_streaming_response()` helper and a `SingleModelProvider<M>` adapter wrapping any rig `CompletionModel` (used by tests).
   * `run_completion`/`process_batch` now take a generic `P: ModelProvider + ?Sized` + model id instead of `Mdl: CompletionModel`, and no longer take `additional_request_params` / `model_id_override` / `max_output_tokens` — provider implementations handle those internally. Concrete callers (e.g. the Lambda) avoid `dyn` entirely and can specialize.
   
   ## `infinity-provider-bedrock` (new crate)
   * `BedrockProvider` wraps the rig-bedrock client and handles all Bedrock-specific request parameters internally: adaptive-thinking config, per-model `additional_model_request_fields` (e.g. anthropic beta flags), and max output tokens.
   * The two opus-4-6 configurations get distinct provider-scoped model ids (`...-v1` and `...-v1:1m`) that both map to the same underlying Bedrock model.
   
   ## `infinity-protocol`
   * New `ModelRef { provider_id, model_id }` — models are now identified globally by provider id + model id.
   * `ModelInfo` gains `provider_id`; `CreateSession`/`SwitchModel` carry `ModelRef` (**breaking**).
   
   ## `infinity-daemon`
   * `model_picker.rs` replaced by `models.rs` with `ModelCatalog`: providers stored in a `HashMap` keyed by stable unique non-empty ids (asserted, since the empty id is the metadata serde sentinel); just Bedrock registered for now, but all code paths support multiple providers.
   * Per-thread model selection: `ThreadInfo` gains a non-optional `selected_model: ModelRef`, assigned at thread creation (no parent fallback) and backfilled with the default model when loading metadata serialized before this change. `thread_worker` resolves its thread's model (and context window) from the catalog at startup, falling back to the global default with a warning if the stored model is gone.
   * Fixes the bug where a session created with a specific model reverted to the default after a daemon restart — the selection is now persisted in thread metadata and re-resolved on every worker start.
   * Removed the never-written `active_model_id`/`additional_request_params` RwLocks and the dead `Session::model_name`/`context_window` fields.
   
   ## `infinity-agent-lambda`
   * Instantiates the concrete `BedrockProvider` (no `dyn`) and invokes it with the existing hardcoded model id.
   
   ## CLI / web
   * CLI model picker now uses `infinity_protocol::ModelInfo` (no longer re-exports daemon types); selections are sent as `ModelRef`. Dropped the unused `rig-bedrock` dependency.
   * `infinity-ui` `ModelInfo` type gains `provider_id`.
   
   `./check.bash` passes except two pre-existing, unrelated `sandbox-local` environment-specific test failures.

### Commit Statistics

<csr-read-only-do-not-edit/>

 - 195 commits contributed to the release.
 - 60 commits were understood as [conventional](https://www.conventionalcommits.org).
 - 33 unique issues were worked on: [#10](https://github.com/hydro-project/infinity/issues/10), [#107](https://github.com/hydro-project/infinity/issues/107), [#110](https://github.com/hydro-project/infinity/issues/110), [#13](https://github.com/hydro-project/infinity/issues/13), [#15](https://github.com/hydro-project/infinity/issues/15), [#18](https://github.com/hydro-project/infinity/issues/18), [#19](https://github.com/hydro-project/infinity/issues/19), [#20](https://github.com/hydro-project/infinity/issues/20), [#21](https://github.com/hydro-project/infinity/issues/21), [#22](https://github.com/hydro-project/infinity/issues/22), [#23](https://github.com/hydro-project/infinity/issues/23), [#26](https://github.com/hydro-project/infinity/issues/26), [#29](https://github.com/hydro-project/infinity/issues/29), [#35](https://github.com/hydro-project/infinity/issues/35), [#37](https://github.com/hydro-project/infinity/issues/37), [#41](https://github.com/hydro-project/infinity/issues/41), [#52](https://github.com/hydro-project/infinity/issues/52), [#53](https://github.com/hydro-project/infinity/issues/53), [#55](https://github.com/hydro-project/infinity/issues/55), [#60](https://github.com/hydro-project/infinity/issues/60), [#61](https://github.com/hydro-project/infinity/issues/61), [#67](https://github.com/hydro-project/infinity/issues/67), [#68](https://github.com/hydro-project/infinity/issues/68), [#69](https://github.com/hydro-project/infinity/issues/69), [#70](https://github.com/hydro-project/infinity/issues/70), [#71](https://github.com/hydro-project/infinity/issues/71), [#72](https://github.com/hydro-project/infinity/issues/72), [#74](https://github.com/hydro-project/infinity/issues/74), [#8](https://github.com/hydro-project/infinity/issues/8), [#89](https://github.com/hydro-project/infinity/issues/89), [#90](https://github.com/hydro-project/infinity/issues/90), [#95](https://github.com/hydro-project/infinity/issues/95), [#96](https://github.com/hydro-project/infinity/issues/96)

### Commit Details

<csr-read-only-do-not-edit/>

<details><summary>view details</summary>

 * **[#10](https://github.com/hydro-project/infinity/issues/10)**
    - Add GitHub Actions workflows for lints, tests, conventional commits, and docs ([`ea6b62e`](https://github.com/hydro-project/infinity/commit/ea6b62e7b00f2a6b7e7338fa12e60fb3a46bb012))
 * **[#107](https://github.com/hydro-project/infinity/issues/107)**
    - Set up cargo-smart-release release workflow (mirroring hydro) ([`ffc27d0`](https://github.com/hydro-project/infinity/commit/ffc27d0bf5d964a655fedab9460bf5017971e6b6))
 * **[#110](https://github.com/hydro-project/infinity/issues/110)**
    - Rig-free provider stack, native Bedrock, minimal deps; refreshed scale claims ([`49ad32e`](https://github.com/hydro-project/infinity/commit/49ad32e467d92f82cdac76095b6cb0a3daf2f964))
 * **[#13](https://github.com/hydro-project/infinity/issues/13)**
    - Add Claude Fable 5 to Bedrock models list ([`66ddd8f`](https://github.com/hydro-project/infinity/commit/66ddd8ff3797df0284b0658382249133361b55d9))
 * **[#15](https://github.com/hydro-project/infinity/issues/15)**
    - Pass CLI-selected model to new sessions ([`a20554d`](https://github.com/hydro-project/infinity/commit/a20554d63a64440f1ac7d2aa810697e035712832))
 * **[#18](https://github.com/hydro-project/infinity/issues/18)**
    - Make model providers extensible via a dyn-compatible `ModelProvider` trait ([`b4a31e2`](https://github.com/hydro-project/infinity/commit/b4a31e2925c371f38b85b8b2e878fdd226566766))
 * **[#19](https://github.com/hydro-project/infinity/issues/19)**
    - Run model providers as configurable separate processes over Unix sockets ([`84f7aff`](https://github.com/hydro-project/infinity/commit/84f7aff103f885169f4a6f4ba34aca3af9111a91))
 * **[#20](https://github.com/hydro-project/infinity/issues/20)**
    - TUI snapshot-test infrastructure with vt100 + alacritty, documenting ~25 viewport bugs ([`16c123c`](https://github.com/hydro-project/infinity/commit/16c123c012d0a73e88a35cbd6b3b53df0cb282f5))
 * **[#21](https://github.com/hydro-project/infinity/issues/21)**
    - Fix all 34 snapshot-documented TUI bugs (reflow, wide chars, spinner state, tiny terminals) ([`cdcf093`](https://github.com/hydro-project/infinity/commit/cdcf0933658551227caa38def229e734fb0b0e42))
 * **[#22](https://github.com/hydro-project/infinity/issues/22)**
    - Context usage resets to zero after session replay ([`c49fc7d`](https://github.com/hydro-project/infinity/commit/c49fc7d0e78a22c2f0f8f6c84878e4e6a3dcfe35))
 * **[#23](https://github.com/hydro-project/infinity/issues/23)**
    - Update displayed model when loading an existing session ([`bb92400`](https://github.com/hydro-project/infinity/commit/bb92400080d4b85fcc34a51b6a54b8959cabade3))
 * **[#26](https://github.com/hydro-project/infinity/issues/26)**
    - Add snapshot test for model update on session load ([`5b9f795`](https://github.com/hydro-project/infinity/commit/5b9f795b75a820cad2a13f73c84fb377f8fd95dd))
 * **[#29](https://github.com/hydro-project/infinity/issues/29)**
    - Drop "Using provider" info message, show provider_id in status displays ([`24fa6cb`](https://github.com/hydro-project/infinity/commit/24fa6cbf5564d4df2297451bdc76c9619ec741fe))
 * **[#35](https://github.com/hydro-project/infinity/issues/35)**
    - Fix test compilation after merge ([`ba08e40`](https://github.com/hydro-project/infinity/commit/ba08e40ae829ed59bd2d08f1b986ce4d7b1e71e3))
 * **[#37](https://github.com/hydro-project/infinity/issues/37)**
    - Show provider ID in CLI model picker ([`4368bde`](https://github.com/hydro-project/infinity/commit/4368bde7e4be240e52932067702ccad333c17a08))
 * **[#41](https://github.com/hydro-project/infinity/issues/41)**
    - Render subscription event body in gray instead of orange ([`a959465`](https://github.com/hydro-project/infinity/commit/a9594657afe733d50c9cecb61ac29f8066faaf01))
 * **[#52](https://github.com/hydro-project/infinity/issues/52)**
    - Use total_tokens for context usage and compaction trigger ([`b7a9805`](https://github.com/hydro-project/infinity/commit/b7a980585d981b1ae22f1bb4fad12b739202b524))
 * **[#53](https://github.com/hydro-project/infinity/issues/53)**
    - Increase LengthDelimitedCodec max frame size to 256 MiB ([`8ad86d8`](https://github.com/hydro-project/infinity/commit/8ad86d850d761c58669dffb906ef389654e4990d))
 * **[#55](https://github.com/hydro-project/infinity/issues/55)**
    - Add deterministic e2e tests for TUI↔daemon and web UI (Playwright) ([`646c8f3`](https://github.com/hydro-project/infinity/commit/646c8f3dfcbb352369e70022cab1292cbbc49384))
 * **[#60](https://github.com/hydro-project/infinity/issues/60)**
    - Replay in-progress thinking and response state to clients attaching mid-response ([`7a6e971`](https://github.com/hydro-project/infinity/commit/7a6e9715a7b602d0a04bc527a3c76f4c6a1ccd80))
 * **[#61](https://github.com/hydro-project/infinity/issues/61)**
    - Multimodal (image) tool results end-to-end, with image display + review fixes ([`1935c38`](https://github.com/hydro-project/infinity/commit/1935c387d806a1da271e15078b26e06f228737c6))
 * **[#67](https://github.com/hydro-project/infinity/issues/67)**
    - Mid-session model switching per thread, with TUI + desktop UI and e2e tests ([`1c4f71a`](https://github.com/hydro-project/infinity/commit/1c4f71a611507dc7575c20b724faef680cbde2c7))
 * **[#68](https://github.com/hydro-project/infinity/issues/68)**
    - Count deferred soft wraps in the inline viewport's output tracker ([`78bd1b5`](https://github.com/hydro-project/infinity/commit/78bd1b5524346a9cc786cdf12f0e6e7a0b2ea085))
 * **[#69](https://github.com/hydro-project/infinity/issues/69)**
    - Strip ANSI escapes from external text instead of panicking ([`45f198e`](https://github.com/hydro-project/infinity/commit/45f198eec86493474ba48f812f397a4dbc113321))
 * **[#70](https://github.com/hydro-project/infinity/issues/70)**
    - Strip ANSI escapes from external text instead of panicking ([`45f198e`](https://github.com/hydro-project/infinity/commit/45f198eec86493474ba48f812f397a4dbc113321))
 * **[#71](https://github.com/hydro-project/infinity/issues/71)**
    - Extract provider protocol into `infinity-provider-protocol` crate ([`27b40fe`](https://github.com/hydro-project/infinity/commit/27b40fed6c5fd1fad5ebfabb1a2a909b7018a0cf))
 * **[#72](https://github.com/hydro-project/infinity/issues/72)**
    - Add `infinity daemon restart` and post-update daemon check ([`440752d`](https://github.com/hydro-project/infinity/commit/440752df0e987e0eaaf7adda2dfaf23f9a2955db))
 * **[#74](https://github.com/hydro-project/infinity/issues/74)**
    - Reset context usage when compaction is applied so it does not re-trigger ([`a536c9f`](https://github.com/hydro-project/infinity/commit/a536c9fa6d51bd2eaf0d5cf88af237ea1cce0e65))
 * **[#8](https://github.com/hydro-project/infinity/issues/8)**
    - Add automated THIRD-PARTY file generation with license enforcement ([`e2e0719`](https://github.com/hydro-project/infinity/commit/e2e0719faebbffc72ec7bd8a8b3b02223da8ba0e))
 * **[#89](https://github.com/hydro-project/infinity/issues/89)**
    - Don't eat scrollback when a resize races the re-anchor cursor query ([`d5d3def`](https://github.com/hydro-project/infinity/commit/d5d3defcb1df5e0b88d466566e621cbffcb5f411))
 * **[#90](https://github.com/hydro-project/infinity/issues/90)**
    - Add `keeps_session_alive` flag to prevent non-interactive clients from blocking idle shutdown ([`1b20fda`](https://github.com/hydro-project/infinity/commit/1b20fdac512ea534ee24006b95903f3961ff5179))
 * **[#95](https://github.com/hydro-project/infinity/issues/95)**
    - Show a placeholder in the session picker when there are no sessions ([`cbe6f17`](https://github.com/hydro-project/infinity/commit/cbe6f1766a2d73aaad47a02cfe5dcc8ce063b0c0))
 * **[#96](https://github.com/hydro-project/infinity/issues/96)**
    - Extract shared agent system engine ([`9c921fd`](https://github.com/hydro-project/infinity/commit/9c921fde280b50c89c3e5b9caadccf83a46078a4))
 * **Uncategorized**
    - Release infinity-agent-core v0.1.0, infinity-mcp-bridge v0.1.0, infinity-rap-bridge v0.1.0, infinity-daemon v0.1.0, infinity-agent-cli v0.1.0, sandbox-core v0.1.0, sandbox-local v0.1.0, sandbox-remote v0.1.0 ([`7599fbb`](https://github.com/hydro-project/infinity/commit/7599fbbdfad042a6fd85c23002bf937fecbe7b45))
    - Release infinity-provider-protocol v0.1.0, infinity-provider-bedrock v0.1.0, infinity-provider-rig v0.1.0, infinity-agent-core v0.1.0, infinity-mcp-bridge v0.1.0, infinity-rap-bridge v0.1.0, infinity-daemon v0.1.0, infinity-agent-cli v0.1.0, sandbox-core v0.1.0, sandbox-local v0.1.0, sandbox-remote v0.1.0 ([`7e1cd1d`](https://github.com/hydro-project/infinity/commit/7e1cd1df69d8fce402bef4085e9d17f871994503))
    - Release rap-protocol v0.1.0, rap-client v0.1.0, rap-steering-server v0.1.0, rap-github-event-poller v0.1.0, infinity-protocol v0.1.0, infinity-provider-protocol v0.1.0, infinity-provider-bedrock v0.1.0, infinity-provider-rig v0.1.0, infinity-agent-core v0.1.0, infinity-mcp-bridge v0.1.0, infinity-rap-bridge v0.1.0, infinity-daemon v0.1.0, infinity-agent-cli v0.1.0, sandbox-core v0.1.0, sandbox-local v0.1.0, sandbox-remote v0.1.0 ([`dd8c7f4`](https://github.com/hydro-project/infinity/commit/dd8c7f49028a26052d785b4241f9ade125f0afb3))
    - Rewrite ACP server with per-session daemon connections ([`ff1b8ee`](https://github.com/hydro-project/infinity/commit/ff1b8ee3af55c3fa816d507a7634012d2cc1fdad))
    - /stop pauses agent instead of resetting session ([`c308202`](https://github.com/hydro-project/infinity/commit/c308202b8b11a8092399eefbf7c087ddab11971a))
    - Support prefix matching for --session arg ([`dba5f88`](https://github.com/hydro-project/infinity/commit/dba5f88e924e5bf8f2d512b6ce6485230c56110f))
    - Log communication failures instead of swallowing ([`eb9404f`](https://github.com/hydro-project/infinity/commit/eb9404f66f01aadc31270571e4e0008ad59ea234))
    - Add /archive slash command ([`5bccd1d`](https://github.com/hydro-project/infinity/commit/5bccd1dd693778407114f7af182e7339ab0b8420))
    - Remove easy-to-trigger compaction shortcuts from CLI ([`698ff54`](https://github.com/hydro-project/infinity/commit/698ff544df0bea23b3087794dd869976368a8528))
    - Show choice picker alongside input and cancel choices on tool interruption ([`59d3314`](https://github.com/hydro-project/infinity/commit/59d331491087ef43aa3cea9215a94c2089675b30))
    - Add `--session` flag to connect to a session by name or ID ([`4059c54`](https://github.com/hydro-project/infinity/commit/4059c548b91070d2885215957ec45e381fce7565))
    - Add clap conflicts_with to prevent --headless with --local ([`9c44d23`](https://github.com/hydro-project/infinity/commit/9c44d23fad385e6f326d7cbdb957669cb1de8bb1))
    - Add -H/--headless flag, rename -m to -i/--initial-message ([`b5d3cac`](https://github.com/hydro-project/infinity/commit/b5d3cac9bd75ca35d65d77db97fbe6da548bf0f3))
    - Accept Ctrl+C as cancel in TUI menu popups ([`b026adc`](https://github.com/hydro-project/infinity/commit/b026adc49c8997cff35b88d181b202b0209ca477))
    - Collapse `if` blocks into match guards to satisfy clippy ([`1e1ae38`](https://github.com/hydro-project/infinity/commit/1e1ae380ae017878c224f8633b0ea7c95993ec82))
    - Serve web UI from daemon with `bundled-web` feature ([`bc19ab2`](https://github.com/hydro-project/infinity/commit/bc19ab28e1ff552a4992812b75ffefd796973948))
    - Add directory tab completion in session picker ([`0297d74`](https://github.com/hydro-project/infinity/commit/0297d743512c02edd25a8ede1ee551ea65d878dc))
    - Require explicit `ssh` keyword after `--` in `remote add` ([`ea42de7`](https://github.com/hydro-project/infinity/commit/ea42de747adccf033b0be82f6d5131bd68e6408d))
    - New session button uses location picker instead of local-only CWD picker ([`947b37a`](https://github.com/hydro-project/infinity/commit/947b37af6289db10485ee7e0a4267333edc4bcef))
    - Add UserChoiceComplete daemon-to-client message ([`4169bdc`](https://github.com/hydro-project/infinity/commit/4169bdceccae28a77d664b9942758651defe8a0b))
    - Add workspace lints and fix all lint violations ([`b92b7a1`](https://github.com/hydro-project/infinity/commit/b92b7a17f4b69e2652f5cce813320eca851717e4))
    - Add RAP view_update protocol + diff view in web UI ([`7085405`](https://github.com/hydro-project/infinity/commit/7085405bbfa8d07f6a69bc0e418761a56d108a67))
    - Add remote host migration UI and daemon orchestration ([`ba10ffd`](https://github.com/hydro-project/infinity/commit/ba10ffd62644a4c86c31a7fb6d5eaaca8c403b55))
    - Log panics from fire-and-forget spawned tasks instead of silently swallowing them ([`44fcca2`](https://github.com/hydro-project/infinity/commit/44fcca250a44029e36b49df6013a049d33bc985f))
    - Add support for connecting to remote sessions via your local daemon ([`67f4085`](https://github.com/hydro-project/infinity/commit/67f40855a59ac5263ec3f3726c69017c4cd0b464))
    - Replace all .unwrap() with .expect() and fix clippy warnings ([`7634b82`](https://github.com/hydro-project/infinity/commit/7634b823ad70378e666379a9a8e8a7935a06026f))
    - Add precheck script, lints ([`9757071`](https://github.com/hydro-project/infinity/commit/9757071818663cefb8e6a12438071d95000379a8))
    - Replace bincode with serde_json for CLI↔daemon unix socket wire format ([`62f3822`](https://github.com/hydro-project/infinity/commit/62f382276e6fc8ee76888ac1c629538c977e1745))
    - Introduce display_as typed variants and use Pierre to display in web client ([`1e65518`](https://github.com/hydro-project/infinity/commit/1e65518e4f041f76e6359b08ff88e32fc8753cda))
    - Move HistoryManager to interior mutability; remove callback_with_history hack; restore subscribe_rx in select ([`1e92087`](https://github.com/hydro-project/infinity/commit/1e9208751e55d0029acd419ae12f1bf05cc7104e))
    - Display subthreads in web UI and make it possible to connect to subthreads directly ([`718509d`](https://github.com/hydro-project/infinity/commit/718509d481340bd43497530b3f1212b3f3be27af))
    - Remove always-shown stop/continue prompt; add /stop command ([`b80f635`](https://github.com/hydro-project/infinity/commit/b80f635c1c423908cdd5fe8981d3b805604af975))
    - Show current session in session switcher UI ([`34145d9`](https://github.com/hydro-project/infinity/commit/34145d9927397efd29a50a41f7839e965c9c6475))
    - Use user.name/email with fallback ([`73708c0`](https://github.com/hydro-project/infinity/commit/73708c07ed08acfd388bdf26654e71f9ab3184bd))
    - Use display_as for tool call pretty-printing in web UI ([`1e4a489`](https://github.com/hydro-project/infinity/commit/1e4a4894ce62c05ab6561539ff3e9a8abf662974))
    - Extract rap-client crate and unify RAP protocol types ([`51406e4`](https://github.com/hydro-project/infinity/commit/51406e4dfab243a4400027507f446862b26ce8d3))
    - Show truncated (8-char) thread ID in terminal status bar before context usage ([`2124c88`](https://github.com/hydro-project/infinity/commit/2124c88c5a0c8c11f8b47105976235f99778b72a))
    - Improve `infinity update` message when there is no user-level RAP config ([`6be3871`](https://github.com/hydro-project/infinity/commit/6be38711de7bca6bd134610fcd2ba223b911556a))
    - Make sure infinity update works when there is no user-level rap.json ([`b5cee45`](https://github.com/hydro-project/infinity/commit/b5cee45d7b7abf308ef5d3452f53f01268495fd4))
    - Initialize RAP config on first server installation ([`7b144ad`](https://github.com/hydro-project/infinity/commit/7b144ad3b534dcd6426e2501c761ddc6da6ded55))
    - Restore cursor visibility on shutdown ([`b15535e`](https://github.com/hydro-project/infinity/commit/b15535eaf1b8481b93907b73c9ae49aaaa0d8f2a))
    - Fix manual compaction and add background auto-compaction triggers ([`9b10a09`](https://github.com/hydro-project/infinity/commit/9b10a0977283f5f628142841cf9515a8b8793793))
    - Hanging caused by `sh -c` intercepting SIGINT, improved config error handling ([`b40442e`](https://github.com/hydro-project/infinity/commit/b40442e37ac91b884f51fcabb018a3735bdf612f))
    - Add rig-mock crate and test suite for agent core and daemon ([`abda067`](https://github.com/hydro-project/infinity/commit/abda06757eeba0ac7817374bc89155211cd2edcd))
    - Add support for UserChoice prompts in RAP protocol and use for permissions expansion in sandbox ([`b0db6a7`](https://github.com/hydro-project/infinity/commit/b0db6a7a0764ddab7df1f5cf3fcefc7129c6ddcb))
    - Make session title column width dynamic based on terminal width ([`49148b3`](https://github.com/hydro-project/infinity/commit/49148b3ff4a8c2d2e1e1d3e382ba9c759b258e7c))
    - Rewrite session idle management, input handling, to allow dropping resources on background idle ([`527cd09`](https://github.com/hydro-project/infinity/commit/527cd097895fb761869915844160834e38350553))
    - Add session status (running/idle/stopped) to CLI session list ([`56dd66d`](https://github.com/hydro-project/infinity/commit/56dd66d112dc068524573916a9183fe11f18b999))
    - Add rap-github-event-poller crate for local GitHub event polling ([`783a9ec`](https://github.com/hydro-project/infinity/commit/783a9ec48c0f8f97522c34f62460a48911ac9875))
    - Fix auto-exit on idle: send DetachedIdle message instead of closing connection ([`1478ba4`](https://github.com/hydro-project/infinity/commit/1478ba404d1653d5ae750ca5ebb990cd207071d3))
    - Fix hang when shutting down CLI with a non-idle agent ([`0bf608b`](https://github.com/hydro-project/infinity/commit/0bf608b352ab40026d25d6cc286163c2c40b2da0))
    - Allow auto quit without quit picker when agent is idle ([`3285dc5`](https://github.com/hydro-project/infinity/commit/3285dc5078947b76ad440342316dbd1d665800f4))
    - Add quit picker for graceful disconnect choice; cleanup on ungraceful disconnect ([`d87d7d3`](https://github.com/hydro-project/infinity/commit/d87d7d34130e9d2b5feda891bdc63267fc0689eb))
    - Shift core agent runtime into a daemon with a network protocol for clients ([`141d697`](https://github.com/hydro-project/infinity/commit/141d69792c3aa951fcbfbea847879582f1d06ec3))
    - Move sccache pre-start from CLI to sandbox backend ([`c190095`](https://github.com/hydro-project/infinity/commit/c1900951a752f75bbac38fb5a7b29f027a94ebdd))
    - Pre-start sccache server before agent sandbox to avoid startup issues ([`5aa3c40`](https://github.com/hydro-project/infinity/commit/5aa3c402896ca37a4a27b5a5c3cd68ca843e4196))
    - Persist session store on every ResponseDone instead of only on exit ([`6547719`](https://github.com/hydro-project/infinity/commit/6547719c141ce54f71cf6e111dfb3ed372aff840))
    - Add top-level `update` command that updates CLI + RAP tools ([`69a9e6a`](https://github.com/hydro-project/infinity/commit/69a9e6a46a30634dd8715b0c53c606f13406d2a3))
    - Add slash command autocomplete with Tab cycling, multi-column table, and highlight ([`de75168`](https://github.com/hydro-project/infinity/commit/de75168f0c1ab60d0094ee409d5d3a25793b5918))
    - Deduplicate slash command and Ctrl key shortcut handlers ([`73e0c75`](https://github.com/hydro-project/infinity/commit/73e0c75f8a178f6f8e04cd1f92298abd278b464d))
    - Add slash commands as alternatives to ctrl key shortcuts ([`9a8b95e`](https://github.com/hydro-project/infinity/commit/9a8b95e0b0cada8c17bb242200a5d21b8ea00af0))
    - Avoid unnecessary tick timeout drawing spinner ([`dd04f2a`](https://github.com/hydro-project/infinity/commit/dd04f2aa40e4758509c5f4c4512a567a5ad224ee))
    - Avoid overwriting generated commit message when doing anything other than running a command ([`75f6d40`](https://github.com/hydro-project/infinity/commit/75f6d4015a12228da970658cbaa0e00dc4ac9524))
    - Move print_above and print_line_above to InlineViewport methods ([`e778c93`](https://github.com/hydro-project/infinity/commit/e778c93ae0aa43aa0f970de8d24811fa46629693))
    - Add optional "reason" parameter to sleep tool ([`ebbb830`](https://github.com/hydro-project/infinity/commit/ebbb830d7a53f0acb91052bd9e4a9138f9b41a3d))
    - Add --message/-m CLI parameter to send an initial message on startup ([`d186875`](https://github.com/hydro-project/infinity/commit/d186875058544f8f857e8368d5d736dd5c033549))
    - Fix binary install path for local sandbox and improve MCP output ([`e1dd438`](https://github.com/hydro-project/infinity/commit/e1dd438ff6e41440a38b9755fa1a9af284dca58e))
    - Fix spinner during installation ([`f291324`](https://github.com/hydro-project/infinity/commit/f2913241a7e86b9e98680ce137582cd8e9211c8b))
    - Update installation instructions ([`1dc9be4`](https://github.com/hydro-project/infinity/commit/1dc9be4ea709a5b3ac172676d1e52e5b957866fb))
    - Add support for global RAP config and add `rap install` / `rap update` tools ([`6372cd5`](https://github.com/hydro-project/infinity/commit/6372cd5622d2e8b23e04a6d5b001aa6b0e0fab6a))
    - Add Ctrl+J as universal alternative to Alt+Enter for inserting newlines ([`16daf57`](https://github.com/hydro-project/infinity/commit/16daf57739f153c94d2cbf973e2d2e01c24608ba))
    - Widen CLI spinner from 8 to 10 columns ([`b5d36a6`](https://github.com/hydro-project/infinity/commit/b5d36a6cedb869b19fd13408af39b0a109fba198))
    - Fix unnecessary terminal clear whenever ideal viewport shifts up ([`df657ef`](https://github.com/hydro-project/infinity/commit/df657ef196f6ca7e95fd2b034780ae4e4aeef42e))
    - Add displayScript field to RAP tool definitions for pretty-printing tool calls ([`f7e01f2`](https://github.com/hydro-project/infinity/commit/f7e01f2ccfc567fcc44aef1b85eb9e68e3e88131))
    - Update terminal title when root thread title is set, session is loaded, or new session is created ([`63a2dc0`](https://github.com/hydro-project/infinity/commit/63a2dc0aad48b894cc77fcf31803c46613e8ff9c))
    - Fix CLI hang on Ctrl+C/D when sandbox commands are running ([`28e79c7`](https://github.com/hydro-project/infinity/commit/28e79c78ff3289403bb2b7c324a4697f091a88f5))
    - Improve TUI readability ([`90d974c`](https://github.com/hydro-project/infinity/commit/90d974cda9db88140c0bde3797d42c105eac91bd))
    - Redesign spinner states ([`7a8bd6a`](https://github.com/hydro-project/infinity/commit/7a8bd6ace0e87ccfc50280e5f7debcffd4fca82d))
    - Improve spinner display when there is a very large context ([`2c2cdd6`](https://github.com/hydro-project/infinity/commit/2c2cdd66dcd94e37aef65a411274d4f2721edbb7))
    - Implement background compaction using threads ([`6e7e28b`](https://github.com/hydro-project/infinity/commit/6e7e28baff2ea33b6b12f52db370170c51128281))
    - Add send_message_to_child tool for parent-to-child thread messaging ([`897b024`](https://github.com/hydro-project/infinity/commit/897b02403dbea664c7e807ea02bf0fc8e5f480f1))
    - Add child_of validation to spawn_thread to prevent confused child threads from spawning subthreads ([`634fba0`](https://github.com/hydro-project/infinity/commit/634fba01523c7ba3ea0805db7ff9fd0411da7457))
    - Add tool for agent threads to give themselves a descriptive name ([`67a8127`](https://github.com/hydro-project/infinity/commit/67a8127271786193e15e53d79e003ad5579e3bfa))
    - Make viewport_y a private variable now that it is no longer used in drawing scrollback ([`920fee0`](https://github.com/hydro-project/infinity/commit/920fee02d262e48d538e2cf3c07dbb8667d5f838))
    - Eliminate unused scroll_region_bottom ([`71e6bfe`](https://github.com/hydro-project/infinity/commit/71e6bfeb39671657b91f451ce8f26169063826c5))
    - Fix TUI corruption when it is resizing ([`3b60bb5`](https://github.com/hydro-project/infinity/commit/3b60bb534a1a16ae09b031c95b44c4f50a3c7d00))
    - Further minimize flushes for TUI ([`4ef21bb`](https://github.com/hydro-project/infinity/commit/4ef21bb3ddcfc2f770f93ee12f2129281bd63fe5))
    - Only measure cursor position when we are clearing the TUI viewport ([`016d9db`](https://github.com/hydro-project/infinity/commit/016d9db07900501ea1df3a667d1489a5fe7358ac))
    - Revert "Fix SSH performance issues due to repeated synchronous cursor queries" ([`190f52c`](https://github.com/hydro-project/infinity/commit/190f52c9a5626306ea2208c05101921262c94861))
    - Fix SSH performance issues due to repeated synchronous cursor queries ([`e0f47c4`](https://github.com/hydro-project/infinity/commit/e0f47c4176a58ac72d4b2216405fd0a119f77a86))
    - Add MCP server support to the CLI via in-process RAP proxies ([`68b4266`](https://github.com/hydro-project/infinity/commit/68b426683d5c1c090c6f43f437a1d83396a95414))
    - Refactor CLI argument parsing to use clap derive ([`e46a8cb`](https://github.com/hydro-project/infinity/commit/e46a8cb237ec6be4042f9a642bfee03076efe4b3))
    - Correctly re-load token usage when loading session ([`d6506f8`](https://github.com/hydro-project/infinity/commit/d6506f854d183d5348072840666c178eaddbf8a5))
    - Write logs to .infinity/cli.log ([`b178460`](https://github.com/hydro-project/infinity/commit/b178460aaf409b1b92c9f4ee3b5c0a98fbe8afa2))
    - Clear thinking area when a tool is invoked ([`e7bf4a7`](https://github.com/hydro-project/infinity/commit/e7bf4a7febc2cedb406dc6169fb2699540e18875))
    - Improve retry handling and reporting ([`2c0f58c`](https://github.com/hydro-project/infinity/commit/2c0f58cea7f0d768d190642138c3ca99993ec62a))
    - Add model provider abstraction and model_id_override support ([`0effd62`](https://github.com/hydro-project/infinity/commit/0effd6250f6d6cf6d4384d3dafd82bc14af40a86))
    - Correctly handle HistoryManager::fork_new for threaded event handling ([`257c0b8`](https://github.com/hydro-project/infinity/commit/257c0b8842706c00a5ca484c9a9ca10e0fe93a72))
    - Extract shared batch processing logic into infinity-agent-core ([`ec43e34`](https://github.com/hydro-project/infinity/commit/ec43e34fffc0e6d5edadd3759695809ba80199bf))
    - Correctly handle cancellation using process groups ([`c7f9589`](https://github.com/hydro-project/infinity/commit/c7f9589773ff4c02d0efbf851d1b095f147453c2))
    - Fix history management bug with removing trailing empty content ([`ce9d55b`](https://github.com/hydro-project/infinity/commit/ce9d55b9bd1c7dc8ae186fe5f1f1819262086a1e))
    - Rewrite interruption handling to reduce duplication ([`032497e`](https://github.com/hydro-project/infinity/commit/032497e7c828e59cd89bf35577731672d273bbd3))
    - Add support for interrupting with user input during thinking / output ([`aa4d560`](https://github.com/hydro-project/infinity/commit/aa4d560accfd4177984282bf31117e0712fb8530))
    - Change local sandbox metadata store to file-based and set RAP server CWD to .infinity ([`0a891e3`](https://github.com/hydro-project/infinity/commit/0a891e3e69d0baa24fdc34d527259cea60fb7dec))
    - Add multi-line paste support to the CLI input buffer ([`608991a`](https://github.com/hydro-project/infinity/commit/608991a1e2589aeb25e91c32d03a67a51f5d7357))
    - Add model switcher to CLI with Ctrl+M shortcut ([`82fbf32`](https://github.com/hydro-project/infinity/commit/82fbf3267f8b4f77a730f8f0797d8b68e3514251))
    - Show first output line on same line as ✓ for multiline tool results ([`ec7fdd8`](https://github.com/hydro-project/infinity/commit/ec7fdd8ca8a86b7782be87192eee8c79ca611ee6))
    - Darken unhighlighted session picker text for readability on white backgrounds ([`f903e1b`](https://github.com/hydro-project/infinity/commit/f903e1bb471e23fd5dcc472ecdfaa372b3ce48d7))
    - Fix up broken thread state after spawn child ([`9df8171`](https://github.com/hydro-project/infinity/commit/9df8171932a897c2d286fab1b1699748e0320e50))
    - Store each thread's conversation history in a separate file and save on sync ([`79b0d62`](https://github.com/hydro-project/infinity/commit/79b0d628a3b6c5c8a6969bb5a34eec1ba9e12d0a))
    - Add tool call and subscription cancellation protocol for resource cleanup ([`56cfa15`](https://github.com/hydro-project/infinity/commit/56cfa15af99cfc07db6b0bfbe09327fccd72eadb))
    - Remove unused `find` and `has_sessions` methods from SessionStore ([`4bf2462`](https://github.com/hydro-project/infinity/commit/4bf24625f33c47202209052bd7b743c775dbd1b7))
    - Enforce absolute paths in local sandbox and add CWD to system prompt ([`c7fa225`](https://github.com/hydro-project/infinity/commit/c7fa225eb52f91077f53ddc7c63ad9546b70e45b))
    - Fix multiline subscription event display in CLI terminal ([`d1ccd0e`](https://github.com/hydro-project/infinity/commit/d1ccd0e8c9ab28b51e04627346aab197fc8c0de5))
    - Refactored the CLI TUI to support a component-based architecture and multi-session management: ([`3001c65`](https://github.com/hydro-project/infinity/commit/3001c6573813f4c7554befb02fb7cc8b274816f9))
    - Implement output streaming for execute_command using RAP subscriptions with debouncing. ([`b2fb764`](https://github.com/hydro-project/infinity/commit/b2fb7643665e2052419103a4c7d4466758b0e026))
    - Two changes to improve tool call/result display: ([`5ad3eb5`](https://github.com/hydro-project/infinity/commit/5ad3eb565a43ba76b8b61b2ac4f19449cd2d2d35))
    - Add thinking token visualization to the CLI terminal. ([`f416795`](https://github.com/hydro-project/infinity/commit/f41679517609de5f139bac11495c0e5b8944a1f6))
    - Add RAP protocol for notifying tool servers of thread closure ([`2d60e9d`](https://github.com/hydro-project/infinity/commit/2d60e9d12b84d01984b17e56c859caac8757859d))
    - Add up/down arrow key support to text input ([`f46e4f0`](https://github.com/hydro-project/infinity/commit/f46e4f062897c53aec357f6a13340e7f482da83e))
    - Avoid wrapping tool calls in thread status ([`675221c`](https://github.com/hydro-project/infinity/commit/675221cf1deea6b6ea0eea39984756171edf4604))
    - Persist display_as mapping in store.json and use it during history replay, document ([`1ca626a`](https://github.com/hydro-project/infinity/commit/1ca626a6d1fd200b1267c548079878192811d096))
    - Improve jj config management and shift spinner to top ([`40a3ac5`](https://github.com/hydro-project/infinity/commit/40a3ac57cf3b1a336a413862aa4c6b29fa1dc935))
    - Add help command and handle restoring a sandbox ([`b6f0bce`](https://github.com/hydro-project/infinity/commit/b6f0bce789e72d0953ce37ddd6018d14cb6a0439))
    - Coalesce multiple incoming events into one LLM invocation ([`ad2bb1a`](https://github.com/hydro-project/infinity/commit/ad2bb1acfccf8b180415a4fa101f14d96ce5ee6a))
    - Show thinking status after receiving a subscription event ([`8a5e1ea`](https://github.com/hydro-project/infinity/commit/8a5e1ea27b64a4a9d966b123634e922af046af8e))
    - Rich thread progress TUI ([`94f053f`](https://github.com/hydro-project/infinity/commit/94f053f06f8fd8c5e2efd573ff1695fe1d8aae70))
    - Add support for synchronous tool calls that are uninterruptible ([`544ee9c`](https://github.com/hydro-project/infinity/commit/544ee9c4d5c8507bbacb5dcc5f8006972301588e))
    - Improve thread handling ([`2f53c50`](https://github.com/hydro-project/infinity/commit/2f53c502e97f174734ef2ffe10b300e0f1f7b364))
    - Support launching RAP servers as a subprocess ([`5439050`](https://github.com/hydro-project/infinity/commit/5439050aef622ee1ac16227ded7646e3d08e55fb))
    - Fix context usage tracking ([`4fb463d`](https://github.com/hydro-project/infinity/commit/4fb463d2ec79ce9e73f83d066f3d2ea3be300f9a))
    - Session persistence ([`b4d4141`](https://github.com/hydro-project/infinity/commit/b4d41412a89ce7f707e65ddae5086dcca0baa085))
    - Store RAP config in a local directory ([`71c8101`](https://github.com/hydro-project/infinity/commit/71c81015e8d2b096aaa533d16e94b6944480b16d))
    - Rich diff printing ([`72441f1`](https://github.com/hydro-project/infinity/commit/72441f15385b2aa4d54c04bfbd981dee0220674f))
    - Context window tracking ([`1fc5ed8`](https://github.com/hydro-project/infinity/commit/1fc5ed8bf8c0fc6e330de316ed3f235a7dfaac3a))
    - Rich input navigation ([`a047137`](https://github.com/hydro-project/infinity/commit/a047137af49abbb7ee3adbe89701d75ed574bde7))
    - Fix cursor on line wrap ([`a6babda`](https://github.com/hydro-project/infinity/commit/a6babda2b0524569719490250dc7e3fb6802b26d))
    - Don't clear and re-render on height changes unless it affects the viewport ([`53f2cf9`](https://github.com/hydro-project/infinity/commit/53f2cf9f980a7d0aafebb066205706bcda02a8de))
    - Fix pointer arithmetic in text input ([`b62bc23`](https://github.com/hydro-project/infinity/commit/b62bc23462887ba12b0eb2ab2815ca064003bd3a))
    - Improve handling of changing height ([`8e458e5`](https://github.com/hydro-project/infinity/commit/8e458e587765bb8486b3667f4651ccc03a3dca83))
    - Initial multi-line input and thinking animation ([`35b1ab3`](https://github.com/hydro-project/infinity/commit/35b1ab3034378faf13047b8922d495fc4ed635a0))
    - Simplify ([`d521df8`](https://github.com/hydro-project/infinity/commit/d521df820f8e41c9c0370479bd33c684a8d35c1a))
    - Preliminary support for resizable TUI ([`4e30228`](https://github.com/hydro-project/infinity/commit/4e30228cbe42dbcf494f77d0a063360f2bc3d71c))
    - Coalesce resize events ([`adec81f`](https://github.com/hydro-project/infinity/commit/adec81ffc07c0407a378824e3389e8b587d9e651))
    - Add display_as to RAP tool results ([`18b60a5`](https://github.com/hydro-project/infinity/commit/18b60a5aa8a463d70eec75aca3e9a6e77722a972))
    - Code editing tools ([`36c7466`](https://github.com/hydro-project/infinity/commit/36c7466a0707836590fb385d313a2f929c3465e1))
    - Use ModifierDiff from upstream Ratatui ([`a2d9cbd`](https://github.com/hydro-project/infinity/commit/a2d9cbd20e5be5d2d1f4c274ba46f8188735a95b))
    - Handle more styles for spans ([`b22c325`](https://github.com/hydro-project/infinity/commit/b22c325554dbec251de3db1d356325dfa36e0b68))
    - Use Ratatui to render inline viewport ([`c426a64`](https://github.com/hydro-project/infinity/commit/c426a641502bb5bd91265148bf2e6e418008a2a0))
    - Run clippy ([`ea864bf`](https://github.com/hydro-project/infinity/commit/ea864bf5a21cb030738936df2749af7ad0c255d8))
    - Clean up dependencies ([`fcef65d`](https://github.com/hydro-project/infinity/commit/fcef65df6274e43596bf84f9b2eaf4d8955e9b93))
    - Cache jj workspaces for local sandboxes ([`ba0ba4e`](https://github.com/hydro-project/infinity/commit/ba0ba4e372432f9d1044f0e57e06b9ada870de30))
    - Initial functional Jujutsu filesystem sandbox ([`4118c89`](https://github.com/hydro-project/infinity/commit/4118c890809b1f93e0ca92a6861ab9351e6e8864))
    - Add sleep tools to CLI ([`6990407`](https://github.com/hydro-project/infinity/commit/69904073235be8df3c25bb6265f6ec64ed06060a))
    - Implement RAP server support in CLI ([`885c17b`](https://github.com/hydro-project/infinity/commit/885c17b1847339f9747eb910fc0f3752a9b2eeeb))
    - Redesign CLI interface to display calls / subscription events ([`6c3155a`](https://github.com/hydro-project/infinity/commit/6c3155aba36a809a5a805fcefb1048f63aac0040))
    - Support threading in CLI frontend ([`676711e`](https://github.com/hydro-project/infinity/commit/676711e55f6a67d9e53f7752636809376833743d))
    - Reduce scope of message sending trait ([`8514532`](https://github.com/hydro-project/infinity/commit/8514532f2a9bf6d96c15a29c7d25fcfc32a4b5c6))
    - Refactor out side effects of prepare_input and add snapshot tests ([`86736c1`](https://github.com/hydro-project/infinity/commit/86736c1eae594d48dd9a1fed2b5fd4bd9284f3ee))
    - Restore close_thread tool and tool call logging ([`7793dab`](https://github.com/hydro-project/infinity/commit/7793dab6706ea73b6d7a842e07701663441f5342))
    - Remove old implementation ([`cdcfa16`](https://github.com/hydro-project/infinity/commit/cdcfa167724f2ab18d3d91b822b9e05de9c2f233))
    - Stream output of LLM processor instead of accumulating text ([`8eeaefd`](https://github.com/hydro-project/infinity/commit/8eeaefdf4ee0dca2b62d487171f2329fd2d930bf))
    - Initial refactor to split out core runtime from Lambda ([`7242d5c`](https://github.com/hydro-project/infinity/commit/7242d5c2f4e145100ff28d544fe4206a432a625d))
</details>

