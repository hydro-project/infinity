

## v0.1.0 (2026-09-03)

### New Features

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
 - <csr-id-d37891afe2ca6f17ac2985823b21b380c9ed591e/> enable credentials process support

### Bug Fixes

 - <csr-id-a6366b8cf44fb703a96dea545869e3aa72b4238c/> remove fable5 model

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

### Reverted

 - <csr-id-658f94cb0c747fb26a75015cb8d530020a0eccd2/> "fix(infinity-provider-bedrock): remove fable5 model"
   This reverts commit a6366b8cf44fb703a96dea545869e3aa72b4238c.

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

 - 10 commits contributed to the release.
 - 9 commits were understood as [conventional](https://www.conventionalcommits.org).
 - 9 unique issues were worked on: [#107](https://github.com/hydro-project/infinity/issues/107), [#110](https://github.com/hydro-project/infinity/issues/110), [#18](https://github.com/hydro-project/infinity/issues/18), [#19](https://github.com/hydro-project/infinity/issues/19), [#38](https://github.com/hydro-project/infinity/issues/38), [#54](https://github.com/hydro-project/infinity/issues/54), [#61](https://github.com/hydro-project/infinity/issues/61), [#66](https://github.com/hydro-project/infinity/issues/66), [#71](https://github.com/hydro-project/infinity/issues/71)

### Commit Details

<csr-read-only-do-not-edit/>

<details><summary>view details</summary>

 * **[#107](https://github.com/hydro-project/infinity/issues/107)**
    - Set up cargo-smart-release release workflow (mirroring hydro) ([`ffc27d0`](https://github.com/hydro-project/infinity/commit/ffc27d0bf5d964a655fedab9460bf5017971e6b6))
 * **[#110](https://github.com/hydro-project/infinity/issues/110)**
    - Rig-free provider stack, native Bedrock, minimal deps; refreshed scale claims ([`49ad32e`](https://github.com/hydro-project/infinity/commit/49ad32e467d92f82cdac76095b6cb0a3daf2f964))
 * **[#18](https://github.com/hydro-project/infinity/issues/18)**
    - Make model providers extensible via a dyn-compatible `ModelProvider` trait ([`b4a31e2`](https://github.com/hydro-project/infinity/commit/b4a31e2925c371f38b85b8b2e878fdd226566766))
 * **[#19](https://github.com/hydro-project/infinity/issues/19)**
    - Run model providers as configurable separate processes over Unix sockets ([`84f7aff`](https://github.com/hydro-project/infinity/commit/84f7aff103f885169f4a6f4ba34aca3af9111a91))
 * **[#38](https://github.com/hydro-project/infinity/issues/38)**
    - "fix(infinity-provider-bedrock): remove fable5 model" ([`658f94c`](https://github.com/hydro-project/infinity/commit/658f94cb0c747fb26a75015cb8d530020a0eccd2))
    - Remove fable5 model ([`a6366b8`](https://github.com/hydro-project/infinity/commit/a6366b8cf44fb703a96dea545869e3aa72b4238c))
 * **[#54](https://github.com/hydro-project/infinity/issues/54)**
    - "fix(infinity-provider-bedrock): remove fable5 model" ([`658f94c`](https://github.com/hydro-project/infinity/commit/658f94cb0c747fb26a75015cb8d530020a0eccd2))
 * **[#61](https://github.com/hydro-project/infinity/issues/61)**
    - Multimodal (image) tool results end-to-end, with image display + review fixes ([`1935c38`](https://github.com/hydro-project/infinity/commit/1935c387d806a1da271e15078b26e06f228737c6))
 * **[#66](https://github.com/hydro-project/infinity/issues/66)**
    - Enable credentials process support ([`d37891a`](https://github.com/hydro-project/infinity/commit/d37891afe2ca6f17ac2985823b21b380c9ed591e))
 * **[#71](https://github.com/hydro-project/infinity/issues/71)**
    - Extract provider protocol into `infinity-provider-protocol` crate ([`27b40fe`](https://github.com/hydro-project/infinity/commit/27b40fed6c5fd1fad5ebfabb1a2a909b7018a0cf))
 * **Uncategorized**
    - Release rap-protocol v0.1.0, rap-client v0.1.0, rap-steering-server v0.1.0, rap-github-event-poller v0.1.0, infinity-protocol v0.1.0, infinity-provider-protocol v0.1.0, infinity-provider-bedrock v0.1.0, infinity-provider-rig v0.1.0, infinity-agent-core v0.1.0, infinity-mcp-bridge v0.1.0, infinity-rap-bridge v0.1.0, infinity-daemon v0.1.0, infinity-agent-cli v0.1.0, sandbox-core v0.1.0, sandbox-local v0.1.0, sandbox-remote v0.1.0 ([`dd8c7f4`](https://github.com/hydro-project/infinity/commit/dd8c7f49028a26052d785b4241f9ade125f0afb3))
</details>

