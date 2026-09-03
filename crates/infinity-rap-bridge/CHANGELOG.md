

## v0.1.0 (2026-09-03)

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

### New Features (BREAKING)

 - <csr-id-8bef2c534f90b7fe038cb6dda1fb2015fa9e737d/> add high-level agent system API
   Add ergonomic local agent-system APIs on top of the engine extracted in #96:
   
   - static builder conveniences for tools, prompts, and RAP notification;
   - channel-backed `ThreadHandle`s for sending inputs and streaming events;
   - launcher mode and `ThreadBuilder` for per-thread tools, prompts, and models;
   - root-based configuration inheritance for child threads;
   - direct local `McpToolSet` and `RapToolSet` adapters;
   - usage-oriented high-level and low-level documentation.

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

### Commit Statistics

<csr-read-only-do-not-edit/>

 - 5 commits contributed to the release.
 - 5 commits were understood as [conventional](https://www.conventionalcommits.org).
 - 5 unique issues were worked on: [#107](https://github.com/hydro-project/infinity/issues/107), [#110](https://github.com/hydro-project/infinity/issues/110), [#113](https://github.com/hydro-project/infinity/issues/113), [#92](https://github.com/hydro-project/infinity/issues/92), [#96](https://github.com/hydro-project/infinity/issues/96)

### Commit Details

<csr-read-only-do-not-edit/>

<details><summary>view details</summary>

 * **[#107](https://github.com/hydro-project/infinity/issues/107)**
    - Set up cargo-smart-release release workflow (mirroring hydro) ([`ffc27d0`](https://github.com/hydro-project/infinity/commit/ffc27d0bf5d964a655fedab9460bf5017971e6b6))
 * **[#110](https://github.com/hydro-project/infinity/issues/110)**
    - Rig-free provider stack, native Bedrock, minimal deps; refreshed scale claims ([`49ad32e`](https://github.com/hydro-project/infinity/commit/49ad32e467d92f82cdac76095b6cb0a3daf2f964))
 * **[#113](https://github.com/hydro-project/infinity/issues/113)**
    - Introduce typed ThreadId for RAP group ids ([`4b18b37`](https://github.com/hydro-project/infinity/commit/4b18b37de219cb7fe27ce7c027b87f4fb35fbbf5))
 * **[#92](https://github.com/hydro-project/infinity/issues/92)**
    - Add high-level agent system API ([`8bef2c5`](https://github.com/hydro-project/infinity/commit/8bef2c534f90b7fe038cb6dda1fb2015fa9e737d))
 * **[#96](https://github.com/hydro-project/infinity/issues/96)**
    - Extract shared agent system engine ([`9c921fd`](https://github.com/hydro-project/infinity/commit/9c921fde280b50c89c3e5b9caadccf83a46078a4))
</details>

