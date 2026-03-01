use async_trait::async_trait;
use rig::{
    OneOrMany,
    agent::Text,
    message::{ToolResult, ToolResultContent, UserContent},
};
use tracing;

use super::{Tool, ToolContext};
use crate::message::{InputMessage, InputMessageContent, SyntheticKind, TaggedSyntheticKind};
use crate::traits::{ConversationStore, InputSender};

/// Tool that spawns a new child thread and returns its ID.
pub struct SpawnThreadTool<C: ConversationStore> {
    pub conversation_store: C,
}

#[async_trait]
impl<M: InputSender + 'static, C: ConversationStore + 'static> Tool<M> for SpawnThreadTool<C> {
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
                    "description": "Instructions for the spawned thread describing what it should do. Make sure to include in the instructions what **you plan to do while the thread runs** to make sure the child thread doesn't accidentally duplicate your work."
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
        let new_thread_id = self
            .conversation_store
            .spawn_thread(&context.group_id, &id, false)
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
                        "You are now INSIDE the thread that you requested to create. Follow the instructions in the tool call parameters for **THIS TOOL CALL**. Be careful not to get confused by the context before this call, which is from the parent thread. Your thread ID is {}",
                        new_thread_id
                    ),
                })),
            })),
            group_id: new_thread_id,
            metadata: None,
            synthetic: None,
        };

        context
            .message_sender
            .send_to_input_queue(parent_result, &context.group_id, &id)
            .await?;

        let child_group_id = child_result.group_id.clone();
        context
            .message_sender
            .send_to_input_queue(child_result, &child_group_id, &id)
            .await?;

        Ok(())
    }
}

/// Tool that sends a report to the parent thread without closing the current thread.
pub struct ReportToParentTool<C: ConversationStore> {
    pub conversation_store: C,
}

#[async_trait]
impl<M: InputSender + 'static, C: ConversationStore + 'static> Tool<M> for ReportToParentTool<C> {
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
            format!("Report from child thread: {}", report_text)
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

        let report_group_id = report_message.group_id.clone();
        context
            .message_sender
            .send_to_input_queue(report_message, &report_group_id, &id)
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
            .message_sender
            .send_to_input_queue(tool_result, &context.group_id, &id)
            .await?;

        Ok(())
    }
}

/// Tool that closes a thread, returning control to the parent.
pub struct CloseThreadTool<C: ConversationStore> {
    pub conversation_store: C,
}

#[async_trait]
impl<M: InputSender + 'static, C: ConversationStore + 'static> Tool<M> for CloseThreadTool<C> {
    fn name(&self) -> &str {
        "close_thread"
    }

    fn description(&self) -> &str {
        "Permanently shuts down the specified thread, marking it as complete. Use this from a thread that wants to close itself. Make sure to cancel any subscriptions you created before calling this. If the thread has additional information has not already been reported to the parent, include it in report_to_parent. Omit report_to_parent if there is nothing worth reporting."
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
                    "description": "Optional report to send to the parent thread."
                }
            },
            "required": ["thread_id"]
        })
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        id: String,
        call_id: Option<String>,
        context: &ToolContext<M>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let thread_id = args["thread_id"].as_str().ok_or("thread_id is required")?;

        if thread_id != context.group_id {
            let tool_result = InputMessage {
                content: InputMessageContent::User(UserContent::ToolResult(ToolResult {
                    id: id.clone(),
                    call_id,
                    content: OneOrMany::one(ToolResultContent::Text(Text {
                        text: format!(
                            "Error: the provided thread ID to close {} does not match the current thread ID {}",
                            thread_id, context.group_id
                        ),
                    })),
                })),
                group_id: context.group_id.clone(),
                metadata: None,
                synthetic: None,
            };

            context
                .message_sender
                .send_to_input_queue(tool_result, &context.group_id, &id)
                .await?;

            return Ok(());
        }

        let report = args.get("report_to_parent").and_then(|v| v.as_str());

        self.conversation_store
            .close_thread(thread_id)
            .await
            .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?;

        tracing::info!("Closed thread {}", thread_id);

        let is_subscription = self
            .conversation_store
            .is_subscription_event_thread(thread_id)
            .await
            .unwrap_or(false);

        if let Some((parent_id, spawn_tool_call_id)) = self
            .conversation_store
            .get_thread_parent_info(thread_id)
            .await
            .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?
        {
            let report_text = if is_subscription {
                report.map(|report_text| format!(
                        "An event from your subscription {} was processed by a child thread. The subscription remains active. Report from the child:\n{}",
                        spawn_tool_call_id, report_text
                    ))
            } else if let Some(report_text) = report {
                Some(format!(
                    "Child thread with ID {} has shut down. Report from child thread: {}",
                    thread_id, report_text
                ))
            } else {
                Some(format!("Child thread with ID {} has shut down", thread_id))
            };

            if let Some(report_text) = report_text {
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
                            text: report_text,
                        })),
                    })),
                    group_id: parent_id,
                    metadata: None,
                    synthetic: Some(SyntheticKind::Tagged(TaggedSyntheticKind::ThreadReport {
                        tool_call_id: spawn_tool_call_id,
                    })),
                };

                let report_group_id = report_message.group_id.clone();
                context
                    .message_sender
                    .send_to_input_queue(report_message, &report_group_id, &id)
                    .await?;
            }
        }

        Ok(())
    }
}
