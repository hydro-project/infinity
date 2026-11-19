//! Peer links: the inbound socket listener, the dialing loops (over SSH or
//! a local socket), and the per-link frame pump.

use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::mpsc;
use tokio_util::codec::{Framed, LengthDelimitedCodec};

use crate::address::validate_name;
use crate::config::PeerTransport;
use crate::mailbox::{LINK_QUEUE, Link, Mailbox};
use crate::wire::{Frame, PROTOCOL_VERSION, codec};

type Conn = Framed<UnixStream, LengthDelimitedCodec>;

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
/// Reconnect attempts back off up to this interval.
const MAX_BACKOFF: Duration = Duration::from_secs(30);
/// A link that stayed up this long resets the reconnect backoff.
const STABLE_LINK: Duration = Duration::from_secs(60);
const PING_EVERY: Duration = Duration::from_secs(20);
/// A link that receives nothing (not even pings) for this long is dead.
const LINK_DEAD_AFTER: Duration = Duration::from_secs(70);
/// A peer that does not take a frame for this long is not reading.
const WRITE_TIMEOUT: Duration = Duration::from_secs(60);
/// Marks the remote home directory in ssh output, so whatever the remote
/// shell's startup files print cannot be mistaken for it.
const HOME_SENTINEL: &str = "rap-agent-mail-home:";

async fn send_frame(conn: &mut Conn, frame: &Frame) -> Result<(), String> {
    let bytes = serde_json::to_vec(frame).expect("bug: failed to serialize frame");
    conn.send(Bytes::from(bytes))
        .await
        .map_err(|e| format!("write failed: {e}"))
}

async fn recv_frame(conn: &mut Conn) -> Result<Frame, String> {
    match conn.next().await {
        Some(Ok(bytes)) => {
            serde_json::from_slice(&bytes).map_err(|e| format!("malformed frame: {e}"))
        }
        Some(Err(e)) => Err(format!("read failed: {e}")),
        None => Err("connection closed".to_owned()),
    }
}

/// Dialing side of the handshake. Returns the peer's announced name and
/// host ID.
async fn handshake_dial(mailbox: &Mailbox, conn: &mut Conn) -> Result<(String, String), String> {
    let own_id = mailbox.host_id().await;
    send_frame(
        conn,
        &Frame::Hello {
            name: mailbox.name().to_owned(),
            host_id: own_id.clone(),
            protocol: PROTOCOL_VERSION,
        },
    )
    .await?;
    let frame = tokio::time::timeout(HANDSHAKE_TIMEOUT, recv_frame(conn))
        .await
        .map_err(|_| "handshake timed out".to_owned())?
        .map_err(|e| {
            format!(
                "{e} during handshake: is rap-agent-mail running (with daemon lifetime) on the \
                 peer, and its daemon up?"
            )
        })?;
    match frame {
        Frame::Welcome { host_id, .. } if host_id == own_id => {
            Err("this peer is this mailbox itself".to_owned())
        }
        Frame::Welcome {
            name,
            host_id,
            protocol,
        } if protocol == PROTOCOL_VERSION => {
            validate_name("host", &name)?;
            Ok((name, host_id))
        }
        Frame::Welcome { protocol, .. } => Err(format!(
            "peer speaks protocol {protocol}, this mailbox speaks {PROTOCOL_VERSION}: update \
             rap-agent-mail on both hosts"
        )),
        Frame::Reject { reason } => Err(format!("peer refused the link: {reason}")),
        other => Err(format!("unexpected handshake frame: {other:?}")),
    }
}

