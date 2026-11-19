//! The RAP side: toolset manifest and invocation endpoints.

use std::sync::Arc;

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post};
use axum::{Json, Router};
use rap_protocol::{RapInvocation, ThreadId, ToolCallId, ToolDef, ToolsetManifest};
use serde::Deserialize;

use crate::mailbox::Mailbox;

pub fn router(mailbox: Arc<Mailbox>) -> Router {
    Router::new()
        .route("/.well-known/rap-toolset", get(toolset))
        .route("/invoke", post(invoke))
        .route("/close_thread", post(close_thread))
        .route("/cancel_tool_call", post(cancel_tool_call))
        .with_state(mailbox)
}

async fn toolset(State(mailbox): State<Arc<Mailbox>>, headers: HeaderMap) -> Json<ToolsetManifest> {
    let host = headers
        .get("host")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("127.0.0.1");
    Json(manifest(&format!("http://{host}/invoke"), mailbox.name()))
}

async fn invoke(
    State(mailbox): State<Arc<Mailbox>>,
    Json(invocation): Json<RapInvocation>,
) -> StatusCode {
    tokio::spawn(rap_protocol::log_panic("agent_mail_invoke", async move {
        mailbox.handle_invocation(invocation).await;
    }));
    StatusCode::OK
}

#[derive(Deserialize)]
struct CloseThread {
    thread_id: ThreadId,
}

async fn close_thread(
    State(mailbox): State<Arc<Mailbox>>,
    Json(req): Json<CloseThread>,
) -> StatusCode {
    tokio::spawn(rap_protocol::log_panic(
        "agent_mail_close_thread",
        async move {
            mailbox.thread_closed(&req.thread_id).await;
        },
    ));
    StatusCode::OK
}

#[derive(Deserialize)]
struct CancelToolCall {
    thread_id: ThreadId,
    tool_call_id: ToolCallId,
}

async fn cancel_tool_call(
    State(mailbox): State<Arc<Mailbox>>,
    Json(req): Json<CancelToolCall>,
) -> StatusCode {
    tokio::spawn(rap_protocol::log_panic("agent_mail_cancel", async move {
        mailbox
            .tool_cancelled(&req.thread_id, &req.tool_call_id)
            .await;
    }));
    StatusCode::OK
}

fn tool(
    name: &str,
    description: &str,
    input_schema: serde_json::Value,
    display_script: Option<&str>,
) -> ToolDef {
    ToolDef {
        name: name.to_owned(),
        description: description.to_owned(),
        input_schema,
        annotations: None,
        display_script: display_script.map(str::to_owned),
    }
}

fn manifest(endpoint: &str, host: &str) -> ToolsetManifest {
    ToolsetManifest {
        name: "agent-mail".to_owned(),
        description: Some(format!(
            "Message agents on this and other Infinity hosts (this host is `{host}`)."
        )),
        endpoint: endpoint.to_owned(),
        needs_migration: false,
        tools: vec![
            tool(
                "open_inbox",
                &format!(
                    "Make this thread reachable by other agents as `{host}/<name>`. This is a \
                     subscription: every message sent to the inbox arrives later as an event of \
                     this call. By default each message is handled right away in its own child \
                     thread (delivery \"thread\"), even while this thread is busy or waiting on \
                     ask_agent; the message stays out of this thread's context, though the \
                     child may send this thread a report. Delivery \"inline\" \
                     puts messages into this thread instead, where they wait until its current \
                     tool call finishes; use it only for a thread that does nothing but \
                     converse. A thread owns at most one inbox; child threads send as their \
                     nearest ancestor's inbox. Stop with cancel_subscription."
                ),
                serde_json::json!({
                    "type": "object",
                    "properties": {
                        "name": {
                            "type": "string",
                            "description": "Inbox name (letters, digits, '-', '_', '.'), e.g. \"reviewer\". Defaults to the first 8 characters of the session ID."
                        },
                        "description": {
                            "type": "string",
                            "description": "One line telling other agents what this agent does and what to message it about."
                        },
                        "delivery": {
                            "type": "string",
                            "enum": ["thread", "inline"],
                            "description": "\"thread\" (default): one child thread per message. \"inline\": into this thread."
                        },
                        "replace": {
                            "type": "boolean",
                            "description": "Take over the name if another thread holds it (that thread stops receiving). Default false."
                        }
                    }
                }),
                Some(r#"let s = "Open inbox"; if args.name != () { s += " " + args.name; } s"#),
            ),
            tool(
                "list_agents",
                "List the open inboxes on this host and on every connected peer host, with \
                 their descriptions and each host's connection status.",
                serde_json::json!({
                    "type": "object",
                    "properties": {
                        "host": {
                            "type": "string",
                            "description": "Only list this host."
                        }
                    }
                }),
                None,
            ),
            tool(
                "send_message",
                "Send a one-way message to another agent's inbox. Returns once the recipient's \
                 host has handed it to the recipient (or reports it queued if that host is \
                 unreachable right now). Does not wait for any answer; use ask_agent for \
                 questions.",
                serde_json::json!({
                    "type": "object",
                    "properties": {
                        "to": {
                            "type": "string",
                            "description": "Recipient address `host/inbox`, or `inbox` on this host."
                        },
                        "text": { "type": "string", "description": "The message (max 32 KiB)." }
                    },
                    "required": ["to", "text"]
                }),
                Some(r#""Message " + args.to"#),
            ),
            tool(
                "ask_agent",
                "Ask another agent a question and wait for its answer: this tool call stays \
                 pending (costing nothing) until the recipient calls `reply`, for up to 30 \
                 days. If the user interrupts while you wait, the question is withdrawn. If \
                 the recipient's host is unreachable, the question waits for it (up to 7 \
                 days).",
                serde_json::json!({
                    "type": "object",
                    "properties": {
                        "to": {
                            "type": "string",
                            "description": "Recipient address `host/inbox`, or `inbox` on this host."
                        },
                        "text": { "type": "string", "description": "The question (max 32 KiB)." }
                    },
                    "required": ["to", "text"]
                }),
                Some(r#""Ask " + args.to"#),
            ),
            tool(
                "reply",
                "Answer a question received in your inbox (an event with kind \"ask\"). Each \
                 question can be answered once; the answer becomes the result of the asker's \
                 ask_agent call.",
                serde_json::json!({
                    "type": "object",
                    "properties": {
                        "message_id": {
                            "type": "string",
                            "description": "The message_id of the received question."
                        },
                        "text": { "type": "string", "description": "The answer (max 32 KiB)." }
                    },
                    "required": ["message_id", "text"]
                }),
                Some(r#""Reply""#),
            ),
        ],
    }
}
