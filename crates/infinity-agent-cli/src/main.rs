use futures_util::StreamExt;
use rig_bedrock::client::Client;
use tracing_subscriber::EnvFilter;

mod memory_store;

use infinity_agent_core::event_processor;
use infinity_agent_core::message::{InputMessage, InputMessageContent};
use infinity_agent_core::tools::sleep::SleepUntilEventOrInputTool;
use infinity_agent_core::tools::{Tool, ToolContext};
use memory_store::{InMemoryConversationStore, InMemoryMessageSender, InMemoryStateStore};
use rig::client::{CompletionClient, ProviderClient};
use rig::message::UserContent;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    rustls::crypto::aws_lc_rs::default_provider()
        .install_default()
        .expect("Failed to install default CryptoProvider");

    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .init();

    let conversation_store = InMemoryConversationStore::new();
    let state_store = InMemoryStateStore::new();
    let sender = InMemoryMessageSender::new();

    let client = Client::from_env();
    let model = client.completion_model("global.anthropic.claude-haiku-4-5-20251001-v1:0");

    let thread_id = uuid::Uuid::new_v4().to_string();
    println!("Infinity Agent CLI — thread {}", thread_id);
    println!("Type your messages below. Ctrl+C to exit.\n");

    let tool_impls: Vec<Box<dyn Tool<InMemoryMessageSender>>> =
        vec![Box::new(SleepUntilEventOrInputTool)];

    loop {
        print!("> ");
        use std::io::Write;
        std::io::stdout().flush()?;

        let mut input = String::new();
        if std::io::stdin().read_line(&mut input)? == 0 {
            break;
        }
        let input = input.trim();
        if input.is_empty() {
            continue;
        }

        let message_id = uuid::Uuid::new_v4().to_string();
        let input_msg = InputMessage {
            content: InputMessageContent::User(UserContent::text(input)),
            group_id: thread_id.clone(),
            metadata: None,
            synthetic: None,
        };

        // (a) Create history and prepare input
        let mut current_history = event_processor::HistoryManager::new_with_history(
            conversation_store.clone(),
            state_store.clone(),
            thread_id.clone(),
        )
        .await?;

        let prepare_result = event_processor::prepare_input(
            input_msg,
            message_id.clone(),
            &mut current_history,
            &conversation_store,
        )
        .await;

        match prepare_result {
            Ok(event_processor::PrepareResult::Handled) => continue,
            Ok(event_processor::PrepareResult::OAuthRequired { auth_url }) => {
                eprintln!(
                    "OAuth required — open this URL to authenticate:\n  {}\n",
                    auth_url
                );
                continue;
            }
            Err(e) => {
                eprintln!("Error: {}\n", e);
                continue;
            }
            Ok(event_processor::PrepareResult::Ready) => {}
        }

        // (b) Stream completion, printing text chunks as they arrive
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

        let final_action = {
            let mut completion_stream = std::pin::pin!(event_processor::run_completion(
                &model,
                &mut current_history,
                &tool_names,
                &tool_defs,
                &active_thread_id,
                &message_id,
            ));

            let mut action = None;
            while let Some(event) = completion_stream.next().await {
                match event {
                    Ok(event_processor::CompletionEvent::TextChunk(chunk)) => {
                        print!("{}", chunk);
                        std::io::stdout().flush().ok();
                    }
                    Ok(event_processor::CompletionEvent::Action(a)) => {
                        action = Some(a);
                    }
                    Err(e) => {
                        eprintln!("\nError: {}\n", e);
                        break;
                    }
                }
            }
            println!();
            action
        };

        // (c) Execute the action
        if let Some(action) = final_action {
            let tool_context = ToolContext {
                message_sender: sender.clone(),
                group_id: active_thread_id.clone(),
                input_queue_arn: String::new(),
                rap_receiver_url: String::new(),
                user_id: None,
            };
            let tool_registry: std::collections::HashMap<
                String,
                &Box<dyn Tool<InMemoryMessageSender>>,
            > = tool_impls
                .iter()
                .map(|t| (t.name().to_string(), t))
                .collect();

            if let Err(e) =
                event_processor::execute_action(action, &tool_registry, &tool_context).await
            {
                eprintln!("Error: {}\n", e);
            }
        }
    }

    Ok(())
}