/// Listening side of the handshake. Returns the dialer's announced name and
/// host ID.
async fn handshake_accept(mailbox: &Mailbox, conn: &mut Conn) -> Result<(String, String), String> {
    let frame = tokio::time::timeout(HANDSHAKE_TIMEOUT, recv_frame(conn))
        .await
        .map_err(|_| "handshake timed out".to_owned())??;
    let Frame::Hello {
        name,
        host_id,
        protocol,
    } = frame
    else {
        return Err(format!("expected hello, got {frame:?}"));
    };
    let own_id = mailbox.host_id().await;
    let refusal = if protocol != PROTOCOL_VERSION {
        Some(format!(
            "protocol {protocol} is not supported (this mailbox speaks {PROTOCOL_VERSION})"
        ))
    } else if host_id == own_id {
        Some("this mailbox dialed itself".to_owned())
    } else if name == mailbox.name() {
        Some(format!(
            "`{name}` is this mailbox's own name; give one of the two mailboxes a distinct \
             \"name\" in its mail/config.json"
        ))
    } else {
        validate_name("host", &name).err()
    };
    if let Some(reason) = refusal {
        let _ = send_frame(
            conn,
            &Frame::Reject {
                reason: reason.clone(),
            },
        )
        .await;
        return Err(reason);
    }
    send_frame(
        conn,
        &Frame::Welcome {
            name: mailbox.name().to_owned(),
            host_id: own_id,
            protocol: PROTOCOL_VERSION,
        },
    )
    .await?;
    Ok((name, host_id))
}

/// Who is on the other end of a link.
enum LinkPeer {
    /// A peer this mailbox dialed, by the name it dials it by.
    Dialed(String),
    /// A peer that dialed in, known by what it announced. Its name is
    /// re-resolved for every frame: if this mailbox also dials that host
    /// (and learns its host ID meanwhile), it gets the same name on both
    /// links, so replies and acks match whichever link they arrive on.
    Inbound { announced: String, host_id: String },
}

impl LinkPeer {
    async fn name(&self, mailbox: &Mailbox) -> String {
        match self {
            LinkPeer::Dialed(name) => name.clone(),
            LinkPeer::Inbound { announced, host_id } => {
                mailbox.inbound_name(announced, host_id).await
            }
        }
    }
}

