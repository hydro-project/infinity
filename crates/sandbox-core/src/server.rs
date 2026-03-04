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
use crate::types::{
    CloneRepoArgs, DescribeEditsArgs, EditFileArgs, ExecuteCommandArgs, GrepArgs, ReadFileArgs,
    RepoState,
};

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
    #[serde(skip_serializing_if = "Option::is_none")]
    display_as: Option<String>,
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
        .route("/close_thread", post(close_thread_handler::<B, M, C>))
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
            "clone_repo" => handle_clone_repo(&state_clone, &invocation)
                .await
                .map(|t| (t, None)),
            "execute_command" => handle_execute_command(&state_clone, &invocation).await,
            "read_file" => handle_read_file(&state_clone, &invocation).await,
            "edit_file" => handle_edit_file(&state_clone, &invocation).await,
            "grep" => handle_grep(&state_clone, &invocation).await,
            "describe_edits" => handle_describe_edits(&state_clone, &invocation).await,
            _ => Err(SandboxError::Other(format!(
                "unknown operation: {}",
                invocation.operation
            ))),
        };

        let (text, display_as) = match result_text {
            Ok((t, d)) => (t, d),
            Err(e) => (format!("Error: {e}"), None),
        };

        let tool_result = RapToolResult {
            r#type: "tool_result".to_string(),
            group_id: invocation.group_id.clone(),
            id: invocation.id.clone(),
            call_id: invocation.call_id.clone(),
            text,
            display_as,
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

/// Request payload for the `/close_thread` RAP protocol endpoint.
#[derive(Debug, Deserialize)]
struct CloseThreadRequest {
    thread_id: String,
}

/// Best-effort notification endpoint for thread closure.
///
/// When the leader closes a thread it POSTs `{"thread_id": "..."}` to every
/// tool server. The server uses this to clean up thread-specific resources
/// (e.g. cached sandboxes). The response is always 200 OK — the leader does
/// not retry on failure, and tool servers are free to ignore this entirely.
async fn close_thread_handler<
    B: SandboxBackend + 'static,
    M: MetadataStore + 'static,
    C: CallbackClient + 'static,
>(
    State(state): State<Arc<AppState<B, M, C>>>,
    Json(request): Json<CloseThreadRequest>,
) -> StatusCode {
    let thread_id = request.thread_id;
    tracing::info!(thread_id = %thread_id, "received close_thread notification");

    let state_clone = state.clone();
    let handle = tokio::spawn(async move {
        if let Err(e) = state_clone
            .backend
            .cleanup_sandbox_permanently(&thread_id)
            .await
        {
            tracing::warn!(
                thread_id = %thread_id,
                "failed to permanently clean up sandbox: {e}"
            );
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

/// Run an action inside a sandbox: create → action → push → update metadata → cleanup.
///
/// The closure receives the sandbox directory path and returns `(text, display_as)`.
/// `text` is the full result sent to the model; `display_as` is an optional short
/// summary shown in the CLI instead of the full text.
async fn with_sandbox<B, M, C, F, Fut>(
    state: &AppState<B, M, C>,
    group_id: &str,
    jj_description: &str,
    action: F,
) -> Result<(String, Option<String>), SandboxError>
where
    B: SandboxBackend,
    M: MetadataStore,
    C: CallbackClient,
    F: FnOnce(std::path::PathBuf) -> Fut,
    Fut: std::future::Future<Output = Result<(String, Option<String>), SandboxError>>,
{
    let repo_state = state
        .metadata
        .get(group_id)
        .await?
        .ok_or_else(|| SandboxError::RepoNotFound(group_id.to_string()))?;

    let sandbox_dir = state.backend.create_sandbox(&repo_state).await?;

    run_jj(&sandbox_dir, &["describe", "-m", jj_description]).await?;

    let result = action(sandbox_dir.clone()).await;

    state.backend.push_sandbox(&sandbox_dir, group_id).await?;

    let bookmark = format!("sandbox-{}", group_id);
    let updated_state = RepoState {
        group_id: group_id.to_string(),
        remote_uri: repo_state.remote_uri.clone(),
        bookmark: Some(bookmark.clone()),
    };
    state.metadata.put(&updated_state).await?;

    if let Err(e) = state.backend.cleanup_sandbox(&sandbox_dir).await {
        tracing::warn!("failed to cleanup sandbox: {e}");
    }

    tracing::info!(group_id = %group_id, bookmark = %bookmark, "sandbox operation complete");

    result
}

async fn handle_execute_command<B: SandboxBackend, M: MetadataStore, C: CallbackClient>(
    state: &AppState<B, M, C>,
    invocation: &RapInvocation,
) -> Result<(String, Option<String>), SandboxError> {
    let args: ExecuteCommandArgs = serde_json::from_value(invocation.arguments.clone())
        .map_err(|e| SandboxError::Other(format!("invalid arguments: {e}")))?;

    let command = args.command.clone();
    let backend = &state.backend;

    with_sandbox(
        state,
        &invocation.group_id,
        &args.command,
        |sandbox_dir| async move {
            let exec_result = backend.execute_command(&sandbox_dir, &command).await?;

            let stdout = exec_result.stdout;
            let stderr = exec_result.stderr;
            let exit_code = exec_result.exit_code;

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
            Ok((result, None))
        },
    )
    .await
}

async fn handle_read_file<B: SandboxBackend, M: MetadataStore, C: CallbackClient>(
    state: &AppState<B, M, C>,
    invocation: &RapInvocation,
) -> Result<(String, Option<String>), SandboxError> {
    let args: ReadFileArgs = serde_json::from_value(invocation.arguments.clone())
        .map_err(|e| SandboxError::Other(format!("invalid arguments: {e}")))?;

    let description = format!("read_file: {}", args.path);

    with_sandbox(
        state,
        &invocation.group_id,
        &description,
        |sandbox_dir| async move {
            let file_path = sandbox_dir.join(&args.path);
            let content = tokio::fs::read_to_string(&file_path)
                .await
                .map_err(|e| SandboxError::Io(e))?;

            let lines: Vec<&str> = content.lines().collect();
            let total_lines = lines.len();

            let start = args.start_line.unwrap_or(1).max(1);
            let end = args.end_line.unwrap_or(total_lines).min(total_lines);

            if start > total_lines {
                let msg = format!("File has {total_lines} lines, but start_line is {start}");
                return Ok((msg.clone(), Some(msg)));
            }

            let selected: Vec<String> = lines[start - 1..end]
                .iter()
                .enumerate()
                .map(|(i, line)| format!("{:>4}  {}", start + i, line))
                .collect();

            let read_count = end - start + 1;
            let display = if start == 1 && end == total_lines {
                format!("Read {} ({} lines)", args.path, total_lines)
            } else {
                format!(
                    "Read {} lines of {} (lines {}-{})",
                    read_count, args.path, start, end
                )
            };

            let text = format!(
                "<file name=\"{}\" lines=\"{total_lines}\">\n{}\n</file>",
                args.path,
                selected.join("\n")
            );

            Ok((text, Some(display)))
        },
    )
    .await
}

async fn handle_edit_file<B: SandboxBackend, M: MetadataStore, C: CallbackClient>(
    state: &AppState<B, M, C>,
    invocation: &RapInvocation,
) -> Result<(String, Option<String>), SandboxError> {
    let args: EditFileArgs = serde_json::from_value(invocation.arguments.clone())
        .map_err(|e| SandboxError::Other(format!("invalid arguments: {e}")))?;

    let description = format!("edit_file: {}", args.path);

    with_sandbox(
        state,
        &invocation.group_id,
        &description,
        |sandbox_dir| async move {
            let file_path = sandbox_dir.join(&args.path);
            let content = tokio::fs::read_to_string(&file_path)
                .await
                .map_err(|e| SandboxError::Io(e))?;

            let matches: Vec<_> = content.match_indices(&args.old_str).collect();

            if matches.is_empty() {
                return Err(SandboxError::Other(format!(
                    "old_str not found in {}",
                    args.path
                )));
            }
            if matches.len() > 1 {
                return Err(SandboxError::Other(format!(
                    "old_str matches {} locations in {} — must be unique",
                    matches.len(),
                    args.path
                )));
            }

            let new_content = content.replacen(&args.old_str, &args.new_str, 1);
            tokio::fs::write(&file_path, &new_content)
                .await
                .map_err(|e| SandboxError::Io(e))?;

            // Build a unified diff for display
            let display = build_edit_diff(&args.path, &args.old_str, &args.new_str);

            Ok((format!("Replaced text in {}", args.path), Some(display)))
        },
    )
    .await
}

async fn handle_grep<B: SandboxBackend, M: MetadataStore, C: CallbackClient>(
    state: &AppState<B, M, C>,
    invocation: &RapInvocation,
) -> Result<(String, Option<String>), SandboxError> {
    let args: GrepArgs = serde_json::from_value(invocation.arguments.clone())
        .map_err(|e| SandboxError::Other(format!("invalid arguments: {e}")))?;

    let description = format!("grep: {}", args.query);
    let query_for_display = args.query.clone();
    let backend = &state.backend;

    // Verify that ripgrep is installed before creating a sandbox.
    if which::which("rg").is_err() {
        return Err(SandboxError::Other(
            "ripgrep (rg) is not installed or not found in PATH. \
             Please install it: https://github.com/BurntSushi/ripgrep#installation"
                .to_string(),
        ));
    }

    with_sandbox(
        state,
        &invocation.group_id,
        &description,
        |sandbox_dir| async move {
            let mut cmd_parts = vec!["rg".to_string(), "--line-number".to_string()];

            // Context lines
            cmd_parts.push("-C".to_string());
            cmd_parts.push("2".to_string());

            // Max count to avoid huge output
            cmd_parts.push("--max-count".to_string());
            cmd_parts.push("50".to_string());

            if args.case_sensitive != Some(true) {
                cmd_parts.push("--ignore-case".to_string());
            }

            if let Some(ref pattern) = args.include_pattern {
                cmd_parts.push("--glob".to_string());
                cmd_parts.push(pattern.clone());
            }

            if let Some(ref pattern) = args.exclude_pattern {
                cmd_parts.push("--glob".to_string());
                cmd_parts.push(format!("!{pattern}"));
            }

            cmd_parts.push("--".to_string());
            cmd_parts.push(args.query.clone());

            let command = cmd_parts
                .iter()
                .map(|p| {
                    if p.contains(' ') || p.contains('\'') || p.contains('"') || p.contains('\\') {
                        format!("'{}'", p.replace('\'', "'\\''"))
                    } else {
                        p.clone()
                    }
                })
                .collect::<Vec<_>>()
                .join(" ");

            let exec_result = backend.execute_command(&sandbox_dir, &command).await?;

            if exec_result.stdout.is_empty() && exec_result.exit_code == 1 {
                let display = format!("Searched for '{}' — no matches", query_for_display);
                return Ok(("No matches found.".to_string(), Some(display)));
            }

            let stdout = &exec_result.stdout;

            // Build summary: count matching files and match lines
            let mut files = std::collections::HashSet::new();
            let mut match_count = 0usize;
            for line in stdout.lines() {
                // Match lines have the format "file:line:content" (colon after line number)
                // Context lines have "file-line-content" (dash after line number)
                if let Some(colon1) = line.find(':') {
                    let rest = &line[colon1 + 1..];
                    if let Some(colon2) = rest.find(':') {
                        if rest[..colon2].parse::<usize>().is_ok() {
                            files.insert(&line[..colon1]);
                            match_count += 1;
                        }
                    }
                }
            }

            let display = format!(
                "Searched for '{}' — {} match(es) across {} file(s)",
                query_for_display,
                match_count,
                files.len()
            );

            let mut output = exec_result.stdout;
            if !exec_result.stderr.is_empty() {
                output.push_str("\n[stderr]\n");
                output.push_str(&exec_result.stderr);
            }
            Ok((output, Some(display)))
        },
    )
    .await
}

async fn handle_describe_edits<B: SandboxBackend, M: MetadataStore, C: CallbackClient>(
    state: &AppState<B, M, C>,
    invocation: &RapInvocation,
) -> Result<(String, Option<String>), SandboxError> {
    let args: DescribeEditsArgs = serde_json::from_value(invocation.arguments.clone())
        .map_err(|e| SandboxError::Other(format!("invalid arguments: {e}")))?;

    with_sandbox(
        state,
        &invocation.group_id,
        &args.message,
        |_sandbox_dir| async move { Ok(("Edits described.".to_string(), None)) },
    )
    .await
}

/// Build a pretty-printed unified diff for display in the CLI.
fn build_edit_diff(path: &str, old_str: &str, new_str: &str) -> String {
    use similar::{ChangeTag, TextDiff};

    let diff = TextDiff::from_lines(old_str, new_str);
    let mut out = format!("--- {}\n+++ {}", path, path);

    for hunk in diff.unified_diff().context_radius(3).iter_hunks() {
        out.push_str(&format!("\n{}", hunk.header()));
        for change in hunk.iter_changes() {
            let sign = match change.tag() {
                ChangeTag::Delete => "-",
                ChangeTag::Insert => "+",
                ChangeTag::Equal => " ",
            };
            out.push_str(&format!("{} {}", sign, change));
            if change.missing_newline() {
                out.push('\n');
            }
        }
    }

    out.truncate(out.trim_end().len());
    out
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
                description: "Execute a bash command in a sandboxed copy of the repository. The sandbox is an isolated temporary directory with the repo's current state. There is no need to cd to folders like /tmp before running commands.".to_string(),
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
            ToolDef {
                name: "read_file".to_string(),
                description: "Read the content of a single file with optional line range specification. Returns the file content with line numbers. Use start_line and end_line to focus on specific sections of large files.".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "Path to the file to read, relative to the repository root"
                        },
                        "start_line": {
                            "type": "number",
                            "description": "Starting line number, 1-indexed (optional)"
                        },
                        "end_line": {
                            "type": "number",
                            "description": "Ending line number, 1-indexed, inclusive (optional)"
                        }
                    },
                    "required": ["path"]
                }),
            },
            ToolDef {
                name: "edit_file".to_string(),
                description: "Replace text in a file. The old_str must match exactly one location in the file. The new_str will replace it. Ensure old_str is unique and includes enough context to identify a single location.".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "Path to the file to edit, relative to the repository root"
                        },
                        "old_str": {
                            "type": "string",
                            "description": "The exact string to find in the file (must match exactly one location)"
                        },
                        "new_str": {
                            "type": "string",
                            "description": "The replacement string"
                        }
                    },
                    "required": ["path", "old_str", "new_str"]
                }),
            },
            ToolDef {
                name: "grep".to_string(),
                description: "Fast text-based regex search that finds exact pattern matches within files using ripgrep. Search results include line numbers, file paths, and 2 lines of context around each match. Results are capped at 50 matches.".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "query": {
                            "type": "string",
                            "description": "The regex pattern to search for (Rust regex syntax)"
                        },
                        "includePattern": {
                            "type": "string",
                            "description": "Glob pattern for files to include (e.g. '**/*.rs')"
                        },
                        "excludePattern": {
                            "type": "string",
                            "description": "Glob pattern for files to exclude"
                        },
                        "caseSensitive": {
                            "type": "boolean",
                            "description": "Whether the search should be case sensitive (defaults to false)"
                        }
                    },
                    "required": ["query"]
                }),
            },
            ToolDef {
                name: "describe_edits".to_string(),
                description: "Call this after finishing a coding task or subtask to describe the edits you made. Provide a concise summary of what was changed and why.".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "message": {
                            "type": "string",
                            "description": "A description of the edits that were made"
                        }
                    },
                    "required": ["message"]
                }),
            },
        ],
    }
}
