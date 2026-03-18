use rig::client::{CompletionClient, ProviderClient};
use rig::completion::{CompletionModel, GetTokenUsage};
use rig::message::{AssistantContent, Message, ToolResultContent, UserContent};
use rig_bedrock::client::Client as BedrockClient;
use tokio::sync::{mpsc, oneshot};
use tracing_subscriber::EnvFilter;

use clap::Parser;

mod component;
mod inline_viewport;
mod install;
mod mcp_proxy;
mod memory_store;
mod model_picker;
mod modifier_diff;
mod rap_callback;
mod rap_tools;
mod session_picker;
mod session_store;
mod set_title_tool;
mod sleep_tools;
mod terminal;
mod text_input;
mod token_usage;

use infinity_agent_core::batch_processor::DisplayEvent;
use infinity_agent_core::event_processor;
use infinity_agent_core::message::{InputMessage, InputMessageContent};
use infinity_agent_core::tools::config::ToolsConfig;
use infinity_agent_core::tools::sleep::SleepUntilEventOrInputTool;
use infinity_agent_core::tools::thread::{
    CloseThreadTool, ReportToParentTool, SendMessageToChildTool, SpawnThreadTool,
};
use infinity_agent_core::tools::{Tool, ToolContext};
use infinity_agent_core::traits::ConversationStore;
use memory_store::{InMemoryConversationStore, InMemoryMessageSender, InMemoryStateStore};
use model_picker::{ModelEntry, ModelProvider};
use sleep_tools::{SleepTool, SleepUntilTool};

type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// Infinity Agent CLI
#[derive(Parser, Debug)]
#[command(name = "infinity-agent-cli", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,

    /// Model provider to use.
    #[arg(long, value_parser = ["bedrock"])]
    provider: Option<String>,

    /// Send an initial message to the agent on startup.
    #[arg(short, long)]
    message: Option<String>,
}

#[derive(clap::Subcommand, Debug)]
enum Commands {
    /// RAP tool management
    Rap {
        #[command(subcommand)]
        action: RapCommands,
    },
    /// Update the CLI itself
    Update,
}

#[derive(clap::Subcommand, Debug)]
enum RapCommands {
    /// Install a RAP crate and register it in rap.json
    Install {
        /// Install to user-level ~/.infinity/rap.json (required)
        #[arg(long)]
        user: bool,

        /// Crate name to install
        #[arg(long = "crate")]
        crate_name: String,

        /// Git repository URL (passed to cargo install --git)
        #[arg(long)]
        git: Option<String>,

        /// Local path (passed to cargo install --path)
        #[arg(long)]
        path: Option<String>,
    },
    /// Re-install all RAP tools that have a recorded source
    Update {
        /// Update user-level ~/.infinity/rap.json tools (required)
        #[arg(long)]
        user: bool,
    },
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), BoxError> {
    let local = tokio::task::LocalSet::new();
    local.run_until(async_main()).await
}

async fn async_main() -> Result<(), BoxError> {
    std::fs::create_dir_all(".infinity").ok();
    let log_file = std::fs::File::create(".infinity/cli.log").ok();
    if let Some(file) = log_file {
        tracing_subscriber::fmt()
            .with_env_filter(EnvFilter::from_default_env())
            .with_writer(std::sync::Mutex::new(file))
            .with_ansi(false)
            .init();
    }

    let cli = Cli::parse();

    // Handle subcommands
    if let Some(command) = cli.command {
        return match command {
            Commands::Update => install::run_self_update().await,
            Commands::Rap { action } => match action {
                RapCommands::Install {
                    user,
                    crate_name,
                    git,
                    path,
                } => {
                    if !user {
                        return Err("--user is currently required for rap install".into());
                    }
                    install::run_install(install::InstallArgs {
                        crate_name,
                        git,
                        path,
                    })
                    .await
                }
                RapCommands::Update { user } => {
                    if !user {
                        return Err("--user is currently required for rap update".into());
                    }
                    install::run_update().await
                }
            },
        };
    }
    run_with_bedrock(cli.message).await
}

