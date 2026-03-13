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

    /// Base directory for temporary sandbox directories. When not specified,
    /// the system default (e.g. /tmp) is used.
    #[arg(long)]
    tempdir: Option<std::path::PathBuf>,
}

/// Internal entrypoint used when the binary is re-invoked inside a sandbox to
/// run a user command.  Sets PGID = PID so the whole process tree can be
/// signalled as a group, then `exec()`s the given command.
///
/// Usage: `rap-sandbox-local exec -- <program> [args...]`
#[cfg(unix)]
fn exec_mode(args: &[String]) -> ! {
    // Skip "--" separator if present.
    let args = if args.first().map(|s| s.as_str()) == Some("--") {
        &args[1..]
    } else {
        args
    };

    if args.is_empty() {
        eprintln!("exec mode: no command specified");
        std::process::exit(1);
    }

    // Create a new process group with PGID = our PID so that
    // kill(-pid, SIGTERM) reaches this process and all its children.
    use nix::unistd::{Pid, setpgid};
    let _ = setpgid(Pid::from_raw(0), Pid::from_raw(0));

    // Replace this process with the requested command.
    use std::os::unix::process::CommandExt;
    let err = std::process::Command::new(&args[0]).args(&args[1..]).exec();

    eprintln!("exec mode: failed to exec '{}': {err}", args[0]);
    std::process::exit(1);
}

fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // Fast-path: if invoked as `rap-sandbox-local exec ...`, enter exec mode
    // without starting the tokio runtime or any other server infrastructure.
    #[cfg(unix)]
    {
        let raw_args: Vec<String> = std::env::args().collect();
        if raw_args.get(1).map(|s| s.as_str()) == Some("exec") {
            exec_mode(&raw_args[2..]);
        }
    }

    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env().add_directive("info".parse().unwrap()))
        .with_writer(std::io::stderr)
        .init();

    let args = Args::parse();

    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(async {
            let backend = LocalBackend::new(args.enable_sandbox, args.tempdir);
            let metadata = FileMetadataStore::new();
            run_server(backend, metadata, args.port).await
        })
}
