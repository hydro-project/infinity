//! Session migration orchestrator.
//!
//! Handles migrating a session between local and remote daemons.
//! The local daemon always acts as the orchestrator.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use infinity_protocol::{ClientMessage, DaemonMessage};
use tokio::sync::Mutex;
use tokio::sync::mpsc::UnboundedSender;

use crate::remote::{RemoteDaemons, SshPortForward};
use crate::session::SessionManager;

type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// Run the full migration flow for a `RequestMigrate` message.
/// This is spawned as a task from the client handler.
pub async fn orchestrate_migration(
    session_id: String,
    to: String,
    dest_cwd: PathBuf,
    session_manager: Arc<Mutex<SessionManager>>,
    daemon_tx: tokio::sync::mpsc::UnboundedSender<DaemonMessage>,
) {
    let _ = daemon_tx.send(DaemonMessage::MigrateStarted {
        session_id: session_id.clone(),
    });

    let real_session_id = session_id
        .split_once('/')
        .map_or(session_id.as_str(), |(_, s)| s);
    let new_session_id = if to == "local" {
        real_session_id.to_string()
    } else {
        format!("{to}/{real_session_id}")
    };

    match run_migration(&session_id, &to, &dest_cwd, &session_manager).await {
        Ok(()) => {
            let _ = daemon_tx.send(DaemonMessage::MigrateComplete {
                session_id,
                new_session_id,
            });
        }
        Err(e) => {
            let _ = daemon_tx.send(DaemonMessage::MigrateError {
                session_id,
                error: e.to_string(),
            });
        }
    }
}

async fn run_migration(
    session_id: &str,
    to: &str,
    dest_cwd: &Path,
    session_manager: &Arc<Mutex<SessionManager>>,
) -> Result<(), BoxError> {
    let source_is_local = !session_id.contains('/');
    let (source_remote, real_session_id) = if source_is_local {
        (None, session_id.to_string())
    } else {
        let (r, s) = session_id
            .split_once('/')
            .ok_or("invalid remote session id")?;
        (Some(r.to_string()), s.to_string())
    };
    let dest_is_local = to == "local";

    // Shut down the source session first (kills agent + original RAP servers)
    let source_cwd = if source_is_local {
        let mut mgr = session_manager.lock().await;
        let cwd = mgr
            .session_store
            .lock()
            .await
            .get_cwd(&real_session_id)
            .clone();
        mgr.cleanup_session(&real_session_id).await;
        Some(cwd)
    } else {
        None
    };

    // Boot destination RAP servers to discover which need migration.
    // The handler filters server_ports to only migration-needing servers.
    let (dest_rap_urls, dest_spawned, _tunnel_guards) =
        boot_dest_and_tunnel(to, dest_cwd, &source_remote, session_manager).await?;

    // Boot source RAP servers and migrate state
    if source_is_local && !dest_rap_urls.is_empty() {
        let (migration_servers, _spawned) = boot_source_rap_servers(
            source_cwd.as_deref().expect("bug: source local but no cwd"),
            &dest_rap_urls,
        )
        .await?;
        let notifier = rap_client::notifier::RapNotifier::new(
            migration_servers
                .iter()
                .map(|(_, url)| url.clone())
                .collect(),
            crate::rap_tools::SimpleHttpClient::new(),
        );
        if let Err(errors) = notifier
            .request_migration(&real_session_id, &migration_servers, &dest_rap_urls)
            .await
        {
            return Err(format!("RAP migration failed: {}", errors.join("; ")).into());
        }
    }

    // Serialize session data from source
    let session_data = if source_is_local {
        let mgr = session_manager.lock().await;
        mgr.conversation_store().serialize_session(&real_session_id)
    } else {
        let rd = {
            let mgr = session_manager.lock().await;
            mgr.remote_daemons.clone()
        };
        let rd = rd.ok_or("no remote daemons configured")?;

        emigrate_from_remote(
            &rd,
            source_remote
                .as_deref()
                .expect("bug: source remote but no name"),
            &real_session_id,
            dest_rap_urls,
        )
        .await?
    };

    // Kill temporary destination RAP servers
    drop(dest_spawned);

    // Import into destination
    if dest_is_local {
        let mgr = session_manager.lock().await;
        mgr.conversation_store()
            .import_session(&session_data)
            .map_err(|e| format!("import failed: {e}"))?;
        let mut store = mgr.session_store.lock().await;
        store.create(&real_session_id, dest_cwd.to_path_buf());
        store.mark_shut_down(&real_session_id);
        store.save()?;
    } else {
        let rd = {
            let mgr = session_manager.lock().await;
            mgr.remote_daemons.clone()
        };
        let rd = rd.ok_or("no remote daemons configured")?;
        immigrate_to_remote(&rd, to, &real_session_id, dest_cwd, &session_data).await?;
    }

    // Clean up source
    if source_is_local {
        let mgr = session_manager.lock().await;
        let mut store = mgr.session_store.lock().await;
        store.mark_archived(&real_session_id);
        store.save()?;
    } else {
        let rd = {
            let mgr = session_manager.lock().await;
            mgr.remote_daemons.clone()
        };

        if let Some(rd) = rd {
            send_emigrate_done(
                &rd,
                source_remote
                    .as_deref()
                    .expect("bug: source remote but no name"),
                &real_session_id,
            )
            .await?;
        }
    }

    // _tunnel_guards dropped here, cleaning up SSH forwards
    Ok(())
}

