

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

 - <csr-id-1c4f71a611507dc7575c20b724faef680cbde2c7/> mid-session model switching per thread, with TUI + desktop UI and e2e tests
 - <csr-id-a20554d63a64440f1ac7d2aa810697e035712832/> pass CLI-selected model to new sessions
 - <csr-id-4804c9ae531d59dc577d499f8581bb640086a84e/> add archive session support with UI button and sidebar section
   * Add `ArchiveSession { session_id }` variant to `ClientMessage` in `infinity-protocol`
   * Handle `ArchiveSession` in daemon's `client_handler`: calls `cleanup_session`, then `mark_archived` + `save` on the session store
   * Add `strip_client_message` support for `ArchiveSession` (remote session ID prefix stripping)
   * Add archive button (SVG archive icon) in the top-right pill bar, visible when a session is connected
   * Split sidebar session list into active and archived sections
   * Add collapsible "▸ Archived (N)" toggle at the bottom of the sidebar to show/hide archived sessions (rendered at 60% opacity)
   * Add `ArchiveSession` to the TypeScript `ClientMessage` type
 - <csr-id-0297d743512c02edd25a8ede1ee551ea65d878dc/> add directory tab completion in session picker
 - <csr-id-947b37af6289db10485ee7e0a4267333edc4bcef/> new session button uses location picker instead of local-only CWD picker
 - <csr-id-4169bdceccae28a77d664b9942758651defe8a0b/> add UserChoiceComplete daemon-to-client message
 - <csr-id-6dfa04add404a14ef1a48f11003026e160abf5ac/> auto-switch to migrated session in web UI
 - <csr-id-7085405bbfa8d07f6a69bc0e418761a56d108a67/> add RAP view_update protocol + diff view in web UI
 - <csr-id-ba10ffd62644a4c86c31a7fb6d5eaaca8c403b55/> add remote host migration UI and daemon orchestration

### Bug Fixes

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

### Refactor

 - <csr-id-24fa6cbf5564d4df2297451bdc76c9619ec741fe/> drop "Using provider" info message, show provider_id in status displays
 - <csr-id-9757071818663cefb8e6a12438071d95000379a8/> add precheck script, lints

