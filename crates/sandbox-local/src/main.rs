use clap::Parser;
use tracing_subscriber::EnvFilter;

use sandbox_local::backend::LocalBackend;
use sandbox_local::metadata::InMemoryMetadataStore;
use sandbox_local::server::run_server;

#[derive(Parser)]
#[command(about = "Local sandbox RAP server")]
struct Args {
    /// Port to listen on
    #[arg(short, long, default_value_t = 3001)]
    port: u16,

    /// Enable macOS sandbox-exec filesystem write restrictions
    #[arg(long)]
    enable_sandbox: bool,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env().add_directive("info".parse().unwrap()))
        .with_writer(std::io::stderr)
        .init();

    let args = Args::parse();

    let backend = LocalBackend::new(args.enable_sandbox);
    let metadata = InMemoryMetadataStore::new();

    run_server(backend, metadata, args.port).await
}
