use aws_lambda_events::event::sqs::SqsEvent;
use aws_sdk_dynamodb::{Client as DynamoDbClient, types::AttributeValue};
use aws_sdk_dsql::Client as DsqlClient;
use aws_sdk_scheduler::Client as SchedulerClient;
use aws_sdk_sqs::Client as SqsClient;
use lambda_runtime::{Error, LambdaEvent, tracing};
use rig_bedrock::client::Client;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

use crate::conversation_history::ConversationHistoryStore;

use crate::tools::config::ToolsConfig;
use crate::tools::sleep::{SleepTool, SleepUntilEventOrInputTool};
use crate::tools::{Tool, ToolContext, ToolSet, VecToolSet};

use futures_util::StreamExt;
use rig::{
    OneOrMany,
    client::{CompletionClient, ProviderClient},
    completion::{CompletionModel, CompletionRequest, ToolDefinition},
    message::{Message, UserContent},
    streaming::StreamedAssistantContent,
};

#[derive(Debug, Deserialize, Serialize)]
#[serde(untagged)]
pub enum InputMessageContent {
    OAuth(OAuthRequired),
    User(UserContent),
}

#[derive(Debug, Deserialize, Serialize)]
pub struct OAuthRequired {
    #[serde(rename = "type")]
    pub content_type: String, // "oauth_required"
    pub id: String,
    pub call_id: Option<String>,
    pub auth_url: String,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct InputMessage {
    pub content: InputMessageContent,
    pub group_id: String,
    #[serde(default)]
    pub metadata: Option<serde_json::Value>,
    #[serde(default)]
    pub synthetic: Option<String>,
}

#[derive(Debug, Serialize)]
struct OutputMessage {
    text: String,
    metadata: serde_json::Value,
}

#[derive(Debug, Serialize)]
struct OAuthOutputMessage {
    #[serde(rename = "type")]
    message_type: String,
    auth_url: String,
    metadata: serde_json::Value,
}

struct PendingItem {
    message: Message,
    message_id: String,
}

struct HistoryManager {
    dynamodb_client: DynamoDbClient,
    conversation_store: ConversationHistoryStore,
    table_name: String,
    group_id: String,
    history: Vec<Message>,
    processed_message_ids: HashSet<String>,
    processed_tool_calls: HashSet<String>,
    metadata: Option<serde_json::Value>,
    pending_items: Vec<PendingItem>,
    pending_complete_tool_calls: HashSet<String>,
}

impl HistoryManager {
    async fn new_with_history(
        dynamodb_client: DynamoDbClient,
        conversation_store: ConversationHistoryStore,
        table_name: String,
        group_id: String,
    ) -> Result<Self, Error> {
        // Load conversation history from DSQL
        let history = conversation_store.load_history(&group_id).await?;

        // Load metadata and processed IDs from DynamoDB
        let result = dynamodb_client
            .get_item()
            .table_name(&table_name)
            .key("session", AttributeValue::S(group_id.clone()))
            .send()
            .await;

        let (processed_message_ids, processed_tool_calls, metadata) = match result {
            Ok(output) => {
                if let Some(item) = output.item {
                    let processed_ids =
                        if let Some(AttributeValue::Ss(ids)) = item.get("processed_message_ids") {
                            ids.iter().cloned().collect()
                        } else {
                            HashSet::new()
                        };

                    let processed_tools =
                        if let Some(AttributeValue::Ss(ids)) = item.get("processed_tool_calls") {
                            ids.iter().cloned().collect()
                        } else {
                            HashSet::new()
                        };

                    let metadata =
                        if let Some(AttributeValue::S(metadata_str)) = item.get("metadata") {
                            serde_json::from_str(metadata_str).ok()
                        } else {
                            None
                        };

                    (processed_ids, processed_tools, metadata)
                } else {
                    (HashSet::new(), HashSet::new(), None)
                }
            }
            Err(_) => (HashSet::new(), HashSet::new(), None),
        };

        Ok(Self {
            dynamodb_client,
            conversation_store,
            table_name,
            group_id,
            history,
            processed_message_ids,
            processed_tool_calls,
            metadata,
            pending_items: Vec::new(),
            pending_complete_tool_calls: HashSet::new(),
        })
    }

