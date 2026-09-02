//! Shared RAP tool invocation: one implementation of building and posting a
//! [`RapInvocation`], used by every surface that exposes RAP tools (the
//! Lambda runtime through [`RapTool`], the `infinity-rap-bridge` crate, and
//! the Infinity Code daemon's managed servers).
//!
//! Auth is handled by the `HttpClient` implementation (SigV4 for Lambda,
//! plain for local surfaces).

use async_trait::async_trait;
use rap_protocol::RapInvocation;
use tracing;

use super::{Tool, ToolContext, send_tool_error};
use crate::traits::InputSender;
use rap_client::http::HttpClient;

type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// The model-facing surface of one RAP tool, copied from a toolset manifest
/// entry. Embedding-specific tool types hold one of these and delegate their
/// `Tool` metadata methods to it, so the manifest-to-tool field mapping
/// lives in exactly one place.
#[derive(Clone)]
pub struct RapToolDescriptor {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
    pub display_script: Option<String>,
}

impl From<rap_protocol::ToolDef> for RapToolDescriptor {
    fn from(def: rap_protocol::ToolDef) -> Self {
        Self {
            name: def.name,
            description: def.description,
            parameters: def.input_schema,
            display_script: def.display_script,
        }
    }
}

/// One RAP invocation to send: the resolved endpoint plus the tool-call
/// identity from [`Tool::execute`].
pub struct RapInvocationParams<'a> {
    /// The server's invocation endpoint (already resolved by the caller).
    pub endpoint: &'a str,
    /// The tool name from the server's manifest.
    pub operation: &'a str,
    /// The model-provided arguments.
    pub arguments: serde_json::Value,
    /// The tool-call ID from `execute`.
    pub id: String,
    /// The provider call ID from `execute`.
    pub call_id: Option<String>,
    /// Callback destination for the server's asynchronous results. `None`
    /// uses [`ToolContext::callback_url`]; embeddings that run their own
    /// callback listener pass its URL instead.
    pub callback_url: Option<&'a str>,
}

/// Build one [`RapInvocation`] and POST it to the server.
///
/// Error policy, applied identically on every surface:
///
/// - A transport failure (the POST itself fails) is returned as `Err`, which
///   the runtime records as a generic failed tool result.
/// - A non-2xx response returns `Ok` after delivering a descriptive error
///   tool result naming the operation, endpoint, and status. The server was
///   reachable but rejected the invocation, so no callback will ever settle
///   the call; the descriptive result lets the model act on the failure
///   instead of waiting forever.
pub async fn invoke_rap_tool<H, M>(
    http_client: &H,
    params: RapInvocationParams<'_>,
    context: &ToolContext<M>,
) -> Result<(), BoxError>
where
    H: HttpClient,
    M: InputSender + 'static,
    M::Error: 'static,
{
    let RapInvocationParams {
        endpoint,
        operation,
        arguments,
        id,
        call_id,
        callback_url,
    } = params;

    let thread_ancestors = (context.thread_stack.len() > 1)
        .then(|| context.thread_stack[..context.thread_stack.len() - 1].to_vec());

    let invocation = RapInvocation {
        operation: operation.to_owned(),
        arguments,
        id: id.clone(),
        call_id: call_id.clone(),
        callback_url: callback_url
            .unwrap_or(context.callback_url.as_str())
            .to_owned(),
        group_id: context.group_id.clone(),
        user_id: context.user_id.clone(),
        thread_ancestors,
    };

    let body = serde_json::to_string(&invocation)?;
    let status = http_client
        .post(endpoint, &body)
        .await
        .map_err(|e| Box::new(e) as BoxError)?;

    if !(200..300).contains(&status) {
        tracing::warn!("RAP tool {operation} at {endpoint} returned status {status}");
        send_tool_error(
            context,
            &id,
            call_id,
            format!(
                "RAP tool '{operation}' failed: the server at {endpoint} \
                 responded with HTTP {status}. No result will arrive for \
                 this call."
            ),
        )
        .await?;
        return Ok(());
    }
    tracing::info!("Invoked RAP tool {operation} (status: {status})");
    Ok(())
}

