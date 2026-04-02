use std::sync::Arc;

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post};
use axum::{Json, Router};
use clap::Parser;
use tracing_subscriber::EnvFilter;

use rap_github_event_poller::Poller;
use rap_protocol::{
    PlainCallbackClient, RapInvocation, ToolDef, ToolsetManifest, send_tool_result,
};

#[derive(Parser)]
#[command(about = "GitHub event poller RAP server (local, no webhooks needed)")]
struct Args {
    #[arg(short, long, default_value_t = 3003)]
    port: u16,
}

type AppState = Poller<PlainCallbackClient>;

fn build_manifest(endpoint: &str) -> ToolsetManifest {
    ToolsetManifest {
        name: "github-events".to_string(),
        description: Some("Subscribe to GitHub repository events via polling (no webhooks required)"
            .to_string()),
        endpoint: endpoint.to_string(),
        tools: vec![ToolDef {
            name: "subscribe_github_events".to_string(),
            description:
                "Subscribes to GitHub events on a repository via polling. Use filters to match specific events. If there is nothing to do until an event arrives, you may want to use the sleep tool to hibernate until you are woken up by an event. DO NOT re-subscribe after an `interrupt`, the subscription remains active automatically."
                    .to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "owner": {
                        "type": "string",
                        "description": "GitHub repository owner (username or organization)."
                    },
                    "repo": {
                        "type": "string",
                        "description": "GitHub repository name."
                    },
                    "event_type": {
                        "type": "string",
                        "description": "Optional: GitHub event type to filter on (e.g., \"PullRequestEvent\", \"PushEvent\", \"IssuesEvent\"). Note: the Events API uses PascalCase event type names."
                    },
                    "sha": {
                        "type": "string",
                        "description": "Optional: Commit SHA to filter on."
                    },
                    "pr_number": {
                        "type": "number",
                        "description": "Optional: Pull request number to filter on."
                    },
                    "issue_number": {
                        "type": "number",
                        "description": "Optional: Issue number to filter on."
                    },
                    "action": {
                        "type": "string",
                        "description": "Optional: Event action to filter on (e.g., \"opened\", \"closed\")."
                    },
                    "branch": {
                        "type": "string",
                        "description": "Optional: Branch name to filter on."
                    },
                    "actor": {
                        "type": "string",
                        "description": "Optional: GitHub username to filter on."
                    }
                },
                "required": ["owner", "repo"]
            }),
            annotations: None,
            display_script: None,
        }],
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
        "github_event_poller_invoke",
        async move {
            let text = state.subscribe(&invocation).await;
            send_tool_result(
                &PlainCallbackClient::new(),
                &invocation,
                &text,
                None,
                true, // this is a subscription
            )
            .await;
        },
    ));
    StatusCode::OK
}

#[derive(serde::Deserialize)]
struct CancelRequest {
    tool_call_id: String,
}

async fn cancel_handler(
    State(state): State<Arc<AppState>>,
    Json(req): Json<CancelRequest>,
) -> StatusCode {
    state.cancel(&req.tool_call_id).await;
    StatusCode::OK
}

fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let log_file =
        std::fs::File::create("./rap-github-event-poller.log").expect("failed to create log file");

    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .with_writer(std::sync::Mutex::new(log_file))
        .with_ansi(false)
        .init();

    let args = Args::parse();

    let github_token = std::env::var("GITHUB_TOKEN").ok();
    if github_token.is_none() {
        eprintln!("warning: GITHUB_TOKEN not set; API rate limits will be very low (60 req/hr)");
    }

    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(async {
            let poller = Arc::new(Poller::new(PlainCallbackClient::new(), github_token));

            // Spawn the background polling loop
            let poller_bg = Arc::clone(&poller);
            tokio::spawn(rap_protocol::log_panic(
                "github_event_poller_bg",
                async move {
                    poller_bg.run().await;
                },
            ));

            let app = Router::new()
                .route("/.well-known/rap-toolset", get(toolset_handler))
                .route("/invoke", post(invoke_handler))
                .route("/cancel_tool_call", post(cancel_handler))
                .with_state(poller);

            let embedded = std::env::var("RAP_EMBEDDED").is_ok();
            let bind_port = if embedded { 0 } else { args.port };
            let listener = tokio::net::TcpListener::bind(format!("0.0.0.0:{bind_port}")).await?;
            let actual_port = listener.local_addr()?.port();

            if embedded {
                println!("{}", serde_json::json!({ "port": actual_port }));
            }

            tracing::info!("github-event-poller RAP server listening on port {actual_port}");
            eprintln!("github-event-poller RAP server listening on port {actual_port}");

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
