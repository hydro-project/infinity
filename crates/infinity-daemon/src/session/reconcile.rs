//! Boot-time reconciliation of pending RAP tool calls and active subscriptions.
//!
//! If the daemon shuts down (or crashes) while a RAP tool call is in flight
//! or a subscription is active, the tool server may give up on it in the
//! meantime (e.g. because the server itself restarted and lost its state).
//! When the agent boots back up, the affected thread would otherwise hang
//! forever waiting for a callback that will never arrive.
//!
//! On session boot, [`reconcile_rap_state`] queries the RAP server that
//! originally received each pending tool call / active subscription via the
//! `/tool_call_status` protocol message and prunes the ones the server gave
//! up on:
//!
//! * A **pending tool call** is answered with a synthetic failed tool result
//!   so the model can observe the failure and retry.
//! * An **active subscription** is terminated with a synthetic final
//!   subscription-failure event, which also removes it from the thread's
//!   active-subscription tracking when processed.
//!
//! Servers that respond but do not support `/tool_call_status` (4xx /
//! invalid body) cannot confirm the call is alive, so their pending work is
//! treated as failed and pruned. Servers that are *unreachable* (or return a
//! transient 5xx error) yield an *unknown* liveness, which is treated
//! conservatively: nothing is pruned, matching the previous behavior of
//! waiting indefinitely.

use std::collections::HashMap;

use infinity_agent_core::message::{
    InfinityMessage, InputMessage, InputMessageContent, SyntheticKind, TaggedSyntheticKind,
};
use infinity_agent_core::traits::{ConversationStore, InputSender, StateStore};
use rap_client::http::HttpClient;
use rap_client::notifier::{ToolCallLiveness, check_tool_call_status};
use rig::message::{ToolResult, ToolResultContent, UserContent};

use crate::memory_store::{InMemoryConversationStore, InMemoryStateStore};

/// What kind of pending RAP work a tool call represents.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PendingKind {
    /// A dispatched tool call still waiting for its result.
    ToolCall,
    /// An active subscription established by a completed tool call.
    Subscription,
}

/// A pending RAP tool call or active subscription found in a session's threads.
#[derive(Debug, Clone)]
pub struct PendingRapCall {
    pub thread_id: String,
    pub tool_call_id: String,
    pub tool_name: String,
    /// Base URL of the RAP server that originally received the invocation.
    pub server_url: String,
    pub kind: PendingKind,
}

/// Scan all open threads of a session for pending RAP tool calls and active
/// subscriptions. Only tool calls whose tool name maps to a known RAP server
/// (via `tool_servers`) are returned — built-in tools (sleep, spawn_thread,
/// …) never involve a RAP server and must not be pruned.
pub async fn collect_pending_rap_calls(
    conversation_store: &InMemoryConversationStore,
    state_store: &InMemoryStateStore,
    session_id: &str,
    tool_servers: &HashMap<String, String>,
) -> Vec<PendingRapCall> {
    let mut thread_ids = vec![session_id.to_owned()];
    thread_ids.extend(
        conversation_store
            .get_open_subthreads(session_id)
            .into_iter()
            .map(|t| t.thread_id),
    );

    let mut pending = Vec::new();
    for thread_id in thread_ids {
        let history = match conversation_store
            .load_history_up_to(&thread_id, None, None)
            .await
        {
            Ok(h) => h,
            Err(e) => {
                tracing::warn!("reconcile: failed to load history for {thread_id}: {e}");
                continue;
            }
        };

        // A thread is waiting on a tool call iff its history ends with an
        // unanswered ToolCall (mirrors the thread worker's idle check).
        if let Some(InfinityMessage::ToolCall { call, .. }) = history.last() {
            let name = call.function.name.clone();
            if let Some(server_url) = tool_servers.get(&name) {
                pending.push(PendingRapCall {
                    thread_id: thread_id.clone(),
                    tool_call_id: call.id.clone(),
                    tool_name: name,
                    server_url: server_url.clone(),
                    kind: PendingKind::ToolCall,
                });
            }
        }

        // Active subscriptions are tracked by the tool_call_id that
        // established them; recover the tool name from history to find the
        // owning server.
        let subscriptions = state_store
            .get_active_subscriptions(&thread_id)
            .await
            .unwrap_or_default();
        for tool_call_id in subscriptions {
            let tool_name = history.iter().find_map(|m| {
                if let InfinityMessage::ToolCall { call, .. } = m
                    && call.id == tool_call_id
                {
                    Some(call.function.name.clone())
                } else {
                    None
                }
            });
            let Some(tool_name) = tool_name else {
                tracing::warn!(
                    "reconcile: subscription {tool_call_id} in thread {thread_id} has no \
                     originating tool call in history; skipping"
                );
                continue;
            };
            let Some(server_url) = tool_servers.get(&tool_name) else {
                tracing::warn!(
                    "reconcile: subscription {tool_call_id} (tool `{tool_name}`) in thread \
                     {thread_id} has no known RAP server; skipping"
                );
                continue;
            };
            pending.push(PendingRapCall {
                thread_id: thread_id.clone(),
                tool_call_id,
                tool_name,
                server_url: server_url.clone(),
                kind: PendingKind::Subscription,
            });
        }
    }
    pending
}

