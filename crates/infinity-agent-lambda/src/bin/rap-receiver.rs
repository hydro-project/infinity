//! RAP (Reactive Agent Protocol) HTTP receiver.
//!
//! Tool Lambdas and external RAP servers POST callbacks (tool results,
//! subscription events, OAuth challenges, user choices) to this function's
//! URL. Each callback is converted to the runtime's `InputMessage` with the
//! shared `infinity_rap_bridge::prepare_callback` path (the same conversion
//! the Infinity Code daemon uses, so the platforms cannot drift) and forwarded
//!
//! HTTP contract (unchanged from the previous JS implementation): POST a
//! JSON `RapCallback` body; responds `200 {"ok":true}` on success,
//! `400 {"error":...}` for malformed payloads, `500 {"error":...}` on
//! delivery failure.

use aws_lambda_events::event::lambda_function_urls::{
    LambdaFunctionUrlRequest, LambdaFunctionUrlResponse,
};
use aws_sdk_sqs::Client as SqsClient;
use base64::Engine;
use lambda_runtime::{Error, LambdaEvent, run, service_fn, tracing};

use infinity_rap_bridge::{CallbackDelivery, prepare_callback};
use rap_protocol::RapCallback;

#[tokio::main]
async fn main() -> Result<(), Error> {
    tracing::init_default_subscriber();

    let config = aws_config::load_defaults(aws_config::BehaviorVersion::latest()).await;
    let sqs_client = SqsClient::new(&config);
    let input_queue_url =
        std::env::var("INPUT_QUEUE_URL").expect("INPUT_QUEUE_URL environment variable must be set");

    run(service_fn(
        move |event: LambdaEvent<LambdaFunctionUrlRequest>| {
            let sqs_client = sqs_client.clone();
            let input_queue_url = input_queue_url.clone();
            async move { handle(event, &sqs_client, &input_queue_url).await }
        },
    ))
    .await
}

async fn handle(
    event: LambdaEvent<LambdaFunctionUrlRequest>,
    sqs_client: &SqsClient,
    input_queue_url: &str,
) -> Result<LambdaFunctionUrlResponse, Error> {
    let body = match request_body(&event.payload) {
        Ok(body) => body,
        Err(e) => return Ok(respond(400, &format!(r#"{{"error":"{e}"}}"#))),
    };

    let enqueue = match prepare_enqueue(&body) {
        Ok(action) => action,
        Err(e) => {
            tracing::warn!("rejecting RAP callback: {e}");
            return Ok(respond(400, &serde_json::json!({ "error": e }).to_string()));
        }
    };

    let Some(enqueue) = enqueue else {
        // View updates are a display side channel; without a live client
        // attached there is nothing to update, but the callback is valid.
        return Ok(respond(200, r#"{"ok":true}"#));
    };

    if let Err(e) = sqs_client
        .send_message()
        .queue_url(input_queue_url)
        .message_body(enqueue.message_body)
        .message_group_id(enqueue.group_id)
        .message_deduplication_id(enqueue.dedup_id)
        .send()
        .await
    {
        tracing::error!("failed to enqueue RAP callback: {e}");
        return Ok(respond(500, r#"{"error":"Internal error"}"#));
    }

    Ok(respond(200, r#"{"ok":true}"#))
}

/// A prepared SQS send: the serialized `InputMessage`, its FIFO message
/// group (the target thread), and the deduplication ID.
struct PreparedEnqueue {
    message_body: String,
    group_id: String,
    dedup_id: String,
}

/// Parse and convert one callback body. `Ok(None)` means the callback is
/// valid but is not agent input (view updates).
fn prepare_enqueue(body: &str) -> Result<Option<PreparedEnqueue>, String> {
    let callback: RapCallback =
        serde_json::from_str(body).map_err(|e| format!("invalid RAP callback: {e}"))?;

    let CallbackDelivery::Input { message, dedup_id } = prepare_callback(callback) else {
        return Ok(None);
    };

    let group_id = message.group_id.clone();
    let message_body = serde_json::to_string(&message)
        .map_err(|e| format!("failed to serialize input message: {e}"))?;
    Ok(Some(PreparedEnqueue {
        message_body,
        group_id,
        dedup_id,
    }))
}

/// Extract the request body, decoding base64 when the platform indicates it.
fn request_body(request: &LambdaFunctionUrlRequest) -> Result<String, String> {
    let Some(body) = &request.body else {
        return Err("missing request body".to_owned());
    };
    if !request.is_base64_encoded {
        return Ok(body.clone());
    }
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(body)
        .map_err(|e| format!("invalid base64 body: {e}"))?;
    String::from_utf8(bytes).map_err(|e| format!("body is not UTF-8: {e}"))
}

fn respond(status_code: i64, body: &str) -> LambdaFunctionUrlResponse {
    let mut response = LambdaFunctionUrlResponse::default();
    response.status_code = status_code;
    response.headers.insert(
        aws_lambda_events::http::header::CONTENT_TYPE,
        "application/json"
            .parse()
            .expect("bug: static header value is valid"),
    );
    response.body = Some(body.to_owned());
    response
}
