use tracing_subscriber::EnvFilter;

use sandbox_local::backend::LocalBackend;
use sandbox_local::metadata::InMemoryMetadataStore;
use sandbox_local::server::run_server;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env().add_directive("info".parse().unwrap()))
        .init();

    let port: u16 = std::env::var("PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(3001);

    let backend = LocalBackend::new();
    let metadata = InMemoryMetadataStore::new();

    run_server(backend, metadata, port).await
}
