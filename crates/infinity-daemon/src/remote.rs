use std::collections::HashMap;
use std::sync::Arc;

use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use infinity_protocol::{ClientMessage, DaemonMessage, SessionInfo};
use serde::Deserialize;
use tokio::net::UnixStream;
use tokio::process::Child;
use tokio::sync::mpsc;
use tokio_util::codec::{Framed, LengthDelimitedCodec};

type BoxError = Box<dyn std::error::Error + Send + Sync>;
pub type BroadcastClients = Arc<std::sync::Mutex<Vec<mpsc::UnboundedSender<DaemonMessage>>>>;

#[derive(Deserialize, Clone, Debug)]
pub struct RemoteConfig {
    pub name: String,
    pub ssh_args: Vec<String>,
}

pub fn load_remotes_config() -> Vec<RemoteConfig> {
    let path = infinity_protocol::remotes_config_path();
    match std::fs::read_to_string(&path) {
        Ok(contents) => serde_json::from_str(&contents).unwrap_or_default(),
        Err(_) => Vec::new(),
    }
}

#[derive(Debug, Clone)]
pub enum RemoteStatus {
    Connecting,
    Connected,
    Disconnected(String),
}

pub struct RemoteState {
    pub status: RemoteStatus,
    pub sessions: HashMap<String, SessionInfo>,
    /// Local socket path for the SSH tunnel (set when connected).
    pub local_sock: Option<std::path::PathBuf>,
    /// SSH args for this remote.
    pub ssh_args: Vec<String>,
}

type RemoteMap = Arc<std::sync::Mutex<HashMap<String, RemoteState>>>;

#[derive(Clone)]
pub struct RemoteDaemons {
    remotes: RemoteMap,
}

impl RemoteDaemons {
    pub fn new(configs: Vec<RemoteConfig>, broadcast_clients: BroadcastClients) -> Self {
        let remotes: RemoteMap = Arc::new(std::sync::Mutex::new(HashMap::new()));
        for cfg in &configs {
            remotes.lock().expect("bug: mutex poisoned").insert(
                cfg.name.clone(),
                RemoteState {
                    status: RemoteStatus::Connecting,
                    sessions: HashMap::new(),
                    local_sock: None,
                    ssh_args: cfg.ssh_args.clone(),
                },
            );
            let name = cfg.name.clone();
            let ssh_args = cfg.ssh_args.clone();
            let map = remotes.clone();
            let bc = broadcast_clients.clone();
            tokio::task::spawn_local(rap_protocol::log_panic(
                "remote_control_worker",
                control_worker(name, ssh_args, map, bc),
            ));
        }
        Self { remotes }
    }

    /// Collect all remote sessions with prefixed IDs (used for initial Welcome).
    pub fn all_remote_sessions(&self) -> HashMap<String, SessionInfo> {
        let map = self.remotes.lock().expect("bug: mutex poisoned");
        let mut result = HashMap::new();
        for (remote_name, state) in map.iter() {
            if !matches!(state.status, RemoteStatus::Connected) {
                continue;
            }
            for (sid, info) in &state.sessions {
                let mut info = info.clone();
                info.remote = Some(remote_name.clone());
                for t in &mut info.threads {
                    t.thread_id = format!("{remote_name}/{}", t.thread_id);
                    t.parent_thread_id = format!("{remote_name}/{}", t.parent_thread_id);
                }
                result.insert(format!("{}/{}", remote_name, sid), info);
            }
        }
        result
    }

    /// Open a new proxied connection to a remote session.
    /// Reuses the existing SSH tunnel socket when available.
    pub async fn connect_remote_session(
        &self,
        remote_name: &str,
        session_id: &str,
        thread_id: Option<&str>,
    ) -> Result<
        (
            mpsc::UnboundedSender<ClientMessage>,
            mpsc::UnboundedReceiver<DaemonMessage>,
        ),
        BoxError,
    > {
        let res = self.open_raw_connection(remote_name).await?;

        res.0
            .send(ClientMessage::Connect {
                session_id: session_id.to_owned(),
                thread_id: thread_id.map(|t| t.to_owned()),
            })
            .map_err(|e| format!("send Connect failed: {e}"))?;

        Ok(res)
    }

