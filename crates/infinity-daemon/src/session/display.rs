use infinity_agent_core::batch_processor::DisplayEvent;
use infinity_protocol::{DaemonMessage, TokenUsage};
use rig::completion::GetTokenUsage;
use rig::message::{ToolResultContent, UserContent};

use crate::memory_store::InMemoryConversationStore;

pub(crate) fn display_event_to_daemon<R: GetTokenUsage>(
    evt: DisplayEvent<R>,
) -> Option<DaemonMessage> {
    Some(match evt {
        DisplayEvent::StartOutput { prefix } => DaemonMessage::StartOutput { prefix },
        DisplayEvent::TextChunk { prefix, chunk } => DaemonMessage::TextChunk { prefix, chunk },
        DisplayEvent::ToolCall {
            name,
            args,
            prefix,
            display_script,
        } => DaemonMessage::ToolCall {
            name,
            args: args.to_string(),
            prefix,
            display_script,
        },
        DisplayEvent::ToolResult {
            text,
            display_as,
            prefix,
        } => DaemonMessage::ToolResult {
            text,
            display_as,
            prefix,
        },
        DisplayEvent::Info(s) => DaemonMessage::Info(s),
        DisplayEvent::ResponseDone(thread_id, r) => {
            let token_usage = r.and_then(|r| r.token_usage()).map(|u| TokenUsage {
                input_tokens: Some(u.input_tokens),
                output_tokens: Some(u.output_tokens),
            });
            DaemonMessage::ResponseDone {
                thread_id,
                token_usage,
            }
        }
        DisplayEvent::UserInput(s) => DaemonMessage::UserInputEcho(s),
        DisplayEvent::SubscriptionEvent { name, text, prefix } => {
            DaemonMessage::SubscriptionEvent { name, text, prefix }
        }
        DisplayEvent::OAuthRequired { auth_url } => DaemonMessage::OAuthRequired { auth_url },
        DisplayEvent::UserChoiceRequired {
            id,
            prompt,
            choices,
            default,
            response_url: _,
        } => DaemonMessage::UserChoiceRequired {
            id,
            prompt,
            choices,
            default,
        },
        DisplayEvent::ThinkingStart { prefix } => DaemonMessage::ThinkingStart { prefix },
        DisplayEvent::ThinkingEnd { prefix } => DaemonMessage::ThinkingEnd { prefix },
        DisplayEvent::ThinkingChunk { prefix, chunk } => {
            DaemonMessage::ThinkingChunk { prefix, chunk }
        }
    })
}

pub(crate) fn history_message_to_daemon(
    msg: &rig::message::Message,
    tid: &str,
    store: &InMemoryConversationStore,
) -> Option<DaemonMessage> {
    use rig::message::{AssistantContent, Message};
    match msg {
        Message::User { content } => match content.first() {
            UserContent::Text(text) => Some(DaemonMessage::UserInputEcho(text.text.clone())),
            UserContent::ToolResult(res) => {
                if let ToolResultContent::Text(t) = res.content.first() {
                    let display_as = store.get_display_as(tid, &res.id);
                    Some(DaemonMessage::ToolResult {
                        text: t.to_string(),
                        display_as,
                        prefix: None,
                    })
                } else {
                    None
                }
            }
            _ => None,
        },
        Message::Assistant { content, .. } => match content.first() {
            AssistantContent::Text(text) => Some(DaemonMessage::TextChunk {
                prefix: None,
                chunk: text.text.clone(),
            }),
            AssistantContent::ToolCall(call) => Some(DaemonMessage::ToolCall {
                name: call.function.name.clone(),
                args: call.function.arguments.to_string(),
                prefix: None,
                display_script: None,
            }),
            _ => None,
        },
    }
}
