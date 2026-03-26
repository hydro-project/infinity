//! HTTP callback handler for RAP tool results.
//!
//! Starts a local HTTP server that tools POST results back to.
//! The server URL becomes the `callback_url` in tool invocations.

use http_body_util::{BodyExt, Full};
use hyper::body::{Bytes, Incoming};
use hyper::server::conn::http1;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use infinity_agent_core::message::{
    InputMessage, InputMessageContent, OAuthRequired, SyntheticKind, TaggedSyntheticKind,
    UserChoiceRequired,
};
use rap_protocol::RapCallback;
use rig::message::{ToolResult, ToolResultContent, UserContent};
use std::convert::Infallible;
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio::sync::mpsc;

type BoxError = Box<dyn std::error::Error + Send + Sync>;

struct CallbackState {
    input_tx: mpsc::UnboundedSender<(InputMessage, String)>,
}

/// Start the callback server. Returns the base URL tools should POST to.
pub async fn start_callback_server(
    input_tx: mpsc::UnboundedSender<(InputMessage, String)>,
) -> Result<String, BoxError> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let base_url = format!("http://127.0.0.1:{}", addr.port());
    let state = Arc::new(CallbackState { input_tx });

    tokio::spawn(async move {
        loop {
            let (stream, _) = match listener.accept().await {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!("Callback accept error: {}", e);
                    continue;
                }
            };
            let state = state.clone();
            tokio::spawn(async move {
                let svc = hyper::service::service_fn(move |req: Request<Incoming>| {
                    let state = state.clone();
                    async move { Ok::<_, Infallible>(handle(req, state).await) }
                });
                if let Err(e) = http1::Builder::new()
                    .serve_connection(TokioIo::new(stream), svc)
                    .await
                {
                    tracing::warn!("Callback connection error: {}", e);
                }
            });
        }
    });

    tracing::info!("RAP callback server listening on {}", addr);
    Ok(base_url)
}

fn ok_response(status: StatusCode, body: &str) -> Response<Full<Bytes>> {
    Response::builder()
        .status(status)
        .body(Full::new(Bytes::from(body.to_string())))
        .unwrap()
}

async fn handle(req: Request<Incoming>, state: Arc<CallbackState>) -> Response<Full<Bytes>> {
    if req.method() != hyper::Method::POST {
        return ok_response(StatusCode::METHOD_NOT_ALLOWED, "POST only");
    }

    let body = match req.into_body().collect().await {
        Ok(b) => b.to_bytes(),
        Err(e) => {
            tracing::warn!("Failed to read callback body: {}", e);
            return ok_response(StatusCode::BAD_REQUEST, "Failed to read body");
        }
    };

    let cb: RapCallback = match serde_json::from_slice(&body) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!("Invalid callback payload: {}", e);
            return ok_response(StatusCode::BAD_REQUEST, &format!("Bad request: {}", e));
        }
    };

    tracing::info!("RAP callback: {:?}", cb);

    let input_msg = match cb {
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
                content_type: "oauth_required".to_string(),
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
                content_type: "user_choice_required".to_string(),
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
    };

    let dedup = uuid::Uuid::new_v4().to_string();
    let res = state.input_tx.send((input_msg, dedup));
    tracing::debug!("Got result {:?}", res);

    ok_response(StatusCode::OK, "OK")
}
