

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

 - <csr-id-ba10ffd62644a4c86c31a7fb6d5eaaca8c403b55/> add remote host migration UI and daemon orchestration

### Bug Fixes

 - <csr-id-a0b74f2b3a8b732e6731e1e94d2bff57d0ce422f/> prevent git from resolving outer repo in sandboxes
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

 - <csr-id-7634b823ad70378e666379a9a8e8a7935a06026f/> replace all .unwrap() with .expect() and fix clippy warnings
 - <csr-id-9757071818663cefb8e6a12438071d95000379a8/> add precheck script, lints

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

### Commit Statistics

<csr-read-only-do-not-edit/>

 - 32 commits contributed to the release.
 - 11 commits were understood as [conventional](https://www.conventionalcommits.org).
 - 5 unique issues were worked on: [#107](https://github.com/hydro-project/infinity/issues/107), [#113](https://github.com/hydro-project/infinity/issues/113), [#78](https://github.com/hydro-project/infinity/issues/78), [#8](https://github.com/hydro-project/infinity/issues/8), [#96](https://github.com/hydro-project/infinity/issues/96)

### Commit Details

<csr-read-only-do-not-edit/>

<details><summary>view details</summary>

 * **[#107](https://github.com/hydro-project/infinity/issues/107)**
    - Set up cargo-smart-release release workflow (mirroring hydro) ([`ffc27d0`](https://github.com/hydro-project/infinity/commit/ffc27d0bf5d964a655fedab9460bf5017971e6b6))
 * **[#113](https://github.com/hydro-project/infinity/issues/113)**
    - Introduce typed ThreadId for RAP group ids ([`4b18b37`](https://github.com/hydro-project/infinity/commit/4b18b37de219cb7fe27ce7c027b87f4fb35fbbf5))
 * **[#78](https://github.com/hydro-project/infinity/issues/78)**
    - Extensible sandbox modes via `ModeProvider`, with jj and git as built-in providers ([`c431616`](https://github.com/hydro-project/infinity/commit/c43161629513d2f163ca7ab44c0a1093386118bb))
 * **[#8](https://github.com/hydro-project/infinity/issues/8)**
    - Add automated THIRD-PARTY file generation with license enforcement ([`e2e0719`](https://github.com/hydro-project/infinity/commit/e2e0719faebbffc72ec7bd8a8b3b02223da8ba0e))
 * **[#96](https://github.com/hydro-project/infinity/issues/96)**
    - Extract shared agent system engine ([`9c921fd`](https://github.com/hydro-project/infinity/commit/9c921fde280b50c89c3e5b9caadccf83a46078a4))
 * **Uncategorized**
    - Release infinity-agent-core v0.1.0, infinity-mcp-bridge v0.1.0, infinity-rap-bridge v0.1.0, infinity-daemon v0.1.0, infinity-agent-cli v0.1.0, sandbox-core v0.1.0, sandbox-local v0.1.0, sandbox-remote v0.1.0 ([`7599fbb`](https://github.com/hydro-project/infinity/commit/7599fbbdfad042a6fd85c23002bf937fecbe7b45))
    - Release infinity-provider-protocol v0.1.0, infinity-provider-bedrock v0.1.0, infinity-provider-rig v0.1.0, infinity-agent-core v0.1.0, infinity-mcp-bridge v0.1.0, infinity-rap-bridge v0.1.0, infinity-daemon v0.1.0, infinity-agent-cli v0.1.0, sandbox-core v0.1.0, sandbox-local v0.1.0, sandbox-remote v0.1.0 ([`7e1cd1d`](https://github.com/hydro-project/infinity/commit/7e1cd1df69d8fce402bef4085e9d17f871994503))
    - Release rap-protocol v0.1.0, rap-client v0.1.0, rap-steering-server v0.1.0, rap-github-event-poller v0.1.0, infinity-protocol v0.1.0, infinity-provider-protocol v0.1.0, infinity-provider-bedrock v0.1.0, infinity-provider-rig v0.1.0, infinity-agent-core v0.1.0, infinity-mcp-bridge v0.1.0, infinity-rap-bridge v0.1.0, infinity-daemon v0.1.0, infinity-agent-cli v0.1.0, sandbox-core v0.1.0, sandbox-local v0.1.0, sandbox-remote v0.1.0 ([`dd8c7f4`](https://github.com/hydro-project/infinity/commit/dd8c7f49028a26052d785b4241f9ade125f0afb3))
    - Remove unused `_keepalive` field from SpawnedCommand ([`24820fe`](https://github.com/hydro-project/infinity/commit/24820fe9b2be138774a8a4e069019b9b11444a0d))
    - Prevent git from resolving outer repo in sandboxes ([`a0b74f2`](https://github.com/hydro-project/infinity/commit/a0b74f2b3a8b732e6731e1e94d2bff57d0ce422f))
    - Add workspace lints and fix all lint violations ([`b92b7a1`](https://github.com/hydro-project/infinity/commit/b92b7a17f4b69e2652f5cce813320eca851717e4))
    - Delete child metadata on squash to prevent migration failure ([`2759040`](https://github.com/hydro-project/infinity/commit/2759040634532e82b9fe9dc53fc646a78220bb42))
    - Add remote host migration UI and daemon orchestration ([`ba10ffd`](https://github.com/hydro-project/infinity/commit/ba10ffd62644a4c86c31a7fb6d5eaaca8c403b55))
    - Replace all .unwrap() with .expect() and fix clippy warnings ([`7634b82`](https://github.com/hydro-project/infinity/commit/7634b823ad70378e666379a9a8e8a7935a06026f))
    - Add precheck script, lints ([`9757071`](https://github.com/hydro-project/infinity/commit/9757071818663cefb8e6a12438071d95000379a8))
    - Add write:/path permissions, thread_ancestors protocol field, and ancestor-aware grant system ([`3464fad`](https://github.com/hydro-project/infinity/commit/3464fade510fd5ab7aa2dc2ffa27f61711c6be31))
    - Add Direct sandbox mode and better error for empty repos ([`12b7454`](https://github.com/hydro-project/infinity/commit/12b7454c172b2bb455c96e6c25c2096e3348bc49))
    - Add support for UserChoice prompts in RAP protocol and use for permissions expansion in sandbox ([`b0db6a7`](https://github.com/hydro-project/infinity/commit/b0db6a7a0764ddab7df1f5cf3fcefc7129c6ddcb))
    - Shift core agent runtime into a daemon with a network protocol for clients ([`141d697`](https://github.com/hydro-project/infinity/commit/141d69792c3aa951fcbfbea847879582f1d06ec3))
    - Add steering file instructions to the default agent prompt ([`40b1f78`](https://github.com/hydro-project/infinity/commit/40b1f78d18466b99040caecc772adfaa7c6ed705))
    - Add SandboxMode enum with Jj/Git variants; thread description through push_sandbox ([`bc26456`](https://github.com/hydro-project/infinity/commit/bc26456ef75bafcf57ee2b7f568f0a04330d294d))
    - Refactor JJ sandbox lifecycle: absolute revisions, cleanup on shutdown ([`f525abf`](https://github.com/hydro-project/infinity/commit/f525abfe4f5fcd28b5e62c8678df542b5924a308))
    - Add squash_sandbox tool and base_thread_id to clone_repo ([`5ffac5f`](https://github.com/hydro-project/infinity/commit/5ffac5f4d28c8efe8dfb861d883921292cf31423))
    - Fix escaping of grep tool arguments and detect cd to original folder ([`8b32a63`](https://github.com/hydro-project/infinity/commit/8b32a63ddb205fb1cf7e41284e3bf6a9edd131f9))
    - Implement output streaming for execute_command using RAP subscriptions with debouncing. ([`b2fb764`](https://github.com/hydro-project/infinity/commit/b2fb7643665e2052419103a4c7d4466758b0e026))
    - Persist display_as mapping in store.json and use it during history replay, document ([`1ca626a`](https://github.com/hydro-project/infinity/commit/1ca626a6d1fd200b1267c548079878192811d096))
    - Improve jj config management and shift spinner to top ([`40a3ac5`](https://github.com/hydro-project/infinity/commit/40a3ac57cf3b1a336a413862aa4c6b29fa1dc935))
    - Run clippy ([`ea864bf`](https://github.com/hydro-project/infinity/commit/ea864bf5a21cb030738936df2749af7ad0c255d8))
    - Clean up dependencies ([`fcef65d`](https://github.com/hydro-project/infinity/commit/fcef65df6274e43596bf84f9b2eaf4d8955e9b93))
    - Cache jj workspaces for local sandboxes ([`ba0ba4e`](https://github.com/hydro-project/infinity/commit/ba0ba4e372432f9d1044f0e57e06b9ada870de30))
    - Use jj workspaces ([`8d6bc44`](https://github.com/hydro-project/infinity/commit/8d6bc4477531f5a181ae5276a581978b4b2a225a))
    - Initial functional Jujutsu filesystem sandbox ([`4118c89`](https://github.com/hydro-project/infinity/commit/4118c890809b1f93e0ca92a6861ab9351e6e8864))
</details>

