//! The per-thread driver loop used by local (resident) agent systems.
//!
//! One driver task owns one thread: it batches inputs from the thread's
//! channel, runs [`Thread::step`]s, interrupts an in-flight completion when
//! user text arrives, defers synthetic events while a tool call is pending,
//! triggers auto-compaction, and idles out when there is nothing left to
//! wait for. The [router](super::router) respawns a driver when new input
//! arrives for an idle thread.

use std::collections::HashSet;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use tokio::sync::{mpsc, oneshot};

use crate::message::{InputMessage, InputMessageContent, SyntheticKind, TaggedSyntheticKind};
use crate::traits::{ConversationStore, InputSender, StateStore};
use rap_client::http::HttpClient;
use rig::message::UserContent;

use super::builder::SystemInner;
use super::defer::InMemoryDeferQueue;
use super::events::AgentEvent;
use super::observer::ThreadObserver;
use super::sender::ChannelSender;
use super::thread::{StepOutcome, Thread, is_user_text_input};

type BoxError = Box<dyn std::error::Error + Send + Sync>;

type StepFuture<'a> = Pin<Box<dyn Future<Output = Result<StepOutcome, BoxError>> + 'a>>;

/// Set of thread IDs with a live driver.
pub type ActiveThreads = Arc<Mutex<HashSet<String>>>;

fn is_compaction_complete(msg: &InputMessage) -> bool {
    msg.synthetic.as_ref().is_some_and(|s| {
        matches!(
            s,
            SyntheticKind::Tagged(TaggedSyntheticKind::CompactionComplete)
        )
    })
}

fn compaction_trigger_input(thread_id: &str) -> (InputMessage, String) {
    (
        InputMessage {
            content: InputMessageContent::User(UserContent::text("")),
            group_id: thread_id.to_owned(),
            metadata: None,
            synthetic: Some(SyntheticKind::Tagged(TaggedSyntheticKind::Compaction)),
            display_as: None,
            subscription: false,
        },
        uuid::Uuid::new_v4().to_string(),
    )
}

