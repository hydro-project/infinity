

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

### Commit Statistics

<csr-read-only-do-not-edit/>

 - 4 commits contributed to the release.
 - 4 commits were understood as [conventional](https://www.conventionalcommits.org).
 - 4 unique issues were worked on: [#107](https://github.com/hydro-project/infinity/issues/107), [#110](https://github.com/hydro-project/infinity/issues/110), [#61](https://github.com/hydro-project/infinity/issues/61), [#71](https://github.com/hydro-project/infinity/issues/71)

### Commit Details

<csr-read-only-do-not-edit/>

<details><summary>view details</summary>

 * **[#107](https://github.com/hydro-project/infinity/issues/107)**
    - Set up cargo-smart-release release workflow (mirroring hydro) ([`ffc27d0`](https://github.com/hydro-project/infinity/commit/ffc27d0bf5d964a655fedab9460bf5017971e6b6))
 * **[#110](https://github.com/hydro-project/infinity/issues/110)**
    - Rig-free provider stack, native Bedrock, minimal deps; refreshed scale claims ([`49ad32e`](https://github.com/hydro-project/infinity/commit/49ad32e467d92f82cdac76095b6cb0a3daf2f964))
 * **[#61](https://github.com/hydro-project/infinity/issues/61)**
    - Multimodal (image) tool results end-to-end, with image display + review fixes ([`1935c38`](https://github.com/hydro-project/infinity/commit/1935c387d806a1da271e15078b26e06f228737c6))
 * **[#71](https://github.com/hydro-project/infinity/issues/71)**
    - Extract provider protocol into `infinity-provider-protocol` crate ([`27b40fe`](https://github.com/hydro-project/infinity/commit/27b40fed6c5fd1fad5ebfabb1a2a909b7018a0cf))
</details>

