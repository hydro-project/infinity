//! Daemon I/O: manages per-thread connections to the Infinity daemon.
//!
//! Inbound: [`DaemonEvent`]s — `(thread_ts, DaemonMessage)` from all active
//! connections. Outbound: [`DaemonCommand`]s — instructions to
//! create/connect/send on daemon connections.

use std::collections::HashMap;

use infinity_protocol::DaemonMessage;
use infinity_slack_dataflow::daemon::{DaemonCommand, DaemonEvent};
use tokio::sync::mpsc;

use crate::daemon_client::DaemonClient;

/// Endpoints and task handle returned by [`spawn`].
pub struct DaemonIo {
    /// Daemon messages from all active connections; the bot wraps this in a
    /// stream and feeds it to the embedded dataflow.
    pub events: mpsc::Receiver<DaemonEvent>,
    /// Commands produced by the dataflow; the dispatch task acts on them. The
    /// channel is unbounded because the dataflow emits commands from a
    /// synchronous callback during a tick (it cannot await backpressure), and
    /// command volume is bounded by human-scale chat traffic.
    pub commands: mpsc::UnboundedSender<DaemonCommand>,
    /// Handle of the command-dispatch task. This task never finishes during
    /// normal operation (the command channel stays open for the bot's
    /// lifetime); completion means a panic or bug and should be treated as
    /// fatal by the caller.
    pub dispatch: tokio::task::JoinHandle<()>,
}

