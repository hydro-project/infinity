use infinity_agent_core::message::InfinityMessage;
use infinity_agent_core::system::AgentEvent;
use infinity_protocol::{DaemonMessage, TokenUsage};
use rig::message::{AssistantContent, ToolResultContent, UserContent};

pub(crate) fn agent_event_to_daemon(thread_id: &str, evt: &AgentEvent) -> Option<DaemonMessage> {
    let tid = Some(thread_id.to_owned());
    Some(match evt {
        AgentEvent::CompletionStarted => DaemonMessage::StartOutput { thread_id: tid },
        AgentEvent::TextChunk { text } => DaemonMessage::TextChunk {
            thread_id: tid,
            chunk: text.clone(),
        },
        AgentEvent::ToolCall {
            name,
            args,
            display_as,
        } => DaemonMessage::ToolCall {
            name: name.clone(),
            args: args.to_string(),
            thread_id: tid,
            display_as: display_as.clone(),
        },
        AgentEvent::ToolResult { segments } => DaemonMessage::ToolResult {
            segments: segments.clone(),
            thread_id: tid,
        },
        AgentEvent::Info { text } => DaemonMessage::Info {
            thread_id: tid,
            text: text.clone(),
        },
        AgentEvent::CompletionFinished { usage } => {
            let token_usage = usage.map(|u| TokenUsage {
                input_tokens: Some(u.input_tokens),
                output_tokens: Some(u.output_tokens),
                total_tokens: Some(u.total_tokens),
            });
            DaemonMessage::ResponseDone {
                thread_id: tid,
                token_usage,
            }
        }
        AgentEvent::UserInput { text } => DaemonMessage::UserInputEcho {
            thread_id: tid,
            text: text.clone(),
        },
        AgentEvent::SubscriptionEvent { name, text } => DaemonMessage::SubscriptionEvent {
            name: name.clone(),
            text: text.clone(),
            thread_id: tid,
        },
        AgentEvent::OAuthRequired { auth_url } => DaemonMessage::OAuthRequired {
            thread_id: tid,
            auth_url: auth_url.clone(),
        },
        AgentEvent::ThinkingStarted => DaemonMessage::ThinkingStart { thread_id: tid },
        AgentEvent::ThinkingEnded => DaemonMessage::ThinkingEnd { thread_id: tid },
        AgentEvent::ThinkingChunk { text } => DaemonMessage::ThinkingChunk {
            thread_id: tid,
            chunk: text.clone(),
        },
        AgentEvent::CompactionApplied => DaemonMessage::CompactionApplied { thread_id: tid },
    })
}

pub(crate) fn history_message_to_daemon(
    msg: &InfinityMessage,
    tid: &str,
    history: &[InfinityMessage],
) -> Option<DaemonMessage> {
    let thread_id = Some(tid.to_owned());
    match msg {
        InfinityMessage::SubscriptionEvent {
            result,
            tool_call_id,
            child_thread_id,
            ..
        } => {
            let text = if let ToolResultContent::Text(t) = result.content.first() {
                t.text
            } else {
                String::new()
            };
            let name = if let Some(child_id) = child_thread_id {
                format!("Report from child thread {}", child_id)
            } else {
                history
                    .iter()
                    .find_map(|m| {
                        if let InfinityMessage::ToolCall { call, .. } = m
                            && call.id == *tool_call_id
                        {
                            Some(format!(
                                "{}({})",
                                call.function.name, call.function.arguments
                            ))
                        } else {
                            None
                        }
                    })
                    .unwrap_or_else(|| tool_call_id.clone())
            };
            Some(DaemonMessage::SubscriptionEvent {
                name,
                text,
                thread_id,
            })
        }
        InfinityMessage::ToolCall { call, display_as } => Some(DaemonMessage::ToolCall {
            name: call.function.name.clone(),
            args: call.function.arguments.to_string(),
            thread_id,
            display_as: display_as.clone(),
        }),
        InfinityMessage::ToolResult {
            result,
            display_segments,
        } => {
            if let ToolResultContent::Text(t) = result.content.first() {
                let segments = if let Some(segs) = display_segments {
                    let mut s = segs.clone();
                    s.push(rap_protocol::DisplaySegment::Text(t.text));
                    s
                } else {
                    vec![rap_protocol::DisplaySegment::Text(t.text)]
                };
                Some(DaemonMessage::ToolResult {
                    segments,
                    thread_id,
                })
            } else {
                None
            }
        }
        InfinityMessage::User { content } => {
            if let UserContent::Text(text) = content {
                Some(DaemonMessage::UserInputEcho {
                    thread_id,
                    text: text.text.clone(),
                })
            } else {
                None
            }
        }
        InfinityMessage::Assistant { content } => {
            if let AssistantContent::Text(text) = content {
                Some(DaemonMessage::TextChunk {
                    thread_id,
                    chunk: text.text.clone(),
                })
            } else {
                None
            }
        }
    }
}
