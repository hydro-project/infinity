//! Defines the Hydro dataflow graph for the Slack bot.
//!
//! Two streams feed the dataflow:
//! - Slack I/O: `Stream<SlackEvent>` in, `Stream<SlackAction>` out
//! - Daemon I/O: `Stream<DaemonEvent>` in, `Stream<DaemonCommand>` out

use hydro_lang::prelude::*;

// NOTE: types used inside `q!()` bodies must be imported (not referenced via
// `crate::...` paths): the embedded include site re-expands the quoted code in
// a foreign crate where `crate::` does not resolve. These imports are carried
// into the staged module (forced `pub`) and glob-imported at that site.
// *Expression* paths are fine either way -- stageleft rewrites them through
// the staged module (e.g. `crate::runtime::get()` is emitted as
// `infinity_slack_dataflow::__staged::runtime::get()`); only paths in *type*
// position (closure parameter annotations, patterns) are pasted verbatim and
// must therefore come from these imports.
use crate::daemon::{DaemonCommand, DaemonEvent};
use crate::slack::{
    SlackAction, SlackEvent, MODEL_PICKER_ACTION_ID, MODEL_PICKER_BLOCK_ID,
    MODEL_PICKER_CALLBACK_ID,
};

/// The full dataflow graph. Returns (slack_actions, daemon_commands).
#[expect(clippy::type_complexity, reason = "Hydro stream type signatures")]
pub fn slack_dataflow<'a, P: 'a>(
    slack_events: Stream<SlackEvent, Process<'a, P>, Unbounded>,
    daemon_events: Stream<DaemonEvent, Process<'a, P>, Unbounded>,
) -> (
    Stream<SlackAction, Process<'a, P>, Unbounded>,
    Stream<DaemonCommand, Process<'a, P>, Unbounded>,
) {
    // Filter: drop bot messages and unauthorized users.
    let filtered = slack_events.filter(q!(|event: &SlackEvent| {
        !event.is_bot && !event.is_unauthorized
    }));

    // Partition into non-button messages vs button clicks to avoid cloning the
    // full stream and eliminate redundant `if is_button_click` checks.
    let (non_buttons, button_clicks) =
        filtered.partition(q!(|event: &SlackEvent| !event.is_button_click));

    // --- Button clicks → stop any active stream so the response appears in a new message ---
    let button_stop_actions = button_clicks.clone().map(q!(|event: SlackEvent| {
        // A click inside a dedicated subagent Slack thread targets that
        // subagent's stream; remap it so the I/O layer finds the stream
        // under its (root thread, subagent) key.
        let rt = crate::runtime::get();
        let route = rt
            .subagent_routes
            .lock()
            .expect("bug: lock poisoned")
            .get(&event.thread_ts)
            .cloned();
        match route {
            Some(route) => SlackAction::StreamStop {
                channel: event.channel,
                thread_ts: route.root_thread_ts,
                subagent: Some(route.subagent_id),
                end_of_response: false,
            },
            None => SlackAction::StreamStop {
                channel: event.channel,
                thread_ts: event.thread_ts,
                subagent: None,
                end_of_response: false,
            },
        }
    }));

    // --- Button clicks → update the button message to show which choice was selected ---
    let button_update_actions = button_clicks.clone().filter_map(q!(|event: SlackEvent| {
        let message_ts = event.message_ts?;
        let selected_label = event.button_text.unwrap_or_else(|| "…".to_owned());
        // Strip the " ✓" suffix if present (it was the default marker).
        let selected_label = selected_label
            .strip_suffix(" ✓")
            .unwrap_or(&selected_label)
            .to_owned();
        let blocks = serde_json::json!([
            {
                "type": "section",
                "text": {
                    "type": "mrkdwn",
                    "text": format!("✅ Selected: *{selected_label}*")
                }
            }
        ]);
        Some(SlackAction::UpdateMessage {
            channel: event.channel,
            ts: message_ts,
            text: format!("Selected: {selected_label}"),
            blocks,
        })
    }));

    // --- Button clicks → AnswerChoice commands ---
    let button_commands = button_clicks.filter_map(q!(|event: SlackEvent| {
        let rt = crate::runtime::get();
        // Clicks inside a dedicated subagent Slack thread belong to the
        // owning session, which is keyed by the root conversation thread.
        let thread_ts = {
            let routes = rt.subagent_routes.lock().expect("bug: lock poisoned");
            routes
                .get(&event.thread_ts)
                .map(|r| r.root_thread_ts.clone())
                .unwrap_or(event.thread_ts)
        };
        rt.channels
            .lock()
            .expect("bug: lock poisoned")
            .insert(thread_ts.clone(), event.channel.clone());

        // Parse action_id: "choice_{choice_id}_{selected_index}"
        let action_id = event.action_id.unwrap_or_default();
        let selected: usize = event.button_value.and_then(|v| v.parse().ok()).unwrap_or(0);
        // Strip "choice_" prefix, then split off the trailing "_{index}"
        let rest = action_id.strip_prefix("choice_").unwrap_or(&action_id);
        let choice_id = match rest.rsplit_once('_') {
            Some((id, _)) => id.to_owned(),
            None => rest.to_owned(),
        };

        // Remove from choice_messages so that the subsequent UserChoiceComplete
        // from the daemon doesn't redundantly try to dismiss the buttons (we
        // already update them in button_update_actions).
        rt.choice_messages
            .lock()
            .expect("bug: lock poisoned")
            .remove(choice_id.as_str());

        Some(DaemonCommand::AnswerChoice {
            thread_ts,
            choice_id: choice_id.into(),
            selected,
        })
    }));

    // --- Partition non-buttons into slash commands vs regular messages ---
    let (regular_messages, slash_commands) =
        non_buttons.partition(q!(|event: &SlackEvent| { event.slash_command.is_none() }));

    // --- Slash commands → respond via the command's response_url ---
    let command_response_actions = slash_commands.filter_map(q!(|event: SlackEvent| {
        let response_url = event.response_url?;
        let command = event.slash_command.unwrap_or_default();

        match command.as_str() {
            "/model" => {
                let arg = event.text.trim();
                // With no argument, prefer an interactive modal picker
                // (needs a trigger_id and a non-empty model list); fall
                // back to the text listing otherwise.
                if arg.is_empty() {
                    if let Some(trigger_id) = event.trigger_id {
                        if let Some(view) = crate::flow::build_model_picker_view(&response_url) {
                            return Some(SlackAction::OpenView { trigger_id, view });
                        }
                    }
                }
                Some(SlackAction::CommandResponse {
                    response_url,
                    text: crate::flow::handle_model_command(arg),
                })
            }
            other => Some(SlackAction::CommandResponse {
                response_url,
                text: format!("Unknown command `{other}`."),
            }),
        }
    }));

    // --- Partition regular messages into app-home opens vs chat messages ---
    let (non_app_home, app_home_events) =
        regular_messages.partition(q!(|event: &SlackEvent| { !event.is_app_home_opened }));

    // --- Partition out native stop-button clicks (`agent_session_stopped`) ---
    let (non_stops, session_stops) =
        non_app_home.partition(q!(|event: &SlackEvent| { !event.is_session_stopped }));

    // --- Partition out message-shortcut invocations (context ⋮ menu) ---
    let (non_shortcuts, shortcuts) =
        non_stops.partition(q!(|event: &SlackEvent| { event.shortcut.is_none() }));

    // --- Partition out fork-thread readiness, then rewind-modal submissions ---
    // (Fork-ready events also carry `rewind_user_turn`, so split them first.)
    let (non_fork_ready, fork_ready) = non_shortcuts.partition(q!(|event: &SlackEvent| {
        event.fork_source_thread_ts.is_none()
    }));
    let (chat_messages, rewind_submits) = non_fork_ready.partition(q!(|event: &SlackEvent| {
        event.rewind_user_turn.is_none()
    }));

    // --- Message shortcuts (context ⋮ menu) → validate + act ---
    // "Fork from here": the I/O layer resolves the governing user turn (it
    // needs the Slack thread's replies) and opens the modal; the dataflow
    // only validates what it can check synchronously.
    // "Compact session": resolved fully here (session-scoped, no modal); the
    // matching TriggerCompaction command is emitted by the parallel
    // `compact_commands` branch below, which resolves the same target
    // deterministically — this branch only acks/errs ephemerally.
    let shortcut_actions = shortcuts.clone().filter_map(q!(|event: SlackEvent| {
        let response_url = event.response_url?;
        if event.shortcut.as_deref() == Some(crate::slack::COMPACT_SHORTCUT_CALLBACK_ID) {
            let text = match crate::flow::resolve_compact_session(&event.thread_ts) {
                Ok((root_ts, _session)) => {
                    let title = {
                        let titles = crate::runtime::get()
                            .thread_titles
                            .lock()
                            .expect("bug: lock poisoned");
                        titles.get(&root_ts).cloned()
                    };
                    match title {
                        Some(t) => format!(
                            "🗜️ Compacting *{t}*… a note will be posted in the thread when done."
                        ),
                        None => "🗜️ Compacting this session… a note will be posted in the \
                                 thread when done."
                            .to_owned(),
                    }
                }
                Err(text) => text,
            };
            return Some(SlackAction::CommandResponse { response_url, text });
        }
        if event.shortcut.as_deref() != Some(crate::slack::REWIND_SHORTCUT_CALLBACK_ID) {
            let shortcut = event.shortcut.unwrap_or_default();
            return Some(SlackAction::CommandResponse {
                response_url,
                text: format!("Unknown shortcut `{shortcut}`."),
            });
        }
        let Some(trigger_id) = event.trigger_id else {
            return Some(SlackAction::CommandResponse {
                response_url,
                text: "⚠️ Could not open the fork dialog (no trigger id).".to_owned(),
            });
        };
        let Some(target_ts) = event.message_ts else {
            return Some(SlackAction::CommandResponse {
                response_url,
                text: "⚠️ Could not identify the message to fork from.".to_owned(),
            });
        };
        let rt = crate::runtime::get();
        // v1: a subagent's dedicated thread has no independent turn history
        // to fork from — point the user at the main conversation.
        if rt
            .subagent_routes
            .lock()
            .expect("bug: lock poisoned")
            .contains_key(&event.thread_ts)
        {
            return Some(SlackAction::CommandResponse {
                response_url,
                text: "⏪ Forking isn't available inside a subagent's thread — use it on a \
                       message in the main conversation."
                    .to_owned(),
            });
        }
        let has_session = rt
            .sessions
            .lock()
            .expect("bug: lock poisoned")
            .get(&event.thread_ts)
            .is_some();
        if !has_session {
            return Some(SlackAction::CommandResponse {
                response_url,
                text: "⏪ This thread has no Infinity session to fork.".to_owned(),
            });
        }
        Some(SlackAction::OpenRewindModal {
            channel: event.channel,
            thread_ts: event.thread_ts,
            target_ts,
            trigger_id,
            response_url,
        })
    }));

    // --- "Compact session" shortcut → TriggerCompaction commands (parallel
    // to the ephemeral ack above; both branches resolve the same target
    // deterministically) ---
    let compact_commands = shortcuts.filter_map(q!(|event: SlackEvent| {
        if event.shortcut.as_deref() != Some(crate::slack::COMPACT_SHORTCUT_CALLBACK_ID) {
            return None;
        }
        let (thread_ts, session_id) =
            crate::flow::resolve_compact_session(&event.thread_ts).ok()?;
        Some(DaemonCommand::TriggerCompaction {
            thread_ts,
            session_id,
        })
    }));

    // --- Fork-thread readiness → ForkSession command ---
    // The I/O layer has posted the fork's anchor message; `thread_ts` is that
    // new Slack thread. Exactly like a first message on a new thread, stash
    // the edited turn until the daemon's `Connected` for the fork arrives
    // (which also maps the new thread to the fork's session).
    let fork_commands = fork_ready.clone().filter_map(q!(|event: SlackEvent| {
        let source_ts = event.fork_source_thread_ts?;
        let user_turn = event.rewind_user_turn?;
        let rt = crate::runtime::get();
        let session_id = rt
            .sessions
            .lock()
            .expect("bug: lock poisoned")
            .get(&source_ts)
            .cloned()?;
        rt.channels
            .lock()
            .expect("bug: lock poisoned")
            .insert(event.thread_ts.clone(), event.channel.clone());
        rt.pending_input
            .lock()
            .expect("bug: lock poisoned")
            .insert(event.thread_ts.clone(), event.text.trim().to_owned());
        Some(DaemonCommand::ForkSession {
            thread_ts: event.thread_ts,
            session_id,
            user_turn,
        })
    }));

    // --- Fork-thread readiness → "thinking" status on the new thread ---
    let fork_actions = fork_ready.map(q!(|event: SlackEvent| SlackAction::SetStatus {
        channel: event.channel,
        thread_ts: event.thread_ts,
        status: "is thinking".to_owned(),
    }));

    // --- Rewind-modal submissions → fork into a new Slack thread ---
    // Non-destructive: the original thread and its session are untouched
    // (no stream stop, no marker in the flow of its conversation). The I/O
    // layer posts the fork's anchor + a cross-link and reports back with a
    // fork-ready event keyed on the new thread.
    let rewind_actions = rewind_submits.filter_map(q!(|event: SlackEvent| {
        let user_turn = event.rewind_user_turn?;
        let rt = crate::runtime::get();
        let has_session = rt
            .sessions
            .lock()
            .expect("bug: lock poisoned")
            .get(&event.thread_ts)
            .is_some();
        if !has_session {
            return Some(SlackAction::PostMessage {
                channel: event.channel,
                text: "⚠️ Could not fork: this thread no longer has an Infinity session."
                    .to_owned(),
                thread_ts: Some(event.thread_ts),
                subagent: None,
            });
        }
        Some(SlackAction::ForkThread {
            channel: event.channel,
            source_thread_ts: event.thread_ts,
            source_message_ts: event.message_ts.unwrap_or_default(),
            user_turn,
            text: event.text.trim().to_owned(),
        })
    }));

    // --- Stop events → halt the thread's stream + confirm to the user ---
    // Slack has already finalized the streaming message(s) client-side; the
    // I/O layer must not re-touch them, must drop buffered output, and must
    // suppress any raced remainder of the current response. A stop inside a
    // subagent's dedicated thread halts that subagent's stream only. The
    // matching `Interrupt` daemon command (below) cancels the generation
    // itself.
    let session_stop_actions = session_stops
        .clone()
        .flat_map_ordered(q!(|event: SlackEvent| {
            let rt = crate::runtime::get();
            let route = rt
                .subagent_routes
                .lock()
                .expect("bug: lock poisoned")
                .get(&event.thread_ts)
                .cloned();
            let (thread_ts, subagent) = match route {
                Some(route) => (route.root_thread_ts, Some(route.subagent_id)),
                None => (event.thread_ts, None),
            };
            vec![
                SlackAction::HaltStreams {
                    channel: event.channel.clone(),
                    thread_ts: thread_ts.clone(),
                    subagent: subagent.clone(),
                    stopped_message_ts: event.streaming_message_ts,
                },
                // Per Slack's agent-sessions guidance: confirm to the user
                // that work stopped.
                SlackAction::PostMessage {
                    channel: event.channel,
                    text: "⏹️ Stopped — the agent was interrupted. Send a new message to \
                           continue."
                        .to_owned(),
                    thread_ts: Some(thread_ts),
                    subagent,
                },
            ]
        }));

    // --- Stop events → Interrupt commands (cancel generation on the daemon) ---
    let interrupt_commands = session_stops.filter_map(q!(|event: SlackEvent| {
        let rt = crate::runtime::get();
        let route = rt
            .subagent_routes
            .lock()
            .expect("bug: lock poisoned")
            .get(&event.thread_ts)
            .cloned();
        let (thread_ts, target_thread) = match route {
            Some(route) => {
                // The id round-trips from a `ThreadRef` the daemon sent us, so
                // a parse failure would be a bug; fall back to interrupting
                // the whole session rather than dropping the user's stop.
                let target = route.subagent_id.parse().ok();
                if target.is_none() {
                    tracing::error!("bug: unparsable subagent thread id {:?}", route.subagent_id);
                }
                (route.root_thread_ts, target)
            }
            None => (event.thread_ts, None),
        };
        let session_id = {
            let sessions = rt.sessions.lock().expect("bug: lock poisoned");
            sessions.get(&thread_ts).cloned()
        };
        // No session mapped (e.g. a stop in a thread from before a bot
        // restart): nothing to interrupt — the HaltStreams action above
        // already settled the Slack side.
        let session_id = session_id?;
        Some(DaemonCommand::Interrupt {
            thread_ts,
            session_id,
            target_thread,
        })
    }));

    // --- App-home opens → pin suggested prompts (once per user per run) ---
    let app_home_actions = app_home_events.filter_map(q!(|event: SlackEvent| {
        let rt = crate::runtime::get();
        let mut seen = rt.app_home_seen.lock().expect("bug: lock poisoned");
        if seen.insert(event.user) {
            Some(SlackAction::SetSuggestedPrompts {
                channel: event.channel,
            })
        } else {
            None
        }
    }));

    // --- Chat messages → CreateSession / SendInput commands ---
    let message_commands = chat_messages.clone().filter_map(q!(|event: SlackEvent| {
        let rt = crate::runtime::get();
        // A new message in a thread lifts any user-stop suppression for that
        // thread (`event.thread_ts` is the target thread ts the latch is
        // keyed by: the root conversation thread or a subagent anchor).
        rt.halted_streams
            .lock()
            .expect("bug: lock poisoned")
            .remove(&event.thread_ts);
        // A reply inside a dedicated subagent Slack thread is addressed to
        // that subagent's agent thread, on the session owned by the root
        // conversation thread.
        let route = rt
            .subagent_routes
            .lock()
            .expect("bug: lock poisoned")
            .get(&event.thread_ts)
            .cloned();
        let (thread_ts, target_thread) = match route {
            Some(route) => {
                // The id round-trips from a `ThreadRef` the daemon sent us,
                // so a parse failure would be a bug; fall back to the root
                // thread rather than dropping the user's message.
                let target = route.subagent_id.parse().ok();
                if target.is_none() {
                    tracing::error!("bug: unparsable subagent thread id {:?}", route.subagent_id);
                }
                (route.root_thread_ts, target)
            }
            None => (event.thread_ts, None),
        };
        rt.channels
            .lock()
            .expect("bug: lock poisoned")
            .insert(thread_ts.clone(), event.channel.clone());

        let existing = {
            let sessions = rt.sessions.lock().expect("bug: lock poisoned");
            sessions.get(&thread_ts).cloned()
        };
        if let Some(session_id) = existing {
            Some(DaemonCommand::SendInput {
                thread_ts,
                session_id,
                target_thread,
                text: event.text.trim().to_owned(),
            })
        } else {
            // Stash the text to send after Connected arrives.
            let mut pending = rt.pending_input.lock().expect("bug: lock poisoned");
            pending.insert(thread_ts.clone(), event.text.trim().to_owned());
            let model = rt.default_model.lock().expect("bug: lock poisoned").clone();
            Some(DaemonCommand::CreateSession {
                thread_ts,
                cwd: rt.config.default_cwd.clone(),
                model,
            })
        }
    }));

    let daemon_commands = button_commands
        .merge_ordered(
            message_commands,
            nondet!(/**
                a button command has non-deterministic ordering w.r.t. message commands
                because batching may interleave them arbitrarily
            */),
        )
        .merge_ordered(
            interrupt_commands,
            nondet!(/**
                interrupt commands have non-deterministic ordering w.r.t. other daemon
                commands because they originate from independent user interaction events
            */),
        )
        .merge_ordered(
            fork_commands,
            nondet!(/**
                fork commands have non-deterministic ordering w.r.t. other daemon
                commands because they originate from independent user interaction events
            */),
        )
        .merge_ordered(
            compact_commands,
            nondet!(/**
                compaction commands have non-deterministic ordering w.r.t. other daemon
                commands because they originate from independent shortcut events
            */),
        );

    // Finalize the interrupted stream (if any) and set an "is thinking"
    // status as soon as a chat message arrives. The daemon cancels an
    // in-flight turn when user input lands, but it broadcasts no ToolResult
    // events for the interrupted calls, and the cancelled turn's ResponseDone
    // is swallowed by the had-tool-call keep-open rule — so without an
    // explicit stop the old stream message would stay active (its task cards
    // spinning forever) and the next turn would continue streaming *above*
    // the user's message. Emitting the stop here (same branch, before the
    // status, so in-branch ordering holds) finalizes the old message; the new
    // turn's output can only be produced after the daemon receives SendInput,
    // so it starts a fresh stream below. Replies inside a subagent's
    // dedicated thread finalize that subagent's stream; the status indicator
    // always lives on the root conversation thread.
    let status_actions = chat_messages.flat_map_ordered(q!(|event: SlackEvent| {
        let rt = crate::runtime::get();
        let route = rt
            .subagent_routes
            .lock()
            .expect("bug: lock poisoned")
            .get(&event.thread_ts)
            .cloned();
        let (root_ts, subagent) = match route {
            Some(r) => (r.root_thread_ts, Some(r.subagent_id)),
            None => (event.thread_ts, None),
        };
        let mut actions = Vec::new();
        let has_session = {
            let sessions = rt.sessions.lock().expect("bug: lock poisoned");
            sessions.get(&root_ts).is_some()
        };
        // Only finalize for known sessions (a brand-new thread has nothing to
        // stop). Stale tool bookkeeping must be cleared even when no output
        // was delivered yet, so the clear runs unconditionally for the key.
        let was_in_flight =
            crate::flow::clear_interrupted_turn_state(rt, &root_ts, subagent.as_deref());
        if has_session && was_in_flight {
            actions.push(SlackAction::StreamStop {
                channel: event.channel.clone(),
                thread_ts: root_ts.clone(),
                subagent,
                end_of_response: true,
            });
        }
        actions.push(SlackAction::SetStatus {
            channel: event.channel,
            thread_ts: root_ts,
            status: "is thinking".to_owned(),
        });
        actions
    }));

    // --- Daemon events → Slack actions (streaming responses) ---
    // Uses `flat_map_ordered` so a single event can emit both a status update
    // and a stream action while preserving their order (important so the
    // status clear on stop is never reordered before a "thinking" update).
    let daemon_slack_actions = daemon_events.flat_map_ordered(q!(|de: DaemonEvent| {
        let rt = crate::runtime::get();
        let channel = rt
            .channels
            .lock()
            .expect("bug: lock poisoned")
            .get(&de.thread_ts)
            .cloned()
            .unwrap_or_default();
        let has_thread = !channel.is_empty() && !de.thread_ts.is_empty();
        // Output from a subagent thread is routed into a dedicated Slack
        // thread (created lazily by the I/O layer) so concurrent subagents
        // don't interleave in the root conversation.
        let subagent = {
            let sessions = rt.sessions.lock().expect("bug: lock poisoned");
            crate::flow::subagent_of(&de.message, sessions.get(&de.thread_ts))
        };
        // Key for per-stream state (tool tasks, tool-call flags), distinct
        // per subagent so parallel streams don't share state.
        let skey = crate::slack::stream_key(&de.thread_ts, subagent.as_deref());

        match &de.message {
            infinity_protocol::DaemonMessage::Connected {
                root_thread_id: session_id,
                title,
                ..
            } => {
                {
                    let mut sessions = rt.sessions.lock().expect("bug: lock poisoned");
                    sessions.insert(de.thread_ts.clone(), session_id.clone());
                }
                // Title the agent thread if the session already has one.
                let Some(title) = title.clone() else {
                    return Vec::new();
                };
                if !has_thread {
                    return Vec::new();
                }
                let mut titles = rt.thread_titles.lock().expect("bug: lock poisoned");
                if titles.get(&de.thread_ts) == Some(&title) {
                    return Vec::new();
                }
                titles.insert(de.thread_ts.clone(), title.clone());
                vec![SlackAction::SetThreadTitle {
                    channel: channel.clone(),
                    thread_ts: de.thread_ts,
                    title,
                }]
            }
            infinity_protocol::DaemonMessage::SessionsUpdated { sessions } => {
                // Keep the Slack thread title in sync with the session
                // title (generated by the agent after the first response).
                if !has_thread {
                    return Vec::new();
                }
                let session_id = {
                    let store = rt.sessions.lock().expect("bug: lock poisoned");
                    match store.get(&de.thread_ts).cloned() {
                        Some(id) => id,
                        None => return Vec::new(),
                    }
                };
                let Some(title) = sessions.get(&session_id).and_then(|s| s.title.clone()) else {
                    return Vec::new();
                };
                let mut titles = rt.thread_titles.lock().expect("bug: lock poisoned");
                if titles.get(&de.thread_ts) == Some(&title) {
                    return Vec::new();
                }
                titles.insert(de.thread_ts.clone(), title.clone());
                vec![SlackAction::SetThreadTitle {
                    channel: channel.clone(),
                    thread_ts: de.thread_ts,
                    title,
                }]
            }
            infinity_protocol::DaemonMessage::TextChunk { chunk, .. } => {
                let mut actions = Vec::new();
                // Reflect text generation in the root thread status. Only
                // root output drives the status: subagent streams stop
                // independently and must not re-arm the status after the
                // root response has completed (it would never be cleared).
                if has_thread && subagent.is_none() {
                    actions.push(SlackAction::SetStatus {
                        channel: channel.clone(),
                        thread_ts: de.thread_ts.clone(),
                        status: "is thinking".to_owned(),
                    });
                }
                actions.push(SlackAction::StreamAppend {
                    channel: channel.clone(),
                    thread_ts: de.thread_ts,
                    text: chunk.clone(),
                    subagent,
                });
                actions
            }
            infinity_protocol::DaemonMessage::ToolCall { name, args, .. } => {
                rt.had_tool_call
                    .lock()
                    .expect("bug: lock poisoned")
                    .insert(skey.clone(), true);
                // Render the tool call as a streaming task update. The
                // task is marked complete when the ToolResult arrives (or
                // when the stream stops, as a safety net).
                let seq = rt
                    .tool_task_seq
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let task_id = format!("task_{seq}");
                // Show the tool arguments as the task `details`. Task chunk
                // fields are capped at 256 chars, so truncate.
                let details = {
                    let a = args.trim();
                    if a.chars().count() > 200 {
                        let head: String = a.chars().take(200).collect();
                        format!("{head}…")
                    } else {
                        a.to_owned()
                    }
                };
                rt.tool_tasks
                    .lock()
                    .expect("bug: lock poisoned")
                    .entry(skey)
                    .or_default()
                    .push_back((task_id.clone(), name.clone(), details.clone()));
                let mut actions = Vec::new();
                // Show which tool is running in the root thread status (root
                // output only; see the TextChunk arm).
                if has_thread && subagent.is_none() {
                    actions.push(SlackAction::SetStatus {
                        channel: channel.clone(),
                        thread_ts: de.thread_ts.clone(),
                        status: format!("is running {name}"),
                    });
                }
                actions.push(SlackAction::StreamTaskUpdate {
                    channel: channel.clone(),
                    thread_ts: de.thread_ts,
                    task_id,
                    title: name.clone(),
                    status: "in_progress".to_owned(),
                    details,
                    subagent,
                });
                actions
            }
            infinity_protocol::DaemonMessage::ToolResult { .. } => {
                // Pair the result with its call: an orphan sentinel (call
                // delivered before a restart) or the oldest in-flight tool
                // task (see `pair_tool_result`).
                let popped =
                    crate::flow::pair_tool_result(rt, &skey, &de.thread_ts, subagent.as_deref());
                let mut actions = Vec::new();
                // Tool finished — back to thinking until the next tool/text
                // (root output only; see the TextChunk arm).
                if has_thread && subagent.is_none() {
                    actions.push(SlackAction::SetStatus {
                        channel: channel.clone(),
                        thread_ts: de.thread_ts.clone(),
                        status: "is thinking".to_owned(),
                    });
                }
                if let Some((task_id, title, details)) = popped {
                    actions.push(SlackAction::StreamTaskUpdate {
                        channel: channel.clone(),
                        thread_ts: de.thread_ts,
                        task_id,
                        title,
                        status: "complete".to_owned(),
                        details,
                        subagent,
                    });
                }
                actions
            }
            infinity_protocol::DaemonMessage::ResponseDone { .. } => {
                let had_tool = rt
                    .had_tool_call
                    .lock()
                    .expect("bug: lock poisoned")
                    .remove(&skey)
                    .unwrap_or(false);
                if had_tool {
                    // More output coming after tool execution; keep the stream open.
                    Vec::new()
                } else {
                    // The response is over: discard any stale in-flight tool
                    // entries (results that never arrived, e.g. interrupted
                    // tools). ToolResult matches completions to calls by FIFO
                    // order only, so a stale entry would mislabel the next
                    // response's first completion. (The I/O layer completes
                    // the on-screen cards themselves when it stops the
                    // stream.)
                    if let Some(stale) = rt
                        .tool_tasks
                        .lock()
                        .expect("bug: lock poisoned")
                        .remove(&skey)
                    {
                        if !stale.is_empty() {
                            tracing::debug!(
                                "discarding {} stale tool task(s) for {skey} at response end",
                                stale.len()
                            );
                        }
                    }
                    // Same for unconsumed orphan-result sentinels (results
                    // that never arrived, e.g. interrupted tools): left in
                    // place they would swallow the next response's first
                    // completions.
                    if let Some(stale) = rt
                        .resume_orphan_results
                        .lock()
                        .expect("bug: lock poisoned")
                        .remove(&skey)
                    {
                        if stale > 0 {
                            tracing::debug!(
                                "discarding {stale} stale orphan-result sentinel(s) for {skey} \
                                 at response end"
                            );
                        }
                    }
                    vec![SlackAction::StreamStop {
                        channel: channel.clone(),
                        thread_ts: de.thread_ts,
                        subagent,
                        end_of_response: true,
                    }]
                }
            }
            infinity_protocol::DaemonMessage::Error { text, .. } => {
                // Finalize any in-flight stream first so the error message —
                // and any output after it — appears below the streamed text
                // rather than the stream continuing to edit the message above
                // the error (discontinuity). For root errors this also clears
                // the thread status, which would otherwise linger if no
                // ResponseDone follows. (StreamStop is a no-op when no stream
                // is active.)
                vec![
                    SlackAction::StreamStop {
                        channel: channel.clone(),
                        thread_ts: de.thread_ts.clone(),
                        subagent: subagent.clone(),
                        end_of_response: false,
                    },
                    SlackAction::PostMessage {
                        channel: channel.clone(),
                        text: format!("⚠️ {text}"),
                        thread_ts: Some(de.thread_ts),
                        subagent,
                    },
                ]
            }
            infinity_protocol::DaemonMessage::UserChoiceRequired {
                id,
                prompt,
                choices,
                default,
                ..
            } => {
                let buttons: Vec<serde_json::Value> = choices
                    .iter()
                    .enumerate()
                    .map(|(i, choice)| {
                        let label = if i == *default {
                            format!("{choice} ✓")
                        } else {
                            choice.clone()
                        };
                        serde_json::json!({
                            "type": "button",
                            "text": { "type": "plain_text", "text": label },
                            "action_id": format!("choice_{id}_{i}"),
                            "value": i.to_string(),
                        })
                    })
                    .collect();
                let blocks = serde_json::json!([
                    {
                        "type": "section",
                        "text": { "type": "mrkdwn", "text": format!("⚠️ *{prompt}*") }
                    },
                    {
                        "type": "actions",
                        "block_id": format!("choice_{id}"),
                        "elements": buttons
                    }
                ]);
                // Finalize any in-flight stream first so the buttons — and
                // any output produced after the choice — appear as new
                // messages rather than continuing the pre-choice message.
                // (StreamStop is a no-op when no stream is active.)
                vec![
                    SlackAction::StreamStop {
                        channel: channel.clone(),
                        thread_ts: de.thread_ts.clone(),
                        subagent: subagent.clone(),
                        end_of_response: false,
                    },
                    SlackAction::PostBlocks {
                        channel: channel.clone(),
                        fallback_text: prompt.clone(),
                        blocks,
                        thread_ts: Some(de.thread_ts),
                        choice_id: Some(id.clone()),
                        subagent,
                    },
                ]
            }
            infinity_protocol::DaemonMessage::UserChoiceComplete { choice_id } => {
                // The choice was resolved (by another client, timeout, or
                // interruption). Dismiss the button message if we posted one.
                vec![SlackAction::DismissChoiceButtons {
                    choice_id: choice_id.clone(),
                }]
            }
            infinity_protocol::DaemonMessage::CompactionApplied { .. } => {
                // Context was compacted — via the "Compact session" shortcut
                // or daemon-initiated auto-compaction; both are worth
                // surfacing. Routed into the subagent's dedicated thread when
                // a subagent compacted.
                // (Replay catch-up after a restart never re-posts this: the
                // resume delta forwards only text/tool events.)
                if !has_thread {
                    return Vec::new();
                }
                vec![SlackAction::PostMessage {
                    channel: channel.clone(),
                    text: "🗜️ Context compacted — older conversation was summarized to free up \
                           context space."
                        .to_owned(),
                    thread_ts: Some(de.thread_ts),
                    subagent,
                }]
            }
            _ => Vec::new(),
        }
    }));

    // Merge all slack action streams.
    let slack_actions = daemon_slack_actions
        .merge_ordered(
            status_actions,
            nondet!(/**
                daemon-sourced actions (streaming text, tool calls) have non-deterministic
                ordering w.r.t. status actions ("Thinking...") because they originate from
                independent event sources
            */),
        )
        .merge_ordered(
            button_stop_actions,
            nondet!(/**
                button-stop actions have non-deterministic ordering w.r.t. other slack actions
                because they originate from user interaction events
            */),
        )
        .merge_ordered(
            button_update_actions,
            nondet!(/**
                button-update actions have non-deterministic ordering w.r.t. other slack actions
                because they originate from user interaction events
            */),
        )
        .merge_ordered(
            command_response_actions,
            nondet!(/**
                slash command responses have non-deterministic ordering w.r.t. other slack
                actions because they originate from user interaction events
            */),
        )
        .merge_ordered(
            app_home_actions,
            nondet!(/**
                suggested-prompt actions have non-deterministic ordering w.r.t. other slack
                actions because they originate from user interaction events
            */),
        )
        .merge_ordered(
            session_stop_actions,
            nondet!(/**
                stop-button actions have non-deterministic ordering w.r.t. other slack
                actions because they originate from user interaction events
            */),
        )
        .merge_ordered(
            shortcut_actions,
            nondet!(/**
                shortcut actions have non-deterministic ordering w.r.t. other slack
                actions because they originate from user interaction events
            */),
        )
        .merge_ordered(
            rewind_actions,
            nondet!(/**
                fork-thread actions have non-deterministic ordering w.r.t. other slack
                actions because they originate from user interaction events
            */),
        )
        .merge_ordered(
            fork_actions,
            nondet!(/**
                fork status actions have non-deterministic ordering w.r.t. other slack
                actions because they originate from user interaction events
            */),
        );

    (slack_actions, daemon_commands)
}