/// Boot RAP servers at `cwd` and return (config_id, url) pairs for the given migration config_ids.
/// Caller must kill spawned_servers when done.
async fn boot_source_rap_servers(
    cwd: &Path,
    dest_rap_urls: &HashMap<String, String>,
) -> Result<(Vec<(String, String)>, Vec<tokio::process::Child>), BoxError> {
    let booted = crate::session::boot_rap_servers(cwd, &mut |_text| async {}).await?;
    let migration_servers: Vec<(String, String)> = dest_rap_urls
        .keys()
        .filter_map(|id| {
            booted
                .server_ids
                .iter()
                .find(|(_, sid)| *sid == id)
                .map(|(url, _)| (id.clone(), url.clone()))
        })
        .collect();
    Ok((migration_servers, booted.spawned_servers))
}

/// Filter BootedRapServers.server_ports to only servers that declare needsMigration.
pub async fn filter_migration_server_ports(
    booted: &crate::session::BootedRapServers,
) -> Result<HashMap<String, u16>, BoxError> {
    let servers_with_ids: Vec<(String, Option<String>)> = booted
        .urls
        .iter()
        .map(|u| (u.clone(), booted.server_ids.get(u).cloned()))
        .collect();
    let loaded = crate::rap_tools::load_rap_tools::<crate::memory_store::InMemoryMessageSender>(
        &servers_with_ids,
    )
    .await?;
    let migration_ids: std::collections::HashSet<String> = loaded
        .migration_servers
        .into_iter()
        .map(|(id, _)| id)
        .collect();

    tracing::info!(
        "Identified servers that require migration {:?}",
        migration_ids
    );

    Ok(booted
        .server_ports
        .iter()
        .filter(|(id, _)| migration_ids.contains(*id))
        .map(|(id, port)| (id.clone(), *port))
        .collect())
}

