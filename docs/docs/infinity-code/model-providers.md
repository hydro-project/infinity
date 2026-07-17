---
sidebar_position: 4
title: Model Providers
---

# Model Providers

Infinity Code doesn't talk to any LLM API directly. All model inference goes through **model providers**: separate processes that the daemon launches and manages. Each provider exposes one or more models (for example, the Bedrock provider exposes the Claude models available on AWS Bedrock).

At least one provider must be installed before the agent can start.

## Installing a provider

Providers are installed with `infinity provider install`, which `cargo install`s the provider crate and registers it in `~/.infinity/providers.json`:

```bash
infinity provider install bedrock --git https://github.com/hydro-project/infinity --crate infinity-provider-bedrock
```

- The first argument (`bedrock`) is the **provider id**, a name you choose. Models are tracked as `provider id + model id`, so pick a stable name.
- `--crate` is the crate to install; its binary becomes the provider's command.
- `--git` / `--path` record where the crate comes from, exactly like `infinity rap install`, so the provider can be updated later.

### The Bedrock provider

`infinity-provider-bedrock` serves the Claude models available on AWS Bedrock. It uses your ambient AWS configuration through the standard SDK credential chain (`AWS_PROFILE`, environment variables, SSO, instance profiles), and requires Bedrock model access to be enabled on the account.

## Configuration

Providers live in `~/.infinity/providers.json`, a JSON object mapping provider id to its configuration:

```json
{
  "bedrock": {
    "command": ["infinity-provider-bedrock"],
    "crate_name": "infinity-provider-bedrock",
    "git": "https://github.com/hydro-project/infinity"
  },
  "my-provider": {
    "command": ["/usr/local/bin/my-provider", "--some-flag"]
  }
}
```

- **`command`**: the command (argv) the daemon runs to start the provider. Bare names are looked up on `PATH`; absolute paths are used as-is.
- **`crate_name`**, **`git`**, **`path`** *(optional)*: installation source, recorded by `infinity provider install` and used by `infinity provider update`. Hand-written entries without these still work; they just can't be auto-updated.

**Order matters**: providers are registered in the order they appear in the file, and the first model of the first provider is the default model for new sessions. You can edit the file by hand. The daemon reads it at startup, so run `infinity daemon restart` after changes.

## Switching models

The model picker (`/model` or `Ctrl+M` in the TUI) lists every model from every configured provider. Each thread remembers its own selection; if a selected model disappears from the configuration, the thread falls back to the default model with a warning.

## Updating

```bash
infinity provider update
```

re-installs every provider that has a recorded source (`crate_name` plus optional `git`/`path`). Providers are also updated as part of a full

```bash
infinity update
```

which updates the CLI binary, model providers, and RAP servers in one go.

## Troubleshooting

When the CLI launches the daemon in the background, it waits for the daemon to report readiness; if the daemon exits during startup instead, the CLI prints everything the daemon wrote to stdout/stderr, so configuration errors surface directly:

- **"no model providers configured"**: the daemon refuses to start without `~/.infinity/providers.json`. Install a provider (see above).
- **Provider fails to start**: if a configured provider can't be spawned (e.g. its binary was removed) or doesn't come up, the daemon exits with the provider's error. Details are also logged to `~/.infinity/daemon.log`.

## Building your own provider

Any process that implements the provider protocol can serve models. See [Model Providers in the Infinity Runtime docs](/docs/infinity-runtime/model-providers) for the `ModelProvider` trait and a guide to writing a provider crate.