    async fn handle_content(
        &mut self,
        message: Message,
        message_id: String,
    ) -> Result<bool, Error> {
        // Check if we've already processed this message
        if self.processed_message_ids.contains(&message_id) {
            tracing::info!("Message {} already processed, skipping", message_id);
            return Ok(false);
        }

        if let Message::User { content } = &message
            && let UserContent::ToolResult(ref tool_result) = content.first()
        {
            // Check if we've already processed this tool call
            if self.processed_tool_calls.contains(tool_result.id.as_str()) {
                tracing::info!(
                    "Tool call {} already processed, ignoring duplicate result",
                    tool_result.id
                );
                // Still mark the message as processed to avoid reprocessing
                self.processed_message_ids.insert(message_id.clone());
                self.update_processed_message_id(message_id).await?;
                return Ok(false);
            } else {
                tracing::info!("Handling tool call result {}", tool_result.id);
            }
        } else {
            if let Some(Message::Assistant { content, .. }) = self.history.last() {
                if let rig::message::AssistantContent::ToolCall(tool_call) = content.first() {
                    if !self.processed_tool_calls.contains(tool_call.id.as_str()) {
                        tracing::info!(
                            "Tool call {} interrupted by new user message",
                            tool_call.id
                        );

                        let synthetic_result = Message::User {
                            content: OneOrMany::one(UserContent::ToolResult(
                                rig::message::ToolResult {
                                    id: tool_call.id.clone(),
                                    call_id: tool_call.call_id.clone(),
                                    content: OneOrMany::one(rig::message::ToolResultContent::Text(
                                        rig::agent::Text {
                                            text: "Tool call interrupted by user".to_string(),
                                        },
                                    )),
                                },
                            )),
                        };

                        self.history.push(synthetic_result.clone());
                        self.append_pending(
                            synthetic_result,
                            format!("{}-interrupted", tool_call.id),
                        );
                        self.mark_tool_call_complete(tool_call.id.clone());
                    }
                }
            }
        }

        self.history.push(message.clone());
        self.append_pending(message, message_id.clone());
        self.processed_message_ids.insert(message_id);
        Ok(true)
    }

    fn handle_completion<R>(
        &mut self,
        completion: &StreamedAssistantContent<R>,
        completion_id: String,
    ) {
        // Check if we've already processed this message
        if self.processed_message_ids.contains(&completion_id) {
            tracing::info!("Completion {} already processed, skipping", completion_id);
            return;
        }

        let message = match completion {
            StreamedAssistantContent::Text(text) => Message::Assistant {
                id: None,
                content: OneOrMany::one(rig::message::AssistantContent::Text(text.clone())),
            },
            StreamedAssistantContent::Reasoning(reasoning) => Message::Assistant {
                id: None,
                content: OneOrMany::one(rig::message::AssistantContent::Reasoning(
                    reasoning.clone(),
                )),
            },
            StreamedAssistantContent::ToolCall(call) => Message::Assistant {
                id: None,
                content: OneOrMany::one(rig::message::AssistantContent::ToolCall(call.clone())),
            },
            StreamedAssistantContent::ToolCallDelta { .. } => {
                return;
            }
            StreamedAssistantContent::Final(_) => {
                return;
            }
        };

        self.history.push(message.clone());
        self.append_pending(message, completion_id);
    }

    fn append_pending(&mut self, message: Message, message_id: String) {
        // If this is a tool result, mark the tool call as complete
        if let Message::User { content } = &message {
            if let UserContent::ToolResult(result) = content.first() {
                self.mark_tool_call_complete(result.id.clone());
            }
        }

        self.pending_items.push(PendingItem {
            message,
            message_id,
        });
    }

