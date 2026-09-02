#![warn(missing_docs)]

//! Connect RAP tool servers to local Infinity agent systems.
//!
//! [`RapToolSet`] discovers tools and builds lifecycle notifications.
//! [`RapCallbackBridge`] binds the callback destination separately, making
//! input routing and display-only view handling explicit.

use infinity_agent_core::message::InputMessage;
use infinity_agent_core::system::local::ChannelSender;
use infinity_agent_core::tools::Tool;
use infinity_agent_core::tools::rap_tool::RapTool;
use infinity_agent_core::traits::InputSender;
use rap_client::callback_server::{bind_callback_listener, start_callback_server_on};
use rap_client::http::{InMemoryToolsetCache, SimpleHttpClient};
use rap_client::notifier::RapNotifier;
use rap_client::toolset_loader::ToolsetLoader;
use rap_protocol::{RapCallback, RapViewUpdate};

mod callback;
use callback::convert_callback;
use tokio::sync::mpsc;

/// Error type returned while connecting or invoking RAP servers.
pub type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// A callback converted into either agent input or display-only view state.
#[derive(Debug)]
pub enum CallbackDelivery {
    /// Input for an agent thread, with the ID used to deduplicate delivery.
    Input {
        /// Converted runtime input.
        message: Box<InputMessage>,
        /// Deduplication ID for the input queue.
        dedup_id: String,
    },
    /// A display-only view update that must not enter agent history.
    ViewUpdate(RapViewUpdate),
}

/// Convert one RAP callback and assign its input-queue deduplication ID.
pub fn prepare_callback(callback: RapCallback) -> CallbackDelivery {
    if let RapCallback::ViewUpdate(update) = callback {
        return CallbackDelivery::ViewUpdate(update);
    }

    let dedup_id = match &callback {
        RapCallback::ToolResult(result) => format!("rap-tool-result-{}", result.id),
        RapCallback::OAuth(oauth) => format!("rap-oauth-{}", oauth.id),
        RapCallback::UserChoice(choice) => format!("rap-user-choice-{}", choice.id),
        // RAP subscription events do not carry a per-event identity. A fresh
        // ID preserves repeated events from one subscription.
        RapCallback::SubscriptionEvent(_) => uuid::Uuid::new_v4().to_string(),
        RapCallback::ViewUpdate(_) => unreachable!("view update returned above"),
    };
    let message = convert_callback(callback).expect("bug: non-view RAP callback must convert");
    CallbackDelivery::Input {
        message: Box::new(message),
        dedup_id,
    }
}

/// A bound RAP callback listener that has not started accepting requests.
pub struct RapCallbackBridge {
    listener: tokio::net::TcpListener,
    callback_url: String,
}

impl RapCallbackBridge {
    /// Bind a callback listener on localhost.
    pub async fn bind() -> Result<Self, BoxError> {
        let (listener, callback_url) = bind_callback_listener().await?;
        Ok(Self {
            listener,
            callback_url,
        })
    }

    /// Use an already-bound callback listener.
    pub fn from_listener(listener: tokio::net::TcpListener) -> Result<Self, BoxError> {
        let callback_url = format!("http://127.0.0.1:{}", listener.local_addr()?.port());
        Ok(Self {
            listener,
            callback_url,
        })
    }

    /// URL that RAP invocations should use for callbacks.
    pub fn callback_url(&self) -> &str {
        &self.callback_url
    }

