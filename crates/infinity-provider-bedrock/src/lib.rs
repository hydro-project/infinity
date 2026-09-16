//! AWS Bedrock implementation of the [`ModelProvider`] trait, built directly
//! on the official `aws-sdk-bedrockruntime` Converse API.
//!
//! The provider handles all Bedrock-specific request parameters internally:
//! per-model `additional_model_request_fields` (e.g. anthropic thinking
//! configuration and beta flags), per-model max output token limits, and
//! prompt caching (a cache point is appended to the last message of every
//! request). Callers only deal in plain
//! [`CompletionRequest`]s.

mod convert;
mod stream;

use async_trait::async_trait;
use aws_sdk_bedrockruntime::error::{DisplayErrorContext, ProvideErrorMetadata, SdkError};
use infinity_provider_protocol::{
    CompletionError, CompletionRequest, ErrorClass, ModelEntry, ModelProvider, ModelStream,
};
use tokio::sync::OnceCell;

type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// How long to wait for Bedrock to accept a `ConverseStream` request before
/// giving up. Bedrock occasionally black-holes a request (it neither
/// responds nor fails); classifying the timeout as [`ErrorClass::Transient`]
/// lets the caller retry. This deliberately only covers request
/// *initiation* — once a response stream is live it is never artificially
/// cut off by us.
const REQUEST_INITIATION_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// Extract a useful message from an AWS SDK error: the service error message
/// when present (the SDK's plain `Display` omits it), otherwise the full
/// error chain.
pub(crate) fn sdk_error_message<E, R>(err: &SdkError<E, R>) -> String
where
    E: ProvideErrorMetadata + std::error::Error + 'static,
    R: std::fmt::Debug,
{
    match err.message() {
        Some(message) => message.to_owned(),
        None => DisplayErrorContext(err).to_string(),
    }
}

/// Classify a Bedrock error into the retry classification declared to
/// callers, from the service error code (when the failure reached the
/// service) and the error message.
///
/// The message heuristics live *here*, in the provider — the agent runtime
/// only ever sees the resulting [`ErrorClass`].
pub(crate) fn classify_bedrock_error(code: Option<&str>, message: &str) -> ErrorClass {
    let msg = message.to_ascii_lowercase();
    if matches!(
        code,
        Some("ThrottlingException" | "ServiceQuotaExceededException")
    ) || msg.contains("please wait before trying again")
        || msg.contains("too many requests")
        || msg.contains("please try again")
    {
        return ErrorClass::Throttled;
    }
    // Context overflow: Bedrock reports it as a ValidationException
    // ("Input is too long for requested model."), anthropic models as
    // "input length and `max_tokens` exceed context limit".
    if msg.contains("too long")
        || msg.contains("too large")
        || msg.contains("input length")
        || (msg.contains("exceed") && msg.contains("context"))
    {
        return ErrorClass::ContextOverflow;
    }
    if matches!(
        code,
        Some(
            "InternalServerException"
                | "ServiceUnavailableException"
                | "ModelTimeoutException"
                | "ModelNotReadyException"
                | "ModelStreamErrorException"
                | "ModelErrorException"
        )
    ) || msg.contains("unexpected end of stream")
        || msg.contains("unexpected error when processing the request")
        || msg.contains("is unable to process your request")
    {
        return ErrorClass::Transient;
    }
    ErrorClass::Fatal
}

/// Classify a full [`SdkError`]: service errors go through
/// [`classify_bedrock_error`]; transport-level failures (dispatch, timeout,
/// unparsable response) are transient; request construction failures are
/// ours and fatal.
pub(crate) fn classify_sdk_error<E, R>(err: &SdkError<E, R>) -> ErrorClass
where
    E: ProvideErrorMetadata + std::error::Error + 'static,
    R: std::fmt::Debug,
{
    match err {
        SdkError::ConstructionFailure(_) => ErrorClass::Fatal,
        SdkError::TimeoutError(_) | SdkError::DispatchFailure(_) | SdkError::ResponseError(_) => {
            ErrorClass::Transient
        }
        _ => classify_bedrock_error(err.code(), &sdk_error_message(err)),
    }
}

