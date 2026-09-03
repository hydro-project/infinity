

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

 - <csr-id-b71baa13e96ed6a593683aa617a6a1d2d11a7f12/> redesign landing page around the runtime, add builder `start()` API, and benchmark agent memory scaling
   ## Runtime API (`infinity-agent-core`)
   
   * `AgentSystemBuilder::start()` builds the local system and starts it with the thread-builder API in one call; `AgentSystemBuilder::start_with_observer(...)` does the same for the custom-observer path. `build_local()` remains the two-phase form for embeddings that hold the built `LocalAgentSystem` before running it (as the daemon does).
   * Updated the module example, simple internal call sites, and all agent-systems docs pages to the shorter form.
   
   ## Memory-scaling benchmark
   
   * New `crates/infinity-agent-core/examples/agent_scale.rs`: launches waves of agents against a self-driving scripted model; every turn is a full runtime round trip (streamed ~1.2 KB completion with a tool call, async tool result through the input queue, follow-up completion). Samples `VmRSS` after `malloc_trim` once each wave is idle and prints a CSV. Adds `libc` as a dev-dependency.
   * Measured (AMD EPYC 9R14, single thread, in-memory stores): **50,000 agents × 20 turns = 2,000,000 completions in 255 s; 7.9 GB RSS, ≈154 KB per idle agent** — fifty thousand resident agents fit in a Raspberry Pi 5's memory.
   
   ## Landing page redesign
   
   Rebuilt around the priorities: clean Rust API in the hero, then Scale → Asynchrony → Serverless chapters, with RAP and Infinity Code as separate product sections.
   
   * **Hero**: tagline leads with the Rust framework and the measured Raspberry Pi claim; the subline pitches the architecture ("Infinity does for agents what async did for threads: instead of blocking on slow tools, agents run them concurrently, yield while they wait, and cost nothing until the next event arrives"); stock Docusaurus code block shows the quickstart snippet using the new `start()` API.
   * **Scale**: new `MemoryChart` component plots the measured 21-point agents-vs-memory curve (inverted axes: resident agents against RSS) with a Raspberry Pi 5 reference line, per-agent slope annotation, and a methodology caption linking to the benchmark source.
   * **Asynchrony**: new `AgentTrace` component, a transcript-style session log: the agent backgrounds `cargo test`, drafts release notes while the tests run, and goes idle; the run's failure event wakes it to fix and rerun, and exit 0 closes it out. Tagged rows (user / agent / event) without timestamps; explicit idle rows; fixed-height opacity-only reveal with extra-long pauses after idle rows. Copy names the primitives (subscriptions, `spawn_thread`, `sleep`) and explains agent systems as actor-like pools that the runtime intelligently schedules like an async executor, so thousands of agents make progress on a few threads.
   * **Serverless**: keeps the time-slicing animation; copy reads "Because agent turns never block, Infinity is perfect for serverless environments" (one Lambda step per SQS delivery) and "Infinity agents can run *forever* with *near-zero cost*", closing on the CDK constructs and laptop-to-cloud portability.
   * **RAP / Infinity Code**: standalone sections with title + subtitle headers, aligned to the same 1200px content grid as the chapters; all separators span the identical content width with the same color.
   * **Footer**: reorganized into three highlight columns — Infinity Runtime (Quickstart, Architecture, Deploy on AWS Lambda), Reactive Agent Protocol (What is RAP?, Build a RAP Tool, Specification), and Infinity Code (Get Started, Background Agents, Slack Bot).
   * **Style**: all copy rewritten in active Hydro-landing register (~two sentences per paragraph, minimal em-dashes); removed the old layer cards, numbered "·" kickers, dotted connectors, alternating flip layout, the Rust-API detail chapter, and the unused `SliceDiagram`/code panels.
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
 - <csr-id-66ddd8ff3797df0284b0658382249133361b55d9/> add Claude Fable 5 to Bedrock models list
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
 - <csr-id-4169bdceccae28a77d664b9942758651defe8a0b/> add UserChoiceComplete daemon-to-client message
 - <csr-id-ba10ffd62644a4c86c31a7fb6d5eaaca8c403b55/> add remote host migration UI and daemon orchestration

