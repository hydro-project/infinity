use futures_util::StreamExt;
use rig::client::{CompletionClient, ProviderClient};
use rig::completion::CompletionModel;
use rig::message::{AssistantContent, Message, ToolResultContent, UserContent};
use rig_bedrock::client::Client;
use rig_bedrock::streaming::{BedrockStreamingResponse, BedrockUsage};
use tokio::sync::mpsc;
use tracing_subscriber::EnvFilter;

mod inline_viewport;
mod memory_store;
mod modifier_diff;
mod rap_callback;
mod rap_tools;
mod sleep_tools;
mod terminal;
mod text_input;

use infinity_agent_core::event_processor::{self, CompletionAction};
use infinity_agent_core::message::{InputMessage, InputMessageContent};
use infinity_agent_core::tools::config::{ToolSetConfig, ToolsConfig};
use infinity_agent_core::tools::sleep::SleepUntilEventOrInputTool;
use infinity_agent_core::tools::thread::{CloseThreadTool, ReportToParentTool, SpawnThreadTool};
use infinity_agent_core::tools::{Tool, ToolContext};
use infinity_agent_core::traits::ConversationStore;
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

    std::fs::create_dir_all(".infinity").ok();

    let store_path = ".infinity/store.json";
    let session_path = ".infinity/session.json";

    // Try to load persisted store, fall back to empty.
    let conversation_store = InMemoryConversationStore::load_from_file(store_path)
        .unwrap_or_else(|_| InMemoryConversationStore::new());
    let state_store = InMemoryStateStore::new();

    // Try to load persisted thread_id, fall back to new.
    let (thread_id, initial_tokens_used) = {
        let session = std::fs::read_to_string(session_path)
            .ok()
            .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok());
        let tid = session
            .as_ref()
            .and_then(|v| v.get("thread_id")?.as_str().map(String::from))
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        let tokens = session
            .as_ref()
            .and_then(|v| v.get("total_tokens_used")?.as_u64())
            .unwrap_or(0) as usize;
        (tid, tokens)
    };

    let (input_tx, input_rx) = mpsc::unbounded_channel::<(InputMessage, String)>();
    let sender = InMemoryMessageSender::new(input_tx.clone());

    let client = Client::from_env();
    let model = client.completion_model("global.anthropic.claude-opus-4-6-v1");

    let (display_tx, display_rx) = mpsc::unbounded_channel::<DisplayEvent<_>>();

    // Clone the store so the agent loop owns one copy and we keep one for saving.
    let agent_store = conversation_store.clone();

    // Spawn a task that boots the agent (callback server, RAP config, tool loading)
    // then enters the agent loop. Info messages go to the terminal via display_tx.
    let agent_display_tx = display_tx.clone();
    let agent_input_tx = input_tx.clone();

    // Start the RAP callback server so tools can POST results back
    let callback_url = rap_callback::start_callback_server(agent_input_tx)
        .await
        .expect("Failed to start RAP callback server");

    // Ensure .infinity/rap.json exists, creating it with defaults if needed
    let rap_config_path = ".infinity/rap.json";
    if !std::path::Path::new(rap_config_path).exists() {
        std::fs::create_dir_all(".infinity").ok();
        let rap_bins = discover_rap_binaries();
        let tool_sets: Vec<ToolSetConfig> = rap_bins
            .into_iter()
            .map(|bin| ToolSetConfig::ToolsetCommand { command: bin })
            .collect();
        let default_config = ToolsConfig { tool_sets };
        let json = serde_json::to_string_pretty(&default_config)
            .expect("failed to serialize default rap config");
        std::fs::write(rap_config_path, json).expect("failed to write .infinity/rap.json");
        let _ = agent_display_tx.send(DisplayEvent::Info(
            "Initialized .infinity/rap.json".to_string(),
        ));
    }

    // Load RAP tool servers from config file.
    // Servers can be specified as static URLs or as commands to launch.
    // Command-based servers are spawned with RAP_EMBEDDED=1 and must emit
    // a JSON line `{"port": <u16>}` on stdout when ready.
    let mut spawned_children: Vec<std::process::Child> = Vec::new();

    let rap_tools: Vec<Box<dyn Tool<InMemoryMessageSender>>> =
        match ToolsConfig::from_file(rap_config_path).ok() {
            Some(config) => {
                let mut urls = config.toolset_server_urls();

                // Spawn command-based servers and collect their URLs.
                for cmd in config.toolset_commands() {
                    let _ = agent_display_tx
                        .send(DisplayEvent::Info(format!("Launching RAP server: {cmd}")));
                    match spawn_rap_server(&cmd) {
                        Ok((child, port)) => {
                            let url = format!("http://127.0.0.1:{port}");
                            let _ = agent_display_tx.send(DisplayEvent::Info(format!(
                                "RAP server ready on port {port}"
                            )));
                            urls.push(url);
                            spawned_children.push(child);
                        }
                        Err(e) => {
                            let _ = agent_display_tx.send(DisplayEvent::Info(format!(
                                "Warning: failed to launch RAP server '{cmd}': {e}"
                            )));
                        }
                    }
                }

                if !urls.is_empty() {
                    match rap_tools::load_rap_tools(&urls).await {
                        Ok(tools) => {
                            let _ = agent_display_tx.send(DisplayEvent::Info(format!(
                                "Loaded {} RAP tool(s) from {}",
                                tools.len(),
                                rap_config_path
                            )));
                            tools
                        }
                        Err(e) => {
                            let _ = agent_display_tx.send(DisplayEvent::Info(format!(
                                "Warning: failed to load RAP tools: {}",
                                e
                            )));
                            Vec::new()
                        }
                    }
                } else {
                    Vec::new()
                }
            }
            None => Vec::new(),
        };

    let agent_handle = tokio::spawn(async move {
        agent_loop(
            input_rx,
            agent_display_tx,
            model,
            agent_store,
            state_store,
            sender,
            callback_url,
            rap_tools,
        )
        .await;
    });

    // Replay persisted history to the terminal so previous messages are visible.
    if let Ok(history) = conversation_store.load_history(&thread_id).await {
        for message in &history {
            match message {
                Message::User { content } => match content.first() {
                    UserContent::Text(text) => {
                        let _ = display_tx.send(DisplayEvent::UserInput(text.text.clone()));
                        let _ = display_tx.send(DisplayEvent::StartOutput { prefix: None });
                    }
                    UserContent::ToolResult(res) => {
                        if let ToolResultContent::Text(text) = res.content.first() {
                            let _ = display_tx.send(DisplayEvent::ToolResult {
                                text: text.text,
                                display_as: None,
                                prefix: None,
                            });
                        }
                    }
                    _ => {}
                },
                Message::Assistant { content, .. } => match content.first() {
                    AssistantContent::Text(text) => {
                        let _ = display_tx.send(DisplayEvent::TextChunk(text.text));
                    }
                    AssistantContent::ToolCall(call) => {
                        let _ = display_tx.send(DisplayEvent::ToolCall {
                            name: call.function.name.clone(),
                            args: call.function.arguments.clone(),
                            prefix: None,
                        });
                    }
                    _ => {}
                },
            }
        }

        let _ = display_tx.send(DisplayEvent::ResponseDone(
            None,
            BedrockStreamingResponse {
                usage: Some(BedrockUsage {
                    input_tokens: 0,
                    output_tokens: 0,
                    total_tokens: initial_tokens_used as i32,
                }),
            },
        ));
    }

    let (new_session_tx, mut new_session_rx) = mpsc::unbounded_channel::<String>();

    // Track the active thread_id so we can save the latest on shutdown.
    let active_thread_id = std::sync::Arc::new(std::sync::Mutex::new(thread_id.clone()));
    let active_thread_id_for_listener = active_thread_id.clone();

    // Listen for new-session signals from the terminal (Ctrl+N).
    tokio::spawn(async move {
        while let Some(new_id) = new_session_rx.recv().await {
            *active_thread_id_for_listener.lock().unwrap() = new_id;
        }
    });

    let result = terminal::run(
        input_tx,
        display_rx,
        thread_id,
        "claude-opus-4-6".to_string(),
        new_session_tx,
    )
    .await;
    agent_handle.abort();

    // Send SIGINT to any command-based RAP servers we spawned so they
    // run their graceful-shutdown path (e.g. draining background tasks).
    for mut child in spawned_children {
        #[cfg(unix)]
        {
            use nix::sys::signal::{self, Signal};
            use nix::unistd::Pid;
            let _ = signal::kill(Pid::from_raw(child.id() as i32), Signal::SIGINT);
        }
        #[cfg(not(unix))]
        {
            let _ = child.kill();
        }
        let _ = child.wait();
    }

    // Persist store and session on shutdown.
    let final_thread_id = active_thread_id.lock().unwrap().clone();
    let final_tokens = match &result {
        Ok(tokens) => *tokens,
        Err(_) => initial_tokens_used,
    };
    if let Err(e) = conversation_store.save_to_file(store_path) {
        eprintln!("Warning: failed to save conversation store: {}", e);
    }
    let session_json = serde_json::json!({
        "thread_id": final_thread_id,
        "total_tokens_used": final_tokens,
    });
    if let Err(e) = std::fs::write(
        session_path,
        serde_json::to_string_pretty(&session_json).unwrap(),
    ) {
        eprintln!("Warning: failed to save session: {}", e);
    }

    result.map(|_| ())
}

