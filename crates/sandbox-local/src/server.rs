use sandbox_core::callback::PlainCallbackClient;
use sandbox_core::metadata::MetadataStore;
use sandbox_core::sandbox::SandboxBackend;
use sandbox_core::server::build_router;

/// Run the sandbox RAP server locally with axum::serve.
/// Uses graceful shutdown on Ctrl+C so that the backend's `Drop`
/// implementation runs and cached sandboxes are cleaned up.
///
/// When the `RAP_EMBEDDED` environment variable is set, the server
/// binds to an OS-assigned port and emits a JSON line on stdout:
///   `{"port": <u16>}`
/// so the parent process can discover the listening address.
pub async fn run_server<B: SandboxBackend + 'static, M: MetadataStore + 'static>(
    backend: B,
    metadata: M,
    port: u16,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let embedded = std::env::var("RAP_EMBEDDED").is_ok();

    let (app, tracker) = build_router(backend, metadata, PlainCallbackClient::new());

    // In embedded mode, ignore the requested port and let the OS assign one.
    let bind_port = if embedded { 0 } else { port };
    let listener = tokio::net::TcpListener::bind(format!("0.0.0.0:{bind_port}")).await?;
    let actual_port = listener.local_addr()?.port();

    if embedded {
        // Emit the ready signal as a JSON line on stdout so the parent can
        // parse the port. This MUST be the first (and only) line on stdout.
        println!("{}", serde_json::json!({ "port": actual_port }));
    }

    tracing::info!("sandbox RAP server listening on port {actual_port}");

    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            tokio::signal::ctrl_c()
                .await
                .expect("failed to listen for ctrl+c");
            tracing::info!("received ctrl+c, shutting down gracefully");
        })
        .await?;

    // Cancel any in-flight commands (sends SIGTERM to child processes)
    tracker.cancel_all_in_flight().await;

    Ok(())
}