    /// Start callback reception, forwarding agent inputs into `sender` and
    /// returning view updates on a channel.
    ///
    /// Each converted input is sent with its prepared deduplication ID, so
    /// HTTP retries of one callback are absorbed by the state store. Send
    /// failures are logged and do not stop the listener. Admission belongs
    /// to the receiving system: the local router consults the state store's
    /// existence and stopped-thread checks before spawning a driver, so an
    /// embedding using this form only needs to store or drop the returned view
    /// updates.
    pub fn serve_into<S>(
        self,
        sender: S,
    ) -> (
        mpsc::UnboundedReceiver<RapViewUpdate>,
        tokio::task::JoinHandle<()>,
    )
    where
        S: InputSender + 'static,
    {
        let (view_tx, view_rx) = mpsc::unbounded_channel();
        let task = start_callback_server_on(self.listener, move |callback| {
            let sender = sender.clone();
            let view_tx = view_tx.clone();
            async move {
                match prepare_callback(callback) {
                    CallbackDelivery::Input { message, dedup_id } => {
                        if let Err(error) = sender.send_to_input_queue(*message, &dedup_id).await {
                            tracing::warn!(%error, "failed to route RAP callback");
                        }
                    }
                    CallbackDelivery::ViewUpdate(update) => {
                        if view_tx.send(update).is_err() {
                            tracing::warn!("RAP view-update destination has stopped");
                        }
                    }
                }
            }
        });
        (view_rx, task)
    }
}

/// RAP tools discovered from one or more servers, ready for a local system.
///
/// Callback reception is configured separately through
/// [`RapCallbackBridge`]. Pass that bridge's URL to [`connect`](Self::connect),
/// then serve it into the local system's sender after the system is built.
pub struct RapToolSet {
    tools: Vec<RapTool<SimpleHttpClient>>,
    server_urls: Vec<String>,
}

impl RapToolSet {
    /// Discover tools from each server using an explicit callback destination.
    ///
    /// `session_id` scopes the in-memory manifest cache. Each server must
    /// expose `/.well-known/rap-toolset`. `callback_url` should name a bound
    /// [`RapCallbackBridge`] or another receiver for RAP callbacks.
    pub async fn connect(
        server_urls: impl IntoIterator<Item = String>,
        session_id: &str,
        callback_url: impl Into<String>,
    ) -> Result<Self, BoxError> {
        let server_urls: Vec<String> = server_urls.into_iter().collect();
        let callback_url = callback_url.into();
        let http = SimpleHttpClient::new();
        let loaded = ToolsetLoader::new(http.clone(), InMemoryToolsetCache::new())
            .load_toolsets(&server_urls, session_id)
            .await?;

        let mut tools = Vec::new();
        for toolset in loaded {
            let endpoint = toolset.manifest.endpoint;
            for definition in toolset.manifest.tools {
                tools.push(RapTool {
                    descriptor: definition.into(),
                    endpoint: endpoint.clone(),
                    http_client: http.clone(),
                    callback_url: Some(callback_url.clone()),
                });
            }
        }

        Ok(Self { tools, server_urls })
    }

    /// Build the discovered tools for registration on a local agent system.
    pub fn tools(&self) -> Vec<Box<dyn Tool<ChannelSender>>> {
        self.tools
            .iter()
            .cloned()
            .map(|tool| Box::new(tool) as Box<dyn Tool<ChannelSender>>)
            .collect()
    }

