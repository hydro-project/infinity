use aws_lambda_events::event::sqs::SqsEvent;
use aws_sdk_dsql::Client as DsqlClient;
use aws_sdk_dynamodb::{Client as DynamoDbClient, types::AttributeValue};
use aws_sdk_scheduler::Client as SchedulerClient;
use aws_sdk_sqs::Client as SqsClient;
use lambda_runtime::{Error, LambdaEvent, tracing};
use rig::message::AssistantContent;
use rig_bedrock::client::Client;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

use crate::conversation_history::ConversationHistoryStore;

use crate::tools::config::ToolsConfig;
use crate::tools::sleep::{SleepTool, SleepUntilEventOrInputTool, SleepUntilTool};
use crate::tools::thread::{CloseThreadTool, ReportToParentTool, SpawnThreadTool};
use crate::tools::{Tool, ToolContext, ToolSet, VecToolSet};

use futures_util::StreamExt;
use rig::{
    OneOrMany,
    client::{CompletionClient, ProviderClient},
    completion::{CompletionModel, CompletionRequest, ToolDefinition},
    message::{Message, ToolResult, ToolResultContent, UserContent},
    streaming::StreamedAssistantContent,
};

#[derive(Debug)]
enum CompletionError {
    UnexpectedEndOfStream,
    UnknownTool {
        name: String,
        id: String,
        call_id: Option<String>,
    },
    Other(Error),
}

impl From<Error> for CompletionError {
    fn from(e: Error) -> Self {
        CompletionError::Other(e)
    }
}

impl From<&str> for CompletionError {
    fn from(s: &str) -> Self {
        if s == "unexpected end of stream" {
            CompletionError::UnexpectedEndOfStream
        } else {
            CompletionError::Other(Error::from(s))
        }
    }
}

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

/// Distinguishes subscription events (which spawn a subthread) from thread reports
/// (which inject directly into the target thread).
/// A plain string deserializes as SubscriptionEvent for backward compatibility
/// with external producers like the GitHub webhook handler.
#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(untagged)]
pub enum SyntheticKind {
    Tagged(TaggedSyntheticKind),
    /// Backward compat: a bare string is treated as a subscription event
    SubscriptionEvent(String),
}

#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(tag = "type")]
pub enum TaggedSyntheticKind {
    #[serde(rename = "subscription_event")]
    SubscriptionEvent { tool_call_id: String },
    #[serde(rename = "thread_report")]
    ThreadReport { tool_call_id: String },
}

impl SyntheticKind {
    pub fn tool_call_id(&self) -> &str {
        match self {
            SyntheticKind::Tagged(TaggedSyntheticKind::SubscriptionEvent { tool_call_id }) => {
                tool_call_id
            }
            SyntheticKind::Tagged(TaggedSyntheticKind::ThreadReport { tool_call_id }) => {
                tool_call_id
            }
            SyntheticKind::SubscriptionEvent(id) => id,
        }
    }

    pub fn is_thread_report(&self) -> bool {
        matches!(
            self,
            SyntheticKind::Tagged(TaggedSyntheticKind::ThreadReport { .. })
        )
    }
}