    fn mark_tool_call_complete(&mut self, call_id: String) {
        self.processed_tool_calls.insert(call_id.clone());
        self.pending_complete_tool_calls.insert(call_id);
    }

    async fn sync(&mut self) -> Result<(), Error> {
        if self.pending_items.is_empty() && self.pending_complete_tool_calls.is_empty() {
            return Ok(());
        }

        // Store conversation messages in DSQL
        if !self.pending_items.is_empty() {
            let messages_for_dsql: Vec<(Message, String)> = self
                .pending_items
                .iter()
                .map(|item| (item.message.clone(), item.message_id.clone()))
                .collect();

            self.conversation_store
                .append_messages(&self.group_id, messages_for_dsql)
                .await?;
        }

        // Store metadata and processed IDs in DynamoDB
        let message_ids: Vec<String> = self
            .pending_items
            .iter()
            .map(|item| item.message_id.clone())
            .collect();

        let tool_call_ids: Vec<String> = self.pending_complete_tool_calls.iter().cloned().collect();

        // Build the update expression for DynamoDB (only metadata and processed IDs)
        let mut add_parts = Vec::new();
        let mut builder = self
            .dynamodb_client
            .update_item()
            .table_name(&self.table_name)
            .key("session", AttributeValue::S(self.group_id.clone()));

        if !message_ids.is_empty() {
            add_parts.push("processed_message_ids :message_ids");
            builder = builder
                .expression_attribute_values(":message_ids", AttributeValue::Ss(message_ids));
        }

        if !tool_call_ids.is_empty() {
            add_parts.push("processed_tool_calls :tool_call_ids");
            builder = builder
                .expression_attribute_values(":tool_call_ids", AttributeValue::Ss(tool_call_ids));
        }

        if !add_parts.is_empty() {
            let update_expr = format!("ADD {}", add_parts.join(", "));
            builder.update_expression(update_expr).send().await?;
        }

        // Clear pending items
        self.pending_items.clear();
        self.pending_complete_tool_calls.clear();

        Ok(())
    }

    async fn update_processed_message_id(&self, message_id: String) -> Result<(), Error> {
        self.dynamodb_client
            .update_item()
            .table_name(&self.table_name)
            .key("session", AttributeValue::S(self.group_id.clone()))
            .update_expression("ADD processed_message_ids :message_id")
            .expression_attribute_values(":message_id", AttributeValue::Ss(vec![message_id]))
            .send()
            .await?;

        Ok(())
    }

    async fn update_metadata(&mut self, metadata: serde_json::Value) -> Result<(), Error> {
        self.metadata = Some(metadata.clone());

        let metadata_json = serde_json::to_string(&metadata)?;
        self.dynamodb_client
            .update_item()
            .table_name(&self.table_name)
            .key("session", AttributeValue::S(self.group_id.clone()))
            .update_expression("SET metadata = :metadata")
            .expression_attribute_values(":metadata", AttributeValue::S(metadata_json))
            .send()
            .await?;

        Ok(())
    }

    fn get_metadata(&self) -> Option<serde_json::Value> {
        self.metadata.clone()
    }

