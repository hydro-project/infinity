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
        AgentEvent::CompletionRetrying {
            error,
            attempt,
            delay,
        } => {
            let text = format!(
                "Completion failed ({error}); retrying in {}s...",
                delay.as_secs()
            );
            // The first failure is an error so a failing completion is never
            // a silent retry loop; later retries are progress.
            if *attempt == 1 {
                DaemonMessage::Error {
                    thread_id: tid,
                    text,
                }
            } else {
                DaemonMessage::Info {
                    thread_id: tid,
                    text,
                }
            }
        }
        AgentEvent::CompletionFailed {
            error,
            retries,
            discarded_user_messages,
        } => DaemonMessage::Error {
            thread_id: tid,
            text: completion_failed_text(error, *retries, *discarded_user_messages),
        },
        AgentEvent::Error { error } => DaemonMessage::Error {
            thread_id: tid,
            text: format!("Error: {error}"),
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

/// The give-up message. User messages in a daemon session come from its
/// clients, so a discarded one is the reader's to resend.
fn completion_failed_text(error: &str, retries: u32, discarded_user_messages: usize) -> String {
    let after = match retries {
        0 => String::new(),
        1 => " after 1 retry".to_owned(),
        n => format!(" after {n} retries"),
    };
    let outcome = match discarded_user_messages {
        0 => String::new(),
        1 => " Your message was not kept; please resend it.".to_owned(),
        n => format!(" Your last {n} messages were not kept; please resend them."),
    };
    format!("Completion failed{after}: {error}.{outcome}")
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

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    fn text_of(msg: DaemonMessage) -> (bool, String) {
        match msg {
            DaemonMessage::Error { text, .. } => (true, text),
            DaemonMessage::Info { text, .. } => (false, text),
            other => panic!("unexpected message: {other:?}"),
        }
    }

    /// Only the first retry is an error; later retries are progress.
    #[test]
    fn first_retry_is_an_error_later_retries_are_info() {
        let tid = ThreadId::from_ref("t");
        let retry = |attempt| AgentEvent::CompletionRetrying {
            error: "backend hiccup".to_owned(),
            attempt,
            delay: Duration::from_secs(5),
        };
        let (is_error, text) = text_of(agent_event_to_daemon(tid, &retry(1)));
        assert!(is_error && text.contains("backend hiccup"), "{text}");
        let (is_error, _) = text_of(agent_event_to_daemon(tid, &retry(2)));
        assert!(!is_error);
    }

    /// The give-up error asks for a resend only when a user message was
    /// discarded.
    #[test]
    fn give_up_asks_for_resend_only_when_a_user_message_was_discarded() {
        let tid = ThreadId::from_ref("t");
        let failed = |discarded_user_messages| AgentEvent::CompletionFailed {
            error: "access denied".to_owned(),
            retries: 0,
            discarded_user_messages,
        };
        let (is_error, text) = text_of(agent_event_to_daemon(tid, &failed(1)));
        assert!(
            is_error && text.contains("access denied") && text.contains("resend"),
            "{text}"
        );
        let (_, text) = text_of(agent_event_to_daemon(tid, &failed(0)));
        assert!(!text.contains("resend"), "{text}");
    }
}