/// Convert an [`SdkError`] into a classified [`CompletionError`].
pub(crate) fn completion_error<E, R>(err: &SdkError<E, R>) -> CompletionError
where
    E: ProvideErrorMetadata + std::error::Error + 'static,
    R: std::fmt::Debug,
{
    CompletionError::provider(classify_sdk_error(err), sdk_error_message(err))
}

/// A model offered by the Bedrock provider, along with the Bedrock-specific
/// invocation configuration that stays internal to this crate.
struct BedrockModel {
    entry: ModelEntry,
    /// The actual Bedrock model id to invoke. `entry.model_id` is the
    /// provider-scoped id shown in pickers and must be unique, but two
    /// catalog entries may expose the *same* Bedrock model with different
    /// request parameters (e.g. `claude-opus-4-6-v1` vs the
    /// `claude-opus-4-6-v1:1m` entry, which only adds the 1M-context beta
    /// flag) — this field maps such entries back to the real model id.
    bedrock_model_id: String,
    /// Extra `additional_model_request_fields` merged into every request.
    additional_request_params: Option<serde_json::Value>,
}

/// [`ModelProvider`] backed by AWS Bedrock.
pub struct BedrockProvider {
    /// Lazily initialized so [`BedrockProvider::from_env`] stays synchronous
    /// (AWS config loading is async).
    client: OnceCell<aws_sdk_bedrockruntime::Client>,
    models: Vec<BedrockModel>,
}

impl BedrockProvider {
    /// Create a provider using AWS configuration from the environment
    /// (region, credentials, profiles — the standard AWS lookup chain).
    pub fn from_env() -> Self {
        Self {
            client: OnceCell::new(),
            models: default_models(),
        }
    }

    /// Create a provider from an existing Bedrock runtime client.
    pub fn new(client: aws_sdk_bedrockruntime::Client) -> Self {
        Self {
            client: OnceCell::from(client),
            models: default_models(),
        }
    }

    async fn client(&self) -> &aws_sdk_bedrockruntime::Client {
        self.client
            .get_or_init(|| async {
                let config = aws_config::defaults(aws_config::BehaviorVersion::latest())
                    .load()
                    .await;
                aws_sdk_bedrockruntime::Client::new(&config)
            })
            .await
    }
}

fn default_models() -> Vec<BedrockModel> {
    let summarized_adaptive_thinking = serde_json::json!({
        "thinking": {
            "type": "adaptive",
            "display": "summarized"
        }
    });
    let adaptive_thinking = serde_json::json!({
        "thinking": {
            "type": "adaptive"
        }
    });
    vec![
        BedrockModel {
            entry: ModelEntry {
                model_id: "global.anthropic.claude-fable-5".to_owned(),
                display_name: "claude-fable-5".to_owned(),
                context_window: 1_000_000,
                max_output_tokens: Some(128_000),
                supports_image_input: true,
            },
            bedrock_model_id: "global.anthropic.claude-fable-5".to_owned(),
            additional_request_params: Some(summarized_adaptive_thinking.clone()),
        },
        BedrockModel {
            entry: ModelEntry {
                model_id: "global.anthropic.claude-opus-4-8".to_owned(),
                display_name: "claude-opus-4.8".to_owned(),
                context_window: 1_000_000,
                max_output_tokens: Some(128_000),
                supports_image_input: true,
            },
            bedrock_model_id: "global.anthropic.claude-opus-4-8".to_owned(),
            additional_request_params: Some(summarized_adaptive_thinking),
        },
        BedrockModel {
            entry: ModelEntry {
                // Same Bedrock model as the entry below — this picker entry
                // just enables the 1M-context beta. `bedrock_model_id` maps
                // the synthetic `:1m` id back to the real model id.
                model_id: "global.anthropic.claude-opus-4-6-v1:1m".to_owned(),
                display_name: "claude-opus-4-6 1m".to_owned(),
                context_window: 1_000_000,
                max_output_tokens: Some(128_000),
                supports_image_input: true,
            },
            bedrock_model_id: "global.anthropic.claude-opus-4-6-v1".to_owned(),
            additional_request_params: Some(serde_json::json!({
                "thinking": {
                    "type": "adaptive"
                },
                "anthropic_beta": ["context-1m-2025-08-07"]
            })),
        },
        BedrockModel {
            entry: ModelEntry {
                model_id: "global.anthropic.claude-opus-4-6-v1".to_owned(),
                display_name: "claude-opus-4-6".to_owned(),
                context_window: 200_000,
                max_output_tokens: Some(128_000),
                supports_image_input: true,
            },
            bedrock_model_id: "global.anthropic.claude-opus-4-6-v1".to_owned(),
            additional_request_params: Some(adaptive_thinking.clone()),
        },
        BedrockModel {
            entry: ModelEntry {
                model_id: "global.anthropic.claude-sonnet-4-6".to_owned(),
                display_name: "claude-sonnet-4-6".to_owned(),
                context_window: 200_000,
                max_output_tokens: Some(64_000),
                supports_image_input: true,
            },
            bedrock_model_id: "global.anthropic.claude-sonnet-4-6".to_owned(),
            additional_request_params: Some(adaptive_thinking),
        },
    ]
}

