use async_trait::async_trait;
use infinity_agent_core::message::{InputMessage, InputMessageContent};
use infinity_agent_core::tools::{Tool, ToolContext};
use infinity_agent_core::traits::InputSender;
use rig::{
    OneOrMany,
    agent::Text,
    message::{ToolResult, ToolResultContent, UserContent},
};

use crate::session::SessionStoreHandle;

pub struct SetTitleTool {
    pub session_store: SessionStoreHandle,
}

#[async_trait]
impl<M: InputSender + 'static> Tool<M> for SetTitleTool {
    fn name(&self) -> &str {
        "set_title"
    }

    fn description(&self) -> &str {
        "Set a short, friendly human-readable title for the current thread describing what it is working on."
    }

    fn display_script(&self) -> Option<&str> {
        Some(r#""Set Thread Title: " + args.title"#)
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "title": {
                    "type": "string",
                    "description": "A short human-readable title for the thread"
                }
            },
            "required": ["title"]
        })
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        id: String,
        call_id: Option<String>,
        context: &ToolContext<M>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let title = args["title"].as_str().unwrap_or("").to_string();
        if context.thread_stack.len() <= 1 {
            // for now, we only save titles for root threads
            let mut store = self.session_store.lock().await;
            store.set_title(&context.group_id, &title);
            let _ = store.save();
        }

        let msg = InputMessage {
            content: InputMessageContent::User(UserContent::ToolResult(ToolResult {
                id: id.clone(),
                call_id,
                content: OneOrMany::one(ToolResultContent::Text(Text {
                    text: format!("Title set to: {}", title),
                })),
            })),
            group_id: context.group_id.clone(),
            metadata: None,
            synthetic: None,
            display_as: None,
            subscription: false,
        };

        context
            .message_sender
            .send_to_input_queue(msg, &context.group_id, &id)
            .await?;

        Ok(())
    }
}