#[derive(Debug, Deserialize, Serialize)]
pub struct InputMessage {
    pub content: InputMessageContent,
    pub group_id: String,
    #[serde(default)]
    pub metadata: Option<serde_json::Value>,
    #[serde(default)]
    pub synthetic: Option<SyntheticKind>,
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
    thread_id: String,
    root_thread_id: String,
    ancestor_chain: Vec<String>,
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
        thread_id: String,
    ) -> Result<Self, Error> {
        // Ensure root thread exists (idempotent for root threads, no-op for children)
        conversation_store.ensure_root_thread(&thread_id).await.ok();

        // Get ancestor chain for nesting prefix
        let ancestor_chain: Vec<String> = conversation_store
            .get_ancestor_chain(&thread_id)
            .await
            .map(|links| links.iter().map(|(tid, _)| tid.clone()).collect())
            .unwrap_or_default();
        let root_thread_id = ancestor_chain
            .first()
            .cloned()
            .unwrap_or_else(|| thread_id.clone());

        // Load conversation history from DSQL with ancestor prefix
        let history = conversation_store
            .load_history_with_ancestors(&thread_id)
            .await?;

        // Load metadata and processed IDs from DynamoDB
        // Metadata is always keyed by root thread; processed IDs by current thread.
        let metadata_result = dynamodb_client
            .get_item()
            .table_name(&table_name)
            .key("session", AttributeValue::S(root_thread_id.clone()))
            .send()
            .await;

        let metadata = match metadata_result {
            Ok(output) => output.item.and_then(|item| {
                item.get("metadata").and_then(|v| {
                    if let AttributeValue::S(s) = v {
                        serde_json::from_str(s).ok()
                    } else {
                        None
                    }
                })
            }),
            Err(_) => None,
        };

        let result = dynamodb_client
            .get_item()
            .table_name(&table_name)
            .key("session", AttributeValue::S(thread_id.clone()))
            .send()
            .await;

        let (processed_message_ids, processed_tool_calls) = match result {
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

                    (processed_ids, processed_tools)
                } else {
                    (HashSet::new(), HashSet::new())
                }
            }
            Err(_) => (HashSet::new(), HashSet::new()),
        };

        Ok(Self {
            dynamodb_client,
            conversation_store,
            table_name,
            thread_id,
            root_thread_id,
            ancestor_chain,
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
            } else if !self.history.last().is_some_and(|l| {
                if let Message::Assistant { content, .. } = l
                    && let AssistantContent::ToolCall(c) = content.first()
                {
                    c.id == tool_result.id
                } else {
                    false
                }
            }) {
                tracing::info!(
                    "Got tool call result for wrong call, ignoring {}",
                    tool_result.id
                );
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
                .append_messages(&self.thread_id, messages_for_dsql)
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
            .key("session", AttributeValue::S(self.thread_id.clone()));

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
            .key("session", AttributeValue::S(self.thread_id.clone()))
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
            .key("session", AttributeValue::S(self.root_thread_id.clone()))
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

    /// Remove trailing reasoning elements from history.
    /// This is needed when Bedrock returns UnexpectedEndOfStream error,
    /// as it doesn't allow input to end with thinking/reasoning.
    fn remove_trailing_reasoning(&mut self) {
        while let Some(Message::Assistant { content, .. }) = self.history.last() {
            if matches!(
                content.first(),
                rig::message::AssistantContent::Reasoning(_)
            ) {
                tracing::info!("Removing trailing reasoning element from history");
                self.history.pop();
            } else {
                break;
            }
        }
    }

    /// Returns the nesting prefix like "[root:nested_1:nested_2]" for non-root threads.
    /// Returns None for root threads.
    fn get_thread_nesting_prefix(&self) -> Option<String> {
        if self.ancestor_chain.is_empty() {
            return None; // root thread, no prefix
        }
        let mut labels: Vec<String> = self
            .ancestor_chain
            .iter()
            .map(|id| {
                let short = if id.len() > 8 { &id[..8] } else { id };
                short.to_string()
            })
            .collect();
        // Add the leaf (current thread)
        let short = if self.thread_id.len() > 8 {
            &self.thread_id[..8]
        } else {
            &self.thread_id
        };
        labels.push(short.to_string());
        Some(format!("[{}]", labels.join(":")))
    }
}

async fn process_completion_stream<M>(
    model: &M,
    completion_counter: &mut usize,
    current_history: &mut HistoryManager,
    tool_registry: &HashMap<String, &Box<dyn Tool>>,
    tool_context: &ToolContext,
    tools: &[ToolDefinition],
    group_id: &str,
    message_id: &str,
) -> Result<String, CompletionError>
where
    M: CompletionModel,
{
    let mut completion_result = model
        .stream(CompletionRequest {
            preamble: None,
            chat_history: current_history.get_history(),
            documents: vec![],
            tools: tools.to_vec(),
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

    let mut accumulated_text = String::new();

    loop {
        let res = completion_result
            .next()
            .await
            .ok_or(CompletionError::UnexpectedEndOfStream)?;
        let chunk = res.map_err(|e| CompletionError::Other(e.into()))?;

        if let StreamedAssistantContent::Reasoning(ref r) = chunk
            && r.signature.is_none()
        {
            continue; // incomplete reasoning
        }

        // Generate a unique ID for each completion chunk
        let completion_id = format!(
            "{}-{}-completion-{}",
            group_id, message_id, completion_counter
        );
        *completion_counter += 1;

        current_history.handle_completion(&chunk, completion_id);

        match chunk {
            StreamedAssistantContent::Text(text) => {
                tracing::info!("[Text] {}", &text.text);
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
                    tool.execute(call.function.arguments, call.id, call.call_id, tool_context)
                        .await?
                } else {
                    tracing::warn!("Unknown tool called: {}", call.function.name);
                    return Err(CompletionError::UnknownTool {
                        name: call.function.name,
                        id: call.id,
                        call_id: call.call_id,
                    });
                }

                break;
            }
            StreamedAssistantContent::ToolCallDelta { .. } => {}
            StreamedAssistantContent::Reasoning(reasoning) => {
                tracing::info!("\n[Reasoning: {:?}]\n", reasoning.reasoning);
            }
            StreamedAssistantContent::Final(_) => {
                tracing::info!("Received final message");
                break;
            }
        }
    }

    Ok(accumulated_text)
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
    let conversation_store =
        ConversationHistoryStore::new(&dsql_client, &dsql_cluster_endpoint).await?;

    // Load tool sets from configuration (DynamoDB preferred, then SSM, then file, then env var)
    let mut tool_sets: Vec<Box<dyn ToolSet>> = if let Ok(ddb_key) =
        std::env::var("TOOLS_CONFIG_DDB_KEY")
    {
        match ToolsConfig::from_dynamodb(&dynamodb_client, &table_name, &ddb_key).await {
            Ok(config) => {
                tracing::info!("Loaded tools configuration from DynamoDB key {}", ddb_key);
                config.into_tool_sets()
            }
            Err(e) => {
                tracing::warn!(
                    "Failed to load tools config from DynamoDB: {}. Using empty tool set.",
                    e
                );
                vec![]
            }
        }
    } else if let Ok(ssm_param) = std::env::var("TOOLS_CONFIG_SSM_PARAM") {
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
            Box::new(SleepUntilTool {
                scheduler_client: scheduler_client.clone(),
                scheduler_role_arn: scheduler_role_arn.clone(),
            }),
            Box::new(SpawnThreadTool {
                conversation_store: conversation_store.clone(),
            }),
            Box::new(ReportToParentTool {
                conversation_store: conversation_store.clone(),
            }),
            Box::new(CloseThreadTool {
                conversation_store: conversation_store.clone(),
            }),
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

        // Skip messages for closed threads
        if conversation_store
            .is_thread_closed(&input_msg.group_id)
            .await
            .unwrap_or(false)
        {
            tracing::warn!(
                "Received message for closed thread {}, skipping",
                input_msg.group_id
            );
            continue;
        }

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
        let content = if let Some(synthetic_kind) = input_msg.synthetic {
            let original_tool_call_id = synthetic_kind.tool_call_id().to_string();
            tracing::info!(
                "Processing synthetic tool result for tool call: {}",
                original_tool_call_id
            );

            // Look up the original tool call from conversation history
            let original_call = current_history.history.iter().find_map(|msg| {
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
            });

            let Some(original_call) = original_call else {
                tracing::warn!(
                    "Could not find original tool call for synthetic message: {}, dropping message",
                    original_tool_call_id
                );
                continue;
            };

            if synthetic_kind.is_thread_report() {
                // Thread reports inject directly into the target thread
                let new_tool_call_id = uuid::Uuid::new_v4().to_string();

                if let UserContent::ToolResult(mut tool_result) = user_content {
                    let mut synthetic_args = original_call.function.arguments.clone();
                    synthetic_args.as_object_mut().unwrap().insert(
                        "kind".to_string(),
                        serde_json::json!(format!(
                            "thread_report:{} (child thread closed)",
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

                    current_history
                        .handle_content(
                            synthetic_tool_call,
                            format!("{}-synthetic-call", new_tool_call_id),
                        )
                        .await?;

                    tool_result.id = new_tool_call_id;
                    UserContent::ToolResult(tool_result)
                } else {
                    panic!("Synthetic message is not a tool result")
                }
            } else {
                // Subscription events spawn a new subthread to process the event
                tracing::info!(
                    "Spawning subthread for subscription event from tool call: {}",
                    original_tool_call_id
                );

                // Get current message order for the spawn point (truncation point for parent)
                let spawn_order = conversation_store
                    .get_current_message_order(&input_msg.group_id)
                    .await?;

                // Create the subthread — spawn_tool_call_id is the original subscription tool call ID
                // so that if the subthread closes with a report, it references the right call in the parent
                let sub_thread_id = conversation_store
                    .spawn_thread(&input_msg.group_id, spawn_order, &original_tool_call_id)
                    .await?;

                tracing::info!(
                    "Created subthread {} for subscription event in parent {}",
                    sub_thread_id,
                    input_msg.group_id
                );

                // Build the seed messages for the subthread:
                // 1. Synthetic spawn_thread tool call (assistant)
                // 2. Spawn result (user) — "You are in a new thread created for processing a subscription event"
                // 3. Synthetic subscription tool call (assistant)
                // 4. The actual event content (user tool result)

                let spawn_call_id = uuid::Uuid::new_v4().to_string();
                let event_call_id = uuid::Uuid::new_v4().to_string();

                let spawn_tool_call = Message::Assistant {
                    id: None,
                    content: OneOrMany::one(rig::message::AssistantContent::ToolCall(
                        rig::message::ToolCall {
                            id: spawn_call_id.clone(),
                            call_id: None,
                            function: rig::message::ToolFunction {
                                name: "spawn_thread".to_string(),
                                arguments: serde_json::json!({
                                    "instructions": format!(
                                        "Process the incoming subscription event for tool call '{}', and close the thread after processing just this event with a report to the parent if appropriate. Only your report will be visible. The subscription will automatically resume after you close the",
                                        original_call.function.name
                                    )
                                }),
                            },
                        },
                    )),
                };

                let spawn_result = Message::User {
                    content: OneOrMany::one(UserContent::ToolResult(ToolResult {
                        id: spawn_call_id.clone(),
                        call_id: None,
                        content: OneOrMany::one(ToolResultContent::Text(rig::agent::Text {
                            text: format!(
                                "You are in a temporary child thread created for processing a subscription event. Your thread ID is {}, the parent which is still subscribing is {}",
                                sub_thread_id, input_msg.group_id
                            ),
                        })),
                    })),
                };

                // Synthetic subscription tool call with the original args + interrupt kind
                let mut synthetic_args = original_call.function.arguments.clone();
                synthetic_args.as_object_mut().unwrap().insert(
                    "kind".to_string(),
                    serde_json::json!(format!(
                        "interrupt:{} (subscription remains active)",
                        original_tool_call_id
                    )),
                );

                let event_tool_call = Message::Assistant {
                    id: None,
                    content: OneOrMany::one(rig::message::AssistantContent::ToolCall(
                        rig::message::ToolCall {
                            id: event_call_id.clone(),
                            call_id: None,
                            function: rig::message::ToolFunction {
                                name: original_call.function.name.clone(),
                                arguments: synthetic_args,
                            },
                        },
                    )),
                };

                // Extract the event content from the tool result
                let event_content = if let UserContent::ToolResult(mut tool_result) = user_content {
                    tool_result.id = event_call_id.clone();
                    tool_result.call_id = None;
                    tool_result
                } else {
                    panic!("Synthetic subscription event is not a tool result")
                };

                // Reload history manager for the subthread so we process inline
                // instead of paying for another SQS → Lambda round trip.
                current_history = HistoryManager::new_with_history(
                    dynamodb_client.clone(),
                    conversation_store.clone(),
                    table_name.clone(),
                    sub_thread_id,
                )
                .await?;

                // Seed the 3 messages via handle_content so they get synced to DSQL later
                current_history
                    .handle_content(spawn_tool_call, format!("{}-spawn-call", spawn_call_id))
                    .await?;
                current_history
                    .handle_content(spawn_result, format!("{}-spawn-result", spawn_call_id))
                    .await?;
                current_history
                    .handle_content(event_tool_call, format!("{}-event-call", event_call_id))
                    .await?;

                // The 4th message (event tool result) becomes the content to process normally
                UserContent::ToolResult(event_content)
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

        let active_thread_id = current_history.thread_id.clone();

        let tool_context = ToolContext {
            sqs_client: sqs_client.clone(),
            group_id: active_thread_id.clone(),
            input_queue_url: std::env::var("INPUT_QUEUE_URL").unwrap_or_default(),
            input_queue_arn: std::env::var("INPUT_QUEUE_ARN").unwrap_or_default(),
            user_id,
        };

        let mut completion_counter = 0;
        let accumulated_text = loop {
            match process_completion_stream(
                &model,
                &mut completion_counter,
                &mut current_history,
                &tool_registry,
                &tool_context,
                &tools,
                &active_thread_id,
                &message_id,
            )
            .await
            {
                Ok(text) => break text,
                Err(CompletionError::UnexpectedEndOfStream) => {
                    tracing::warn!(
                        "Stream ended unexpectedly, removing trailing reasoning and retrying..."
                    );
                    current_history.remove_trailing_reasoning();
                    continue;
                }
                Err(CompletionError::UnknownTool { name, id, call_id }) => {
                    tracing::warn!("Unknown tool '{}' called, sending error result", name);

                    let tool_result = Message::User {
                        content: OneOrMany::one(UserContent::ToolResult(
                            rig::message::ToolResult {
                                id: id.clone(),
                                call_id,
                                content: OneOrMany::one(rig::message::ToolResultContent::Text(
                                    rig::agent::Text {
                                        text: format!("Error: tool '{}' does not exist", name),
                                    },
                                )),
                            },
                        )),
                    };

                    current_history
                        .handle_content(tool_result, format!("{}-unknown-tool", id))
                        .await?;

                    continue;
                }
                Err(CompletionError::Other(e)) => return Err(e),
            }
        };

        // Sync any remaining pending items to DynamoDB after the loop
        current_history.sync().await?;

        // Send accumulated response to output queue
        if !accumulated_text.is_empty() && !output_queue_url.is_empty() {
            let metadata = current_history
                .get_metadata()
                .unwrap_or(serde_json::json!({}));

            // Prepend thread nesting prefix for non-root threads
            let output_text = if let Some(prefix) = current_history.get_thread_nesting_prefix() {
                format!("{} {}", prefix, accumulated_text)
            } else {
                accumulated_text
            };

            let output_msg = OutputMessage {
                text: output_text,
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
