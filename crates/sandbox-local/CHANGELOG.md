

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
 - <csr-id-8b9db6bd4fe0572e4682115340361f7ad8f41b70/> expandable diffs and render performance overhaul
   **Backend** (`crates/sandbox-core/src/server.rs`):
   - Rewrote `push_diff_view` to send old/new file contents per changed file
   instead of a unified diff patch string.
   - Added `parse_changed_files` helper for `jj diff --summary` / `git diff --name-status`.
   - For each file, fetches contents via `jj file show` / `git show` from parent
   and current revisions. Handles Jj, Git, and Brazil modes.
   - New payload format: `{ "files": [{ path, status, oldContents, newContents }] }`
   
   **Frontend** (`infinity-web/src/components/DiffView.tsx`):
   - Switched from `PatchDiff` (patch string) to `MultiFileDiff` (old/new file
   contents) from `@pierre/diffs`. Pierre computes the diff internally with
   full file context, enabling native expand/collapse of unchanged sections.
   - Default `hunkSeparators: "line-info"` provides clickable expand buttons.
 - <csr-id-ad18f9d280af5b8d33ea3f35fd12890f2603d7c2/> detect and warn when a sandbox bookmark is moved externally
   Added detection for external bookmark modifications in sandbox-local.
   When a user moves a sandbox's jj bookmark to a different revision between
   operations, the next modifying tool call (edit_file, create_file, etc.)
   now prepends a warning to the result:
   
   "Warning: bookmark 'sandbox-xyz' was moved externally; overwriting
   with sandbox working copy."
   
   The warning is also logged via tracing::warn for server-side debugging.
 - <csr-id-16b50c811830ba2c707f0aaf973cc90ad555e933/> pretty-print describe_overall_changes for the terminal
   - Added `display_script` to the `describe_overall_changes` tool definition so
   the invocation line shows `◆ Describe changes: <first line>` instead of raw
   JSON with literal `\n`s.
   - Return the full commit message as a `DisplaySegment::Text` in the tool
   result's `display_as` field, so the terminal renders it with `✓` and
   continuation lines. The agent still only sees `"Edits described."` in its
   context.
   - Updated the tool description to tell the agent not to repeat the summary
   since it is now displayed automatically.
   - Added `invoke_raw` test helper (deduplicated with `invoke` as a thin
   wrapper) and `describe_returns_display_segments` test to verify the
   `display_as` segments are returned correctly.
 - <csr-id-c8a9d447f439931ce9ad534af156edb68f64d2a0/> add whitespace-tolerant fallback for `edit_file` matching
 - <csr-id-7085405bbfa8d07f6a69bc0e418761a56d108a67/> add RAP view_update protocol + diff view in web UI
 - <csr-id-ba10ffd62644a4c86c31a7fb6d5eaaca8c403b55/> add remote host migration UI and daemon orchestration
 - <csr-id-73708c07ed08acfd388bdf26654e71f9ab3184bd/> use user.name/email with fallback