/// Handle an Emigrate request: shut down session, boot fresh RAP servers,
/// migrate state, serialize, and return the session data.
pub async fn handle_emigrate(
    session_id: &str,
    dest_rap_urls: HashMap<String, String>,
    session_manager: &Arc<Mutex<SessionManager>>,
) -> Result<String, BoxError> {
    // Get cwd and shut down the session first
    let cwd = {
        let mgr = session_manager.lock().await;
        mgr.session_store.lock().await.get_cwd(session_id).clone()
    };
    {
        let mut mgr = session_manager.lock().await;
        mgr.cleanup_session(session_id).await;
    }

    // Boot fresh RAP servers and perform migration
    let (migration_servers, _spawned) = boot_source_rap_servers(&cwd, &dest_rap_urls).await?;
    if !migration_servers.is_empty() {
        let notifier = rap_client::notifier::RapNotifier::new(
            migration_servers
                .iter()
                .map(|(_, url)| url.clone())
                .collect(),
            crate::rap_tools::SimpleHttpClient::new(),
        );
        if let Err(errors) = notifier
            .request_migration(session_id, &migration_servers, &dest_rap_urls)
            .await
        {
            return Err(format!("RAP migration failed: {}", errors.join("; ")).into());
        }
    }

    let mgr = session_manager.lock().await;
    Ok(mgr.conversation_store().serialize_session(session_id))
}

pub enum LocalOrRemoteSpawned {
    Local(Vec<tokio::process::Child>),
    Remote(UnboundedSender<ClientMessage>),
}

/// Boot RAP servers on the destination and set up SSH tunnels so the source can reach them.
/// Returns (config_id → reachable_url, tunnel guards, spawned dest servers).
async fn boot_dest_and_tunnel(
    to: &str,
    dest_cwd: &Path,
    source_remote: &Option<String>,
    session_manager: &Arc<Mutex<SessionManager>>,
) -> Result<
    (
        HashMap<String, String>,
        LocalOrRemoteSpawned,
        Vec<SshPortForward>,
    ),
    BoxError,
> {
    let dest_is_local = to == "local";
    // Boot RAP servers on destination — server_ports is config_id → port (migration-only)
    let (dest_server_ports, dest_spawned) = if dest_is_local {
        let booted = crate::session::boot_rap_servers(dest_cwd, &mut |_text| async {}).await?;
        let ports = filter_migration_server_ports(&booted).await?;
        (ports, LocalOrRemoteSpawned::Local(booted.spawned_servers))
    } else {
        tracing::info!("Launching RAP servers on remote destination to receive migration");
        let rd = {
            let mgr = session_manager.lock().await;
            mgr.remote_daemons.clone()
        };
        let rd = rd.ok_or("no remote daemons configured")?;
        let (tx, mut rx) = rd.open_raw_connection(to).await?;
        tx.send(ClientMessage::BootRapServers {
            cwd: dest_cwd.to_path_buf(),
        })?;
        let ports = match rx.recv().await {
            Some(DaemonMessage::RapServersBooted { server_ports }) => server_ports,
            Some(msg) => return Err(format!("remote sent unxpected response {:?}", msg).into()),
            None => return Err("remote closed without sending EmigrateResult".into()),
        };

        (ports, LocalOrRemoteSpawned::Remote(tx))
    };

    // Build dest_rap_urls: config_id → reachable URL
    let mut dest_rap_urls = HashMap::new();

    let source_is_local = source_remote.is_none();

    let mut tunnel_guards = Vec::new();

    if dest_is_local && source_is_local {
        // Both local — dest servers are directly reachable
        for (id, port) in &dest_server_ports {
            dest_rap_urls.insert(id.clone(), format!("http://127.0.0.1:{port}"));
        }
    } else if dest_is_local {
        // Source is remote, dest is local — reverse-forward local dest ports onto the source remote
        let rd = {
            let mgr = session_manager.lock().await;
            mgr.remote_daemons.clone()
        };
        let rd = rd.ok_or("no remote daemons configured")?;
        let source_remote_name = source_remote
            .as_deref()
            .expect("bug: source remote but no name");
        let ssh_args = rd
            .get_ssh_args(source_remote_name)
            .ok_or("unknown source remote")?;
        for (id, local_port) in &dest_server_ports {
            let (remote_port, guard) =
                crate::remote::ssh_reverse_forward_port(&ssh_args, *local_port).await?;
            dest_rap_urls.insert(id.clone(), format!("http://127.0.0.1:{remote_port}"));
            tunnel_guards.push(guard);
        }
    } else if source_is_local {
        // Source is local, dest is remote — SSH-forward remote dest ports to local
        let rd = {
            let mgr = session_manager.lock().await;
            mgr.remote_daemons.clone()
        };
        let rd = rd.ok_or("no remote daemons configured")?;
        let ssh_args = rd.get_ssh_args(to).ok_or("unknown remote")?;
        for (id, remote_port) in &dest_server_ports {
            let (local_port, guard) =
                crate::remote::ssh_forward_port(&ssh_args, *remote_port).await?;
            dest_rap_urls.insert(id.clone(), format!("http://127.0.0.1:{local_port}"));
            tunnel_guards.push(guard);
        }
    } else {
        // Both remote — chain: forward dest ports to local, then reverse-forward to source remote
        let rd = {
            let mgr = session_manager.lock().await;
            mgr.remote_daemons.clone()
        };
        let rd = rd.ok_or("no remote daemons configured")?;
        let dest_ssh_args = rd.get_ssh_args(to).ok_or("unknown dest remote")?;
        let source_remote_name = source_remote
            .as_deref()
            .expect("bug: source remote but no name");
        let source_ssh_args = rd
            .get_ssh_args(source_remote_name)
            .ok_or("unknown source remote")?;
        for (id, dest_port) in &dest_server_ports {
            // Step 1: forward dest remote port to local
            let (local_port, guard1) =
                crate::remote::ssh_forward_port(&dest_ssh_args, *dest_port).await?;
            // Step 2: reverse-forward that local port onto source remote
            let (source_port, guard2) =
                crate::remote::ssh_reverse_forward_port(&source_ssh_args, local_port).await?;
            dest_rap_urls.insert(id.clone(), format!("http://127.0.0.1:{source_port}"));
            tunnel_guards.push(guard1);
            tunnel_guards.push(guard2);
        }
    }

    Ok((dest_rap_urls, dest_spawned, tunnel_guards))
}