async fn run_with_bedrock(initial_message: Option<String>) -> Result<(), BoxError> {
    let provider = model_picker::BedrockProvider;
    let models = provider.available_models();
    let default_idx = provider.default_model_index();
    let default_model = &models[default_idx];

    let client = BedrockClient::from_env();
    let model = client.completion_model(&default_model.model_id);

    run_agent(model, models, default_idx, None, initial_message).await
}

async fn run_agent<Mdl>(
    model: Mdl,
    models: Vec<ModelEntry>,
    default_model_index: usize,
    startup_info: Option<String>,
    initial_message: Option<String>,
) -> Result<(), BoxError>
where
    Mdl: CompletionModel + 'static,
    Mdl::StreamingResponse: token_usage::WithTotalTokens,
{
    std::fs::create_dir_all(".infinity").ok();

    let default_model = &models[default_model_index];
    let initial_model_name = default_model.display_name.clone();
    let initial_context_window = default_model.context_window;

    let threads_dir = ".infinity/threads";
    let sessions_path = ".infinity/sessions.json";

    let conversation_store = InMemoryConversationStore::new_with_dir(threads_dir);
    let state_store = InMemoryStateStore::new();

    let mut session_store = session_store::SessionStore::load(sessions_path);

    let thread_id = uuid::Uuid::new_v4().to_string();

    let (input_tx, input_rx) = mpsc::unbounded_channel::<(InputMessage, String)>();
    let sender = InMemoryMessageSender::new(input_tx.clone());

    // Shared model config — swapped atomically when the user switches models.
    let active_additional_params: std::sync::Arc<std::sync::RwLock<Option<serde_json::Value>>> =
        std::sync::Arc::new(std::sync::RwLock::new(
            default_model.additional_request_params.clone(),
        ));
    let active_model_id: std::sync::Arc<std::sync::RwLock<Option<String>>> =
        std::sync::Arc::new(std::sync::RwLock::new(None));

    let (display_tx, display_rx) = mpsc::unbounded_channel::<DisplayEvent<_>>();

    if let Some(info) = startup_info {
        let _ = display_tx.send(DisplayEvent::Info(info));
    }

    let agent_store = conversation_store.clone();

    // Spawn a task that boots the agent (callback server, RAP config, tool loading)
    // then enters the agent loop. Info messages go to the terminal via display_tx.
    let agent_display_tx = display_tx.clone();
    let agent_input_tx = input_tx.clone();

    // Start the RAP callback server so tools can POST results back
    let callback_url = rap_callback::start_callback_server(agent_input_tx)
        .await
        .expect("Failed to start RAP callback server");

    // Ensure .infinity/rap.json exists, creating an empty one if needed
    let rap_config_path = ".infinity/rap.json";
    if !std::path::Path::new(rap_config_path).exists() {
        std::fs::create_dir_all(".infinity").ok();
        let default_config = ToolsConfig::empty();
        let json = serde_json::to_string_pretty(&default_config)
            .expect("failed to serialize default rap config");
        std::fs::write(rap_config_path, json).expect("failed to write .infinity/rap.json");
        let _ = agent_display_tx.send(DisplayEvent::Info(
            "Initialized .infinity/rap.json".to_string(),
        ));
    }

    // Merge global ~/.infinity/rap.json into the local config
    let mut merged_config = install::load_config(std::path::Path::new(rap_config_path));
    if let Ok(user_path) = install::user_config_path() {
        if user_path.exists() {
            let user_config = install::load_config(&user_path);
            merged_config.merge(user_config);
            let _ = agent_display_tx.send(DisplayEvent::Info(format!(
                "Merged user config from {}",
                user_path.display()
            )));
        }
    }

    // Load RAP tool servers from config file.
    // Servers can be specified as static URLs or as commands to launch.
    // Command-based servers are spawned with RAP_EMBEDDED=1 and must emit
    // a JSON line `{"port": <u16>}` on stdout when ready.
    let mut spawned_children: Vec<std::process::Child> = Vec::new();
    let tool_server_urls: Vec<String>;

    let rap_tools: Vec<Box<dyn Tool<InMemoryMessageSender>>> = {
        let config = merged_config;
        let mut urls = config.toolset_server_urls();

        // Spawn command-based servers and collect their URLs.
        for cmd in config.toolset_commands() {
            let _ =
                agent_display_tx.send(DisplayEvent::Info(format!("Launching RAP server: {cmd}")));
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

        // Spawn MCP proxy servers and collect their URLs.
        for (name, cmd, env) in config.mcp_servers() {
            let _ = agent_display_tx.send(DisplayEvent::Info(format!(
                "Starting MCP proxy for '{name}'"
            )));
            match mcp_proxy::start_mcp_proxy(name.clone(), cmd, env).await {
                Ok(port) => {
                    let url = format!("http://127.0.0.1:{port}");
                    let _ = agent_display_tx.send(DisplayEvent::Info(format!(
                        "MCP proxy '{name}' ready on port {port}"
                    )));
                    urls.push(url);
                }
                Err(e) => {
                    let _ = agent_display_tx.send(DisplayEvent::Info(format!(
                        "Warning: failed to start MCP proxy '{name}': {e}"
                    )));
                }
            }
        }

        // Start HTTP MCP proxy servers and collect their URLs.
        for (name, mcp_url, headers) in config.http_mcp_servers() {
            let _ = agent_display_tx.send(DisplayEvent::Info(format!(
                "Starting HTTP MCP proxy for '{name}'"
            )));
            match mcp_proxy::start_http_mcp_proxy(name.clone(), mcp_url, headers).await {
                Ok(port) => {
                    let url = format!("http://127.0.0.1:{port}");
                    let _ = agent_display_tx.send(DisplayEvent::Info(format!(
                        "HTTP MCP proxy '{name}' ready on port {port}"
                    )));
                    urls.push(url);
                }
                Err(e) => {
                    let _ = agent_display_tx.send(DisplayEvent::Info(format!(
                        "Warning: failed to start HTTP MCP proxy '{name}': {e}"
                    )));
                }
            }
        }

        tool_server_urls = urls.clone();

        if !urls.is_empty() {
            match rap_tools::load_rap_tools(&urls).await {
                Ok(tools) => {
                    let _ = agent_display_tx.send(DisplayEvent::Info(format!(
                        "Loaded {} RAP tool(s)",
                        tools.len(),
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
    };

    // Build extra system prompt with the CWD so the agent knows where it is.
    let extra_system_prompt = std::env::current_dir().ok().map(|cwd| {
        format!(
            "The user's current working directory is: {}\n\n\
             Use the `set_title` tool to give the current thread a short, descriptive title. \
             Set it once at the start when the user's intent becomes clear, and update it only \
             when the overall scope of work changes significantly. Do not call it repeatedly \
             for minor follow-ups within the same task.",
            cwd.display()
        )
    });

    let agent_additional_params = active_additional_params.clone();
    let agent_model_id = active_model_id.clone();
    let agent_handle = tokio::task::spawn_local(async move {
        agent_loop(
            input_rx,
            agent_display_tx,
            model,
            agent_store,
            state_store,
            sender,
            callback_url,
            rap_tools,
            tool_server_urls,
            extra_system_prompt,
            agent_additional_params,
            agent_model_id,
        )
        .await;
    });

    let (new_session_tx, mut new_session_rx) = mpsc::unbounded_channel::<String>();
    let (load_session_tx, mut load_session_rx) = mpsc::unbounded_channel::<(String, usize)>();
    let (model_switch_tx, mut model_switch_rx) = mpsc::unbounded_channel::<usize>();

    // Track the active thread_id so we can save the latest on shutdown.
    let active_thread_id = std::sync::Arc::new(std::sync::Mutex::new(thread_id.clone()));
    let active_thread_id_for_new = active_thread_id.clone();
    let active_thread_id_for_load = active_thread_id.clone();

    // Listen for new-session signals from the terminal (Ctrl+N).
    tokio::spawn(async move {
        while let Some(new_id) = new_session_rx.recv().await {
            *active_thread_id_for_new.lock().unwrap() = new_id;
        }
    });

    // Listen for load-session requests from the terminal (Ctrl+L → pick).
    let load_display_tx = display_tx.clone();
    let load_conversation_store = conversation_store.clone();
    tokio::spawn(async move {
        while let Some((tid, tokens)) = load_session_rx.recv().await {
            *active_thread_id_for_load.lock().unwrap() = tid.clone();
            // Replay the selected session's history to the display.
            if let Ok((history, _)) = load_conversation_store
                .load_history_with_ancestors(&tid)
                .await
            {
                replay_history(
                    &load_display_tx,
                    &load_conversation_store,
                    &tid,
                    &history,
                    tokens,
                );
            }
        }
    });

    // Listen for model-switch signals from the terminal (Ctrl+M → pick).
    let switch_additional_params = active_additional_params.clone();
    let switch_model_id = active_model_id.clone();
    let switch_models = models.clone();
    tokio::spawn(async move {
        while let Some(idx) = model_switch_rx.recv().await {
            if let Some(entry) = switch_models.get(idx) {
                *switch_additional_params.write().unwrap() =
                    entry.additional_request_params.clone();
                *switch_model_id.write().unwrap() = Some(entry.model_id.clone());
            }
        }
    });

    let sessions_list = session_store.sessions.clone();

    let result = terminal::run(
        input_tx,
        display_rx,
        thread_id,
        initial_model_name,
        initial_context_window,
        new_session_tx,
        sessions_list,
        load_session_tx,
        model_switch_tx,
        models,
        initial_message,
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

    // Persist session metadata on shutdown (conversation history is saved incrementally per-thread).
    let final_thread_id = active_thread_id.lock().unwrap().clone();
    let final_tokens = match &result {
        Ok(tokens) => *tokens,
        Err(_) => 0,
    };
    let final_title = conversation_store.get_title(&final_thread_id);
    // Update the sessions list with the current session's state.
    session_store.upsert(&final_thread_id, final_tokens, final_title);
    if let Err(e) = session_store.save(sessions_path) {
        eprintln!("Warning: failed to save sessions: {}", e);
    }

    result.map(|_| ())
}

// ── History replay helper ───────────────────────────────────────────────────

fn replay_history<R: GetTokenUsage + token_usage::WithTotalTokens>(
    display_tx: &mpsc::UnboundedSender<DisplayEvent<R>>,
    conversation_store: &InMemoryConversationStore,
    thread_id: &str,
    history: &[Message],
    initial_tokens: usize,
) {
    for message in history {
        match message {
            Message::User { content } => match content.first() {
                UserContent::Text(text) => {
                    let _ = display_tx.send(DisplayEvent::UserInput(text.text.clone()));
                    let _ = display_tx.send(DisplayEvent::StartOutput { prefix: None });
                }
                UserContent::ToolResult(res) => {
                    let display_as = conversation_store.get_display_as(thread_id, &res.id);
                    if let ToolResultContent::Text(text) = res.content.first() {
                        let _ = display_tx.send(DisplayEvent::ToolResult {
                            text: text.text,
                            display_as,
                            prefix: None,
                        });
                    }
                }
                _ => {}
            },
            Message::Assistant { content, .. } => match content.first() {
                AssistantContent::Text(text) => {
                    let _ = display_tx.send(DisplayEvent::TextChunk {
                        prefix: None,
                        chunk: text.text,
                    });
                }
                AssistantContent::ToolCall(call) => {
                    let _ = display_tx.send(DisplayEvent::ToolCall {
                        name: call.function.name.clone(),
                        args: call.function.arguments.clone(),
                        prefix: None,
                        display_script: None,
                    });
                }
                _ => {}
            },
        }
    }

    let _ = display_tx.send(DisplayEvent::ResponseDone(
        None,
        Some(R::with_total_tokens(initial_tokens)),
    ));
}

fn is_user_text_input(msg: &InputMessage) -> bool {
    msg.synthetic.is_none()
        && matches!(
            &msg.content,
            InputMessageContent::User(UserContent::Text(_))
        )
}

// ── Agent loop — dispatcher + per-thread workers ────────────────────────────

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
    tool_server_urls: Vec<String>,
    extra_system_prompt: Option<String>,
    additional_request_params: std::sync::Arc<std::sync::RwLock<Option<serde_json::Value>>>,
    active_model_id: std::sync::Arc<std::sync::RwLock<Option<String>>>,
) where
    Mdl: CompletionModel + Send + Sync + 'static,
{
    let rap_notifier = if tool_server_urls.is_empty() {
        None
    } else {
        Some(infinity_agent_core::rap_notifier::RapNotifier::new(
            tool_server_urls,
            rap_tools::SimpleHttpClient::new(),
        ))
    };

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
            rap_notifier: rap_notifier.clone(),
        }),
        Box::new(SendMessageToChildTool {
            conversation_store: conversation_store.clone(),
        }),
        Box::new(
            infinity_agent_core::tools::cancel_subscription::CancelSubscriptionTool {
                state_store: state_store.clone(),
                rap_notifier: rap_notifier.clone(),
            },
        ),
        Box::new(set_title_tool::SetTitleTool {
            conversation_store: conversation_store.clone(),
        }),
    ];
    tool_impls.extend(rap_tools);

    // Shared across all per-thread workers (Tool is Send + Sync).
    let tool_impls: std::sync::Arc<Vec<Box<dyn Tool<InMemoryMessageSender>>>> =
        std::sync::Arc::new(tool_impls);
    let model = std::sync::Arc::new(model);
    let extra_system_prompt = std::sync::Arc::new(extra_system_prompt);

    // One sender per thread-id; lazily created on first message.
    let mut thread_txs: std::collections::HashMap<
        String,
        mpsc::UnboundedSender<(InputMessage, String)>,
    > = std::collections::HashMap::new();

    while let Some((input_msg, message_id)) = rx.recv().await {
        tracing::trace!("Received message {:?}", &input_msg);
        let group_id = input_msg.group_id.clone();

        // Get or create the per-thread channel + worker task.
        let thread_tx = thread_txs.entry(group_id.clone()).or_insert_with(|| {
            let (tx, rx) = mpsc::unbounded_channel();
            tokio::task::spawn_local(thread_worker(
                group_id,
                rx,
                display_tx.clone(),
                model.clone(),
                conversation_store.clone(),
                state_store.clone(),
                sender.clone(),
                callback_url.clone(),
                tool_impls.clone(),
                extra_system_prompt.as_ref().clone(),
                rap_notifier.clone(),
                additional_request_params.clone(),
                active_model_id.clone(),
            ));
            tx
        });

        thread_tx.send((input_msg, message_id)).unwrap();
    }
}

/// Per-thread worker: processes messages for a single thread ID sequentially,
/// but different threads run concurrently with each other.
async fn thread_worker<Mdl>(
    active_group_id: String,
    mut rx: mpsc::UnboundedReceiver<(InputMessage, String)>,
    display_tx: mpsc::UnboundedSender<DisplayEvent<Mdl::StreamingResponse>>,
    model: std::sync::Arc<Mdl>,
    conversation_store: InMemoryConversationStore,
    state_store: InMemoryStateStore,
    sender: InMemoryMessageSender,
    callback_url: String,
    tool_impls: std::sync::Arc<Vec<Box<dyn Tool<InMemoryMessageSender>>>>,
    extra_system_prompt: Option<String>,
    rap_notifier: Option<
        infinity_agent_core::rap_notifier::RapNotifier<rap_tools::SimpleHttpClient>,
    >,
    additional_request_params: std::sync::Arc<std::sync::RwLock<Option<serde_json::Value>>>,
    active_model_id: std::sync::Arc<std::sync::RwLock<Option<String>>>,
) where
    Mdl: CompletionModel + Send + Sync + 'static,
{
    let current_history = std::cell::RefCell::new(
        match event_processor::HistoryManager::new_with_history(
            conversation_store.clone(),
            state_store.clone(),
            active_group_id.clone(),
        )
        .await
        {
            Ok(h) => h,
            Err(e) => {
                let _ = display_tx.send(DisplayEvent::Info(format!("Error: {}", e)));
                return;
            }
        },
    );

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

    let tool_context = ToolContext {
        message_sender: sender.clone(),
        group_id: active_group_id.clone(),
        input_queue_arn: String::new(),
        callback_url: callback_url.clone(),
        user_id: None,
        thread_stack: current_history.borrow().get_thread_stack(),
    };
    let tool_registry: std::collections::HashMap<String, &dyn Tool<InMemoryMessageSender>> =
        tool_impls
            .iter()
            .map(|t| (t.name().to_string(), t.as_ref()))
            .collect();

    let mut pending_non_interrupt_items = vec![];

    {
        let mut completion_fut = None;
        let mut completion_cancel_tx: Option<oneshot::Sender<()>> = None;

        loop {
            let inputs_before_pending = if let Some(mut_fut) = completion_fut.as_mut() {
                tokio::select! {
                    _ = mut_fut => {
                        // if the LLM completed first, simply loop back and collect a batch in the else branch
                        let _ = completion_fut.take().unwrap();
                        continue;
                    },
                    first = rx.recv() => {
                        let Some(first) = first else {
                            return;
                        };
                        // Drain all immediately-available events before running the LLM.
                        let mut batch = vec![first];
                        while let Ok(item) = rx.try_recv() {
                            batch.push(item);
                        }

                        if batch.iter().any(|(msg, _)| is_user_text_input(msg))
                        {
                            let _ = completion_cancel_tx.take().unwrap().send(());
                            let completion_fut_taken = completion_fut.take().unwrap();
                            completion_fut_taken.await;

                            let (mut user_inputs, non_user_inputs): (Vec<_>, Vec<_>) = batch
                                .into_iter()
                                .partition(|(msg, _)| is_user_text_input(msg));

                            if let InputMessageContent::User(UserContent::Text(text)) = &mut user_inputs[0].0.content {
                                text.text = format!("<interrupt>{}", text.text);
                            } else {
                                panic!("user_inputs should only have user text");
                            }

                            pending_non_interrupt_items.extend(non_user_inputs);
                            user_inputs
                        } else {
                            pending_non_interrupt_items.extend(batch);
                            // if nothing is an interrupt-causing event, simply loop back and continue waiting
                            // for a real interrupt or the LLM to complete naturally
                            continue;
                        }
                    }
                }
            } else {
                let mut batch = vec![];

                if pending_non_interrupt_items.is_empty() {
                    // only block if there are no pending items
                    let first = rx.recv().await;
                    let Some(first) = first else {
                        return;
                    };
                    batch.push(first);
                }

                while let Ok(item) = rx.try_recv() {
                    batch.push(item);
                }

                batch
            };

            let all_inputs: Vec<_> = inputs_before_pending
                .into_iter()
                .chain(pending_non_interrupt_items.drain(..))
                .collect();

            // Save display_as for CLI persistence before shared processing
            for (input_msg, _) in &all_inputs {
                if let Some(ref da) = input_msg.display_as {
                    if let InputMessageContent::User(UserContent::ToolResult(ref res)) =
                        input_msg.content
                    {
                        conversation_store.save_display_as(&active_group_id, &res.id, da);
                    }
                }
            }

            let current_additional_params = additional_request_params.read().unwrap().clone();
            let current_model_id = active_model_id.read().unwrap().clone();

            let result = infinity_agent_core::batch_processor::process_batch(
                all_inputs.into_iter(),
                &current_history,
                &conversation_store,
                &display_tx,
                &active_group_id,
                model.as_ref(),
                &tool_names,
                &tool_defs,
                &tool_registry,
                tool_context.clone(),
                &extra_system_prompt,
                current_additional_params,
                current_model_id,
                rap_notifier.as_ref(),
            )
            .await;

            if let Some((fut, cancel_tx)) = result {
                completion_cancel_tx = Some(cancel_tx);
                completion_fut = Some(fut);
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
        .current_dir(".infinity")
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

