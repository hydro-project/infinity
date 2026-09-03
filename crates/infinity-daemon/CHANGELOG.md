

## v0.1.0 (2026-09-03)

### Chore

 - <csr-id-892cb628cb114102afa29c09e3e798c3dee1b381/> ensure `check.bash` passes
   On AL2023 dev machine.
   
   test(infinity-daemon): tolerate minor rasterization drift in web e2e screenshots
   
   The Playwright screenshot assertions used `max_diff_pixels(0)`, which failed on
   hosts where font rasterization drifts by a few pixels at glyph edges (16–108
   pixels after Playwright's default 0.2 per-pixel color threshold; at most 0.036%
   of raw pixels, confirmed to be antialiasing clusters around text).
   
   * Replace `max_diff_pixels(0)` with `max_diff_pixel_ratio(0.0005)` (0.05% of the
   frame) in `assert_screenshot` — ~4x headroom over the observed drift while
   still catching any real UI regression, which moves orders of magnitude more
   pixels. Ratio-based so it scales with snapshot size.
   * Update the comment to document the observed drift and rationale.
   * Remove stale `*-actual.png` / `*-diff.png` artifacts left behind by the
   failed runs in `tests/web_snapshots/`.
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

 - <csr-id-818871220e9769a5272d4c5336e8fed0ccec39b9/> virtualize chat-view diffs with Pierre Virtualizer
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
 - <csr-id-71602965b09106a3dfdeea1941238dc26188fadb/> model dropdown works before a session exists; drop hover background transition
 - <csr-id-1c4f71a611507dc7575c20b724faef680cbde2c7/> mid-session model switching per thread, with TUI + desktop UI and e2e tests
 - <csr-id-66ddd8ff3797df0284b0658382249133361b55d9/> add Claude Fable 5 to Bedrock models list
 - <csr-id-a20554d63a64440f1ac7d2aa810697e035712832/> pass CLI-selected model to new sessions
 - <csr-id-67d7c8a6341753270ad277620fc04be425698768/> defer subscription events during non-sleep async tool waits
   When the thread worker is waiting for an async tool result (e.g.
   execute_command, clone_repo), subscription events arriving on the input
   channel are now deferred to `pending_non_interrupt_items` instead of
   immediately triggering a new completion round.
   
   Subscription events are still processed immediately when:
   - The pending tool call is a sleep tool (sleep, sleep_until,
   sleep_until_event_or_input)
   - There is an active completion future (existing behavior)
   - No tool call is pending (e.g. after a completion finishes)
 - <csr-id-4804c9ae531d59dc577d499f8581bb640086a84e/> add archive session support with UI button and sidebar section
   * Add `ArchiveSession { session_id }` variant to `ClientMessage` in `infinity-protocol`
   * Handle `ArchiveSession` in daemon's `client_handler`: calls `cleanup_session`, then `mark_archived` + `save` on the session store
   * Add `strip_client_message` support for `ArchiveSession` (remote session ID prefix stripping)
   * Add archive button (SVG archive icon) in the top-right pill bar, visible when a session is connected
   * Split sidebar session list into active and archived sections
   * Add collapsible "▸ Archived (N)" toggle at the bottom of the sidebar to show/hide archived sessions (rendered at 60% opacity)
   * Add `ArchiveSession` to the TypeScript `ClientMessage` type
 - <csr-id-6abd457adc8b7c4ff5dcc62d575250d7f1736f2b/> add pretty-print display scripts for sleep tools
   - `SleepTool`: "Sleeping 30s" or "Sleeping 30s: waiting for deploy"
   - `SleepUntilTool`: "Sleeping until 2025-01-15 09:00 (US/Pacific)"
   - `SleepUntilEventOrInputTool`: "Sleeping until event or input"
   
   Added `display_script()` to all five sleep tool structs across
   `infinity-daemon`, `infinity-agent-lambda`, and `infinity-agent-core`.
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
 - <csr-id-bc19ab28e1ff552a4992812b75ffefd796973948/> serve web UI from daemon with `bundled-web` feature
 - <csr-id-0297d743512c02edd25a8ede1ee551ea65d878dc/> add directory tab completion in session picker
 - <csr-id-ea42de747adccf033b0be82f6d5131bd68e6408d/> require explicit `ssh` keyword after `--` in `remote add`
 - <csr-id-947b37af6289db10485ee7e0a4267333edc4bcef/> new session button uses location picker instead of local-only CWD picker
 - <csr-id-4169bdceccae28a77d664b9942758651defe8a0b/> add UserChoiceComplete daemon-to-client message
 - <csr-id-6dfa04add404a14ef1a48f11003026e160abf5ac/> auto-switch to migrated session in web UI
 - <csr-id-7085405bbfa8d07f6a69bc0e418761a56d108a67/> add RAP view_update protocol + diff view in web UI
 - <csr-id-ba10ffd62644a4c86c31a7fb6d5eaaca8c403b55/> add remote host migration UI and daemon orchestration