// ── Agent loop ──────────────────────────────────────────────────────────────

#[expect(clippy::too_many_arguments, reason = "internal")]
async fn agent_loop<Mdl>(
    mut rx: mpsc::UnboundedReceiver<(InputMessage, String)>,
    display_tx: mpsc::UnboundedSender<DisplayEvent<Mdl::StreamingResponse>>,
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
            let mut resp = None;
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
                    Ok(event_processor::CompletionEvent::ThinkingStart) => {
                        let _ = display_tx.send(DisplayEvent::ThinkingStart);
                    }
                    Ok(event_processor::CompletionEvent::ThinkingEnd) => {
                        let _ = display_tx.send(DisplayEvent::ThinkingEnd);
                    }
                    Ok(event_processor::CompletionEvent::Action(CompletionAction::Done(r))) => {
                        resp = Some(r);
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
            if any_text && let Some(resp) = resp {
                let _ = display_tx.send(DisplayEvent::ResponseDone(thread_prefix.clone(), resp));
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

// ── Command-based RAP server spawning ───────────────────────────────────────

use infinity_agent_core::tools::config::CommandServerReady;

/// Spawn a RAP server from a shell command with `RAP_EMBEDDED=1`.
///
/// The child process must emit a single JSON line on stdout containing
/// `{"port": <u16>}` once it is ready to accept connections.
/// Returns the child handle (caller is responsible for killing it) and the port.
fn spawn_rap_server(command: &str) -> Result<(std::process::Child, u16), BoxError> {
    use std::io::BufRead;
    use std::process::{Command, Stdio};

    let mut child = Command::new("sh")
        .args(["-c", command])
        .env("RAP_EMBEDDED", "1")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("failed to spawn '{command}': {e}"))?;

    let stdout = child
        .stdout
        .take()
        .ok_or("failed to capture stdout from RAP server")?;

    let mut reader = std::io::BufReader::new(stdout);
    let mut line = String::new();
    reader
        .read_line(&mut line)
        .map_err(|e| format!("failed to read startup line from RAP server: {e}"))?;

    if line.is_empty() {
        // Process exited before printing anything.
        let _ = child.kill();
        return Err("RAP server exited before emitting port JSON".into());
    }

    let ready: CommandServerReady = serde_json::from_str(line.trim())
        .map_err(|e| format!("invalid startup JSON from RAP server: {e} (got: {line})"))?;

    Ok((child, ready.port))
}

/// Scan `$PATH` for executables whose name starts with `rap-` and return
/// their names (not full paths — they're on PATH so the bare name suffices).
fn discover_rap_binaries() -> Vec<String> {
    let path_var = match std::env::var("PATH") {
        Ok(p) => p,
        Err(_) => return Vec::new(),
    };

    let mut seen = std::collections::HashSet::new();
    let mut bins = Vec::new();

    for dir in std::env::split_paths(&path_var) {
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with("rap-") && seen.insert(name.to_string()) {
                // Quick sanity check: must be a file (or symlink to one) and executable.
                if let Ok(meta) = entry.metadata() {
                    if meta.is_file() {
                        bins.push(name.to_string());
                    }
                }
            }
        }
    }

    bins.sort();
    bins
}
