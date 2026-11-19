//! End-to-end tests: real mailboxes linked over Unix sockets, with RAP
//! callbacks captured by a real HTTP callback server.

use std::path::{Path, PathBuf};
use std::time::Duration;

use rap_agent_mail::{MailConfig, Running};
use rap_protocol::{RapCallback, RapInvocation, ThreadId};
use tokio::sync::mpsc;

const WAIT: Duration = Duration::from_secs(20);

struct Host {
    name: String,
    dir: PathBuf,
    running: Option<Running>,
    callback_url: String,
    callbacks: mpsc::UnboundedReceiver<RapCallback>,
}

/// Write `<dir>/config.json` naming the mailbox and its socket peers.
fn write_config(dir: &Path, name: &str, peers: &[(&str, &Path)]) {
    let peers: Vec<_> = peers
        .iter()
        .map(|(name, dir)| serde_json::json!({ "name": name, "socket": dir.join("peer.sock") }))
        .collect();
    std::fs::create_dir_all(dir).expect("create state dir");
    std::fs::write(
        dir.join("config.json"),
        serde_json::json!({ "name": name, "peers": peers }).to_string(),
    )
    .expect("write config");
}

impl Host {
    async fn new(root: &Path, name: &str, peers: &[(&str, &Path)]) -> Self {
        let dir = root.join(name);
        write_config(&dir, name, peers);
        let (callback_url, callbacks) = rap_client::callback_server::start_callback_channel()
            .await
            .expect("start callback server");
        let mut host = Self {
            name: name.to_owned(),
            dir,
            running: None,
            callback_url,
            callbacks,
        };
        host.start().await;
        host
    }

    async fn start(&mut self) {
        let mut config =
            MailConfig::load(self.dir.clone(), None, None).expect("config should load");
        config.ack_wait = Duration::from_secs(2);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind RAP port");
        self.running = Some(rap_agent_mail::start(config, listener).expect("mailbox starts"));
    }

    fn stop(&mut self) {
        self.running = None;
    }

    fn running(&self) -> &Running {
        self.running.as_ref().expect("host is running")
    }

    /// Invoke `operation` as tool call `id` in thread `thread` (whose
    /// ancestors, root first, are `ancestors`).
    async fn invoke(
        &self,
        operation: &str,
        args: serde_json::Value,
        thread: &str,
        ancestors: &[&str],
        id: &str,
    ) {
        let invocation = RapInvocation {
            operation: operation.to_owned(),
            arguments: args,
            id: id.into(),
            call_id: None,
            callback_url: self.callback_url.clone(),
            group_id: thread.into(),
            user_id: None,
            thread_ancestors: (!ancestors.is_empty())
                .then(|| ancestors.iter().map(|a| ThreadId::from(*a)).collect()),
        };
        let resp = reqwest::Client::new()
            .post(format!("{}/invoke", self.running().rap_url))
            .json(&invocation)
            .send()
            .await
            .expect("invoke request");
        assert!(resp.status().is_success());
    }

    async fn post(&self, path: &str, body: serde_json::Value) {
        let resp = reqwest::Client::new()
            .post(format!("{}{path}", self.running().rap_url))
            .json(&body)
            .send()
            .await
            .expect("lifecycle request");
        assert!(resp.status().is_success());
    }

    async fn next(&mut self) -> RapCallback {
        tokio::time::timeout(WAIT, self.callbacks.recv())
            .await
            .unwrap_or_else(|_| panic!("{}: no callback within {WAIT:?}", self.name))
            .expect("callback channel open")
    }

    /// The next callback, which must be the tool result for call `id`.
    async fn result(&mut self, id: &str) -> String {
        match self.next().await {
            RapCallback::ToolResult(r) => {
                assert_eq!(r.id.as_str(), id, "result for an unexpected call: {r:?}");
                r.text.expect("text result")
            }
            other => panic!(
                "{}: expected tool result for {id}, got {other:?}",
                self.name
            ),
        }
    }

    /// The next callback, which must be a subscription event; returns
    /// `(tool_call_id, parsed event JSON, associative)`.
    async fn event(&mut self) -> (String, serde_json::Value, bool) {
        match self.next().await {
            RapCallback::SubscriptionEvent(e) => (
                e.tool_call_id.as_str().to_owned(),
                serde_json::from_str(&e.text).expect("event text is JSON"),
                e.associative,
            ),
            other => panic!("{}: expected subscription event, got {other:?}", self.name),
        }
    }

    async fn expect_quiet(&mut self, dur: Duration) {
        if let Ok(Some(cb)) = tokio::time::timeout(dur, self.callbacks.recv()).await {
            panic!("{}: unexpected callback {cb:?}", self.name);
        }
    }

