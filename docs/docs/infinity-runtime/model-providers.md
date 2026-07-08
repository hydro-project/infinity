---
sidebar_position: 4
title: Model Providers
---

# Model Providers

The Infinity Runtime never calls an LLM API directly. All inference goes through the `ModelProvider` trait, an abstraction that decouples the agent loop from any particular model backend. A provider:

- **lists the models it offers** — each with a display name, context window, and output token limit — and
- **invokes a model by id**, streaming the completion response back.

Everything else in the runtime (the agent loop, threading, compaction, token accounting) is written against this trait. The daemon registers each provider under a stable **provider id**, and models are referenced globally as `provider id + model id`, so multiple providers can coexist and even offer models with the same name.

Providers own all backend-specific behavior. Callers hand them a plain [rig](https://docs.rs/rig-core) `CompletionRequest`; the provider is responsible for backend-specific request parameters — thinking configuration, beta feature flags, per-model output token limits, and so on. For example, the Bedrock provider injects Anthropic's adaptive thinking configuration and the 1M-context beta flag for the models that need them, without the agent loop knowing those exist.

## The `ModelProvider` trait

Defined in the `infinity-provider-protocol` crate — a deliberately lightweight crate so provider implementations can depend on it without pulling in the rest of the runtime:

```rust
#[async_trait]
pub trait ModelProvider: Send + Sync {
    /// List the models available from this provider. The first entry is the
    /// provider's default model.
    async fn list_models(&self) -> Result<Vec<ModelEntry>, BoxError>;

    /// Invoke a model by its provider-scoped id, streaming the completion
    /// response. Behaves exactly like rig's `CompletionModel::stream`, with
    /// the streaming response type erased.
    async fn invoke_model(
        &self,
        model_id: &str,
        request: CompletionRequest,
    ) -> Result<ProviderCompletionResponse, CompletionError>;

    /// Whether the given model accepts image content in its input.
    /// The default implementation looks the model up in `list_models`.
    async fn supports_image_input(&self, model_id: &str) -> bool { /* default */ }
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

Because `model_id` is provider-scoped rather than the upstream id, a provider can expose **multiple configurations of the same upstream model** as separate entries — the Bedrock provider offers `claude-opus-4-6` both as a standard 200K-context model and as a 1M-context variant that enables a beta flag on every request.

## Writing a provider

A provider typically wraps a rig `CompletionModel` internally:

1. Implement `list_models` to return your catalog (often a static list).
2. Implement `invoke_model`: resolve the `model_id` to your backend's model, apply any backend-specific request parameters, call the underlying model's `stream`, and erase the response with the `erase_streaming_response` helper:

```rust
async fn invoke_model(
    &self,
    model_id: &str,
    mut request: CompletionRequest,
) -> Result<ProviderCompletionResponse, CompletionError> {
    // ...apply backend-specific parameters to `request`...
    let model = self.client.completion_model(model_id);
    Ok(erase_streaming_response(model.stream(request).await?))
}
```

For tests or single-model setups there's a ready-made adapter, `SingleModelProvider`, which exposes one rig `CompletionModel` as a provider.

The trait is dyn-compatible, which requires erasing the backend-specific streaming response type: streams yield the usual text / tool call / reasoning chunks, and the final response is reduced to a `ProviderStreamingResponse` carrying the token usage — all that downstream code needs.

## The provider process transport

The local daemon (Infinity Code) runs each provider as a separate process configured in `~/.infinity/providers.json` (see [Model Providers in the Infinity Code docs](/docs/infinity-code/model-providers) for installing and configuring them) and aggregates their models into one catalog, with the first model of the first configured provider as the default. These provider processes are served over a **Unix domain socket**. You rarely need the details — `infinity_provider_protocol::remote` provides both sides — but they matter when packaging a provider as an installable crate.

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
2. The binary prints the socket path as its **first stdout line** — that's how the supervising daemon discovers it. Anything else (logging, diagnostics) should go to stderr; the daemon captures both streams and forwards every later line to its own log.
3. It then awaits the server future forever. The daemon owns the process lifecycle: it spawns the binary at startup and kills it on shutdown.

On the wire, the protocol is newline-delimited JSON with one request per connection (concurrent invocations simply use concurrent connections): a `ListModels` request gets a single response, while `InvokeModel` streams the completion back as chunk lines terminated by a stream-end marker. The daemon side is `RemoteModelProvider` — itself a `ModelProvider` implementation that forwards every call over the socket, so in-process and out-of-process providers are indistinguishable to the runtime.

Once your provider crate is published (or available in a git repo), users install it with:

```bash
infinity provider install my-provider --git https://github.com/you/my-provider --crate my-provider
```
