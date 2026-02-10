use async_trait::async_trait;
use lambda_runtime::{Error, tracing};
use rig::{
    OneOrMany,
    agent::Text,
    message::{ToolResult, ToolResultContent, UserContent},
};

use crate::conversation_history::ConversationHistoryStore;
use crate::event_handler::{InputMessage, InputMessageContent, SyntheticKind, TaggedSyntheticKind};

use super::{Tool, ToolContext};

/// Tool that spawns a new child thread and returns its ID.
pub struct SpawnThreadTool {
    pub conversation_store: ConversationHistoryStore,
}

#[async_trait]
impl Tool for SpawnThreadTool {
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
        context: &ToolContext,
    ) -> Result<(), Error> {
        let spawn_order = self
            .conversation_store
            .get_current_message_order(&context.group_id)
            .await?;

        let new_thread_id = self
            .conversation_store
            .spawn_thread(&context.group_id, spawn_order, &id)
            .await?;

        tracing::info!(
            "Spawned new thread {} from parent {}",
            new_thread_id,
            context.group_id
        );

        // Result to the PARENT thread
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

        // Result to the NEW thread
        let child_result = InputMessage {
            content: InputMessageContent::User(UserContent::ToolResult(ToolResult {
                id,
                call_id,
                content: OneOrMany::one(ToolResultContent::Text(Text {
                    text: format!(
                        "You are now in the spawned thread. See the instructions in the tool call. Your thread ID is {}",
                        new_thread_id
                    ),
                })),
            })),
            group_id: new_thread_id,
            metadata: None,
            synthetic: None,
        };

        context
            .sqs_client
            .send_message()
            .queue_url(&context.input_queue_url)
            .message_body(serde_json::to_string(&parent_result)?)
            .send()
            .await?;

        context
            .sqs_client
            .send_message()
            .queue_url(&context.input_queue_url)
            .message_body(serde_json::to_string(&child_result)?)
            .send()
            .await?;

        Ok(())
    }
}

/// Tool that closes a thread, returning control to the parent.
pub struct CloseThreadTool {
    pub conversation_store: ConversationHistoryStore,
}

#[async_trait]
impl Tool for CloseThreadTool {
    fn name(&self) -> &str {
        "close_thread"
    }

    fn description(&self) -> &str {
        "Close the specified thread, marking it as complete. Use this from a thread that wants to close itself. Make sure to cancel any subscriptions you own before calling this. If the thread has something to report or has information that should be remembered for future events handled by the parent or its future children, include it in report_to_parent. Omit report_to_parent if there is nothing worth reporting."
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "thread_id": {
                    "type": "string",
                    "description": "The ID of the thread to close."
                },
                "report_to_parent": {
                    "type": "string",
                    "description": "Optional report to send to the parent thread. Only include this if the thread has something to report or has information that should be remembered for future events handled by the parent or its future children. Omit if there is nothing worth reporting."
                }
            },
            "required": ["thread_id"]
        })
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        _id: String,
        _call_id: Option<String>,
        context: &ToolContext,
    ) -> Result<(), Error> {
        let thread_id = args["thread_id"]
            .as_str()
            .ok_or_else(|| Error::from("thread_id is required"))?;

        let report = args.get("report_to_parent").and_then(|v| v.as_str());

        self.conversation_store.close_thread(thread_id).await?;

        tracing::info!("Closed thread {}", thread_id);

        // If there's a report, send a synthetic subscription event to the parent
        if let Some(report_text) = report {
            if let Some((parent_id, spawn_tool_call_id)) = self
                .conversation_store
                .get_thread_parent_info(thread_id)
                .await?
            {
                tracing::info!(
                    "Sending report from thread {} to parent {} via tool call {}",
                    thread_id,
                    parent_id,
                    spawn_tool_call_id
                );

                let report_message = InputMessage {
                    content: InputMessageContent::User(UserContent::ToolResult(ToolResult {
                        id: String::new(),
                        call_id: None,
                        content: OneOrMany::one(ToolResultContent::Text(Text {
                            // TODO(shadaj): models get confused about this when it is for a subscription event, they don't know what a "child thread" is
                            text: format!("Report from child thread:\n{}", report_text),
                        })),
                    })),
                    group_id: parent_id,
                    metadata: None,
                    synthetic: Some(SyntheticKind::Tagged(TaggedSyntheticKind::ThreadReport {
                        tool_call_id: spawn_tool_call_id,
                    })),
                };

                context
                    .sqs_client
                    .send_message()
                    .queue_url(&context.input_queue_url)
                    .message_body(serde_json::to_string(&report_message)?)
                    .send()
                    .await?;
            }
        }

        Ok(())
    }
}
