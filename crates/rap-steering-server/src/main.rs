use std::path::Path;
use std::sync::Arc;

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post};
use axum::{Json, Router};
use clap::Parser;
use tracing_subscriber::EnvFilter;

use rap_protocol::{
    PlainCallbackClient, RapInvocation, ToolDef, ToolsetManifest, send_tool_result,
};
use rap_steering_server::{
    ListSteeringArgs, LoadSteeringArgs, list_steering_files, load_steering_file,
};

#[derive(Parser)]
#[command(about = "Steering file discovery RAP server")]
struct Args {
    #[arg(short, long, default_value_t = 3002)]
    port: u16,
}

struct AppState {
    callback_client: PlainCallbackClient,
}

fn build_manifest(endpoint: &str) -> ToolsetManifest {
    ToolsetManifest {
        name: "steering-tools".to_string(),
        description: Some("Discover and load project steering files (CLAUDE.md, AGENTS.md, .kiro/steering/, etc.)".to_string()),
        endpoint: endpoint.to_string(),
        needs_migration: false,
        tools: vec![
            ToolDef {
                name: "list_steering".to_string(),
                description: "List all known steering/instruction files found in a project root. Scans for CLAUDE.md, AGENTS.md, .kiro/steering/, .cursorrules, .github/copilot-instructions.md, .windsurfrules, CONVENTIONS.md, .cursor/rules/, .ai/rules/, etc. Returns deduplicated relative paths (symlinks resolved).".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "root": {
                            "type": "string",
                            "description": "Absolute path to the project root"
                        }
                    },
                    "required": ["root"]
                }),
                annotations: None,
                display_script: None,
            },
            ToolDef {
                name: "load_steering".to_string(),
                description: "Load the content of a steering file. The path must be relative to the project root and must be a file discoverable by list_steering.".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "root": {
                            "type": "string",
                            "description": "Absolute path to the project root"
                        },
                        "path": {
                            "type": "string",
                            "description": "Relative path to the steering file (as returned by list_steering)"
                        }
                    },
                    "required": ["root", "path"]
                }),
                annotations: None,
                display_script: Some(r#""Load " + args.path"#.to_string()),
            },
        ],
    }
}

async fn toolset_handler(headers: HeaderMap) -> Json<ToolsetManifest> {
    let host = headers
        .get("host")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("localhost");
    let scheme = if host.starts_with("localhost") || host.starts_with("127.0.0.1") {
        "http"
    } else {
        "https"
    };
    Json(build_manifest(&format!("{scheme}://{host}/invoke")))
}

async fn invoke_handler(
    State(state): State<Arc<AppState>>,
    Json(invocation): Json<RapInvocation>,
) -> StatusCode {
    tokio::spawn(rap_protocol::log_panic(
        "steering_server_invoke",
        async move {
            let text = match invocation.operation.as_str() {
                "list_steering" => handle_list_steering(&invocation).await,
                "load_steering" => handle_load_steering(&invocation).await,
                _ => Err(format!("unknown operation: {}", invocation.operation)),
            };

            let text = match text {
                Ok(t) => t,
                Err(e) => format!("Error: {e}"),
            };

            send_tool_result(&state.callback_client, &invocation, &text, None, false).await;
        },
    ));

    StatusCode::OK
}

async fn handle_list_steering(invocation: &RapInvocation) -> Result<String, String> {
    let args: ListSteeringArgs = serde_json::from_value(invocation.arguments.clone())
        .map_err(|e| format!("invalid arguments: {e}"))?;

    let files = list_steering_files(Path::new(&args.root)).await?;

    if files.is_empty() {
        return Ok("No steering files found.".to_string());
    }

    Ok(files.join("\n"))
}

async fn handle_load_steering(invocation: &RapInvocation) -> Result<String, String> {
    let args: LoadSteeringArgs = serde_json::from_value(invocation.arguments.clone())
        .map_err(|e| format!("invalid arguments: {e}"))?;

    load_steering_file(Path::new(&args.root), &args.path).await
}

fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let log_file =
        std::fs::File::create("./rap-steering.log").expect("failed to create rap-steering.log");

    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .with_writer(std::sync::Mutex::new(log_file))
        .with_ansi(false)
        .init();

    let args = Args::parse();

    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(async {
            let state = Arc::new(AppState {
                callback_client: PlainCallbackClient::new(),
            });

            let app = Router::new()
                .route("/.well-known/rap-toolset", get(toolset_handler))
                .route("/invoke", post(invoke_handler))
                .with_state(state);

            let embedded = std::env::var("RAP_EMBEDDED").is_ok();
            let bind_port = if embedded { 0 } else { args.port };
            let listener = tokio::net::TcpListener::bind(format!("0.0.0.0:{bind_port}")).await?;
            let actual_port = listener.local_addr()?.port();

            if embedded {
                println!("{}", serde_json::json!({ "port": actual_port }));
            }

            tracing::info!("steering RAP server listening on port {actual_port}");

            axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    tokio::signal::ctrl_c()
                        .await
                        .expect("failed to listen for ctrl+c");
                })
                .await?;

            Ok(())
        })
}
