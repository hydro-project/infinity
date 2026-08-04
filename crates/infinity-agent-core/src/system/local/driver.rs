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
use crate::traits::{ConversationStore, StateStore};
use rap_client::http::HttpClient;
use rig::message::UserContent;

use super::sender::ChannelSender;
use crate::system::builder::SystemInner;
use crate::system::defer::InMemoryDeferQueue;
use crate::system::events::AgentEvent;
use crate::system::observer::ThreadObserver;
use crate::system::thread::{StepOutcome, Thread, is_user_text_input};

type BoxError = Box<dyn std::error::Error + Send + Sync>;

type StepFuture<'a> = Pin<Box<dyn Future<Output = Result<StepOutcome, BoxError>> + 'a>>;

/// An in-flight step paired with its cancellation handle. Keeping the two in
/// one value makes it impossible for them to desynchronize: interrupting the
/// step consumes both, and dropping the pair (e.g. on driver teardown) drops
/// the sender, which the step observes as a cancellation.
struct InFlightStep<'a> {
    fut: StepFuture<'a>,
    cancel_tx: oneshot::Sender<()>,
}

impl InFlightStep<'_> {
    /// Interrupt the step and wait for it to wind down. The cancellation
    /// path flushes whatever streamed so far to the store before returning,
    /// so no partial turn is lost.
    async fn cancel(self) -> Result<StepOutcome, BoxError> {
        let _ = self.cancel_tx.send(());
        self.fut.await
    }
}

/// Set of thread IDs with a live driver.
pub type ActiveThreads = Arc<Mutex<HashSet<String>>>;

/// A transition in one thread driver's liveness, reported on
/// [`RunningSystem::thread_lifecycle`](super::RunningSystem#structfield.thread_lifecycle).
///
/// For one driver, `Live` is always reported before its matching `Idle`.
/// Because drivers respawn on demand, one thread can report many
/// `Live`/`Idle` pairs over its lifetime.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ThreadLifecycleEvent {
    /// A driver spawned for the thread: it is processing input or waiting
    /// on in-flight work.
    Live {
        /// The thread whose driver spawned.
        thread_id: String,
    },
    /// The thread's driver exited: nothing is queued and no tool result is
    /// on its way, so the thread is idle until the next message. The thread
    /// may still hold active subscriptions; a subscription event respawns
    /// its driver on arrival.
    Idle {
        /// The thread whose driver exited.
        thread_id: String,
    },
}

fn is_compaction_complete(msg: &InputMessage) -> bool {
    msg.synthetic
        .as_ref()
        .is_some_and(SyntheticKind::is_compaction_complete)
}