### Bug Fixes

 - <csr-id-87902de1c882a64ae78fe53b99a1f8ba50eac57f/> detect repository root when the requested path is nested inside a repo
   * Add `sandbox_core::find_repo_root`, which walks up from a path to the
   closest ancestor containing a `.jj` directory or a `.git` entry
   (directory, or file for worktrees/submodules). This makes jj repos
   detectable when the cwd is inside a nested folder — jj subdirectories
   carry no marker of their own, so the previous `path.join(".jj")` check
   silently fell through to git mode (or failed entirely for
   non-colocated jj repos).
   * `LocalBackend::init_repo` now resolves the requested path to the
   repository root before storing it as the remote URI, so sandboxes are
   always created from the repo root. Falls back to the path as given
   when no root is found, preserving existing error paths (including
   Direct mode on non-VCS directories).
   * `clone_repo` and `open_sandbox_direct` responses now include a note
   when the detected repository root differs from the requested path,
   telling the agent that the sandbox operates on the whole repository
   and that file paths are relative to the root. Both handlers append
   the note with the same `if let Some(note)` + `push_str` pattern, and
   `handle_open_sandbox_direct` documents the distinction between the
   caller-requested path (`args.repo`) and the backend-resolved root
   (`remote_uri`).
   * The migration-import cwd check also walks up to the repo root instead
   of requiring cwd to be the root itself.
   * Add integration tests (`nested_repo_root.rs`) covering jj and git
   clones from nested directories (mode detection + root note + reads
   relative to the root) and the no-note case when cloning from the root.
 - <csr-id-095c36013e3b3ff57662b975361cc94534b71e5c/> pass `--ignore-working-copy` to jj cleanup commands
   When a parent thread continues working after a child is done, the child's
   jj workspace becomes stale. On `close_thread`, `jj workspace forget` and
   `jj abandon` fail with "working copy is stale", leaving the child's
   workspace and commit dangling in jj.
   
   * Add `--ignore-working-copy` to `workspace forget` and `abandon` in
   `cleanup_sandbox_permanently` (async close_thread path)
   * Add `--ignore-working-copy` to the same commands in `Drop for LocalBackend`
   (server shutdown sync path)
   * Add `--ignore-working-copy` to `jj_bookmark_is_empty` (both async and sync)
   * Add `--ignore-working-copy` to `jj git export` from the orig dir in
   `push_sandbox`
   * Add regression test reproducing the production scenario
 - <csr-id-49fda2e8aea86b8e5eb90da4cc9298b0d5a8fb47/> use TempDir with Drop for per-sandbox TMPDIR + update agent docs
   Addresses CR-276014461 review feedback:
 - <csr-id-be95ba464e3b7c84aa2f86a7f924faf79907b83e/> use `TempDir` with Drop semantics for per-sandbox TMPDIR
   Addresses CR-276014461 review feedback (shadajl, benschof):
   
   - Store a `tempfile::TempDir` in `CachedSandbox` instead of a deterministic
   persistent path with manual cleanup
   - Remove `tmp_dir_for()` and `get_or_create_tmp_dir()` helpers
   - Remove manual `remove_dir_all` in `cleanup_sandbox_permanently`
   - `spawn_command` now reads the tmp path directly from the cache entry
   
   Lifecycle semantics: temp files persist across commands within a session
   (TempDir lives in the cache) but are cleaned up on Drop — matching the
   best-effort guarantees of `.gitignore`'d files.
 - <csr-id-25a040760b4343276682d379ac71611484c360ad/> jj bookmark set failing when bookmark is moved forward externally
   Added `-B` (--allow-backwards) to `jj bookmark set` in `jj_push_working_copy`
   so that the sandbox can move the bookmark back to its working copy even when
   the bookmark has been moved forward externally (e.g., to a descendant commit
   after a child sandbox squash).
   
   Added a regression test `external_bookmark_move_forward_is_overwritten_by_next_edit`
   that creates a descendant commit, moves the bookmark forward to it, then
   verifies the sandbox can still make edits. Applied `cargo fmt` to fix formatting.
 - <csr-id-3f0e860e36777a11d560b3e92f0623f14927b517/> allow sccache to work properly inside bwrap/sandbox-exec sandboxes
 - <csr-id-81cef5217ac2e94d72752b97acb35f48ee64e8a4/> sync colocated git refs after jj push_sandbox
 - <csr-id-a0b74f2b3a8b732e6731e1e94d2bff57d0ce422f/> prevent git from resolving outer repo in sandboxes
 - <csr-id-1285fef3439100f51b312ee948fac223d1eba298/> run `workspace update-stale` before squashing stacked commits
 - <csr-id-e3ad1f63046b7720bbb703425603724bd3b5f019/> preserve jj commit message on execute_command, replace self-spawn with process_group
 - <csr-id-2759040634532e82b9fe9dc53fc646a78220bb42/> delete child metadata on squash to prevent migration failure

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

 - <csr-id-6b92106528d80a3636f95ab12105b347ebe939a9/> remove jj command logging mechanism from execute_command
   The execute_command handler had a mechanism that called `jj new` + `jj describe`
   before running each command, then `jj squash --into @-` after completion, to log
   command strings into the jj evolog. This was broken with parallel commands because
   concurrent `jj new`/`jj squash` operations conflict.
 - <csr-id-7634b823ad70378e666379a9a8e8a7935a06026f/> replace all .unwrap() with .expect() and fix clippy warnings
 - <csr-id-9757071818663cefb8e6a12438071d95000379a8/> add precheck script, lints
 - <csr-id-f4a676b825e9029816428c4c4c637e6a91f92c23/> inline tempfile::tempdir() calls, remove make_scratch_tempdir helper

