# Releasing Guide

This is a guide on how to create releases for the published Infinity crates in this workspace.

We use the [`cargo-smart-release` crate](https://github.com/Byron/cargo-smart-release) for our
release workflow, following the same setup as [the Hydro repo](https://github.com/hydro-project/hydro/blob/main/RELEASING.md).
We have our own [GitHub Action release workflow](https://github.com/hydro-project/infinity/actions/workflows/release.yml)
([action YAML here](.github/workflows/release.yml)) which is our intended way to create releases.

Calling `cargo smart-release` is supposed to _just work_, but it has a few rough edges that can
prevent the release workflow from completing successfully. Mainly, it is supposed to generate
changelogs automatically from our [conventional commit](https://www.conventionalcommits.org/)
messages (see [AGENTS.md](AGENTS.md)), but sometimes requires manual intervention in some
situations.

## Which crates are published?

All workspace crates are published in lockstep, **except** for the following, which are marked
`publish = false` in their `Cargo.toml`:

- `rap-test-servers` — in-process stub RAP servers, test support only. (Referenced by other
  crates only as a path-only dev-dependency, which cargo strips when publishing.)
- `infinity-agent-lambda` — AWS Lambda deployment artifact.
- `infinity-slack-bot` — deployment artifact for the Hydro project's own Slack.

The published crates are all listed explicitly (every one is a primary release target, no
secondaries) in the `Determine crates to publish` step of
[`release.yml`](.github/workflows/release.yml).

## Optional: Installing and running `cargo-smart-release` locally

```sh
cargo install cargo-smart-release --git https://github.com/hydro-project/cargo-smart-release.git --rev e6f3368337a0
```
Re-run this command before each release to update the tool before testing locally, as the CI will
always use the pinned version above (keep them in sync with `release.yml`).

To (dry) run the command locally to spot-check for errors and warnings:
```bash
cargo smart-release --update-crates-index \
   --no-changelog-preview --allow-fully-generated-changelogs \
   --bump-dependencies auto --bump minor \
   rap-protocol rap-client rap-steering-server rap-github-event-poller \
   infinity-protocol infinity-provider-protocol \
   infinity-provider-bedrock infinity-provider-rig \
   infinity-agent-core infinity-mcp-bridge infinity-rap-bridge \
   infinity-daemon infinity-agent-cli \
   sandbox-core sandbox-local sandbox-remote
```
Make sure to set `--bump` to the right value, others are `patch`, `major`, `keep`, `auto`. Also
make sure the listed crates are up-to-date and match those in `release.yml`.

## Dry run to ensure changelogs can be generated

`cargo smart-release` tries to generate changelogs from commit messages. However if a particular
package has changes but doesn't have the right commit messages then `cargo smart-release` will
complain and give up.

To see if anything needs addressing, go to the [Release action](https://github.com/hydro-project/infinity/actions/workflows/release.yml)
and click on the "Run workflow" button in the top right corner. Branch should be `main`, version
bump should most likely be `patch`, `minor`, or `major`. Note that semantic versioning is:
```js
    {major}.{minor}.{patch}
```
(Sometimes you might use the `keep` version bump if you have manually changed all the packages'
`Cargo.toml` versions and committed that.)

Make sure to leave "Actually execute and publish the release?" **UNCHECKED** for a dry test run. If
all goes well the action job should complete successfully (with a green check), and the log under
"Release Job" > "Run cargo smart-release" should show that all the changelogs can be modified, with
lines like:

```log
[INFO ] WOULD modify existing changelog for 'rap-protocol'.
[INFO ] WOULD modify existing changelog for 'infinity-agent-core'.
```

Make sure the version bumps look correct too.

### Check log for this!

If the job does not succeed or succeeds but fails to generate changelogs for certain packages, then
you will need to do a bit of manual work. That looks like this in the log (check for this!):
```log
[WARN ] WOULD ask for review after commit as the changelog entry is empty for crates: infinity-mcp-bridge, rap-client
```
In this case, you will need to create a commit to each package's `CHANGELOG.md` to mark it as
unchanged (or minimally changed). For example, [hydro_cli 0.3](https://github.com/hydro-project/hydro/commit/4c2cf81411835529b5d7daa35717834e46e28b9b).

Once all changelogs are ok to autogenerate, we can move on to the real-deal run.

## Real-deal run

Again, go to the [Release action](https://github.com/hydro-project/infinity/actions/workflows/release.yml)
and click on the "Run workflow" button in the top right corner. Select branch `main`, version bump
as needed and this time _check_ the "Actually execute and publish the release?" box.

Hopefully all goes well and the release will appear on the other end.

If the release fails it may leave the repo in a bit of a half-broken or half-released state. Some
or all of the release version tags may be pushed. You may need to manually create some
[GitHub releases](https://github.com/hydro-project/infinity/releases).
You can also try re-running the release action but with the version bump set to `keep`, if versions
have been bumped but not released. You'll have to figure it out, its finicky.

**DO NOT MAKE CHANGES TO `main` WHEN THE RELEASE WORKFLOW IS RUNNING!**

If you make changes to main, then the release workflow may fail at the very end when it tries to
push its generated commits to `main`. The job should've pushed some commit with a bunch of version
tags and you (probably) need to hard-reset main to point to that tagged commit instead of whatever
junk you mistakenly pushed.

## Addendum: Adding new crates

When adding a new crate which is published, you need to:
1. Ensure `publish = true` and other required fields (`license`, `description`, `documentation`,
   `repository`, etc.), are set in `crates/my_crate/Cargo.toml`
   https://doc.rust-lang.org/cargo/reference/publishing.html#before-publishing-a-new-crate
2. Ensure any `path` dependencies to/from `my_crate` also include `version = "^0.1.0"`
   (substitute correct version).
3. You must commit a new (empty) file `crates/my_crate/CHANGELOG.md` to ensure the file will be
   tracked by git and pushed by `cargo-smart-release`.
4. (A) If you want your crate to be lockstep-versioned alongside the other Infinity crates then
   make sure to add it to the crate list in the [`release.yml` workflow](.github/workflows/release.yml)
   (also update the `cargo smart-release` test command above in this file).
5. (B) Otherwise, if your crate is only used via `[dev-dependencies]` then the crate may not
   initially publish due to https://github.com/Byron/cargo-smart-release/issues/36. To workaround
   this, additionally add the crate as a regular `[dependencies]` but marked as `optional = true`.
   When doing the dry run, look for `[INFO ] WOULD modify existing changelog for 'new_crate'.` to
   verify it will publish. (Alternatively, mark it `publish = false` and reference it via
   path-only dev-dependencies, like `rap-test-servers`.)

Then just run the release workflow as normal.

## Addendum: Moving crates

`cargo-smart-release` automatically generates changelogs. However it only looks for changes in the
package's _current_ directory, so if you move a package to a different directory then the changelog
may lose old commit info if you're not careful.

On the commit immediately _before_ you move the package(s) and run the following:
```
cargo changelog --write <crate_to_be_moved> <other_crate_to_be_moved> ...
```
(This command is provided by `cargo install cargo-smart-release`; don't use any other `cargo changelog` command)

Next (even if there are no changes), go through the modified `CHANGELOG.md` files and add a prefix
to **all** (not just the new) the `Commit Statistics` and `Commit Details` headers, for example:
`Pre-Move Commit Statistics`/`Pre-Move Commit Details`.
This is necessary because otherwise `cargo-smart-release` will treat those sections as auto-generated
and will not preserve them, but then won't regenerate them due to the package moving. Commit the
updated changelogs and cherry-pick that commit to the latest version if you went back in history.
The changelogs should now be safely preserved by future releases.

## Addendum: Renaming crates

First, follow the [steps above for moving crates](#addendum-moving-crates).

After renaming a crate, `cargo-smart-release` will see it as a brand new crate with no published
versions on crates.io, and will therefore not bump the version. This is not desired behavior, and
generating the changelog will fail unintelligibly due to the conflicting versions:
```log
BUG: User segments are never auto-generated: ...
```

To fix this, before releasing, manually bump the version of the renamed crate. `Cargo.toml`:
```toml
name = "crate_old_name"
publish = true
version = "0.8.0"
# becomes
name = "crate_new_name"
publish = true
version = "0.9.0"
```
(In this case, bumping the minor version)

You will also need to manually update any crates that depend on the renamed crate as well:
```toml
crate_old_name = { path = "../crate_old_path", version = "^0.8.0" }
# becomes
crate_new_name = { path = "../crate_new_path", version = "^0.9.0" }
```

Commit those changes, then continue as normal. E.g. for a minor version bump, update only the
renamed crates' versions, then continue with a `minor` release which will bump all the other crates.

(There may be other issues with the `git tag`s `cargo-smart-release` uses to track versions if you
are renaming a crate _back to an old name_).

## Addendum: `[build-dependencies]`

Due to bug [cargo-smart-release#16](https://github.com/Byron/cargo-smart-release/issues/16), `cargo-smart-release`
does not properly handle `[build-dependencies]` in `Cargo.toml`. If one workspace crate has another
workspace crate as a build dependency `cargo-smart-release` will fail to find this dependency and
may fail due to versioning issues. The workspace dependency should also be added to the `[dependencies]`
section in order to work around this issue.

## Addendum: The GitHub App account

`cargo smart-release` wants to push to `hydro-project/infinity`'s `main` branch, but branch
protection says you can only push to main via a pull request, and that branch protection also
applies to GitHub Actions.

To get around this problem we use the same [GitHub App account called Hydro Project Bot](https://github.com/organizations/hydro-project/settings/apps/hydro-project-bot)
as the Hydro repo. It is a pretty unremarkable unpublished GitHub App with permissions to modify
repos. We act as the app within GitHub Actions via the `secrets.APP_ID` and
`secrets.APP_PRIVATE_KEY` repository secrets. Importantly, the Hydro Project Bot must be given
permission to bypass `main` branch protection rules on this repo, under "Allow specified actors to
bypass required pull requests".

The workflow also needs the `secrets.CARGO_REGISTRY_TOKEN` repository secret set to a crates.io
API token that is allowed to publish all the Infinity crates.