    /// Open a raw connection to a remote daemon (Welcome consumed, no Connect sent).
    /// Used for migration flows where we send custom messages.
    pub async fn open_raw_connection(
        &self,
        remote_name: &str,
    ) -> Result<
        (
            mpsc::UnboundedSender<ClientMessage>,
            mpsc::UnboundedReceiver<DaemonMessage>,
        ),
        BoxError,
    > {
        tracing::info!("Opening remote connection to {}", remote_name);

        let local_sock = {
            let map = self.remotes.lock().expect("bug: mutex poisoned");
            map.get(remote_name)
                .and_then(|s| s.local_sock.clone())
                .ok_or_else(|| format!("remote '{remote_name}' is not connected"))?
        };

        let stream = UnixStream::connect(&local_sock)
            .await
            .map_err(|e| format!("connect to tunnel failed: {e}"))?;
        let mut framed = Framed::new(stream, LengthDelimitedCodec::new());

        // Read and discard Welcome
        let first = framed
            .next()
            .await
            .ok_or("remote closed before Welcome")?
            .map_err(|e| format!("read error: {e}"))?;
        let _welcome: DaemonMessage =
            serde_json::from_slice(&first).map_err(|e| format!("invalid Welcome: {e}"))?;

        let (client_tx, mut client_rx) = mpsc::unbounded_channel::<ClientMessage>();
        let (daemon_tx, daemon_rx) = mpsc::unbounded_channel::<DaemonMessage>();

        tokio::task::spawn_local(async move {
            let (mut sink, mut stream) = framed.split();
            loop {
                tokio::select! {
                    msg = stream.next() => {
                        let Some(Ok(bytes)) = msg else { break };
                        let Ok(dm) = serde_json::from_slice::<DaemonMessage>(&bytes) else { continue };
                        if matches!(dm, DaemonMessage::Welcome { .. } | DaemonMessage::SessionsUpdated { .. } | DaemonMessage::RemotesUpdated { .. }) {
                            continue;
                        }
                        if daemon_tx.send(dm).is_err() { break; }
                    }
                    msg = client_rx.recv() => {
                        let Some(msg) = msg else { break };
                        let bytes = Bytes::from(serde_json::to_vec(&msg).expect("bug: serialization failed"));
                        if sink.send(bytes).await.is_err() { break; }
                    }
                }
            }
        });

        Ok((client_tx, daemon_rx))
    }

    /// Get the SSH args for a remote by name.
    pub fn get_ssh_args(&self, remote_name: &str) -> Option<Vec<String>> {
        let map = self.remotes.lock().expect("bug: mutex poisoned");
        map.get(remote_name).map(|s| s.ssh_args.clone())
    }

    /// Return the current status of all configured remotes.
    pub fn remote_info_list(&self) -> Vec<infinity_protocol::RemoteInfo> {
        build_remote_info_list(&self.remotes)
    }
}

/// SSH-forward a remote port to a local port.
/// Returns the local port and a guard that kills the SSH process on drop.
pub async fn ssh_forward_port(
    ssh_args: &[String],
    remote_port: u16,
) -> Result<(u16, SshPortForward), BoxError> {
    use std::process::Stdio;

    // Use -L 0:127.0.0.1:<remote_port> to get a dynamic local port
    let mut child = tokio::process::Command::new("ssh")
        .args(ssh_args)
        .arg("-L")
        .arg(format!("0:127.0.0.1:{remote_port}"))
        .arg("-N")
        .arg("-o")
        .arg("ExitOnForwardFailure=yes")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("ssh forward spawn failed: {e}"))?;

    // SSH with -L 0:... doesn't easily report the allocated port.
    // Instead, use a known local port by binding a listener first to find a free one.
    drop(child.stderr.take());

    // Simpler approach: use a temp unix socket like the control worker does.
    // Actually simplest: pick a free port ourselves.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(|e| format!("bind failed: {e}"))?;
    let local_port = listener.local_addr()?.port();
    drop(listener);

    // Kill the dynamic one and re-spawn with the known port
    let _ = child.kill().await;

    let child = tokio::process::Command::new("ssh")
        .args(ssh_args)
        .arg("-L")
        .arg(format!("{local_port}:127.0.0.1:{remote_port}"))
        .arg("-N")
        .arg("-o")
        .arg("ExitOnForwardFailure=yes")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| format!("ssh forward spawn failed: {e}"))?;

    // Wait briefly for the tunnel to establish
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    Ok((local_port, SshPortForward { _child: child }))
}