#[expect(
    clippy::too_many_arguments,
    reason = "driver wiring requires many channels"
)]
pub(crate) async fn drive_thread<C, S, H, O>(
    inner: Rc<SystemInner<C, S, ChannelSender, H>>,
    thread_id: String,
    mut rx: mpsc::UnboundedReceiver<(InputMessage, String)>,
    mut subscribe_rx: mpsc::UnboundedReceiver<(O::SubscribeRequest, oneshot::Sender<()>)>,
    // The router's subscribe queue, used to hand back requests that race in
    // while this driver is exiting (the router respawns a driver for them).
    router_subscribe_tx: mpsc::UnboundedSender<(String, O::SubscribeRequest, oneshot::Sender<()>)>,
    observer: O,
    active_threads: ActiveThreads,
    exit_tx: mpsc::UnboundedSender<String>,
) where
    C: ConversationStore + 'static,
    S: StateStore + 'static,
    H: HttpClient + 'static,
    O: ThreadObserver + 'static,
{
    active_threads
        .lock()
        .expect("bug: mutex poisoned")
        .insert(thread_id.clone());
    let _guard = DriverGuard {
        thread_id: thread_id.clone(),
        active_threads,
        exit_tx,
    };

    let loopback = inner.sender.clone();
    let thread = match Thread::load(inner, thread_id.clone()).await {
        Ok(t) => t,
        Err(e) => {
            observer.on_event(
                &thread_id,
                &AgentEvent::Info {
                    text: format!("Error: {e}"),
                },
            );
            return;
        }
    };

    let mut defer = InMemoryDeferQueue::new();
    // Inputs that queued up while a step was in flight (or were generated
    // internally, like the auto-compaction trigger).
    let mut pending_batch: Vec<(InputMessage, String)> = Vec::new();
    let mut compaction_triggered = false;
    let mut total_tokens: u64 = 0;
    // Set when subscribe_rx is closed, so selects stop polling it (a closed
    // channel is always ready and would otherwise spin).
    let mut subscribe_closed = false;

    let mut step_fut: Option<StepFuture<'_>> = None;
    let mut cancel_tx: Option<oneshot::Sender<()>> = None;

    loop {
        let new_inputs: Vec<(InputMessage, String)> = if let Some(fut) = step_fut.as_mut() {
            tokio::select! {
                biased;

                res = fut => {
                    #[expect(clippy::let_underscore_future, reason = "dropping completed future")]
                    let _ = step_fut.take().expect("bug: step_fut missing after poll");
                    cancel_tx = None;
                    handle_step_result(
                        res,
                        &observer,
                        &thread_id,
                        &mut total_tokens,
                        &mut compaction_triggered,
                        &mut pending_batch,
                    );
                    continue;
                },
                first = rx.recv() => {
                    let Some(first) = first else {
                        // Input channel closed — the system is shutting down.
                        // Interrupt the in-flight step and wait for it to wind
                        // down so pending history items are synced to the
                        // store. The cancellation path flushes the in-flight
                        // turn before the sync, so whatever streamed so far is
                        // preserved.
                        let _ = cancel_tx.take().expect("bug: cancel_tx missing during shutdown").send(());
                        let _ = step_fut.take().expect("bug: step_fut missing during shutdown").await;
                        return;
                    };
                    let mut batch = vec![first];
                    while let Ok(item) = rx.try_recv() {
                        batch.push(item);
                    }

                    if batch.iter().any(|(msg, _)| is_user_text_input(msg)) {
                        // User text interrupts the in-flight completion.
                        let _ = cancel_tx.take().expect("bug: cancel_tx missing during interrupt").send(());
                        let res = step_fut.take().expect("bug: step_fut missing during interrupt").await;
                        handle_step_result(
                            res,
                            &observer,
                            &thread_id,
                            &mut total_tokens,
                            &mut compaction_triggered,
                            &mut pending_batch,
                        );

                        let (mut user_inputs, non_user_inputs): (Vec<_>, Vec<_>) = batch
                            .into_iter()
                            .partition(|(msg, _)| is_user_text_input(msg));

                        if let InputMessageContent::User(UserContent::Text(text)) =
                            &mut user_inputs[0].0.content
                        {
                            text.text = format!("<interrupt>{}", text.text);
                        } else {
                            panic!("bug: user_inputs should only contain user text");
                        }

                        pending_batch.extend(non_user_inputs);
                        user_inputs
                    } else {
                        pending_batch.extend(batch);
                        continue;
                    }
                },
                req = subscribe_rx.recv(), if !subscribe_closed => {
                    match req {
                        Some((req, ack)) => {
                            observer.on_subscribe(&thread_id, req, thread.replay_snapshot());
                            let _ = ack.send(());
                        }
                        None => subscribe_closed = true,
                    }
                    continue;
                }
            }
        } else {
            let mut batch: Vec<(InputMessage, String)> = std::mem::take(&mut pending_batch);
            while let Ok(item) = rx.try_recv() {
                batch.push(item);
            }

            // Deferred events whose blocking tool call has been resolved must
            // be processed even if nothing else arrives.
            let defer_ready = !defer.is_empty() && thread.pending_active_tool_call().is_none();

            if batch.is_empty() && !defer_ready {
                // Handle replays before considering idling out.
                while let Ok((req, ack)) = subscribe_rx.try_recv() {
                    observer.on_subscribe(&thread_id, req, thread.replay_snapshot());
                    let _ = ack.send(());
                }

                if !thread.expects_wakeup().await {
                    tracing::info!("Thread {} going idle", thread_id);
                    // Hand back anything that raced in while we were deciding
                    // to exit, so the router respawns a driver for it instead
                    // of it being silently dropped.
                    rx.close();
                    subscribe_rx.close();
                    while let Ok((msg, id)) = rx.try_recv() {
                        let group_id = msg.group_id.clone();
                        if let Err(e) = loopback.send_to_input_queue(msg, &group_id, &id).await {
                            tracing::warn!("failed to hand back input on idle exit: {e}");
                        }
                    }
                    while let Ok((req, ack)) = subscribe_rx.try_recv() {
                        let _ = router_subscribe_tx.send((thread_id.clone(), req, ack));
                    }
                    return;
                }

                // Park until something arrives.
                loop {
                    tokio::select! {
                        biased;
                        msg = rx.recv() => {
                            match msg {
                                Some(m) => {
                                    batch.push(m);
                                    break;
                                }
                                None => return,
                            }
                        }
                        req = subscribe_rx.recv(), if !subscribe_closed => {
                            match req {
                                Some((req, ack)) => {
                                    observer.on_subscribe(&thread_id, req, thread.replay_snapshot());
                                    let _ = ack.send(());
                                }
                                None => subscribe_closed = true,
                            }
                        }
                    }
                }
                while let Ok(item) = rx.try_recv() {
                    batch.push(item);
                }
            }

            batch
        };

        // Reset the compaction trigger (and the stale token count) once a
        // compaction round-trips: the next completion reports fresh usage.
        for (msg, _) in &new_inputs {
            if is_compaction_complete(msg) {
                compaction_triggered = false;
                total_tokens = 0;
            }
        }

        let batch = match thread.filter_deferrable(new_inputs, &mut defer).await {
            Ok(batch) => batch,
            Err(e) => {
                observer.on_event(
                    &thread_id,
                    &AgentEvent::Info {
                        text: format!("Error: {e}"),
                    },
                );
                continue;
            }
        };
        if batch.is_empty() {
            // Everything was deferred (or nothing arrived); wait for more input.
            continue;
        }

        let (tx, cancel_rx) = oneshot::channel::<()>();
        cancel_tx = Some(tx);
        step_fut = Some(Box::pin(thread.step_no_defer(batch, &observer, cancel_rx)));
    }
}

