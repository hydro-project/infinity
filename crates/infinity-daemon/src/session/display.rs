use infinity_agent_core::batch_processor::DisplayEvent;
use infinity_agent_core::message::InfinityMessage;
use infinity_protocol::{DaemonMessage, TokenUsage};
use rig::completion::GetTokenUsage;
use rig::message::{AssistantContent, ToolResultContent, UserContent};

pub(crate) fn display_event_to_daemon<R: GetTokenUsage>(
    thread_id: &str,
    evt: DisplayEvent<R>,
) -> Option<DaemonMessage> {
    let tid = Some(thread_id.to_owned());
    Some(match evt {
        DisplayEvent::StartOutput => DaemonMessage::StartOutput { thread_id: tid },
        DisplayEvent::TextChunk { chunk } => DaemonMessage::TextChunk {
            thread_id: tid,
            chunk,
        },
        DisplayEvent::ToolCall {
            name,
            args,
            display_as,
        } => DaemonMessage::ToolCall {
            name,
            args: args.to_string(),
            thread_id: tid,
            display_as,
        },
        DisplayEvent::ToolResult { segments } => DaemonMessage::ToolResult {
            segments,
            thread_id: tid,
        },
        DisplayEvent::Info(s) => DaemonMessage::Info {
            thread_id: tid,
            text: s,
        },
        DisplayEvent::ResponseDone(r) => {
            let token_usage = r.and_then(|r| r.token_usage()).map(|u| TokenUsage {
                input_tokens: Some(u.input_tokens),
                output_tokens: Some(u.output_tokens),
            });
            DaemonMessage::ResponseDone {
                thread_id: tid,
                token_usage,
            }
        }
        DisplayEvent::UserInput(s) => DaemonMessage::UserInputEcho {
            thread_id: tid,
            text: s,
        },
        DisplayEvent::SubscriptionEvent { name, text } => DaemonMessage::SubscriptionEvent {
            name,
            text,
            thread_id: tid,
        },
        DisplayEvent::OAuthRequired { auth_url } => DaemonMessage::OAuthRequired {
            thread_id: tid,
            auth_url,
        },
        DisplayEvent::UserChoiceRequired {
            id,
            prompt,
            choices,
            default,
            response_url: _,
        } => DaemonMessage::UserChoiceRequired {
            thread_id: tid,
            id,
            prompt,
            choices,
            default,
        },
        DisplayEvent::ThinkingStart => DaemonMessage::ThinkingStart { thread_id: tid },
        DisplayEvent::ThinkingEnd => DaemonMessage::ThinkingEnd { thread_id: tid },
        DisplayEvent::ThinkingChunk { chunk } => DaemonMessage::ThinkingChunk {
            thread_id: tid,
            chunk,
        },
        DisplayEvent::CompactionApplied => DaemonMessage::CompactionApplied { thread_id: tid },
        DisplayEvent::UserChoiceComplete { choice_id } => {
            DaemonMessage::UserChoiceComplete { choice_id }
        }
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
                    .rev()
                    .find_map(|m| {
                        if let InfinityMessage::ToolCall { call, display_as } = m
                            && call.id == *tool_call_id
                        {
                            Some(display_as.clone().unwrap_or_else(|| {
                                format!("{}({})", call.function.name, call.function.arguments)
                            }))
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