/// Send Emigrate to a remote daemon and receive the serialized session data.
async fn emigrate_from_remote(
    rd: &RemoteDaemons,
    remote_name: &str,
    session_id: &str,
    dest_rap_urls: HashMap<String, String>,
) -> Result<String, BoxError> {
    let (tx, mut rx) = rd.open_raw_connection(remote_name).await?;

    tx.send(ClientMessage::Emigrate {
        session_id: session_id.to_string(),
        dest_rap_urls,
    })?;

    match rx.recv().await {
        Some(DaemonMessage::EmigrateResult { session_data, .. }) => Ok(session_data),
        Some(msg) => Err(format!("remote sent unxpected response {:?}", msg).into()),
        None => Err("remote closed without sending EmigrateResult".into()),
    }
}

/// Send session data to a remote daemon for immigration.
async fn immigrate_to_remote(
    rd: &RemoteDaemons,
    remote_name: &str,
    session_id: &str,
    dest_cwd: &Path,
    session_data: &str,
) -> Result<(), BoxError> {
    let (tx, mut rx) = rd.open_raw_connection(remote_name).await?;

    tx.send(ClientMessage::ImportSession {
        session_id: session_id.to_string(),
        cwd: dest_cwd.to_path_buf(),
        session_data: session_data.to_string(),
    })?;

    match rx.recv().await {
        Some(DaemonMessage::ImportComplete { .. }) => Ok(()),
        Some(msg) => Err(format!("remote sent unxpected response {:?}", msg).into()),
        None => Err("remote closed without sending EmigrateResult".into()),
    }
}

/// Send EmigrateDone to a remote daemon so it can clean up.
async fn send_emigrate_done(
    rd: &RemoteDaemons,
    remote_name: &str,
    session_id: &str,
) -> Result<(), BoxError> {
    let (tx, _rx) = rd.open_raw_connection(remote_name).await?;
    tx.send(ClientMessage::EmigrateDone {
        session_id: session_id.to_string(),
    })
    .map_err(|e| e.into())
}