/// SSH-reverse-forward a local port so it's reachable on the remote host.
/// Returns the allocated port on the remote and a guard.
pub async fn ssh_reverse_forward_port(
    ssh_args: &[String],
    local_port: u16,
) -> Result<(u16, SshPortForward), BoxError> {
    use std::process::Stdio;

    tracing::info!("Reverse forwarding port from remote to localhost:{local_port}");

    // -R 0:127.0.0.1:<local_port> asks the remote to allocate a port.
    // We capture stderr to read the allocated port from OpenSSH's output.
    // However, OpenSSH only prints this with -v and it's unreliable.
    // Instead: pick a port on the remote by probing, or just use the same port number
    // and hope it's free. Safest: use a known free port by asking the remote.
    let remote_port = resolve_free_remote_port(ssh_args).await?;

    let child = tokio::process::Command::new("ssh")
        .args(ssh_args)
        .arg("-R")
        .arg(format!("{remote_port}:127.0.0.1:{local_port}"))
        .arg("-N")
        .arg("-o")
        .arg("ExitOnForwardFailure=yes")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| format!("ssh reverse forward spawn failed: {e}"))?;

    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    tracing::info!(
        "Successfully allocated remote port from remote:{remote_port} to localhost:{local_port}"
    );

    Ok((remote_port, SshPortForward { _child: child }))
}

/// Find a free port on the remote host.
async fn resolve_free_remote_port(ssh_args: &[String]) -> Result<u16, BoxError> {
    let output = tokio::process::Command::new("ssh")
        .args(ssh_args)
        .arg("--")
        .arg("python3 -c 'import socket; s=socket.socket(); s.bind((\"\",0)); print(s.getsockname()[1]); s.close()'")
        .output()
        .await
        .map_err(|e| format!("ssh failed: {e}"))?;
    if !output.status.success() {
        return Err("failed to find free remote port".into());
    }
    let port: u16 = String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse()
        .map_err(|e| format!("invalid port: {e}"))?;
    Ok(port)
}

/// Guard that keeps an SSH port-forward process alive.
pub struct SshPortForward {
    _child: Child,
}

/// Prefix session IDs in a sessions map with the remote name.
fn prefix_sessions(
    name: &str,
    sessions: HashMap<String, SessionInfo>,
) -> HashMap<String, SessionInfo> {
    sessions
        .into_iter()
        .map(|(id, mut info)| {
            info.remote = Some(name.to_owned());
            for t in &mut info.threads {
                t.thread_id = format!("{name}/{}", t.thread_id);
                t.parent_thread_id = format!("{name}/{}", t.parent_thread_id);
            }
            (format!("{name}/{id}"), info)
        })
        .collect()
}

/// An open SSH-forwarded connection: holds the SSH child process and temp
/// socket path so the tunnel stays alive and cleans up on drop.
struct SshTunnel {
    _child: Child,
    local_sock: std::path::PathBuf,
}

impl Drop for SshTunnel {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.local_sock);
    }
}

/// Resolve the remote daemon socket path by running `echo $HOME` over SSH.
async fn resolve_remote_sock(ssh_args: &[String]) -> Result<String, BoxError> {
    let output = tokio::process::Command::new("ssh")
        .args(ssh_args)
        .arg("--")
        .arg("echo $HOME/.infinity/daemon.sock")
        .output()
        .await
        .map_err(|e| format!("ssh spawn failed: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "ssh echo failed: {}",
            String::from_utf8_lossy(&output.stderr)
        )
        .into());
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

/// Open an SSH `-L` tunnel to the remote daemon's unix socket and connect.
async fn open_remote_connection(
    ssh_args: &[String],
) -> Result<(Framed<UnixStream, LengthDelimitedCodec>, SshTunnel), BoxError> {
    use std::process::Stdio;

    let remote_sock = resolve_remote_sock(ssh_args).await?;

    let local_sock =
        infinity_protocol::state_dir().join(format!("remote-{}.sock", uuid::Uuid::new_v4()));
    // Ensure no stale socket
    let _ = std::fs::remove_file(&local_sock);

    let child = tokio::process::Command::new("ssh")
        .args(ssh_args)
        .arg("-L")
        .arg(format!("{}:{}", local_sock.display(), remote_sock))
        .arg("-N")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| format!("ssh tunnel spawn failed: {e}"))?;

    // Wait for the local socket to appear (SSH needs a moment to bind it)
    for _ in 0..50 {
        if local_sock.exists() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }

    let stream = UnixStream::connect(&local_sock)
        .await
        .map_err(|e| format!("connect to SSH tunnel socket failed: {e}"))?;

    let tunnel = SshTunnel {
        _child: child,
        local_sock,
    };

    Ok((Framed::new(stream, LengthDelimitedCodec::new()), tunnel))
}

