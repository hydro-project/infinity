use std::sync::Arc;

use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use infinity_agent_core::message::{
    InputMessage, InputMessageContent, SyntheticKind, TaggedSyntheticKind,
};
use infinity_protocol::{ClientMessage, DaemonMessage, length_delimited_codec};
use rig::message::UserContent;
use tokio::net::UnixStream;
use tokio::sync::{Mutex, mpsc};
use tokio_util::codec::Framed;

use crate::session::SessionManager;

/// List directory entries matching a partial path for tab-completion.
/// Given "/home/user/fo", lists entries in "/home/user/" that start with "fo".
/// Given "/home/user/", lists all entries in "/home/user/".
/// Directories get a trailing `/` in the result.
async fn list_directory_completions(input: &str) -> Vec<String> {
    use std::path::Path;

    let path = Path::new(input);
    let (dir, prefix) = if input.ends_with('/') {
        (path.to_owned(), "".to_owned())
    } else {
        (
            path.parent().unwrap_or(Path::new("/")).to_owned(),
            path.file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("")
                .to_owned(),
        )
    };

    let Ok(mut read_dir) = tokio::fs::read_dir(&dir).await else {
        return Vec::new();
    };

    let mut entries: Vec<String> = Vec::new();
    while let Ok(Some(e)) = read_dir.next_entry().await {
        let Some(name) = e.file_name().to_str().map(|s| s.to_owned()) else {
            continue;
        };
        if !name.starts_with(&*prefix) {
            continue;
        }
        let full = dir.join(&name);
        match tokio::fs::metadata(&full).await {
            Ok(m) if m.is_dir() => entries.push(format!("{}/", full.display())),
            _ => {} // only complete directories
        }
    }
    entries.sort_by(|a, b| a.trim_end_matches('/').cmp(b.trim_end_matches('/')));
    entries
}

/// Split "remote_name/real_id" into (remote_name, real_id).
fn is_remote_session(session_id: &str) -> Option<(&str, &str)> {
    session_id.split_once('/')
}

fn prefix_thread_id(thread_id: Option<String>, remote_name: &str) -> Option<String> {
    thread_id.map(|id| format!("{remote_name}/{id}"))
}

