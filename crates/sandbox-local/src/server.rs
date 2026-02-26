use sandbox_core::callback::PlainCallbackClient;
use sandbox_core::metadata::MetadataStore;
use sandbox_core::sandbox::SandboxBackend;
use sandbox_core::server::build_router;

/// Run the sandbox RAP server locally with axum::serve.
pub async fn run_server<B: SandboxBackend + 'static, M: MetadataStore + 'static>(
    backend: B,
    metadata: M,
    port: u16,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let (app, _tracker) = build_router(backend, metadata, PlainCallbackClient::new());

    let listener = tokio::net::TcpListener::bind(format!("0.0.0.0:{port}")).await?;
    tracing::info!("sandbox RAP server listening on port {port}");
    axum::serve(listener, app).await?;
    Ok(())
}
