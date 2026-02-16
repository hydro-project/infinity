use aws_lambda_events::event::sqs::SqsEvent;
use aws_sdk_dsql::Client as DsqlClient;
use aws_sdk_dynamodb::Client as DynamoDbClient;
use aws_sdk_scheduler::Client as SchedulerClient;
use aws_sdk_sqs::Client as SqsClient;
use lambda_runtime::{Error, LambdaEvent, tracing};
use rig_bedrock::client::Client;

use infinity_agent_core::event_processor;
use infinity_agent_core::message::InputMessage;
use infinity_agent_core::tools::config::ToolsConfig;
use infinity_agent_core::tools::sleep::SleepUntilEventOrInputTool;
use infinity_agent_core::tools::thread::{CloseThreadTool, ReportToParentTool, SpawnThreadTool};
use infinity_agent_core::tools::toolset_loader::ToolsetLoader;
use infinity_agent_core::tools::{Tool, ToolContext};

use rig::client::{CompletionClient, ProviderClient};

use crate::conversation_history::DsqlConversationStore;
use crate::state_store::DynamoDbStateStore;
use crate::tools::lambda_tool::LambdaTool;
use crate::tools::rap_http::RapHttpClient;
use crate::tools::sleep::{SleepTool, SleepUntilTool};
use crate::tools::sqs_sender::SqsMessageSender;
use crate::tools::toolset_cache::DynamoDbToolsetCache;