/// Pump frames on an established link until it fails. Returns why it ended.
async fn run_link(mailbox: &Arc<Mailbox>, link_peer: LinkPeer, conn: Conn) -> String {
    let peer = &link_peer.name(mailbox).await;
    if let LinkPeer::Inbound { host_id, .. } = &link_peer {
        mailbox.note_inbound_peer(peer, host_id).await;
    }
    let (mut sink, mut stream) = conn.split();
    let (tx, mut rx) = mpsc::channel::<Frame>(LINK_QUEUE);
    let link_id = mailbox.next_link_id.fetch_add(1, Ordering::Relaxed);
    let backlog = mailbox
        .register_link(
            peer,
            Link {
                id: link_id,
                tx: tx.clone(),
            },
        )
        .await;
    tracing::info!(
        "link {link_id} to `{peer}` up ({} queued envelopes)",
        backlog.len()
    );

    // Inbound frames are handled in order by a task of their own, so tearing
    // the link down never cancels a delivery halfway (its ack is simply
    // lost, and the peer retransmits).
    let (inbound_tx, mut inbound_rx) = mpsc::channel::<Frame>(LINK_QUEUE);
    let processor = {
        let mailbox = mailbox.clone();
        let tx = tx.clone();
        async move {
            while let Some(frame) = inbound_rx.recv().await {
                let peer = link_peer.name(&mailbox).await;
                match frame {
                    Frame::Envelope(envelope) => {
                        let id = envelope.id.clone();
                        if let Some(outcome) = mailbox.accept(Some(&peer), envelope).await {
                            let _ = tx.try_send(Frame::Ack { id, outcome });
                        }
                    }
                    Frame::Ack { id, outcome } => mailbox.handle_ack(&id, outcome).await,
                    Frame::ListInboxes { request_id } => {
                        let inboxes = mailbox.inbox_infos().await;
                        let _ = tx.try_send(Frame::Inboxes {
                            request_id,
                            inboxes,
                        });
                    }
                    Frame::Inboxes {
                        request_id,
                        inboxes,
                    } => mailbox.resolve_inbox_list(&request_id, inboxes),
                    Frame::Ping
                    | Frame::Hello { .. }
                    | Frame::Welcome { .. }
                    | Frame::Reject { .. } => {}
                }
            }
        }
    };
    tokio::spawn(rap_protocol::log_panic("peer_link_processor", processor));

    // Liveness: both sides ping every PING_EVERY; a link that has received
    // nothing for LINK_DEAD_AFTER is dropped (so a hung peer shows up as
    // unreachable instead of a link that silently swallows messages).
    let started = Instant::now();
    let last_rx_ms = AtomicU64::new(0);
    // Set while the reader waits on a full inbound queue: then the silence is
    // our own slow processing, not the peer's.
    let backed_up = std::sync::atomic::AtomicBool::new(false);
    let heartbeat = async {
        let mut tick = tokio::time::interval(PING_EVERY);
        loop {
            tick.tick().await;
            let idle =
                started.elapsed() - Duration::from_millis(last_rx_ms.load(Ordering::Relaxed));
            if idle > LINK_DEAD_AFTER && !backed_up.load(Ordering::Relaxed) {
                return format!(
                    "peer stopped responding ({}s without traffic)",
                    idle.as_secs()
                );
            }
            let _ = tx.try_send(Frame::Ping);
        }
    };
    let writer = async {
        // The backlog goes first; anything enqueued since registration is
        // behind it in `rx`.
        let backlog = backlog.into_iter().map(Frame::Envelope);
        let mut frames = futures_util::stream::iter(backlog)
            .chain(futures_util::stream::poll_fn(|cx| rx.poll_recv(cx)));
        while let Some(frame) = frames.next().await {
            let bytes = serde_json::to_vec(&frame).expect("bug: failed to serialize frame");
            match tokio::time::timeout(WRITE_TIMEOUT, sink.send(Bytes::from(bytes))).await {
                Ok(Ok(())) => {}
                Ok(Err(e)) => return format!("write failed: {e}"),
                Err(_) => return "peer stopped reading".to_owned(),
            }
        }
        "link closed".to_owned()
    };
    let reader = async {
        loop {
            let frame = match stream.next().await {
                Some(Ok(bytes)) => match serde_json::from_slice::<Frame>(&bytes) {
                    Ok(frame) => frame,
                    Err(e) => return format!("malformed frame from peer: {e}"),
                },
                Some(Err(e)) => return format!("read failed: {e}"),
                None => return "peer closed the connection".to_owned(),
            };
            let touch =
                || last_rx_ms.store(started.elapsed().as_millis() as u64, Ordering::Relaxed);
            touch();
            match frame {
                Frame::Hello { .. } | Frame::Welcome { .. } | Frame::Reject { .. } => {
                    return "protocol error: handshake frame on an established link".to_owned();
                }
                Frame::Ping => continue,
                frame => match inbound_tx.try_send(frame) {
                    Ok(()) => {}
                    Err(mpsc::error::TrySendError::Full(frame)) => {
                        backed_up.store(true, Ordering::Relaxed);
                        let sent = inbound_tx.send(frame).await;
                        backed_up.store(false, Ordering::Relaxed);
                        touch();
                        if sent.is_err() {
                            return "link processor stopped".to_owned();
                        }
                    }
                    Err(mpsc::error::TrySendError::Closed(_)) => {
                        return "link processor stopped".to_owned();
                    }
                },
            }
        }
    };
    let reason = tokio::select! {
        reason = writer => reason,
        reason = reader => reason,
        reason = heartbeat => reason,
    };

    mailbox.unregister_link(peer, link_id);
    tracing::info!("link {link_id} to `{peer}` down: {reason}");
    reason
}

/// Accept inbound links on `<state_dir>/peer.sock` forever. The links run
/// inside this future, so dropping it closes them too.
pub async fn serve_peers(mailbox: Arc<Mailbox>, listener: UnixListener) {
    let mut links = tokio::task::JoinSet::new();
    loop {
        let stream = tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok((stream, _)) => stream,
                Err(e) => {
                    tracing::warn!("peer accept failed: {e}");
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    continue;
                }
            },
            Some(_) = links.join_next(), if !links.is_empty() => continue,
        };
        let mailbox = mailbox.clone();
        links.spawn(rap_protocol::log_panic("peer_link", async move {
            let mut conn = Framed::new(stream, codec());
            match handshake_accept(&mailbox, &mut conn).await {
                Ok((announced, host_id)) => {
                    run_link(&mailbox, LinkPeer::Inbound { announced, host_id }, conn).await;
                }
                Err(e) => tracing::warn!("refused inbound peer link: {e}"),
            }
        }));
    }
}