### Bug Fixes

 - <csr-id-d05419624342684d21b96d727d7a3085556d7593/> stop replays from duplicating the transcript on reconnect
   * `infinity-web/src/App.tsx`: a `Replay` always carries the complete
   transcript for the connected thread, so the handler now clears the
   message state (bumping the generation to invalidate MessageList's
   prefix cache), pending choices, and streaming flag before processing
   the replayed history. Previously the WS-reconnect path (`Welcome` →
   re-`Connect`) never reset the transcript — unlike `navigateTo` /
   `MigrateComplete` — so each reconnect appended a full extra copy of
   the history to the messages array and DOM, making the page
   progressively laggier with every disconnect/reconnect cycle. The
   input draft is deliberately preserved (unlike `resetMessages`).
   
   * `crates/infinity-daemon/src/session/thread_worker.rs`:
   `handle_subscribe` now replaces an existing subscription on the same
   channel (`same_channel`) instead of pushing a duplicate. The web
   client re-sends `Connect` every 5s until `Connected` arrives; when a
   resume was slow, each retry stacked another subscriber for the same
   client, causing every display event (and another full `Replay`) to be
   delivered N times from then on.
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
 - <csr-id-d56cfe2d01f2bd119082f994eefbd3f1e7db5ef0/> don't flush deferred subscription events while an async tool call is in flight
   A subscription event (or thread report) arriving while a non-sleep async
   tool call (e.g. a RAP `edit_file`) was awaiting its result could interrupt
   that call: `handle_content` injects a synthetic "Tool call interrupted by
   user" result and sends RAP cancellations, even though the tool actually
   completed — its real result arrives later and is dropped as stale.
   
   **Root cause:** `thread_worker` already defers deferrable synthetic events
   (subscription events / thread reports / parent messages) into
   `pending_non_interrupt_items` while waiting for an async tool result, but
   the unconditional `pending_non_interrupt_items.drain(..)` when building
   `all_inputs` flushed them anyway whenever *any* non-deferrable item was
   present — e.g. a stale/duplicate tool result. In the reported session the
   duplicate `tooluse_LVEs…` result caused the deferred `tooluse_6zyy…`
   subscription event to be flushed, interrupting the in-flight
   `tooluse_y9Yx…` `edit_file` call.
   
   Changes in `crates/infinity-daemon/src/session/thread_worker.rs`:
   
   * Add `pending_non_sleep_tool_call()` helper returning the id of the
   trailing unanswered non-sleep tool call; reuse it for the existing
   `waiting_for_non_sleep_tool` check
   * Guard the pending-items drain: deferred events are only flushed when it
   is safe — no non-sleep tool call is pending, or the batch settles it
   first (contains the call's actual tool result, or a user text input,
   which deliberately interrupts). Otherwise only non-deferrable pending
   items are processed and deferrable events stay queued for a later
   iteration
   * Add regression test
   `stale_result_does_not_flush_deferred_events_during_async_tool_wait`
   reproducing the log scenario (subscription event + stale tool result
   while an async tool is in flight); verified it fails without the fix
   and passes with it
   
   `./check.bash` passes. Note: the lambda path
   (`infinity-agent-lambda/src/event_handler.rs`) has no deferral mechanism
   at all and can theoretically hit the same interruption; fixing it would
   require re-enqueueing/delaying SQS messages and is left as a follow-up.
 - <csr-id-a536c9fa6d51bd2eaf0d5cf88af237ea1cce0e65/> reset context usage when compaction is applied so it does not re-trigger
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
 - <csr-id-be6bbd5ca0f907b3a75df4b5615a7181f756e18d/> compaction inside child thread no longer panics on indexing
   When compaction triggered inside a child thread, `safe_spawn_point()` returned
   an in-memory index that included ancestor messages prepended to the history.
   This index was used as `spawn_order_override` for the compaction grandchild,
   but the child's actual store only contained its own messages — causing a panic:
   "range end index X out of range for slice of length Y".
   
   The fix:
   - `load_history_with_ancestors` now returns `ancestor_prefix_len` as a third
   tuple element (how many messages at the front come from ancestor threads).
   - `HistoryManager` stores this in a `Cell<usize>` field.
   - `safe_spawn_point()` subtracts `ancestor_prefix_len` so the returned index
   is relative to the thread's own store.
   - `apply_compaction()` adds `ancestor_prefix_len` to the split position (since
   ancestors occupy the beginning of in-memory history) and resets it to 0 after
   compaction (ancestors are consumed into the summary).
   
   A regression test (`compaction_inside_child_thread_does_not_panic`) reproduces
   the exact panic from issue #31 and uses insta snapshots to verify both the
   compaction child's inherited history and the post-compaction history.
 - <csr-id-a1d3f5415b0f75bbe16e296a9e0a5d5f316fe41e/> spawned threads inherit parent's model instead of default
 - <csr-id-c2aab9e11055ad6d2cd80c7743be17504c27d56b/> persist in-flight tool results when a session is stopped
   Previously, stopping an agent while the model was processing a tool result
   lost that result: session_wrapper dropped the agent_loop future, closing each
   thread worker's input channel. The worker's `rx.recv()` arm reacted with a
   bare `return`, dropping the in-flight completion before pending history items
   (the tool result) could be synced to the conversation store.
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
 - <csr-id-a7b01f1d6db083e71274686ef83d8d344043d1eb/> recover sessions after daemon restart
   When the daemon restarts, sessions that were actively running (shut_down: false)
   could not be resumed because send_input only restarted sessions marked as
   shut_down. Now it restarts any session that exists in the store but isn't
   running in memory, regardless of the shut_down flag.
 - <csr-id-b959506eea3eb763bb8a6699dd6a5f37f9fe7a98/> prevent compaction from truncating pending tool calls
   When auto-compaction triggers while an async tool call is pending, the
   compaction summary's `up_to_order` previously included the trailing
   unanswered tool call. After `apply_compaction`, the tool call was gone
   from history, causing the subsequent tool result to be orphaned (no
   matching tool call in the conversation sent to the LLM).
 - <csr-id-8acdad4371582a557ccf5e0ce3be873bb9e8e97b/> fix build to also install infinity-ui deps
 - <csr-id-eb9404f66f01aadc31270571e4e0008ad59ea234/> log communication failures instead of swallowing
 - <csr-id-a9ca556512f5754b76d20533b9dd5e94b836d9cd/> defer thread reports and parent messages during async tool waits
   The `is_subscription_event` check only matched `SubscriptionEvent` synthetic
   messages, so `ThreadReport` (from `report_to_parent`) and `ParentMessage`
   (from `send_message_to_child`) messages were not deferred when a non-sleep
   async tool call was pending. This caused child thread reports to interrupt
   active tool calls.
   
   * Renamed `is_subscription_event` → `is_deferrable_synthetic_event` and
   extended the match to include `ThreadReport` and `ParentMessage` variants.
   * Updated both call sites and their comments.
   * Added `thread_report_deferred_during_async_tool_wait` test that verifies
   a thread report arriving during an async tool wait is deferred and then
   included in the next completion batch alongside the tool result.
 - <csr-id-935f849fe7fadd90cb92744aa7463309b6b86ab7/> handle disconnected remote sessions gracefully
   * When a remote proxy connection closes, break out of the client handler
   loop so the WS drops and the client auto-reconnects properly.
   
   * When the daemon receives a `UserInput` with a remote-prefixed session
   ID and there is no active remote proxy, return "remote is not connected"
   instead of falling through to the local session manager.
   
   * In the web UI, gray out sessions in the sidebar when their remote is
   not connected.
   
   * In the web UI, disable the input box until a `Connected` message is
   received (covers both local and remote sessions). On WS reconnect,
   re-send `Connect` for the previously viewed session.
   
   * Add a 5-second retry timer for `Connect` — if `Connected` is not
   received within 5s, re-send the `Connect` message. The timer is
   cleared on `Connected`, disconnect, or session change.
 - <csr-id-fe820d8894b7768579245399b9b157e280b87bea/> embed subscription invocation inside SubscriptionEvent to prevent duplicate replay entries
 - <csr-id-6daafa1d376bc019911394f98bb80f3ca45dc968/> new sessions sort to top of session list in other clients
 - <csr-id-ecd7f72d7677a85fb7abcacabd967756548dc130/> pass session_id and thread_id separately when connecting to remote subthreads
 - <csr-id-430a06fe7bfd234a98e36bc8a8c03451ab368fc0/> strip remote prefix from thread_id in Connect handler
 - <csr-id-19514793fe837eabdd0be96cfeca053e31fdb52c/> include views in thread snapshot serialization
 - <csr-id-44fcca250a44029e36b49df6013a049d33bc985f/> log panics from fire-and-forget spawned tasks instead of silently swallowing them
 - <csr-id-916057d8289b3a52524e21bcd825489ee1f73e3c/> use typed DisplaySegment and improve error logging
 - <csr-id-9ef4823d5d9826dac71eb9a50a4520035549178e/> use typed DisplaySegment for callback serialization
 - <csr-id-b40442e37ac91b884f51fcabb018a3735bdf612f/> hanging caused by `sh -c` intercepting SIGINT, improved config error handling
   Fixes a (the?) hanging issue encountered on Ubuntu 24.04

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
 - <csr-id-c72f4cff47d1a3edd4020e26f7bf543da082b68d/> don't auto-boot idle/stopped sessions on Connect; defer to first UserInput

