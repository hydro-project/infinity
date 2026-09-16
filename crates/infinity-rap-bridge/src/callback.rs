//! Conversion from RAP callback wire messages ([`RapCallback`]) into the
//! runtime's [`InputMessage`].
//!
//! This is the single authoritative mapping, shared by every callback
//! ingestion point (the Infinity Code daemon's local callback server and the
//! AWS `rap-receiver` Lambda), so the platforms cannot drift in how tool
//! results, subscription events, OAuth challenges, and user choices enter
//! the input queue.

use infinity_provider_protocol::message::{
    Image, ImageMediaType, ImageSource, Text, ToolResult, ToolResultContent, UserContent,
};
use rap_protocol::{RapCallback, RapToolResultContent};

use infinity_agent_core::message::{
    InputMessage, InputMessageContent, OAuthRequired, SyntheticKind, TaggedSyntheticKind,
    UserChoiceRequired,
};

/// Convert a RAP callback into the [`InputMessage`] to enqueue for its
/// thread (`message.group_id`).
///
/// Returns `None` for [`RapCallback::ViewUpdate`]: view updates are a
/// display side channel, not agent input. The caller routes them to its
/// own view storage (or drops them when it has none) instead of enqueueing.
pub(crate) fn convert_callback(cb: RapCallback) -> Option<InputMessage> {
    tracing::info!("RAP callback: {:?}", cb);

    Some(match cb {
        RapCallback::ToolResult(tr) => InputMessage {
            content: InputMessageContent::User(UserContent::ToolResult(ToolResult {
                id: tr.id.into_inner(),
                call_id: tr.call_id.map(|c| c.into_inner()),
                content: tool_result_content(tr.content, tr.text),
            })),
            group_id: tr.group_id,
            metadata: None,
            synthetic: None,
            display_as: tr.display_as,
            subscription: tr.subscription.unwrap_or(false),
        },
        RapCallback::SubscriptionEvent(se) => {
            let is_final = se.r#final.unwrap_or(false);
            InputMessage {
                content: InputMessageContent::User(UserContent::ToolResult(ToolResult {
                    id: se.tool_call_id.clone().into_inner(),
                    call_id: None,
                    content: vec![ToolResultContent::Text(Text { text: se.text })],
                })),
                group_id: se.group_id,
                metadata: None,
                synthetic: Some(SyntheticKind::Tagged(
                    TaggedSyntheticKind::SubscriptionEvent {
                        tool_call_id: se.tool_call_id,
                        associative: se.associative,
                        r#final: is_final,
                    },
                )),
                display_as: None,
                subscription: false,
            }
        }
        RapCallback::OAuth(oa) => InputMessage {
            content: InputMessageContent::OAuth(OAuthRequired {
                content_type: "oauth_required".to_owned(),
                id: oa.id,
                call_id: oa.call_id,
                auth_url: oa.auth_url,
            }),
            group_id: oa.group_id,
            metadata: None,
            synthetic: None,
            display_as: None,
            subscription: false,
        },
        RapCallback::UserChoice(uc) => InputMessage {
            content: InputMessageContent::UserChoice(UserChoiceRequired {
                content_type: "user_choice_required".to_owned(),
                id: uc.id,
                call_id: uc.call_id,
                prompt: uc.prompt,
                choices: uc.choices,
                default: uc.default,
                response_url: uc.response_url,
            }),
            group_id: uc.group_id,
            metadata: None,
            synthetic: None,
            display_as: None,
            subscription: false,
        },
        RapCallback::ViewUpdate(_) => return None,
    })
}

