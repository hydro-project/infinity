use std::collections::BTreeMap;
use std::rc::Rc;
use std::sync::Arc;

use aws_lambda_events::event::sqs::SqsEvent;
use aws_sdk_dsql::Client as DsqlClient;
use aws_sdk_dynamodb::Client as DynamoDbClient;
use aws_sdk_scheduler::Client as SchedulerClient;
use aws_sdk_sqs::Client as SqsClient;
use infinity_provider_bedrock::BedrockProvider;
use lambda_runtime::{Error, LambdaEvent, tracing};

use infinity_agent_core::ThreadId;
use infinity_agent_core::event_processor;
use infinity_agent_core::message::InputMessage;
use infinity_agent_core::system::{
    AgentEvent, AgentSystemBuilder, EventCollector, NoDeferral, StaticModel, ThreadConfig,
    ThreadConfigSource,
};
use infinity_agent_core::tools::Tool;
use infinity_agent_core::tools::config::ToolsConfig;
use infinity_agent_core::tools::rap_tool::RapTool;
use infinity_agent_core::traits::{ConversationStore, StateStore};
use infinity_provider_protocol::{ModelEntry, ModelProvider};
use rap_client::toolset_loader::ToolsetLoader;

use crate::conversation_history::DsqlConversationStore;
use crate::state_store::DynamoDbStateStore;
use crate::tools::rap_http::RapHttpClient;
use crate::tools::sleep::{SleepTool, SleepUntilTool, WakeupScheduler};
use crate::tools::sqs_sender::SqsMessageSender;
use crate::tools::toolset_cache::DynamoDbToolsetCache;

/// The model invoked for all Lambda completions (hardcoded for now).
const MODEL_ID: &str = "global.anthropic.claude-sonnet-4-6";

pub(crate) async fn function_handler(event: LambdaEvent<SqsEvent>) -> Result<(), Error> {
    let payload = event.payload;
    tracing::info!("Payload: {:?}", payload);

    let config = aws_config::load_defaults(aws_config::BehaviorVersion::latest()).await;
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

    let toolset_cache = DynamoDbToolsetCache::new(dynamodb_client, table_name);
    let toolset_loader = ToolsetLoader::new(http_client.clone(), toolset_cache);

    let input_queue_url = std::env::var("INPUT_QUEUE_URL").unwrap_or_default();
    let input_queue_arn = std::env::var("INPUT_QUEUE_ARN").unwrap_or_default();
    let callback_url = std::env::var("RAP_CALLBACK_URL")
        .or_else(|_| std::env::var("RAP_RECEIVER_URL"))
        .unwrap_or_default();

    let sender = SqsMessageSender {
        sqs_client,
        input_queue_url,
        output_queue_url,
    };

    // Parse all records in delivery order. With `batchSize: 1` (the CDK
    // default here) a batch holds one message, but SQS FIFO batches may span
    // multiple message groups when the batch size is larger; the system's
    // `step` partitions by group and runs the per-thread steps concurrently.
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

    let mut system = AgentSystemBuilder::new(
        conversation_store.clone(),
        state_store.clone(),
        model,
        sender.clone(),
    )
    .thread_config(LambdaThreadConfig {
        toolset_server_urls,
        toolset_loader,
        http_client,
        rap_notifier,
        scheduler_client,
        scheduler_role_arn,
        delay_queue_url: std::env::var("DELAY_QUEUE_URL").unwrap_or_default(),
        input_queue_arn,
    })
    .callback_url(callback_url)
    .build();

    // One SQS delivery = one slice per thread in the batch. There is nowhere
    // to hold deferred events in a Lambda, so `NoDeferral` processes
    // everything immediately.
    let collector = EventCollector::new();
    system.step(inputs, &collector, &mut NoDeferral).await?;

    // Transform each thread's events into output-queue messages.
    #[derive(Default)]
    struct ThreadOutput {
        accumulated_text: String,
        oauth_auth_url: Option<String>,
        required_choices: Vec<infinity_agent_core::system::UserChoice>,
        completed_choices: Vec<String>,
    }
    let mut per_thread: BTreeMap<ThreadId, ThreadOutput> = BTreeMap::new();
    for (thread_id, event) in collector.take() {
        let entry = per_thread.entry(thread_id).or_default();
        match event {
            AgentEvent::TextChunk { text } => {
                entry.accumulated_text.push_str(&text);
            }
            AgentEvent::ToolCall { name, args, .. } if name != "sleep_until_event_or_input" => {
                entry.accumulated_text.push_str(&format!(
                    "\n[Tool Call: {} with arguments {}]\n",
                    name, args
                ));
            }
            AgentEvent::OAuthRequired { auth_url } => {
                entry.oauth_auth_url = Some(auth_url);
            }
            AgentEvent::UserChoiceRequired { choice } => {
                entry.required_choices.push(choice);
            }
            AgentEvent::UserChoiceDismissed { choice_id } => {
                entry.completed_choices.push(choice_id);
            }
            _ => {}
        }
    }

    for (
        thread_id,
        ThreadOutput {
            accumulated_text,
            oauth_auth_url,
            required_choices,
            completed_choices,
        },
    ) in per_thread
    {
        // Resolve the thread's root and metadata for the output-queue messages.
        let root_id = conversation_store
            .get_ancestor_chain(&thread_id)
            .await?
            .first()
            .map(|(id, _)| id.clone())
            .unwrap_or_else(|| thread_id.clone());
        let metadata = state_store
            .get_metadata(&root_id)
            .await
            .ok()
            .flatten()
            .unwrap_or(serde_json::json!({}));

        // Send OAuth output if needed
        if let Some(auth_url) = oauth_auth_url {
            let oauth_msg = event_processor::OAuthOutputMessage {
                message_type: "oauth_required".to_owned(),
                auth_url,
                metadata: metadata.clone(),
            };
            sender
                .send_to_output(&serde_json::to_string(&oauth_msg)?)
                .await?;
        }

        for choice in required_choices {
            let message = event_processor::UserChoiceOutputMessage {
                message_type: "user_choice_required".to_owned(),
                id: choice.id,
                prompt: choice.prompt,
                choices: choice.choices,
                default: choice.default,
                metadata: metadata.clone(),
            };
            sender
                .send_to_output(&serde_json::to_string(&message)?)
                .await?;
        }
        for choice_id in completed_choices {
            let message = event_processor::UserChoiceCompleteOutputMessage {
                message_type: "user_choice_complete".to_owned(),
                choice_id,
                metadata: metadata.clone(),
            };
            sender
                .send_to_output(&serde_json::to_string(&message)?)
                .await?;
        }

        // Send accumulated text to output queue
        if !accumulated_text.is_empty() {
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
                .await?;
        }
    }

    Ok(())
}

