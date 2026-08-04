use std::sync::Arc;

use aws_lambda_events::event::sqs::SqsEvent;
use aws_sdk_dsql::Client as DsqlClient;
use aws_sdk_dynamodb::Client as DynamoDbClient;
use aws_sdk_scheduler::Client as SchedulerClient;
use aws_sdk_sqs::Client as SqsClient;
use infinity_provider_bedrock::BedrockProvider;
use lambda_runtime::{Error, LambdaEvent, tracing};

use infinity_agent_core::event_processor;
use infinity_agent_core::message::InputMessage;
use infinity_agent_core::system::{
    AgentEvent, AgentSystemBuilder, EventCollector, NoDeferral, StaticModel,
};
use infinity_agent_core::tools::Tool;
use infinity_agent_core::tools::config::ToolsConfig;
use infinity_agent_core::tools::rap_tool::RapTool;
use infinity_provider_protocol::{ModelEntry, ModelProvider};
use rap_client::toolset_loader::ToolsetLoader;

use crate::conversation_history::DsqlConversationStore;
use crate::state_store::DynamoDbStateStore;
use crate::tools::rap_http::RapHttpClient;
use crate::tools::sleep::{SleepTool, SleepUntilTool};
use crate::tools::sqs_sender::SqsMessageSender;
use crate::tools::toolset_cache::DynamoDbToolsetCache;

/// The model invoked for all Lambda completions (hardcoded for now).
const MODEL_ID: &str = "global.anthropic.claude-sonnet-4-6";

