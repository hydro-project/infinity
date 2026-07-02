//! HTTP callback handler for RAP tool results.
//!
//! Wraps the generic `rap_client` callback server with agent-specific
//! conversion from `RapCallback` into `InputMessage`, routing directly
//! to the session manager.

use infinity_agent_core::message::{
    InputMessage, InputMessageContent, OAuthRequired, SyntheticKind, TaggedSyntheticKind,
    UserChoiceRequired,
};
use rap_protocol::RapCallback;
use rig::message::{ToolResult, ToolResultContent, UserContent};
use std::sync::Arc;
use tokio::sync::Mutex;

use crate::session::SessionManager;

type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// Bind the callback listener, create a fully-initialized `SessionManager`
/// (with the callback URL already set), and start the accept loop.
///
/// Incoming RAP callbacks are converted to `InputMessage` and routed
/// directly to the session manager, eliminating an mpsc indirection.
pub async fn start_callback_server(
    state_dir: std::path::PathBuf,
) -> Result<Arc<Mutex<SessionManager>>, BoxError> {
    let (listener, callback_url) = rap_client::callback_server::bind_callback_listener().await?;
    let session_manager = SessionManager::new(state_dir, callback_url).await?;
    Ok(serve_callbacks(listener, session_manager))
}

/// Start the callback accept loop for an already-built [`SessionManager`]
/// on a pre-bound listener (whose base URL must match the manager's
/// `callback_url`). This is the generic entry point; tests use it to wire a
/// manager built via [`SessionManager::with_providers`].
pub fn serve_callbacks(
    listener: tokio::net::TcpListener,
    session_manager: SessionManager,
) -> Arc<Mutex<SessionManager>> {
    let session_manager = Arc::new(Mutex::new(session_manager));

    // Use a channel to bridge from the Send-required callback server
    // to the LocalSet where SessionManager lives.
    let (cb_tx, mut cb_rx) = tokio::sync::mpsc::unbounded_channel::<RapCallback>();
    rap_client::callback_server::start_callback_server_on(listener, move |cb| {
        let cb_tx = cb_tx.clone();
        async move {
            let _ = cb_tx.send(cb);
        }
    });

    let sm = session_manager.clone();
    tokio::task::spawn_local(async move {
        while let Some(cb) = cb_rx.recv().await {
            // ViewUpdate is a side-channel — store and broadcast without going through the agent loop.
            if let RapCallback::ViewUpdate(vu) = &cb {
                tracing::info!(
                    "RAP view_update: type={} group={}",
                    vu.view_type,
                    vu.group_id
                );
                let mgr = sm.lock().await;
                mgr.handle_view_update(&vu.group_id, &vu.view_type, vu.content.clone());
                continue;
            }

            let input_msg = convert_callback(cb);
            let group_id = input_msg.group_id.clone();
            let dedup = uuid::Uuid::new_v4().to_string();
            let mut emit = async |_msg: infinity_protocol::DaemonMessage| {};
            sm.lock()
                .await
                .send_input(&group_id, (input_msg, Some(dedup)), None, &mut emit)
                .await;
        }
    });

    session_manager
}

fn convert_callback(cb: RapCallback) -> InputMessage {
    tracing::info!("RAP callback: {:?}", cb);

    match cb {
        RapCallback::ToolResult(tr) => InputMessage {
            content: InputMessageContent::User(UserContent::ToolResult(ToolResult {
                id: tr.id,
                call_id: tr.call_id,
                content: rig::OneOrMany::one(ToolResultContent::Text(rig::agent::Text {
                    text: tr.text,
                })),
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
                    id: se.tool_call_id.clone(),
                    call_id: None,
                    content: rig::OneOrMany::one(ToolResultContent::Text(rig::agent::Text {
                        text: se.text,
                    })),
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
        RapCallback::ViewUpdate(_) => {
            unreachable!("bug: ViewUpdate should be handled before convert_callback")
        }
    }
}
