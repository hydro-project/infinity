use infinity_daemon::rap_callback;
use tracing_subscriber::EnvFilter;

use clap::Parser;

use infinity_agent_cli::daemon_client;
use infinity_agent_cli::install;

mod acp_server;

type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// Infinity Agent CLI
#[derive(Parser, Debug)]
#[command(name = "infinity-agent-cli", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,

    /// Send an initial message to the agent on startup.
    #[arg(short = 'i', long = "initial-message")]
    initial_message: Option<String>,

    /// Send the task to the daemon and exit without opening the TUI.
    #[arg(short = 'H', long, conflicts_with = "local")]
    headless: Option<String>,

    /// Connect directly to an existing session by ID.
    #[arg(short = 's', long)]
    session: Option<String>,

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
    /// Model provider management
    Provider {
        #[command(subcommand)]
        action: ProviderCommands,
    },
    /// Update the CLI itself
    Update {
        /// Comma-separated list of features to pass to cargo install (overrides auto-detected features)
        #[arg(long)]
        features: Option<String>,
    },
    /// Daemon management
    Daemon {
        #[command(subcommand)]
        action: Option<DaemonCommands>,
    },
    /// Remote daemon management
    Remote {
        #[command(subcommand)]
        action: RemoteCommands,
    },
    /// Run as an ACP (Agent Client Protocol) stdio server
    Acp,
}

#[derive(clap::Subcommand, Debug)]
enum DaemonCommands {
    /// Stop the running daemon
    Stop,
    /// Restart the daemon (stop it if running, then start a fresh instance)
    Restart,
}

#[derive(clap::Subcommand, Debug)]
enum RemoteCommands {
    /// Add a new remote: infinity remote add <name> -- ssh <ssh_args...>
    Add {
        /// Name for this remote
        name: String,
        /// Transport and arguments (passed after --), e.g. "ssh my-host"
        #[arg(last = true, required = true)]
        args: Vec<String>,
    },
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

#[derive(clap::Subcommand, Debug)]
enum ProviderCommands {
    /// Install a model provider crate and register it in ~/.infinity/providers.json
    Install {
        /// Provider id to register under (e.g. "bedrock")
        id: String,

        /// Crate name to install (its binary becomes the provider command)
        #[arg(long = "crate")]
        crate_name: String,

        /// Git repository URL (passed to cargo install --git)
        #[arg(long)]
        git: Option<String>,

        /// Local path (passed to cargo install --path)
        #[arg(long)]
        path: Option<String>,
    },
    /// Re-install all model providers that have a recorded source
    Update,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> std::process::ExitCode {
    // Parse CLI arguments via clap.
    let cli = Cli::parse();

    std::fs::create_dir_all(".infinity").ok();
    if !matches!(cli.command, Some(Commands::Daemon { .. })) {
        let log_name = if matches!(cli.command, Some(Commands::Acp)) {
            "acp.log"
        } else {
            "cli.log"
        };
        let log_file = std::fs::File::create(format!(".infinity/{log_name}")).ok();
        if let Some(file) = log_file {
            tracing_subscriber::fmt()
                .with_env_filter(EnvFilter::from_default_env())
                .with_writer(std::sync::Mutex::new(file))
                .with_ansi(false)
                .init();
        }
    }

    let local = tokio::task::LocalSet::new();
    match local.run_until(async_main(cli)).await {
        Ok(()) => std::process::ExitCode::SUCCESS,
        // Print errors with Display formatting: the default `main` error
        // handling uses Debug, which escapes newlines in string-based
        // errors and mangles multi-line reports (e.g. captured daemon
        // startup output).
        Err(e) => {
            eprintln!("Error: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}

async fn async_main(cli: Cli) -> Result<(), BoxError> {
    // Handle subcommands
    if let Some(command) = cli.command {
        return match command {
            Commands::Acp => acp_server::run().await,
            Commands::Update { features } => install::run_self_update(features.as_deref()).await,
            Commands::Daemon { action } => match action {
                Some(DaemonCommands::Stop) => {
                    let pid = daemon_client::stop_daemon()
                        .await?
                        .ok_or("daemon is not running")?;
                    println!("daemon stopped (pid {pid})");
                    Ok(())
                }
                Some(DaemonCommands::Restart) => daemon_client::restart_daemon().await,
                None => infinity_daemon::run_daemon(true).await,
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
            Commands::Provider { action } => match action {
                ProviderCommands::Install {
                    id,
                    crate_name,
                    git,
                    path,
                } => {
                    install::run_provider_install(install::ProviderInstallArgs {
                        id,
                        crate_name,
                        git,
                        path,
                    })
                    .await
                }
                ProviderCommands::Update => install::run_provider_update().await,
            },
            Commands::Remote { action } => match action {
                RemoteCommands::Add { name, args } => {
                    if args.first().map(|s| s.as_str()) != Some("ssh") {
                        return Err("expected 'ssh' as first argument after --".into());
                    }
                    let ssh_args = &args[1..];
                    if ssh_args.is_empty() {
                        return Err("no ssh arguments provided after 'ssh'".into());
                    }
                    let path = infinity_protocol::remotes_config_path();
                    let mut remotes: Vec<infinity_daemon::remote::RemoteConfig> =
                        match std::fs::read_to_string(&path) {
                            Ok(s) => serde_json::from_str(&s)
                                .map_err(|e| format!("failed to parse {}: {e}", path.display()))?,
                            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
                            Err(e) => {
                                return Err(
                                    format!("failed to read {}: {e}", path.display()).into()
                                );
                            }
                        };
                    if remotes.iter().any(|r| r.name == name) {
                        return Err(format!("remote '{name}' already exists").into());
                    }
                    remotes.push(infinity_daemon::remote::RemoteConfig {
                        name: name.clone(),
                        ssh_args: ssh_args.to_vec(),
                    });
                    if let Some(parent) = path.parent() {
                        std::fs::create_dir_all(parent).ok();
                    }
                    std::fs::write(&path, serde_json::to_string_pretty(&remotes)?)?;
                    println!("added remote '{name}' (ssh {})", ssh_args.join(" "));
                    Ok(())
                }
            },
        };
    }

    // Headless mode: send task to daemon and exit without TUI.
    if let Some(message) = cli.headless {
        return daemon_client::run_headless(message).await;
    }

    // Try daemon mode first — auto-launches daemon if not running.
    let daemon_err = if cli.local {
        None
    } else {
        match daemon_client::run_with_daemon(cli.initial_message.clone(), cli.session.clone()).await
        {
            Ok(()) => return Ok(()),
            Err(e) => {
                tracing::debug!("daemon mode failed, falling back to direct mode: {e}");
                Some(format!("{e}"))
            }
        }
    };

    // Direct mode: run daemon session manager in-process
    run_direct(cli.initial_message, cli.session, daemon_err).await
}

#[tracing::instrument]
async fn run_direct(
    initial_message: Option<String>,
    session: Option<String>,
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
        daemon_client::run_in_memory(
            from_daemon_rx,
            to_daemon_tx,
            initial_message,
            session,
            daemon_err
        )
    );

    let mut mgr = mgr.lock().await;
    let session_ids: Vec<String> = mgr.sessions.keys().cloned().collect();
    for sid in session_ids {
        mgr.cleanup_session(&sid).await;
    }

    res
}