### Refactor

 - <csr-id-a84b99e871770df5fa923e1b8881c3e07486baf0/> don't commit turn data to history until the turn is completed.
   There was a bug encountered where after a timeout the bedrock api would reject a retried request because the request did not end in a user message.
   
   There was already some commit-then-rollback-on-error kind of logic but that's kind of fragile so this revision changes it to buffer up the data and only commit it when the turn completes.
   
   
   fix(infinity-agent-core): trim trailing reasoning on abandoned turns
   
   Addresses PR #63 review: the terminal flush paths (user cancellation,
   retries exhausted) committed the partial turn verbatim, which could leave
   history ending on a reasoning block. The next input is a fresh user turn,
   and user-input-after-reasoning is rejected by some providers. Adds
   flush_turn_trimming_reasoning(), which keeps the visible partial text but
   drops trailing reasoning/empty-text before committing, restoring the
   pre-refactor remove_trailing_reasoning behavior on these paths.
 - <csr-id-24fa6cbf5564d4df2297451bdc76c9619ec741fe/> drop "Using provider" info message, show provider_id in status displays
 - <csr-id-53e7ef6c60baca2442de2be8d31d82094f50f410/> introduce InfinityMessage to replace bare rig Message in conversation storage
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
 - <csr-id-3de5ccb2b52057ec67d5eee4314a1bf136e61f0e/> mark compaction_during_tool_call test as ignored (pending fix)
 - <csr-id-c7ba255cd41e31de8e4bc38f01f7681797367dd6/> reproduce compaction-during-tool-call history corruption bug
 - <csr-id-f0e04cf86af4e871f8848505208c0460a8f3907a/> add integration test for list_tools callback deserialization

### New Features (BREAKING)

 - <csr-id-8bef2c534f90b7fe038cb6dda1fb2015fa9e737d/> add high-level agent system API
   Add ergonomic local agent-system APIs on top of the engine extracted in #96:
   
   - static builder conveniences for tools, prompts, and RAP notification;
   - channel-backed `ThreadHandle`s for sending inputs and streaming events;
   - launcher mode and `ThreadBuilder` for per-thread tools, prompts, and models;
   - root-based configuration inheritance for child threads;
   - direct local `McpToolSet` and `RapToolSet` adapters;
   - usage-oriented high-level and low-level documentation.
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