    /// Build the lifecycle notifier for the connected RAP servers.
    pub fn notifier(&self) -> RapNotifier<SimpleHttpClient> {
        RapNotifier::new(self.server_urls.clone(), SimpleHttpClient::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use rap_protocol::{
        RapOAuth, RapSubscriptionEvent, RapToolResult, RapUserChoice, RapViewUpdate,
    };
    use std::sync::{Arc, Mutex};

    #[test]
    fn stable_dedup_for_single_callbacks() {
        let cases = [
            (
                RapCallback::ToolResult(RapToolResult {
                    group_id: "g".into(),
                    id: "call".into(),
                    call_id: None,
                    text: Some("done".into()),
                    content: None,
                    display_as: None,
                    subscription: None,
                }),
                "rap-tool-result-call",
            ),
            (
                RapCallback::OAuth(RapOAuth {
                    group_id: "g".into(),
                    id: "auth".into(),
                    call_id: None,
                    auth_url: "https://example.com".into(),
                }),
                "rap-oauth-auth",
            ),
            (
                RapCallback::UserChoice(RapUserChoice {
                    group_id: "g".into(),
                    id: "choice".into(),
                    call_id: None,
                    prompt: "pick".into(),
                    choices: vec!["a".into()],
                    default: 0,
                    response_url: "https://example.com".into(),
                }),
                "rap-user-choice-choice",
            ),
        ];
        for (callback, expected) in cases {
            let CallbackDelivery::Input { dedup_id, .. } = prepare_callback(callback) else {
                panic!("expected agent input")
            };
            assert_eq!(dedup_id, expected);
        }
    }

    #[test]
    fn repeated_subscription_events_are_not_deduplicated() {
        let callback = || {
            RapCallback::SubscriptionEvent(RapSubscriptionEvent {
                group_id: "g".into(),
                tool_call_id: "call".into(),
                text: "tick".into(),
                associative: false,
                r#final: None,
            })
        };
        let CallbackDelivery::Input {
            dedup_id: first, ..
        } = prepare_callback(callback())
        else {
            panic!("expected agent input")
        };
        let CallbackDelivery::Input {
            dedup_id: second, ..
        } = prepare_callback(callback())
        else {
            panic!("expected agent input")
        };
        assert_ne!(first, second);
    }

    #[test]
    fn view_update_remains_out_of_agent_history() {
        let CallbackDelivery::ViewUpdate(update) =
            prepare_callback(RapCallback::ViewUpdate(RapViewUpdate {
                group_id: "g".into(),
                view_type: "diff".into(),
                content: serde_json::json!({}),
            }))
        else {
            panic!("expected view update")
        };
        assert_eq!(update.view_type, "diff");
    }

    /// An [`InputSender`] that records deliveries for assertions.
    #[derive(Clone, Default)]
    struct RecordingSender {
        sent: Arc<Mutex<Vec<(rap_protocol::ThreadId, String)>>>,
    }

    #[derive(Debug)]
    struct RecordingSendError;

    impl std::fmt::Display for RecordingSendError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "recording sender never fails")
        }
    }

    impl std::error::Error for RecordingSendError {}

    #[async_trait]
    impl InputSender for RecordingSender {
        type Error = RecordingSendError;

        async fn send_to_input_queue(
            &self,
            message: InputMessage,
            dedup_id: &str,
        ) -> Result<(), RecordingSendError> {
            self.sent
                .lock()
                .expect("bug: test mutex poisoned")
                .push((message.group_id, dedup_id.to_owned()));
            Ok(())
        }
    }

    /// `serve_into` routes agent inputs into the sender with the prepared
    /// deduplication ID and emits view updates on the returned channel.
    #[tokio::test]
    async fn serve_into_forwards_inputs_and_emits_views() {
        use rap_client::http::HttpClient;

        let bridge = RapCallbackBridge::bind().await.expect("bind listener");
        let url = bridge.callback_url().to_owned();
        let sender = RecordingSender::default();
        let (mut views, _server_task) = bridge.serve_into(sender.clone());

        let http = SimpleHttpClient::new();
        let tool_result = serde_json::to_string(&RapCallback::ToolResult(RapToolResult {
            group_id: "t1".into(),
            id: "call-1".into(),
            call_id: None,
            text: Some("done".into()),
            content: None,
            display_as: None,
            subscription: None,
        }))
        .expect("serialize tool result callback");
        // The callback server acknowledges only after the handler has
        // forwarded the input, so the recording is visible once POST returns.
        assert_eq!(http.post(&url, &tool_result).await.expect("post"), 200);
        assert_eq!(
            sender
                .sent
                .lock()
                .expect("bug: test mutex poisoned")
                .as_slice(),
            &[("t1".into(), "rap-tool-result-call-1".to_owned())]
        );

        let view = serde_json::to_string(&RapCallback::ViewUpdate(RapViewUpdate {
            group_id: "t1".into(),
            view_type: "diff".into(),
            content: serde_json::json!({ "lines": 3 }),
        }))
        .expect("serialize view update callback");
        assert_eq!(http.post(&url, &view).await.expect("post"), 200);
        let update = tokio::time::timeout(std::time::Duration::from_secs(5), views.recv())
            .await
            .expect("timed out waiting for the view update")
            .expect("view channel closed");
        assert_eq!(update.view_type, "diff");
        assert_eq!(
            sender.sent.lock().expect("bug: test mutex poisoned").len(),
            1,
            "view updates must not become agent input"
        );
    }
}
