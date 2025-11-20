use aws_lambda_events::event::sqs::SqsEvent;
use aws_sdk_dynamodb::{Client as DynamoDbClient, types::AttributeValue};
use aws_sdk_scheduler::Client as SchedulerClient;
use aws_sdk_sqs::{Client as SqsClient};
use lambda_runtime::{Error, LambdaEvent, tracing};
use rig_bedrock::client::Client;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

use crate::tools::{Tool, ToolContext};
use crate::tools::sleep::SleepTool;
use crate::tools::lambda_tool::LambdaTool;

use futures_util::StreamExt;
use rig::{
    OneOrMany,
    client::{CompletionClient, ProviderClient},
    completion::{CompletionModel, CompletionRequest, ToolDefinition},
    message::{Message, UserContent},
    streaming::StreamedAssistantContent,
};

#[derive(Debug, Deserialize, Serialize)]
pub struct InputMessage {
    pub content: UserContent,
    pub group_id: String,
    #[serde(default)]
    pub metadata: Option<serde_json::Value>,
}

#[derive(Debug, Serialize)]
struct OutputMessage {
    text: String,
    metadata: serde_json::Value,
}

struct HistoryManager {
    dynamodb_client: DynamoDbClient,
    table_name: String,
    group_id: String,
    history: Vec<Message>,
    processed_message_ids: std::collections::HashSet<String>,
    metadata: Option<serde_json::Value>,
}

impl HistoryManager {
    async fn new_with_history(
        dynamodb_client: DynamoDbClient,
        table_name: String,
        group_id: String,
    ) -> Result<Self, Error> {
        // Load existing history from DynamoDB
        let result = dynamodb_client
            .get_item()
            .table_name(&table_name)
            .key("session", AttributeValue::S(group_id.clone()))
            .send()
            .await;

        let (history, processed_message_ids, metadata) = match result {
            Ok(output) => {
                if let Some(item) = output.item {
                    let history = if let Some(AttributeValue::L(messages)) = item.get("history") {
                        messages
                            .iter()
                            .filter_map(|attr| {
                                if let AttributeValue::S(json_str) = attr {
                                    serde_json::from_str::<Message>(json_str).ok()
                                } else {
                                    None
                                }
                            })
                            .collect()
                    } else {
                        vec![]
                    };

                    let processed_ids =
                        if let Some(AttributeValue::Ss(ids)) = item.get("processed_message_ids") {
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

                    (history, processed_ids, metadata)
                } else {
                    (vec![], HashSet::new(), None)
                }
            }
            Err(_) => (vec![], HashSet::new(), None),
        };

        Ok(Self {
            dynamodb_client,
            table_name,
            group_id,
            history,
            processed_message_ids,
            metadata,
        })
    }

    async fn handle_user_content(
        &mut self,
        content: UserContent,
        message_id: String,
    ) -> Result<(), Error> {
        // Check if we've already processed this message
        if self.processed_message_ids.contains(&message_id) {
            tracing::info!("Message {} already processed, skipping", message_id);
            return Ok(());
        }

        let message = Message::User {
            content: OneOrMany::one(content),
        };

        self.history.push(message.clone());
        self.append_to_dynamodb(message, message_id.clone()).await?;
        self.processed_message_ids.insert(message_id);
        Ok(())
    }

    async fn handle_completion<R>(
        &mut self,
        completion: &StreamedAssistantContent<R>,
        completion_id: String,
    ) -> Result<(), Error> {
        // Check if we've already processed this message
        if self.processed_message_ids.contains(&completion_id) {
            tracing::info!("Completion {} already processed, skipping", completion_id);
            return Ok(());
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
                return Ok(());
            }
            StreamedAssistantContent::Final(_) => {
                return Ok(());
            }
        };

        self.history.push(message.clone());
        self.append_to_dynamodb(message, completion_id).await?;
        Ok(())
    }