pub(crate) async fn function_handler(event: LambdaEvent<SqsEvent>) -> Result<(), Error> {
    let payload = event.payload;
    tracing::info!("Payload: {:?}", payload);

    let config = aws_config::load_from_env().await;
    let dynamodb_client = DynamoDbClient::new(&config);
    let dsql_client = DsqlClient::new(&config);
    let sqs_client = SqsClient::new(&config);
    let scheduler_client = SchedulerClient::new(&config);
    let table_name = "InfinityAgentsState".to_string();
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
            &dynamodb_client
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
                .unwrap_or(&"{}".to_string()),
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
    } else if let Ok(ssm_param) = std::env::var("TOOLS_CONFIG_SSM_PARAM") {
        let ssm_client = aws_sdk_ssm::Client::new(&config);
        match ssm_client.get_parameter().name(&ssm_param).send().await {
            Ok(resp) => {
                let value = resp.parameter().and_then(|p| p.value()).unwrap_or("{}");
                ToolsConfig::from_json(value).ok()
            }
            Err(e) => {
                tracing::warn!("Failed to load tools config from SSM: {}", e);
                None
            }
        }
    } else {
        let config_path =
            std::env::var("TOOLS_CONFIG_PATH").unwrap_or_else(|_| "tools.json".to_string());
        ToolsConfig::from_file(&config_path)
            .ok()
            .or_else(|| ToolsConfig::from_env().ok())
    };

    let toolset_server_urls: Vec<String> = tools_config
        .as_ref()
        .map(|tc| tc.toolset_server_urls())
        .unwrap_or_default();

    let http_client = RapHttpClient::new(&config);
    let toolset_cache = DynamoDbToolsetCache::new(dynamodb_client.clone(), table_name.clone());
    let toolset_loader = ToolsetLoader::new(http_client.clone(), toolset_cache);

    let client = Client::from_env();
    let model = client.completion_model("global.anthropic.claude-haiku-4-5-20251001-v1:0");

    let input_queue_url = std::env::var("INPUT_QUEUE_URL").unwrap_or_default();
    let input_queue_arn = std::env::var("INPUT_QUEUE_ARN").unwrap_or_default();
    let rap_receiver_url = std::env::var("RAP_RECEIVER_URL").unwrap_or_default();

    let sender = SqsMessageSender {
        sqs_client: sqs_client.clone(),
        input_queue_url: input_queue_url.clone(),
        output_queue_url: output_queue_url.clone(),
    };

    for record in payload.records {
        let message_id = record.message_id.unwrap_or_default();
        let body = record.body.unwrap();
        let input_msg: InputMessage = serde_json::from_str(&body)?;

        // Build tools for this record
        let mut tool_impls: Vec<Box<dyn Tool<SqsMessageSender>>> = Vec::new();

        // Load RAP toolsets
        if !toolset_server_urls.is_empty() {
            // We need the root thread ID for session scoping — peek at conversation store
            let session_id = input_msg.group_id.clone(); // simplified; real impl uses root thread
            match toolset_loader
                .load_toolsets(&toolset_server_urls, &session_id)
                .await
            {
                Ok(loaded) => {
                    for ts in loaded {
                        let endpoint = ts.manifest.endpoint.clone();
                        for def in ts.manifest.tools {
                            tool_impls.push(Box::new(LambdaTool {
                                name: def.name,
                                description: def.description,
                                parameters: def.input_schema,
                                function_url: endpoint.clone(),
                                http_client: http_client.clone(),
                            }));
                        }
                    }
                }
                Err(e) => tracing::warn!("Failed to load RAP toolsets: {}", e),
            }
        }

        // Add built-in tools
        tool_impls.push(Box::new(SleepTool {
            scheduler_client: scheduler_client.clone(),
            scheduler_role_arn: scheduler_role_arn.clone(),
            delay_queue_url: std::env::var("DELAY_QUEUE_URL").unwrap_or_default(),
        }));
        tool_impls.push(Box::new(SleepUntilEventOrInputTool));
        tool_impls.push(Box::new(SleepUntilTool {
            scheduler_client: scheduler_client.clone(),
            scheduler_role_arn: scheduler_role_arn.clone(),
            delay_queue_url: std::env::var("DELAY_QUEUE_URL").unwrap_or_default(),
        }));
        tool_impls.push(Box::new(SpawnThreadTool {
            conversation_store: conversation_store.clone(),
        }));
        tool_impls.push(Box::new(ReportToParentTool {
            conversation_store: conversation_store.clone(),
        }));
        tool_impls.push(Box::new(CloseThreadTool {
            conversation_store: conversation_store.clone(),
        }));

        let sender_clone = sender.clone();
        let input_queue_arn_clone = input_queue_arn.clone();
        let rap_receiver_url_clone = rap_receiver_url.clone();

        // (a) Create history and prepare input
        let mut current_history = event_processor::HistoryManager::new_with_history(
            conversation_store.clone(),
            state_store.clone(),
            input_msg.group_id.clone(),
        )
        .await
        .map_err(|e| Error::from(format!("{}", e)))?;

        let prepare_result = event_processor::prepare_input(
            input_msg,
            message_id.clone(),
            &mut current_history,
            &conversation_store,
        )
        .await
        .map_err(|e| Error::from(format!("{}", e)))?;

        match prepare_result {
            event_processor::PrepareResult::Handled => continue,
            event_processor::PrepareResult::OAuthRequired { auth_url } => {
                let metadata = current_history
                    .get_metadata()
                    .unwrap_or(serde_json::json!({}));
                let oauth_msg = event_processor::OAuthOutputMessage {
                    message_type: "oauth_required".to_string(),
                    auth_url,
                    metadata,
                };
                sender
                    .send_to_output(&serde_json::to_string(&oauth_msg)?)
                    .await
                    .map_err(|e| Error::from(format!("{}", e)))?;
                continue;
            }
            event_processor::PrepareResult::Ready => {}
        }

        // (b) Build tool definitions and run completion
        let tool_names: std::collections::HashSet<String> =
            tool_impls.iter().map(|t| t.name().to_string()).collect();
        let tool_defs: Vec<rig::completion::ToolDefinition> = tool_impls
            .iter()
            .map(|t| rig::completion::ToolDefinition {
                name: t.name().to_string(),
                description: t.description().to_string(),
                parameters: t.parameters(),
            })
            .collect();

        let active_thread_id = current_history.thread_id.clone();

        // (b) Consume the completion stream, collecting text and the final action
        use futures_util::StreamExt;

        let (accumulated_text, final_action) = {
            let mut completion_stream = std::pin::pin!(event_processor::run_completion(
                &model,
                &mut current_history,
                &tool_names,
                &tool_defs,
                &active_thread_id,
                &message_id,
            ));

            let mut text = String::new();
            let mut action = None;

            while let Some(event) = completion_stream.next().await {
                match event.map_err(|e| Error::from(format!("{}", e)))? {
                    event_processor::CompletionEvent::TextChunk(chunk) => {
                        text.push_str(&chunk);
                    }
                    event_processor::CompletionEvent::Action(a) => {
                        action = Some(a);
                    }
                }
            }
            (text, action)
        };

        current_history.sync().await?;

        // For root threads, append tool call info to the output so the user sees it
        let mut accumulated_text = accumulated_text;
        if let Some(event_processor::CompletionAction::ExecuteToolCall {
            ref tool_name,
            ref tool_args,
            ..
        }) = final_action
        {
            if tool_name != "sleep_until_event_or_input" {
                accumulated_text.push_str(&format!(
                    "\n[Tool Call: {} with arguments {}]\n",
                    tool_name, tool_args
                ));
            }
        }

        // Send accumulated text to output queue
        if !accumulated_text.is_empty() {
            let metadata = current_history
                .get_metadata()
                .unwrap_or(serde_json::json!({}));
            let output_text = if let Some(prefix) = current_history.get_thread_nesting_prefix() {
                format!("{} {}", prefix, accumulated_text)
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

        if let Some(action) = final_action {
            let user_id = current_history
                .get_metadata()
                .and_then(|m| m.get("user_id").and_then(|v| v.as_str()).map(String::from));

            let tool_context = ToolContext {
                message_sender: sender_clone.clone(),
                group_id: active_thread_id,
                input_queue_arn: input_queue_arn_clone,
                rap_receiver_url: rap_receiver_url_clone,
                user_id,
            };

            let tool_registry: std::collections::HashMap<String, &Box<dyn Tool<SqsMessageSender>>> =
                tool_impls
                    .iter()
                    .map(|t| (t.name().to_string(), t))
                    .collect();

            event_processor::execute_action(action, &tool_registry, &tool_context)
                .await
                .map_err(|e| Error::from(format!("{}", e)))?;
        }
    }

    Ok(())
}
