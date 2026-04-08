use infinity_agent_core::batch_processor::DisplayEvent;
use infinity_protocol::{DaemonMessage, TokenUsage};
use rig::completion::GetTokenUsage;
use rig::message::{ToolResultContent, UserContent};

use crate::memory_store::InMemoryConversationStore;

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
    msg: &rig::message::Message,
    tid: &str,
    store: &InMemoryConversationStore,
) -> Option<DaemonMessage> {
    let thread_id = Some(tid.to_owned());
    use rig::message::{AssistantContent, Message};
    match msg {
        Message::User { content } => match content.first() {
            UserContent::Text(text) => Some(DaemonMessage::UserInputEcho {
                thread_id,
                text: text.text,
            }),
            UserContent::ToolResult(res) => {
                if let ToolResultContent::Text(t) = res.content.first() {
                    let display_as = store.get_display_as(tid, &res.id);
                    Some(DaemonMessage::ToolResult {
                        segments: rap_protocol::build_display_segments(
                            display_as.as_deref(),
                            &t.to_string(),
                        ),
                        thread_id,
                    })
                } else {
                    None
                }
            }
            _ => None,
        },
        Message::Assistant { content, .. } => match content.first() {
            AssistantContent::Text(text) => Some(DaemonMessage::TextChunk {
                thread_id,
                chunk: text.text,
            }),
            AssistantContent::ToolCall(call) => Some(DaemonMessage::ToolCall {
                name: call.function.name.clone(),
                args: call.function.arguments.to_string(),
                thread_id,
                display_as: None,
            }),
            _ => None,
        },
    }
}
