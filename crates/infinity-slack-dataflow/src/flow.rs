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
        SlackAction::StreamStop {
            channel: event.channel,
            thread_ts: event.thread_ts,
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

        Some(DaemonCommand::AnswerChoice {
            thread_ts: event.thread_ts,
            choice_id,
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
    let (chat_messages, app_home_events) =
        regular_messages.partition(q!(|event: &SlackEvent| { !event.is_app_home_opened }));

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
        rt.channels
            .lock()
            .expect("bug: lock poisoned")
            .insert(event.thread_ts.clone(), event.channel.clone());

        let existing = {
            let sessions = rt.sessions.lock().expect("bug: lock poisoned");
            sessions.get(&event.thread_ts).cloned()
        };
        if let Some(session_id) = existing {
            Some(DaemonCommand::SendInput {
                thread_ts: event.thread_ts,
                session_id,
                text: event.text.trim().to_owned(),
            })
        } else {
            // Stash the text to send after Connected arrives.
            let mut pending = rt.pending_input.lock().expect("bug: lock poisoned");
            pending.insert(event.thread_ts.clone(), event.text.trim().to_owned());
            let model = rt.default_model.lock().expect("bug: lock poisoned").clone();
            Some(DaemonCommand::CreateSession {
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

    // Set an "is thinking" status as soon as a chat message arrives.
    let status_actions = chat_messages.map(q!(|event: SlackEvent| {
        SlackAction::SetStatus {
            channel: event.channel,
            thread_ts: event.thread_ts,
            status: "is thinking".to_owned(),
        }
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

        match &de.message {
            infinity_protocol::DaemonMessage::Connected {
                session_id, title, ..
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
                // Reflect text generation in the thread status.
                if has_thread {
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
                });
                actions
            }
            infinity_protocol::DaemonMessage::ToolCall { name, args, .. } => {
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
                    .entry(de.thread_ts.clone())
                    .or_default()
                    .push_back((task_id.clone(), name.clone(), details.clone()));
                let mut actions = Vec::new();
                // Show which tool is running in the thread status.
                if has_thread {
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
                });
                actions
            }
            infinity_protocol::DaemonMessage::ToolResult { .. } => {
                // Complete the oldest in-flight tool task for this thread.
                let popped = rt
                    .tool_tasks
                    .lock()
                    .expect("bug: lock poisoned")
                    .get_mut(&de.thread_ts)
                    .and_then(|q| q.pop_front());
                let mut actions = Vec::new();
                // Tool finished — back to thinking until the next tool/text.
                if has_thread {
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
                    });
                }
                actions
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
                    Vec::new()
                } else {
                    vec![SlackAction::StreamStop {
                        channel: channel.clone(),
                        thread_ts: de.thread_ts,
                    }]
                }
            }
            infinity_protocol::DaemonMessage::Error { text, .. } => {
                vec![SlackAction::PostMessage {
                    channel: channel.clone(),
                    text: format!("⚠️ {text}"),
                    thread_ts: Some(de.thread_ts),
                }]
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
                    },
                    SlackAction::PostBlocks {
                        channel: channel.clone(),
                        fallback_text: prompt.clone(),
                        blocks,
                        thread_ts: Some(de.thread_ts),
                        choice_id: Some(id.clone()),
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
        );

    (slack_actions, daemon_commands)
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