### Bug Fixes

 - <csr-id-448dbedc29f585eada388df6b775631ccdd11554/> return tool failures to the agent
   * Add a shared `send_tool_error` helper that enqueues an error `ToolResult` with the original tool and call IDs
   * Convert thread-tool argument and relationship validation failures, including missing `close_thread.thread_id`, from propagated errors or duplicated message construction into queued tool results
   * Make the batch processor enqueue a generic `Error: Tool call failed` result when asynchronous tool execution returns an error, while retaining the detailed display event and logging delivery failures
   * Add regressions covering missing `close_thread` arguments and generic failed-tool fallback delivery
 - <csr-id-6df9db14dd4af2fceb3412514e82cdfb5a052fe5/> fix broken tests after merge
 - <csr-id-cd2c5de45d1f3c981d165277b8c7242415ced3a3/> suppress stream content after the turn's tool call
   Confirmed bug: with Bedrock (Claude with adaptive/interleaved thinking), the model can
   stream *concurrent* tool calls in a single assistant message, with reasoning
   interleaved between them. `run_completion` only executes the first tool call
   (the rest are ignored), but it kept forwarding everything streamed after it:
   
   * the ignored calls' name/argument deltas and the interleaved reasoning were
   yielded as `ThinkingChunk`s, so clients (web UI, TUI, ACP) saw "thinking"
   streaming *after* the `ToolCall` but *before* its `ToolResult` display event
   (which only arrives in a later round, via the MCP/RAP callback)
   * worse, a trailing reasoning/text block was committed to history *after* the
   tool call, which broke the `history.last()` match in `handle_content` when
   the tool result arrived — the result was silently dropped and the call
   stranded as unanswered
   
   Fix in `run_completion`: once the turn's first tool call has been emitted,
   drop all subsequent stream content for that turn (logging it at info level)
   instead of buffering/yielding it; only `Final` still flows through to flush
   the turn and finish the round. This keeps the executed tool call as the last
   committed history entry and restores the expected client event order:
   `ToolCall → ResponseDone → ToolResult → StartOutput → thinking…`.
   
   Also removed the now-dead "Ignoring batched tool call" branch and normalized
   the surrounding indentation (rustfmt does not format `async_stream` bodies).
   
   Added regression test `concurrent_tool_calls_suppress_trailing_stream_content`
   that replays the Bedrock chunk sequence (reasoning → tc-1 → tc-2 deltas →
   interleaved reasoning → tc-2) and asserts no display events are emitted after
   the executed call and that history still ends with tc-1.
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
 - <csr-id-b959506eea3eb763bb8a6699dd6a5f37f9fe7a98/> prevent compaction from truncating pending tool calls
   When auto-compaction triggers while an async tool call is pending, the
   compaction summary's `up_to_order` previously included the trailing
   unanswered tool call. After `apply_compaction`, the tool call was gone
   from history, causing the subsequent tool result to be orphaned (no
   matching tool call in the conversation sent to the LLM).
 - <csr-id-fe820d8894b7768579245399b9b157e280b87bea/> embed subscription invocation inside SubscriptionEvent to prevent duplicate replay entries
 - <csr-id-5cf4d552ad412a7946c39c6d8a84913fd5a1685e/> address review comments for error handling
   - thread.rs: Return an error ToolResult instead of panicking when
   spawn_thread fails in the conversation store, since this is a
   runtime/IO failure, not a logic bug
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