### Performance (BREAKING)

 - <csr-id-9c073687aaf9d38799d87f5157bbc7a01efadffe/> cut idle-agent memory ~30% — seventy thousand agents in 8 GB
   Optimizes per-agent resident memory for the `agent_scale` benchmark added by the parent commit: 154 → 107.4 KB per idle agent (measured slope), with 80,000 agents × 20 turns fitting in 8.89 GB and the 8 GB line crossed at ~71,700 agents. Three independent optimizations:
   
   ## Router owns driver futures directly (no `spawn_local` per driver)
   
   * `route_loop` drives all thread drivers through one `FuturesUnordered` pool instead of spawning each as its own `LocalSet` task. When a driver goes idle, its future yields its thread ID and the router immediately frees the future and the worker entry (input/subscribe channels).
   * Previously a finished driver's `JoinHandle`, task allocation, and channel blocks were retained in the `workers` map until the thread's *next* message — ~13 KB per idle agent.
   * Panic isolation preserved (each pooled future is wrapped in `rap_protocol::log_panic`); shutdown wind-down drains the pool.
   
   ## `InfinityMessage::SubscriptionEvent` payloads boxed
   
   * Boxed `result` and `invocation` in the rare `SubscriptionEvent` variant, shrinking `size_of::<InfinityMessage>()` from 352 → 184 bytes; every stored history message previously paid for the fattest variant inline. `Box` is serde-transparent so the persisted format is unchanged.
   * Added `InfinityMessage::tool_result()` helper. (Boxing only `invocation` was measured and rejected: the enum grows to 200 bytes because `SubscriptionEvent` with an inline result becomes the largest variant.)
   
   ## Tool-call dedup derived from history instead of a durable index (BREAKING)
   
   * `HistoryManager` no longer maintains `processed_tool_calls` / `pending_complete_tool_calls`. Incoming tool results are deduplicated by walking the history tail just in time: scan back across trailing tool calls/results (future-proof for concurrent calls, e.g. `tc tc tr tr`), accept on a matching unanswered call, reject as duplicate on a matching result, discard as stale on any other message (user text, assistant content, subscription events — all turn boundaries, since a subscription event is only injected once pending calls are settled).
   * `safe_spawn_point` uses the same walk, tracking answered calls during the scan.
   * Durable message-ID dedup is limited to inputs that are not naturally idempotent: user text and subscription events (a redelivered subscription event would mint a fresh injected invocation). Tool results and assistant/tool-call items no longer persist IDs.
   * **BREAKING**: `StateStore::get_processed_ids` returns a single `HashSet<String>`; `add_processed_tool_calls` removed. Updated `InMemoryStateStore`, the daemon's `PersistentStateStore`, and the Lambda `DynamoDbStateStore` (old DynamoDB `processed_tool_calls` attributes are ignored). `ThreadState` drops its `processed_tool_call_ids` field entirely — serde ignores unknown fields by default, so snapshots from older versions still deserialize.
   
   ## Benchmark & docs
   
   * `agent_scale` drains lifecycle notifications per wave (like a real embedding) so they aren't counted as per-agent memory.
   * Landing page: `MemoryChart` regenerated from an 80,000-agent run (8.89 GB total, 108.5 KB/agent); hero, chapter title, and copy updated to "seventy thousand agents on a Raspberry Pi" (measured 8 GB crossing: ~71,700).
   * `history-manager.md` updated for the new dedup model.
   
   All workspace tests pass; clippy clean.