fn prefix_daemon_message(msg: DaemonMessage, remote_name: &str) -> DaemonMessage {
    match msg {
        DaemonMessage::Connected {
            session_id,
            thread_id,
            model_name,
            context_window,
            title,
            total_tokens_used,
            provider_id,
        } => DaemonMessage::Connected {
            session_id: format!("{remote_name}/{session_id}"),
            thread_id: format!("{remote_name}/{thread_id}"),
            model_name,
            context_window,
            title,
            total_tokens_used,
            provider_id,
        },
        DaemonMessage::StartOutput { thread_id } => DaemonMessage::StartOutput {
            thread_id: prefix_thread_id(thread_id, remote_name),
        },
        DaemonMessage::TextChunk { thread_id, chunk } => DaemonMessage::TextChunk {
            thread_id: prefix_thread_id(thread_id, remote_name),
            chunk,
        },
        DaemonMessage::ToolCall {
            name,
            args,
            thread_id,
            display_as,
        } => DaemonMessage::ToolCall {
            name,
            args,
            thread_id: prefix_thread_id(thread_id, remote_name),
            display_as,
        },
        DaemonMessage::ToolResult {
            segments,
            thread_id,
        } => DaemonMessage::ToolResult {
            segments,
            thread_id: prefix_thread_id(thread_id, remote_name),
        },
        DaemonMessage::Info { thread_id, text } => DaemonMessage::Info {
            thread_id: prefix_thread_id(thread_id, remote_name),
            text,
        },
        DaemonMessage::ResponseDone {
            thread_id,
            token_usage,
        } => DaemonMessage::ResponseDone {
            thread_id: prefix_thread_id(thread_id, remote_name),
            token_usage,
        },
        DaemonMessage::UserInputEcho { thread_id, text } => DaemonMessage::UserInputEcho {
            thread_id: prefix_thread_id(thread_id, remote_name),
            text,
        },
        DaemonMessage::SubscriptionEvent {
            name,
            text,
            thread_id,
        } => DaemonMessage::SubscriptionEvent {
            name,
            text,
            thread_id: prefix_thread_id(thread_id, remote_name),
        },
        DaemonMessage::OAuthRequired {
            thread_id,
            auth_url,
        } => DaemonMessage::OAuthRequired {
            thread_id: prefix_thread_id(thread_id, remote_name),
            auth_url,
        },
        DaemonMessage::UserChoiceRequired {
            thread_id,
            id,
            prompt,
            choices,
            default,
        } => DaemonMessage::UserChoiceRequired {
            thread_id: prefix_thread_id(thread_id, remote_name),
            id,
            prompt,
            choices,
            default,
        },
        DaemonMessage::ThinkingStart { thread_id } => DaemonMessage::ThinkingStart {
            thread_id: prefix_thread_id(thread_id, remote_name),
        },
        DaemonMessage::ThinkingEnd { thread_id } => DaemonMessage::ThinkingEnd {
            thread_id: prefix_thread_id(thread_id, remote_name),
        },
        DaemonMessage::ThinkingChunk { thread_id, chunk } => DaemonMessage::ThinkingChunk {
            thread_id: prefix_thread_id(thread_id, remote_name),
            chunk,
        },
        DaemonMessage::CompactionApplied { thread_id } => DaemonMessage::CompactionApplied {
            thread_id: prefix_thread_id(thread_id, remote_name),
        },
        DaemonMessage::Error { thread_id, text } => DaemonMessage::Error {
            thread_id: prefix_thread_id(thread_id, remote_name),
            text,
        },
        DaemonMessage::ViewUpdate {
            thread_id,
            view_type,
            content,
        } => DaemonMessage::ViewUpdate {
            thread_id: prefix_thread_id(thread_id, remote_name),
            view_type,
            content,
        },
        DaemonMessage::Replay {
            history,
            pending_choices,
            views,
        } => DaemonMessage::Replay {
            history: history
                .into_iter()
                .map(|m| prefix_daemon_message(m, remote_name))
                .collect(),
            pending_choices: pending_choices
                .into_iter()
                .map(|m| prefix_daemon_message(m, remote_name))
                .collect(),
            views,
        },
        other => other,
    }
}

fn strip_id(id: &str, remote_name: &str) -> String {
    id.strip_prefix(&format!("{remote_name}/"))
        .unwrap_or(id)
        .to_owned()
}

fn strip_client_message(msg: ClientMessage, remote_name: &str) -> ClientMessage {
    match msg {
        ClientMessage::UserInput { session_id, text } => ClientMessage::UserInput {
            session_id: strip_id(&session_id, remote_name),
            text,
        },
        ClientMessage::SoftDetach { session_id } => ClientMessage::SoftDetach {
            session_id: strip_id(&session_id, remote_name),
        },
        ClientMessage::ShutdownSession { session_id } => ClientMessage::ShutdownSession {
            session_id: strip_id(&session_id, remote_name),
        },
        ClientMessage::ArchiveSession { session_id } => ClientMessage::ArchiveSession {
            session_id: strip_id(&session_id, remote_name),
        },
        ClientMessage::SwitchModel { session_id, model } => ClientMessage::SwitchModel {
            session_id: strip_id(&session_id, remote_name),
            model,
        },
        ClientMessage::TriggerCompaction { session_id } => ClientMessage::TriggerCompaction {
            session_id: strip_id(&session_id, remote_name),
        },
        other => other,
    }
}