pub(crate) async fn drive_thread<C, S, H, O>(
    inner: Rc<SystemInner<C, S, ChannelSender, H>>,
    thread_id: String,
    mut rx: mpsc::UnboundedReceiver<(InputMessage, String)>,
    mut subscribe_rx: mpsc::UnboundedReceiver<(O::SubscribeRequest, oneshot::Sender<()>)>,
    observer: O,
    active_threads: ActiveThreads,
    lifecycle_tx: mpsc::UnboundedSender<ThreadLifecycleEvent>,
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
    // Report liveness transitions at the same two points where
    // `active_threads` changes, so the channel and the set can never
    // disagree about a driver's state.
    let _ = lifecycle_tx.send(ThreadLifecycleEvent::Live {
        thread_id: thread_id.clone(),
    });
    let _guard = DriverGuard {
        thread_id: thread_id.clone(),
        active_threads,
        lifecycle_tx,
    };

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

    let mut in_flight: Option<InFlightStep<'_>> = None;

    loop {
        let new_inputs: Vec<(InputMessage, String)> = if let Some(flight) = in_flight.as_mut() {
            tokio::select! {
                biased;

                res = &mut flight.fut => {
                    in_flight = None;
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
                        // store.
                        let _ = in_flight
                            .take()
                            .expect("bug: in-flight step missing during shutdown")
                            .cancel()
                            .await;
                        return;
                    };
                    let mut batch = vec![first];
                    while let Ok(item) = rx.try_recv() {
                        batch.push(item);
                    }

                    if batch.iter().any(|(msg, _)| is_user_text_input(msg)) {
                        // User text interrupts the in-flight completion.
                        let res = in_flight
                            .take()
                            .expect("bug: in-flight step missing during interrupt")
                            .cancel()
                            .await;
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
                // From here to `return` there must be no await: the router
                // runs on the same `LocalSet`, so a section without awaits is
                // atomic with respect to it. Anything already in the channels
                // is found by the `try_recv`s below, and anything sent after
                // `return` finds the channels closed (dropping the receivers
                // closes them), which makes the router respawn a driver.
                while let Ok((req, ack)) = subscribe_rx.try_recv() {
                    observer.on_subscribe(&thread_id, req, thread.replay_snapshot());
                    let _ = ack.send(());
                }
                if let Ok(item) = rx.try_recv() {
                    batch.push(item);
                    while let Ok(item) = rx.try_recv() {
                        batch.push(item);
                    }
                } else if !thread.awaiting_tool_result() {
                    // Active subscriptions do not keep the driver resident:
                    // their events respawn it on arrival. Resource managers
                    // that care (e.g. the daemon's RAP servers) check
                    // subscriptions when they observe the `Idle` event.
                    tracing::info!("Thread {} going idle", thread_id);
                    return;
                } else {
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

        let (cancel_tx, cancel_rx) = oneshot::channel::<()>();
        in_flight = Some(InFlightStep {
            fut: Box::pin(thread.step_no_defer(batch, &observer, cancel_rx)),
            cancel_tx,
        });
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
                pending_batch.push((
                    InputMessage {
                        content: InputMessageContent::User(UserContent::text("")),
                        group_id: thread_id.to_owned(),
                        metadata: None,
                        synthetic: Some(SyntheticKind::Tagged(TaggedSyntheticKind::Compaction)),
                        display_as: None,
                        subscription: false,
                    },
                    uuid::Uuid::new_v4().to_string(),
                ));
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
    lifecycle_tx: mpsc::UnboundedSender<ThreadLifecycleEvent>,
}

impl Drop for DriverGuard {
    fn drop(&mut self) {
        self.active_threads
            .lock()
            .expect("bug: mutex poisoned")
            .remove(&self.thread_id);
        // Notify the embedding that this thread's driver exited, so it can
        // release per-conversation resources once nothing else is live.
        let _ = self.lifecycle_tx.send(ThreadLifecycleEvent::Idle {
            thread_id: self.thread_id.clone(),
        });
    }
}
#[cfg(test)]
mod tests {
    use crate::system::events::AgentEvent;
    use crate::system::test_support::*;

    /// An active subscription does not keep a driver resident: once the
    /// subscription tool's result settles the call, the driver idles out, and a
    /// later subscription event respawns it.
    #[tokio::test(flavor = "current_thread")]
    async fn subscribed_thread_idles_and_event_respawns_it() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (mut running, mut rx, mut ctrl, _conv) =
                    start_system(vec![Box::new(SubscribeTool)], None);
                running.send_user_text("t1", "subscribe").await;
                let _req = ctrl.next_request().await;
                ctrl.send_tool_call("tc-sub", "subscribe_tool", serde_json::json!({}));
                ctrl.finish();
                collect_until_finished(&mut rx).await;

                // The subscription result triggers one more round; after it the
                // thread holds an active subscription and nothing else.
                let _req2 = ctrl.next_request().await;
                ctrl.send_text("subscribed");
                ctrl.finish();
                collect_until_finished(&mut rx).await;

                wait_idle(&mut running).await;
                assert!(
                    running.is_idle(),
                    "an active subscription must not keep the driver resident"
                );

                // A subscription event arrives later: the router respawns a
                // driver and the event is processed in a child thread, which
                // reports a request to the model.
                running
                    .send(
                        subscription_event_input("t1", "tc-sub", "late event").0,
                        "ev-late",
                    )
                    .await;
                let child_req = ctrl.next_request().await;
                assert!(
                    tool_result_texts(&child_req)
                        .iter()
                        .any(|t| t.contains("late event")),
                    "the event must reach a respawned driver"
                );
                ctrl.send_text("handled");
                ctrl.finish();
            })
            .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn auto_compaction_triggers_and_applies() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                // Tiny context window so the usage report crosses the threshold.
                let (mut running, mut rx, mut ctrl, _conv) =
                    start_system(vec![], Some(small_context_entry()));
                running.send_user_text("t1", "hello").await;
                let _req = ctrl.next_request().await;
                ctrl.send_text("hi there");
                ctrl.finish_with_usage(high_usage());
                collect_until_finished(&mut rx).await;

                // The driver spawns a compaction child; close it with a summary.
                let creq = ctrl.next_request().await;
                assert!(is_compaction_req(&creq));
                handle_compaction_child(&mut ctrl, &creq, "summary of everything");

                // The parent eventually applies the compaction.
                loop {
                    if let Evt::E(AgentEvent::CompactionApplied) = next_evt(&mut rx).await {
                        break;
                    }
                }
                wait_idle(&mut running).await;
            })
            .await;
    }
}