/// A RAP tool that invokes a remote tool server endpoint via HTTP.
/// Generic over the HTTP client (SigV4-signed for Lambda, plain for CLI)
/// and the input sender (SQS for Lambda, mpsc for CLI).
#[derive(Clone)]
pub struct RapTool<H: HttpClient> {
    pub descriptor: RapToolDescriptor,
    pub endpoint: String,
    pub http_client: H,
    /// Callback destination supplied to the RAP server. When absent, the
    /// enclosing system's callback URL is used.
    pub callback_url: Option<String>,
}

#[async_trait]
impl<H: HttpClient + 'static, M: InputSender + 'static> Tool<M> for RapTool<H> {
    fn name(&self) -> &str {
        &self.descriptor.name
    }

    fn description(&self) -> &str {
        &self.descriptor.description
    }

    fn parameters(&self) -> serde_json::Value {
        self.descriptor.parameters.clone()
    }

    fn display_script(&self) -> Option<&str> {
        self.descriptor.display_script.as_deref()
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        id: String,
        call_id: Option<String>,
        context: &ToolContext<M>,
    ) -> Result<(), BoxError> {
        invoke_rap_tool(
            &self.http_client,
            RapInvocationParams {
                endpoint: &self.endpoint,
                operation: &self.descriptor.name,
                arguments: args,
                id,
                call_id,
                callback_url: self.callback_url.as_deref(),
            },
            context,
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::{InputMessage, InputMessageContent};
    use infinity_provider_protocol::message::UserContent;
    use std::sync::{Arc, Mutex};

    #[derive(Debug)]
    struct TestError(String);
    impl std::fmt::Display for TestError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "{}", self.0)
        }
    }
    impl std::error::Error for TestError {}

    /// Mock HTTP client returning a fixed status (or a transport error) and
    /// recording each posted `(url, body)`.
    #[derive(Clone)]
    struct MockHttp {
        status: Option<u16>,
        posts: Arc<Mutex<Vec<(String, String)>>>,
    }

    impl MockHttp {
        fn returning(status: u16) -> Self {
            Self {
                status: Some(status),
                posts: Arc::new(Mutex::new(Vec::new())),
            }
        }

        fn failing() -> Self {
            Self {
                status: None,
                posts: Arc::new(Mutex::new(Vec::new())),
            }
        }
    }

    #[async_trait]
    impl HttpClient for MockHttp {
        type Error = TestError;

        async fn post(&self, url: &str, body: &str) -> Result<u16, TestError> {
            self.posts
                .lock()
                .expect("bug: mock posts mutex poisoned")
                .push((url.to_owned(), body.to_owned()));
            self.status
                .ok_or_else(|| TestError("connection refused".to_owned()))
        }

        async fn get(&self, _url: &str) -> Result<(u16, Vec<u8>), TestError> {
            Err(TestError("unused".to_owned()))
        }
    }

    /// Input sender that records every enqueued message.
    #[derive(Clone)]
    struct CapturingSender {
        messages: Arc<Mutex<Vec<(InputMessage, String)>>>,
    }

    #[async_trait]
    impl InputSender for CapturingSender {
        type Error = TestError;

        async fn send_to_input_queue(
            &self,
            message: InputMessage,
            dedup_id: &str,
        ) -> Result<(), TestError> {
            self.messages
                .lock()
                .expect("bug: mock messages mutex poisoned")
                .push((message, dedup_id.to_owned()));
            Ok(())
        }
    }

    fn test_context(thread_stack: Vec<&str>) -> ToolContext<CapturingSender> {
        ToolContext {
            message_sender: CapturingSender {
                messages: Arc::new(Mutex::new(Vec::new())),
            },
            group_id: "thread-1".into(),
            callback_url: "http://context-callback".to_owned(),
            user_id: Some("user-1".to_owned()),
            thread_stack: thread_stack
                .into_iter()
                .map(rap_protocol::ThreadId::from)
                .collect(),
        }
    }

    fn params<'a>(arguments: serde_json::Value) -> RapInvocationParams<'a> {
        RapInvocationParams {
            endpoint: "http://server/invoke",
            operation: "lookup",
            arguments,
            id: "tc-1".to_owned(),
            call_id: Some("call-1".to_owned()),
            callback_url: None,
        }
    }

    fn sent_messages(context: &ToolContext<CapturingSender>) -> Vec<(InputMessage, String)> {
        context
            .message_sender
            .messages
            .lock()
            .expect("bug: mock messages mutex poisoned")
            .clone()
    }

    #[tokio::test]
    async fn non_2xx_delivers_descriptive_error_result() {
        let http = MockHttp::returning(503);
        let context = test_context(vec!["thread-1"]);

        invoke_rap_tool(&http, params(serde_json::json!({})), &context)
            .await
            .expect("non-2xx is reported to the model, not returned as Err");

        let sent = sent_messages(&context);
        assert_eq!(sent.len(), 1, "exactly one error result is enqueued");
        let (message, dedup_id) = &sent[0];
        assert_eq!(dedup_id, "tc-1", "dedup ID is the tool-call ID");
        assert_eq!(message.group_id.as_str(), "thread-1");
        let InputMessageContent::User(UserContent::ToolResult(result)) = &message.content else {
            panic!("expected a tool result, got {:?}", message.content);
        };
        assert_eq!(result.id, "tc-1");
        assert_eq!(result.call_id.as_deref(), Some("call-1"));
        let Some(infinity_provider_protocol::message::ToolResultContent::Text(text)) =
            result.content.first()
        else {
            panic!("expected text content");
        };
        for expected in ["Error:", "lookup", "http://server/invoke", "503"] {
            assert!(
                text.text.contains(expected),
                "error text must mention {expected:?}: {}",
                text.text
            );
        }
    }

    #[tokio::test]
    async fn transport_failure_returns_err_without_result() {
        let http = MockHttp::failing();
        let context = test_context(vec!["thread-1"]);

        let result = invoke_rap_tool(&http, params(serde_json::json!({})), &context).await;

        assert!(result.is_err(), "transport failures surface as Err");
        assert!(
            sent_messages(&context).is_empty(),
            "the runtime's generic fallback owns transport failures"
        );
    }

    #[tokio::test]
    async fn subthread_invocation_carries_ancestors_and_callback_override() {
        let http = MockHttp::returning(204);
        let context = test_context(vec!["root", "middle", "leaf"]);
        let mut request = params(serde_json::json!({}));
        request.callback_url = Some("http://bridge-listener");

        invoke_rap_tool(&http, request, &context)
            .await
            .expect("2xx must succeed");

        let posts = http.posts.lock().expect("bug: mock posts mutex poisoned");
        let invocation: RapInvocation =
            serde_json::from_str(&posts[0].1).expect("posted body is a RapInvocation");
        assert_eq!(
            invocation.thread_ancestors,
            Some(vec!["root".into(), "middle".into()]),
            "ancestors exclude the current thread"
        );
        assert_eq!(invocation.callback_url, "http://bridge-listener");
        assert!(
            sent_messages(&context).is_empty(),
            "a successful invocation enqueues nothing; the result arrives by callback"
        );
    }

    /// A root thread has no ancestors to report, and without an override the
    /// invocation carries the context's callback URL.
    #[tokio::test]
    async fn root_invocation_defaults_to_context_callback() {
        let http = MockHttp::returning(200);
        let context = test_context(vec!["thread-1"]);

        invoke_rap_tool(&http, params(serde_json::json!({})), &context)
            .await
            .expect("2xx must succeed");

        let posts = http.posts.lock().expect("bug: mock posts mutex poisoned");
        let invocation: RapInvocation =
            serde_json::from_str(&posts[0].1).expect("posted body is a RapInvocation");
        assert_eq!(invocation.thread_ancestors, None);
        assert_eq!(invocation.callback_url, "http://context-callback");
    }
}