pub(crate) async fn function_handler(event: LambdaEvent<SqsEvent>) -> Result<(), Error> {
    let payload = event.payload;
    tracing::info!("Payload: {:?}", payload);

    let config = aws_config::load_from_env().await;
    let dynamodb_client = DynamoDbClient::new(&config);
    let dsql_client = DsqlClient::new(&config);
    let sqs_client = SqsClient::new(&config);
    let scheduler_client = SchedulerClient::new(&config);
    let table_name = "InfinityAgentsState".to_owned();
    let output_queue_url = std::env::var("OUTPUT_QUEUE_URL").unwrap_or_default();
    let scheduler_role_arn = std::env::var("SCHEDULER_ROLE_ARN").unwrap_or_default();
    let dsql_cluster_endpoint = std::env::var("DSQL_CLUSTER_ENDPOINT")
        .map_err(|_| Error::from("DSQL_CLUSTER_ENDPOINT environment variable is required"))?;

    let conversation_store =
        DsqlConversationStore::new(&dsql_client, &dsql_cluster_endpoint).await?;
    let state_store = DynamoDbStateStore::new(dynamodb_client.clone(), table_name.clone());

    // Load tools configuration
    let tools_config = if let Ok(ddb_key) = std::env::var("TOOLS_CONFIG_DDB_KEY") {
        match ToolsConfig::from_json(
            dynamodb_client
                .get_item()
                .table_name(&table_name)
                .key(
                    "session",
                    aws_sdk_dynamodb::types::AttributeValue::S(ddb_key.clone()),
                )
                .send()
                .await?
                .item()
                .and_then(|i| i.get("config").and_then(|v| v.as_s().ok()))
                .unwrap_or(&"{}".to_owned()),
        ) {
            Ok(config) => {
                tracing::info!("Loaded tools config from DynamoDB key {}", ddb_key);
                Some(config)
            }
            Err(e) => {
                tracing::warn!("Failed to load tools config from DynamoDB: {}", e);
                None
            }
        }
    } else {
        let config_path =
            std::env::var("TOOLS_CONFIG_PATH").unwrap_or_else(|_| "tools.json".to_owned());
        ToolsConfig::from_file(&config_path)
            .ok()
            .or_else(|| ToolsConfig::from_env().ok())
    };

    let toolset_server_urls: Vec<String> = tools_config
        .as_ref()
        .map(|tc| {
            tc.toolset_server_urls()
                .into_iter()
                .map(|(url, _)| url)
                .collect()
        })
        .unwrap_or_default();

    let http_client = RapHttpClient::new(&config);

    let rap_notifier =
        rap_client::notifier::RapNotifier::new(toolset_server_urls.clone(), http_client.clone());

    let toolset_cache = DynamoDbToolsetCache::new(dynamodb_client.clone(), table_name.clone());
    let toolset_loader = ToolsetLoader::new(http_client.clone(), toolset_cache);

    let input_queue_url = std::env::var("INPUT_QUEUE_URL").unwrap_or_default();
    let input_queue_arn = std::env::var("INPUT_QUEUE_ARN").unwrap_or_default();
    let callback_url = std::env::var("RAP_CALLBACK_URL")
        .or_else(|_| std::env::var("RAP_RECEIVER_URL"))
        .unwrap_or_default();

    let sender = SqsMessageSender {
        sqs_client: sqs_client.clone(),
        input_queue_url: input_queue_url.clone(),
        output_queue_url: output_queue_url.clone(),
    };

    // Parse all records into a batch — FIFO guarantees they share the same group_id
    let mut inputs: Vec<(InputMessage, String)> = Vec::new();
    for record in payload.records {
        let message_id = record.message_id.unwrap_or_default();
        let body = record.body.expect("SQS record missing body");
        let input_msg: InputMessage = serde_json::from_str(&body)?;
        inputs.push((input_msg, message_id));
    }

    if inputs.is_empty() {
        return Ok(());
    }

    let group_id = inputs[0].0.group_id.clone();

    // Build tools once for the shared group_id
    let mut tool_impls: Vec<Box<dyn Tool<SqsMessageSender>>> = Vec::new();

    // Load RAP toolsets
    if !toolset_server_urls.is_empty() {
        let session_id = group_id.clone();
        match toolset_loader
            .load_toolsets(&toolset_server_urls, &session_id)
            .await
        {
            Ok(loaded) => {
                for ts in loaded {
                    let endpoint = ts.manifest.endpoint.clone();
                    for def in ts.manifest.tools {
                        tool_impls.push(Box::new(RapTool {
                            name: def.name,
                            description: def.description,
                            parameters: def.input_schema,
                            endpoint: endpoint.clone(),
                            http_client: http_client.clone(),
                            display_script: def.display_script,
                        }));
                    }
                }
            }
            Err(e) => tracing::warn!("Failed to load RAP toolsets: {}", e),
        }
    }

    // Platform-specific sleep tools (durable timers via EventBridge / SQS
    // delays). The remaining built-in tools come from the system builder.
    tool_impls.push(Box::new(SleepTool {
        scheduler_client: scheduler_client.clone(),
        scheduler_role_arn: scheduler_role_arn.clone(),
        delay_queue_url: std::env::var("DELAY_QUEUE_URL").unwrap_or_default(),
        input_queue_arn: input_queue_arn.clone(),
    }));
    tool_impls.push(Box::new(SleepUntilTool {
        scheduler_client: scheduler_client.clone(),
        scheduler_role_arn: scheduler_role_arn.clone(),
        delay_queue_url: std::env::var("DELAY_QUEUE_URL").unwrap_or_default(),
        input_queue_arn,
    }));

    // Resolve the model's capabilities (image input, context window) from its
    // catalog entry; fall back to a minimal entry if listing fails.
    let provider: Arc<dyn ModelProvider> = Arc::new(BedrockProvider::from_env());
    let model = match StaticModel::new(provider.clone(), MODEL_ID).await {
        Ok(m) => m,
        Err(e) => {
            tracing::warn!("Failed to resolve model entry for {MODEL_ID}: {e}");
            StaticModel::from_entry(
                provider,
                &ModelEntry {
                    model_id: MODEL_ID.to_owned(),
                    display_name: MODEL_ID.to_owned(),
                    context_window: 0,
                    max_output_tokens: None,
                    supports_image_input: false,
                },
            )
        }
    };

    let system = AgentSystemBuilder::new(conversation_store, state_store, model, sender.clone())
        .tools(tool_impls)
        .callback_url(callback_url)
        .rap_notifier(rap_notifier)
        .build();

    // One SQS delivery = one slice: load the thread, run one step over the
    // batch, and let the process exit. There is nowhere to hold deferred
    // events in a Lambda, so `NoDeferral` processes everything immediately.
    let thread = system
        .thread(&group_id)
        .await
        .map_err(|e| Error::from(format!("{}", e)))?;
    let collector = EventCollector::new();
    // Keep the cancel sender alive for the step's duration (dropping it
    // signals cancellation). Lambda never interrupts a slice.
    let (_cancel_tx, cancel_rx) = tokio::sync::oneshot::channel();
    thread
        .step(inputs, &collector, &mut NoDeferral, cancel_rx)
        .await
        .map_err(|e| Error::from(format!("{}", e)))?;

    // Transform the step's events into output-queue messages.
    let mut accumulated_text = String::new();
    let mut oauth_auth_url: Option<String> = None;

    for event in collector.take() {
        match event {
            AgentEvent::TextChunk { text } => {
                accumulated_text.push_str(&text);
            }
            AgentEvent::ToolCall { name, args, .. } if name != "sleep_until_event_or_input" => {
                accumulated_text.push_str(&format!(
                    "\n[Tool Call: {} with arguments {}]\n",
                    name, args
                ));
            }
            AgentEvent::OAuthRequired { auth_url } => {
                oauth_auth_url = Some(auth_url);
            }
            _ => {}
        }
    }

    // Send OAuth output if needed
    if let Some(auth_url) = oauth_auth_url {
        let metadata = thread
            .history()
            .get_metadata()
            .unwrap_or(serde_json::json!({}));
        let oauth_msg = event_processor::OAuthOutputMessage {
            message_type: "oauth_required".to_owned(),
            auth_url,
            metadata,
        };
        sender
            .send_to_output(&serde_json::to_string(&oauth_msg)?)
            .await
            .map_err(|e| Error::from(format!("{}", e)))?;
    }

    // Send accumulated text to output queue
    if !accumulated_text.is_empty() {
        let metadata = thread
            .history()
            .get_metadata()
            .unwrap_or(serde_json::json!({}));
        let thread_id = thread.history().thread_id.clone();
        let root_id = thread.history().root_thread_id.clone();
        let output_text = if thread_id != root_id {
            format!("[{}] {}", thread_id, accumulated_text)
        } else {
            accumulated_text
        };
        let output_msg = event_processor::OutputMessage {
            text: output_text,
            metadata,
        };
        sender
            .send_to_output(&serde_json::to_string(&output_msg)?)
            .await
            .map_err(|e| Error::from(format!("{}", e)))?;
    }

    Ok(())
}