/// The fully resolved parameters for one Bedrock invocation, separated from
/// the send so the request assembly is unit-testable.
struct PreparedRequest {
    bedrock_model_id: String,
    system: Option<Vec<aws_sdk_bedrockruntime::types::SystemContentBlock>>,
    messages: Vec<aws_sdk_bedrockruntime::types::Message>,
    tool_config: Option<aws_sdk_bedrockruntime::types::ToolConfiguration>,
    inference_config: aws_sdk_bedrockruntime::types::InferenceConfiguration,
    additional_params: Option<aws_smithy_types::Document>,
}

fn prepare_request(
    models: &[BedrockModel],
    model_id: &str,
    mut request: CompletionRequest,
) -> Result<PreparedRequest, CompletionError> {
    let known = models.iter().find(|m| m.entry.model_id == model_id);

    // Resolve the actual Bedrock model id (provider-scoped ids may alias
    // the same Bedrock model with different parameters). Unknown ids are
    // passed through unchanged so callers can invoke arbitrary models.
    let bedrock_model_id = known
        .map(|m| m.bedrock_model_id.clone())
        .unwrap_or_else(|| model_id.to_owned());

    // Merge the per-model request parameters (e.g. anthropic thinking
    // config) with any caller-supplied additional params (caller wins).
    // Unknown models get only what the caller provided, so non-anthropic
    // models are not sent anthropic-specific parameters.
    let params = match (
        known.and_then(|m| m.additional_request_params.clone()),
        request.additional_params.take(),
    ) {
        (Some(mut base), Some(caller)) => {
            merge_params(&mut base, &caller);
            Some(base)
        }
        (base, caller) => base.or(caller),
    };

    let max_tokens = request
        .max_tokens
        .or_else(|| known.and_then(|m| m.entry.max_output_tokens));

    let mut messages = convert::messages(request.chat_history)?;
    // Prompt caching: cache everything up to (and including) the last
    // message, so the shared history prefix is reused across calls.
    convert::append_cache_point(&mut messages);

    Ok(PreparedRequest {
        bedrock_model_id,
        system: request.preamble.map(|preamble| {
            vec![aws_sdk_bedrockruntime::types::SystemContentBlock::Text(
                preamble,
            )]
        }),
        messages,
        tool_config: convert::tool_config(&request.tools)?,
        inference_config: aws_sdk_bedrockruntime::types::InferenceConfiguration::builder()
            .set_max_tokens(max_tokens.map(|t| t.min(i32::MAX as u64) as i32))
            .build(),
        additional_params: params.map(convert::json_to_document),
    })
}