/// Handle a client over a unix socket (serialized framing).
pub async fn handle_client(stream: UnixStream, session_manager: Arc<Mutex<SessionManager>>) {
    let mut framed = Framed::new(stream, length_delimited_codec());
    let (client_msg_tx, client_msg_rx) = mpsc::unbounded_channel();
    let (daemon_msg_tx, mut daemon_msg_rx) = mpsc::unbounded_channel();

    // Spawn the core handler
    let mgr = session_manager.clone();
    tokio::pin! {
        let handler = handle_client_channels(client_msg_rx, daemon_msg_tx, mgr);
    }

    // Bridge: framed socket <-> channels
    loop {
        tokio::select! {
            msg = daemon_msg_rx.recv() => {
                let Some(msg) = msg else { // handle_client_channels has dropped the senders, so we disconnect
                    break
                };
                let bytes = Bytes::from(serde_json::to_vec(&msg).expect("bug: failed to serialize DaemonMessage"));

                if let Err(e) = framed.send(bytes).await {
                    tracing::error!("Failed to send daemon message to client: {e}");
                    break;
                }
            }
            _ = &mut handler => {
                return;
            }
            frame = framed.next() => {
                match frame {
                    Some(Ok(bytes)) => {
                        let Ok(msg) = serde_json::from_slice::<ClientMessage>(&bytes) else { continue };
                        client_msg_tx.send(msg).expect("bug: client message receiver dropped");
                    }
                    Some(Err(e)) => {
                        tracing::error!("Error reading from client socket: {e}");
                        break;
                    }
                    None => {
                        tracing::info!("Client closed the connection");
                        break;
                    }
                }
            }
        }
    }

    drop(client_msg_tx);

    handler.await;
}