### Refactor (BREAKING)

 - <csr-id-4b18b37de219cb7fe27ce7c027b87f4fb35fbbf5/> introduce typed ThreadId for RAP group ids
   * refactor(rap-protocol)!: introduce typed ThreadId for RAP group ids
   
   Stage 1 of the string-ID to typed-ID migration (#108). Defines `ThreadId`
   via the published `strkind` 0.0.1 macro and converts every RAP `group_id`
   on the wire types, plus all consumers. Serialization is transparent, so
   the wire format and persisted metadata are byte-identical (verified: no
   insta snapshot changes; full test suite passes).
   
   * rap-protocol: `strkind! { pub ThreadId; }` with docs (RAP calls it
   `group_id` on the wire; UUIDs in the daemon, caller-chosen conversation
   keys in the Lambda runtime). `group_id: ThreadId` on `RapInvocation`,
   `RapToolResult`, `RapUserChoice`, `RapSubscriptionEvent`,
   `RapViewUpdate`, `RapOAuth`; `thread_ancestors: Option<Vec<ThreadId>>`;
   `send_subscription_event`/`send_view_update` take `ThreadId`
   * sandbox-core/local/remote (via sub-agent): `ThreadId` re-exported from
   sandbox-core root; `MetadataStore::get/delete(&ThreadId)`;
   `SandboxBackend::push_sandbox`/`cleanup_sandbox_permanently` typed;
   `RepoState.{group_id, root_thread_id}`, `CloneRepoArgs.base_thread_id`,
   `SquashSandboxArgs.from_thread_id`, `CloneContext.group_id`,
   `SandboxError::RepoNotFound`, and server request payload structs typed
   * infinity-agent-core: rap_tool boundary converts `ToolContext` strings
   into `ThreadId` when building invocations (context types themselves are
   Stage 2)
   * infinity-rap-bridge: callback conversion unwraps `ThreadId` into
   `InputMessage.group_id` (String until Stage 2)
   * rap-github-event-poller: `Subscription.group_id: ThreadId`
   * infinity-daemon: view-update routing + test fixture conversions
   * Workspace: `strkind = "0.0.1"` added to workspace dependencies
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

 - 104 commits contributed to the release.
 - 68 commits were understood as [conventional](https://www.conventionalcommits.org).
 - 32 unique issues were worked on: [#10](https://github.com/hydro-project/infinity/issues/10), [#105](https://github.com/hydro-project/infinity/issues/105), [#107](https://github.com/hydro-project/infinity/issues/107), [#110](https://github.com/hydro-project/infinity/issues/110), [#113](https://github.com/hydro-project/infinity/issues/113), [#13](https://github.com/hydro-project/infinity/issues/13), [#15](https://github.com/hydro-project/infinity/issues/15), [#18](https://github.com/hydro-project/infinity/issues/18), [#19](https://github.com/hydro-project/infinity/issues/19), [#22](https://github.com/hydro-project/infinity/issues/22), [#24](https://github.com/hydro-project/infinity/issues/24), [#29](https://github.com/hydro-project/infinity/issues/29), [#33](https://github.com/hydro-project/infinity/issues/33), [#39](https://github.com/hydro-project/infinity/issues/39), [#52](https://github.com/hydro-project/infinity/issues/52), [#53](https://github.com/hydro-project/infinity/issues/53), [#55](https://github.com/hydro-project/infinity/issues/55), [#59](https://github.com/hydro-project/infinity/issues/59), [#60](https://github.com/hydro-project/infinity/issues/60), [#61](https://github.com/hydro-project/infinity/issues/61), [#63](https://github.com/hydro-project/infinity/issues/63), [#67](https://github.com/hydro-project/infinity/issues/67), [#71](https://github.com/hydro-project/infinity/issues/71), [#73](https://github.com/hydro-project/infinity/issues/73), [#74](https://github.com/hydro-project/infinity/issues/74), [#8](https://github.com/hydro-project/infinity/issues/8), [#83](https://github.com/hydro-project/infinity/issues/83), [#85](https://github.com/hydro-project/infinity/issues/85), [#90](https://github.com/hydro-project/infinity/issues/90), [#92](https://github.com/hydro-project/infinity/issues/92), [#94](https://github.com/hydro-project/infinity/issues/94), [#96](https://github.com/hydro-project/infinity/issues/96)

### Commit Details

<csr-read-only-do-not-edit/>

<details><summary>view details</summary>

 * **[#10](https://github.com/hydro-project/infinity/issues/10)**
    - Add GitHub Actions workflows for lints, tests, conventional commits, and docs ([`ea6b62e`](https://github.com/hydro-project/infinity/commit/ea6b62e7b00f2a6b7e7338fa12e60fb3a46bb012))
 * **[#105](https://github.com/hydro-project/infinity/issues/105)**
    - Cut idle-agent memory ~30% — seventy thousand agents in 8 GB ([`9c07368`](https://github.com/hydro-project/infinity/commit/9c073687aaf9d38799d87f5157bbc7a01efadffe))
 * **[#107](https://github.com/hydro-project/infinity/issues/107)**
    - Set up cargo-smart-release release workflow (mirroring hydro) ([`ffc27d0`](https://github.com/hydro-project/infinity/commit/ffc27d0bf5d964a655fedab9460bf5017971e6b6))
 * **[#110](https://github.com/hydro-project/infinity/issues/110)**
    - Rig-free provider stack, native Bedrock, minimal deps; refreshed scale claims ([`49ad32e`](https://github.com/hydro-project/infinity/commit/49ad32e467d92f82cdac76095b6cb0a3daf2f964))
 * **[#113](https://github.com/hydro-project/infinity/issues/113)**
    - Introduce typed ThreadId for RAP group ids ([`4b18b37`](https://github.com/hydro-project/infinity/commit/4b18b37de219cb7fe27ce7c027b87f4fb35fbbf5))
 * **[#13](https://github.com/hydro-project/infinity/issues/13)**
    - Add Claude Fable 5 to Bedrock models list ([`66ddd8f`](https://github.com/hydro-project/infinity/commit/66ddd8ff3797df0284b0658382249133361b55d9))
 * **[#15](https://github.com/hydro-project/infinity/issues/15)**
    - Pass CLI-selected model to new sessions ([`a20554d`](https://github.com/hydro-project/infinity/commit/a20554d63a64440f1ac7d2aa810697e035712832))
 * **[#18](https://github.com/hydro-project/infinity/issues/18)**
    - Make model providers extensible via a dyn-compatible `ModelProvider` trait ([`b4a31e2`](https://github.com/hydro-project/infinity/commit/b4a31e2925c371f38b85b8b2e878fdd226566766))
 * **[#19](https://github.com/hydro-project/infinity/issues/19)**
    - Run model providers as configurable separate processes over Unix sockets ([`84f7aff`](https://github.com/hydro-project/infinity/commit/84f7aff103f885169f4a6f4ba34aca3af9111a91))
 * **[#22](https://github.com/hydro-project/infinity/issues/22)**
    - Context usage resets to zero after session replay ([`c49fc7d`](https://github.com/hydro-project/infinity/commit/c49fc7d0e78a22c2f0f8f6c84878e4e6a3dcfe35))
 * **[#24](https://github.com/hydro-project/infinity/issues/24)**
    - Persist in-flight tool results when a session is stopped ([`c2aab9e`](https://github.com/hydro-project/infinity/commit/c2aab9e11055ad6d2cd80c7743be17504c27d56b))
 * **[#29](https://github.com/hydro-project/infinity/issues/29)**
    - Drop "Using provider" info message, show provider_id in status displays ([`24fa6cb`](https://github.com/hydro-project/infinity/commit/24fa6cbf5564d4df2297451bdc76c9619ec741fe))
 * **[#33](https://github.com/hydro-project/infinity/issues/33)**
    - Spawned threads inherit parent's model instead of default ([`a1d3f54`](https://github.com/hydro-project/infinity/commit/a1d3f5415b0f75bbe16e296a9e0a5d5f316fe41e))
 * **[#39](https://github.com/hydro-project/infinity/issues/39)**
    - Compaction inside child thread no longer panics on indexing ([`be6bbd5`](https://github.com/hydro-project/infinity/commit/be6bbd5ca0f907b3a75df4b5615a7181f756e18d))
 * **[#52](https://github.com/hydro-project/infinity/issues/52)**
    - Use total_tokens for context usage and compaction trigger ([`b7a9805`](https://github.com/hydro-project/infinity/commit/b7a980585d981b1ae22f1bb4fad12b739202b524))
 * **[#53](https://github.com/hydro-project/infinity/issues/53)**
    - Increase LengthDelimitedCodec max frame size to 256 MiB ([`8ad86d8`](https://github.com/hydro-project/infinity/commit/8ad86d850d761c58669dffb906ef389654e4990d))
 * **[#55](https://github.com/hydro-project/infinity/issues/55)**
    - Add deterministic e2e tests for TUI↔daemon and web UI (Playwright) ([`646c8f3`](https://github.com/hydro-project/infinity/commit/646c8f3dfcbb352369e70022cab1292cbbc49384))
 * **[#59](https://github.com/hydro-project/infinity/issues/59)**
    - Don't flush deferred subscription events while an async tool call is in flight ([`d56cfe2`](https://github.com/hydro-project/infinity/commit/d56cfe2d01f2bd119082f994eefbd3f1e7db5ef0))
 * **[#60](https://github.com/hydro-project/infinity/issues/60)**
    - Replay in-progress thinking and response state to clients attaching mid-response ([`7a6e971`](https://github.com/hydro-project/infinity/commit/7a6e9715a7b602d0a04bc527a3c76f4c6a1ccd80))
 * **[#61](https://github.com/hydro-project/infinity/issues/61)**
    - Multimodal (image) tool results end-to-end, with image display + review fixes ([`1935c38`](https://github.com/hydro-project/infinity/commit/1935c387d806a1da271e15078b26e06f228737c6))
 * **[#63](https://github.com/hydro-project/infinity/issues/63)**
    - Don't commit turn data to history until the turn is completed. ([`a84b99e`](https://github.com/hydro-project/infinity/commit/a84b99e871770df5fa923e1b8881c3e07486baf0))
 * **[#67](https://github.com/hydro-project/infinity/issues/67)**
    - Mid-session model switching per thread, with TUI + desktop UI and e2e tests ([`1c4f71a`](https://github.com/hydro-project/infinity/commit/1c4f71a611507dc7575c20b724faef680cbde2c7))
 * **[#71](https://github.com/hydro-project/infinity/issues/71)**
    - Extract provider protocol into `infinity-provider-protocol` crate ([`27b40fe`](https://github.com/hydro-project/infinity/commit/27b40fed6c5fd1fad5ebfabb1a2a909b7018a0cf))
 * **[#73](https://github.com/hydro-project/infinity/issues/73)**
    - Model dropdown works before a session exists; drop hover background transition ([`7160296`](https://github.com/hydro-project/infinity/commit/71602965b09106a3dfdeea1941238dc26188fadb))
 * **[#74](https://github.com/hydro-project/infinity/issues/74)**
    - Reset context usage when compaction is applied so it does not re-trigger ([`a536c9f`](https://github.com/hydro-project/infinity/commit/a536c9fa6d51bd2eaf0d5cf88af237ea1cce0e65))
 * **[#8](https://github.com/hydro-project/infinity/issues/8)**
    - Add automated THIRD-PARTY file generation with license enforcement ([`e2e0719`](https://github.com/hydro-project/infinity/commit/e2e0719faebbffc72ec7bd8a8b3b02223da8ba0e))
 * **[#83](https://github.com/hydro-project/infinity/issues/83)**
    - Ensure `check.bash` passes ([`892cb62`](https://github.com/hydro-project/infinity/commit/892cb628cb114102afa29c09e3e798c3dee1b381))
 * **[#85](https://github.com/hydro-project/infinity/issues/85)**
    - Virtualize chat-view diffs with Pierre Virtualizer ([`8188712`](https://github.com/hydro-project/infinity/commit/818871220e9769a5272d4c5336e8fed0ccec39b9))
 * **[#90](https://github.com/hydro-project/infinity/issues/90)**
    - Add `keeps_session_alive` flag to prevent non-interactive clients from blocking idle shutdown ([`1b20fda`](https://github.com/hydro-project/infinity/commit/1b20fdac512ea534ee24006b95903f3961ff5179))
 * **[#92](https://github.com/hydro-project/infinity/issues/92)**
    - Add high-level agent system API ([`8bef2c5`](https://github.com/hydro-project/infinity/commit/8bef2c534f90b7fe038cb6dda1fb2015fa9e737d))
 * **[#94](https://github.com/hydro-project/infinity/issues/94)**
    - Stop replays from duplicating the transcript on reconnect ([`d054196`](https://github.com/hydro-project/infinity/commit/d05419624342684d21b96d727d7a3085556d7593))
 * **[#96](https://github.com/hydro-project/infinity/issues/96)**
    - Extract shared agent system engine ([`9c921fd`](https://github.com/hydro-project/infinity/commit/9c921fde280b50c89c3e5b9caadccf83a46078a4))
 * **Uncategorized**
    - Release rap-protocol v0.1.0, rap-client v0.1.0, rap-steering-server v0.1.0, rap-github-event-poller v0.1.0, infinity-protocol v0.1.0, infinity-provider-protocol v0.1.0, infinity-provider-bedrock v0.1.0, infinity-provider-rig v0.1.0, infinity-agent-core v0.1.0, infinity-mcp-bridge v0.1.0, infinity-rap-bridge v0.1.0, infinity-daemon v0.1.0, infinity-agent-cli v0.1.0, sandbox-core v0.1.0, sandbox-local v0.1.0, sandbox-remote v0.1.0 ([`dd8c7f4`](https://github.com/hydro-project/infinity/commit/dd8c7f49028a26052d785b4241f9ade125f0afb3))
    - Feat(infinity-daemon): improve error message when listing models fails PR: #27 ([`2f9a1ed`](https://github.com/hydro-project/infinity/commit/2f9a1ed1067e3843416890937f8ffae74403fb0f))
    - Recover sessions after daemon restart ([`a7b01f1`](https://github.com/hydro-project/infinity/commit/a7b01f1d6db083e71274686ef83d8d344043d1eb))
    - Prevent compaction from truncating pending tool calls ([`b959506`](https://github.com/hydro-project/infinity/commit/b959506eea3eb763bb8a6699dd6a5f37f9fe7a98))
    - Mark compaction_during_tool_call test as ignored (pending fix) ([`3de5ccb`](https://github.com/hydro-project/infinity/commit/3de5ccb2b52057ec67d5eee4314a1bf136e61f0e))
    - Reproduce compaction-during-tool-call history corruption bug ([`c7ba255`](https://github.com/hydro-project/infinity/commit/c7ba255cd41e31de8e4bc38f01f7681797367dd6))
    - Fix build to also install infinity-ui deps ([`8acdad4`](https://github.com/hydro-project/infinity/commit/8acdad4371582a557ccf5e0ce3be873bb9e8e97b))
    - Log communication failures instead of swallowing ([`eb9404f`](https://github.com/hydro-project/infinity/commit/eb9404f66f01aadc31270571e4e0008ad59ea234))
    - Defer thread reports and parent messages during async tool waits ([`a9ca556`](https://github.com/hydro-project/infinity/commit/a9ca556512f5754b76d20533b9dd5e94b836d9cd))
    - Handle disconnected remote sessions gracefully ([`935f849`](https://github.com/hydro-project/infinity/commit/935f849fe7fadd90cb92744aa7463309b6b86ab7))
    - Defer subscription events during non-sleep async tool waits ([`67d7c8a`](https://github.com/hydro-project/infinity/commit/67d7c8a6341753270ad277620fc04be425698768))
    - Add archive session support with UI button and sidebar section ([`4804c9a`](https://github.com/hydro-project/infinity/commit/4804c9ae531d59dc577d499f8581bb640086a84e))
    - Add pretty-print display scripts for sleep tools ([`6abd457`](https://github.com/hydro-project/infinity/commit/6abd457adc8b7c4ff5dcc62d575250d7f1736f2b))
    - Show choice picker alongside input and cancel choices on tool interruption ([`59d3314`](https://github.com/hydro-project/infinity/commit/59d331491087ef43aa3cea9215a94c2089675b30))
    - Serve web UI from daemon with `bundled-web` feature ([`bc19ab2`](https://github.com/hydro-project/infinity/commit/bc19ab28e1ff552a4992812b75ffefd796973948))
    - Embed subscription invocation inside SubscriptionEvent to prevent duplicate replay entries ([`fe820d8`](https://github.com/hydro-project/infinity/commit/fe820d8894b7768579245399b9b157e280b87bea))
    - Ignore trailing forward slash when sorting items ([`1abcb55`](https://github.com/hydro-project/infinity/commit/1abcb555d305c536170e0a89b5fbe141b79370ab))
    - Add directory tab completion in session picker ([`0297d74`](https://github.com/hydro-project/infinity/commit/0297d743512c02edd25a8ede1ee551ea65d878dc))
    - Introduce InfinityMessage to replace bare rig Message in conversation storage ([`53e7ef6`](https://github.com/hydro-project/infinity/commit/53e7ef6c60baca2442de2be8d31d82094f50f410))
    - Require explicit `ssh` keyword after `--` in `remote add` ([`ea42de7`](https://github.com/hydro-project/infinity/commit/ea42de747adccf033b0be82f6d5131bd68e6408d))
    - New session button uses location picker instead of local-only CWD picker ([`947b37a`](https://github.com/hydro-project/infinity/commit/947b37af6289db10485ee7e0a4267333edc4bcef))
    - Add UserChoiceComplete daemon-to-client message ([`4169bdc`](https://github.com/hydro-project/infinity/commit/4169bdceccae28a77d664b9942758651defe8a0b))
    - New sessions sort to top of session list in other clients ([`6daafa1`](https://github.com/hydro-project/infinity/commit/6daafa1d376bc019911394f98bb80f3ca45dc968))
    - Add workspace lints and fix all lint violations ([`b92b7a1`](https://github.com/hydro-project/infinity/commit/b92b7a17f4b69e2652f5cce813320eca851717e4))
    - Auto-switch to migrated session in web UI ([`6dfa04a`](https://github.com/hydro-project/infinity/commit/6dfa04add404a14ef1a48f11003026e160abf5ac))
    - Pass session_id and thread_id separately when connecting to remote subthreads ([`ecd7f72`](https://github.com/hydro-project/infinity/commit/ecd7f72d7677a85fb7abcacabd967756548dc130))
    - Strip remote prefix from thread_id in Connect handler ([`430a06f`](https://github.com/hydro-project/infinity/commit/430a06fe7bfd234a98e36bc8a8c03451ab368fc0))
    - Include views in thread snapshot serialization ([`1951479`](https://github.com/hydro-project/infinity/commit/19514793fe837eabdd0be96cfeca053e31fdb52c))
    - Add RAP view_update protocol + diff view in web UI ([`7085405`](https://github.com/hydro-project/infinity/commit/7085405bbfa8d07f6a69bc0e418761a56d108a67))
    - Add remote host migration UI and daemon orchestration ([`ba10ffd`](https://github.com/hydro-project/infinity/commit/ba10ffd62644a4c86c31a7fb6d5eaaca8c403b55))
    - Log panics from fire-and-forget spawned tasks instead of silently swallowing them ([`44fcca2`](https://github.com/hydro-project/infinity/commit/44fcca250a44029e36b49df6013a049d33bc985f))
    - Set tokens used and last updated for only the current thread ([`925c6a5`](https://github.com/hydro-project/infinity/commit/925c6a584f16e9e1843836bd14ec24bec0b84351))
    - Fixup previous 9067f810 ([`bf1fe18`](https://github.com/hydro-project/infinity/commit/bf1fe184da75a754bf5106f1a26aaaf4982e8d05))
    - Add support for connecting to remote sessions via your local daemon ([`67f4085`](https://github.com/hydro-project/infinity/commit/67f40855a59ac5263ec3f3726c69017c4cd0b464))
    - Replace all .unwrap() with .expect() and fix clippy warnings ([`7634b82`](https://github.com/hydro-project/infinity/commit/7634b823ad70378e666379a9a8e8a7935a06026f))
    - Add precheck script, lints ([`9757071`](https://github.com/hydro-project/infinity/commit/9757071818663cefb8e6a12438071d95000379a8))
    - Use typed DisplaySegment and improve error logging ([`916057d`](https://github.com/hydro-project/infinity/commit/916057d8289b3a52524e21bcd825489ee1f73e3c))
    - Use typed DisplaySegment for callback serialization ([`9ef4823`](https://github.com/hydro-project/infinity/commit/9ef4823d5d9826dac71eb9a50a4520035549178e))
    - Add integration test for list_tools callback deserialization ([`f0e04cf`](https://github.com/hydro-project/infinity/commit/f0e04cf86af4e871f8848505208c0460a8f3907a))
    - Fix threads being displayed as idle when client disconnects ([`90aa1ff`](https://github.com/hydro-project/infinity/commit/90aa1ffb594bf53e30525e578b4a32c51037f0f2))
    - Change idle_tx semantics to "might be idle" and ping on client disconnect ([`3127d65`](https://github.com/hydro-project/infinity/commit/3127d65607bede5077d0a675c67f294d98f7e177))
    - Replace bincode with serde_json for CLI↔daemon unix socket wire format ([`62f3822`](https://github.com/hydro-project/infinity/commit/62f382276e6fc8ee76888ac1c629538c977e1745))
    - Introduce display_as typed variants and use Pierre to display in web client ([`1e65518`](https://github.com/hydro-project/infinity/commit/1e65518e4f041f76e6359b08ff88e32fc8753cda))
    - Replace idle_cleaned with idle flag; yellow idle dot in webui ([`e08bb90`](https://github.com/hydro-project/infinity/commit/e08bb90469ffb66e1870226167b8edcda3b789b7))
    - Don't auto-boot idle/stopped sessions on Connect; defer to first UserInput ([`c72f4cf`](https://github.com/hydro-project/infinity/commit/c72f4cff47d1a3edd4020e26f7bf543da082b68d))
    - Fix broken spawn_local tests in infinity-agent-core and infinity-daemon ([`c85fc1a`](https://github.com/hydro-project/infinity/commit/c85fc1a13893c94a77e88805e3c986aa1741d75a))
    - Move HistoryManager to interior mutability; remove callback_with_history hack; restore subscribe_rx in select ([`1e92087`](https://github.com/hydro-project/infinity/commit/1e9208751e55d0029acd419ae12f1bf05cc7104e))
    - Display subthreads in web UI and make it possible to connect to subthreads directly ([`718509d`](https://github.com/hydro-project/infinity/commit/718509d481340bd43497530b3f1212b3f3be27af))
    - Keep agent running in background when client disconnects ([`b2db0fe`](https://github.com/hydro-project/infinity/commit/b2db0fe95d2c24ec0d89da846543fedb97788d1d))
    - Use display_as for tool call pretty-printing in web UI ([`1e4a489`](https://github.com/hydro-project/infinity/commit/1e4a4894ce62c05ab6561539ff3e9a8abf662974))
    - Fix info log events not streaming during idle session restart ([`ac4bf76`](https://github.com/hydro-project/infinity/commit/ac4bf76db0be6d5098be17f3f9cea184365104e6))
    - Extract rap-client crate and unify RAP protocol types ([`51406e4`](https://github.com/hydro-project/infinity/commit/51406e4dfab243a4400027507f446862b26ce8d3))
    - Add WebSocket server to infinity daemon and Vite React web UI ([`ad1cda0`](https://github.com/hydro-project/infinity/commit/ad1cda0ff31741e8ea9f58776513ae98db5d6f7c))
    - Make sure RAP loading logs are printed when re-connecting to an idled agent ([`9517323`](https://github.com/hydro-project/infinity/commit/95173236d3ceb6e677adf7597b54c3a1dd34a304))
    - Unify all duplicate RAP protocol types into rap-protocol crate ([`2def5ee`](https://github.com/hydro-project/infinity/commit/2def5eec01a5c197432a7959942cca8b0eb9d6a0))
    - Unify RapInvocation into a single type in rap-protocol ([`e14509e`](https://github.com/hydro-project/infinity/commit/e14509ecf6e6bf622d6ca0a1252148b647c1ef7f))
    - Fix manual compaction and add background auto-compaction triggers ([`9b10a09`](https://github.com/hydro-project/infinity/commit/9b10a0977283f5f628142841cf9515a8b8793793))
    - Hanging caused by `sh -c` intercepting SIGINT, improved config error handling ([`b40442e`](https://github.com/hydro-project/infinity/commit/b40442e37ac91b884f51fcabb018a3735bdf612f))
    - Add rig-mock crate and test suite for agent core and daemon ([`abda067`](https://github.com/hydro-project/infinity/commit/abda06757eeba0ac7817374bc89155211cd2edcd))
    - Fix send_input to resolve child thread IDs to root session ID ([`70dab05`](https://github.com/hydro-project/infinity/commit/70dab057e9589779bd3d6abdd1689bb075f626b4))
    - Fix thread_worker idle detection for close_thread tool calls ([`43078d9`](https://github.com/hydro-project/infinity/commit/43078d90a057aa00eb2f70b113d8a9641b9b893f))
    - Add support for UserChoice prompts in RAP protocol and use for permissions expansion in sandbox ([`b0db6a7`](https://github.com/hydro-project/infinity/commit/b0db6a7a0764ddab7df1f5cf3fcefc7129c6ddcb))
    - Rewrite session idle management, input handling, to allow dropping resources on background idle ([`527cd09`](https://github.com/hydro-project/infinity/commit/527cd097895fb761869915844160834e38350553))
    - Add session status (running/idle/stopped) to CLI session list ([`56dd66d`](https://github.com/hydro-project/infinity/commit/56dd66d112dc068524573916a9183fe11f18b999))
    - No need to detach session before cleaning it up ([`bd07c46`](https://github.com/hydro-project/infinity/commit/bd07c460de023eb1adcd75aa22a434be635abd29))
    - Clean up idle sessions that are soft detached ([`e3c9645`](https://github.com/hydro-project/infinity/commit/e3c9645151b61d28575d5631556fe1a754e8a10a))
    - Make InMemoryStateStore a single global instance on SessionManager ([`2942253`](https://github.com/hydro-project/infinity/commit/29422534459f49ce4e84e1131d1f2fcca1d8a9af))
    - Refactor thread processing loop to improve clarity ([`1d72b0b`](https://github.com/hydro-project/infinity/commit/1d72b0bfd4b9408fbb95dc4c9428a89a24eef7f9))
    - Fix auto-exit on idle: send DetachedIdle message instead of closing connection ([`1478ba4`](https://github.com/hydro-project/infinity/commit/1478ba404d1653d5ae750ca5ebb990cd207071d3))
    - Allow auto quit without quit picker when agent is idle ([`3285dc5`](https://github.com/hydro-project/infinity/commit/3285dc5078947b76ad440342316dbd1d665800f4))
    - Add quit picker for graceful disconnect choice; cleanup on ungraceful disconnect ([`d87d7d3`](https://github.com/hydro-project/infinity/commit/d87d7d34130e9d2b5feda891bdc63267fc0689eb))
    - Shift core agent runtime into a daemon with a network protocol for clients ([`141d697`](https://github.com/hydro-project/infinity/commit/141d69792c3aa951fcbfbea847879582f1d06ec3))
</details>