#[async_trait]
impl ModelProvider for BedrockProvider {
    async fn list_models(&self) -> Result<Vec<ModelEntry>, BoxError> {
        Ok(self.models.iter().map(|m| m.entry.clone()).collect())
    }

    async fn invoke_model(
        &self,
        model_id: &str,
        request: CompletionRequest,
    ) -> Result<ModelStream, CompletionError> {
        let prepared = prepare_request(&self.models, model_id, request)?;

        let send = self
            .client()
            .await
            .converse_stream()
            .model_id(prepared.bedrock_model_id)
            .set_system(prepared.system)
            .set_messages(Some(prepared.messages))
            .set_tool_config(prepared.tool_config)
            .set_inference_config(Some(prepared.inference_config))
            .set_additional_model_request_fields(prepared.additional_params)
            .send();

        // Guard request *initiation* only (see REQUEST_INITIATION_TIMEOUT);
        // the returned stream itself is never timed out.
        let response = tokio::time::timeout(REQUEST_INITIATION_TIMEOUT, send)
            .await
            .map_err(|_| {
                tracing::error!(
                    "Bedrock ConverseStream request initiation timed out after {:?}",
                    REQUEST_INITIATION_TIMEOUT
                );
                CompletionError::provider(
                    ErrorClass::Transient,
                    format!(
                        "timed out waiting {}s for Bedrock to accept the request",
                        REQUEST_INITIATION_TIMEOUT.as_secs()
                    ),
                )
            })?
            .map_err(|e| {
                tracing::error!(error = %DisplayErrorContext(&e), "Bedrock ConverseStream SDK error");
                completion_error(&e)
            })?;

        Ok(stream::convert_stream(response))
    }
}