    async fn open_inbox(&mut self, name: &str, thread: &str, id: &str) {
        self.invoke(
            "open_inbox",
            serde_json::json!({ "name": name }),
            thread,
            &[],
            id,
        )
        .await;
        let text = self.result(id).await;
        assert!(text.starts_with("Inbox open"), "{text}");
    }

    async fn wait_linked(&self, peer: &str) {
        let deadline = tokio::time::Instant::now() + WAIT;
        while self.running().mailbox.status_of(peer) != "connected" {
            assert!(
                tokio::time::Instant::now() < deadline,
                "{} never linked to {peer}: {}",
                self.name,
                self.running().mailbox.status_of(peer)
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
}

#[tokio::test]
async fn ask_and_reply_across_hosts() {
    let root = tempfile::tempdir().expect("tempdir");
    let mut beta = Host::new(root.path(), "beta", &[]).await;
    let beta_dir = beta.dir.clone();
    let mut alpha = Host::new(root.path(), "alpha", &[("beta", &beta_dir)]).await;
    alpha.wait_linked("beta").await;

    beta.open_inbox("reviewer", "b-root", "b-open").await;
    alpha.open_inbox("planner", "a-root", "a-open").await;

    alpha
        .invoke(
            "ask_agent",
            serde_json::json!({ "to": "beta/reviewer", "text": "Is PR 7 safe to merge?" }),
            "a-root",
            &[],
            "a-ask",
        )
        .await;
    let (call, event, associative) = beta.event().await;
    assert_eq!(call, "b-open");
    assert!(!associative, "default delivery spawns a child thread");
    assert_eq!(event["from"], "alpha/planner");
    assert_eq!(event["kind"], "ask");
    assert_eq!(event["text"], "Is PR 7 safe to merge?");
    let message_id = event["message_id"].as_str().expect("message id").to_owned();
    // The ask stays pending: no result until the reply.
    alpha.expect_quiet(Duration::from_millis(500)).await;

    // The reply comes from the per-message child thread, speaking as the inbox.
    beta.invoke(
        "reply",
        serde_json::json!({ "message_id": message_id, "text": "Yes, ship it." }),
        "b-child",
        &["b-root"],
        "b-reply",
    )
    .await;
    assert_eq!(
        beta.result("b-reply").await,
        "Reply delivered to alpha/planner."
    );
    assert_eq!(
        alpha.result("a-ask").await,
        "Reply from beta/reviewer (another agent, not your user):\n\nYes, ship it."
    );

    beta.invoke(
        "reply",
        serde_json::json!({ "message_id": message_id, "text": "again" }),
        "b-child",
        &["b-root"],
        "b-reply-2",
    )
    .await;
    let text = beta.result("b-reply-2").await;
    assert!(text.contains("already answered"), "{text}");

    // Messages flow the other way over the link alpha dialed.
    beta.invoke(
        "send_message",
        serde_json::json!({ "to": "alpha/planner", "text": "FYI: merged." }),
        "b-root",
        &[],
        "b-tell",
    )
    .await;
    let text = beta.result("b-tell").await;
    assert!(text.starts_with("Delivered to alpha/planner"), "{text}");
    let (call, event, _) = alpha.event().await;
    assert_eq!(call, "a-open");
    assert_eq!(event["from"], "beta/reviewer");
    assert_eq!(event["kind"], "tell");
}

#[tokio::test]
async fn queued_messages_survive_restarts_and_deliver_once() {
    let root = tempfile::tempdir().expect("tempdir");
    let mut beta = Host::new(root.path(), "beta", &[]).await;
    let beta_dir = beta.dir.clone();
    let mut alpha = Host::new(root.path(), "alpha", &[("beta", &beta_dir)]).await;
    beta.open_inbox("reviewer", "b-root", "b-open").await;
    alpha.wait_linked("beta").await;

    beta.stop();
    alpha
        .invoke(
            "send_message",
            serde_json::json!({ "to": "beta/reviewer", "text": "while you were out" }),
            "a-root",
            &[],
            "a-tell",
        )
        .await;
    let text = alpha.result("a-tell").await;
    assert!(text.starts_with("Queued for beta/reviewer"), "{text}");

    // The sender restarts too before the peer comes back: the outbox is
    // durable on its side, the inbox registry on the recipient's.
    alpha.stop();
    alpha.start().await;
    beta.start().await;

    let (call, event, _) = beta.event().await;
    assert_eq!(call, "b-open");
    assert_eq!(event["text"], "while you were out");
    assert_eq!(event["from"], "alpha (no inbox)");

    // Once acknowledged, nothing is retransmitted across reconnects.
    alpha.wait_linked("beta").await;
    alpha.stop();
    alpha.start().await;
    alpha.wait_linked("beta").await;
    beta.expect_quiet(Duration::from_secs(1)).await;
    alpha.expect_quiet(Duration::from_millis(100)).await;
}

#[tokio::test]
async fn queued_ask_to_missing_inbox_fails_the_pending_call() {
    let root = tempfile::tempdir().expect("tempdir");
    let beta_dir = root.path().join("beta");
    let mut alpha = Host::new(root.path(), "alpha", &[("beta", &beta_dir)]).await;

    alpha
        .invoke(
            "ask_agent",
            serde_json::json!({ "to": "beta/nobody", "text": "hello?" }),
            "a-root",
            &[],
            "a-ask",
        )
        .await;
    // Unreachable peer: the ask waits rather than failing.
    alpha.expect_quiet(Duration::from_secs(3)).await;

    let _beta = Host::new(root.path(), "beta", &[]).await;
    let text = alpha.result("a-ask").await;
    assert!(
        text.starts_with("Error: your question to beta/nobody was not delivered")
            && text.contains("no inbox named `nobody`"),
        "{text}"
    );
}

#[tokio::test]
async fn withdrawn_questions_cannot_be_answered() {
    let root = tempfile::tempdir().expect("tempdir");
    let mut beta = Host::new(root.path(), "beta", &[]).await;
    let beta_dir = beta.dir.clone();
    let mut alpha = Host::new(root.path(), "alpha", &[("beta", &beta_dir)]).await;
    alpha.wait_linked("beta").await;
    beta.open_inbox("reviewer", "b-root", "b-open").await;

    alpha
        .invoke(
            "ask_agent",
            serde_json::json!({ "to": "beta/reviewer", "text": "q" }),
            "a-root",
            &[],
            "a-ask",
        )
        .await;
    let (_, event, _) = beta.event().await;
    let message_id = event["message_id"].as_str().expect("message id").to_owned();

    // The user interrupts the waiting asker.
    alpha
        .post(
            "/cancel_tool_call",
            serde_json::json!({ "thread_id": "a-root", "tool_call_id": "a-ask" }),
        )
        .await;
    tokio::time::sleep(Duration::from_millis(500)).await;

    beta.invoke(
        "reply",
        serde_json::json!({ "message_id": message_id, "text": "too late" }),
        "b-root",
        &[],
        "b-reply",
    )
    .await;
    let text = beta.result("b-reply").await;
    // The Withdraw reached beta (an unacknowledged ask is still withdrawn
    // there), so beta refuses the reply itself.
    assert!(
        text.contains("withdrew this question; the reply was not sent"),
        "{text}"
    );
    alpha.expect_quiet(Duration::from_millis(500)).await;
}

#[tokio::test]
async fn closing_a_session_closes_inboxes_its_threads_own() {
    let root = tempfile::tempdir().expect("tempdir");
    let mut alpha = Host::new(root.path(), "alpha", &[]).await;
    // A child thread of session "s1" owns the inbox.
    alpha
        .invoke(
            "open_inbox",
            serde_json::json!({ "name": "worker" }),
            "s1-child",
            &["s1"],
            "open",
        )
        .await;
    assert!(alpha.result("open").await.starts_with("Inbox open"));

    // The daemon reports the session (its root thread) closed.
    alpha
        .post("/close_thread", serde_json::json!({ "thread_id": "s1" }))
        .await;
    tokio::time::sleep(Duration::from_millis(300)).await;

    alpha
        .invoke(
            "send_message",
            serde_json::json!({ "to": "worker", "text": "hi" }),
            "other",
            &[],
            "tell",
        )
        .await;
    let text = alpha.result("tell").await;
    assert!(text.contains("no inbox named `worker`"), "{text}");
}

#[tokio::test]
async fn local_delivery_rejections_and_limits() {
    let root = tempfile::tempdir().expect("tempdir");
    let mut alpha = Host::new(root.path(), "alpha", &[]).await;
    alpha.open_inbox("planner", "a-root", "a-open").await;

    // Bare names and the own host name both address local inboxes.
    for (i, to) in ["planner", "alpha/planner"].iter().enumerate() {
        let id = format!("tell-{i}");
        alpha
            .invoke(
                "send_message",
                serde_json::json!({ "to": to, "text": "hi" }),
                "other",
                &[],
                &id,
            )
            .await;
        // Event and result race; collect both.
        let (a, b) = (alpha.next().await, alpha.next().await);
        let results: Vec<_> = [a, b]
            .into_iter()
            .filter_map(|cb| match cb {
                RapCallback::ToolResult(r) => r.text,
                _ => None,
            })
            .collect();
        assert_eq!(results.len(), 1);
        assert!(
            results[0].starts_with("Delivered to alpha/planner"),
            "{results:?}"
        );
    }

    for (args, expected) in [
        (
            serde_json::json!({ "to": "nobody", "text": "hi" }),
            "no inbox named `nobody`",
        ),
        (
            serde_json::json!({ "to": "mars/rover", "text": "hi" }),
            "unknown host `mars`",
        ),
        (
            serde_json::json!({ "to": "a/b/c", "text": "hi" }),
            "invalid inbox name",
        ),
        (
            serde_json::json!({ "to": "planner", "text": "  " }),
            "must not be empty",
        ),
    ] {
        alpha
            .invoke("send_message", args, "other", &[], "bad")
            .await;
        let text = alpha.result("bad").await;
        assert!(
            text.starts_with("Error:") && text.contains(expected),
            "{text}"
        );
    }

    // Another thread cannot silently steal an inbox name...
    alpha
        .invoke(
            "open_inbox",
            serde_json::json!({ "name": "planner" }),
            "thief",
            &[],
            "steal",
        )
        .await;
    let text = alpha.result("steal").await;
    assert!(text.contains("already open in another thread"), "{text}");
    // ...but can take it over explicitly, which ends the old subscription.
    alpha
        .invoke(
            "open_inbox",
            serde_json::json!({ "name": "planner", "replace": true }),
            "thief",
            &[],
            "take",
        )
        .await;
    let (a, b) = (alpha.next().await, alpha.next().await);
    let ended_old = [&a, &b].iter().any(|cb| {
        matches!(cb, RapCallback::SubscriptionEvent(e)
            if e.tool_call_id.as_str() == "a-open" && e.r#final == Some(true))
    });
    assert!(ended_old, "old subscription not ended: {a:?} {b:?}");
    // Runaway senders hit the per-session rate limit (every send that passes
    // argument validation counts, delivered or not), even when each message
    // comes from a fresh per-message child thread of the same session.
    let mut limited = false;
    for i in 0..12 {
        let id = format!("spam-{i}");
        let child = format!("spammer-child-{i}");
        alpha
            .invoke(
                "send_message",
                serde_json::json!({ "to": "nobody", "text": "spam" }),
                &child,
                &["spammer"],
                &id,
            )
            .await;
        if alpha.result(&id).await.contains("rate limit") {
            limited = true;
            assert_eq!(i, 10, "limit should trip on the 11th send in a minute");
            break;
        }
    }
    assert!(limited);
}

#[tokio::test]
async fn withdrawn_queued_question_is_never_delivered() {
    let root = tempfile::tempdir().expect("tempdir");
    let mut beta = Host::new(root.path(), "beta", &[]).await;
    let beta_dir = beta.dir.clone();
    beta.open_inbox("reviewer", "b-root", "b-open").await;
    beta.stop();
    let alpha = Host::new(root.path(), "alpha", &[("beta", &beta_dir)]).await;

    alpha
        .invoke(
            "ask_agent",
            serde_json::json!({ "to": "beta/reviewer", "text": "still there?" }),
            "a-root",
            &[],
            "a-ask",
        )
        .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    alpha
        .post(
            "/cancel_tool_call",
            serde_json::json!({ "thread_id": "a-root", "tool_call_id": "a-ask" }),
        )
        .await;
    tokio::time::sleep(Duration::from_millis(300)).await;

    beta.start().await;
    alpha.wait_linked("beta").await;
    beta.expect_quiet(Duration::from_secs(2)).await;
}

#[tokio::test]
async fn replies_match_when_hosts_dial_each_other_under_other_names() {
    let root = tempfile::tempdir().expect("tempdir");
    let (alpha_dir, beta_dir) = (root.path().join("alpha"), root.path().join("beta"));
    // Beta calls alpha "lap", not "alpha": replies may come back on either
    // link, and both must count as coming from the host that was asked.
    let mut beta = Host::new(root.path(), "beta", &[("lap", &alpha_dir)]).await;
    let mut alpha = Host::new(root.path(), "alpha", &[("beta", &beta_dir)]).await;
    alpha.wait_linked("beta").await;
    beta.wait_linked("lap").await;
    beta.open_inbox("reviewer", "b-root", "b-open").await;

    for i in 0..4 {
        let ask = format!("a-ask-{i}");
        alpha
            .invoke(
                "ask_agent",
                serde_json::json!({ "to": "beta/reviewer", "text": format!("q{i}") }),
                &format!("a-{i}"),
                &[],
                &ask,
            )
            .await;
        let (_, event, _) = beta.event().await;
        assert_eq!(
            event["from"], "lap (no inbox)",
            "beta names alpha its own way"
        );
        let reply = format!("b-reply-{i}");
        beta.invoke(
            "reply",
            serde_json::json!({ "message_id": event["message_id"], "text": format!("a{i}") }),
            "b-root",
            &[],
            &reply,
        )
        .await;
        let text = beta.result(&reply).await;
        assert!(text.starts_with("Reply delivered"), "{text}");
        let text = alpha.result(&ask).await;
        assert!(text.ends_with(&format!("a{i}")), "{text}");
    }
}

#[tokio::test]
async fn link_refused_when_peer_has_the_same_name() {
    let root = tempfile::tempdir().expect("tempdir");
    // A mailbox named "alpha" listening in directory "twin".
    let twin_dir = root.path().join("twin");
    write_config(&twin_dir, "alpha", &[]);
    let twin_config = MailConfig::load(twin_dir.clone(), None, None).expect("config");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let _twin = rap_agent_mail::start(twin_config, listener).expect("start");

    let alpha = Host::new(root.path(), "alpha", &[("twin", &twin_dir)]).await;
    let deadline = tokio::time::Instant::now() + WAIT;
    loop {
        let status = alpha.running().mailbox.status_of("twin");
        if status.contains("own name") {
            break;
        }
        assert!(tokio::time::Instant::now() < deadline, "status: {status}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test]
async fn closing_a_session_declines_its_open_questions_and_ends_its_subscription_later() {
    let root = tempfile::tempdir().expect("tempdir");
    let mut beta = Host::new(root.path(), "beta", &[]).await;
    let beta_dir = beta.dir.clone();
    let mut alpha = Host::new(root.path(), "alpha", &[("beta", &beta_dir)]).await;
    alpha.wait_linked("beta").await;
    beta.open_inbox("reviewer", "b-root", "b-open").await;

    alpha
        .invoke(
            "ask_agent",
            serde_json::json!({ "to": "beta/reviewer", "text": "q" }),
            "a-root",
            &[],
            "a-ask",
        )
        .await;
    beta.event().await;

    // Beta's session is stopped before it answers: the asker hears so.
    beta.post(
        "/close_thread",
        serde_json::json!({ "thread_id": "b-root" }),
    )
    .await;
    let text = alpha.result("a-ask").await;
    assert!(
        text.starts_with("Error: your question to beta/reviewer will not be answered"),
        "{text}"
    );

    // The closed inbox's subscription can only be ended once the session
    // runs again. A call right after the close may come from a tool call
    // still in flight in the stopped session, so it does not count...
    beta.invoke(
        "list_agents",
        serde_json::json!({}),
        "b-root",
        &[],
        "b-list",
    )
    .await;
    let text = beta.result("b-list").await;
    assert!(text.contains("(no open inboxes)"), "{text}");
    beta.expect_quiet(Duration::from_millis(300)).await;

    // ...but reopening an inbox does: the old subscription ends after the
    // new one is confirmed.
    beta.open_inbox("reviewer", "b-root", "b-reopen").await;
    match beta.next().await {
        RapCallback::SubscriptionEvent(e) => {
            assert_eq!(e.tool_call_id.as_str(), "b-open");
            assert_eq!(e.r#final, Some(true));
            assert!(!e.associative, "the notice must not interrupt the thread");
        }
        other => panic!("expected the final event, got {other:?}"),
    }
}

#[tokio::test]
async fn hosts_that_dial_in_can_be_messaged_while_away() {
    let root = tempfile::tempdir().expect("tempdir");
    let mut beta = Host::new(root.path(), "beta", &[]).await;
    let beta_dir = beta.dir.clone();
    let mut alpha = Host::new(root.path(), "alpha", &[("beta", &beta_dir)]).await;
    alpha.open_inbox("planner", "a-root", "a-open").await;
    alpha.wait_linked("beta").await;
    // Beta does not dial alpha, so it only knows alpha from its inbound link.
    alpha.stop();
    tokio::time::sleep(Duration::from_millis(200)).await;

    beta.invoke(
        "send_message",
        serde_json::json!({ "to": "alpha/planner", "text": "for later" }),
        "b-root",
        &[],
        "b-tell",
    )
    .await;
    let text = beta.result("b-tell").await;
    assert!(text.starts_with("Queued for alpha/planner"), "{text}");

    alpha.start().await;
    let (_, event, _) = alpha.event().await;
    assert_eq!(event["text"], "for later");
}