/// Build the tool-result content from a RAP tool result. A tool provides
/// either `text` or `content`: when `content` is present (and non-empty) it is
/// used (images become image blocks); otherwise the plain `text` becomes a
/// single text block. An absent/empty result degrades to an empty text block.
fn tool_result_content(
    content: Option<Vec<RapToolResultContent>>,
    text: Option<String>,
) -> Vec<ToolResultContent> {
    match content {
        Some(items) if !items.is_empty() => items
            .into_iter()
            .map(|item| match item {
                RapToolResultContent::Text { text } => ToolResultContent::Text(Text { text }),
                RapToolResultContent::Image { data, media_type } => {
                    ToolResultContent::Image(Image {
                        data: ImageSource::Base64(data),
                        media_type: ImageMediaType::from_mime_type(&media_type),
                    })
                }
            })
            .collect(),
        _ => vec![ToolResultContent::Text(Text {
            text: text.unwrap_or_default(),
        })],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rap_protocol::{
        RapOAuth, RapSubscriptionEvent, RapToolResult, RapUserChoice, RapViewUpdate,
    };

    fn tool_result_cb(
        content: Option<Vec<RapToolResultContent>>,
        text: Option<String>,
    ) -> RapCallback {
        RapCallback::ToolResult(RapToolResult {
            group_id: "t1".into(),
            id: "call-1".into(),
            call_id: Some("prov-1".into()),
            text,
            content,
            display_as: None,
            subscription: None,
        })
    }

    #[test]
    fn text_tool_result_converts() {
        let msg = convert_callback(tool_result_cb(None, Some("plain output".to_owned())))
            .expect("tool results convert");
        assert_eq!(msg.group_id.as_str(), "t1");
        assert!(!msg.subscription);
        assert!(msg.synthetic.is_none());
        match msg.content {
            InputMessageContent::User(UserContent::ToolResult(tr)) => {
                assert_eq!(tr.id, "call-1");
                assert_eq!(tr.call_id.as_deref(), Some("prov-1"));
                match tr.content.first() {
                    Some(ToolResultContent::Text(t)) => assert_eq!(t.text, "plain output"),
                    other => panic!("expected text content, got {other:?}"),
                }
            }
            other => panic!("expected tool result, got {other:?}"),
        }
    }

    #[test]
    fn display_as_and_subscription_flag_pass_through() {
        let cb = RapCallback::ToolResult(RapToolResult {
            group_id: "t1".into(),
            id: "call-1".into(),
            call_id: None,
            text: Some("done".to_owned()),
            content: None,
            display_as: Some(vec![rap_protocol::DisplaySegment::Text(
                "pretty".to_owned(),
            )]),
            subscription: Some(true),
        });
        let msg = convert_callback(cb).expect("tool results convert");
        assert!(msg.subscription);
        assert!(msg.display_as.is_some());
    }

    #[test]
    fn subscription_event_converts_with_flags() {
        let cb = RapCallback::SubscriptionEvent(RapSubscriptionEvent {
            group_id: "t1".into(),
            tool_call_id: "sub-1".into(),
            text: "tick".to_owned(),
            associative: true,
            r#final: Some(true),
        });
        let msg = convert_callback(cb).expect("subscription events convert");
        match msg.synthetic {
            Some(SyntheticKind::Tagged(TaggedSyntheticKind::SubscriptionEvent {
                tool_call_id,
                associative,
                r#final,
            })) => {
                assert_eq!(tool_call_id.as_str(), "sub-1");
                assert!(associative);
                assert!(r#final);
            }
            other => panic!("expected tagged subscription event, got {other:?}"),
        }
    }

    #[test]
    fn oauth_converts() {
        let cb = RapCallback::OAuth(RapOAuth {
            group_id: "t1".into(),
            id: "call-1".into(),
            call_id: None,
            auth_url: "https://auth".to_owned(),
        });
        let msg = convert_callback(cb).expect("oauth converts");
        match msg.content {
            InputMessageContent::OAuth(oa) => {
                assert_eq!(oa.content_type, "oauth_required");
                assert_eq!(oa.auth_url, "https://auth");
            }
            other => panic!("expected oauth content, got {other:?}"),
        }
    }

    #[test]
    fn user_choice_converts() {
        let cb = RapCallback::UserChoice(RapUserChoice {
            group_id: "t1".into(),
            id: "choice-1".into(),
            call_id: None,
            prompt: "pick one".to_owned(),
            choices: vec!["a".to_owned(), "b".to_owned()],
            default: 1,
            response_url: "http://choose".to_owned(),
        });
        let msg = convert_callback(cb).expect("user choices convert");
        match msg.content {
            InputMessageContent::UserChoice(uc) => {
                assert_eq!(uc.content_type, "user_choice_required");
                assert_eq!(uc.choices.len(), 2);
                assert_eq!(uc.default, 1);
            }
            other => panic!("expected user choice content, got {other:?}"),
        }
    }

    #[test]
    fn view_update_is_not_agent_input() {
        let cb = RapCallback::ViewUpdate(RapViewUpdate {
            group_id: "t1".into(),
            view_type: "diff".to_owned(),
            content: serde_json::json!({}),
        });
        assert!(convert_callback(cb).is_none());
    }

    #[test]
    fn tool_result_content_falls_back_to_text() {
        let content = tool_result_content(None, Some("plain output".to_owned()));
        assert_eq!(content.len(), 1);
        match content.first() {
            Some(ToolResultContent::Text(t)) => assert_eq!(t.text, "plain output"),
            other => panic!("expected text content, got {other:?}"),
        }
    }

    #[test]
    fn empty_structured_content_falls_back_to_text() {
        let content = tool_result_content(Some(vec![]), Some("fallback".to_owned()));
        assert_eq!(content.len(), 1);
        match content.first() {
            Some(ToolResultContent::Text(t)) => assert_eq!(t.text, "fallback"),
            other => panic!("expected text content, got {other:?}"),
        }
    }

    #[test]
    fn neither_content_nor_text_is_empty_text() {
        let content = tool_result_content(None, None);
        assert_eq!(content.len(), 1);
        match content.first() {
            Some(ToolResultContent::Text(t)) => assert_eq!(t.text, ""),
            other => panic!("expected empty text content, got {other:?}"),
        }
    }

    #[test]
    fn structured_content_with_image_is_converted() {
        let content = tool_result_content(
            Some(vec![
                RapToolResultContent::Text {
                    text: "Read image file".to_owned(),
                },
                RapToolResultContent::Image {
                    data: "aGVsbG8=".to_owned(),
                    media_type: "image/png".to_owned(),
                },
            ]),
            None,
        );
        assert_eq!(content.len(), 2);
        match content.first() {
            Some(ToolResultContent::Text(t)) => assert_eq!(t.text, "Read image file"),
            other => panic!("expected text content, got {other:?}"),
        }
        match content.last() {
            Some(ToolResultContent::Image(img)) => {
                assert_eq!(img.data, ImageSource::Base64("aGVsbG8=".to_owned()));
                assert_eq!(img.media_type, Some(ImageMediaType::PNG));
            }
            other => panic!("expected image content, got {other:?}"),
        }
    }

    #[test]
    fn unknown_image_media_type_maps_to_none() {
        let content = tool_result_content(
            Some(vec![RapToolResultContent::Image {
                data: "aGVsbG8=".to_owned(),
                media_type: "image/whoknows".to_owned(),
            }]),
            None,
        );
        match content.first() {
            Some(ToolResultContent::Image(img)) => assert_eq!(img.media_type, None),
            other => panic!("expected image content, got {other:?}"),
        }
    }
}
