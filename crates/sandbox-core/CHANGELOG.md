

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
 - <csr-id-0ba5d1b522d484a02a948352613ff01171b118c4/> return diff from create_file for Pierre pretty printing
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
 - <csr-id-cb15aa6da38e31c71b2cd71d4ec192150bf0c393/> resolve clippy 1.97 question_mark lints in server.rs
   CI's Lint job (clippy 1.97) fails on three new `clippy::question_mark`
   warnings that local stable clippy 1.96 does not yet flag:
   
   * `detect_cd_to_original_repo`: replace the two
   `match stripped.find(...) { Some(end) => ..., None => return None }`
   blocks (double- and single-quoted `cd` path parsing) with
   `let end = stripped.find(...)?;`
   * `parse_changed_files`: rewrite the trailing
   `else if let Some(rest) = ... else { return None }` branch for the
   `D` (deleted) status using the `?` operator inside the `else` block
   
   No behavior change. Verified with cargo fmt, stable and nightly
   `cargo clippy --all-targets -- -D warnings` (both clean workspace-wide),
   and `cargo test -p sandbox-core` (14/14 pass).
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
 - <csr-id-1f902f58463344c0c7d7e604bd103389d8b3915b/> add --max-columns 1000 to ripgrep grep tool
   Added `--max-columns 1000` flag to the `rg` command in `handle_grep()` to
   prevent extremely long lines (e.g. from minified JS/CSS files) from flooding
   the agent's context window. Lines exceeding 1000 characters will be truncated
   by ripgrep, preserving useful context for normal source files.
 - <csr-id-25a040760b4343276682d379ac71611484c360ad/> jj bookmark set failing when bookmark is moved forward externally
   Added `-B` (--allow-backwards) to `jj bookmark set` in `jj_push_working_copy`
   so that the sandbox can move the bookmark back to its working copy even when
   the bookmark has been moved forward externally (e.g., to a descendant commit
   after a child sandbox squash).
   
   Added a regression test `external_bookmark_move_forward_is_overwritten_by_next_edit`
   that creates a descendant commit, moves the bookmark forward to it, then
   verifies the sandbox can still make edits. Applied `cargo fmt` to fix formatting.
 - <csr-id-cbb03c1d54b82e3ec4425e2116552d72fc97c9a2/> pass full file contents to build_edit_diff so line numbers are correct
   In `handle_edit_file`, `build_edit_diff` was called with only the `old_str`/`new_str`
   snippets instead of the full file contents. This caused `similar` to generate hunk
   headers starting at line 1 (e.g. `@@ -1,3 +1,3 @@`) regardless of where the edit
   actually occurred. Pierre then rendered those incorrect line numbers literally.
   
   Changed to pass `&content` (original file) and `&new_content` (modified file) so the
   unified diff contains accurate line offsets.
 - <csr-id-d638b171d98bc30af30e085896489e9802c671d6/> compute diff view from bookmark parent instead of base revision
 - <csr-id-8eda4e273f1468bfeece99da4898bddd717ec1ee/> compute jj diff from workspace dir instead of orig repo
 - <csr-id-a0b74f2b3a8b732e6731e1e94d2bff57d0ce422f/> prevent git from resolving outer repo in sandboxes
 - <csr-id-1285fef3439100f51b312ee948fac223d1eba298/> run `workspace update-stale` before squashing stacked commits
 - <csr-id-e6846ae64082c8ce49bc57e9a144e71f07c2208f/> clean up cached sandbox worktrees after migration
 - <csr-id-e3ad1f63046b7720bbb703425603724bd3b5f019/> preserve jj commit message on execute_command, replace self-spawn with process_group
 - <csr-id-2759040634532e82b9fe9dc53fc646a78220bb42/> delete child metadata on squash to prevent migration failure
 - <csr-id-901f9177beb74cd2f56c1d8d59cd1d64488604ac/> ensure jj sandboxes are loaded before migration export
 - <csr-id-37387d634305c22bab23d41f7ab535cdbd13802d/> reject clone_repo re-init unless upgrading from Direct mode

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
 - <csr-id-0951c1d559b5cdd7f6605eefae065c00d4165df7/> return error when ripgrep is not installed
   The grep tool runs `rg` via execute_command. When `rg` is not on PATH,
   the command exits with code 1 and empty stdout, which is indistinguishable
   from a legitimate "no matches" result — silently returning zero results.
   
   Use the `which` crate to check for `rg` before creating a sandbox. If it
   is not found, return a SandboxError with a clear message instead of
   proceeding with a command that will silently fail.

