use async_trait::async_trait;
use aws_lambda_events::event::sqs::SqsEvent;
use aws_sdk_dynamodb::{Client as DynamoDbClient, types::AttributeValue};
use aws_sdk_scheduler::{
    Client as SchedulerClient,
    types::{FlexibleTimeWindow, FlexibleTimeWindowMode, Target},
};
use aws_sdk_sqs::{Client as SqsClient, types::MessageAttributeValue};
use chrono::{Duration, Utc};
use lambda_runtime::{Error, LambdaEvent, tracing};
use rig_bedrock::client::Client;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

use futures_util::StreamExt;
use rig::{
    OneOrMany,
    agent::Text,
    client::{CompletionClient, ProviderClient},
    completion::{CompletionModel, CompletionRequest, ToolDefinition},
    message::{Message, ToolResult, ToolResultContent, UserContent},
    streaming::StreamedAssistantContent,
};

#[derive(Debug, Deserialize, Serialize)]
struct InputMessage {
    content: UserContent,
    #[serde(default)]
    metadata: Option<serde_json::Value>,
}

#[derive(Debug, Serialize)]
struct OutputMessage {
    text: String,
    metadata: serde_json::Value,
}

// Context passed to tool implementations
struct ToolContext {
    sqs_client: SqsClient,
    group_id: String,
    metadata: Option<serde_json::Value>,
}

// Trait for tool implementations
#[async_trait]
trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn parameters(&self) -> serde_json::Value;
    async fn execute(
        &self,
        args: serde_json::Value,
        id: String,
        call_id: Option<String>,
        context: &ToolContext,
    ) -> Result<(), Error>;
}

// Sleep tool implementation
struct SleepTool {
    scheduler_client: SchedulerClient,
    input_queue_url: String,
    input_queue_arn: String,
    scheduler_role_arn: String,
}

#[async_trait]
impl Tool for SleepTool {
    fn name(&self) -> &str {
        "sleep"
    }

    fn description(&self) -> &str {
        "Sleep for a specified number of seconds before continuing. Useful for waiting or delaying actions."
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "seconds": {
                    "type": "number",
                    "description": "Number of seconds to sleep"
                }
            },
            "required": ["seconds"]
        })
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        id: String,
        call_id: Option<String>,
        context: &ToolContext,
    ) -> Result<(), Error> {
        let seconds = args["seconds"].as_f64().unwrap_or(0.0) as i64;

        // Create tool result message to be sent after sleep
        let tool_result_msg = InputMessage {
            content: UserContent::ToolResult(ToolResult {
                id,
                call_id,
                content: OneOrMany::one(ToolResultContent::Text(Text {
                    text: format!("Slept for {} seconds", seconds),
                })),
            }),
            metadata: context.metadata.clone(),
        };

        // SQS supports delays up to 900 seconds (15 minutes)
        // For longer delays, use EventBridge Scheduler
        if seconds <= 900 {
            // Use SQS delay for short sleeps
            context
                .sqs_client
                .send_message()
                .queue_url(&self.input_queue_url)
                .message_body(serde_json::to_string(&tool_result_msg)?)
                .message_attributes(
                    "ConversationGroupId",
                    MessageAttributeValue::builder()
                        .data_type("String")
                        .string_value(&context.group_id)
                        .build()?,
                )
                .delay_seconds(seconds as i32)
                .send()
                .await?;

            tracing::info!("Scheduled sleep for {} seconds using SQS delay", seconds);
        } else {
            // Use EventBridge Scheduler for longer sleeps
            let schedule_time = Utc::now() + Duration::seconds(seconds);
            let schedule_name = format!("sleep-{}", chrono::Utc::now().timestamp_millis());

            self.scheduler_client
                .create_schedule()
                .name(&schedule_name)
                .schedule_expression(format!("at({})", schedule_time.format("%Y-%m-%dT%H:%M:%S")))
                .flexible_time_window(
                    FlexibleTimeWindow::builder()
                        .mode(FlexibleTimeWindowMode::Off)
                        .build()?,
                )
                .target(
                    Target::builder()
                        .arn(&self.input_queue_arn)
                        .role_arn(&self.scheduler_role_arn)
                        .input(serde_json::to_string(&tool_result_msg)?)
                        .build()?,
                )
                .send()
                .await?;

            tracing::info!(
                "Scheduled sleep for {} seconds using EventBridge Scheduler",
                seconds
            );
        }

        tracing::info!("Sleep scheduled for {} seconds", seconds);
        Ok(())
    }
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

    // Get the conversation group ID from message attributes
    let group_id = payload
        .records
        .first()
        .unwrap()
        .message_attributes
        .get("ConversationGroupId")
        .unwrap()
        .string_value
        .as_ref()
        .unwrap()
        .clone();

    // Initialize AWS clients
    let config = aws_config::load_from_env().await;
    let dynamodb_client = DynamoDbClient::new(&config);
    let sqs_client = SqsClient::new(&config);
    let scheduler_client = SchedulerClient::new(&config);
    let table_name = "AgentZeroState".to_string();
    let output_queue_url = std::env::var("OUTPUT_QUEUE_URL").unwrap_or_else(|_| "".to_string());
    let scheduler_role_arn = std::env::var("SCHEDULER_ROLE_ARN").unwrap_or_else(|_| "".to_string());

    let mut current_history =
        HistoryManager::new_with_history(dynamodb_client.clone(), table_name, group_id.clone())
            .await?;

    for record in payload.records {
        let message_id = record.message_id.unwrap_or_default();
        let body = record.body.unwrap();

        // Parse the input message to extract metadata
        let input_msg: InputMessage = serde_json::from_str(&body)?;

        // Update metadata if provided (for first message in conversation)
        if let Some(metadata) = input_msg.metadata {
            current_history.update_metadata(metadata).await?;
        }

        current_history
            .handle_user_content(input_msg.content, message_id)
            .await?;
    }

    // Register tools
    let tool_impls: Vec<Box<dyn Tool>> = vec![Box::new(SleepTool {
        scheduler_client: scheduler_client.clone(),
        input_queue_url: std::env::var("INPUT_QUEUE_URL").unwrap_or_default(),
        input_queue_arn: std::env::var("INPUT_QUEUE_ARN").unwrap_or_default(),
        scheduler_role_arn: scheduler_role_arn.clone(),
    })];

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

    // Create tool context for execution
    let tool_context = ToolContext {
        sqs_client: sqs_client.clone(),
        group_id: group_id.clone(),
        metadata: current_history.get_metadata(),
    };

    let client = Client::from_env();
    let model = client.completion_model("global.anthropic.claude-haiku-4-5-20251001-v1:0");

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
        let completion_id = format!("{}-completion-{}", group_id, completion_counter);
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
            .message_attributes(
                "ConversationGroupId",
                MessageAttributeValue::builder()
                    .data_type("String")
                    .string_value(&group_id)
                    .build()?,
            )
            .send()
            .await?;

        tracing::info!("Sent response to output queue");
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
