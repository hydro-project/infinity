use futures_util::StreamExt;
use rig::client::{CompletionClient, ProviderClient};
use rig::completion::CompletionModel;
use rig::message::{AssistantContent, Message, ToolResultContent, UserContent};
use rig_bedrock::client::Client;
use tokio::sync::mpsc;
use tracing_subscriber::EnvFilter;

mod inline_viewport;
mod memory_store;
mod modifier_diff;
mod rap_callback;
mod rap_tools;
mod sleep_tools;
mod terminal;

use infinity_agent_core::event_processor;
use infinity_agent_core::message::{InputMessage, InputMessageContent};
use infinity_agent_core::tools::config::ToolsConfig;
use infinity_agent_core::tools::sleep::SleepUntilEventOrInputTool;
use infinity_agent_core::tools::thread::{CloseThreadTool, ReportToParentTool, SpawnThreadTool};
use infinity_agent_core::tools::{Tool, ToolContext};
use memory_store::{InMemoryConversationStore, InMemoryMessageSender, InMemoryStateStore};
use sleep_tools::{SleepTool, SleepUntilTool};
use terminal::DisplayEvent;

type BoxError = Box<dyn std::error::Error + Send + Sync>;

#[tokio::main]
async fn main() -> Result<(), BoxError> {
    let log_file = std::fs::File::create("/tmp/infinity-agent-cli.log").ok();
    if let Some(file) = log_file {
        tracing_subscriber::fmt()
            .with_env_filter(EnvFilter::from_default_env())
            .with_writer(std::sync::Mutex::new(file))
            .with_ansi(false)
            .init();
    }

    let conversation_store = InMemoryConversationStore::new();
    let state_store = InMemoryStateStore::new();

    let (input_tx, input_rx) = mpsc::unbounded_channel::<(InputMessage, String)>();
    let sender = InMemoryMessageSender::new(input_tx.clone());

    // Start the RAP callback server so tools can POST results back
    let callback_url = rap_callback::start_callback_server(input_tx.clone())
        .await
        .expect("Failed to start RAP callback server");

    // Load RAP tool servers from config file
    let rap_config_path =
        std::env::var("RAP_CONFIG").unwrap_or_else(|_| "rap-servers.json".to_string());
    let rap_tools: Vec<Box<dyn Tool<InMemoryMessageSender>>> =
        if let Ok(config) = ToolsConfig::from_file(&rap_config_path) {
            let urls = config.toolset_server_urls();
            if !urls.is_empty() {
                match rap_tools::load_rap_tools(&urls).await {
                    Ok(tools) => {
                        eprintln!(
                            "Loaded {} RAP tool(s) from {}",
                            tools.len(),
                            rap_config_path
                        );
                        tools
                    }
                    Err(e) => {
                        eprintln!("Warning: failed to load RAP tools: {}", e);
                        Vec::new()
                    }
                }
            } else {
                Vec::new()
            }
        } else {
            Vec::new()
        };

    let client = Client::from_env();
    let model = client.completion_model("global.anthropic.claude-opus-4-6-v1");

    let thread_id = uuid::Uuid::new_v4().to_string();
    let (display_tx, display_rx) = mpsc::unbounded_channel::<DisplayEvent>();

    let agent_handle = tokio::spawn(agent_loop(
        input_rx,
        display_tx,
        model,
        conversation_store,
        state_store,
        sender,
        callback_url,
        rap_tools,
    ));

    let result = terminal::run(input_tx, display_rx, thread_id).await;
    agent_handle.abort();
    result
}

// ── Agent loop ──────────────────────────────────────────────────────────────