### Performance

 - <csr-id-8345b7dfb667dcca90b792a63ad49f045302308f/> coalesce consecutive streamed text chunks
   Address PR #50 review: panic on history/pending desync.
   
   When a model provider streams an assistant response as multiple text
   chunks, each chunk was persisted as its own message row, inflating disk
   usage. `handle_completion` now merges an incoming text chunk into the
   last pending item when it is already an assistant text message.
   
   * In `try_merge_pending_text`, the in-memory history tail must always
   correspond to the last pending item, so a mismatch is now treated as a
   logical bug via `let ... else { panic!("bug: pending_items and history
   out of sync") }` instead of silently no-op'ing.

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

 - 113 commits contributed to the release.
 - 33 commits were understood as [conventional](https://www.conventionalcommits.org).
 - 20 unique issues were worked on: [#104](https://github.com/hydro-project/infinity/issues/104), [#105](https://github.com/hydro-project/infinity/issues/105), [#107](https://github.com/hydro-project/infinity/issues/107), [#110](https://github.com/hydro-project/infinity/issues/110), [#113](https://github.com/hydro-project/infinity/issues/113), [#13](https://github.com/hydro-project/infinity/issues/13), [#18](https://github.com/hydro-project/infinity/issues/18), [#19](https://github.com/hydro-project/infinity/issues/19), [#39](https://github.com/hydro-project/infinity/issues/39), [#50](https://github.com/hydro-project/infinity/issues/50), [#52](https://github.com/hydro-project/infinity/issues/52), [#61](https://github.com/hydro-project/infinity/issues/61), [#63](https://github.com/hydro-project/infinity/issues/63), [#71](https://github.com/hydro-project/infinity/issues/71), [#8](https://github.com/hydro-project/infinity/issues/8), [#82](https://github.com/hydro-project/infinity/issues/82), [#87](https://github.com/hydro-project/infinity/issues/87), [#88](https://github.com/hydro-project/infinity/issues/88), [#92](https://github.com/hydro-project/infinity/issues/92), [#96](https://github.com/hydro-project/infinity/issues/96)

### Commit Details

<csr-read-only-do-not-edit/>

<details><summary>view details</summary>

 * **[#104](https://github.com/hydro-project/infinity/issues/104)**
    - Redesign landing page around the runtime, add builder `start()` API, and benchmark agent memory scaling ([`b71baa1`](https://github.com/hydro-project/infinity/commit/b71baa13e96ed6a593683aa617a6a1d2d11a7f12))
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
 * **[#18](https://github.com/hydro-project/infinity/issues/18)**
    - Make model providers extensible via a dyn-compatible `ModelProvider` trait ([`b4a31e2`](https://github.com/hydro-project/infinity/commit/b4a31e2925c371f38b85b8b2e878fdd226566766))
 * **[#19](https://github.com/hydro-project/infinity/issues/19)**
    - Run model providers as configurable separate processes over Unix sockets ([`84f7aff`](https://github.com/hydro-project/infinity/commit/84f7aff103f885169f4a6f4ba34aca3af9111a91))
 * **[#39](https://github.com/hydro-project/infinity/issues/39)**
    - Compaction inside child thread no longer panics on indexing ([`be6bbd5`](https://github.com/hydro-project/infinity/commit/be6bbd5ca0f907b3a75df4b5615a7181f756e18d))
 * **[#50](https://github.com/hydro-project/infinity/issues/50)**
    - Coalesce consecutive streamed text chunks ([`8345b7d`](https://github.com/hydro-project/infinity/commit/8345b7dfb667dcca90b792a63ad49f045302308f))
 * **[#52](https://github.com/hydro-project/infinity/issues/52)**
    - Use total_tokens for context usage and compaction trigger ([`b7a9805`](https://github.com/hydro-project/infinity/commit/b7a980585d981b1ae22f1bb4fad12b739202b524))
 * **[#61](https://github.com/hydro-project/infinity/issues/61)**
    - Multimodal (image) tool results end-to-end, with image display + review fixes ([`1935c38`](https://github.com/hydro-project/infinity/commit/1935c387d806a1da271e15078b26e06f228737c6))
 * **[#63](https://github.com/hydro-project/infinity/issues/63)**
    - Don't commit turn data to history until the turn is completed. ([`a84b99e`](https://github.com/hydro-project/infinity/commit/a84b99e871770df5fa923e1b8881c3e07486baf0))
 * **[#71](https://github.com/hydro-project/infinity/issues/71)**
    - Extract provider protocol into `infinity-provider-protocol` crate ([`27b40fe`](https://github.com/hydro-project/infinity/commit/27b40fed6c5fd1fad5ebfabb1a2a909b7018a0cf))
 * **[#8](https://github.com/hydro-project/infinity/issues/8)**
    - Add automated THIRD-PARTY file generation with license enforcement ([`e2e0719`](https://github.com/hydro-project/infinity/commit/e2e0719faebbffc72ec7bd8a8b3b02223da8ba0e))
 * **[#82](https://github.com/hydro-project/infinity/issues/82)**
    - Suppress stream content after the turn's tool call ([`cd2c5de`](https://github.com/hydro-project/infinity/commit/cd2c5de45d1f3c981d165277b8c7242415ced3a3))
 * **[#87](https://github.com/hydro-project/infinity/issues/87)**
    - Fix broken tests after merge ([`6df9db1`](https://github.com/hydro-project/infinity/commit/6df9db14dd4af2fceb3412514e82cdfb5a052fe5))
 * **[#88](https://github.com/hydro-project/infinity/issues/88)**
    - Return tool failures to the agent ([`448dbed`](https://github.com/hydro-project/infinity/commit/448dbedc29f585eada388df6b775631ccdd11554))
 * **[#92](https://github.com/hydro-project/infinity/issues/92)**
    - Add high-level agent system API ([`8bef2c5`](https://github.com/hydro-project/infinity/commit/8bef2c534f90b7fe038cb6dda1fb2015fa9e737d))
 * **[#96](https://github.com/hydro-project/infinity/issues/96)**
    - Extract shared agent system engine ([`9c921fd`](https://github.com/hydro-project/infinity/commit/9c921fde280b50c89c3e5b9caadccf83a46078a4))
 * **Uncategorized**
    - Release infinity-provider-protocol v0.1.0, infinity-provider-bedrock v0.1.0, infinity-provider-rig v0.1.0, infinity-agent-core v0.1.0, infinity-mcp-bridge v0.1.0, infinity-rap-bridge v0.1.0, infinity-daemon v0.1.0, infinity-agent-cli v0.1.0, sandbox-core v0.1.0, sandbox-local v0.1.0, sandbox-remote v0.1.0 ([`7e1cd1d`](https://github.com/hydro-project/infinity/commit/7e1cd1df69d8fce402bef4085e9d17f871994503))
    - Release rap-protocol v0.1.0, rap-client v0.1.0, rap-steering-server v0.1.0, rap-github-event-poller v0.1.0, infinity-protocol v0.1.0, infinity-provider-protocol v0.1.0, infinity-provider-bedrock v0.1.0, infinity-provider-rig v0.1.0, infinity-agent-core v0.1.0, infinity-mcp-bridge v0.1.0, infinity-rap-bridge v0.1.0, infinity-daemon v0.1.0, infinity-agent-cli v0.1.0, sandbox-core v0.1.0, sandbox-local v0.1.0, sandbox-remote v0.1.0 ([`dd8c7f4`](https://github.com/hydro-project/infinity/commit/dd8c7f49028a26052d785b4241f9ade125f0afb3))
    - Prevent close_thread on root thread and deduplicate get_thread_parent_info calls ([`bbfb25d`](https://github.com/hydro-project/infinity/commit/bbfb25dc3514f3c124b5ac50f102291e0c131e9c))
    - Prevent compaction from truncating pending tool calls ([`b959506`](https://github.com/hydro-project/infinity/commit/b959506eea3eb763bb8a6699dd6a5f37f9fe7a98))
    - Add pretty-print display scripts for sleep tools ([`6abd457`](https://github.com/hydro-project/infinity/commit/6abd457adc8b7c4ff5dcc62d575250d7f1736f2b))
    - Show choice picker alongside input and cancel choices on tool interruption ([`59d3314`](https://github.com/hydro-project/infinity/commit/59d331491087ef43aa3cea9215a94c2089675b30))
    - Embed subscription invocation inside SubscriptionEvent to prevent duplicate replay entries ([`fe820d8`](https://github.com/hydro-project/infinity/commit/fe820d8894b7768579245399b9b157e280b87bea))
    - Introduce InfinityMessage to replace bare rig Message in conversation storage ([`53e7ef6`](https://github.com/hydro-project/infinity/commit/53e7ef6c60baca2442de2be8d31d82094f50f410))
    - Add UserChoiceComplete daemon-to-client message ([`4169bdc`](https://github.com/hydro-project/infinity/commit/4169bdceccae28a77d664b9942758651defe8a0b))
    - Add workspace lints and fix all lint violations ([`b92b7a1`](https://github.com/hydro-project/infinity/commit/b92b7a17f4b69e2652f5cce813320eca851717e4))
    - Add remote host migration UI and daemon orchestration ([`ba10ffd`](https://github.com/hydro-project/infinity/commit/ba10ffd62644a4c86c31a7fb6d5eaaca8c403b55))
    - Address review comments for error handling ([`5cf4d55`](https://github.com/hydro-project/infinity/commit/5cf4d552ad412a7946c39c6d8a84913fd5a1685e))
    - Replace all .unwrap() with .expect() and fix clippy warnings ([`7634b82`](https://github.com/hydro-project/infinity/commit/7634b823ad70378e666379a9a8e8a7935a06026f))
    - Add precheck script, lints ([`9757071`](https://github.com/hydro-project/infinity/commit/9757071818663cefb8e6a12438071d95000379a8))
    - Introduce display_as typed variants and use Pierre to display in web client ([`1e65518`](https://github.com/hydro-project/infinity/commit/1e65518e4f041f76e6359b08ff88e32fc8753cda))
    - Fix broken spawn_local tests in infinity-agent-core and infinity-daemon ([`c85fc1a`](https://github.com/hydro-project/infinity/commit/c85fc1a13893c94a77e88805e3c986aa1741d75a))
    - Move HistoryManager to interior mutability; remove callback_with_history hack; restore subscribe_rx in select ([`1e92087`](https://github.com/hydro-project/infinity/commit/1e9208751e55d0029acd419ae12f1bf05cc7104e))
    - Display subthreads in web UI and make it possible to connect to subthreads directly ([`718509d`](https://github.com/hydro-project/infinity/commit/718509d481340bd43497530b3f1212b3f3be27af))
    - Fix streaming cancellation and ensure is_thinking is reset on remove_trailing_reasoning ([`6b65a9c`](https://github.com/hydro-project/infinity/commit/6b65a9c664e79c44d2d6372da0466fbd4546afce))
    - Use display_as for tool call pretty-printing in web UI ([`1e4a489`](https://github.com/hydro-project/infinity/commit/1e4a4894ce62c05ab6561539ff3e9a8abf662974))
    - Extract rap-client crate and unify RAP protocol types ([`51406e4`](https://github.com/hydro-project/infinity/commit/51406e4dfab243a4400027507f446862b26ce8d3))
    - Add write:/path permissions, thread_ancestors protocol field, and ancestor-aware grant system ([`3464fad`](https://github.com/hydro-project/infinity/commit/3464fade510fd5ab7aa2dc2ffa27f61711c6be31))
    - Unify all duplicate RAP protocol types into rap-protocol crate ([`2def5ee`](https://github.com/hydro-project/infinity/commit/2def5eec01a5c197432a7959942cca8b0eb9d6a0))
    - Unify RapInvocation into a single type in rap-protocol ([`e14509e`](https://github.com/hydro-project/infinity/commit/e14509ecf6e6bf622d6ca0a1252148b647c1ef7f))
    - Improve handling of API errors that require retrying ([`3ba5dba`](https://github.com/hydro-project/infinity/commit/3ba5dba4872487b6a523bf3d8deae906d2df3e12))
    - Include thread ID of subscription event child thread reports ([`7288d8d`](https://github.com/hydro-project/infinity/commit/7288d8ddd5a23a7e46315d5b4543316f0362108b))
    - Fix manual compaction and add background auto-compaction triggers ([`9b10a09`](https://github.com/hydro-project/infinity/commit/9b10a0977283f5f628142841cf9515a8b8793793))
    - Hanging caused by `sh -c` intercepting SIGINT, improved config error handling ([`b40442e`](https://github.com/hydro-project/infinity/commit/b40442e37ac91b884f51fcabb018a3735bdf612f))
    - Add rig-mock crate and test suite for agent core and daemon ([`abda067`](https://github.com/hydro-project/infinity/commit/abda06757eeba0ac7817374bc89155211cd2edcd))
    - Further increase timeout for output streaming ([`6d28500`](https://github.com/hydro-project/infinity/commit/6d28500597508463ed6f08e436a6c6862546431d))
    - Add support for UserChoice prompts in RAP protocol and use for permissions expansion in sandbox ([`b0db6a7`](https://github.com/hydro-project/infinity/commit/b0db6a7a0764ddab7df1f5cf3fcefc7129c6ddcb))
    - Refactor thread processing loop to improve clarity ([`1d72b0b`](https://github.com/hydro-project/infinity/commit/1d72b0bfd4b9408fbb95dc4c9428a89a24eef7f9))
    - Add rap-github-event-poller crate for local GitHub event polling ([`783a9ec`](https://github.com/hydro-project/infinity/commit/783a9ec48c0f8f97522c34f62460a48911ac9875))
    - Allow auto quit without quit picker when agent is idle ([`3285dc5`](https://github.com/hydro-project/infinity/commit/3285dc5078947b76ad440342316dbd1d665800f4))
    - Shift core agent runtime into a daemon with a network protocol for clients ([`141d697`](https://github.com/hydro-project/infinity/commit/141d69792c3aa951fcbfbea847879582f1d06ec3))
    - Add steering file instructions to the default agent prompt ([`40b1f78`](https://github.com/hydro-project/infinity/commit/40b1f78d18466b99040caecc772adfaa7c6ed705))
    - Add support for global RAP config and add `rap install` / `rap update` tools ([`6372cd5`](https://github.com/hydro-project/infinity/commit/6372cd5622d2e8b23e04a6d5b001aa6b0e0fab6a))
    - Add displayScript field to RAP tool definitions for pretty-printing tool calls ([`f7e01f2`](https://github.com/hydro-project/infinity/commit/f7e01f2ccfc567fcc44aef1b85eb9e68e3e88131))
    - Simplify thread report display event name to "Report from child thread {id}" ([`4e50948`](https://github.com/hydro-project/infinity/commit/4e5094811d5584206f11751e1da0c8fe7bf2d7b3))
    - Redesign spinner states ([`7a8bd6a`](https://github.com/hydro-project/infinity/commit/7a8bd6ace0e87ccfc50280e5f7debcffd4fca82d))
    - Improve spinner display when there is a very large context ([`2c2cdd6`](https://github.com/hydro-project/infinity/commit/2c2cdd66dcd94e37aef65a411274d4f2721edbb7))
    - Implement background compaction using threads ([`6e7e28b`](https://github.com/hydro-project/infinity/commit/6e7e28baff2ea33b6b12f52db370170c51128281))
    - Add send_message_to_child tool for parent-to-child thread messaging ([`897b024`](https://github.com/hydro-project/infinity/commit/897b02403dbea664c7e807ea02bf0fc8e5f480f1))
    - Add squash_sandbox tool and base_thread_id to clone_repo ([`5ffac5f`](https://github.com/hydro-project/infinity/commit/5ffac5f4d28c8efe8dfb861d883921292cf31423))
    - Add child_of validation to spawn_thread to prevent confused child threads from spawning subthreads ([`634fba0`](https://github.com/hydro-project/infinity/commit/634fba01523c7ba3ea0805db7ff9fd0411da7457))
    - Make sure LLM provide streams are always consumed to completion ([`42dcae7`](https://github.com/hydro-project/infinity/commit/42dcae7c1201cba811766cc49a520ae9a698bf2d))
    - Add MCP server support to the CLI via in-process RAP proxies ([`68b4266`](https://github.com/hydro-project/infinity/commit/68b426683d5c1c090c6f43f437a1d83396a95414))
    - Increase stream timeout for fault tolerance ([`14130f9`](https://github.com/hydro-project/infinity/commit/14130f9d31060f3689e832e75cbab963fa62d32c))
    - Improve error logging for Bedrock provider failures ([`1221736`](https://github.com/hydro-project/infinity/commit/12217365ac1a9c187fcdc12bf889dff665a3e19d))
    - Correctly re-load token usage when loading session ([`d6506f8`](https://github.com/hydro-project/infinity/commit/d6506f854d183d5348072840666c178eaddbf8a5))
    - Handle retry due to rate limiting ([`dfc0d89`](https://github.com/hydro-project/infinity/commit/dfc0d89343280f3f45cce15c5b2e800cb393c76d))
    - Fix short circuiting behavior for parallel tool calls ([`20adff9`](https://github.com/hydro-project/infinity/commit/20adff90b25a89d8c8631364718404a9edf4aacf))
    - Improve stream parsing robustness ([`2a9d85c`](https://github.com/hydro-project/infinity/commit/2a9d85c31fb16a0dc2cd8df6ee48dc7e945973ee))
    - Improve retry logic for stream errors ([`6a6e1c1`](https://github.com/hydro-project/infinity/commit/6a6e1c12f52f0616e3d03dda702e8ab8830c82b2))
    - Display in-progress tool call construction as thinking tokens ([`2ad1422`](https://github.com/hydro-project/infinity/commit/2ad1422bcb602b37f5ee3c99c482e95b78a105dd))
    - Improve retry handling and reporting ([`2c0f58c`](https://github.com/hydro-project/infinity/commit/2c0f58cea7f0d768d190642138c3ca99993ec62a))
    - Add model provider abstraction and model_id_override support ([`0effd62`](https://github.com/hydro-project/infinity/commit/0effd6250f6d6cf6d4384d3dafd82bc14af40a86))
    - Correctly handle HistoryManager::fork_new for threaded event handling ([`257c0b8`](https://github.com/hydro-project/infinity/commit/257c0b8842706c00a5ca484c9a9ca10e0fe93a72))
    - Extract shared batch processing logic into infinity-agent-core ([`ec43e34`](https://github.com/hydro-project/infinity/commit/ec43e34fffc0e6d5edadd3759695809ba80199bf))
    - Fix history management bug with removing trailing empty content ([`ce9d55b`](https://github.com/hydro-project/infinity/commit/ce9d55b9bd1c7dc8ae186fe5f1f1819262086a1e))
    - Add support for interrupting with user input during thinking / output ([`aa4d560`](https://github.com/hydro-project/infinity/commit/aa4d560accfd4177984282bf31117e0712fb8530))
    - Add model switcher to CLI with Ctrl+M shortcut ([`82fbf32`](https://github.com/hydro-project/infinity/commit/82fbf3267f8b4f77a730f8f0797d8b68e3514251))
    - Add tool call and subscription cancellation protocol for resource cleanup ([`56cfa15`](https://github.com/hydro-project/infinity/commit/56cfa15af99cfc07db6b0bfbe09327fccd72eadb))
    - Enforce absolute paths in local sandbox and add CWD to system prompt ([`c7fa225`](https://github.com/hydro-project/infinity/commit/c7fa225eb52f91077f53ddc7c63ad9546b70e45b))
    - Format and update snapshots ([`772a00c`](https://github.com/hydro-project/infinity/commit/772a00c383299383409c6ff8c834d344bdec4d11))
    - Implement output streaming for execute_command using RAP subscriptions with debouncing. ([`b2fb764`](https://github.com/hydro-project/infinity/commit/b2fb7643665e2052419103a4c7d4466758b0e026))
    - Add thinking token visualization to the CLI terminal. ([`f416795`](https://github.com/hydro-project/infinity/commit/f41679517609de5f139bac11495c0e5b8944a1f6))
    - Improve system prompt to explain threaded code editing flow ([`aeb483a`](https://github.com/hydro-project/infinity/commit/aeb483a8a8bf4ce8cdd704cf68fe5905f474be11))
    - Add RAP protocol for notifying tool servers of thread closure ([`2d60e9d`](https://github.com/hydro-project/infinity/commit/2d60e9d12b84d01984b17e56c859caac8757859d))
    - Remove busted KV caching API use ([`3367cde`](https://github.com/hydro-project/infinity/commit/3367cded25b25970acfd3e17788f3aea4a81f70a))
    - Coalesce multiple incoming events into one LLM invocation ([`ad2bb1a`](https://github.com/hydro-project/infinity/commit/ad2bb1acfccf8b180415a4fa101f14d96ce5ee6a))
    - Add support for synchronous tool calls that are uninterruptible ([`544ee9c`](https://github.com/hydro-project/infinity/commit/544ee9c4d5c8507bbacb5dcc5f8006972301588e))
    - Improve thread handling ([`2f53c50`](https://github.com/hydro-project/infinity/commit/2f53c502e97f174734ef2ffe10b300e0f1f7b364))
    - Redesign synthetic tool call structure to reduce confusion ([`487589f`](https://github.com/hydro-project/infinity/commit/487589f545338197e646e7eceea810f70cb47501))
    - Support launching RAP servers as a subprocess ([`5439050`](https://github.com/hydro-project/infinity/commit/5439050aef622ee1ac16227ded7646e3d08e55fb))
    - Store RAP config in a local directory ([`71c8101`](https://github.com/hydro-project/infinity/commit/71c81015e8d2b096aaa533d16e94b6944480b16d))
    - Update model invocations for new API ([`cee4d10`](https://github.com/hydro-project/infinity/commit/cee4d101f4a9322882e3b76e39fe3da8ecf0b4d2))
    - Context window tracking ([`1fc5ed8`](https://github.com/hydro-project/infinity/commit/1fc5ed8bf8c0fc6e330de316ed3f235a7dfaac3a))
    - Initial multi-line input and thinking animation ([`35b1ab3`](https://github.com/hydro-project/infinity/commit/35b1ab3034378faf13047b8922d495fc4ed635a0))
    - Add display_as to RAP tool results ([`18b60a5`](https://github.com/hydro-project/infinity/commit/18b60a5aa8a463d70eec75aca3e9a6e77722a972))
    - Code editing tools ([`36c7466`](https://github.com/hydro-project/infinity/commit/36c7466a0707836590fb385d313a2f929c3465e1))
    - Use Ratatui to render inline viewport ([`c426a64`](https://github.com/hydro-project/infinity/commit/c426a641502bb5bd91265148bf2e6e418008a2a0))
    - Run clippy ([`ea864bf`](https://github.com/hydro-project/infinity/commit/ea864bf5a21cb030738936df2749af7ad0c255d8))
    - Clean up dependencies ([`fcef65d`](https://github.com/hydro-project/infinity/commit/fcef65df6274e43596bf84f9b2eaf4d8955e9b93))
    - Cache jj workspaces for local sandboxes ([`ba0ba4e`](https://github.com/hydro-project/infinity/commit/ba0ba4e372432f9d1044f0e57e06b9ada870de30))
    - Implement RAP server support in CLI ([`885c17b`](https://github.com/hydro-project/infinity/commit/885c17b1847339f9747eb910fc0f3752a9b2eeeb))
    - Redesign CLI interface to display calls / subscription events ([`6c3155a`](https://github.com/hydro-project/infinity/commit/6c3155aba36a809a5a805fcefb1048f63aac0040))
    - Reduce scope of message sending trait ([`8514532`](https://github.com/hydro-project/infinity/commit/8514532f2a9bf6d96c15a29c7d25fcfc32a4b5c6))
    - Refactor out side effects of prepare_input and add snapshot tests ([`86736c1`](https://github.com/hydro-project/infinity/commit/86736c1eae594d48dd9a1fed2b5fd4bd9284f3ee))
    - Restore close_thread tool and tool call logging ([`7793dab`](https://github.com/hydro-project/infinity/commit/7793dab6706ea73b6d7a842e07701663441f5342))
    - Remove old implementation ([`cdcfa16`](https://github.com/hydro-project/infinity/commit/cdcfa167724f2ab18d3d91b822b9e05de9c2f233))
    - Stream output of LLM processor instead of accumulating text ([`8eeaefd`](https://github.com/hydro-project/infinity/commit/8eeaefdf4ee0dca2b62d487171f2329fd2d930bf))
    - Initial refactor to split out core runtime from Lambda ([`7242d5c`](https://github.com/hydro-project/infinity/commit/7242d5c2f4e145100ff28d544fe4206a432a625d))
</details>