/// Bind the inbound peer socket, readable only by this user.
pub fn bind_peer_socket(state_dir: &Path) -> Result<UnixListener, String> {
    use std::os::unix::fs::PermissionsExt;
    let path = state_dir.join("peer.sock");
    // Only one mailbox runs per state dir (see the singleton lock), so an
    // existing socket is stale.
    match std::fs::remove_file(&path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(format!("failed to remove stale {}: {e}", path.display())),
    }
    let listener =
        UnixListener::bind(&path).map_err(|e| format!("failed to bind {}: {e}", path.display()))?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
        .map_err(|e| format!("failed to restrict {}: {e}", path.display()))?;
    Ok(listener)
}

/// Keep a link to `peer` up forever, reconnecting with backoff.
pub async fn dial_peer(mailbox: Arc<Mailbox>, peer: String, transport: PeerTransport) {
    let mut backoff = Duration::from_secs(1);
    let mut remote_home: Option<String> = None;
    let mut last_failure: Option<String> = None;
    loop {
        set_status(
            &mailbox,
            &peer,
            match &last_failure {
                Some(reason) => format!("connecting; last attempt: {reason}"),
                None => "connecting".to_owned(),
            },
        );
        let started = Instant::now();
        let reason = match connect(&mailbox, &peer, &transport, &mut remote_home).await {
            Ok((mut conn, _tunnel)) => match handshake_dial(&mailbox, &mut conn).await {
                Ok((announced, host_id)) => {
                    if announced != peer {
                        tracing::info!("peer `{peer}` calls itself `{announced}`");
                    }
                    mailbox.learn_peer(&peer, &host_id, &announced).await;
                    set_status(&mailbox, &peer, "connected".to_owned());
                    // `_tunnel` (the SSH process) lives until the link ends.
                    run_link(&mailbox, LinkPeer::Dialed(peer.clone()), conn).await
                }
                Err(e) => e,
            },
            Err(e) => e,
        };
        if started.elapsed() >= STABLE_LINK {
            backoff = Duration::from_secs(1);
        }
        tracing::info!("peer `{peer}`: {reason}; retrying in {backoff:?}");
        set_status(&mailbox, &peer, format!("disconnected: {reason}"));
        last_failure = Some(reason);
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(MAX_BACKOFF);
    }
}

fn set_status(mailbox: &Mailbox, peer: &str, status: String) {
    mailbox
        .peer_status
        .lock()
        .expect("bug: poisoned")
        .insert(peer.to_owned(), status);
}

/// Open a connection to the peer. For SSH peers, also returns the SSH child
/// holding the socket forward (killed when dropped).
async fn connect(
    mailbox: &Mailbox,
    peer: &str,
    transport: &PeerTransport,
    remote_home: &mut Option<String>,
) -> Result<(Conn, Option<tokio::process::Child>), String> {
    match transport {
        PeerTransport::Socket(path) => {
            let stream = UnixStream::connect(path)
                .await
                .map_err(|e| format!("connect to {} failed: {e}", path.display()))?;
            Ok((Framed::new(stream, codec()), None))
        }
        PeerTransport::Ssh {
            ssh_args,
            remote_socket,
        } => {
            let remote_socket = match remote_socket {
                Some(path) => path.clone(),
                None => {
                    let home = match remote_home {
                        Some(home) => home.clone(),
                        None => {
                            let home = resolve_remote_home(ssh_args).await?;
                            *remote_home = Some(home.clone());
                            home
                        }
                    };
                    format!("{home}/.infinity/mail/peer.sock")
                }
            };
            let links_dir = mailbox.config.state_dir.join("links");
            std::fs::create_dir_all(&links_dir)
                .map_err(|e| format!("failed to create {}: {e}", links_dir.display()))?;
            let local = links_dir.join(format!("{peer}.sock"));
            let (stream, child) = ssh_forward(ssh_args, &local, &remote_socket).await?;
            Ok((Framed::new(stream, codec()), Some(child)))
        }
    }
}

/// SSH options that make an unattended tunnel fail fast instead of hanging.
fn ssh_base_command() -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new("ssh");
    cmd.args([
        "-o",
        "BatchMode=yes",
        "-o",
        "ConnectTimeout=20",
        "-o",
        "ServerAliveInterval=15",
        "-o",
        "ServerAliveCountMax=3",
    ])
    .stdin(Stdio::null())
    .kill_on_drop(true);
    cmd
}