/// Build the synthetic tool result injected when a pending tool call was
/// abandoned by its RAP server.
fn tool_call_failure_message(pending: &PendingRapCall) -> InputMessage {
    InputMessage {
        content: InputMessageContent::User(UserContent::ToolResult(ToolResult {
            id: pending.tool_call_id.clone(),
            call_id: None,
            content: rig::OneOrMany::one(ToolResultContent::Text(rig::agent::Text {
                text: format!(
                    "Error: the `{}` tool call failed — the tool server is no longer \
                     processing it (its state was likely lost in a restart) and no result \
                     will be delivered. Retry the call if the operation is still needed.",
                    pending.tool_name
                ),
            })),
        })),
        group_id: pending.thread_id.clone(),
        metadata: None,
        synthetic: None,
        display_as: None,
        subscription: false,
    }
}

/// Build the synthetic final subscription event injected when an active
/// subscription was abandoned by its RAP server. Marked `final` so that
/// processing it also removes the subscription from active tracking.
fn subscription_failure_message(pending: &PendingRapCall) -> InputMessage {
    InputMessage {
        content: InputMessageContent::User(UserContent::ToolResult(ToolResult {
            id: pending.tool_call_id.clone(),
            call_id: None,
            content: rig::OneOrMany::one(ToolResultContent::Text(rig::agent::Text {
                text: format!(
                    "This subscription (created by `{}`) failed: the tool server is no \
                     longer tracking it (its state was likely lost in a restart) and no \
                     further events will be delivered. Re-subscribe if you still need \
                     these events.",
                    pending.tool_name
                ),
            })),
        })),
        group_id: pending.thread_id.clone(),
        metadata: None,
        synthetic: Some(SyntheticKind::Tagged(
            TaggedSyntheticKind::SubscriptionEvent {
                tool_call_id: pending.tool_call_id.clone(),
                associative: false,
                r#final: true,
            },
        )),
        display_as: None,
        subscription: false,
    }
}