    fn get_history(&self) -> OneOrMany<Message> {
        OneOrMany::many(self.history.clone()).unwrap()
    }
}

/// This is the main body for the function.
/// Write your code inside it.
/// There are some code example in the following URLs:
/// - https://github.com/awslabs/aws-lambda-rust-runtime/tree/main/examples
/// - https://github.com/aws-samples/serverless-rust-demo/
pub(crate) async fn function_handler(event: LambdaEvent<SqsEvent>) -> Result<(), Error> {
    // Extract some useful information from the request
    let payload = event.payload;
    tracing::info!("Payload: {:?}", payload);

    // Initialize AWS clients
    let config = aws_config::load_from_env().await;
    let dynamodb_client = DynamoDbClient::new(&config);
    let dsql_client = DsqlClient::new(&config);
    let sqs_client = SqsClient::new(&config);
    let scheduler_client = SchedulerClient::new(&config);
    let table_name = "InfinityAgentsState".to_string();
    let output_queue_url = std::env::var("OUTPUT_QUEUE_URL").unwrap_or_else(|_| "".to_string());
    let scheduler_role_arn = std::env::var("SCHEDULER_ROLE_ARN").unwrap_or_else(|_| "".to_string());
    let dsql_cluster_endpoint = std::env::var("DSQL_CLUSTER_ENDPOINT")
        .map_err(|_| Error::from("DSQL_CLUSTER_ENDPOINT environment variable is required"))?;

    // Initialize conversation history store
    let conversation_store = ConversationHistoryStore::new(&dsql_client, &dsql_cluster_endpoint).await?;

    // Load tool sets from configuration (SSM preferred, then file, then env var)
    let mut tool_sets: Vec<Box<dyn ToolSet>> = if let Ok(ssm_param) =
        std::env::var("TOOLS_CONFIG_SSM_PARAM")
    {
        let ssm_client = aws_sdk_ssm::Client::new(&config);
        match ToolsConfig::from_ssm(&ssm_client, &ssm_param).await {
            Ok(config) => {
                tracing::info!(
                    "Loaded tools configuration from SSM parameter {}",
                    ssm_param
                );
                config.into_tool_sets()
            }
            Err(e) => {
                tracing::warn!(
                    "Failed to load tools config from SSM: {}. Using empty tool set.",
                    e
                );
                vec![]
            }
        }
    } else {
        let config_path =
            std::env::var("TOOLS_CONFIG_PATH").unwrap_or_else(|_| "tools.json".to_string());
        match ToolsConfig::from_file(&config_path) {
            Ok(config) => {
                tracing::info!("Loaded tools configuration from {}", config_path);
                config.into_tool_sets()
            }
            Err(_) => {
                // Fallback to env var for backwards compatibility
                match ToolsConfig::from_env() {
                    Ok(config) => {
                        tracing::info!(
                            "Loaded tools configuration from TOOLS_CONFIG environment variable"
                        );
                        config.into_tool_sets()
                    }
                    Err(e) => {
                        tracing::warn!("Failed to load tools config: {}. Using empty tool set.", e);
                        vec![]
                    }
                }
            }
        }
    };

    // Add hardcoded sleep tool set
    tool_sets.insert(
        0,
        Box::new(VecToolSet::new(vec![
            Box::new(SleepTool {
                scheduler_client: scheduler_client.clone(),
                scheduler_role_arn: scheduler_role_arn.clone(),
            }),
            Box::new(SleepUntilEventOrInputTool),
        ])),
    );

    // Concatenate all tools from tool sets
    let mut tool_impls: Vec<Box<dyn Tool>> = Vec::new();
    for tool_set in tool_sets {
        tool_impls.extend(tool_set.into_tools());
    }

    let tool_registry: HashMap<String, &Box<dyn Tool>> = tool_impls
        .iter()
        .map(|tool| (tool.name().to_string(), tool))
        .collect();

    let tools: Vec<ToolDefinition> = tool_impls
        .iter()
        .map(|tool| ToolDefinition {
            name: tool.name().to_string(),
            description: tool.description().to_string(),
            parameters: tool.parameters(),
        })
        .collect();

    let client = Client::from_env();
    let model = client.completion_model("global.anthropic.claude-haiku-4-5-20251001-v1:0");

    for record in payload.records {
        let message_id = record.message_id.unwrap_or_default();
        let body = record.body.unwrap();

        let input_msg: InputMessage = serde_json::from_str(&body)?;

        let mut current_history = HistoryManager::new_with_history(
            dynamodb_client.clone(),
            conversation_store.clone(),
            table_name.clone(),
            input_msg.group_id.clone(),
        )
        .await?;

        // Update metadata if provided (for first message in conversation)
        if let Some(metadata) = input_msg.metadata {
            current_history.update_metadata(metadata).await?;
        }

        // Handle OAuth required messages - forward to output queue without adding to history
        if let InputMessageContent::OAuth(oauth) = &input_msg.content {
            if oauth.content_type == "oauth_required" {
                tracing::info!("Received OAuth required message, forwarding to output queue");

                let metadata = current_history
                    .get_metadata()
                    .unwrap_or(serde_json::json!({}));

                let oauth_msg = OAuthOutputMessage {
                    message_type: "oauth_required".to_string(),
                    auth_url: oauth.auth_url.clone(),
                    metadata,
                };

                if !output_queue_url.is_empty() {
                    sqs_client
                        .send_message()
                        .queue_url(&output_queue_url)
                        .message_body(serde_json::to_string(&oauth_msg)?)
                        .send()
                        .await?;
                    tracing::info!("Sent OAuth URL to output queue");
                }
                continue;
            }
        }

        // Extract UserContent from the message
        let user_content = match input_msg.content {
            InputMessageContent::User(content) => content,
            InputMessageContent::OAuth(_) => continue, // Already handled above
        };

        // Handle synthetic tool results
        let content = if let Some(original_tool_call_id) = input_msg.synthetic {
            tracing::info!(
                "Processing synthetic tool result for tool call: {}",
                original_tool_call_id
            );

            // Look up the original tool call from conversation history
            if let Some(original_call) = current_history.history.iter().find_map(|msg| {
                if let Message::Assistant { content, .. } = msg {
                    content.iter().find_map(|c| {
                        if let rig::message::AssistantContent::ToolCall(call) = c {
                            if call.id == original_tool_call_id {
                                Some(call.clone())
                            } else {
                                None
                            }
                        } else {
                            None
                        }
                    })
                } else {
                    None
                }
            }) {
                // Generate a new random tool call ID
                let new_tool_call_id = uuid::Uuid::new_v4().to_string();

                // Extract the tool result content
                if let UserContent::ToolResult(mut tool_result) = user_content {
                    // Create a synthetic tool call with kind: "interrupt" added to original arguments
                    let mut synthetic_args = original_call.function.arguments.clone();
                    synthetic_args.as_object_mut().unwrap().insert(
                        "kind".to_string(),
                        serde_json::json!(format!(
                            "interrupt:{} (subscription remains active)",
                            original_tool_call_id
                        )),
                    );

                    let synthetic_tool_call = Message::Assistant {
                        id: None,
                        content: OneOrMany::one(rig::message::AssistantContent::ToolCall(
                            rig::message::ToolCall {
                                id: new_tool_call_id.clone(),
                                call_id: None,
                                function: rig::message::ToolFunction {
                                    name: original_call.function.name.clone(),
                                    arguments: synthetic_args,
                                },
                            },
                        )),
                    };

                    // Insert the synthetic tool call into history
                    current_history
                        .handle_content(
                            synthetic_tool_call,
                            format!("{}-synthetic-call", new_tool_call_id),
                        )
                        .await?;

                    // Update the tool result with the new generated ID
                    tool_result.id = new_tool_call_id;
                    UserContent::ToolResult(tool_result)
                } else {
                    panic!("Synthetic message is not a tool result")
                }
            } else {
                tracing::warn!(
                    "Could not find original tool call for synthetic message: {}, dropping message",
                    original_tool_call_id
                );

                continue;
            }
        } else {
            user_content
        };

        let is_new = current_history
            .handle_content(
                Message::User {
                    content: OneOrMany::one(content),
                },
                message_id.clone(),
            )
            .await?;

        if !is_new {
            tracing::info!("Message was duplicate or ignored, skipping agent processing");
            continue;
        }

        // Extract user_id from metadata (stored in history)
        let user_id = current_history
            .get_metadata()
            .and_then(|m| m.get("user_id").and_then(|v| v.as_str()).map(String::from));

        let tool_context = ToolContext {
            sqs_client: sqs_client.clone(),
            group_id: input_msg.group_id.clone(),
            input_queue_url: std::env::var("INPUT_QUEUE_URL").unwrap_or_default(),
            input_queue_arn: std::env::var("INPUT_QUEUE_ARN").unwrap_or_default(),
            user_id,
        };

        tracing::info!("History: {:?}", current_history.get_history());

        let mut completion_result = model
            .stream(CompletionRequest {
                preamble: None,
                chat_history: current_history.get_history(),
                documents: vec![],
                tools: tools.clone(),
                temperature: None,
                max_tokens: None,
                tool_choice: None,
                additional_params: Some(serde_json::json!({
                    "anthropic_beta": ["interleaved-thinking-2025-05-14"],
                    "thinking": {
                        "type": "enabled",
                        "budget_tokens": 4096,
                    }
                })),
            })
            .await
            .unwrap();

        let mut completion_counter = 0;
        let mut accumulated_text = String::new();

        while let Some(res) = completion_result.next().await {
            let chunk = res?;

            if let StreamedAssistantContent::Reasoning(ref r) = chunk
                && r.signature.is_none()
            {
                continue; // incomplete reasoning
            }

            // Generate a unique ID for each completion chunk
            let completion_id = format!(
                "{}-{}-completion-{}",
                input_msg.group_id, message_id, completion_counter
            );
            completion_counter += 1;

            current_history.handle_completion(&chunk, completion_id);

            match chunk {
                StreamedAssistantContent::Text(text) => {
                    accumulated_text.push_str(&text.text);
                }
                StreamedAssistantContent::ToolCall(call) => {
                    tracing::info!(
                        "\n[Tool Call: {} with arguments {}]\n",
                        &call.function.name,
                        &call.function.arguments
                    );

                    if call.function.name != "sleep_until_event_or_input" {
                        accumulated_text.push_str(&format!(
                            "\n[Tool Call: {} with arguments {}]\n",
                            &call.function.name, &call.function.arguments
                        ));
                    }

                    // Sync pending items to DynamoDB before executing tool
                    current_history.sync().await?;

                    // Look up and execute tool
                    if let Some(tool) = tool_registry.get(&call.function.name) {
                        tool.execute(
                            call.function.arguments,
                            call.id,
                            call.call_id,
                            &tool_context,
                        )
                        .await?
                    } else {
                        tracing::warn!("Unknown tool called: {}", call.function.name);
                    }

                    break;
                }
                StreamedAssistantContent::ToolCallDelta { .. } => {}
                StreamedAssistantContent::Reasoning(reasoning) => {
                    tracing::info!("\n[Reasoning: {:?}]\n", reasoning.reasoning);
                }
                StreamedAssistantContent::Final(_) => {}
            };
        }

        // Sync any remaining pending items to DynamoDB after the loop
        current_history.sync().await?;

        tracing::info!(
            "Sending accumulated response to output queue {} {}",
            &accumulated_text,
            &output_queue_url
        );

        // Send accumulated response to output queue
        if !accumulated_text.is_empty() && !output_queue_url.is_empty() {
            // Retrieve metadata from DynamoDB (stored from previous messages)
            let metadata = current_history
                .get_metadata()
                .unwrap_or(serde_json::json!({}));

            let output_msg = OutputMessage {
                text: accumulated_text,
                metadata,
            };

            sqs_client
                .send_message()
                .queue_url(&output_queue_url)
                .message_body(serde_json::to_string(&output_msg)?)
                .send()
                .await?;

            tracing::info!("Sent response to output queue");
        } else {
            tracing::info!("Output was empty");
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use lambda_runtime::{Context, LambdaEvent};

    #[tokio::test]
    async fn test_event_handler() {
        let event = LambdaEvent::new(SqsEvent::default(), Context::default());
        let response = function_handler(event).await.unwrap();
        assert_eq!((), response);
    }
}