async fn resolve_remote_home(ssh_args: &[String]) -> Result<String, String> {
    let output = tokio::time::timeout(
        Duration::from_secs(60),
        ssh_base_command()
            .args(ssh_args)
            .arg("--")
            .arg(format!("printf '\\n{HOME_SENTINEL}%s\\n' \"$HOME\""))
            .output(),
    )
    .await
    .map_err(|_| "ssh timed out resolving the remote home directory".to_owned())?
    .map_err(|e| format!("failed to run ssh: {e}"))?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let home = stdout
        .lines()
        .find_map(|line| line.strip_prefix(HOME_SENTINEL))
        .unwrap_or_default();
    if !output.status.success() || !home.starts_with('/') {
        return Err(format!(
            "ssh failed to resolve the remote home directory ({}): {}",
            output.status,
            last_stderr_line(&output.stderr)
        ));
    }
    if home.contains(':') {
        return Err(format!(
            "the remote home directory {home:?} contains ':', which ssh socket forwarding \
             cannot express; set `remote_socket` for this peer in mail/config.json"
        ));
    }
    Ok(home.to_owned())
}

/// Forward local socket `local` to `remote` on the SSH host and connect.
/// ssh's stderr goes to `<peer>.ssh.log` next to the socket (truncated per
/// attempt) and is quoted in errors when ssh exits.
async fn ssh_forward(
    ssh_args: &[String],
    local: &Path,
    remote: &str,
) -> Result<(UnixStream, tokio::process::Child), String> {
    let pid_path = local.with_extension("ssh.pid");
    reap_orphaned_tunnel(&pid_path, local);
    let _ = std::fs::remove_file(local);
    let log_path = local.with_extension("ssh.log");
    let log = std::fs::File::create(&log_path)
        .map_err(|e| format!("failed to create {}: {e}", log_path.display()))?;
    let mut child = ssh_base_command()
        .args([
            "-o",
            "ExitOnForwardFailure=yes",
            "-o",
            "StreamLocalBindUnlink=yes",
            "-N",
            "-L",
        ])
        .arg(format!("{}:{remote}", local.display()))
        .args(ssh_args)
        .stdout(Stdio::null())
        .stderr(log)
        .spawn()
        .map_err(|e| format!("failed to run ssh: {e}"))?;
    if let Some(pid) = child.id() {
        let _ = std::fs::write(&pid_path, pid.to_string());
    }

    let deadline = Instant::now() + Duration::from_secs(60);
    while !local.exists() {
        if let Ok(Some(status)) = child.try_wait() {
            let stderr = std::fs::read(&log_path).unwrap_or_default();
            return Err(format!(
                "ssh exited ({status}): {}",
                last_stderr_line(&stderr)
            ));
        }
        if Instant::now() > deadline {
            return Err("timed out waiting for the ssh socket forward".to_owned());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let stream = UnixStream::connect(local)
        .await
        .map_err(|e| format!("connect to the ssh forward failed: {e}"))?;
    Ok((stream, child))
}

/// Kill the tunnel a previous mailbox process left behind for this peer.
/// A mailbox killed outright (SIGKILL) cannot stop its ssh children, which
/// would otherwise keep a connection open forever. The pid is only trusted
/// if that process is still an ssh forwarding this very socket.
fn reap_orphaned_tunnel(pid_path: &Path, local: &Path) {
    let Some(pid) = std::fs::read_to_string(pid_path)
        .ok()
        .and_then(|s| s.trim().parse::<i32>().ok())
        .filter(|pid| *pid > 1)
    else {
        return;
    };
    let command = std::process::Command::new("ps")
        .args(["-o", "command=", "-p", &pid.to_string()])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default();
    if command.starts_with("ssh ") && command.contains(&format!("-L {}:", local.display())) {
        tracing::info!("killing orphaned ssh tunnel (pid {pid})");
        let _ = nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(pid),
            nix::sys::signal::Signal::SIGTERM,
        );
    }
}

fn last_stderr_line(stderr: &[u8]) -> String {
    let text = String::from_utf8_lossy(stderr);
    text.lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("(no output)")
        .trim()
        .to_owned()
}