### Refactor (BREAKING)

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

 - 33 commits contributed to the release.
 - 19 commits were understood as [conventional](https://www.conventionalcommits.org).
 - 10 unique issues were worked on: [#107](https://github.com/hydro-project/infinity/issues/107), [#15](https://github.com/hydro-project/infinity/issues/15), [#18](https://github.com/hydro-project/infinity/issues/18), [#29](https://github.com/hydro-project/infinity/issues/29), [#52](https://github.com/hydro-project/infinity/issues/52), [#53](https://github.com/hydro-project/infinity/issues/53), [#60](https://github.com/hydro-project/infinity/issues/60), [#67](https://github.com/hydro-project/infinity/issues/67), [#8](https://github.com/hydro-project/infinity/issues/8), [#90](https://github.com/hydro-project/infinity/issues/90)

### Commit Details

<csr-read-only-do-not-edit/>

<details><summary>view details</summary>

 * **[#107](https://github.com/hydro-project/infinity/issues/107)**
    - Set up cargo-smart-release release workflow (mirroring hydro) ([`ffc27d0`](https://github.com/hydro-project/infinity/commit/ffc27d0bf5d964a655fedab9460bf5017971e6b6))
 * **[#15](https://github.com/hydro-project/infinity/issues/15)**
    - Pass CLI-selected model to new sessions ([`a20554d`](https://github.com/hydro-project/infinity/commit/a20554d63a64440f1ac7d2aa810697e035712832))
 * **[#18](https://github.com/hydro-project/infinity/issues/18)**
    - Make model providers extensible via a dyn-compatible `ModelProvider` trait ([`b4a31e2`](https://github.com/hydro-project/infinity/commit/b4a31e2925c371f38b85b8b2e878fdd226566766))
 * **[#29](https://github.com/hydro-project/infinity/issues/29)**
    - Drop "Using provider" info message, show provider_id in status displays ([`24fa6cb`](https://github.com/hydro-project/infinity/commit/24fa6cbf5564d4df2297451bdc76c9619ec741fe))
 * **[#52](https://github.com/hydro-project/infinity/issues/52)**
    - Use total_tokens for context usage and compaction trigger ([`b7a9805`](https://github.com/hydro-project/infinity/commit/b7a980585d981b1ae22f1bb4fad12b739202b524))
 * **[#53](https://github.com/hydro-project/infinity/issues/53)**
    - Increase LengthDelimitedCodec max frame size to 256 MiB ([`8ad86d8`](https://github.com/hydro-project/infinity/commit/8ad86d850d761c58669dffb906ef389654e4990d))
 * **[#60](https://github.com/hydro-project/infinity/issues/60)**
    - Replay in-progress thinking and response state to clients attaching mid-response ([`7a6e971`](https://github.com/hydro-project/infinity/commit/7a6e9715a7b602d0a04bc527a3c76f4c6a1ccd80))
 * **[#67](https://github.com/hydro-project/infinity/issues/67)**
    - Mid-session model switching per thread, with TUI + desktop UI and e2e tests ([`1c4f71a`](https://github.com/hydro-project/infinity/commit/1c4f71a611507dc7575c20b724faef680cbde2c7))
 * **[#8](https://github.com/hydro-project/infinity/issues/8)**
    - Add automated THIRD-PARTY file generation with license enforcement ([`e2e0719`](https://github.com/hydro-project/infinity/commit/e2e0719faebbffc72ec7bd8a8b3b02223da8ba0e))
 * **[#90](https://github.com/hydro-project/infinity/issues/90)**
    - Add `keeps_session_alive` flag to prevent non-interactive clients from blocking idle shutdown ([`1b20fda`](https://github.com/hydro-project/infinity/commit/1b20fdac512ea534ee24006b95903f3961ff5179))
 * **Uncategorized**
    - Add archive session support with UI button and sidebar section ([`4804c9a`](https://github.com/hydro-project/infinity/commit/4804c9ae531d59dc577d499f8581bb640086a84e))
    - Add directory tab completion in session picker ([`0297d74`](https://github.com/hydro-project/infinity/commit/0297d743512c02edd25a8ede1ee551ea65d878dc))
    - New session button uses location picker instead of local-only CWD picker ([`947b37a`](https://github.com/hydro-project/infinity/commit/947b37af6289db10485ee7e0a4267333edc4bcef))
    - Add UserChoiceComplete daemon-to-client message ([`4169bdc`](https://github.com/hydro-project/infinity/commit/4169bdceccae28a77d664b9942758651defe8a0b))
    - Add workspace lints and fix all lint violations ([`b92b7a1`](https://github.com/hydro-project/infinity/commit/b92b7a17f4b69e2652f5cce813320eca851717e4))
    - Auto-switch to migrated session in web UI ([`6dfa04a`](https://github.com/hydro-project/infinity/commit/6dfa04add404a14ef1a48f11003026e160abf5ac))
    - Add RAP view_update protocol + diff view in web UI ([`7085405`](https://github.com/hydro-project/infinity/commit/7085405bbfa8d07f6a69bc0e418761a56d108a67))
    - Add remote host migration UI and daemon orchestration ([`ba10ffd`](https://github.com/hydro-project/infinity/commit/ba10ffd62644a4c86c31a7fb6d5eaaca8c403b55))
    - Add support for connecting to remote sessions via your local daemon ([`67f4085`](https://github.com/hydro-project/infinity/commit/67f40855a59ac5263ec3f3726c69017c4cd0b464))
    - Add precheck script, lints ([`9757071`](https://github.com/hydro-project/infinity/commit/9757071818663cefb8e6a12438071d95000379a8))
    - Replace bincode with serde_json for CLI↔daemon unix socket wire format ([`62f3822`](https://github.com/hydro-project/infinity/commit/62f382276e6fc8ee76888ac1c629538c977e1745))
    - Introduce display_as typed variants and use Pierre to display in web client ([`1e65518`](https://github.com/hydro-project/infinity/commit/1e65518e4f041f76e6359b08ff88e32fc8753cda))
    - Move HistoryManager to interior mutability; remove callback_with_history hack; restore subscribe_rx in select ([`1e92087`](https://github.com/hydro-project/infinity/commit/1e9208751e55d0029acd419ae12f1bf05cc7104e))
    - Display subthreads in web UI and make it possible to connect to subthreads directly ([`718509d`](https://github.com/hydro-project/infinity/commit/718509d481340bd43497530b3f1212b3f3be27af))
    - Use display_as for tool call pretty-printing in web UI ([`1e4a489`](https://github.com/hydro-project/infinity/commit/1e4a4894ce62c05ab6561539ff3e9a8abf662974))
    - Fix manual compaction and add background auto-compaction triggers ([`9b10a09`](https://github.com/hydro-project/infinity/commit/9b10a0977283f5f628142841cf9515a8b8793793))
    - Add rig-mock crate and test suite for agent core and daemon ([`abda067`](https://github.com/hydro-project/infinity/commit/abda06757eeba0ac7817374bc89155211cd2edcd))
    - Add support for UserChoice prompts in RAP protocol and use for permissions expansion in sandbox ([`b0db6a7`](https://github.com/hydro-project/infinity/commit/b0db6a7a0764ddab7df1f5cf3fcefc7129c6ddcb))
    - Add session status (running/idle/stopped) to CLI session list ([`56dd66d`](https://github.com/hydro-project/infinity/commit/56dd66d112dc068524573916a9183fe11f18b999))
    - Fix auto-exit on idle: send DetachedIdle message instead of closing connection ([`1478ba4`](https://github.com/hydro-project/infinity/commit/1478ba404d1653d5ae750ca5ebb990cd207071d3))
    - Allow auto quit without quit picker when agent is idle ([`3285dc5`](https://github.com/hydro-project/infinity/commit/3285dc5078947b76ad440342316dbd1d665800f4))
    - Add quit picker for graceful disconnect choice; cleanup on ungraceful disconnect ([`d87d7d3`](https://github.com/hydro-project/infinity/commit/d87d7d34130e9d2b5feda891bdc63267fc0689eb))
    - Shift core agent runtime into a daemon with a network protocol for clients ([`141d697`](https://github.com/hydro-project/infinity/commit/141d69792c3aa951fcbfbea847879582f1d06ec3))
</details>

