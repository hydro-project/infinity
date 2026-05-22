//! Defines the Hydro dataflow graph for the Slack bot.
//!
//! Two sidecars feed the dataflow:
//! - Slack sidecar: `Stream<SlackEvent>` in, `Stream<SlackAction>` out
//! - Daemon sidecar: `Stream<DaemonEvent>` in, `Stream<DaemonCommand>` out

use hydro_lang::live_collections::stream::NoOrder;
use hydro_lang::prelude::*;

use crate::daemon_sidecar::{DaemonCommand, DaemonEvent};
use crate::sidecar::{SlackAction, SlackEvent};

/// The full dataflow graph. Returns (slack_actions, daemon_commands).
#[expect(clippy::type_complexity, reason = "Hydro stream type signatures")]
pub fn slack_dataflow<'a, P: 'a>(
    slack_events: Stream<SlackEvent, Process<'a, P>, Unbounded>,
    daemon_events: Stream<DaemonEvent, Process<'a, P>, Unbounded>,
) -> (
    Stream<SlackAction, Process<'a, P>, Unbounded, NoOrder>,
    Stream<DaemonCommand, Process<'a, P>, Unbounded, NoOrder>,
) {
    // Filter: drop bot messages and unauthorized users.
    let filtered = slack_events.filter(q!(|event: &SlackEvent| {
        !event.is_bot && !event.is_unauthorized
    }));

    // Partition button clicks from regular messages (avoids cloning the stream).
    let (non_button, button_clicks) =
        filtered.partition(q!(|event: &SlackEvent| !event.is_button_click));

    // Button clicks → AnswerChoice commands
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
        Some(crate::daemon_sidecar::DaemonCommand::AnswerChoice {
            thread_ts: event.thread_ts,
            choice_id,
            selected,
        })
    }));

    // Regular messages → CreateSession or SendInput commands
    let message_commands =
        non_button
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
                    Some(crate::daemon_sidecar::DaemonCommand::CreateSession {
                        thread_ts: event.thread_ts,
                        cwd: rt.config.default_cwd.clone(),
                    })
                }
            }));

    let daemon_commands = message_commands.merge_unordered(button_commands);

    // Set "Thinking..." status when a non-button user message arrives.
    let status_actions = non_button.map(q!(|event: crate::sidecar::SlackEvent| {
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
                infinity_protocol::DaemonMessage::Connected { session_id, .. } => {
                    let mut sessions = rt.sessions.lock().expect("bug: lock poisoned");
                    sessions.insert(de.thread_ts.clone(), session_id.clone());
                    None
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
                    Some(crate::sidecar::SlackAction::StreamAppend {
                        channel: channel.clone(),
                        thread_ts: de.thread_ts,
                        text: format!("\n🔧 `{name}(…)`\n"),
                    })
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
                    })
                }
                _ => None,
            }
        }));

    // Merge all slack action streams.
    let slack_actions = daemon_slack_actions.merge_unordered(status_actions);

    (slack_actions, daemon_commands)
}