/// Spawns the daemon I/O task and returns the dataflow-facing endpoints.
pub fn spawn() -> DaemonIo {
    let (to_df_tx, to_df_rx) = mpsc::channel::<DaemonEvent>(1024);
    let (from_df_tx, mut from_df_rx) = mpsc::unbounded_channel::<DaemonCommand>();

    let dispatch = tokio::spawn(async move {
        // Map of thread_ts → sender half of the daemon client.
        // Each connection's receiver is forwarded to `to_df_tx` by a spawned task.
        let mut connections: HashMap<String, DaemonClient> = HashMap::new();

        while let Some(cmd) = from_df_rx.recv().await {
            tracing::info!("daemon I/O task received command: {cmd:?}");
            match cmd {
                DaemonCommand::CreateSession {
                    thread_ts,
                    cwd,
                    model,
                } => match DaemonClient::connect().await {
                    Ok(daemon) => {
                        if let Err(e) = daemon.create_session(cwd, model).await {
                            tracing::error!("CreateSession failed for {thread_ts}: {e}");
                            continue;
                        }
                        spawn_receiver(
                            ConnKey::root(thread_ts.clone()),
                            &mut connections,
                            daemon,
                            &to_df_tx,
                        );
                    }
                    Err(e) => {
                        tracing::error!("daemon connect failed for {thread_ts}: {e}");
                    }
                },
                DaemonCommand::ConnectSession {
                    thread_ts,
                    session_id,
                    target_thread,
                } => {
                    let subagent = target_thread.as_ref().map(|t| t.to_string());
                    let map_key =
                        infinity_slack_dataflow::slack::stream_key(&thread_ts, subagent.as_deref());
                    if connections.contains_key(&map_key) {
                        tracing::debug!("connection {map_key} already exists, skipping connect");
                        continue;
                    }
                    match DaemonClient::connect().await {
                        Ok(daemon) => {
                            if let Err(e) = daemon
                                .connect_session(&session_id, target_thread.clone())
                                .await
                            {
                                tracing::error!("ConnectSession failed for {thread_ts}: {e}");
                                continue;
                            }
                            spawn_receiver(
                                ConnKey {
                                    map_key,
                                    thread_ts,
                                    subagent,
                                },
                                &mut connections,
                                daemon,
                                &to_df_tx,
                            );
                        }
                        Err(e) => {
                            tracing::error!("daemon connect failed for {thread_ts}: {e}");
                        }
                    }
                }
                DaemonCommand::SendInput {
                    thread_ts,
                    session_id,
                    target_thread,
                    text,
                } => {
                    // The agent thread the input is addressed to: a subagent
                    // thread when the user replied inside a dedicated
                    // subagent Slack thread, the session root otherwise.
                    let input_thread = target_thread.as_ref().unwrap_or(&session_id);
                    if !connections.contains_key(&thread_ts) {
                        // Reconnect after restart: establish a new daemon connection
                        // and attach to the existing session.
                        match DaemonClient::connect().await {
                            Ok(daemon) => {
                                if let Err(e) = daemon.connect_session(&session_id, None).await {
                                    tracing::error!(
                                        "reconnect to session failed for {thread_ts}: {e}"
                                    );
                                    continue;
                                }
                                spawn_receiver(
                                    ConnKey::root(thread_ts.clone()),
                                    &mut connections,
                                    daemon,
                                    &to_df_tx,
                                );
                            }
                            Err(e) => {
                                tracing::error!("daemon connect failed for {thread_ts}: {e}");
                                continue;
                            }
                        }
                    }
                    if let Some(daemon) = connections.get(&thread_ts) {
                        if let Err(e) = daemon.send_input(input_thread, &text).await {
                            tracing::warn!("SendInput failed for {thread_ts}: {e}, reconnecting");
                            connections.remove(&thread_ts);
                            // Reconnect and retry.
                            match DaemonClient::connect().await {
                                Ok(daemon) => {
                                    if let Err(e) = daemon.connect_session(&session_id, None).await
                                    {
                                        tracing::error!(
                                            "reconnect to session failed for {thread_ts}: {e}"
                                        );
                                        continue;
                                    }
                                    let retry_result = daemon.send_input(input_thread, &text).await;
                                    spawn_receiver(
                                        ConnKey::root(thread_ts.clone()),
                                        &mut connections,
                                        daemon,
                                        &to_df_tx,
                                    );
                                    if let Err(e) = retry_result {
                                        tracing::error!(
                                            "SendInput retry failed for {thread_ts}: {e}"
                                        );
                                    }
                                }
                                Err(e) => {
                                    tracing::error!("daemon reconnect failed for {thread_ts}: {e}");
                                }
                            }
                        }
                    }
                }
                DaemonCommand::AnswerChoice {
                    thread_ts,
                    choice_id,
                    selected,
                } => {
                    if let Some(daemon) = connections.get(&thread_ts) {
                        if let Err(e) = daemon.answer_choice(&choice_id, selected).await {
                            tracing::error!("AnswerChoice failed for {thread_ts}: {e}");
                        }
                    } else {
                        tracing::warn!("AnswerChoice: no connection for thread {thread_ts}");
                    }
                }
                DaemonCommand::Interrupt {
                    thread_ts,
                    session_id,
                    target_thread,
                } => {
                    // The agent thread to interrupt: a subagent thread when
                    // the user stopped a dedicated subagent Slack thread, the
                    // session root (whole session) otherwise.
                    let interrupt_thread = target_thread.as_ref().unwrap_or(&session_id);
                    if !connections.contains_key(&thread_ts) {
                        // Reconnect after restart so the stop still lands.
                        match DaemonClient::connect().await {
                            Ok(daemon) => {
                                if let Err(e) = daemon.connect_session(&session_id, None).await {
                                    tracing::error!(
                                        "reconnect to session failed for {thread_ts}: {e}"
                                    );
                                    continue;
                                }
                                spawn_receiver(
                                    ConnKey::root(thread_ts.clone()),
                                    &mut connections,
                                    daemon,
                                    &to_df_tx,
                                );
                            }
                            Err(e) => {
                                tracing::error!("daemon connect failed for {thread_ts}: {e}");
                                continue;
                            }
                        }
                    }
                    if let Some(daemon) = connections.get(&thread_ts) {
                        if let Err(e) = daemon.interrupt(interrupt_thread).await {
                            tracing::error!("Interrupt failed for {thread_ts}: {e}");
                        }
                    }
                }
                DaemonCommand::Rewind {
                    thread_ts,
                    session_id,
                    user_turn,
                } => {
                    if !connections.contains_key(&thread_ts) {
                        // Reconnect after restart so the rewind still lands
                        // (and the SendInput that follows it rides the same
                        // ordered connection).
                        match DaemonClient::connect().await {
                            Ok(daemon) => {
                                if let Err(e) = daemon.connect_session(&session_id, None).await {
                                    tracing::error!(
                                        "reconnect to session failed for {thread_ts}: {e}"
                                    );
                                    continue;
                                }
                                spawn_receiver(
                                    ConnKey::root(thread_ts.clone()),
                                    &mut connections,
                                    daemon,
                                    &to_df_tx,
                                );
                            }
                            Err(e) => {
                                tracing::error!("daemon connect failed for {thread_ts}: {e}");
                                continue;
                            }
                        }
                    }
                    if let Some(daemon) = connections.get(&thread_ts) {
                        if let Err(e) = daemon.rewind(&session_id, user_turn).await {
                            tracing::error!("Rewind failed for {thread_ts}: {e}");
                        }
                    }
                }
            }
        }
    });

    DaemonIo {
        events: to_df_rx,
        commands: from_df_tx,
        dispatch,
    }
}