### Test

 - <csr-id-eb8018be833adca39ebe44f3e86efa9601aa77d6/> add execute_command immediately after clone_repo tests
   * `jj_execute_immediately_after_clone` — verifies `execute_command` right
   after `clone_repo` in jj mode completes without hanging or errors.
   * `git_execute_immediately_after_clone` — same verification for plain git
   (non-jj) mode.
   
   Both confirm no race conditions or hangs when a command runs with no
   intermediate operations after repo initialization.
 - <csr-id-4e433957364cf6cb70f471588836e053011fff35/> add integration test for unmerged subagent changes in jj
 - <csr-id-fbe1ff6527e67bddc2876b846cadd168c5291de9/> add TMPDIR isolation assertion and improve execute_command guidance
 - <csr-id-d02f9efe56d05d75cbae83afa72aba718980b23d/> add integration test for sccache cache dir writability under bwrap
 - <csr-id-d61f3d5a1c40053b7be8eb23fdfe35c735225430/> add check.bash CI script, replace precheck.bash
 - <csr-id-baf6b86cacd291ebf4713e50d9ef866dd8e2da95/> add clone_repo re-init and direct upgrade tests
 - <csr-id-430b314a1d6904abb2ebb04b96af8e86d85cf894/> use TMPDIR separate sandbox, test jj desc/author