    async fn append_to_dynamodb(&self, message: Message, message_id: String) -> Result<(), Error> {
        let message_json = serde_json::to_string(&message)?;

        // Build the update expression based on whether metadata exists
        let (update_expr, update_builder) = if self.metadata.is_some() {
            let metadata_json = serde_json::to_string(&self.metadata)?;
            let expr = "SET history = list_append(if_not_exists(history, :empty_list), :new_message), metadata = :metadata ADD processed_message_ids :message_id";
            let builder = self
                .dynamodb_client
                .update_item()
                .table_name(&self.table_name)
                .key("session", AttributeValue::S(self.group_id.clone()))
                .expression_attribute_values(
                    ":new_message",
                    AttributeValue::L(vec![AttributeValue::S(message_json)]),
                )
                .expression_attribute_values(":empty_list", AttributeValue::L(vec![]))
                .expression_attribute_values(":message_id", AttributeValue::Ss(vec![message_id]))
                .expression_attribute_values(":metadata", AttributeValue::S(metadata_json));
            (expr, builder)
        } else {
            let expr = "SET history = list_append(if_not_exists(history, :empty_list), :new_message) ADD processed_message_ids :message_id";
            let builder = self
                .dynamodb_client
                .update_item()
                .table_name(&self.table_name)
                .key("session", AttributeValue::S(self.group_id.clone()))
                .expression_attribute_values(
                    ":new_message",
                    AttributeValue::L(vec![AttributeValue::S(message_json)]),
                )
                .expression_attribute_values(":empty_list", AttributeValue::L(vec![]))
                .expression_attribute_values(":message_id", AttributeValue::Ss(vec![message_id]));
            (expr, builder)
        };

        update_builder.update_expression(update_expr).send().await?;

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
    let sqs_client = SqsClient::new(&config);
    let scheduler_client = SchedulerClient::new(&config);
    let table_name = "AgentZeroState".to_string();
    let output_queue_url = std::env::var("OUTPUT_QUEUE_URL").unwrap_or_else(|_| "".to_string());
    let scheduler_role_arn = std::env::var("SCHEDULER_ROLE_ARN").unwrap_or_else(|_| "".to_string());

    // Register tools
    let tool_impls: Vec<Box<dyn Tool>> = vec![
        Box::new(SleepTool {
            scheduler_client: scheduler_client.clone(),
            scheduler_role_arn: scheduler_role_arn.clone(),
        }),
        Box::new(LambdaTool {
            name: "get_time".to_string(),
            description: "Get the current time in a specified timezone or UTC.".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "timezone": {
                        "type": "string",
                        "description": "IANA timezone name (e.g., 'America/New_York', 'Europe/London'). Defaults to UTC if not specified."
                    }
                },
                "required": []
            }),
            queue_url: std::env::var("GET_TIME_TOOL_QUEUE_URL").unwrap_or_default(),
        }),
    ];

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

        // Parse the input message to extract metadata
        let input_msg: InputMessage = serde_json::from_str(&body)?;

        let mut current_history =
            HistoryManager::new_with_history(dynamodb_client.clone(), table_name.clone(), input_msg.group_id.clone())
                .await?;

        // Update metadata if provided (for first message in conversation)
        if let Some(metadata) = input_msg.metadata {
            current_history.update_metadata(metadata).await?;
        }

        current_history
            .handle_user_content(input_msg.content, message_id.clone())
            .await?;

        // Create tool context for execution
        let tool_context = ToolContext {
            sqs_client: sqs_client.clone(),
            group_id: input_msg.group_id.clone(),
            input_queue_url: std::env::var("INPUT_QUEUE_URL").unwrap_or_default(),
            input_queue_arn: std::env::var("INPUT_QUEUE_ARN").unwrap_or_default(),
        };

        let mut completion_result = model
            .stream(CompletionRequest {
                preamble: None,
                chat_history: current_history.get_history(),
                documents: vec![],
                tools: tools.clone(),
                temperature: None,
                max_tokens: None,
                tool_choice: None,
                additional_params: None,
            })
            .await
            .unwrap();

        let mut completion_counter = 0;
        let mut accumulated_text = String::new();

        while let Some(Ok(chunk)) = completion_result.next().await {
            // Generate a unique ID for each completion chunk
            let completion_id = format!("{}-{}-completion-{}", input_msg.group_id, message_id, completion_counter);
            completion_counter += 1;

            current_history
                .handle_completion(&chunk, completion_id)
                .await?;

            match chunk {
                StreamedAssistantContent::Text(text) => {
                    tracing::info!("{}", text.text);
                    accumulated_text.push_str(&text.text);
                }
                StreamedAssistantContent::ToolCall(call) => {
                    tracing::info!(
                        "\n[Tool Call: {} with arguments {}]\n",
                        &call.function.name,
                        &call.function.arguments
                    );

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

        tracing::info!("Finished streaming assistant content");
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
