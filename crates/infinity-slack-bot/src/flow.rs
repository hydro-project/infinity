//! Defines the Hydro dataflow graph for the Slack bot.
//!
//! Two sidecars feed the dataflow:
//! - Slack sidecar: `Stream<SlackEvent>` in, `Stream<SlackAction>` out
//! - Daemon sidecar: `Stream<DaemonEvent>` in, `Stream<DaemonCommand>` out

use hydro_lang::prelude::*;

use crate::daemon_sidecar::{DaemonCommand, DaemonEvent};
use crate::sidecar::{SlackAction, SlackEvent};

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
    let button_stop_actions = button_clicks
        .clone()
        .map(q!(|event: crate::sidecar::SlackEvent| {
            crate::sidecar::SlackAction::StreamStop {
                channel: event.channel,
                thread_ts: event.thread_ts,
            }
        }));

    // --- Button clicks → update the button message to show which choice was selected ---
    let button_update_actions =
        button_clicks
            .clone()
            .filter_map(q!(|event: crate::sidecar::SlackEvent| {
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
                Some(crate::sidecar::SlackAction::UpdateMessage {
                    channel: event.channel,
                    ts: message_ts,
                    text: format!("Selected: {selected_label}"),
                    blocks,
                })
            }));

    // --- Button clicks → AnswerChoice commands ---
    let button_commands = button_clicks.filter_map(q!(|event: crate::sidecar::SlackEvent| {
        let rt = crate::runtime::get();
        rt.channels
            .lock()
            .expect("bug: lock poisoned")
            .insert(event.thread_ts.clone(), event.channel.clone());

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
            .remove(&choice_id);

        Some(crate::daemon_sidecar::DaemonCommand::AnswerChoice {
            thread_ts: event.thread_ts,
            choice_id,
            selected,
        })
    }));

    // --- Partition non-buttons into slash commands vs regular messages ---
    let (regular_messages, slash_commands) =
        non_buttons.partition(q!(|event: &crate::sidecar::SlackEvent| {
            event.slash_command.is_none()
        }));

    // --- Slash commands → respond via the command's response_url ---
    let command_response_actions =
        slash_commands.filter_map(q!(|event: crate::sidecar::SlackEvent| {
            let response_url = event.response_url?;
            let command = event.slash_command.unwrap_or_default();

            let response = match command.as_str() {
                "/model" => crate::flow::handle_model_command(event.text.trim()),
                other => format!("Unknown command `{other}`."),
            };

            Some(crate::sidecar::SlackAction::CommandResponse {
                response_url,
                text: response,
            })
        }));

    // --- Partition regular messages into app-home opens vs chat messages ---
    let (chat_messages, app_home_events) =
        regular_messages.partition(q!(|event: &crate::sidecar::SlackEvent| {
            !event.is_app_home_opened
        }));

    // --- App-home opens → pin suggested prompts (once per user per run) ---
    let app_home_actions = app_home_events.filter_map(q!(|event: crate::sidecar::SlackEvent| {
        let rt = crate::runtime::get();
        let mut seen = rt.app_home_seen.lock().expect("bug: lock poisoned");
        if seen.insert(event.user) {
            Some(crate::sidecar::SlackAction::SetSuggestedPrompts {
                channel: event.channel,
            })
        } else {
            None
        }
    }));

    // --- Chat messages → CreateSession / SendInput commands ---
    let message_commands =
        chat_messages
            .clone()
            .filter_map(q!(|event: crate::sidecar::SlackEvent| {
                let rt = crate::runtime::get();
                rt.channels
                    .lock()
                    .expect("bug: lock poisoned")
                    .insert(event.thread_ts.clone(), event.channel.clone());

                let existing = {
                    let sessions = rt.sessions.lock().expect("bug: lock poisoned");
                    sessions.get(&event.thread_ts).cloned()
                };
                if let Some(session_id) = existing {
                    Some(crate::daemon_sidecar::DaemonCommand::SendInput {
                        thread_ts: event.thread_ts,
                        session_id,
                        text: event.text.trim().to_owned(),
                    })
                } else {
                    // Stash the text to send after Connected arrives.
                    let mut pending = rt.pending_input.lock().expect("bug: lock poisoned");
                    pending.insert(event.thread_ts.clone(), event.text.trim().to_owned());
                    let model = rt.default_model.lock().expect("bug: lock poisoned").clone();
                    Some(crate::daemon_sidecar::DaemonCommand::CreateSession {
                        thread_ts: event.thread_ts,
                        cwd: rt.config.default_cwd.clone(),
                        model,
                    })
                }
            }));

    let daemon_commands = button_commands.merge_ordered(
        message_commands,
        nondet!(/**
            a button command has non-deterministic ordering w.r.t. message commands
            because batching may interleave them arbitrarily
        */),
    );

    // Set "Thinking..." status when a chat message arrives.
    let status_actions = chat_messages.map(q!(|event: crate::sidecar::SlackEvent| {
        crate::sidecar::SlackAction::SetStatus {
            channel: event.channel,
            thread_ts: event.thread_ts,
            status: "Thinking...".to_owned(),
        }
    }));

    // --- Daemon events → Slack actions (streaming responses) ---
    let daemon_slack_actions =
        daemon_events.filter_map(q!(|de: crate::daemon_sidecar::DaemonEvent| {
            let rt = crate::runtime::get();
            let channel = rt
                .channels
                .lock()
                .expect("bug: lock poisoned")
                .get(&de.thread_ts)
                .cloned()
                .unwrap_or_default();

            match &de.message {
                infinity_protocol::DaemonMessage::Connected {
                    session_id, title, ..
                } => {
                    {
                        let mut sessions = rt.sessions.lock().expect("bug: lock poisoned");
                        sessions.insert(de.thread_ts.clone(), session_id.clone());
                    }
                    // Title the agent thread if the session already has one.
                    let title = title.clone()?;
                    if channel.is_empty() || de.thread_ts.is_empty() {
                        return None;
                    }
                    let mut titles = rt.thread_titles.lock().expect("bug: lock poisoned");
                    if titles.get(&de.thread_ts) == Some(&title) {
                        return None;
                    }
                    titles.insert(de.thread_ts.clone(), title.clone());
                    Some(crate::sidecar::SlackAction::SetThreadTitle {
                        channel: channel.clone(),
                        thread_ts: de.thread_ts,
                        title,
                    })
                }
                infinity_protocol::DaemonMessage::SessionsUpdated { sessions } => {
                    // Keep the Slack thread title in sync with the session
                    // title (generated by the agent after the first response).
                    if channel.is_empty() || de.thread_ts.is_empty() {
                        return None;
                    }
                    let session_id = {
                        let store = rt.sessions.lock().expect("bug: lock poisoned");
                        store.get(&de.thread_ts).cloned()?
                    };
                    let title = sessions.get(&session_id)?.title.clone()?;
                    let mut titles = rt.thread_titles.lock().expect("bug: lock poisoned");
                    if titles.get(&de.thread_ts) == Some(&title) {
                        return None;
                    }
                    titles.insert(de.thread_ts.clone(), title.clone());
                    Some(crate::sidecar::SlackAction::SetThreadTitle {
                        channel: channel.clone(),
                        thread_ts: de.thread_ts,
                        title,
                    })
                }
                infinity_protocol::DaemonMessage::TextChunk { chunk, .. } => {
                    Some(crate::sidecar::SlackAction::StreamAppend {
                        channel: channel.clone(),
                        thread_ts: de.thread_ts,
                        text: chunk.clone(),
                    })
                }
                infinity_protocol::DaemonMessage::ToolCall { name, .. } => {
                    rt.had_tool_call
                        .lock()
                        .expect("bug: lock poisoned")
                        .insert(de.thread_ts.clone(), true);
                    // Render the tool call as a streaming task update. The
                    // task is marked complete when the ToolResult arrives (or
                    // when the stream stops, as a safety net).
                    let seq = rt
                        .tool_task_seq
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    let task_id = format!("task_{seq}");
                    rt.tool_tasks
                        .lock()
                        .expect("bug: lock poisoned")
                        .entry(de.thread_ts.clone())
                        .or_default()
                        .push_back((task_id.clone(), name.clone()));
                    Some(crate::sidecar::SlackAction::StreamTaskUpdate {
                        channel: channel.clone(),
                        thread_ts: de.thread_ts,
                        task_id,
                        title: name.clone(),
                        status: "in_progress".to_owned(),
                    })
                }
                infinity_protocol::DaemonMessage::ToolResult { .. } => {
                    // Complete the oldest in-flight tool task for this thread.
                    let popped = rt
                        .tool_tasks
                        .lock()
                        .expect("bug: lock poisoned")
                        .get_mut(&de.thread_ts)
                        .and_then(|q| q.pop_front());
                    popped.map(
                        |(task_id, title)| crate::sidecar::SlackAction::StreamTaskUpdate {
                            channel: channel.clone(),
                            thread_ts: de.thread_ts,
                            task_id,
                            title,
                            status: "complete".to_owned(),
                        },
                    )
                }
                infinity_protocol::DaemonMessage::ResponseDone { .. } => {
                    let had_tool = rt
                        .had_tool_call
                        .lock()
                        .expect("bug: lock poisoned")
                        .remove(&de.thread_ts)
                        .unwrap_or(false);
                    if had_tool {
                        // More output coming after tool execution; keep the stream open.
                        None
                    } else {
                        Some(crate::sidecar::SlackAction::StreamStop {
                            channel: channel.clone(),
                            thread_ts: de.thread_ts,
                        })
                    }
                }
                infinity_protocol::DaemonMessage::Error { text, .. } => {
                    Some(crate::sidecar::SlackAction::PostMessage {
                        channel: channel.clone(),
                        text: format!("⚠️ {text}"),
                        thread_ts: Some(de.thread_ts),
                    })
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
                    Some(crate::sidecar::SlackAction::PostBlocks {
                        channel: channel.clone(),
                        fallback_text: prompt.clone(),
                        blocks,
                        thread_ts: Some(de.thread_ts),
                        choice_id: Some(id.clone()),
                    })
                }
                infinity_protocol::DaemonMessage::UserChoiceComplete { choice_id } => {
                    // The choice was resolved (by another client, timeout, or
                    // interruption). Dismiss the button message if we posted one.
                    Some(crate::sidecar::SlackAction::DismissChoiceButtons {
                        choice_id: choice_id.clone(),
                    })
                }
                _ => None,
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
        );

    (slack_actions, daemon_commands)
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
        let mut current = rt.default_model.lock().expect("bug: lock poisoned");
        *current = Some(model_ref);
        format!(
            "✅ Default model switched to *{display}* (`{arg}`).\nNew sessions will use this model."
        )
    } else {
        format!("❌ Model `{arg}` not found. Use `/model` to list available models.")
    }
}