### New Features (BREAKING)

 - <csr-id-c43161629513d2f163ca7ab44c0a1093386118bb/> extensible sandbox modes via `ModeProvider`, with jj and git as built-in providers
   Adds an extension mechanism for custom VCS backends and makes the built-in
   Jujutsu and Git modes plain instances of it, so every non-Direct mode flows
   through the same interface end to end.
   
   ## Core (`sandbox-core`)
   
   * New `ModeProvider` trait: the extension point for sandbox modes. A provider
   claims a repository during `clone_repo` (`detect` → `ModeInit`) and then
   handles all mode-specific behavior: `create_sandbox`, `refresh_sandbox`,
   `describe`, `detect_external_change` (e.g. bookmark-move warnings),
   `push_sandbox`, `squash`, `diff_files`, `cleanup`, `cleanup_blocking`
   (sync, for `Drop`), and `extra_writable_paths`.
   * New `SandboxMode::Custom { id, data }` variant carrying an opaque,
   provider-defined JSON payload; existing `Jj`/`Git`/`Direct` serialization
   is unchanged, so persisted metadata stays compatible.
   * `SandboxBackend` reshaped around providers:
   * `init_repo` is gone — `detect_mode(ctx)` is the single `clone_repo`
   entry point where each backend does its repository setup and then
   consults its providers. `ModeInit::repo_root` (canonicalized) becomes
   the repo's `remote_uri`.
   * Direct mode, which bypasses providers, gets its own `init_direct` hook
   (default: unsupported).
   * New delegating methods `describe_sandbox`, `detect_external_change`,
   `squash_sandbox`, and `diff_files`.
   * `server.rs` is now mode-agnostic: clone detection, the describe and
   external-change logic in `with_sandbox`, `squash_sandbox`, and the diff
   view all delegate to the backend instead of matching on Jj/Git. The diff
   view is now also pushed after git/custom squashes (previously jj-only).
   Session migration remains enum-based since it operates on git bundles and
   rejects custom modes.
   * The reusable jj/git mode logic (detect, describe, external-change warning,
   squash, diff) lives in `jj.rs`/`git.rs`, shared by the local providers and
   the EFS backend. `git::detect_mode` is the documented fallback that claims
   any repo not claimed earlier, preserving the friendly
   "use `open_sandbox_direct`" error for commit-less repos.
   
   ## Local backend (`sandbox-local`)
   
   * New `providers` module with `LocalJjProvider` and `LocalGitProvider`
   (jj workspaces / git worktrees under `{repo}/.infinity/.sandboxes/`).
   * `LocalBackend` holds an ordered `Vec<Arc<dyn ModeProvider>>` — jj then git
   by default; `LocalBackend::new(enabled).with_provider(p)` prepends an
   external provider ahead of the built-ins. All mode dispatch (creation,
   push, squash, diff, cleanup, `Drop`-time cleanup, per-mode writable paths)
   goes through the registry. Dropping a cached sandbox whose mode has no
   registered provider logs a warning instead of silently leaking it.
   * The old `init_repo` root resolution became the private `resolve_repo_root`,
   used by both `detect_mode` and `init_direct` (Direct mode still works on
   non-VCS directories).
   
   ## Remote backend (`sandbox-remote`)
   
   * `EfsBackend` (jj-only) implements the backend methods via the shared
   `sandbox_core::jj` helpers; the old `init_repo` became the private
   `mirror_repo` step of `detect_mode`, making the URL-mirror-then-detect
   flow explicit in one place. `open_sandbox_direct` now fails immediately on
   the remote backend (previously it stored Direct state that failed at
   first use).
   
   Verified with `cargo fmt --check`, clippy (`-D warnings`) on the touched
   crates, workspace-wide `cargo check --all-targets`, and the
   sandbox-core/local test suites — all pass except 4 pre-existing
   sandboxed-execution tests that cannot run in this environment (nested macOS
   `sandbox-exec` is not permitted).

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

 - 76 commits contributed to the release.
 - 36 commits were understood as [conventional](https://www.conventionalcommits.org).
 - 9 unique issues were worked on: [#107](https://github.com/hydro-project/infinity/issues/107), [#113](https://github.com/hydro-project/infinity/issues/113), [#30](https://github.com/hydro-project/infinity/issues/30), [#61](https://github.com/hydro-project/infinity/issues/61), [#65](https://github.com/hydro-project/infinity/issues/65), [#75](https://github.com/hydro-project/infinity/issues/75), [#78](https://github.com/hydro-project/infinity/issues/78), [#8](https://github.com/hydro-project/infinity/issues/8), [#83](https://github.com/hydro-project/infinity/issues/83)

### Commit Details

<csr-read-only-do-not-edit/>

<details><summary>view details</summary>

 * **[#107](https://github.com/hydro-project/infinity/issues/107)**
    - Set up cargo-smart-release release workflow (mirroring hydro) ([`ffc27d0`](https://github.com/hydro-project/infinity/commit/ffc27d0bf5d964a655fedab9460bf5017971e6b6))
 * **[#113](https://github.com/hydro-project/infinity/issues/113)**
    - Introduce typed ThreadId for RAP group ids ([`4b18b37`](https://github.com/hydro-project/infinity/commit/4b18b37de219cb7fe27ce7c027b87f4fb35fbbf5))
 * **[#30](https://github.com/hydro-project/infinity/issues/30)**
    - Pass `--ignore-working-copy` to jj cleanup commands ([`095c360`](https://github.com/hydro-project/infinity/commit/095c36013e3b3ff57662b975361cc94534b71e5c))
 * **[#61](https://github.com/hydro-project/infinity/issues/61)**
    - Multimodal (image) tool results end-to-end, with image display + review fixes ([`1935c38`](https://github.com/hydro-project/infinity/commit/1935c387d806a1da271e15078b26e06f228737c6))
 * **[#65](https://github.com/hydro-project/infinity/issues/65)**
    - Detect repository root when the requested path is nested inside a repo ([`87902de`](https://github.com/hydro-project/infinity/commit/87902de1c882a64ae78fe53b99a1f8ba50eac57f))
 * **[#75](https://github.com/hydro-project/infinity/issues/75)**
    - Detect repository root when the requested path is nested inside a repo ([`87902de`](https://github.com/hydro-project/infinity/commit/87902de1c882a64ae78fe53b99a1f8ba50eac57f))
 * **[#78](https://github.com/hydro-project/infinity/issues/78)**
    - Extensible sandbox modes via `ModeProvider`, with jj and git as built-in providers ([`c431616`](https://github.com/hydro-project/infinity/commit/c43161629513d2f163ca7ab44c0a1093386118bb))
 * **[#8](https://github.com/hydro-project/infinity/issues/8)**
    - Add automated THIRD-PARTY file generation with license enforcement ([`e2e0719`](https://github.com/hydro-project/infinity/commit/e2e0719faebbffc72ec7bd8a8b3b02223da8ba0e))
 * **[#83](https://github.com/hydro-project/infinity/issues/83)**
    - Ensure `check.bash` passes ([`892cb62`](https://github.com/hydro-project/infinity/commit/892cb628cb114102afa29c09e3e798c3dee1b381))
 * **Uncategorized**
    - Release infinity-provider-protocol v0.1.0, infinity-provider-bedrock v0.1.0, infinity-provider-rig v0.1.0, infinity-agent-core v0.1.0, infinity-mcp-bridge v0.1.0, infinity-rap-bridge v0.1.0, infinity-daemon v0.1.0, infinity-agent-cli v0.1.0, sandbox-core v0.1.0, sandbox-local v0.1.0, sandbox-remote v0.1.0 ([`7e1cd1d`](https://github.com/hydro-project/infinity/commit/7e1cd1df69d8fce402bef4085e9d17f871994503))
    - Release rap-protocol v0.1.0, rap-client v0.1.0, rap-steering-server v0.1.0, rap-github-event-poller v0.1.0, infinity-protocol v0.1.0, infinity-provider-protocol v0.1.0, infinity-provider-bedrock v0.1.0, infinity-provider-rig v0.1.0, infinity-agent-core v0.1.0, infinity-mcp-bridge v0.1.0, infinity-rap-bridge v0.1.0, infinity-daemon v0.1.0, infinity-agent-cli v0.1.0, sandbox-core v0.1.0, sandbox-local v0.1.0, sandbox-remote v0.1.0 ([`dd8c7f4`](https://github.com/hydro-project/infinity/commit/dd8c7f49028a26052d785b4241f9ade125f0afb3))
    - Use TempDir with Drop for per-sandbox TMPDIR + update agent docs ([`49fda2e`](https://github.com/hydro-project/infinity/commit/49fda2e8aea86b8e5eb90da4cc9298b0d5a8fb47))
    - Use `TempDir` with Drop semantics for per-sandbox TMPDIR ([`be95ba4`](https://github.com/hydro-project/infinity/commit/be95ba464e3b7c84aa2f86a7f924faf79907b83e))
    - Remove unused `_keepalive` field from SpawnedCommand ([`24820fe`](https://github.com/hydro-project/infinity/commit/24820fe9b2be138774a8a4e069019b9b11444a0d))
    - Fix sandbox-local TMPDIR changing between commands ([`a225604`](https://github.com/hydro-project/infinity/commit/a22560452b5da1edecf7ed4fccecc0fc8588e2bc))
    - Jj bookmark set failing when bookmark is moved forward externally ([`25a0407`](https://github.com/hydro-project/infinity/commit/25a040760b4343276682d379ac71611484c360ad))
    - Remove jj command logging mechanism from execute_command ([`6b92106`](https://github.com/hydro-project/infinity/commit/6b92106528d80a3636f95ab12105b347ebe939a9))
    - Expandable diffs and render performance overhaul ([`8b9db6b`](https://github.com/hydro-project/infinity/commit/8b9db6bd4fe0572e4682115340361f7ad8f41b70))
    - Add execute_command immediately after clone_repo tests ([`eb8018b`](https://github.com/hydro-project/infinity/commit/eb8018be833adca39ebe44f3e86efa9601aa77d6))
    - Detect and warn when a sandbox bookmark is moved externally ([`ad18f9d`](https://github.com/hydro-project/infinity/commit/ad18f9d280af5b8d33ea3f35fd12890f2603d7c2))
    - Pretty-print describe_overall_changes for the terminal ([`16b50c8`](https://github.com/hydro-project/infinity/commit/16b50c811830ba2c707f0aaf973cc90ad555e933))
    - Add integration test for unmerged subagent changes in jj ([`4e43395`](https://github.com/hydro-project/infinity/commit/4e433957364cf6cb70f471588836e053011fff35))
    - Add TMPDIR isolation assertion and improve execute_command guidance ([`fbe1ff6`](https://github.com/hydro-project/infinity/commit/fbe1ff6527e67bddc2876b846cadd168c5291de9))
    - Add integration test for sccache cache dir writability under bwrap ([`d02f9ef`](https://github.com/hydro-project/infinity/commit/d02f9efe56d05d75cbae83afa72aba718980b23d))
    - Allow sccache to work properly inside bwrap/sandbox-exec sandboxes ([`3f0e860`](https://github.com/hydro-project/infinity/commit/3f0e860e36777a11d560b3e92f0623f14927b517))
    - Sync colocated git refs after jj push_sandbox ([`81cef52`](https://github.com/hydro-project/infinity/commit/81cef5217ac2e94d72752b97acb35f48ee64e8a4))
    - Prevent git from resolving outer repo in sandboxes ([`a0b74f2`](https://github.com/hydro-project/infinity/commit/a0b74f2b3a8b732e6731e1e94d2bff57d0ce422f))
    - Add whitespace-tolerant fallback for `edit_file` matching ([`c8a9d44`](https://github.com/hydro-project/infinity/commit/c8a9d447f439931ce9ad534af156edb68f64d2a0))
    - Add workspace lints and fix all lint violations ([`b92b7a1`](https://github.com/hydro-project/infinity/commit/b92b7a17f4b69e2652f5cce813320eca851717e4))
    - Add check.bash CI script, replace precheck.bash ([`d61f3d5`](https://github.com/hydro-project/infinity/commit/d61f3d5a1c40053b7be8eb23fdfe35c735225430))
    - Run `workspace update-stale` before squashing stacked commits ([`1285fef`](https://github.com/hydro-project/infinity/commit/1285fef3439100f51b312ee948fac223d1eba298))
    - Preserve jj commit message on execute_command, replace self-spawn with process_group ([`e3ad1f6`](https://github.com/hydro-project/infinity/commit/e3ad1f63046b7720bbb703425603724bd3b5f019))
    - Delete child metadata on squash to prevent migration failure ([`2759040`](https://github.com/hydro-project/infinity/commit/2759040634532e82b9fe9dc53fc646a78220bb42))
    - Add RAP view_update protocol + diff view in web UI ([`7085405`](https://github.com/hydro-project/infinity/commit/7085405bbfa8d07f6a69bc0e418761a56d108a67))
    - Add remote host migration UI and daemon orchestration ([`ba10ffd`](https://github.com/hydro-project/infinity/commit/ba10ffd62644a4c86c31a7fb6d5eaaca8c403b55))
    - Replace all .unwrap() with .expect() and fix clippy warnings ([`7634b82`](https://github.com/hydro-project/infinity/commit/7634b823ad70378e666379a9a8e8a7935a06026f))
    - Add precheck script, lints ([`9757071`](https://github.com/hydro-project/infinity/commit/9757071818663cefb8e6a12438071d95000379a8))
    - Change idle_tx semantics to "might be idle" and ping on client disconnect ([`3127d65`](https://github.com/hydro-project/infinity/commit/3127d65607bede5077d0a675c67f294d98f7e177))
    - Add clone_repo re-init and direct upgrade tests ([`baf6b86`](https://github.com/hydro-project/infinity/commit/baf6b86cacd291ebf4713e50d9ef866dd8e2da95))
    - Move HistoryManager to interior mutability; remove callback_with_history hack; restore subscribe_rx in select ([`1e92087`](https://github.com/hydro-project/infinity/commit/1e9208751e55d0029acd419ae12f1bf05cc7104e))
    - Use user.name/email with fallback ([`73708c0`](https://github.com/hydro-project/infinity/commit/73708c07ed08acfd388bdf26654e71f9ab3184bd))
    - Inline tempfile::tempdir() calls, remove make_scratch_tempdir helper ([`f4a676b`](https://github.com/hydro-project/infinity/commit/f4a676b825e9029816428c4c4c637e6a91f92c23))
    - Use TMPDIR separate sandbox, test jj desc/author ([`430b314`](https://github.com/hydro-project/infinity/commit/430b314a1d6904abb2ebb04b96af8e86d85cf894))
    - Add Direct sandbox mode and better error for empty repos ([`12b7454`](https://github.com/hydro-project/infinity/commit/12b7454c172b2bb455c96e6c25c2096e3348bc49))
    - Add support for UserChoice prompts in RAP protocol and use for permissions expansion in sandbox ([`b0db6a7`](https://github.com/hydro-project/infinity/commit/b0db6a7a0764ddab7df1f5cf3fcefc7129c6ddcb))
    - Shift core agent runtime into a daemon with a network protocol for clients ([`141d697`](https://github.com/hydro-project/infinity/commit/141d69792c3aa951fcbfbea847879582f1d06ec3))
    - Add SandboxMode enum with Jj/Git variants; thread description through push_sandbox ([`bc26456`](https://github.com/hydro-project/infinity/commit/bc26456ef75bafcf57ee2b7f568f0a04330d294d))
    - Move sccache pre-start from CLI to sandbox backend ([`c190095`](https://github.com/hydro-project/infinity/commit/c1900951a752f75bbac38fb5a7b29f027a94ebdd))
    - Fix Jujutsu not initializing when run from Git repo ([`7e0270f`](https://github.com/hydro-project/infinity/commit/7e0270f8865cd6748bb8422167aa54e22946c38d))
    - Fix binary install path for local sandbox and improve MCP output ([`e1dd438`](https://github.com/hydro-project/infinity/commit/e1dd438ff6e41440a38b9755fa1a9af284dca58e))
    - Auto-install bubblewrap on Linux hosts ([`149a498`](https://github.com/hydro-project/infinity/commit/149a4981285a50a2186fd49f7fbad8fedd4bfb90))
    - Refactor JJ sandbox lifecycle: absolute revisions, cleanup on shutdown ([`f525abf`](https://github.com/hydro-project/infinity/commit/f525abfe4f5fcd28b5e62c8678df542b5924a308))
    - Fix "branch already exists" error when restoring sandbox after CLI restart ([`3733863`](https://github.com/hydro-project/infinity/commit/37338633f94f19dbe9095aeb17cd5ce482a8d96e))
    - Refactor sandbox to support dynamic extra writable paths ([`6db3cba`](https://github.com/hydro-project/infinity/commit/6db3cbaef50daa3daa3f062e5c8f6e5d09e93f5d))
    - Add git helpers and improve sandbox logging/tempdir defaults ([`e481b88`](https://github.com/hydro-project/infinity/commit/e481b88cd00e82b7c12507198f3fbab5b6ed7183))
    - Default sandbox temp directories to ./sandboxes instead of OS temp dir ([`ef6e507`](https://github.com/hydro-project/infinity/commit/ef6e5073167efce2aba439c303e3c2d20786fc57))
    - Fix CLI hang on Ctrl+C/D when sandbox commands are running ([`28e79c7`](https://github.com/hydro-project/infinity/commit/28e79c78ff3289403bb2b7c324a4697f091a88f5))
    - Add squash_sandbox tool and base_thread_id to clone_repo ([`5ffac5f`](https://github.com/hydro-project/infinity/commit/5ffac5f4d28c8efe8dfb861d883921292cf31423))
    - Enable sandboxing by default and implement bubblewrap support on Linux ([`d5f5fe4`](https://github.com/hydro-project/infinity/commit/d5f5fe49f48e3b0b15dcb76585c60751bf4ef4f1))
    - Add --tempdir CLI flag to sandbox-local for custom tempdir base path ([`1ed369e`](https://github.com/hydro-project/infinity/commit/1ed369e6a7695ce7990de68c85bd6f3cb8cd2ef3))
    - Correctly handle cancellation using process groups ([`c7f9589`](https://github.com/hydro-project/infinity/commit/c7f9589773ff4c02d0efbf851d1b095f147453c2))
    - Change local sandbox metadata store to file-based and set RAP server CWD to .infinity ([`0a891e3`](https://github.com/hydro-project/infinity/commit/0a891e3e69d0baa24fdc34d527259cea60fb7dec))
    - Fix escaping of grep tool arguments and detect cd to original folder ([`8b32a63`](https://github.com/hydro-project/infinity/commit/8b32a63ddb205fb1cf7e41284e3bf6a9edd131f9))
    - Enforce absolute paths in local sandbox and add CWD to system prompt ([`c7fa225`](https://github.com/hydro-project/infinity/commit/c7fa225eb52f91077f53ddc7c63ad9546b70e45b))
    - Format and update snapshots ([`772a00c`](https://github.com/hydro-project/infinity/commit/772a00c383299383409c6ff8c834d344bdec4d11))
    - Implement output streaming for execute_command using RAP subscriptions with debouncing. ([`b2fb764`](https://github.com/hydro-project/infinity/commit/b2fb7643665e2052419103a4c7d4466758b0e026))
    - Add RAP protocol for notifying tool servers of thread closure ([`2d60e9d`](https://github.com/hydro-project/infinity/commit/2d60e9d12b84d01984b17e56c859caac8757859d))
    - Persist display_as mapping in store.json and use it during history replay, document ([`1ca626a`](https://github.com/hydro-project/infinity/commit/1ca626a6d1fd200b1267c548079878192811d096))
    - Improve jj config management and shift spinner to top ([`40a3ac5`](https://github.com/hydro-project/infinity/commit/40a3ac57cf3b1a336a413862aa4c6b29fa1dc935))
    - Synchronize base repo workspace status when the user has loaded a sandbox commit ([`061be00`](https://github.com/hydro-project/infinity/commit/061be0029159c2ebc4cdd67dd1388a6c092517b2))
    - Support launching RAP servers as a subprocess ([`5439050`](https://github.com/hydro-project/infinity/commit/5439050aef622ee1ac16227ded7646e3d08e55fb))
    - Store RAP config in a local directory ([`71c8101`](https://github.com/hydro-project/infinity/commit/71c81015e8d2b096aaa533d16e94b6944480b16d))
    - Run clippy ([`ea864bf`](https://github.com/hydro-project/infinity/commit/ea864bf5a21cb030738936df2749af7ad0c255d8))
    - Clean up dependencies ([`fcef65d`](https://github.com/hydro-project/infinity/commit/fcef65df6274e43596bf84f9b2eaf4d8955e9b93))
    - Cache jj workspaces for local sandboxes ([`ba0ba4e`](https://github.com/hydro-project/infinity/commit/ba0ba4e372432f9d1044f0e57e06b9ada870de30))
    - Use jj workspaces ([`8d6bc44`](https://github.com/hydro-project/infinity/commit/8d6bc4477531f5a181ae5276a581978b4b2a225a))
    - Initial functional Jujutsu filesystem sandbox ([`4118c89`](https://github.com/hydro-project/infinity/commit/4118c890809b1f93e0ca92a6861ab9351e6e8864))
</details>