/// Long-running control worker for a single remote daemon.
/// Maintains the connection and directly broadcasts prefixed SessionsUpdated.
async fn control_worker(
    name: String,
    ssh_args: Vec<String>,
    remotes: RemoteMap,
    broadcast_clients: BroadcastClients,
) {
    loop {
        {
            let mut map = remotes.lock().expect("bug: mutex poisoned");
            if let Some(state) = map.get_mut(&name) {
                state.status = RemoteStatus::Connecting;
            }
            drop(map);
            broadcast(
                &broadcast_clients,
                DaemonMessage::RemotesUpdated {
                    remotes: build_remote_info_list(&remotes),
                },
            );
        }

        match control_worker_inner(&name, &ssh_args, &remotes, &broadcast_clients).await {
            Ok(()) => {
                tracing::info!("Remote '{}' control connection closed cleanly", name);
            }
            Err(e) => {
                tracing::warn!("Remote '{}' control connection failed: {e}", name);
                let mut map = remotes.lock().expect("bug: mutex poisoned");
                if let Some(state) = map.get_mut(&name) {
                    state.status = RemoteStatus::Disconnected(e.to_string());
                    state.sessions.clear();
                    state.local_sock = None;
                }
                drop(map);
                broadcast(
                    &broadcast_clients,
                    DaemonMessage::RemotesUpdated {
                        remotes: build_remote_info_list(&remotes),
                    },
                );
            }
        }

        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
    }
}

async fn control_worker_inner(
    name: &str,
    ssh_args: &[String],
    remotes: &RemoteMap,
    broadcast_clients: &BroadcastClients,
) -> Result<(), BoxError> {
    let (mut framed, _tunnel) = open_remote_connection(ssh_args).await?;

    tracing::info!("Established remote connection to {name}");

    // Store the tunnel's local socket path so connect_remote_session can reuse it.
    {
        let mut map = remotes.lock().expect("bug: mutex poisoned");
        if let Some(state) = map.get_mut(name) {
            state.local_sock = Some(_tunnel.local_sock.clone());
        }
    }

    // Read Welcome
    let first = framed
        .next()
        .await
        .ok_or("remote closed before Welcome")?
        .map_err(|e| format!("read error: {e}"))?;
    let welcome: DaemonMessage =
        serde_json::from_slice(&first).map_err(|e| format!("invalid Welcome: {e}"))?;

    let DaemonMessage::Welcome { sessions, .. } = welcome else {
        return Err("expected Welcome as first message".into());
    };

    tracing::debug!("Received remote sessions {:?}", &sessions);

    // Update state and broadcast prefixed sessions
    let prefixed = prefix_sessions(name, sessions.clone());
    {
        let mut map = remotes.lock().expect("bug: mutex poisoned");
        if let Some(state) = map.get_mut(name) {
            state.status = RemoteStatus::Connected;
            state.sessions = sessions;
        }
    }
    broadcast(
        broadcast_clients,
        DaemonMessage::SessionsUpdated { sessions: prefixed },
    );
    broadcast(
        broadcast_clients,
        DaemonMessage::RemotesUpdated {
            remotes: build_remote_info_list(remotes),
        },
    );
    tracing::info!("Remote '{}' connected", name);

    // Listen for SessionsUpdated
    while let Some(frame) = framed.next().await {
        let bytes = frame.map_err(|e| format!("read error: {e}"))?;
        let msg: DaemonMessage =
            serde_json::from_slice(&bytes).map_err(|e| format!("parse error: {e}"))?;

        if let DaemonMessage::SessionsUpdated { sessions } = msg {
            let prefixed = prefix_sessions(name, sessions.clone());
            let mut map = remotes.lock().expect("bug: mutex poisoned");
            if let Some(state) = map.get_mut(name) {
                for (id, info) in sessions {
                    state.sessions.insert(id, info);
                }
            }
            drop(map);
            broadcast(
                broadcast_clients,
                DaemonMessage::SessionsUpdated { sessions: prefixed },
            );
        }
    }

    Ok(())
}

fn build_remote_info_list(remotes: &RemoteMap) -> Vec<infinity_protocol::RemoteInfo> {
    let map = remotes.lock().expect("bug: mutex poisoned");
    map.iter()
        .map(|(name, state)| infinity_protocol::RemoteInfo {
            name: name.clone(),
            status: match &state.status {
                RemoteStatus::Connecting => "connecting".to_owned(),
                RemoteStatus::Connected => "connected".to_owned(),
                RemoteStatus::Disconnected(reason) => format!("disconnected: {reason}"),
            },
        })
        .collect()
}

fn broadcast(bc: &BroadcastClients, msg: DaemonMessage) {
    bc.lock()
        .expect("bug: mutex poisoned")
        .retain(|tx| tx.send(msg.clone()).is_ok());
}