/// Handle a client over raw mpsc channels (for in-memory mode, no serialization).
#[tracing::instrument(skip_all)]
pub async fn handle_client_channels(
    mut client_rx: mpsc::UnboundedReceiver<ClientMessage>,
    daemon_tx: mpsc::UnboundedSender<DaemonMessage>,
    session_manager: Arc<Mutex<SessionManager>>,
) {
    let (mut client_tx, mut client_tx_rx) = mpsc::unbounded_channel::<DaemonMessage>();
    let mut attached_session_id: Option<String> = None;
    let mut remote_proxy_tx: Option<mpsc::UnboundedSender<ClientMessage>> = None;
    let mut remote_proxy_rx: Option<mpsc::UnboundedReceiver<DaemonMessage>> = None;
    let mut active_remote_name: Option<String> = None;
    let mut _booted_rap_servers: Vec<tokio::process::Child> = Vec::new(); // prevents shutdown until close

    // Send Welcome immediately
    {
        let mgr = session_manager.lock().await;
        let remotes = mgr
            .remote_daemons
            .as_ref()
            .map(|rd| rd.remote_info_list())
            .unwrap_or_default();
        let default_entry = mgr.catalog.default_entry();
        let _ = daemon_tx.send(DaemonMessage::Welcome {
            sessions: mgr.list_sessions(Some(daemon_tx.clone())).await,
            available_models: mgr
                .catalog
                .models()
                .iter()
                .map(|m| infinity_protocol::ModelInfo {
                    display_name: m.entry.display_name.clone(),
                    provider_id: m.provider_id.clone(),
                    model_id: m.entry.model_id.clone(),
                    context_window: m.entry.context_window,
                })
                .collect(),
            default_model_name: default_entry.display_name.clone(),
            default_context_window: default_entry.context_window,
            provider_name: mgr.catalog.default_ref().provider_id.clone(),
            remotes,
        });
    }

    loop {
        tokio::select! {
            // Forward session display events to client
            msg = client_tx_rx.recv() => {
                let Some(msg) = msg else {
                    tracing::error!("Session display events dropped");
                    break
                };
                if daemon_tx.send(msg).is_err() {
                    tracing::error!("Failed to send display event to client");
                    break;
                }
            }
            // Forward remote proxy messages to client (prefixed)
            msg = async {
                match remote_proxy_rx.as_mut() {
                    Some(rx) => rx.recv().await,
                    None => std::future::pending().await,
                }
            } => {
                let Some(msg) = msg else {
                    let rn = active_remote_name.as_deref().unwrap_or("unknown");
                    let _ = daemon_tx.send(DaemonMessage::Error { thread_id: None, text: format!("Remote '{rn}' connection closed") });
                    break;
                };
                let rn = active_remote_name.as_deref().unwrap_or("");
                if daemon_tx.send(prefix_daemon_message(msg, rn)).is_err() { break; }
            }
            // Handle client messages
            msg = client_rx.recv() => {
                let Some(msg) = msg else {
                    tracing::info!("Client stream ended");
                    break
                };
                tracing::info!(?msg, "Received client message");

                // If connected to a remote session, forward most messages there
                if let Some(ref proxy_tx) = remote_proxy_tx {
                    let rn = active_remote_name.as_deref().unwrap_or("");
                    match &msg {
                        ClientMessage::Disconnect => {
                            // Disconnect from remote: drop proxy, also disconnect locally
                            let _ = proxy_tx.send(ClientMessage::Disconnect);
                            remote_proxy_tx = None;
                            remote_proxy_rx = None;
                            active_remote_name = None;
                            attached_session_id = None;
                        }
                        ClientMessage::Connect { .. } => {
                            // Switching sessions: tear down current remote proxy first
                            let _ = proxy_tx.send(ClientMessage::Disconnect);
                            remote_proxy_tx = None;
                            remote_proxy_rx = None;
                            active_remote_name = None;
                            // Fall through to handle the new Connect below
                        }
                        ClientMessage::CreateSession { .. } => {
                            // Tear down current remote proxy; fall through to handle below
                            let _ = proxy_tx.send(ClientMessage::Disconnect);
                            remote_proxy_tx = None;
                            remote_proxy_rx = None;
                            active_remote_name = None;
                        }
                        ClientMessage::RequestMigrate { .. } | ClientMessage::ListDirectory { .. } => {
                            // Always handled locally; fall through without tearing down proxy
                        }
                        _ => {
                            // Forward everything else to remote (stripped)
                            let _ = proxy_tx.send(strip_client_message(msg, rn));
                            continue;
                        }
                    }
                }

                    match msg {
                        ClientMessage::CreateSession { cwd, location, model } => {
                            if let Some(rname) = location {
                                let rd = {
                                    let mgr = session_manager.lock().await;
                                    mgr.remote_daemons.clone()
                                };
                                if let Some(rd) = rd {
                                    match rd.open_raw_connection(&rname).await {
                                        Ok((tx, rx)) => {
                                            let _ = tx.send(ClientMessage::CreateSession { cwd, location: None, model });
                                            remote_proxy_tx = Some(tx);
                                            remote_proxy_rx = Some(rx);
                                            active_remote_name = Some(rname);
                                            attached_session_id = None;
                                        }
                                        Err(e) => {
                                            let _ = daemon_tx.send(DaemonMessage::Error {
                                                thread_id: None,
                                                text: format!("failed to connect to remote: {e}"),
                                            });
                                        }
                                    }
                                } else {
                                    let _ = daemon_tx.send(DaemonMessage::Error {
                                        thread_id: None,
                                        text: "no remote daemons configured".into(),
                                    });
                                }
                            } else {
                                let mut mgr = session_manager.lock().await;
                                let mut emit = async |msg: DaemonMessage| {
                                    let _ = daemon_tx.send(msg);
                                };
                                let model = model.unwrap_or_else(|| mgr.catalog.default_ref().clone());
                                match mgr.create_session(&cwd, model, &mut emit).await {
                                    Ok(sid) => {
                                        mgr.attach_client(&sid, client_tx.clone(), false).await;
                                        attached_session_id = Some(sid);
                                    }
                                    Err(e) => { let _ = daemon_tx.send(DaemonMessage::Error { thread_id: None, text: format!("failed to create session: {e}") }); }
                                }
                            }
                        }
                    ClientMessage::Connect { session_id, thread_id } => {
                        if let Some((rname, real_session_id)) = is_remote_session(&session_id) {
                            let real_thread_id = thread_id.as_deref().map(|t| strip_id(t, rname));
                            let rname = rname.to_owned();
                            let rd = {
                                let mgr = session_manager.lock().await;
                                mgr.remote_daemons.clone()
                            };
                            if let Some(rd) = rd {
                                match rd.connect_remote_session(&rname, real_session_id, real_thread_id.as_deref()).await {
                                    Ok((tx, rx)) => {
                                        remote_proxy_tx = Some(tx);
                                        remote_proxy_rx = Some(rx);
                                        active_remote_name = Some(rname);
                                        attached_session_id = Some(session_id);
                                    }
                                    Err(e) => {
                                        let _ = daemon_tx.send(DaemonMessage::Error {
                                            thread_id: Some(session_id),
                                            text: format!("failed to connect to remote: {e}"),
                                        });
                                    }
                                }
                            } else {
                                let _ = daemon_tx.send(DaemonMessage::Error {
                                    thread_id: Some(session_id),
                                    text: "no remote daemons configured".into(),
                                });
                            }
                        } else {
                            let mut mgr = session_manager.lock().await;
                            let mut emit = async |msg: DaemonMessage| {
                                let _ = daemon_tx.send(msg);
                            };
                            let target = thread_id.as_deref().unwrap_or(&session_id);
                            match mgr.resume_session(&session_id, target, &mut emit).await {
                                Ok(()) => {
                                    mgr.attach_client(target, client_tx.clone(), true).await;
                                    attached_session_id = Some(session_id);
                                }
                                Err(e) => { let _ = daemon_tx.send(DaemonMessage::Error { thread_id: Some(session_id), text: format!("failed to resume session: {e}") }); }
                            }
                        }
                    }
                    ClientMessage::UserInput { session_id: thread_id, text } => {
                        if is_remote_session(&thread_id).is_some() {
                            let _ = daemon_tx.send(DaemonMessage::Error { thread_id: Some(thread_id), text: "remote is not connected".into() });
                            continue;
                        }
                        let mut mgr = session_manager.lock().await;
                        let mut emit = async |msg: DaemonMessage| {
                            let _ = daemon_tx.send(msg);
                        };
                        if !mgr.send_input(&thread_id, (InputMessage {
                            content: InputMessageContent::User(UserContent::text(&text)),
                            group_id: thread_id.clone(),
                            metadata: None,
                            synthetic: None,
                            display_as: None,
                            subscription: false,
                        }, None), Some(client_tx.clone()), &mut emit).await {
                            let _ = daemon_tx.send(DaemonMessage::Error { thread_id: Some(thread_id), text: "session not found".into() });
                        }
                    }
                    ClientMessage::Disconnect => {
                        let (new_tx, new_rx) = mpsc::unbounded_channel::<DaemonMessage>();
                        client_tx = new_tx;
                        client_tx_rx = new_rx;
                        if let Some(ref sid) = attached_session_id {
                            let mgr = session_manager.lock().await;
                            mgr.send_idle_ping(sid);
                        }
                        attached_session_id = None;
                    }
                    ClientMessage::SoftDetach { session_id } => {
                        let mgr = session_manager.lock().await;
                        let do_cleanup = mgr.is_session_idle(&session_id);
                        tracing::debug!(do_cleanup, "Handling SoftDetach");
                        if do_cleanup {
                            let (new_tx, new_rx) = mpsc::unbounded_channel::<DaemonMessage>();
                            client_tx = new_tx;
                            client_tx_rx = new_rx;
                            mgr.send_idle_ping(&session_id);
                            attached_session_id = None;
                            let _ = daemon_tx.send(DaemonMessage::DetachedIdle);
                        } else {
                            let _ = daemon_tx.send(DaemonMessage::DisconnectNotIdle);
                        }
                    }
                    ClientMessage::ShutdownSession { session_id } => {
                        let mut mgr = session_manager.lock().await;
                        mgr.cleanup_session(&session_id).await;
                        attached_session_id = None;
                    }
                    ClientMessage::ArchiveSession { session_id } => {
                        let mut mgr = session_manager.lock().await;
                        mgr.cleanup_session(&session_id).await;
                        let mut store = mgr.session_store.lock().await;
                        store.mark_archived(&session_id);
                        if let Err(e) = store.save() {
                            tracing::error!("Failed to save session store after archiving {session_id}: {e}");
                        }
                        attached_session_id = None;
                    }
                    ClientMessage::TriggerCompaction { session_id } => {
                        let mut mgr = session_manager.lock().await;
                        let mut emit = async |msg: DaemonMessage| {
                            let _ = daemon_tx.send(msg);
                        };
                        mgr.send_input(&session_id, (InputMessage {
                            content: InputMessageContent::User(UserContent::text("")),
                            group_id: session_id.clone(),
                            metadata: None,
                            synthetic: Some(SyntheticKind::Tagged(TaggedSyntheticKind::Compaction)),
                            display_as: None,
                            subscription: false,
                        }, None), Some(client_tx.clone()), &mut emit).await;
                    }
                    ClientMessage::SwitchModel { .. } => {
                        let _ = daemon_tx.send(DaemonMessage::Info { thread_id: None, text: "Model switching not yet implemented".into() });
                    }
                    ClientMessage::UserChoiceAnswered { choice_id, selected } => {
                        if let Some(ref sid) = attached_session_id {
                            let mgr = session_manager.lock().await;
                            if let Some(pending) = mgr.conversation_store().remove_pending_choice(sid, &choice_id) {
                                let url = pending.response_url;
                                let id = pending.id;
                                tokio::spawn(rap_protocol::log_panic("user_choice_post", async move {
                                    let client = reqwest::Client::new();
                                    let _ = client
                                        .post(&url)
                                        .json(&serde_json::json!({
                                            "id": id,
                                            "selected": selected
                                        }))
                                        .send()
                                        .await;
                                }));
                                mgr.broadcast(DaemonMessage::UserChoiceComplete { choice_id });
                            }
                        }
                    }
                    ClientMessage::RequestMigrate { session_id, to, dest_cwd } => {
                        let mgr = session_manager.clone();
                        let tx = daemon_tx.clone();
                        tokio::task::spawn_local(crate::migrate::orchestrate_migration(
                            session_id, to, dest_cwd, mgr, tx,
                        ));
                    }
                    ClientMessage::Emigrate { session_id, dest_rap_urls } => {
                        // Daemon-to-daemon: shut down session, migrate RAP servers, serialize, return data
                        let mgr = session_manager.clone();
                        match crate::migrate::handle_emigrate(&session_id, dest_rap_urls, &mgr).await {
                            Ok(data) => {
                                let _ = daemon_tx.send(DaemonMessage::EmigrateResult {
                                    session_id,
                                    session_data: data,
                                });
                            }
                            Err(e) => {
                                let _ = daemon_tx.send(DaemonMessage::Error {
                                    thread_id: None,
                                    text: e.to_string(),
                                });
                            }
                        }
                    }
                    ClientMessage::EmigrateDone { session_id } => {
                        // Daemon-to-daemon: immigration complete, archive local session
                        let mgr = session_manager.lock().await;
                        let mut store = mgr.session_store.lock().await;
                        store.mark_archived(&session_id);
                        let _ = store.save();
                    }
                    ClientMessage::ImportSession { session_id, cwd, session_data } => {
                        // Daemon-to-daemon: import a serialized session
                        let mgr = session_manager.lock().await;
                        match mgr.conversation_store().import_session(&session_data) {
                            Ok(()) => {
                                let mut store = mgr.session_store.lock().await;
                                store.create(&session_id, cwd);
                                store.mark_shut_down(&session_id);
                                let _ = store.save();
                                let _ = daemon_tx.send(DaemonMessage::ImportComplete { session_id });
                            }
                            Err(e) => {
                                let _ = daemon_tx.send(DaemonMessage::Error {
                                    thread_id: None,
                                    text: format!("import failed: {e}"),
                                });
                            }
                        }
                    }
                    ClientMessage::BootRapServers { cwd } => {
                        match crate::session::boot_rap_servers(&cwd, &mut |_text| async {}).await {
                            Ok(booted) => {
                                match crate::migrate::filter_migration_server_ports(&booted).await {
                                    Ok(server_ports) => {
                                        _booted_rap_servers = booted.spawned_servers;
                                        let _ = daemon_tx.send(DaemonMessage::RapServersBooted { server_ports });
                                    }
                                    Err(e) => {
                                        let _ = daemon_tx.send(DaemonMessage::Error {
                                            thread_id: None,
                                            text: format!("failed to identify RAP servers for migration: {e}"),
                                        });
                                    }
                                }
                            }
                            Err(e) => {
                                let _ = daemon_tx.send(DaemonMessage::Error {
                                    thread_id: None,
                                    text: format!("failed to boot RAP servers: {e}"),
                                });
                            }
                        }
                    }
                    ClientMessage::ListDirectory { path, on } => {
                        if let Some(ref remote_name) = on {
                            // Forward to the remote daemon
                            let rd = {
                                let mgr = session_manager.lock().await;
                                mgr.remote_daemons.clone()
                            };
                            if let Some(rd) = rd {
                                let remote_name = remote_name.clone();
                                let tx = daemon_tx.clone();
                                tokio::task::spawn_local(async move {
                                    match rd.open_raw_connection(&remote_name).await {
                                        Ok((remote_tx, mut remote_rx)) => {
                                            let _ = remote_tx.send(ClientMessage::ListDirectory {
                                                path: path.clone(),
                                                on: None,
                                            });
                                            // Wait for the DirectoryListing response
                                            while let Some(msg) = remote_rx.recv().await {
                                                if let DaemonMessage::DirectoryListing { request_path, entries, .. } = msg {
                                                    let _ = tx.send(DaemonMessage::DirectoryListing {
                                                        request_path,
                                                        entries,
                                                        on: Some(remote_name),
                                                    });
                                                    break;
                                                }
                                            }
                                        }
                                        Err(e) => {
                                            tracing::warn!("ListDirectory remote error: {e}");
                                            let _ = tx.send(DaemonMessage::DirectoryListing {
                                                request_path: path,
                                                entries: Vec::new(),
                                                on: Some(remote_name),
                                            });
                                        }
                                    }
                                });
                            } else {
                                let _ = daemon_tx.send(DaemonMessage::DirectoryListing {
                                    request_path: path,
                                    entries: Vec::new(),
                                    on,
                                });
                            }
                        } else {
                            let entries = list_directory_completions(&path).await;
                            let _ = daemon_tx.send(DaemonMessage::DirectoryListing {
                                request_path: path,
                                entries,
                                on: None,
                            });
                        }
                    }
                }
            }
        }
    }

    // Client stream ended — drop client_tx to invalidate subscriber senders,
    // then ping idle so the agent can shut down if it was already idle.
    // Also tear down any active remote proxy and kill booted RAP servers.
    drop(remote_proxy_tx);
    drop(client_tx);
    if let Some(ref sid) = attached_session_id
        && active_remote_name.is_none()
    {
        let mgr = session_manager.lock().await;
        mgr.send_idle_ping(sid);
    }
}
