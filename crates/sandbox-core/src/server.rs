use std::sync::Arc;

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tracing;

use crate::callback::CallbackClient;
use crate::error::SandboxError;
use crate::jj::run_jj;
use crate::metadata::MetadataStore;
use crate::sandbox::SandboxBackend;
use crate::types::{CloneRepoArgs, ExecuteCommandArgs, RepoState};

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct RapInvocation {
    operation: String,
    arguments: serde_json::Value,
    id: String,
    call_id: Option<String>,
    callback_url: String,
    group_id: String,
    user_id: Option<String>,
}

#[derive(Debug, Serialize)]
struct RapToolResult {
    r#type: String,
    group_id: String,
    id: String,
    call_id: Option<String>,
    text: String,
}

#[derive(Serialize)]
struct ToolsetManifest {
    name: String,
    description: String,
    endpoint: String,
    tools: Vec<ToolDef>,
}

#[derive(Serialize)]
struct ToolDef {
    name: String,
    description: String,
    #[serde(rename = "inputSchema")]
    input_schema: serde_json::Value,
}

type PendingTasks = Arc<Mutex<Vec<JoinHandle<()>>>>;

struct AppState<B: SandboxBackend, M: MetadataStore, C: CallbackClient> {
    backend: B,
    metadata: M,
    callback_client: C,
    pending_tasks: PendingTasks,
}

/// Shared handle to pending background tasks.
#[derive(Clone)]
pub struct TaskTracker {
    pending_tasks: PendingTasks,
}

impl TaskTracker {
    pub async fn drain(&self) {
        let tasks: Vec<JoinHandle<()>> = {
            let mut pending = self.pending_tasks.lock().await;
            std::mem::take(&mut *pending)
        };
        for handle in tasks {
            if let Err(e) = handle.await {
                tracing::error!("background task panicked: {e}");
            }
        }
    }
}

/// Build the axum Router for the sandbox RAP server.
pub fn build_router<B, M, C>(backend: B, metadata: M, callback_client: C) -> (Router, TaskTracker)
where
    B: SandboxBackend + 'static,
    M: MetadataStore + 'static,
    C: CallbackClient + 'static,
{
    let pending_tasks: PendingTasks = Arc::new(Mutex::new(Vec::new()));

    let state = Arc::new(AppState {
        backend,
        metadata,
        callback_client,
        pending_tasks: pending_tasks.clone(),
    });

    let tracker = TaskTracker { pending_tasks };

    let router = Router::new()
        .route("/.well-known/rap-toolset", get(toolset_handler::<B, M, C>))
        .route("/invoke", post(invoke_handler::<B, M, C>))
        .with_state(state);

    (router, tracker)
}

async fn toolset_handler<
    B: SandboxBackend + 'static,
    M: MetadataStore + 'static,
    C: CallbackClient + 'static,
>(
    headers: HeaderMap,
    State(_state): State<Arc<AppState<B, M, C>>>,
) -> Json<ToolsetManifest> {
    let host = headers
        .get("host")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("localhost");
    let scheme = if host.starts_with("localhost") || host.starts_with("127.0.0.1") {
        "http"
    } else {
        "https"
    };
    let endpoint = format!("{scheme}://{host}/invoke");
    Json(build_manifest(&endpoint))
}

async fn invoke_handler<
    B: SandboxBackend + 'static,
    M: MetadataStore + 'static,
    C: CallbackClient + 'static,
>(
    State(state): State<Arc<AppState<B, M, C>>>,
    Json(invocation): Json<RapInvocation>,
) -> StatusCode {
    let state_clone = state.clone();
    let handle = tokio::spawn(async move {
        let result_text = match invocation.operation.as_str() {
            "clone_repo" => handle_clone_repo(&state_clone, &invocation).await,
            "execute_command" => handle_execute_command(&state_clone, &invocation).await,
            _ => Err(SandboxError::Other(format!(
                "unknown operation: {}",
                invocation.operation
            ))),
        };

        let text = match result_text {
            Ok(t) => t,
            Err(e) => format!("Error: {e}"),
        };

        let tool_result = RapToolResult {
            r#type: "tool_result".to_string(),
            group_id: invocation.group_id.clone(),
            id: invocation.id.clone(),
            call_id: invocation.call_id.clone(),
            text,
        };

        let body = serde_json::to_string(&tool_result).unwrap();
        if let Err(e) = state_clone
            .callback_client
            .post_json(&invocation.callback_url, &body)
            .await
        {
            tracing::error!("failed to send tool result to callback: {e}");
        }
    });

    state.pending_tasks.lock().await.push(handle);

    StatusCode::OK
}

