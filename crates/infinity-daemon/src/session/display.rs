use infinity_agent_core::ThreadId;
use infinity_agent_core::message::InfinityMessage;
use infinity_agent_core::system::AgentEvent;
use infinity_protocol::{DaemonMessage, ThreadRef, TokenUsage};
use infinity_provider_protocol::message::{AssistantContent, ToolResultContent, UserContent};

pub(crate) fn agent_event_to_daemon(thread_id: &ThreadId<str>, evt: &AgentEvent) -> DaemonMessage {
    let tid = Some(ThreadRef::local(thread_id.to_owned()));
    match evt {
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
        AgentEvent::UserChoiceRequired { choice } => DaemonMessage::UserChoiceRequired {
            thread_id: tid,
            id: choice.id.clone(),
            prompt: choice.prompt.clone(),
            choices: choice.choices.clone(),
            default: choice.default,
        },
        AgentEvent::UserChoiceDismissed { choice_id } => DaemonMessage::UserChoiceComplete {
            choice_id: choice_id.clone(),
        },
        AgentEvent::CompactionApplied => DaemonMessage::CompactionApplied { thread_id: tid },
    }
}

pub(crate) fn history_message_to_daemon(
    msg: &InfinityMessage,
    tid: &ThreadId<str>,
    history: &[InfinityMessage],
) -> Option<DaemonMessage> {
    let thread_id = Some(ThreadRef::local(tid.to_owned()));
    match msg {
        InfinityMessage::SubscriptionEvent {
            result,
            tool_call_id,
            child_thread_id,
            ..
        } => {
            let text = if let Some(ToolResultContent::Text(t)) = result.content.first() {
                t.text.clone()
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
                            && call.id == tool_call_id.as_str()
                        {
                            Some(format!(
                                "{}({})",
                                call.function.name, call.function.arguments
                            ))
                        } else {
                            None
                        }
                    })
                    .unwrap_or_else(|| tool_call_id.as_str().to_owned())
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
            if let Some(ToolResultContent::Text(t)) = result.content.first() {
                // Same prioritized-segments shape the live path emits (see
                // `AgentEvent::ToolResult` emission in the runtime).
                Some(DaemonMessage::ToolResult {
                    segments: rap_protocol::build_display_segments(
                        display_segments.as_deref(),
                        &t.text,
                    ),
                    thread_id,
                })
            } else {
                None
            }
        }
        InfinityMessage::User { content } => {
            if let UserContent::Text(text) = content {
                // Interrupting inputs are stored with the `<interrupt>`
                // prefix the model sees; strip it for display exactly like
                // the live `AgentEvent::UserInput` echo does.
                let display_text = text
                    .text
                    .strip_prefix("<interrupt>")
                    .unwrap_or(&text.text)
                    .to_owned();
                Some(DaemonMessage::UserInputEcho {
                    thread_id,
                    text: display_text,
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