/// Per-thread configuration for the Lambda system: RAP toolsets are loaded
/// (with the DynamoDB manifest cache) for each thread's session, and the
/// platform sleep tools are added alongside them.
struct LambdaThreadConfig {
    toolset_server_urls: Vec<String>,
    toolset_loader: ToolsetLoader<RapHttpClient, DynamoDbToolsetCache>,
    http_client: RapHttpClient,
    rap_notifier: rap_client::notifier::RapNotifier<RapHttpClient>,
    scheduler_client: SchedulerClient,
    scheduler_role_arn: String,
    delay_queue_url: String,
    input_queue_arn: String,
}

impl LambdaThreadConfig {
    fn wakeup_scheduler(&self) -> WakeupScheduler {
        WakeupScheduler {
            scheduler_client: self.scheduler_client.clone(),
            scheduler_role_arn: self.scheduler_role_arn.clone(),
            delay_queue_url: self.delay_queue_url.clone(),
            input_queue_arn: self.input_queue_arn.clone(),
        }
    }
}

#[async_trait::async_trait(?Send)]
impl ThreadConfigSource<SqsMessageSender, RapHttpClient> for LambdaThreadConfig {
    async fn resolve(
        &self,
        thread_id: &ThreadId,
    ) -> Result<
        ThreadConfig<SqsMessageSender, RapHttpClient>,
        Box<dyn std::error::Error + Send + Sync>,
    > {
        let mut tools: Vec<Rc<dyn Tool<SqsMessageSender>>> = Vec::new();

        // Load RAP toolsets for this thread's session.
        if !self.toolset_server_urls.is_empty() {
            match self
                .toolset_loader
                .load_toolsets(&self.toolset_server_urls, thread_id.as_str())
                .await
            {
                Ok(loaded) => {
                    for ts in loaded {
                        let endpoint = ts.manifest.endpoint.clone();
                        for def in ts.manifest.tools {
                            tools.push(Rc::new(RapTool {
                                descriptor: def.into(),
                                endpoint: endpoint.clone(),
                                http_client: self.http_client.clone(),
                                callback_url: None,
                            }));
                        }
                    }
                }
                Err(e) => tracing::warn!("Failed to load RAP toolsets: {}", e),
            }
        }

        // Platform-specific sleep tools (durable timers via EventBridge / SQS
        // delays). The remaining built-in tools come from the system builder.
        tools.push(Rc::new(SleepTool {
            scheduler: self.wakeup_scheduler(),
        }));
        tools.push(Rc::new(SleepUntilTool {
            scheduler: self.wakeup_scheduler(),
        }));

        Ok(ThreadConfig {
            tools,
            extra_system_prompt: None,
            rap_notifier: Some(self.rap_notifier.clone()),
        })
    }
}
