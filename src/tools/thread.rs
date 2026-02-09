use async_trait::async_trait;
use lambda_runtime::{Error, tracing};
use rig::{
    OneOrMany,
    agent::Text,
    message::{ToolResult, ToolResultContent, UserContent},
};

use crate::conversation_history::ConversationHistoryStore;
use crate::event_handler::{InputMessage, InputMessageContent};

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
            .spawn_thread(&context.group_id, spawn_order)
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
        "Close the specified thread, marking it as complete. Use this from a thread that wants to close itself. Make sure to cancel any subscriptions you own before calling this."
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "thread_id": {
                    "type": "string",
                    "description": "The ID of the thread to close."
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
        _context: &ToolContext,
    ) -> Result<(), Error> {
        let thread_id = args["thread_id"]
            .as_str()
            .ok_or_else(|| Error::from("thread_id is required"))?;

        self.conversation_store.close_thread(thread_id).await?;

        tracing::info!("Closed thread {}", thread_id);

        // We do not send a tool call back because the thread is now closed.
        // TODO(shadaj): notify the parent that the child was closed, and do not allow closing from the parent

        Ok(())
    }
}
