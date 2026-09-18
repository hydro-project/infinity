---
sidebar_position: 4
title: Model Providers
---

# Model Providers

The Infinity Runtime never calls an LLM API directly. Instead, all inference goes through the `ModelProvider` trait, which decouples the agent loop from any particular model backend. A provider lists the models it offers (each with a display name, context window, and output token limit), and it invokes a model by id, streaming the completion response back.

Everything else in the runtime, such as the agent loop, threading, compaction, and token accounting, is written against this trait. The Lambda deployment plugs in the Bedrock provider directly. The Infinity Code daemon registers each provider under a stable **provider id** and references models globally as `provider id + model id`, so multiple providers can coexist and can even offer models with the same name.

Providers own all backend-specific behavior. Callers hand them a plain `CompletionRequest` (defined by the protocol crate), and the provider is responsible for backend-specific request parameters such as thinking configuration, beta feature flags, and per-model output token limits. For example, the Bedrock provider injects Anthropic's adaptive thinking configuration and the 1M-context beta flag for the models that need them, without the agent loop knowing that those exist.

## The `ModelProvider` trait

The trait lives in `infinity-provider-protocol`, a deliberately lightweight crate so provider implementations can depend on it without pulling in the rest of the runtime:

```rust
#[async_trait]
pub trait ModelProvider: Send + Sync {
    /// List the models available from this provider. The first entry is the
    /// provider's default model.
    async fn list_models(&self) -> Result<Vec<ModelEntry>, BoxError>;

    /// Invoke a model by its provider-scoped id, streaming the completion
    /// response.
    async fn invoke_model(
        &self,
        model_id: &str,
        request: CompletionRequest,
    ) -> Result<ModelStream, CompletionError>;
}
```

A `ModelEntry` describes one model:

```rust
pub struct ModelEntry {
    /// Provider-scoped identifier. Unique within the provider, but need not
    /// match the upstream API's model id.
    pub model_id: String,
    /// Human-readable name shown in pickers.
    pub display_name: String,
    /// Context window size in tokens (used for compaction thresholds).
    pub context_window: usize,
    /// Max output tokens per request (None = backend default).
    pub max_output_tokens: Option<u64>,
    /// Whether the model accepts image content in its input (e.g. image
    /// tool results). Defaults to false; the runtime replaces image content
    /// with a text placeholder for models that don't support it.
    pub supports_image_input: bool,
}
```

Because `model_id` is provider-scoped rather than the upstream id, a provider can expose **multiple configurations of the same upstream model** as separate entries. For example, the Bedrock provider offers `claude-opus-4-6` both as a standard 200K-context model and as a 1M-context variant that enables a beta flag on every request.

## Writing a provider

A provider implements the two trait methods against its backend's API:

1. Implement `list_models` to return your catalog (often a static list).
2. Implement `invoke_model`: resolve the `model_id` to your backend's model, apply any backend-specific request parameters, call the backend, and adapt its streaming response into a `ModelStream`, which is a pinned stream of `StreamChunk` items (text, tool calls and tool-call deltas, reasoning, and a `Final` chunk carrying the completion's token usage).

For single-model setups and tests, there is a ready-made adapter: you can implement the one-method `CompletionModel` trait and wrap it with `SingleModelProvider::new(entry, model)`, which advertises the given `ModelEntry` and forwards every invocation to that model.

The protocol crate itself has no dependency on any LLM SDK. To serve one of [rig](https://docs.rs/rig-core)'s backends (OpenAI, Anthropic, Gemini, ...), you can use the optional `infinity-provider-rig` bridge crate: `RigCompletionModel::new(rig_model).into_provider(entry)` produces a `ModelProvider`. The bridge's `convert` module also exposes the raw request/stream conversions, for providers that need to inject per-model request parameters themselves:

```rust
use infinity_provider_rig::RigCompletionModel;
use infinity_provider_protocol::ModelEntry;
use rig::client::{CompletionClient, ProviderClient};

let client = rig::providers::anthropic::Client::from_env();
let model = client.completion_model("claude-sonnet-4-5");
let provider = RigCompletionModel::new(model).into_provider(ModelEntry {
    model_id: "claude-sonnet-4-5".to_owned(),
    display_name: "Claude Sonnet 4.5".to_owned(),
    context_window: 200_000,
    max_output_tokens: Some(64_000),
    supports_image_input: true,
});
```

The trait is dyn-compatible: the backend-specific streaming response type is erased behind `ModelStream`, and the final `StreamChunk::Final` is reduced to a `FinalResponse` carrying the token usage, which is all that downstream code needs.

## The provider process transport

The Infinity Code daemon runs each provider as a separate process, configured in `~/.infinity/providers.json` (see [Model Providers in the Infinity Code docs](/docs/infinity-code/model-providers) for installing and configuring them). The daemon aggregates the providers' models into one catalog, with the first model of the first configured provider as the default. These provider processes are served over a **Unix domain socket**. You will rarely need the details, since `infinity_provider_protocol::remote` provides both sides of the transport, but they matter when packaging a provider as an installable crate.

A provider binary does three things:

```rust
use std::sync::Arc;
use infinity_provider_protocol::remote::serve_provider;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let provider = Arc::new(MyProvider::new());
    let (socket_path, server) = serve_provider(provider)?;

    // stdout contract: the first line is the socket path.
    println!("{}", socket_path.display());

    server.await;
    Ok(())
}
```

1. `serve_provider` binds a listener on a freshly generated temp socket path and serves the provider on it.
2. The binary prints the socket path as its **first stdout line**, which is how the supervising daemon discovers it. Anything else (logging, diagnostics) should go to stderr; the daemon captures both streams and will forward every later line to its own log.
3. It then awaits the server future forever. The daemon owns the process lifecycle: it spawns the binary at startup and kills it on shutdown.

On the wire, the protocol is newline-delimited JSON with one request per connection (concurrent invocations use concurrent connections). A `ListModels` request gets a single response, while `InvokeModel` streams the completion back as chunk lines terminated by a stream-end marker. The daemon side is `RemoteModelProvider`, which is itself a `ModelProvider` implementation that forwards every call over the socket, so in-process and out-of-process providers are indistinguishable to the runtime.

Once your provider crate is published (or available in a git repo), users can install it with:

```bash
infinity provider install my-provider --git https://github.com/you/my-provider --crate my-provider
```
