

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

 - 12 commits contributed to the release over the course of 164 calendar days.
 - 8 commits were understood as [conventional](https://www.conventionalcommits.org).
 - 3 unique issues were worked on: [#107](https://github.com/hydro-project/infinity/issues/107), [#113](https://github.com/hydro-project/infinity/issues/113), [#8](https://github.com/hydro-project/infinity/issues/8)

### Commit Details

<csr-read-only-do-not-edit/>

<details><summary>view details</summary>

 * **[#107](https://github.com/hydro-project/infinity/issues/107)**
    - Set up cargo-smart-release release workflow (mirroring hydro) ([`ffc27d0`](https://github.com/hydro-project/infinity/commit/ffc27d0bf5d964a655fedab9460bf5017971e6b6))
 * **[#113](https://github.com/hydro-project/infinity/issues/113)**
    - Introduce typed ThreadId for RAP group ids ([`4b18b37`](https://github.com/hydro-project/infinity/commit/4b18b37de219cb7fe27ce7c027b87f4fb35fbbf5))
 * **[#8](https://github.com/hydro-project/infinity/issues/8)**
    - Add automated THIRD-PARTY file generation with license enforcement ([`e2e0719`](https://github.com/hydro-project/infinity/commit/e2e0719faebbffc72ec7bd8a8b3b02223da8ba0e))
 * **Uncategorized**
    - Add workspace lints and fix all lint violations ([`b92b7a1`](https://github.com/hydro-project/infinity/commit/b92b7a17f4b69e2652f5cce813320eca851717e4))
    - Add remote host migration UI and daemon orchestration ([`ba10ffd`](https://github.com/hydro-project/infinity/commit/ba10ffd62644a4c86c31a7fb6d5eaaca8c403b55))
    - Log panics from fire-and-forget spawned tasks instead of silently swallowing them ([`44fcca2`](https://github.com/hydro-project/infinity/commit/44fcca250a44029e36b49df6013a049d33bc985f))
    - Replace all .unwrap() with .expect() and fix clippy warnings ([`7634b82`](https://github.com/hydro-project/infinity/commit/7634b823ad70378e666379a9a8e8a7935a06026f))
    - Add precheck script, lints ([`9757071`](https://github.com/hydro-project/infinity/commit/9757071818663cefb8e6a12438071d95000379a8))
    - Add write:/path permissions, thread_ancestors protocol field, and ancestor-aware grant system ([`3464fad`](https://github.com/hydro-project/infinity/commit/3464fade510fd5ab7aa2dc2ffa27f61711c6be31))
    - Unify all duplicate RAP protocol types into rap-protocol crate ([`2def5ee`](https://github.com/hydro-project/infinity/commit/2def5eec01a5c197432a7959942cca8b0eb9d6a0))
    - Add rig-mock crate and test suite for agent core and daemon ([`abda067`](https://github.com/hydro-project/infinity/commit/abda06757eeba0ac7817374bc89155211cd2edcd))
    - Add rap-github-event-poller crate for local GitHub event polling ([`783a9ec`](https://github.com/hydro-project/infinity/commit/783a9ec48c0f8f97522c34f62460a48911ac9875))
</details>