async fn handle_clone_repo<B: SandboxBackend, M: MetadataStore, C: CallbackClient>(
    state: &AppState<B, M, C>,
    invocation: &RapInvocation,
) -> Result<String, SandboxError> {
    let args: CloneRepoArgs = serde_json::from_value(invocation.arguments.clone())
        .map_err(|e| SandboxError::Other(format!("invalid arguments: {e}")))?;

    let remote_uri = state
        .backend
        .init_repo(&args.repo, &invocation.group_id)
        .await?;

    let repo_state = RepoState {
        group_id: invocation.group_id.clone(),
        remote_uri: remote_uri.clone(),
        bookmark: None,
    };
    state.metadata.put(&repo_state).await?;

    tracing::info!(group_id = %invocation.group_id, remote = %remote_uri, "repo cloned");
    Ok("Repository initialized.".to_string())
}

async fn handle_execute_command<B: SandboxBackend, M: MetadataStore, C: CallbackClient>(
    state: &AppState<B, M, C>,
    invocation: &RapInvocation,
) -> Result<String, SandboxError> {
    let args: ExecuteCommandArgs = serde_json::from_value(invocation.arguments.clone())
        .map_err(|e| SandboxError::Other(format!("invalid arguments: {e}")))?;

    let repo_state = state
        .metadata
        .get(&invocation.group_id)
        .await?
        .ok_or_else(|| SandboxError::RepoNotFound(invocation.group_id.clone()))?;

    let sandbox_dir = state.backend.create_sandbox(&repo_state).await?;

    run_jj(&sandbox_dir, &["describe", "-m", &args.command]).await?;

    let exec_result = state
        .backend
        .execute_command(&sandbox_dir, &args.command)
        .await?;

    let stdout = exec_result.stdout;
    let stderr = exec_result.stderr;
    let exit_code = exec_result.exit_code;

    state
        .backend
        .push_sandbox(&sandbox_dir, &invocation.group_id)
        .await?;

    let bookmark = format!("sandbox-{}", invocation.group_id);
    let updated_state = RepoState {
        group_id: invocation.group_id.clone(),
        remote_uri: repo_state.remote_uri.clone(),
        bookmark: Some(bookmark.clone()),
    };
    state.metadata.put(&updated_state).await?;

    if let Err(e) = state.backend.cleanup_sandbox(&sandbox_dir).await {
        tracing::warn!("failed to cleanup sandbox: {e}");
    }

    tracing::info!(group_id = %invocation.group_id, bookmark = %bookmark, exit_code, "command executed");

    let mut result = String::new();
    if !stdout.is_empty() {
        result.push_str(&stdout);
    }
    if !stderr.is_empty() {
        if !result.is_empty() {
            result.push('\n');
        }
        result.push_str("[stderr]\n");
        result.push_str(&stderr);
    }
    if result.is_empty() {
        result = format!("Command completed with exit code {exit_code}");
    } else {
        result.push_str(&format!("\n[exit code: {exit_code}]"));
    }
    Ok(result)
}

fn build_manifest(endpoint: &str) -> ToolsetManifest {
    ToolsetManifest {
        name: "sandbox-tools".to_string(),
        description: "Sandboxed code editing and execution tools using jujutsu for filesystem versioning".to_string(),
        endpoint: endpoint.to_string(),
        tools: vec![
            ToolDef {
                name: "clone_repo".to_string(),
                description: "Initialize a repository for sandboxed execution. Provide a local path to a git repo (local mode) or a git remote URI (remote mode). This must be called before execute_command.".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "repo": {
                            "type": "string",
                            "description": "Local path to a git repository, or a git remote URI"
                        }
                    },
                    "required": ["repo"]
                }),
            },
            ToolDef {
                name: "execute_command".to_string(),
                description: "Execute a bash command in a sandboxed copy of the repository. The sandbox is an isolated temporary directory with the repo's current state. All filesystem changes are tracked and persisted via jujutsu.".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "command": {
                            "type": "string",
                            "description": "The bash command to execute in the sandbox"
                        }
                    },
                    "required": ["command"]
                }),
            },
        ],
    }
}