/// Quote `text` as a Slack blockquote, truncated to a readable snippet.
pub fn quote_snippet(text: &str) -> String {
    const MAX: usize = 500;
    let truncated: String = if text.chars().count() > MAX {
        let head: String = text.chars().take(MAX).collect();
        format!("{head}…")
    } else {
        text.to_owned()
    };
    truncated
        .lines()
        .map(|l| format!("> {l}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Identify the subagent that produced a daemon message, if any.
///
/// Returns the subagent's thread id (in `ThreadRef` string form) when the
/// message carries a `thread_id` different from the session's root thread.
/// Returns `None` for root-thread output — and also when the root thread is
/// not yet known, in which case output is conservatively routed to the root
/// conversation rather than forking a dedicated Slack thread.
/// Pair an arriving `ToolResult` with its tool call for the given stream.
///
/// `ToolResult` carries no call id, so pairing is positional. Two sources, in
/// priority order:
///
/// 1. **Orphan sentinels** (`Runtime::resume_orphan_results`): results for
///    calls that were delivered *before a restart*. Their cards live in
///    finalized messages of the previous process (completed at drain time),
///    so the result produces no card update — returns `None` — but it must
///    not consume a post-restart call's queue entry. The delivery cursor
///    still advances so a second restart during the same response counts
///    delivered results correctly.
/// 2. The oldest in-flight tool task of the stream's FIFO queue — returned
///    as `(task_id, title, details)` for the "complete" card update.
pub fn pair_tool_result(
    rt: &crate::runtime::Runtime,
    skey: &str,
    thread_ts: &str,
    subagent: Option<&str>,
) -> Option<(String, String, String)> {
    let consumed_orphan = {
        let mut orphans = rt.resume_orphan_results.lock().expect("bug: lock poisoned");
        match orphans.get_mut(skey) {
            Some(n) if *n > 0 => {
                *n -= 1;
                if *n == 0 {
                    orphans.remove(skey);
                }
                true
            }
            _ => false,
        }
    };
    if consumed_orphan {
        rt.stream_progress
            .lock()
            .expect("bug: lock poisoned")
            .entry((thread_ts.to_owned(), subagent.map(str::to_owned)))
            .or_default()
            .tool_results += 1;
        return None;
    }
    rt.tool_tasks
        .lock()
        .expect("bug: lock poisoned")
        .get_mut(skey)
        .and_then(|q| q.pop_front())
}

/// Clear the per-stream bookkeeping of a turn the user just interrupted by
/// sending a message, returning whether anything suggested a response was in
/// flight.
///
/// When user input arrives mid-turn, the daemon cancels the in-flight step
/// and settles unanswered tool calls with *synthetic* results that are
/// written to history only — no `ToolResult` events are broadcast — so the
/// stream's FIFO queue entries for them would never be popped and would
/// mislabel the next turn's completions. The orphan-result sentinels from a
/// resume are stale for the same reason.
///
/// `had_tool_call` is deliberately **left alone**: the cancelled step still
/// broadcasts exactly one `ResponseDone`, which consumes the flag (and is
/// swallowed by the keep-open rule — harmless, since the stream was already
/// finalized by the caller's `StreamStop`). Clearing it here would instead
/// let that `ResponseDone` emit a second stop that wipes the fresh
/// "is thinking" status.
pub fn clear_interrupted_turn_state(
    rt: &crate::runtime::Runtime,
    thread_ts: &str,
    subagent: Option<&str>,
) -> bool {
    let skey = crate::slack::stream_key(thread_ts, subagent);
    let had_tasks = rt
        .tool_tasks
        .lock()
        .expect("bug: lock poisoned")
        .remove(&skey)
        .map(|q| !q.is_empty())
        .unwrap_or(false);
    let orphans = rt
        .resume_orphan_results
        .lock()
        .expect("bug: lock poisoned")
        .remove(&skey)
        .unwrap_or(0);
    let delivering = rt
        .stream_progress
        .lock()
        .expect("bug: lock poisoned")
        .contains_key(&(thread_ts.to_owned(), subagent.map(str::to_owned)));
    if had_tasks || orphans > 0 {
        tracing::debug!(
            "cleared interrupted-turn state for {skey} (open tasks: {had_tasks}, \
             orphan sentinels: {orphans})"
        );
    }
    had_tasks || orphans > 0 || delivering
}

pub fn subagent_of(
    msg: &infinity_protocol::DaemonMessage,
    root: Option<&infinity_protocol::ThreadRef>,
) -> Option<String> {
    use infinity_protocol::DaemonMessage as DM;
    let thread_id = match msg {
        DM::StartOutput { thread_id }
        | DM::TextChunk { thread_id, .. }
        | DM::ToolCall { thread_id, .. }
        | DM::ToolResult { thread_id, .. }
        | DM::Info { thread_id, .. }
        | DM::ResponseDone { thread_id, .. }
        | DM::UserInputEcho { thread_id, .. }
        | DM::SubscriptionEvent { thread_id, .. }
        | DM::OAuthRequired { thread_id, .. }
        | DM::UserChoiceRequired { thread_id, .. }
        | DM::ThinkingStart { thread_id }
        | DM::ThinkingEnd { thread_id }
        | DM::ThinkingChunk { thread_id, .. }
        | DM::CompactionApplied { thread_id }
        | DM::Error { thread_id, .. } => thread_id.as_ref()?,
        _ => return None,
    };
    match root {
        Some(root) if root != thread_id => Some(thread_id.to_string()),
        _ => None,
    }
}

/// Build the Block Kit view for the `/model` picker modal. Returns `None` if
/// no models are available yet (so the caller can fall back to a text
/// response). The current default is preselected as the initial option.
///
/// `response_url` is stashed in the view's `private_metadata` so the modal
/// submission handler can post an ephemeral confirmation back to the user.
pub fn build_model_picker_view(response_url: &str) -> Option<serde_json::Value> {
    let rt = crate::runtime::get();
    let models = rt.available_models.lock().expect("bug: lock poisoned");
    if models.is_empty() {
        return None;
    }
    let current = rt.default_model.lock().expect("bug: lock poisoned");

    let mut options = Vec::with_capacity(models.len());
    let mut initial_option: Option<serde_json::Value> = None;
    for m in models.iter() {
        let value = format!("{}/{}", m.provider_id, m.model_id);
        // Slack option `value` is limited to 75 chars; skip anything longer so
        // it can't break the whole view (still switchable via `/model <id>`).
        if value.chars().count() > 75 {
            continue;
        }
        // Slack option `text` is limited to 75 chars.
        let mut label = format!("{} ({value})", m.display_name);
        if label.chars().count() > 75 {
            label = label.chars().take(75).collect();
        }
        let option = serde_json::json!({
            "text": { "type": "plain_text", "text": label },
            "value": value,
        });
        let is_current = current
            .as_ref()
            .map(|c| c.provider_id == m.provider_id && c.model_id == m.model_id)
            .unwrap_or(false);
        if is_current {
            initial_option = Some(option.clone());
        }
        options.push(option);
    }

    if options.is_empty() {
        return None;
    }

    // Use a `static_select` dropdown (supports up to 100 options) rather than
    // `radio_buttons`, which Slack caps at 10 options.
    let mut element = serde_json::json!({
        "type": "static_select",
        "action_id": MODEL_PICKER_ACTION_ID,
        "placeholder": { "type": "plain_text", "text": "Pick a model" },
        "options": options,
    });
    if let Some(initial) = initial_option {
        element["initial_option"] = initial;
    }

    Some(serde_json::json!({
        "type": "modal",
        "callback_id": MODEL_PICKER_CALLBACK_ID,
        "private_metadata": response_url,
        "title": { "type": "plain_text", "text": "Select model" },
        "submit": { "type": "plain_text", "text": "Switch" },
        "close": { "type": "plain_text", "text": "Cancel" },
        "blocks": [
            {
                "type": "input",
                "block_id": MODEL_PICKER_BLOCK_ID,
                "label": { "type": "plain_text", "text": "Default model for new sessions" },
                "element": element,
            }
        ],
    }))
}

/// Resolve which session a "Compact session" shortcut invocation targets.
///
/// The shortcut is invoked on a message, so `thread_ts` identifies the exact
/// Slack thread: the root conversation thread of a session, or a subagent's
/// dedicated thread (mapped back to its session via `subagent_routes` —
/// compaction is session-scoped, so both land on the same session root).
///
/// Returns `Ok((root_thread_ts, session_root))` or `Err(user-facing message)`.
pub fn resolve_compact_session(
    thread_ts: &str,
) -> Result<(String, infinity_protocol::ThreadRef), String> {
    let rt = crate::runtime::get();
    // A subagent's dedicated thread belongs to the session keyed by its root
    // conversation thread.
    let root_ts = {
        let routes = rt.subagent_routes.lock().expect("bug: lock poisoned");
        routes
            .get(thread_ts)
            .map(|r| r.root_thread_ts.clone())
            .unwrap_or_else(|| thread_ts.to_owned())
    };
    let session = {
        let sessions = rt.sessions.lock().expect("bug: lock poisoned");
        sessions.get(&root_ts).cloned()
    };
    match session {
        Some(session) => Ok((root_ts, session)),
        None => Err(
            "🗜️ This thread has no Infinity session to compact. Send the bot a message here \
             first."
                .to_owned(),
        ),
    }
}

/// Handle the `/model` slash command. With no argument, lists available
/// models and the current default; with an argument, switches the default
/// model for new sessions. Returns the response text.
pub fn handle_model_command(arg: &str) -> String {
    let rt = crate::runtime::get();
    if arg.is_empty() {
        // List available models and show current default.
        let models = rt.available_models.lock().expect("bug: lock poisoned");
        let current = rt.default_model.lock().expect("bug: lock poisoned");
        if models.is_empty() {
            return "No models available yet. The model list is populated after the first session is created.".to_owned();
        }
        let mut lines = vec!["*Available models:*".to_owned()];
        for m in models.iter() {
            let is_current = current
                .as_ref()
                .map(|c| c.provider_id == m.provider_id && c.model_id == m.model_id)
                .unwrap_or(false);
            let marker = if is_current { " ← current" } else { "" };
            lines.push(format!(
                "• `{}/{}` — {}{marker}",
                m.provider_id, m.model_id, m.display_name
            ));
        }
        if current.is_none() {
            lines.push("\n_No override set — using daemon default._".to_owned());
        }
        lines.push("\nUse `/model <provider_id>/<model_id>` to switch.".to_owned());
        return lines.join("\n");
    }

    // Try to switch to the named model.
    let found_model = {
        let models = rt.available_models.lock().expect("bug: lock poisoned");
        if let Some((provider_id, model_id)) = arg.split_once('/') {
            models
                .iter()
                .find(|m| m.provider_id == provider_id && m.model_id == model_id)
                .map(|m| {
                    (
                        m.provider_id.clone(),
                        m.model_id.clone(),
                        m.display_name.clone(),
                    )
                })
        } else {
            // Allow matching by display_name or model_id alone.
            models
                .iter()
                .find(|m| m.model_id == arg || m.display_name.to_lowercase() == arg.to_lowercase())
                .map(|m| {
                    (
                        m.provider_id.clone(),
                        m.model_id.clone(),
                        m.display_name.clone(),
                    )
                })
        }
    };
    if let Some((provider_id, model_id, display)) = found_model {
        let model_ref = infinity_protocol::ModelRef {
            provider_id,
            model_id,
        };
        {
            let mut current = rt.default_model.lock().expect("bug: lock poisoned");
            *current = Some(model_ref.clone());
        }
        // Persist the selection to slack.json so it survives restarts.
        if let Err(e) = rt.config.save_default_model(&model_ref) {
            tracing::error!("failed to persist default model to slack.json: {e}");
            return format!(
                "✅ Default model switched to *{display}* (`{arg}`).\nNew sessions will use this model.\n⚠️ Failed to save to slack.json — the selection will reset on restart."
            );
        }
        format!(
            "✅ Default model switched to *{display}* (`{arg}`).\nNew sessions will use this model."
        )
    } else {
        format!("❌ Model `{arg}` not found. Use `/model` to list available models.")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subagent_of_distinguishes_root_and_subagent_output() {
        let root = infinity_protocol::ThreadRef::from("root1");
        let chunk = |thread_id: Option<&str>| infinity_protocol::DaemonMessage::TextChunk {
            thread_id: thread_id.map(Into::into),
            chunk: "x".to_owned(),
        };

        // A different thread id is a subagent.
        assert_eq!(
            subagent_of(&chunk(Some("sub1")), Some(&root)),
            Some("sub1".to_owned())
        );
        // The root's own id is not.
        assert_eq!(subagent_of(&chunk(Some("root1")), Some(&root)), None);
        // No thread id means root output.
        assert_eq!(subagent_of(&chunk(None), Some(&root)), None);
        // Unknown root: routed to the root conversation, not forked.
        assert_eq!(subagent_of(&chunk(Some("sub1")), None), None);
        // Messages without a per-thread id are never subagent output.
        let welcome = infinity_protocol::DaemonMessage::UserChoiceComplete {
            choice_id: "c1".into(),
        };
        assert_eq!(subagent_of(&welcome, Some(&root)), None);
    }

    #[test]
    fn clear_interrupted_turn_state_scopes_to_the_stream_key() {
        crate::runtime::ensure_test_init();
        let rt = crate::runtime::get();
        // Unique ids: the runtime is shared process-wide across tests.
        let thread_ts = "991.1";
        let root_key = crate::slack::stream_key(thread_ts, None);
        let sub_key = crate::slack::stream_key(thread_ts, Some("subA"));

        // Idle thread: nothing to clear, nothing in flight.
        assert!(!clear_interrupted_turn_state(rt, thread_ts, None));

        // Root turn in flight (open tool task + delivery cursor + a stale
        // orphan sentinel); the subagent's stream has its own open task.
        rt.tool_tasks.lock().expect("lock").insert(
            root_key.clone(),
            [("t_root".to_owned(), "bash".to_owned(), "{}".to_owned())]
                .into_iter()
                .collect(),
        );
        rt.tool_tasks.lock().expect("lock").insert(
            sub_key.clone(),
            [("t_sub".to_owned(), "grep".to_owned(), "{}".to_owned())]
                .into_iter()
                .collect(),
        );
        rt.resume_orphan_results
            .lock()
            .expect("lock")
            .insert(root_key.clone(), 2);
        rt.stream_progress
            .lock()
            .expect("lock")
            .insert((thread_ts.to_owned(), None), Default::default());

        // Clearing the ROOT stream reports in-flight and leaves the subagent
        // stream untouched.
        assert!(clear_interrupted_turn_state(rt, thread_ts, None));
        assert!(!rt.tool_tasks.lock().expect("lock").contains_key(&root_key));
        assert!(!rt
            .resume_orphan_results
            .lock()
            .expect("lock")
            .contains_key(&root_key));
        assert!(rt.tool_tasks.lock().expect("lock").contains_key(&sub_key));

        // The delivery cursor alone (text-only turn) still reports in-flight;
        // the cursor itself is owned by the I/O layer's end-of-response path
        // and is not removed here.
        assert!(clear_interrupted_turn_state(rt, thread_ts, None));

        // The subagent stream clears independently.
        assert!(clear_interrupted_turn_state(rt, thread_ts, Some("subA")));
        assert!(!rt.tool_tasks.lock().expect("lock").contains_key(&sub_key));

        // Clean up shared runtime state.
        rt.stream_progress
            .lock()
            .expect("lock")
            .remove(&(thread_ts.to_owned(), None));
    }

    #[test]
    fn pair_tool_result_consumes_sentinels_before_the_queue() {
        crate::runtime::ensure_test_init();
        let rt = crate::runtime::get();
        // Unique ids: the runtime is shared process-wide across tests.
        let thread_ts = "881.1";
        let skey = crate::slack::stream_key(thread_ts, None);

        // Two orphan sentinels (calls delivered before a restart) and one
        // real in-flight task from a post-restart call.
        rt.resume_orphan_results
            .lock()
            .expect("lock")
            .insert(skey.clone(), 2);
        rt.tool_tasks.lock().expect("lock").insert(
            skey.clone(),
            [("task_new".to_owned(), "bash".to_owned(), "{}".to_owned())]
                .into_iter()
                .collect(),
        );

        // First two results pair with sentinels: no card update, cursor
        // advances, the real task stays queued.
        assert_eq!(pair_tool_result(rt, &skey, thread_ts, None), None);
        assert_eq!(pair_tool_result(rt, &skey, thread_ts, None), None);
        assert_eq!(
            rt.stream_progress
                .lock()
                .expect("lock")
                .get(&(thread_ts.to_owned(), None))
                .map(|c| c.tool_results),
            Some(2)
        );
        assert!(!rt
            .resume_orphan_results
            .lock()
            .expect("lock")
            .contains_key(&skey));

        // Third result pairs with the new call's entry (card completes)...
        assert_eq!(
            pair_tool_result(rt, &skey, thread_ts, None),
            Some(("task_new".to_owned(), "bash".to_owned(), "{}".to_owned()))
        );
        // ... and a surplus result with nothing queued emits nothing.
        assert_eq!(pair_tool_result(rt, &skey, thread_ts, None), None);

        // Clean up shared runtime state.
        rt.tool_tasks.lock().expect("lock").remove(&skey);
        rt.stream_progress
            .lock()
            .expect("lock")
            .remove(&(thread_ts.to_owned(), None));
    }

    #[test]
    fn resolve_compact_session_maps_threads_to_their_session() {
        crate::runtime::ensure_test_init();
        let rt = crate::runtime::get();
        // Unique ids: runtime state is shared across tests in this binary.
        let (root, anchor) = ("901.1", "901.2");
        rt.sessions
            .lock()
            .expect("lock")
            .insert_ephemeral(root.into(), "sess_cshort".into());
        rt.subagent_routes.lock().expect("lock").insert(
            anchor.into(),
            crate::runtime::SubagentRoute {
                root_thread_ts: root.into(),
                subagent_id: "sub_cshort".into(),
            },
        );

        // Root conversation thread resolves to its own session.
        let (ts, session) = resolve_compact_session(root).expect("root thread");
        assert_eq!(ts, root);
        assert_eq!(session.to_string(), "sess_cshort");
        // A subagent's dedicated thread resolves to the owning session root
        // (compaction is session-scoped).
        let (ts, session) = resolve_compact_session(anchor).expect("subagent thread");
        assert_eq!(ts, root);
        assert_eq!(session.to_string(), "sess_cshort");
        // A thread with no session gets a friendly error.
        let err = resolve_compact_session("901.9").expect_err("no session");
        assert!(err.contains("no Infinity session"), "got: {err}");

        // Clean up shared runtime state.
        rt.subagent_routes.lock().expect("lock").remove(anchor);
    }

    #[test]
    fn model_picker_view_reflects_available_models() {
        crate::runtime::ensure_test_init();
        let rt = crate::runtime::get();

        // With no models available, the picker can't be built (caller falls
        // back to a text response).
        rt.available_models.lock().expect("lock").clear();
        assert!(build_model_picker_view("https://example.com").is_none());

        {
            let mut models = rt.available_models.lock().expect("lock");
            *models = vec![
                infinity_protocol::ModelInfo {
                    display_name: "Claude Sonnet 4".to_owned(),
                    provider_id: "bedrock".to_owned(),
                    model_id: "claude-sonnet-4".to_owned(),
                    context_window: 200_000,
                },
                infinity_protocol::ModelInfo {
                    display_name: "Claude Haiku".to_owned(),
                    provider_id: "bedrock".to_owned(),
                    model_id: "claude-haiku".to_owned(),
                    context_window: 200_000,
                },
            ];
        }
        {
            let mut current = rt.default_model.lock().expect("lock");
            *current = Some(infinity_protocol::ModelRef {
                provider_id: "bedrock".to_owned(),
                model_id: "claude-haiku".to_owned(),
            });
        }

        let view = build_model_picker_view("https://hooks.slack.com/commands/T1/1/x")
            .expect("view should build with models present");

        assert_eq!(view["type"], "modal");
        assert_eq!(view["callback_id"], MODEL_PICKER_CALLBACK_ID);
        assert_eq!(
            view["private_metadata"],
            "https://hooks.slack.com/commands/T1/1/x"
        );

        let element = &view["blocks"][0]["element"];
        assert_eq!(element["type"], "static_select");
        let options = element["options"].as_array().expect("options array");
        assert_eq!(options.len(), 2);
        assert_eq!(options[0]["value"], "bedrock/claude-sonnet-4");
        // The current default is preselected.
        assert_eq!(element["initial_option"]["value"], "bedrock/claude-haiku");

        // Reset shared runtime state so other tests are unaffected.
        rt.available_models.lock().expect("lock").clear();
        *rt.default_model.lock().expect("lock") = None;
    }
}
