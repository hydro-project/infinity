

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
 - <csr-id-7085405bbfa8d07f6a69bc0e418761a56d108a67/> add RAP view_update protocol + diff view in web UI
 - <csr-id-ba10ffd62644a4c86c31a7fb6d5eaaca8c403b55/> add remote host migration UI and daemon orchestration

### Bug Fixes

 - <csr-id-44fcca250a44029e36b49df6013a049d33bc985f/> log panics from fire-and-forget spawned tasks instead of silently swallowing them

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

 - <csr-id-7634b823ad70378e666379a9a8e8a7935a06026f/> replace all .unwrap() with .expect() and fix clippy warnings
 - <csr-id-9757071818663cefb8e6a12438071d95000379a8/> add precheck script, lints

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

### Commit Statistics

<csr-read-only-do-not-edit/>

 - 17 commits contributed to the release.
 - 10 commits were understood as [conventional](https://www.conventionalcommits.org).
 - 4 unique issues were worked on: [#107](https://github.com/hydro-project/infinity/issues/107), [#113](https://github.com/hydro-project/infinity/issues/113), [#61](https://github.com/hydro-project/infinity/issues/61), [#8](https://github.com/hydro-project/infinity/issues/8)

### Commit Details

<csr-read-only-do-not-edit/>

<details><summary>view details</summary>

 * **[#107](https://github.com/hydro-project/infinity/issues/107)**
    - Set up cargo-smart-release release workflow (mirroring hydro) ([`ffc27d0`](https://github.com/hydro-project/infinity/commit/ffc27d0bf5d964a655fedab9460bf5017971e6b6))
 * **[#113](https://github.com/hydro-project/infinity/issues/113)**
    - Introduce typed ThreadId for RAP group ids ([`4b18b37`](https://github.com/hydro-project/infinity/commit/4b18b37de219cb7fe27ce7c027b87f4fb35fbbf5))
 * **[#61](https://github.com/hydro-project/infinity/issues/61)**
    - Multimodal (image) tool results end-to-end, with image display + review fixes ([`1935c38`](https://github.com/hydro-project/infinity/commit/1935c387d806a1da271e15078b26e06f228737c6))
 * **[#8](https://github.com/hydro-project/infinity/issues/8)**
    - Add automated THIRD-PARTY file generation with license enforcement ([`e2e0719`](https://github.com/hydro-project/infinity/commit/e2e0719faebbffc72ec7bd8a8b3b02223da8ba0e))
 * **Uncategorized**
    - Add workspace lints and fix all lint violations ([`b92b7a1`](https://github.com/hydro-project/infinity/commit/b92b7a17f4b69e2652f5cce813320eca851717e4))
    - Add RAP view_update protocol + diff view in web UI ([`7085405`](https://github.com/hydro-project/infinity/commit/7085405bbfa8d07f6a69bc0e418761a56d108a67))
    - Add remote host migration UI and daemon orchestration ([`ba10ffd`](https://github.com/hydro-project/infinity/commit/ba10ffd62644a4c86c31a7fb6d5eaaca8c403b55))
    - Log panics from fire-and-forget spawned tasks instead of silently swallowing them ([`44fcca2`](https://github.com/hydro-project/infinity/commit/44fcca250a44029e36b49df6013a049d33bc985f))
    - Replace all .unwrap() with .expect() and fix clippy warnings ([`7634b82`](https://github.com/hydro-project/infinity/commit/7634b823ad70378e666379a9a8e8a7935a06026f))
    - Add precheck script, lints ([`9757071`](https://github.com/hydro-project/infinity/commit/9757071818663cefb8e6a12438071d95000379a8))
    - Introduce display_as typed variants and use Pierre to display in web client ([`1e65518`](https://github.com/hydro-project/infinity/commit/1e65518e4f041f76e6359b08ff88e32fc8753cda))
    - Add write:/path permissions, thread_ancestors protocol field, and ancestor-aware grant system ([`3464fad`](https://github.com/hydro-project/infinity/commit/3464fade510fd5ab7aa2dc2ffa27f61711c6be31))
    - Unify all duplicate RAP protocol types into rap-protocol crate ([`2def5ee`](https://github.com/hydro-project/infinity/commit/2def5eec01a5c197432a7959942cca8b0eb9d6a0))
    - Unify RapInvocation into a single type in rap-protocol ([`e14509e`](https://github.com/hydro-project/infinity/commit/e14509ecf6e6bf622d6ca0a1252148b647c1ef7f))
    - Add support for UserChoice prompts in RAP protocol and use for permissions expansion in sandbox ([`b0db6a7`](https://github.com/hydro-project/infinity/commit/b0db6a7a0764ddab7df1f5cf3fcefc7129c6ddcb))
    - Allow auto quit without quit picker when agent is idle ([`3285dc5`](https://github.com/hydro-project/infinity/commit/3285dc5078947b76ad440342316dbd1d665800f4))
    - Add steering file instructions to the default agent prompt ([`40b1f78`](https://github.com/hydro-project/infinity/commit/40b1f78d18466b99040caecc772adfaa7c6ed705))
</details>