fn handle_step_result(
    res: Result<StepOutcome, BoxError>,
    observer: &impl ThreadObserver,
    thread_id: &str,
    total_tokens: &mut u64,
    compaction_triggered: &mut bool,
    pending_batch: &mut Vec<(InputMessage, String)>,
) {
    match res {
        Ok(StepOutcome::Completed {
            usage,
            context_window,
        }) => {
            if let Some(u) = usage {
                // Use total_tokens which includes cached input. When prompt
                // caching is active, `input_tokens` alone undercounts usage.
                *total_tokens = u.total_tokens;
            }
            // Background compaction: trigger when the context is > 75% full.
            if !*compaction_triggered
                && context_window > 0
                && *total_tokens as usize > context_window * 3 / 4
            {
                *compaction_triggered = true;
                tracing::info!(
                    "Auto-compaction for thread {}: {} total tokens > 75% of {} context window",
                    thread_id,
                    total_tokens,
                    context_window
                );
                observer.on_event(
                    thread_id,
                    &AgentEvent::Info {
                        text: "✦ Auto-compaction triggered (context > 75%)".to_owned(),
                    },
                );
                pending_batch.push(compaction_trigger_input(thread_id));
            }
        }
        Ok(StepOutcome::Skipped) => {}
        Err(e) => {
            observer.on_event(
                thread_id,
                &AgentEvent::Info {
                    text: format!("Error: {e}"),
                },
            );
        }
    }
}

struct DriverGuard {
    thread_id: String,
    active_threads: ActiveThreads,
    exit_tx: mpsc::UnboundedSender<String>,
}

impl Drop for DriverGuard {
    fn drop(&mut self) {
        self.active_threads
            .lock()
            .expect("bug: mutex poisoned")
            .remove(&self.thread_id);
        // Notify the embedding that this thread's driver exited, so it can
        // release per-conversation resources once nothing else is live.
        let _ = self.exit_tx.send(self.thread_id.clone());
    }
}
