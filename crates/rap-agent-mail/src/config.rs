//! Mailbox configuration: its own host name and the peers it dials.
//!
//! Peers come from the daemon's `~/.infinity/remotes.json` (each remote is
//! dialed over SSH, reaching `<remote $HOME>/.infinity/mail/peer.sock`) plus
//! optional entries in `<state_dir>/config.json`:
//!
//! ```json
//! {
//!   "name": "laptop",
//!   "peers": [
//!     { "name": "devbox", "ssh_args": ["devbox"], "remote_socket": "/home/me/.infinity/mail/peer.sock" },
//!     { "name": "other-daemon", "socket": "/home/me/other/.infinity/mail/peer.sock" }
//!   ]
//! }
//! ```
//!
//! A `config.json` peer replaces a `remotes.json` remote of the same name.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::address::{default_host_name, validate_name};

/// How to reach a peer mailbox.
#[derive(Debug, Clone)]
pub enum PeerTransport {
    /// Forward a local socket to the peer's socket over SSH.
    Ssh {
        ssh_args: Vec<String>,
        /// The peer's socket path on the remote host. `None` resolves
        /// `$HOME/.infinity/mail/peer.sock` on the remote.
        remote_socket: Option<String>,
    },
    /// Connect to a peer socket on this machine.
    Socket(PathBuf),
}

#[derive(Debug, Clone)]
pub struct MailConfig {
    /// This mailbox's host name, announced to peers that this mailbox dials.
    pub name: String,
    /// Directory for durable state, sockets, and logs.
    pub state_dir: PathBuf,
    /// Peers this mailbox dials, by the name it calls them.
    pub peers: BTreeMap<String, PeerTransport>,
    /// The callback URL of the runtime that owns this (daemon-lifetime)
    /// mailbox. Deferred callbacks (events, ask results) go here instead of
    /// the URL recorded at invocation time, which goes stale when the daemon
    /// restarts on a new port.
    pub callback_override: Option<String>,
    /// How long a send waits for the peer's acknowledgement before telling
    /// the agent the message is queued.
    pub ack_wait: std::time::Duration,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileConfig {
    name: Option<String>,
    #[serde(default)]
    peers: Vec<FilePeer>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FilePeer {
    name: String,
    ssh_args: Option<Vec<String>>,
    remote_socket: Option<String>,
    socket: Option<PathBuf>,
}

#[derive(Debug, Deserialize)]
struct Remote {
    name: String,
    ssh_args: Vec<String>,
}

impl MailConfig {
    /// Load `<state_dir>/config.json` (optional) and `remotes_path`
    /// (optional, the daemon's `remotes.json`).
    pub fn load(
        state_dir: PathBuf,
        remotes_path: Option<&Path>,
        callback_override: Option<String>,
    ) -> Result<Self, String> {
        let config_path = state_dir.join("config.json");
        let file: FileConfig = match std::fs::read_to_string(&config_path) {
            Ok(s) => serde_json::from_str(&s)
                .map_err(|e| format!("invalid {}: {e}", config_path.display()))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => FileConfig::default(),
            Err(e) => return Err(format!("failed to read {}: {e}", config_path.display())),
        };

        let name = file.name.unwrap_or_else(default_host_name);
        validate_name("host", &name)?;

        let mut peers = BTreeMap::new();
        if let Some(path) = remotes_path {
            match std::fs::read_to_string(path) {
                Ok(s) => {
                    let remotes: Vec<Remote> = serde_json::from_str(&s)
                        .map_err(|e| format!("invalid {}: {e}", path.display()))?;
                    for remote in remotes {
                        // The daemon allows remote names the mailbox cannot
                        // use; skip those rather than refuse to start.
                        if let Err(e) = validate_name("host", &remote.name) {
                            tracing::warn!("skipping remote {:?}: {e}", remote.name);
                            continue;
                        }
                        if remote.name == name {
                            tracing::warn!(
                                "skipping remote {:?}: it has this mailbox's own name; set a \
                                 distinct \"name\" in {}",
                                remote.name,
                                config_path.display()
                            );
                            continue;
                        }
                        peers.insert(
                            remote.name,
                            PeerTransport::Ssh {
                                ssh_args: remote.ssh_args,
                                remote_socket: None,
                            },
                        );
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(format!("failed to read {}: {e}", path.display())),
            }
        }
        for peer in file.peers {
            let transport = match (peer.ssh_args, peer.socket) {
                (Some(ssh_args), None) => PeerTransport::Ssh {
                    ssh_args,
                    remote_socket: peer.remote_socket,
                },
                (None, Some(socket)) if peer.remote_socket.is_none() => {
                    PeerTransport::Socket(socket)
                }
                _ => {
                    return Err(format!(
                        "peer {:?} in {}: set either `ssh_args` (optionally with \
                         `remote_socket`) or `socket`",
                        peer.name,
                        config_path.display()
                    ));
                }
            };
            peers.insert(peer.name, transport);
        }

        for peer in peers.keys() {
            validate_name("host", peer)?;
            if *peer == name {
                return Err(format!(
                    "peer {peer:?} has this mailbox's own name; set a distinct \"name\" in {}",
                    config_path.display()
                ));
            }
        }

        Ok(Self {
            name,
            state_dir,
            peers,
            callback_override,
            ack_wait: std::time::Duration::from_secs(15),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_peers_override_remotes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let remotes = dir.path().join("remotes.json");
        std::fs::write(
            &remotes,
            r#"[{"name":"devbox","ssh_args":["devbox"]},{"name":"gpu","ssh_args":["-J","b","gpu"]}]"#,
        )
        .expect("write remotes");
        std::fs::write(
            dir.path().join("config.json"),
            r#"{"name":"laptop","peers":[{"name":"devbox","socket":"/tmp/x.sock"}]}"#,
        )
        .expect("write config");

        let config = MailConfig::load(dir.path().to_owned(), Some(&remotes), None)
            .expect("config should load");
        assert_eq!(config.name, "laptop");
        assert!(matches!(config.peers["devbox"], PeerTransport::Socket(_)));
        assert!(matches!(config.peers["gpu"], PeerTransport::Ssh { .. }));
    }

    #[test]
    fn rejects_peer_named_like_self_and_ambiguous_transport() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            dir.path().join("config.json"),
            r#"{"name":"laptop","peers":[{"name":"laptop","socket":"/tmp/x.sock"}]}"#,
        )
        .expect("write config");
        assert!(MailConfig::load(dir.path().to_owned(), None, None).is_err());

        std::fs::write(
            dir.path().join("config.json"),
            r#"{"peers":[{"name":"x","socket":"/tmp/x.sock","ssh_args":["x"]}]}"#,
        )
        .expect("write config");
        assert!(MailConfig::load(dir.path().to_owned(), None, None).is_err());
    }
}
