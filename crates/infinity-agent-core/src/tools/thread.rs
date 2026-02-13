use async_trait::async_trait;
use rig::{
    OneOrMany,
    agent::Text,
    message::{ToolResult, ToolResultContent, UserContent},
};
use tracing;

use super::{Tool, ToolContext};
use crate::message::{InputMessage, InputMessageContent, SyntheticKind, TaggedSyntheticKind};
use crate::traits::{ConversationStore, MessageSender};

/// Tool that spawns a new child thread and returns its ID.
pub struct SpawnThreadTool<C: ConversationStore> {
    pub conversation_store: C,
}

#[async_trait]
impl<M: MessageSender + 'static, C: ConversationStore + 'static> Tool<M> for SpawnThreadTool<C> {
    fn name(&self) -> &str {
        "spawn_thread"
    }

    fn description(&self) -> &str {
        "Spawn a new child thread for a sub-task. The new thread inherits the conversation context of the current thread and will see its tasks automatically. Returns the new thread ID."
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "instructions": {
                    "type": "string",
                    "description": "Instructions for the spawned thread describing what it should do"
                }
            },
            "required": ["instructions"]
        })
    }

    async fn execute(
        &self,
        _args: serde_json::Value,
        id: String,
        call_id: Option<String>,
        context: &ToolContext<M>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let spawn_order = self
            .conversation_store
            .get_current_message_order(&context.group_id)
            .await
            .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?;

        let new_thread_id = self
            .conversation_store
            .spawn_thread(&context.group_id, spawn_order, &id)
            .await
            .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?;

        tracing::info!(
            "Spawned new thread {} from parent {}",
            new_thread_id,
            context.group_id
        );

        let parent_result = InputMessage {
            content: InputMessageContent::User(UserContent::ToolResult(ToolResult {
                id: id.clone(),
                call_id: call_id.clone(),
                content: OneOrMany::one(ToolResultContent::Text(Text {
                    text: format!(
                        "Child thread is successfully spawned and has ID: {}. You will be notified automatically if the child has anything to report.",
                        new_thread_id
                    ),
                })),
            })),
            group_id: context.group_id.clone(),
            metadata: None,
            synthetic: None,
        };

        let child_result = InputMessage {
            content: InputMessageContent::User(UserContent::ToolResult(ToolResult {
                id: id.clone(),
                call_id,
                content: OneOrMany::one(ToolResultContent::Text(Text {
                    text: format!(
                        "You are now INSIDE the thread that you requested to create. Follow the instructions in the tool call. Be careful not to get confused by the context before this call, which is from the parent thread. Your thread ID is {}",
                        new_thread_id
                    ),
                })),
            })),
            group_id: new_thread_id,
            metadata: None,
            synthetic: None,
        };

        context
            .send_to_input_queue(
                &serde_json::to_string(&parent_result)?,
                &context.group_id,
                &id,
            )
            .await?;

        context
            .send_to_input_queue(
                &serde_json::to_string(&child_result)?,
                &child_result.group_id,
                &id,
            )
            .await?;

        Ok(())
    }
}

/// Tool that sends a report to the parent thread without closing the current thread.
pub struct ReportToParentTool<C: ConversationStore> {
    pub conversation_store: C,
}

#[async_trait]
impl<M: MessageSender + 'static, C: ConversationStore + 'static> Tool<M> for ReportToParentTool<C> {
    fn name(&self) -> &str {
        "report_to_parent"
    }

    fn description(&self) -> &str {
        "Send a report to the parent thread. Use this when you have intermediate results, updates, or information the parent should know about while you continue working."
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "report": {
                    "type": "string",
                    "description": "The report content to send to the parent thread."
                }
            },
            "required": ["report"]
        })
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        id: String,
        call_id: Option<String>,
        context: &ToolContext<M>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let report_text = args["report"].as_str().ok_or("report is required")?;

        let (parent_id, spawn_tool_call_id) = self
            .conversation_store
            .get_thread_parent_info(&context.group_id)
            .await
            .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?
            .ok_or("No parent thread found — this is a root thread")?;

        let is_subscription = self
            .conversation_store
            .is_subscription_event_thread(&context.group_id)
            .await
            .unwrap_or(false);

        tracing::info!(
            "Sending report from thread {} to parent {} via tool call {}",
            context.group_id,
            parent_id,
            spawn_tool_call_id
        );

        let formatted_report = if is_subscription {
            format!(
                "Report from temporary child thread created to process a subscription event:\n{}",
                report_text
            )
        } else {
            format!("Report from child thread:\n{}", report_text)
        };

        let report_message = InputMessage {
            content: InputMessageContent::User(UserContent::ToolResult(ToolResult {
                id: String::new(),
                call_id: None,
                content: OneOrMany::one(ToolResultContent::Text(Text {
                    text: formatted_report,
                })),
            })),
            group_id: parent_id,
            metadata: None,
            synthetic: Some(SyntheticKind::Tagged(TaggedSyntheticKind::ThreadReport {
                tool_call_id: spawn_tool_call_id,
            })),
        };

        context
            .send_to_input_queue(
                &serde_json::to_string(&report_message)?,
                &report_message.group_id,
                &id,
            )
            .await?;

        let tool_result = InputMessage {
            content: InputMessageContent::User(UserContent::ToolResult(ToolResult {
                id: id.clone(),
                call_id,
                content: OneOrMany::one(ToolResultContent::Text(Text {
                    text: "Report sent to parent thread.".to_string(),
                })),
            })),
            group_id: context.group_id.clone(),
            metadata: None,
            synthetic: None,
        };

        context
            .send_to_input_queue(
                &serde_json::to_string(&tool_result)?,
                &context.group_id,
                &id,
            )
            .await?;

        Ok(())
    }
}
