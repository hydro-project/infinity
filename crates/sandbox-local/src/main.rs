use clap::Parser;
use tracing_subscriber::EnvFilter;

use sandbox_local::backend::LocalBackend;
use sandbox_local::metadata::FileMetadataStore;
use sandbox_local::server::run_server;

#[derive(Parser)]
#[command(about = "Local sandbox RAP server")]
struct Args {
    /// Port to listen on
    #[arg(short, long, default_value_t = 3001)]
    port: u16,

    /// Enable sandbox filesystem write restrictions (macOS sandbox-exec / Linux bwrap).
    /// Enabled by default; use --no-enable-sandbox to disable.
    #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
    enable_sandbox: bool,

    /// Directory for sandbox metadata files (group_id.json).
    /// Defaults to `./sandbox` (relative to the working directory).
    #[arg(long)]
    metadata_dir: Option<std::path::PathBuf>,
}

fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let args = Args::parse();

    let metadata_dir = args.metadata_dir.unwrap_or_else(|| {
        std::env::current_dir()
            .expect("failed to get current directory")
            .join("sandbox")
    });

    // When RAP_EMBEDDED is set, log to the metadata dir; otherwise use CWD.
    let embedded = std::env::var("RAP_EMBEDDED").is_ok();
    let log_path = if embedded {
        std::fs::create_dir_all(&metadata_dir).ok();
        metadata_dir.join("rap-sandbox.log")
    } else {
        std::path::PathBuf::from("./rap-sandbox.log")
    };

    let log_file = std::fs::File::create(&log_path).expect("failed to create rap-sandbox.log");

    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .with_writer(std::sync::Mutex::new(log_file))
        .with_ansi(false)
        .init();

    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(async {
            let backend = LocalBackend::new(args.enable_sandbox);
            let metadata = FileMetadataStore::new(metadata_dir);
            run_server(backend, metadata, args.port).await
        })
}