/// Shallow-merge `extra`'s top-level keys into `base` (extra wins).
fn merge_params(base: &mut serde_json::Value, extra: &serde_json::Value) {
    if let (Some(base_obj), Some(extra_obj)) = (base.as_object_mut(), extra.as_object()) {
        for (k, v) in extra_obj {
            base_obj.insert(k.clone(), v.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use infinity_provider_protocol::Message;

    fn request(prompt: &str) -> CompletionRequest {
        CompletionRequest {
            preamble: Some("system".to_owned()),
            chat_history: vec![Message::user(prompt)],
            tools: vec![],
            max_tokens: None,
            additional_params: None,
        }
    }

    #[test]
    fn known_model_gets_defaults_and_thinking_params() {
        let models = default_models();
        let prepared =
            prepare_request(&models, "global.anthropic.claude-sonnet-4-6", request("hi"))
                .expect("prepare");
        assert_eq!(
            prepared.bedrock_model_id,
            "global.anthropic.claude-sonnet-4-6"
        );
        // max_tokens defaults from the model entry.
        assert_eq!(prepared.inference_config.max_tokens, Some(64_000));
        // Adaptive thinking is applied.
        let params = prepared.additional_params.expect("params set");
        let aws_smithy_types::Document::Object(obj) = params else {
            panic!("expected object params");
        };
        assert!(obj.contains_key("thinking"));
        // System prompt present.
        assert!(prepared.system.is_some());
        // Cache point on the last (only) message.
        let last = prepared.messages.last().expect("one message");
        assert!(matches!(
            last.content.last(),
            Some(aws_sdk_bedrockruntime::types::ContentBlock::CachePoint(_))
        ));
    }

    #[test]
    fn caller_params_win_in_merge() {
        let models = default_models();
        let mut req = request("hi");
        req.additional_params = Some(serde_json::json!({
            "thinking": { "type": "disabled" },
            "custom": 1,
        }));
        let prepared =
            prepare_request(&models, "global.anthropic.claude-sonnet-4-6", req).expect("prepare");
        let aws_smithy_types::Document::Object(obj) =
            prepared.additional_params.expect("params set")
        else {
            panic!("expected object params");
        };
        // Caller override replaced the per-model thinking config.
        assert_eq!(
            obj.get("thinking"),
            Some(&convert::json_to_document(
                serde_json::json!({ "type": "disabled" })
            ))
        );
        assert!(obj.contains_key("custom"));
    }

    #[test]
    fn unknown_model_passes_through_without_params() {
        let models = default_models();
        let prepared =
            prepare_request(&models, "amazon.nova-pro-v1:0", request("hi")).expect("prepare");
        assert_eq!(prepared.bedrock_model_id, "amazon.nova-pro-v1:0");
        assert!(prepared.additional_params.is_none());
        assert_eq!(prepared.inference_config.max_tokens, None);
    }

    #[test]
    fn caller_max_tokens_wins_over_model_default() {
        let models = default_models();
        let mut req = request("hi");
        req.max_tokens = Some(1000);
        let prepared =
            prepare_request(&models, "global.anthropic.claude-sonnet-4-6", req).expect("prepare");
        assert_eq!(prepared.inference_config.max_tokens, Some(1000));
    }

    #[test]
    fn provider_scoped_alias_resolves_to_bedrock_model_id() {
        let models = default_models();
        let prepared = prepare_request(
            &models,
            "global.anthropic.claude-opus-4-6-v1:1m",
            request("hi"),
        )
        .expect("prepare");
        assert_eq!(
            prepared.bedrock_model_id,
            "global.anthropic.claude-opus-4-6-v1"
        );
        let aws_smithy_types::Document::Object(obj) =
            prepared.additional_params.expect("params set")
        else {
            panic!("expected object params");
        };
        assert!(obj.contains_key("anthropic_beta"));
    }

    // ── Error classification ──
    //
    // Classification is mostly string/code matching against real Bedrock
    // responses, so asserting the match table here would be tautological.
    // The real assertions live in `tests/live.rs` (feature `live-tests`),
    // which classifies actual Bedrock service errors using local AWS
    // credentials.

    /// A black-holed `ConverseStream` request must fail with a transient
    /// (retryable) error after the initiation timeout instead of hanging
    /// forever. Uses a Bedrock client whose HTTP connector never responds.
    #[tokio::test(start_paused = true)]
    async fn request_initiation_times_out_with_transient_error() {
        #[derive(Debug)]
        struct NeverRespond;
        impl aws_smithy_runtime_api::client::http::HttpConnector for NeverRespond {
            fn call(
                &self,
                _request: aws_smithy_runtime_api::client::orchestrator::HttpRequest,
            ) -> aws_smithy_runtime_api::client::http::HttpConnectorFuture {
                aws_smithy_runtime_api::client::http::HttpConnectorFuture::new(
                    std::future::pending(),
                )
            }
        }
        impl aws_smithy_runtime_api::client::http::HttpClient for NeverRespond {
            fn http_connector(
                &self,
                _settings: &aws_smithy_runtime_api::client::http::HttpConnectorSettings,
                _components: &aws_smithy_runtime_api::client::runtime_components::RuntimeComponents,
            ) -> aws_smithy_runtime_api::client::http::SharedHttpConnector {
                aws_smithy_runtime_api::client::http::SharedHttpConnector::new(NeverRespond)
            }
        }

        let config = aws_sdk_bedrockruntime::Config::builder()
            .behavior_version(aws_config::BehaviorVersion::latest())
            .region(aws_sdk_bedrockruntime::config::Region::new("us-east-1"))
            .credentials_provider(aws_sdk_bedrockruntime::config::Credentials::for_tests())
            .http_client(NeverRespond)
            .build();
        let provider = BedrockProvider::new(aws_sdk_bedrockruntime::Client::from_conf(config));

        let started = tokio::time::Instant::now();
        let Err(err) = provider
            .invoke_model("global.anthropic.claude-sonnet-4-6", request("hi"))
            .await
        else {
            panic!("black-holed request must time out")
        };
        assert_eq!(err.class(), ErrorClass::Transient);
        assert!(
            err.to_string().contains("timed out"),
            "unexpected message: {err}"
        );
        // The timeout must be the initiation timeout, not some other layer.
        assert!(started.elapsed() >= REQUEST_INITIATION_TIMEOUT);
    }
}
