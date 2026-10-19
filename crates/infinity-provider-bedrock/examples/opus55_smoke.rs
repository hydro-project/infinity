//! Minimal live smoke test: stream a few tokens from claude-opus-5.5 through
//! the provider's real `invoke_model` path (the same path the daemon uses).
//! Run manually: `cargo run -p infinity-provider-bedrock --example opus55_smoke`
use futures_util::StreamExt;
use infinity_provider_protocol::{CompletionRequest, Message, ModelProvider};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let provider = infinity_provider_bedrock::BedrockProvider::from_env();
    let models = provider.list_models().await?;
    let opus = models
        .iter()
        .find(|m| m.model_id.contains("opus-5-5"))
        .expect("opus-5-5 in catalog");
    println!(
        "catalog entry: {} (ctx {}, max_out {:?})",
        opus.model_id, opus.context_window, opus.max_output_tokens
    );
    let mut stream = provider
        .invoke_model(
            &opus.model_id,
            CompletionRequest {
                preamble: Some("Reply with exactly: OK".to_owned()),
                chat_history: vec![Message::user("go")],
                tools: vec![],
                max_tokens: Some(16),
                additional_params: None,
            },
        )
        .await?;
    while let Some(ev) = stream.next().await {
        println!("event: {ev:?}");
    }
    Ok(())
}