/// Identifies what a daemon connection serves: its key in the connections
/// map, the Slack thread its events are tagged with, and — for a subagent
/// resume connection — the subagent whose stream it carries.
struct ConnKey {
    /// Key in the connections map. The root connection of a Slack thread is
    /// keyed by the bare `thread_ts`; a subagent resume connection by
    /// `slack::stream_key(thread_ts, subagent)` so it can coexist with the
    /// root connection.
    map_key: String,
    /// The Slack thread `DaemonEvent`s from this connection are tagged with
    /// (always the root conversation thread; the dataflow routes subagent
    /// events by their `thread_id`).
    thread_ts: String,
    /// `Some` for a subagent resume connection (`ThreadRef` string form).
    subagent: Option<String>,
}

impl ConnKey {
    /// The root connection for a Slack thread.
    fn root(thread_ts: String) -> Self {
        Self {
            map_key: thread_ts.clone(),
            thread_ts,
            subagent: None,
        }
    }
}

/// Spawn a task that forwards DaemonMessages from a connection into the dataflow.
/// On `Connected`, automatically sends any pending input text for this thread.
/// Intercepts the initial `Welcome` message to update available models in the
/// runtime, and `Replay` messages to catch up resumed streams (see
/// [`forward_replay`]).
fn spawn_receiver(
    conn: ConnKey,
    connections: &mut HashMap<String, DaemonClient>,
    mut daemon: DaemonClient,
    to_df_tx: &mpsc::Sender<DaemonEvent>,
) {
    let rx = std::mem::replace(&mut daemon.rx, mpsc::channel(1).1);
    let tx_half = daemon;

    let to_df = to_df_tx.clone();
    let ts = conn.thread_ts.clone();
    let subagent = conn.subagent.clone();
    let tx_for_input = tx_half.tx.clone();
    tokio::spawn(async move {
        let mut rx = rx;
        let mut first = true;

        while let Some(msg) = rx.recv().await {
            // The daemon sends a Welcome message as the first message on
            // every connection. Intercept it to capture the available models
            // list; skip Welcomes that arrive later (e.g. after reconnect).
            if let DaemonMessage::Welcome {
                available_models, ..
            } = &msg
            {
                if first {
                    let rt = infinity_slack_dataflow::runtime::get();
                    let mut models = rt.available_models.lock().expect("bug: lock poisoned");
                    *models = available_models.clone();
                    tracing::info!(
                        "updated available models from daemon Welcome ({} models)",
                        models.len()
                    );
                }
                first = false;
                continue;
            }
            first = false;

            // On Connected, send pending input automatically (stashed by the
            // dataflow until the session exists; root connections only — a
            // subagent resume connection must not consume it).
            if let DaemonMessage::Connected {
                root_thread_id: ref session_id,
                ..
            } = msg
            {
                if subagent.is_none() {
                    let rt = infinity_slack_dataflow::runtime::get();
                    let pending_text = {
                        let mut pending = rt.pending_input.lock().expect("bug: lock poisoned");
                        pending.remove(&ts)
                    };
                    if let Some(text) = pending_text {
                        if let Err(e) = tx_for_input
                            .send(infinity_protocol::ClientMessage::UserInput {
                                thread_id: session_id.clone(),
                                text,
                            })
                            .await
                        {
                            tracing::error!("sending pending input for {ts} failed: {e}");
                        }
                    }
                }
            }

            // Replays are only meaningful for streams being resumed after a
            // restart; everything already on screen must not be re-posted.
            if let DaemonMessage::Replay {
                history,
                pending_choices,
                in_progress,
                ..
            } = msg
            {
                if !forward_replay(
                    &ts,
                    subagent.as_deref(),
                    &history,
                    &pending_choices,
                    in_progress,
                    &to_df,
                )
                .await
                {
                    break;
                }
                continue;
            }

            if to_df
                .send(DaemonEvent {
                    thread_ts: ts.clone(),
                    message: msg,
                })
                .await
                .is_err()
            {
                break;
            }
        }
    });

    connections.insert(conn.map_key, tx_half);
}

