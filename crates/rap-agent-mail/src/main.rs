use std::path::{Path, PathBuf};

use clap::Parser;
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(about = "RAP server that lets Infinity agents message agents on other daemons")]
struct Args {
    /// State directory (inbox registry, outbox, peer socket, logs).
    /// Defaults to `~/.infinity/mail`.
    #[arg(long)]
    state_dir: Option<PathBuf>,
    /// Daemon remotes file whose entries become SSH peers. Defaults to
    /// `~/.infinity/remotes.json`.
    #[arg(long)]
    remotes: Option<PathBuf>,
    /// RAP port when run standalone (with `RAP_EMBEDDED` set, a free port is
    /// picked and printed instead).
    #[arg(short, long, default_value_t = 3007)]
    port: u16,
}

type BoxError = Box<dyn std::error::Error + Send + Sync>;

fn home_dir() -> Result<PathBuf, BoxError> {
    dirs::home_dir().ok_or_else(|| "could not determine home directory".into())
}

/// Hold the state directory exclusively. The running mailbox wins: a second
/// one (say, `infinity --local` while the daemon runs) gives up after a
/// grace period that covers a previous instance still shutting down. Its
/// daemon's lifeline (stdin EOF) ends a mailbox when its daemon dies, so a
/// holder is never a stale orphan for long.
fn acquire_singleton(state_dir: &Path) -> Result<nix::fcntl::Flock<std::fs::File>, String> {
    use nix::errno::Errno;
    use nix::fcntl::{Flock, FlockArg};

    let lock_path = state_dir.join("lock");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&lock_path)
            .map_err(|e| format!("failed to open {}: {e}", lock_path.display()))?;
        match Flock::lock(file, FlockArg::LockExclusiveNonblock) {
            Ok(lock) => {
                let pid_path = state_dir.join("pid");
                std::fs::write(&pid_path, std::process::id().to_string())
                    .map_err(|e| format!("failed to write {}: {e}", pid_path.display()))?;
                return Ok(lock);
            }
            Err((_, Errno::EWOULDBLOCK)) if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            Err((_, Errno::EWOULDBLOCK)) => {
                let holder = std::fs::read_to_string(state_dir.join("pid")).unwrap_or_default();
                return Err(format!(
                    "another rap-agent-mail (pid {}) already serves {}; only one mailbox runs \
                     per state directory",
                    holder.trim(),
                    state_dir.display()
                ));
            }
            Err((_, e)) => return Err(format!("failed to lock {}: {e}", lock_path.display())),
        }
    }
}

/// Log a fatal startup error and exit. Also printed on stdout, where the
/// daemon (which discards stderr) quotes the first line in its boot error.
fn fail(msg: &str) -> ! {
    tracing::error!("{msg}");
    println!("rap-agent-mail: {msg}");
    std::process::exit(1);
}

fn main() -> Result<(), BoxError> {
    let args = Args::parse();
    let state_dir = match args.state_dir {
        Some(dir) => dir,
        None => home_dir()?.join(".infinity").join("mail"),
    };
    std::fs::create_dir_all(&state_dir)?;

    let log_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(state_dir.join("mail.log"))?;
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_writer(std::sync::Mutex::new(log_file))
        .with_ansi(false)
        .init();

    let embedded = std::env::var_os("RAP_EMBEDDED").is_some();
    if embedded && std::env::var("RAP_SERVER_LIFETIME").as_deref() != Ok("daemon") {
        // Spawned per session, several mailboxes would fight over one state
        // directory and inboxes would go deaf whenever their session idles.
        fail(
            "must run with daemon lifetime: set \"lifetime\": \"daemon\" on its entry in \
             ~/.infinity/rap.json (or reinstall with `infinity rap install --user \
             --daemon-lifetime --crate rap-agent-mail ...`) and restart the daemon",
        );
    }

    let _lock = acquire_singleton(&state_dir).unwrap_or_else(|e| fail(&e));

    let remotes = match args.remotes {
        Some(path) => path,
        None => home_dir()?.join(".infinity").join("remotes.json"),
    };
    let callback_override = std::env::var("RAP_CALLBACK_URL").ok();
    let config = rap_agent_mail::MailConfig::load(state_dir, Some(&remotes), callback_override)
        .unwrap_or_else(|e| fail(&e));

    // The daemon holds the write end of our stdin: EOF means it exited
    // (possibly by crashing), and its mailbox must not outlive it. Read on a
    // plain thread so a blocked read never holds up runtime shutdown.
    let (lifeline_tx, lifeline_rx) = tokio::sync::oneshot::channel::<()>();
    if embedded {
        std::thread::spawn(move || {
            use std::io::Read;
            let mut buf = [0u8; 64];
            while matches!(std::io::stdin().read(&mut buf), Ok(n) if n > 0) {}
            let _ = lifeline_tx.send(());
        });
    }

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let result = runtime.block_on(async {
        let port = if embedded { 0 } else { args.port };
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", port))
            .await
            .unwrap_or_else(|e| fail(&format!("failed to bind 127.0.0.1:{port}: {e}")));
        let actual_port = listener.local_addr()?.port();
        let running = rap_agent_mail::start(config, listener).unwrap_or_else(|e| fail(&e));
        if embedded {
            println!("{}", serde_json::json!({ "port": actual_port }));
        }

        let mut sigterm =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        let lifeline = async {
            match lifeline_rx.await {
                Ok(()) => (),
                // Not embedded: no lifeline to watch.
                Err(_) => std::future::pending().await,
            }
        };
        tokio::select! {
            _ = tokio::signal::ctrl_c() => tracing::info!("interrupted"),
            _ = sigterm.recv() => tracing::info!("terminated"),
            () = lifeline => tracing::info!("daemon exited (stdin closed)"),
        }
        tracing::info!("mailbox shutting down");
        drop(running);
        Ok::<(), BoxError>(())
    });
    // Dropping the aborted tasks kills their SSH tunnels (`kill_on_drop`).
    runtime.shutdown_timeout(std::time::Duration::from_secs(2));
    result
}
