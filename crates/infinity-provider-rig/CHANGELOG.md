

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

### Refactor (BREAKING)

 - <csr-id-49ad32e467d92f82cdac76095b6cb0a3daf2f964/> rig-free provider stack, native Bedrock, minimal deps; refreshed scale claims
   Remove `rig` from the core provider/agent stack. `infinity-provider-protocol` now owns a
   minimal model API; the Bedrock provider talks to the official AWS SDK directly
   (eliminating the maintained `rig-bedrock` patch); rig survives only as an optional
   bridge crate. Re-measured the memory benchmark and refreshed the landing/README claims.

### Commit Statistics

<csr-read-only-do-not-edit/>

 - 2 commits contributed to the release.
 - 2 commits were understood as [conventional](https://www.conventionalcommits.org).
 - 2 unique issues were worked on: [#107](https://github.com/hydro-project/infinity/issues/107), [#110](https://github.com/hydro-project/infinity/issues/110)

### Commit Details

<csr-read-only-do-not-edit/>

<details><summary>view details</summary>

 * **[#107](https://github.com/hydro-project/infinity/issues/107)**
    - Set up cargo-smart-release release workflow (mirroring hydro) ([`ffc27d0`](https://github.com/hydro-project/infinity/commit/ffc27d0bf5d964a655fedab9460bf5017971e6b6))
 * **[#110](https://github.com/hydro-project/infinity/issues/110)**
    - Rig-free provider stack, native Bedrock, minimal deps; refreshed scale claims ([`49ad32e`](https://github.com/hydro-project/infinity/commit/49ad32e467d92f82cdac76095b6cb0a3daf2f964))
</details>