/// Forward the undelivered remainder of a resumed stream's `Replay` into the
/// dataflow, trimmed by the delivery cursor persisted at shutdown (see
/// [`infinity_slack_dataflow::resume::replay_delta`]). Replays for streams
/// that are not being resumed are dropped: their content was already
/// delivered before the restart (and for never-resumed threads the dataflow
/// has always ignored replays). Returns `false` when the dataflow channel
/// closed.
async fn forward_replay(
    thread_ts: &str,
    subagent: Option<&str>,
    history: &[DaemonMessage],
    pending_choices: &[DaemonMessage],
    in_progress: bool,
    to_df: &mpsc::Sender<DaemonEvent>,
) -> bool {
    let rt = infinity_slack_dataflow::runtime::get();
    let cursor = rt
        .resuming
        .lock()
        .expect("bug: lock poisoned")
        .remove(&(thread_ts.to_owned(), subagent.map(str::to_owned)));
    let Some(cursor) = cursor else {
        tracing::debug!("dropping replay for {thread_ts} (not resuming)");
        return true;
    };

    let delta = infinity_slack_dataflow::resume::replay_delta(history, &cursor, in_progress);
    tracing::info!(
        "resuming stream in {thread_ts}{}: {} replayed event(s), {} synthetic done(s), \
         {} orphan result(s), in_progress={in_progress}",
        subagent
            .map(|s| format!(" (subagent {s})"))
            .unwrap_or_default(),
        delta.events.len(),
        delta.synthetic_dones,
        delta.orphan_results,
    );

    // Seed sentinels for the results of calls skipped by the cursor BEFORE
    // forwarding any delta event, so replayed orphan results already pair
    // with sentinels (see `Runtime::resume_orphan_results`).
    if delta.orphan_results > 0 {
        let skey = infinity_slack_dataflow::slack::stream_key(thread_ts, subagent);
        rt.resume_orphan_results
            .lock()
            .expect("bug: lock poisoned")
            .insert(skey, delta.orphan_results);
    }

    for message in delta.events {
        if to_df
            .send(DaemonEvent {
                thread_ts: thread_ts.to_owned(),
                message,
            })
            .await
            .is_err()
        {
            return false;
        }
    }

    // Re-post choice buttons that are still pending and were never posted
    // (e.g. the choice arrived during downtime). Choices already on screen
    // are tracked in choice_messages (restored from the resume file) — their
    // buttons still work after the restart, so they must not be duplicated.
    for choice in pending_choices {
        let DaemonMessage::UserChoiceRequired { id, .. } = choice else {
            continue;
        };
        let already_posted = rt
            .choice_messages
            .lock()
            .expect("bug: lock poisoned")
            .contains_key(id.as_str());
        if already_posted {
            continue;
        }
        if to_df
            .send(DaemonEvent {
                thread_ts: thread_ts.to_owned(),
                message: choice.clone(),
            })
            .await
            .is_err()
        {
            return false;
        }
    }

    // Synthesize the ResponseDone(s) replay history never carries, so the
    // resumed stream ends in the same state the live event sequence would
    // have produced (see `replay_delta`'s docs for the counting).
    let done_thread_id = match subagent {
        None => None,
        Some(sub) => match sub.parse::<infinity_protocol::ThreadRef>() {
            Ok(t) => Some(t),
            Err(e) => {
                // Round-tripped from a ThreadRef, so this would be a bug;
                // fall back to the root rather than dropping the dones.
                tracing::error!("bug: unparsable subagent thread id {sub:?}: {e}");
                None
            }
        },
    };
    for _ in 0..delta.synthetic_dones {
        if to_df
            .send(DaemonEvent {
                thread_ts: thread_ts.to_owned(),
                message: DaemonMessage::ResponseDone {
                    thread_id: done_thread_id.clone(),
                    token_usage: None,
                },
            })
            .await
            .is_err()
        {
            return false;
        }
    }
    true
}