### Refactor

 - <csr-id-6b92106528d80a3636f95ab12105b347ebe939a9/> remove jj command logging mechanism from execute_command
   The execute_command handler had a mechanism that called `jj new` + `jj describe`
   before running each command, then `jj squash --into @-` after completion, to log
   command strings into the jj evolog. This was broken with parallel commands because
   concurrent `jj new`/`jj squash` operations conflict.
 - <csr-id-7634b823ad70378e666379a9a8e8a7935a06026f/> replace all .unwrap() with .expect() and fix clippy warnings
 - <csr-id-9757071818663cefb8e6a12438071d95000379a8/> add precheck script, lints

### Style

 - <csr-id-db2bfffff0273c3ac58247766f1376d73d3ba8f9/> remove backticks from execute_command tool description

### Test

 - <csr-id-fbe1ff6527e67bddc2876b846cadd168c5291de9/> add TMPDIR isolation assertion and improve execute_command guidance

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

 - 92 commits contributed to the release.
 - 37 commits were understood as [conventional](https://www.conventionalcommits.org).
 - 10 unique issues were worked on: [#10](https://github.com/hydro-project/infinity/issues/10), [#107](https://github.com/hydro-project/infinity/issues/107), [#113](https://github.com/hydro-project/infinity/issues/113), [#30](https://github.com/hydro-project/infinity/issues/30), [#61](https://github.com/hydro-project/infinity/issues/61), [#65](https://github.com/hydro-project/infinity/issues/65), [#75](https://github.com/hydro-project/infinity/issues/75), [#77](https://github.com/hydro-project/infinity/issues/77), [#78](https://github.com/hydro-project/infinity/issues/78), [#8](https://github.com/hydro-project/infinity/issues/8)

### Commit Details

<csr-read-only-do-not-edit/>

<details><summary>view details</summary>

 * **[#10](https://github.com/hydro-project/infinity/issues/10)**
    - Add GitHub Actions workflows for lints, tests, conventional commits, and docs ([`ea6b62e`](https://github.com/hydro-project/infinity/commit/ea6b62e7b00f2a6b7e7338fa12e60fb3a46bb012))
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
 * **[#77](https://github.com/hydro-project/infinity/issues/77)**
    - Resolve clippy 1.97 question_mark lints in server.rs ([`cb15aa6`](https://github.com/hydro-project/infinity/commit/cb15aa6da38e31c71b2cd71d4ec192150bf0c393))
 * **[#78](https://github.com/hydro-project/infinity/issues/78)**
    - Extensible sandbox modes via `ModeProvider`, with jj and git as built-in providers ([`c431616`](https://github.com/hydro-project/infinity/commit/c43161629513d2f163ca7ab44c0a1093386118bb))
 * **[#8](https://github.com/hydro-project/infinity/issues/8)**
    - Add automated THIRD-PARTY file generation with license enforcement ([`e2e0719`](https://github.com/hydro-project/infinity/commit/e2e0719faebbffc72ec7bd8a8b3b02223da8ba0e))
 * **Uncategorized**
    - Release infinity-agent-core v0.1.0, infinity-mcp-bridge v0.1.0, infinity-rap-bridge v0.1.0, infinity-daemon v0.1.0, infinity-agent-cli v0.1.0, sandbox-core v0.1.0, sandbox-local v0.1.0, sandbox-remote v0.1.0 ([`7599fbb`](https://github.com/hydro-project/infinity/commit/7599fbbdfad042a6fd85c23002bf937fecbe7b45))
    - Release infinity-provider-protocol v0.1.0, infinity-provider-bedrock v0.1.0, infinity-provider-rig v0.1.0, infinity-agent-core v0.1.0, infinity-mcp-bridge v0.1.0, infinity-rap-bridge v0.1.0, infinity-daemon v0.1.0, infinity-agent-cli v0.1.0, sandbox-core v0.1.0, sandbox-local v0.1.0, sandbox-remote v0.1.0 ([`7e1cd1d`](https://github.com/hydro-project/infinity/commit/7e1cd1df69d8fce402bef4085e9d17f871994503))
    - Release rap-protocol v0.1.0, rap-client v0.1.0, rap-steering-server v0.1.0, rap-github-event-poller v0.1.0, infinity-protocol v0.1.0, infinity-provider-protocol v0.1.0, infinity-provider-bedrock v0.1.0, infinity-provider-rig v0.1.0, infinity-agent-core v0.1.0, infinity-mcp-bridge v0.1.0, infinity-rap-bridge v0.1.0, infinity-daemon v0.1.0, infinity-agent-cli v0.1.0, sandbox-core v0.1.0, sandbox-local v0.1.0, sandbox-remote v0.1.0 ([`dd8c7f4`](https://github.com/hydro-project/infinity/commit/dd8c7f49028a26052d785b4241f9ade125f0afb3))
    - Use TempDir with Drop for per-sandbox TMPDIR + update agent docs ([`49fda2e`](https://github.com/hydro-project/infinity/commit/49fda2e8aea86b8e5eb90da4cc9298b0d5a8fb47))
    - Remove unused `_keepalive` field from SpawnedCommand ([`24820fe`](https://github.com/hydro-project/infinity/commit/24820fe9b2be138774a8a4e069019b9b11444a0d))
    - Use --max-columns-preview, extend ([`4d3576f`](https://github.com/hydro-project/infinity/commit/4d3576f071754c86f36358f3de91c58999089fe7))
    - Add --max-columns 1000 to ripgrep grep tool ([`1f902f5`](https://github.com/hydro-project/infinity/commit/1f902f58463344c0c7d7e604bd103389d8b3915b))
    - Jj bookmark set failing when bookmark is moved forward externally ([`25a0407`](https://github.com/hydro-project/infinity/commit/25a040760b4343276682d379ac71611484c360ad))
    - Remove jj command logging mechanism from execute_command ([`6b92106`](https://github.com/hydro-project/infinity/commit/6b92106528d80a3636f95ab12105b347ebe939a9))
    - Expandable diffs and render performance overhaul ([`8b9db6b`](https://github.com/hydro-project/infinity/commit/8b9db6bd4fe0572e4682115340361f7ad8f41b70))
    - Detect and warn when a sandbox bookmark is moved externally ([`ad18f9d`](https://github.com/hydro-project/infinity/commit/ad18f9d280af5b8d33ea3f35fd12890f2603d7c2))
    - Pretty-print describe_overall_changes for the terminal ([`16b50c8`](https://github.com/hydro-project/infinity/commit/16b50c811830ba2c707f0aaf973cc90ad555e933))
    - Pass full file contents to build_edit_diff so line numbers are correct ([`cbb03c1`](https://github.com/hydro-project/infinity/commit/cbb03c1d54b82e3ec4425e2116552d72fc97c9a2))
    - Compute diff view from bookmark parent instead of base revision ([`d638b17`](https://github.com/hydro-project/infinity/commit/d638b171d98bc30af30e085896489e9802c671d6))
    - Compute jj diff from workspace dir instead of orig repo ([`8eda4e2`](https://github.com/hydro-project/infinity/commit/8eda4e273f1468bfeece99da4898bddd717ec1ee))
    - Add TMPDIR isolation assertion and improve execute_command guidance ([`fbe1ff6`](https://github.com/hydro-project/infinity/commit/fbe1ff6527e67bddc2876b846cadd168c5291de9))
    - Remove backticks from execute_command tool description ([`db2bfff`](https://github.com/hydro-project/infinity/commit/db2bfffff0273c3ac58247766f1376d73d3ba8f9))
    - Prevent git from resolving outer repo in sandboxes ([`a0b74f2`](https://github.com/hydro-project/infinity/commit/a0b74f2b3a8b732e6731e1e94d2bff57d0ce422f))
    - Add whitespace-tolerant fallback for `edit_file` matching ([`c8a9d44`](https://github.com/hydro-project/infinity/commit/c8a9d447f439931ce9ad534af156edb68f64d2a0))
    - Add workspace lints and fix all lint violations ([`b92b7a1`](https://github.com/hydro-project/infinity/commit/b92b7a17f4b69e2652f5cce813320eca851717e4))
    - Return diff from create_file for Pierre pretty printing ([`0ba5d1b`](https://github.com/hydro-project/infinity/commit/0ba5d1b522d484a02a948352613ff01171b118c4))
    - Run `workspace update-stale` before squashing stacked commits ([`1285fef`](https://github.com/hydro-project/infinity/commit/1285fef3439100f51b312ee948fac223d1eba298))
    - Clean up cached sandbox worktrees after migration ([`e6846ae`](https://github.com/hydro-project/infinity/commit/e6846ae64082c8ce49bc57e9a144e71f07c2208f))
    - Preserve jj commit message on execute_command, replace self-spawn with process_group ([`e3ad1f6`](https://github.com/hydro-project/infinity/commit/e3ad1f63046b7720bbb703425603724bd3b5f019))
    - Delete child metadata on squash to prevent migration failure ([`2759040`](https://github.com/hydro-project/infinity/commit/2759040634532e82b9fe9dc53fc646a78220bb42))
    - Add RAP view_update protocol + diff view in web UI ([`7085405`](https://github.com/hydro-project/infinity/commit/7085405bbfa8d07f6a69bc0e418761a56d108a67))
    - Ensure jj sandboxes are loaded before migration export ([`901f917`](https://github.com/hydro-project/infinity/commit/901f9177beb74cd2f56c1d8d59cd1d64488604ac))
    - Add remote host migration UI and daemon orchestration ([`ba10ffd`](https://github.com/hydro-project/infinity/commit/ba10ffd62644a4c86c31a7fb6d5eaaca8c403b55))
    - Replace all .unwrap() with .expect() and fix clippy warnings ([`7634b82`](https://github.com/hydro-project/infinity/commit/7634b823ad70378e666379a9a8e8a7935a06026f))
    - Add precheck script, lints ([`9757071`](https://github.com/hydro-project/infinity/commit/9757071818663cefb8e6a12438071d95000379a8))
    - Reject clone_repo re-init unless upgrading from Direct mode ([`37387d6`](https://github.com/hydro-project/infinity/commit/37387d634305c22bab23d41f7ab535cdbd13802d))
    - Introduce display_as typed variants and use Pierre to display in web client ([`1e65518`](https://github.com/hydro-project/infinity/commit/1e65518e4f041f76e6359b08ff88e32fc8753cda))
    - Display subthreads in web UI and make it possible to connect to subthreads directly ([`718509d`](https://github.com/hydro-project/infinity/commit/718509d481340bd43497530b3f1212b3f3be27af))
    - Use user.name/email with fallback ([`73708c0`](https://github.com/hydro-project/infinity/commit/73708c07ed08acfd388bdf26654e71f9ab3184bd))
    - Add write:/path permissions, thread_ancestors protocol field, and ancestor-aware grant system ([`3464fad`](https://github.com/hydro-project/infinity/commit/3464fade510fd5ab7aa2dc2ffa27f61711c6be31))
    - Unify all duplicate RAP protocol types into rap-protocol crate ([`2def5ee`](https://github.com/hydro-project/infinity/commit/2def5eec01a5c197432a7959942cca8b0eb9d6a0))
    - Use @- as base revision when @ is empty in jj sandbox creation ([`4f522a9`](https://github.com/hydro-project/infinity/commit/4f522a94da5e6fce1e0f225428c19b5a12da9e46))
    - Add Direct sandbox mode and better error for empty repos ([`12b7454`](https://github.com/hydro-project/infinity/commit/12b7454c172b2bb455c96e6c25c2096e3348bc49))
    - Add rig-mock crate and test suite for agent core and daemon ([`abda067`](https://github.com/hydro-project/infinity/commit/abda06757eeba0ac7817374bc89155211cd2edcd))
    - Add support for UserChoice prompts in RAP protocol and use for permissions expansion in sandbox ([`b0db6a7`](https://github.com/hydro-project/infinity/commit/b0db6a7a0764ddab7df1f5cf3fcefc7129c6ddcb))
    - Allow auto quit without quit picker when agent is idle ([`3285dc5`](https://github.com/hydro-project/infinity/commit/3285dc5078947b76ad440342316dbd1d665800f4))
    - Shift core agent runtime into a daemon with a network protocol for clients ([`141d697`](https://github.com/hydro-project/infinity/commit/141d69792c3aa951fcbfbea847879582f1d06ec3))
    - Add steering file instructions to the default agent prompt ([`40b1f78`](https://github.com/hydro-project/infinity/commit/40b1f78d18466b99040caecc772adfaa7c6ed705))
    - Add SandboxMode enum with Jj/Git variants; thread description through push_sandbox ([`bc26456`](https://github.com/hydro-project/infinity/commit/bc26456ef75bafcf57ee2b7f568f0a04330d294d))
    - Try to directly load the existing jj bookmark first before creating a new one ([`a8b6ec7`](https://github.com/hydro-project/infinity/commit/a8b6ec7f22e10b7de2afb814ceed2cb07ea27be0))
    - Fix malformed parsing for describe_overall_changes ([`868bbf3`](https://github.com/hydro-project/infinity/commit/868bbf3cbe44a1651c1204854b6d4dfc7624de3e))
    - Avoid overwriting generated commit message when doing anything other than running a command ([`75f6d40`](https://github.com/hydro-project/infinity/commit/75f6d4015a12228da970658cbaa0e00dc4ac9524))
    - Improve retrying logic and prevent git stdin hangs ([`f747a9b`](https://github.com/hydro-project/infinity/commit/f747a9bac85955a2d53888bff777d5c097d7f740))
    - Fix Jujutsu not initializing when run from Git repo ([`7e0270f`](https://github.com/hydro-project/infinity/commit/7e0270f8865cd6748bb8422167aa54e22946c38d))
    - Refactor JJ sandbox lifecycle: absolute revisions, cleanup on shutdown ([`f525abf`](https://github.com/hydro-project/infinity/commit/f525abfe4f5fcd28b5e62c8678df542b5924a308))
    - Fix worktree handling with pre-commit hooks and re-initialization ([`5cdd757`](https://github.com/hydro-project/infinity/commit/5cdd75719293b2cc4dd52558f5595668c8bd476d))
    - Fix "branch already exists" error when restoring sandbox after CLI restart ([`3733863`](https://github.com/hydro-project/infinity/commit/37338633f94f19dbe9095aeb17cd5ce482a8d96e))
    - Add git helpers and improve sandbox logging/tempdir defaults ([`e481b88`](https://github.com/hydro-project/infinity/commit/e481b88cd00e82b7c12507198f3fbab5b6ed7183))
    - Delete child sandbox bookmark after squashing ([`41be1d0`](https://github.com/hydro-project/infinity/commit/41be1d036645d83600d65841b4f784e6c7f3dfd3))
    - Add displayScript field to RAP tool definitions for pretty-printing tool calls ([`f7e01f2`](https://github.com/hydro-project/infinity/commit/f7e01f2ccfc567fcc44aef1b85eb9e68e3e88131))
    - Fix CLI hang on Ctrl+C/D when sandbox commands are running ([`28e79c7`](https://github.com/hydro-project/infinity/commit/28e79c78ff3289403bb2b7c324a4697f091a88f5))
    - Redesign spinner states ([`7a8bd6a`](https://github.com/hydro-project/infinity/commit/7a8bd6ace0e87ccfc50280e5f7debcffd4fca82d))
    - Implement background compaction using threads ([`6e7e28b`](https://github.com/hydro-project/infinity/commit/6e7e28baff2ea33b6b12f52db370170c51128281))
    - Add squash_sandbox tool and base_thread_id to clone_repo ([`5ffac5f`](https://github.com/hydro-project/infinity/commit/5ffac5f4d28c8efe8dfb861d883921292cf31423))
    - Correctly handle git initialization ([`8232243`](https://github.com/hydro-project/infinity/commit/82322433ef4802e05688258992b5a2dcb01b8b93))
    - Correctly handle cancellation using process groups ([`c7f9589`](https://github.com/hydro-project/infinity/commit/c7f9589773ff4c02d0efbf851d1b095f147453c2))
    - Fix escaping of grep tool arguments and detect cd to original folder ([`8b32a63`](https://github.com/hydro-project/infinity/commit/8b32a63ddb205fb1cf7e41284e3bf6a9edd131f9))
    - Add tool call and subscription cancellation protocol for resource cleanup ([`56cfa15`](https://github.com/hydro-project/infinity/commit/56cfa15af99cfc07db6b0bfbe09327fccd72eadb))
    - Add create_file tool to the sandbox editing tools ([`7327934`](https://github.com/hydro-project/infinity/commit/7327934698c59a708967438334319b684d01e0aa))
    - Update describe_edits tool description to request git-style messages ([`afe2f49`](https://github.com/hydro-project/infinity/commit/afe2f49ce627afdcb97683c2b1b5aa5c4b33318d))
    - Format and update snapshots ([`772a00c`](https://github.com/hydro-project/infinity/commit/772a00c383299383409c6ff8c834d344bdec4d11))
    - Implement output streaming for execute_command using RAP subscriptions with debouncing. ([`b2fb764`](https://github.com/hydro-project/infinity/commit/b2fb7643665e2052419103a4c7d4466758b0e026))
    - Two changes to improve tool call/result display: ([`5ad3eb5`](https://github.com/hydro-project/infinity/commit/5ad3eb565a43ba76b8b61b2ac4f19449cd2d2d35))
    - Return error when ripgrep is not installed ([`0951c1d`](https://github.com/hydro-project/infinity/commit/0951c1d559b5cdd7f6605eefae065c00d4165df7))
    - Add RAP protocol for notifying tool servers of thread closure ([`2d60e9d`](https://github.com/hydro-project/infinity/commit/2d60e9d12b84d01984b17e56c859caac8757859d))
    - Add describe_edits tool for setting commit messages ([`55c3e53`](https://github.com/hydro-project/infinity/commit/55c3e53789804d4c0426805de4422aa4cb4ed5fd))
    - Improve jj config management and shift spinner to top ([`40a3ac5`](https://github.com/hydro-project/infinity/commit/40a3ac57cf3b1a336a413862aa4c6b29fa1dc935))
    - Synchronize base repo workspace status when the user has loaded a sandbox commit ([`061be00`](https://github.com/hydro-project/infinity/commit/061be0029159c2ebc4cdd67dd1388a6c092517b2))
    - Add help command and handle restoring a sandbox ([`b6f0bce`](https://github.com/hydro-project/infinity/commit/b6f0bce789e72d0953ce37ddd6018d14cb6a0439))
    - Rich diff printing ([`72441f1`](https://github.com/hydro-project/infinity/commit/72441f15385b2aa4d54c04bfbd981dee0220674f))
    - Preliminary support for resizable TUI ([`4e30228`](https://github.com/hydro-project/infinity/commit/4e30228cbe42dbcf494f77d0a063360f2bc3d71c))
    - Add display_as to RAP tool results ([`18b60a5`](https://github.com/hydro-project/infinity/commit/18b60a5aa8a463d70eec75aca3e9a6e77722a972))
    - Code editing tools ([`36c7466`](https://github.com/hydro-project/infinity/commit/36c7466a0707836590fb385d313a2f929c3465e1))
    - Run clippy ([`ea864bf`](https://github.com/hydro-project/infinity/commit/ea864bf5a21cb030738936df2749af7ad0c255d8))
    - Clean up dependencies ([`fcef65d`](https://github.com/hydro-project/infinity/commit/fcef65df6274e43596bf84f9b2eaf4d8955e9b93))
    - Cache jj workspaces for local sandboxes ([`ba0ba4e`](https://github.com/hydro-project/infinity/commit/ba0ba4e372432f9d1044f0e57e06b9ada870de30))
    - Use jj workspaces ([`8d6bc44`](https://github.com/hydro-project/infinity/commit/8d6bc4477531f5a181ae5276a581978b4b2a225a))
    - Initial functional Jujutsu filesystem sandbox ([`4118c89`](https://github.com/hydro-project/infinity/commit/4118c890809b1f93e0ca92a6861ab9351e6e8864))
</details>