/// Query the owning RAP server for each pending tool call / active
/// subscription in the session and inject failure messages for the ones the
/// server has given up on (including servers that respond without supporting
/// the status endpoint). Servers reporting *alive* — or servers that cannot
/// be reached at all (*unknown*) — leave the pending work untouched.
pub async fn reconcile_rap_state<H: HttpClient, S: InputSender>(
    conversation_store: &InMemoryConversationStore,
    state_store: &InMemoryStateStore,
    session_id: &str,
    tool_servers: &HashMap<String, String>,
    client: &H,
    sender: &S,
) {
    let pending =
        collect_pending_rap_calls(conversation_store, state_store, session_id, tool_servers).await;

    for p in pending {
        let liveness =
            check_tool_call_status(client, &p.server_url, &p.thread_id, &p.tool_call_id).await;
        match liveness {
            ToolCallLiveness::Alive => {
                tracing::debug!(
                    "reconcile: {:?} {} in thread {} still alive on {}",
                    p.kind,
                    p.tool_call_id,
                    p.thread_id,
                    p.server_url
                );
            }
            ToolCallLiveness::Unknown => {
                tracing::debug!(
                    "reconcile: {:?} {} in thread {} has unknown status on {}; keeping",
                    p.kind,
                    p.tool_call_id,
                    p.thread_id,
                    p.server_url
                );
            }
            ToolCallLiveness::Gone => {
                tracing::info!(
                    "reconcile: pruning {:?} {} in thread {} — server {} gave up on it",
                    p.kind,
                    p.tool_call_id,
                    p.thread_id,
                    p.server_url
                );
                let msg = match p.kind {
                    PendingKind::ToolCall => tool_call_failure_message(&p),
                    PendingKind::Subscription => subscription_failure_message(&p),
                };
                let dedup_id = uuid::Uuid::new_v4().to_string();
                if let Err(e) = sender
                    .send_to_input_queue(msg, &p.thread_id, &dedup_id)
                    .await
                {
                    tracing::error!(
                        "reconcile: failed to inject failure message for {} in thread {}: {e}",
                        p.tool_call_id,
                        p.thread_id
                    );
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::memory_store::InMemoryMessageSender;
    use async_trait::async_trait;
    use infinity_agent_core::traits::ConversationStore;
    use tokio::sync::mpsc;

    #[derive(Debug)]
    struct MockHttpError(String);
    impl std::fmt::Display for MockHttpError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "{}", self.0)
        }
    }
    impl std::error::Error for MockHttpError {}

    /// Mock HTTP client that records `post_read` requests and returns canned
    /// responses keyed by URL. URLs without a canned response yield an error
    /// (simulating an unreachable server).
    #[derive(Clone, Default)]
    struct MockHttp {
        responses: Arc<Mutex<HashMap<String, (u16, String)>>>,
        requests: Arc<Mutex<Vec<(String, String)>>>,
    }

    impl MockHttp {
        fn respond(&self, url: &str, status: u16, body: &str) {
            self.responses
                .lock()
                .expect("bug: mutex poisoned")
                .insert(url.to_owned(), (status, body.to_owned()));
        }

        fn requests(&self) -> Vec<(String, String)> {
            self.requests.lock().expect("bug: mutex poisoned").clone()
        }
    }

    #[async_trait]
    impl HttpClient for MockHttp {
        type Error = MockHttpError;

        async fn post(&self, _url: &str, _body: &str) -> Result<u16, MockHttpError> {
            Ok(200)
        }

        async fn post_read(&self, url: &str, body: &str) -> Result<(u16, Vec<u8>), MockHttpError> {
            self.requests
                .lock()
                .expect("bug: mutex poisoned")
                .push((url.to_owned(), body.to_owned()));
            match self.responses.lock().expect("bug: mutex poisoned").get(url) {
                Some((status, body)) => Ok((*status, body.clone().into_bytes())),
                None => Err(MockHttpError("connection refused".to_owned())),
            }
        }

        async fn get(&self, _url: &str) -> Result<(u16, Vec<u8>), MockHttpError> {
            Ok((404, vec![]))
        }
    }

    fn test_model_ref() -> infinity_protocol::ModelRef {
        infinity_protocol::ModelRef {
            provider_id: "mock".to_owned(),
            model_id: "mock".to_owned(),
        }
    }

    fn tmp_stores() -> (
        InMemoryConversationStore,
        InMemoryStateStore,
        tempfile::TempDir,
    ) {
        let dir = tempfile::tempdir().expect("create temp dir");
        let conv =
            InMemoryConversationStore::new_with_dir(dir.path().join("threads"), test_model_ref());
        let state = InMemoryStateStore::new(dir.path().join("state"));
        (conv, state, dir)
    }

    fn tool_call_msg(id: &str, name: &str) -> InfinityMessage {
        InfinityMessage::ToolCall {
            call: rig::message::ToolCall {
                id: id.to_owned(),
                call_id: None,
                signature: None,
                additional_params: None,
                function: rig::message::ToolFunction {
                    name: name.to_owned(),
                    arguments: serde_json::json!({}),
                },
            },
            display_as: None,
        }
    }

    fn tool_result_msg(id: &str, text: &str) -> InfinityMessage {
        InfinityMessage::ToolResult {
            result: ToolResult {
                id: id.to_owned(),
                call_id: None,
                content: rig::OneOrMany::one(ToolResultContent::Text(rig::agent::Text {
                    text: text.to_owned(),
                })),
            },
            display_segments: None,
        }
    }

    async fn append(conv: &InMemoryConversationStore, thread_id: &str, msg: InfinityMessage) {
        conv.append_messages(thread_id, vec![(msg, uuid::Uuid::new_v4().to_string())])
            .await
            .expect("append message");
    }

    fn capture_sender() -> (
        InMemoryMessageSender,
        mpsc::UnboundedReceiver<(InputMessage, String)>,
    ) {
        let (tx, rx) = mpsc::unbounded_channel();
        (InMemoryMessageSender::new(tx), rx)
    }

    fn servers(entries: &[(&str, &str)]) -> HashMap<String, String> {
        entries
            .iter()
            .map(|(tool, url)| ((*tool).to_owned(), (*url).to_owned()))
            .collect()
    }

    const STATUS_URL: &str = "http://server-a/tool_call_status";

    #[tokio::test]
    async fn prunes_dead_pending_tool_call() {
        let (conv, state, _dir) = tmp_stores();
        conv.ensure_root_thread("t1").await.expect("ensure root");
        append(&conv, "t1", tool_call_msg("tc-1", "my_rap_tool")).await;

        let http = MockHttp::default();
        http.respond(STATUS_URL, 200, r#"{"alive": false}"#);
        let (sender, mut rx) = capture_sender();

        reconcile_rap_state(
            &conv,
            &state,
            "t1",
            &servers(&[("my_rap_tool", "http://server-a")]),
            &http,
            &sender,
        )
        .await;

        // The server was asked about the right tool call.
        let requests = http.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].0, STATUS_URL);
        let req: rap_protocol::RapToolCallStatusRequest =
            serde_json::from_str(&requests[0].1).expect("valid status request body");
        assert_eq!(req.thread_id, "t1");
        assert_eq!(req.tool_call_id, "tc-1");

        // A synthetic failed tool result was injected.
        let (msg, _) = rx.try_recv().expect("failure message injected");
        assert_eq!(msg.group_id, "t1");
        assert!(msg.synthetic.is_none());
        let InputMessageContent::User(UserContent::ToolResult(result)) = &msg.content else {
            panic!("expected tool result content, got {:?}", msg.content);
        };
        assert_eq!(result.id, "tc-1");
        let ToolResultContent::Text(text) = result.content.first() else {
            panic!("expected text content");
        };
        assert!(text.text.contains("my_rap_tool"), "text: {}", text.text);
        assert!(text.text.contains("no longer"), "text: {}", text.text);
        assert!(rx.try_recv().is_err(), "only one message expected");
    }

    #[tokio::test]
    async fn keeps_alive_pending_tool_call() {
        let (conv, state, _dir) = tmp_stores();
        conv.ensure_root_thread("t1").await.expect("ensure root");
        append(&conv, "t1", tool_call_msg("tc-1", "my_rap_tool")).await;

        let http = MockHttp::default();
        http.respond(STATUS_URL, 200, r#"{"alive": true}"#);
        let (sender, mut rx) = capture_sender();

        reconcile_rap_state(
            &conv,
            &state,
            "t1",
            &servers(&[("my_rap_tool", "http://server-a")]),
            &http,
            &sender,
        )
        .await;

        assert_eq!(http.requests().len(), 1);
        assert!(rx.try_recv().is_err(), "alive tool call must not be pruned");
    }

    #[tokio::test]
    async fn keeps_pending_tool_call_when_server_unreachable() {
        let (conv, state, _dir) = tmp_stores();
        conv.ensure_root_thread("t1").await.expect("ensure root");
        append(&conv, "t1", tool_call_msg("tc-1", "my_rap_tool")).await;

        // No canned response → post_read errors (unreachable server).
        let http = MockHttp::default();
        let (sender, mut rx) = capture_sender();

        reconcile_rap_state(
            &conv,
            &state,
            "t1",
            &servers(&[("my_rap_tool", "http://server-a")]),
            &http,
            &sender,
        )
        .await;

        assert!(
            rx.try_recv().is_err(),
            "unknown liveness must not prune the tool call"
        );
    }

    #[tokio::test]
    async fn prunes_pending_tool_call_when_endpoint_unsupported() {
        let (conv, state, _dir) = tmp_stores();
        conv.ensure_root_thread("t1").await.expect("ensure root");
        append(&conv, "t1", tool_call_msg("tc-1", "my_rap_tool")).await;

        // Old server without the endpoint → 404. The server is reachable but
        // cannot be tracking the call, so the call is treated as failed.
        let http = MockHttp::default();
        http.respond(STATUS_URL, 404, "not found");
        let (sender, mut rx) = capture_sender();

        reconcile_rap_state(
            &conv,
            &state,
            "t1",
            &servers(&[("my_rap_tool", "http://server-a")]),
            &http,
            &sender,
        )
        .await;

        let (msg, _) = rx.try_recv().expect(
            "a 404 from the status endpoint must prune the tool call (server can't be tracking it)",
        );
        assert_eq!(msg.group_id, "t1");
        let InputMessageContent::User(UserContent::ToolResult(result)) = &msg.content else {
            panic!("expected tool result content");
        };
        assert_eq!(result.id, "tc-1");
    }

    #[tokio::test]
    async fn prunes_dead_subscription_when_endpoint_unsupported() {
        let (conv, state, _dir) = tmp_stores();
        conv.ensure_root_thread("t1").await.expect("ensure root");
        append(&conv, "t1", tool_call_msg("tc-sub", "subscribe_events")).await;
        append(&conv, "t1", tool_result_msg("tc-sub", "subscribed")).await;
        state
            .add_active_subscription("t1", "tc-sub")
            .await
            .expect("track subscription");

        // The server doesn't implement the endpoint → the subscription is
        // treated as lost.
        let http = MockHttp::default();
        http.respond(STATUS_URL, 404, "not found");
        let (sender, mut rx) = capture_sender();

        reconcile_rap_state(
            &conv,
            &state,
            "t1",
            &servers(&[("subscribe_events", "http://server-a")]),
            &http,
            &sender,
        )
        .await;

        let (msg, _) = rx
            .try_recv()
            .expect("unsupported endpoint must prune the subscription");
        assert!(
            matches!(
                &msg.synthetic,
                Some(SyntheticKind::Tagged(
                    TaggedSyntheticKind::SubscriptionEvent { r#final: true, .. }
                ))
            ),
            "expected a final synthetic subscription event, got {:?}",
            msg.synthetic
        );
    }

    #[tokio::test]
    async fn keeps_pending_tool_call_on_transient_server_error() {
        let (conv, state, _dir) = tmp_stores();
        conv.ensure_root_thread("t1").await.expect("ensure root");
        append(&conv, "t1", tool_call_msg("tc-1", "my_rap_tool")).await;

        // A 5xx is a transient server error, not proof the call was lost.
        let http = MockHttp::default();
        http.respond(STATUS_URL, 500, "internal error");
        let (sender, mut rx) = capture_sender();

        reconcile_rap_state(
            &conv,
            &state,
            "t1",
            &servers(&[("my_rap_tool", "http://server-a")]),
            &http,
            &sender,
        )
        .await;

        assert!(
            rx.try_recv().is_err(),
            "a 5xx from the status endpoint must not prune the tool call"
        );
    }

    #[tokio::test]
    async fn skips_pending_builtin_tool_call() {
        let (conv, state, _dir) = tmp_stores();
        conv.ensure_root_thread("t1").await.expect("ensure root");
        // Pending tool call for a tool that is not provided by any RAP server
        // (e.g. a built-in like sleep or spawn_thread).
        append(&conv, "t1", tool_call_msg("tc-1", "sleep")).await;

        let http = MockHttp::default();
        let (sender, mut rx) = capture_sender();

        reconcile_rap_state(
            &conv,
            &state,
            "t1",
            &servers(&[("my_rap_tool", "http://server-a")]),
            &http,
            &sender,
        )
        .await;

        assert!(http.requests().is_empty(), "no server should be queried");
        assert!(rx.try_recv().is_err(), "built-in calls must not be pruned");
    }

    #[tokio::test]
    async fn prunes_dead_subscription_with_final_event() {
        let (conv, state, _dir) = tmp_stores();
        conv.ensure_root_thread("t1").await.expect("ensure root");
        // The subscription's originating tool call completed (has a result),
        // so it is not a pending tool call — only an active subscription.
        append(&conv, "t1", tool_call_msg("tc-sub", "subscribe_events")).await;
        append(&conv, "t1", tool_result_msg("tc-sub", "subscribed")).await;
        state
            .add_active_subscription("t1", "tc-sub")
            .await
            .expect("track subscription");

        let http = MockHttp::default();
        http.respond(STATUS_URL, 200, r#"{"alive": false}"#);
        let (sender, mut rx) = capture_sender();

        reconcile_rap_state(
            &conv,
            &state,
            "t1",
            &servers(&[("subscribe_events", "http://server-a")]),
            &http,
            &sender,
        )
        .await;

        let (msg, _) = rx.try_recv().expect("subscription failure event injected");
        assert_eq!(msg.group_id, "t1");
        let Some(SyntheticKind::Tagged(TaggedSyntheticKind::SubscriptionEvent {
            tool_call_id,
            associative,
            r#final,
        })) = &msg.synthetic
        else {
            panic!(
                "expected synthetic subscription event, got {:?}",
                msg.synthetic
            );
        };
        assert_eq!(tool_call_id, "tc-sub");
        assert!(!associative);
        assert!(
            r#final,
            "event must be final so the subscription is removed from tracking"
        );
        let InputMessageContent::User(UserContent::ToolResult(result)) = &msg.content else {
            panic!("expected tool result content");
        };
        assert_eq!(result.id, "tc-sub");
        let ToolResultContent::Text(text) = result.content.first() else {
            panic!("expected text content");
        };
        assert!(
            text.text.contains("subscription") && text.text.contains("failed"),
            "text: {}",
            text.text
        );
        assert!(rx.try_recv().is_err(), "only one message expected");
    }

    #[tokio::test]
    async fn keeps_alive_subscription() {
        let (conv, state, _dir) = tmp_stores();
        conv.ensure_root_thread("t1").await.expect("ensure root");
        append(&conv, "t1", tool_call_msg("tc-sub", "subscribe_events")).await;
        append(&conv, "t1", tool_result_msg("tc-sub", "subscribed")).await;
        state
            .add_active_subscription("t1", "tc-sub")
            .await
            .expect("track subscription");

        let http = MockHttp::default();
        http.respond(STATUS_URL, 200, r#"{"alive": true}"#);
        let (sender, mut rx) = capture_sender();

        reconcile_rap_state(
            &conv,
            &state,
            "t1",
            &servers(&[("subscribe_events", "http://server-a")]),
            &http,
            &sender,
        )
        .await;

        assert_eq!(http.requests().len(), 1);
        assert!(rx.try_recv().is_err(), "alive subscription must be kept");
    }

    #[tokio::test]
    async fn collects_from_open_child_threads() {
        let (conv, state, _dir) = tmp_stores();
        conv.ensure_root_thread("root").await.expect("ensure root");
        let child = conv
            .spawn_thread("root", "tc-spawn", false, None)
            .await
            .expect("spawn child");
        append(&conv, &child, tool_call_msg("tc-child", "my_rap_tool")).await;

        let http = MockHttp::default();
        http.respond(STATUS_URL, 200, r#"{"alive": false}"#);
        let (sender, mut rx) = capture_sender();

        reconcile_rap_state(
            &conv,
            &state,
            "root",
            &servers(&[("my_rap_tool", "http://server-a")]),
            &http,
            &sender,
        )
        .await;

        let requests = http.requests();
        assert_eq!(requests.len(), 1);
        let req: rap_protocol::RapToolCallStatusRequest =
            serde_json::from_str(&requests[0].1).expect("valid status request body");
        assert_eq!(req.thread_id, child, "child thread id must be used");
        assert_eq!(req.tool_call_id, "tc-child");

        let (msg, _) = rx.try_recv().expect("failure message injected");
        assert_eq!(
            msg.group_id, child,
            "failure must be routed to the child thread"
        );
    }

    /// Multiple pieces of pending work across servers are each routed to the
    /// server that owns the tool, and only the dead ones are pruned.
    #[tokio::test]
    async fn routes_to_owning_server_and_prunes_selectively() {
        let (conv, state, _dir) = tmp_stores();
        conv.ensure_root_thread("root").await.expect("ensure root");
        let child = conv
            .spawn_thread("root", "tc-spawn", false, None)
            .await
            .expect("spawn child");

        // Root: active subscription owned by server A (alive).
        append(&conv, "root", tool_call_msg("tc-sub", "subscribe_events")).await;
        append(&conv, "root", tool_result_msg("tc-sub", "subscribed")).await;
        state
            .add_active_subscription("root", "tc-sub")
            .await
            .expect("track subscription");

        // Child: pending tool call owned by server B (dead).
        append(&conv, &child, tool_call_msg("tc-b", "tool_b")).await;

        let http = MockHttp::default();
        http.respond(STATUS_URL, 200, r#"{"alive": true}"#);
        http.respond(
            "http://server-b/tool_call_status",
            200,
            r#"{"alive": false}"#,
        );
        let (sender, mut rx) = capture_sender();

        reconcile_rap_state(
            &conv,
            &state,
            "root",
            &servers(&[
                ("subscribe_events", "http://server-a"),
                ("tool_b", "http://server-b"),
            ]),
            &http,
            &sender,
        )
        .await;

        let requests = http.requests();
        assert_eq!(requests.len(), 2);
        assert!(
            requests
                .iter()
                .any(|(url, body)| { url == STATUS_URL && body.contains("tc-sub") })
        );
        assert!(requests.iter().any(|(url, body)| {
            url == "http://server-b/tool_call_status" && body.contains("tc-b")
        }));

        // Only the dead tool call on server B is pruned.
        let (msg, _) = rx.try_recv().expect("one failure message");
        assert_eq!(msg.group_id, child);
        let InputMessageContent::User(UserContent::ToolResult(result)) = &msg.content else {
            panic!("expected tool result content");
        };
        assert_eq!(result.id, "tc-b");
        assert!(rx.try_recv().is_err(), "alive subscription must be kept");
    }

    // ── End-to-end: injected failures flow through the agent loop ──

    async fn test_catalog(
        model: rig_mock::MockCompletionModel,
    ) -> Arc<crate::models::ModelCatalog> {
        use infinity_agent_core::model_provider::{ModelEntry, SingleModelProvider};
        let entry = ModelEntry {
            model_id: "mock".to_owned(),
            display_name: "mock".to_owned(),
            context_window: 0,
            max_output_tokens: None,
        };
        Arc::new(
            crate::models::ModelCatalog::new(vec![(
                "mock".to_owned(),
                Arc::new(SingleModelProvider::new(entry, model)) as _,
            )])
            .await
            .expect("build test catalog"),
        )
    }

    /// Spawn a real agent loop wired to an `InMemoryMessageSender`, returning
    /// the sender used for reconciliation injection.
    async fn spawn_agent_loop_for_reconcile(
        session_id: &str,
        conv: InMemoryConversationStore,
        state: InMemoryStateStore,
        model: rig_mock::MockCompletionModel,
    ) -> InMemoryMessageSender {
        use crate::session::{AgentMessage, SubscriberMap};

        let (agent_tx, agent_rx) = mpsc::unbounded_channel();
        let (idle_tx, _idle_rx) = mpsc::unbounded_channel();
        let (input_tx, mut input_adapter_rx) = mpsc::unbounded_channel::<(InputMessage, String)>();
        let agent_tx_clone = agent_tx.clone();
        tokio::task::spawn_local(async move {
            while let Some((msg, id)) = input_adapter_rx.recv().await {
                if agent_tx_clone
                    .send(AgentMessage::Input(Box::new(msg), id))
                    .is_err()
                {
                    break;
                }
            }
        });
        let sender = InMemoryMessageSender::new(input_tx);
        let subscriber_map: SubscriberMap = Arc::new(Mutex::new(HashMap::new()));
        let active_threads = Arc::new(Mutex::new(std::collections::HashSet::new()));

        tokio::task::spawn_local(crate::session::agent_loop(
            session_id.to_owned(),
            agent_rx,
            test_catalog(model).await,
            conv,
            state,
            sender.clone(),
            String::new(),
            Arc::new(vec![]),
            Arc::new(None),
            None,
            subscriber_map,
            active_threads,
            idle_tx,
            tokio_util::sync::CancellationToken::new(),
        ));
        sender
    }

    /// A pruned pending tool call wakes the thread worker: the model receives
    /// a completion containing the injected failure result and can continue.
    #[tokio::test(flavor = "current_thread")]
    async fn pruned_tool_call_resumes_thread_worker() {
        use rig_mock::mock_model;
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (conv, state, _dir) = tmp_stores();
                conv.ensure_root_thread("t1").await.expect("ensure root");
                append(&conv, "t1", tool_call_msg("tc-1", "my_rap_tool")).await;

                let (model, mut ctrl) = mock_model();
                let sender =
                    spawn_agent_loop_for_reconcile("t1", conv.clone(), state.clone(), model).await;

                let http = MockHttp::default();
                http.respond(STATUS_URL, 200, r#"{"alive": false}"#);
                reconcile_rap_state(
                    &conv,
                    &state,
                    "t1",
                    &servers(&[("my_rap_tool", "http://server-a")]),
                    &http,
                    &sender,
                )
                .await;

                // The injected failure result triggers a completion whose
                // history contains it.
                let req =
                    tokio::time::timeout(std::time::Duration::from_secs(5), ctrl.next_request())
                        .await
                        .expect("model should be woken by the injected failure result");
                let has_failure = req.chat_history.iter().any(|m| {
                    if let rig::message::Message::User { content } = m
                        && let UserContent::ToolResult(r) = content.first()
                        && let ToolResultContent::Text(t) = r.content.first()
                    {
                        r.id == "tc-1" && t.text.contains("no longer")
                    } else {
                        false
                    }
                });
                assert!(has_failure, "failure result should be in the completion");
                ctrl.send_text("recovered");
                ctrl.finish();
            })
            .await;
    }

    /// A pruned subscription's final failure event removes the subscription
    /// from active tracking once processed by the thread worker.
    #[tokio::test(flavor = "current_thread")]
    async fn pruned_subscription_is_removed_from_tracking() {
        use rig_mock::mock_model;
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (conv, state, _dir) = tmp_stores();
                conv.ensure_root_thread("t1").await.expect("ensure root");
                append(&conv, "t1", tool_call_msg("tc-sub", "subscribe_events")).await;
                append(&conv, "t1", tool_result_msg("tc-sub", "subscribed")).await;
                state
                    .add_active_subscription("t1", "tc-sub")
                    .await
                    .expect("track subscription");

                let (model, mut ctrl) = mock_model();
                let sender =
                    spawn_agent_loop_for_reconcile("t1", conv.clone(), state.clone(), model).await;

                let http = MockHttp::default();
                http.respond(STATUS_URL, 200, r#"{"alive": false}"#);
                reconcile_rap_state(
                    &conv,
                    &state,
                    "t1",
                    &servers(&[("subscribe_events", "http://server-a")]),
                    &http,
                    &sender,
                )
                .await;

                // The injected final subscription event triggers a completion
                // containing the failure text.
                let req =
                    tokio::time::timeout(std::time::Duration::from_secs(5), ctrl.next_request())
                        .await
                        .expect("model should be woken by the injected subscription failure");
                let has_failure = req.chat_history.iter().any(|m| {
                    if let rig::message::Message::User { content } = m
                        && let UserContent::ToolResult(r) = content.first()
                        && let ToolResultContent::Text(t) = r.content.first()
                    {
                        t.text.contains("subscription") && t.text.contains("failed")
                    } else {
                        false
                    }
                });
                assert!(
                    has_failure,
                    "subscription failure should be in the completion"
                );
                ctrl.send_text("acknowledged");
                ctrl.finish();

                // The final event removes the subscription from tracking.
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
                loop {
                    let subs = state
                        .get_active_subscriptions("t1")
                        .await
                        .expect("get subscriptions");
                    if subs.is_empty() {
                        break;
                    }
                    assert!(
                        std::time::Instant::now() < deadline,
                        "subscription should be removed from tracking, still have: {subs:?}"
                    );
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
            })
            .await;
    }
}
