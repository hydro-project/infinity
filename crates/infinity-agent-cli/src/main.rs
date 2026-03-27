use infinity_daemon::rap_callback;
use tracing_subscriber::EnvFilter;

use clap::Parser;

mod daemon_client;

use infinity_agent_cli::install;

type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// Infinity Agent CLI
#[derive(Parser, Debug)]
#[command(name = "infinity-agent-cli", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,

    /// Send an initial message to the agent on startup.
    #[arg(short, long)]
    message: Option<String>,

    #[arg(short, long)]
    local: bool,
}

#[derive(clap::Subcommand, Debug)]
enum Commands {
    /// RAP tool management
    Rap {
        #[command(subcommand)]
        action: RapCommands,
    },
    /// Update the CLI itself
    Update,
    /// Daemon management
    Daemon {
        #[command(subcommand)]
        action: Option<DaemonCommands>,
    },
}

#[derive(clap::Subcommand, Debug)]
enum DaemonCommands {
    /// Stop the running daemon
    Stop,
}

#[derive(clap::Subcommand, Debug)]
enum RapCommands {
    /// Install a RAP crate and register it in rap.json
    Install {
        /// Install to user-level ~/.infinity/rap.json (required)
        #[arg(long)]
        user: bool,

        /// Crate name to install
        #[arg(long = "crate")]
        crate_name: String,

        /// Git repository URL (passed to cargo install --git)
        #[arg(long)]
        git: Option<String>,

        /// Local path (passed to cargo install --path)
        #[arg(long)]
        path: Option<String>,
    },
    /// Re-install all RAP tools that have a recorded source
    Update {
        /// Update user-level ~/.infinity/rap.json tools (required)
        #[arg(long)]
        user: bool,
    },
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), BoxError> {
    let local = tokio::task::LocalSet::new();
    local.run_until(async_main()).await
}

async fn async_main() -> Result<(), BoxError> {
    std::fs::create_dir_all(".infinity").ok();

    // Parse CLI arguments via clap.
    let cli = Cli::parse();

    if !matches!(cli.command, Some(Commands::Daemon { .. })) {
        let log_file = std::fs::File::create(".infinity/cli.log").ok();
        if let Some(file) = log_file {
            tracing_subscriber::fmt()
                .with_env_filter(EnvFilter::from_default_env())
                .with_writer(std::sync::Mutex::new(file))
                .with_ansi(false)
                .init();
        }
    }

    // Handle subcommands
    if let Some(command) = cli.command {
        return match command {
            Commands::Update => install::run_self_update().await,
            Commands::Daemon { action } => match action {
                Some(DaemonCommands::Stop) => {
                    let pid_path = infinity_protocol::pid_path();
                    let pid_str = std::fs::read_to_string(&pid_path)
                        .map_err(|_| "daemon is not running (no pid file)")?;
                    let pid: i32 = pid_str.trim().parse().map_err(|_| "invalid pid file")?;
                    nix::sys::signal::kill(
                        nix::unistd::Pid::from_raw(pid),
                        nix::sys::signal::Signal::SIGTERM,
                    )
                    .map_err(|e| format!("failed to send SIGTERM: {e}"))?;
                    println!("sent SIGTERM to daemon (pid {pid})");
                    Ok(())
                }
                None => infinity_daemon::run_daemon().await,
            },
            Commands::Rap { action } => match action {
                RapCommands::Install {
                    user,
                    crate_name,
                    git,
                    path,
                } => {
                    if !user {
                        return Err("--user is currently required for rap install".into());
                    }
                    install::run_install(install::InstallArgs {
                        crate_name,
                        git,
                        path,
                    })
                    .await
                }
                RapCommands::Update { user } => {
                    if !user {
                        return Err("--user is currently required for rap update".into());
                    }
                    install::run_update().await
                }
            },
        };
    }

    // Try daemon mode first — auto-launches daemon if not running.
    let daemon_err = if cli.local {
        None
    } else {
        match daemon_client::run_with_daemon(cli.message.clone()).await {
            Ok(()) => return Ok(()),
            Err(e) => {
                tracing::debug!("daemon mode failed, falling back to direct mode: {e}");
                Some(format!("{e}"))
            }
        }
    };

    // Direct mode: run daemon session manager in-process
    run_direct(cli.message, daemon_err).await
}

#[tracing::instrument]
async fn run_direct(
    initial_message: Option<String>,
    daemon_err: Option<String>,
) -> Result<(), BoxError> {
    let state_dir = std::env::current_dir()?.join(".infinity");

    let mgr = rap_callback::start_callback_server(state_dir)
        .await
        .map_err(|e| format!("Failed to start callback server: {e}"))?;
    tracing::info!("Shared callback server started");

    // In-memory channels — no serialization
    let (to_daemon_tx, to_daemon_rx) = tokio::sync::mpsc::unbounded_channel();
    let (from_daemon_tx, from_daemon_rx) = tokio::sync::mpsc::unbounded_channel();

    // Run the daemon's client handler on channels directly
    let (_, res) = tokio::join!(
        infinity_daemon::client_handler::handle_client_channels(
            to_daemon_rx,
            from_daemon_tx,
            mgr.clone(),
        ),
        daemon_client::run_in_memory(from_daemon_rx, to_daemon_tx, initial_message, daemon_err)
    );

    let mut mgr = mgr.lock().await;
    let session_ids: Vec<String> = mgr.sessions.keys().cloned().collect();
    for sid in session_ids {
        mgr.cleanup_session(&sid).await;
    }

    res
}