#[expect(clippy::too_many_arguments, reason = "internal")]
async fn agent_loop<Mdl>(
    mut rx: mpsc::UnboundedReceiver<(InputMessage, String)>,
    display_tx: mpsc::UnboundedSender<DisplayEvent>,
    model: Mdl,
    conversation_store: InMemoryConversationStore,
    state_store: InMemoryStateStore,
    sender: InMemoryMessageSender,
    callback_url: String,
    rap_tools: Vec<Box<dyn Tool<InMemoryMessageSender>>>,
) where
    Mdl: CompletionModel + Send + Sync + 'static,
{
    let mut tool_impls: Vec<Box<dyn Tool<InMemoryMessageSender>>> = vec![
        Box::new(SleepUntilEventOrInputTool),
        Box::new(SleepTool),
        Box::new(SleepUntilTool),
        Box::new(SpawnThreadTool {
            conversation_store: conversation_store.clone(),
        }),
        Box::new(ReportToParentTool {
            conversation_store: conversation_store.clone(),
        }),
        Box::new(CloseThreadTool {
            conversation_store: conversation_store.clone(),
        }),
    ];
    tool_impls.extend(rap_tools);

    while let Some((input_msg, message_id)) = rx.recv().await {
        let active_group_id = input_msg.group_id.clone();

        let mut current_history = match event_processor::HistoryManager::new_with_history(
            conversation_store.clone(),
            state_store.clone(),
            active_group_id.clone(),
        )
        .await
        {
            Ok(h) => h,
            Err(e) => {
                let _ = display_tx.send(DisplayEvent::Info(format!("Error: {}", e)));
                continue;
            }
        };

        // Echo to the terminal — subscription events vs user input.
        if let Some(synth) = input_msg.synthetic.as_ref() {
            if let InputMessageContent::User(UserContent::ToolResult(res)) = &input_msg.content
                && let ToolResultContent::Text(text) = res.content.first()
            {
                let orig_call = current_history.get_history().into_iter().find(|h| {
                    if let Message::Assistant { content, .. } = h
                        && let AssistantContent::ToolCall(c) = content.first()
                    {
                        c.id == synth.tool_call_id()
                    } else {
                        false
                    }
                });

                if let Some(h) = orig_call
                    && let Message::Assistant { content, .. } = h
                    && let AssistantContent::ToolCall(c) = content.first()
                {
                    let _ = display_tx.send(DisplayEvent::SubscriptionEvent {
                        name: format!("{}({})", c.function.name, c.function.arguments),
                        text: text.text,
                        prefix: current_history.get_thread_nesting_prefix(),
                    });
                }
            }
        } else if let InputMessageContent::User(UserContent::ToolResult(res)) = &input_msg.content
            && let ToolResultContent::Text(text) = res.content.first()
        {
            let _ = display_tx.send(DisplayEvent::ToolResult {
                text: text.text,
                display_as: input_msg.display_as.clone(),
                prefix: current_history.get_thread_nesting_prefix(),
            });
        } else if let InputMessageContent::User(UserContent::Text(ref text)) = input_msg.content {
            let _ = display_tx.send(DisplayEvent::UserInput(text.text.clone()));
        }

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
                let _ = display_tx.send(DisplayEvent::Info(format!(
                    "OAuth required — open this URL:\n  {}",
                    auth_url
                )));
                continue;
            }
            Err(e) => {
                let _ = display_tx.send(DisplayEvent::Info(format!("Error: {}", e)));
                continue;
            }
            Ok(event_processor::PrepareResult::Ready) => {}
        }

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
        let thread_prefix = current_history.get_thread_nesting_prefix();

        let final_action = {
            let mut stream = std::pin::pin!(event_processor::run_completion(
                &model,
                &mut current_history,
                &tool_names,
                &tool_defs,
                &active_thread_id,
                &message_id,
            ));
            let mut action = None;
            let mut started = false;
            let mut any_text = false;
            while let Some(ev) = stream.next().await {
                match ev {
                    Ok(event_processor::CompletionEvent::TextChunk(chunk)) => {
                        any_text = true;
                        if !started {
                            let _ = display_tx.send(DisplayEvent::StartOutput {
                                prefix: thread_prefix.clone(),
                            });
                            started = true;
                        }
                        let _ = display_tx.send(DisplayEvent::TextChunk(chunk));
                    }
                    Ok(event_processor::CompletionEvent::Action(a)) => {
                        action = Some(a);
                    }
                    Err(e) => {
                        let _ = display_tx.send(DisplayEvent::Info(format!("Error: {}", e)));
                        break;
                    }
                }
            }
            if any_text {
                let _ = display_tx.send(DisplayEvent::ResponseDone);
            }
            action
        };

        current_history.sync().await.ok();

        if let Some(event_processor::CompletionAction::ExecuteToolCall {
            ref tool_name,
            ref tool_args,
            ..
        }) = final_action
            && tool_name != "sleep_until_event_or_input"
        {
            let _ = display_tx.send(DisplayEvent::ToolCall {
                name: tool_name.clone(),
                args: tool_args.clone(),
                prefix: thread_prefix.clone(),
            });
        }

        if let Some(action) = final_action {
            let tool_context = ToolContext {
                message_sender: sender.clone(),
                group_id: active_thread_id.clone(),
                input_queue_arn: String::new(),
                callback_url: callback_url.clone(),
                user_id: None,
            };
            let tool_registry: std::collections::HashMap<String, &dyn Tool<InMemoryMessageSender>> =
                tool_impls
                    .iter()
                    .map(|t| (t.name().to_string(), t.as_ref()))
                    .collect();
            if let Err(e) =
                event_processor::execute_action(action, &tool_registry, &tool_context).await
            {
                let _ = display_tx.send(DisplayEvent::Info(format!("Error: {}", e)));
            }
        }
    }
}
