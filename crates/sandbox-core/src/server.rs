use std::collections::HashMap;
use std::sync::Arc;

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use tokio::io::AsyncReadExt;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tracing;

use crate::callback::CallbackClient;
use crate::error::SandboxError;
use crate::jj::run_jj;
use crate::metadata::MetadataStore;
use crate::sandbox::SandboxBackend;
use crate::types::{
    CloneRepoArgs, CreateFileArgs, DescribeOverallChangesArgs, EditFileArgs, ExecuteCommandArgs,
    GrepArgs, ReadFileArgs, RepoState, SquashSandboxArgs,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    subscription: Option<bool>,
}

#[derive(Debug, Serialize)]
struct RapSubscriptionEvent {
    r#type: String,
    group_id: String,
    tool_call_id: String,
    text: String,
    associative: bool,
}

/// Events produced by the stdout/stderr readers and process exit waiter.
enum OutputEvent {
    Stdout(String),
    Stderr(String),
    Exit(i32),
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
    #[serde(rename = "displayScript", skip_serializing_if = "Option::is_none")]
    display_script: Option<String>,
}

type PendingTasks = Arc<Mutex<Vec<JoinHandle<()>>>>;

/// In-flight cancellable commands, keyed by tool_call_id.
///
/// The sender is created in `invoke_handler` (synchronously, before the task
/// is spawned) and the receiver is passed into the command handler.  The
/// `cancel_tool_call_handler` removes the sender and sends `()` to signal
/// cancellation; the handler receives it and sends SIGTERM to the process.
type InFlightMap = Arc<Mutex<HashMap<String, tokio::sync::oneshot::Sender<()>>>>;

/// Send SIGTERM to a process group by PID.
///
/// The spawned command is expected to have set its PGID to its own PID
/// (via the `exec` sub-entrypoint of the sandbox-local binary), so
/// sending the signal to the negative PID targets the entire group.
#[cfg(unix)]
fn kill_process(pid: Option<u32>) {
    if let Some(pid) = pid {
        use nix::sys::signal::{self, Signal};
        use nix::unistd::Pid;
        let _ = signal::kill(Pid::from_raw(-(pid as i32)), Signal::SIGTERM);
        tracing::info!(pid, "sent SIGTERM to process group");
    }
}

struct AppState<B: SandboxBackend, M: MetadataStore, C: CallbackClient> {
    backend: B,
    metadata: M,
    callback_client: C,
    pending_tasks: PendingTasks,
    in_flight: InFlightMap,
}

/// Shared handle to pending background tasks and in-flight commands.
#[derive(Clone)]
pub struct TaskTracker {
    pending_tasks: PendingTasks,
    in_flight: InFlightMap,
}

impl TaskTracker {
    /// Cancel all in-flight commands by sending the cancel signal,
    /// which triggers SIGTERM to child processes.
    pub async fn cancel_all_in_flight(&self) {
        let senders: Vec<_> = {
            let mut map = self.in_flight.lock().await;
            map.drain().map(|(_, tx)| tx).collect()
        };
        for tx in senders {
            let _ = tx.send(());
        }
    }

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
    let in_flight: InFlightMap = Arc::new(Mutex::new(HashMap::new()));

    let tracker = TaskTracker {
        pending_tasks,
        in_flight: in_flight.clone(),
    };

    let state = Arc::new(AppState {
        backend,
        metadata,
        callback_client,
        pending_tasks: tracker.pending_tasks.clone(),
        in_flight,
    });

    let router = Router::new()
        .route("/.well-known/rap-toolset", get(toolset_handler::<B, M, C>))
        .route("/invoke", post(invoke_handler::<B, M, C>))
        .route("/close_thread", post(close_thread_handler::<B, M, C>))
        .route(
            "/cancel_tool_call",
            post(cancel_tool_call_handler::<B, M, C>),
        )
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
    // For execute_command, register the cancellation channel synchronously
    // (before spawning the task) so that cancel_tool_call always finds an
    // entry — even if the cancel arrives before the command starts.
    if invocation.operation == "execute_command" {
        let (cancel_tx, cancel_rx) = tokio::sync::oneshot::channel();
        state
            .in_flight
            .lock()
            .await
            .insert(invocation.id.clone(), cancel_tx);

        let state_clone = state.clone();
        let handle = tokio::spawn(async move {
            handle_execute_command_streaming(&state_clone, &invocation, cancel_rx).await;
        });
        state.pending_tasks.lock().await.push(handle);
        return StatusCode::OK;
    }

    let state_clone = state.clone();
    let handle = tokio::spawn(async move {
        let result_text = match invocation.operation.as_str() {
            "clone_repo" => handle_clone_repo(&state_clone, &invocation)
                .await
                .map(|t| (t, None)),
            "read_file" => handle_read_file(&state_clone, &invocation).await,
            "edit_file" => handle_edit_file(&state_clone, &invocation).await,
            "create_file" => handle_create_file(&state_clone, &invocation).await,
            "grep" => handle_grep(&state_clone, &invocation).await,
            "describe_overall_changes" => {
                handle_describe_overall_changes(&state_clone, &invocation).await
            }
            "squash_sandbox" => handle_squash_sandbox(&state_clone, &invocation).await,
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
            subscription: None,
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

/// Request payload for the `/cancel_tool_call` RAP protocol endpoint.
#[derive(Debug, Deserialize)]
struct CancelToolCallRequest {
    tool_call_id: String,
    #[allow(dead_code)]
    thread_id: String,
}

/// Best-effort notification endpoint for tool call cancellation.
///
/// When the runtime interrupts a tool call (e.g. because of a new user
/// message), it POSTs `{"thread_id":"…","tool_call_id":"…"}` to every
/// tool server. The server uses this to abort in-flight operations — for
/// example, killing a long-running `execute_command` process. The
/// response is always 200 OK regardless of whether anything was actually
/// cancelled.
async fn cancel_tool_call_handler<
    B: SandboxBackend + 'static,
    M: MetadataStore + 'static,
    C: CallbackClient + 'static,
>(
    State(state): State<Arc<AppState<B, M, C>>>,
    Json(request): Json<CancelToolCallRequest>,
) -> StatusCode {
    tracing::info!(
        tool_call_id = %request.tool_call_id,
        thread_id = %request.thread_id,
        "received cancel_tool_call notification"
    );

    let sender = state.in_flight.lock().await.remove(&request.tool_call_id);

    if let Some(sender) = sender {
        // Signal the command handler to SIGTERM the process and clean up.
        let _ = sender.send(());
        tracing::info!(
            tool_call_id = %request.tool_call_id,
            "sent cancel signal to command handler"
        );
    } else {
        tracing::info!(
            tool_call_id = %request.tool_call_id,
            "no in-flight command found for tool call (may have already completed)"
        );
    }

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

    let bookmark = format!("sandbox-{}", invocation.group_id);
    let path = std::path::PathBuf::from(&remote_uri);

    // Resolve the base revision to an absolute jj change_id.
    let _ = crate::jj::run_jj(&path, &["workspace", "update-stale"]).await;
    let rev_to_resolve = args
        .base_thread_id
        .as_ref()
        .map(|id| format!("sandbox-{}", id));
    let base_res =
        crate::jj::jj_resolve_revision(&path, rev_to_resolve.as_deref().unwrap_or("@")).await;
    let base_revision = if base_res.as_ref().is_err_and(|e| match e {
        SandboxError::JujutsuError(e) if e.contains("It looks like this is a git repo.") => {
            true
        }
        _ => false,
    }) {
        run_jj(&path, &["git", "init"]).await?;
        crate::jj::jj_resolve_revision(&path, rev_to_resolve.as_deref().unwrap_or("@")).await?
    } else {
        base_res?
    };

    let repo_state = RepoState {
        group_id: invocation.group_id.clone(),
        remote_uri: remote_uri.clone(),
        bookmark: bookmark.clone(),
        base_revision: Some(base_revision),
    };
    state.metadata.put(&repo_state).await?;

    tracing::info!(group_id = %invocation.group_id, remote = %remote_uri, "repo cloned");
    Ok(match rev_to_resolve {
        Some(rev) => format!("Repository initialized on top of {rev}."),
        None => "Repository initialized.".to_string(),
    })
}

/// Run an action inside a sandbox: create → action → push → cleanup.
///
/// The closure receives the sandbox directory path and returns `(text, display_as)`.
/// `text` is the full result sent to the model; `display_as` is an optional short
/// summary shown in the CLI instead of the full text.
async fn with_sandbox<B, M, C, F, Fut>(
    state: &AppState<B, M, C>,
    group_id: &str,
    jj_description: Option<&str>,
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

    if let Some(jj_description) = jj_description {
        run_jj(&sandbox_dir, &["describe", "-m", jj_description]).await?;
    }

    let result = action(sandbox_dir.clone()).await;

    state.backend.push_sandbox(&sandbox_dir, group_id).await?;

    if let Err(e) = state.backend.cleanup_sandbox(&sandbox_dir).await {
        tracing::warn!("failed to cleanup sandbox: {e}");
    }

    tracing::info!(group_id = %group_id, "sandbox operation complete");

    result
}

// ── Streaming execute_command handler ──

/// Check if a command tries to `cd` into the original repo directory.
///
/// The agent sometimes emits commands like
///   `cd /Users/foo/my-repo && cargo build`
/// which escapes the sandbox and can hang on file locks. This function
/// detects such patterns and returns a user-friendly error message.
fn detect_cd_to_original_repo(command: &str, remote_uri: &str) -> Option<String> {
    // Only relevant for local paths (remote URIs like s3:// won't match).
    if !remote_uri.starts_with('/') {
        return None;
    }

    let trimmed = command.trim();

    // Must start with `cd `.
    if !trimmed.starts_with("cd ") {
        return None;
    }

    let after_cd = trimmed[3..].trim_start();

    // Strip optional quotes around the path.
    let (path_str, _rest) = if after_cd.starts_with('"') {
        match after_cd[1..].find('"') {
            Some(end) => (&after_cd[1..1 + end], &after_cd[2 + end..]),
            None => return None,
        }
    } else if after_cd.starts_with('\'') {
        match after_cd[1..].find('\'') {
            Some(end) => (&after_cd[1..1 + end], &after_cd[2 + end..]),
            None => return None,
        }
    } else {
        // Unquoted: take until whitespace, `&&`, or `;`.
        let end = after_cd
            .find(|c: char| c.is_whitespace() || c == '&' || c == ';')
            .unwrap_or(after_cd.len());
        (&after_cd[..end], &after_cd[end..])
    };

    let path_normalized = path_str.trim_end_matches('/');
    let uri_normalized = remote_uri.trim_end_matches('/');

    // Exact match or the cd target is a subdirectory of the original repo.
    if path_normalized == uri_normalized
        || path_normalized.starts_with(&format!("{uri_normalized}/"))
    {
        Some(format!(
            "Error: Do not `cd` to the absolute path `{path_str}`. \
             Commands are already executed in a sandboxed copy of that repository. \
             Run your commands directly in the current working directory \
             (e.g., `cargo build` instead of `cd {remote_uri} && cargo build`). \
             If you need to enter a subdirectory, use a relative path \
             (e.g., `cd src && ...`)."
        ))
    } else {
        None
    }
}

/// Format command output in the same style as the original non-streaming handler.
fn format_exec_output(stdout: &str, stderr: &str, exit_code: i32) -> String {
    let mut result = String::new();
    if !stdout.is_empty() {
        result.push_str(stdout);
    }
    if !stderr.is_empty() {
        if !result.is_empty() {
            result.push('\n');
        }
        result.push_str("[stderr]\n");
        result.push_str(stderr);
    }
    if result.is_empty() {
        result = format!("Command completed with exit code {exit_code}");
    } else {
        result.push_str(&format!("\n[exit code: {exit_code}]"));
    }
    result
}

/// Format output collected before a cancellation.
// fn format_cancel_output(stdout: &str, stderr: &str) -> String {
//     let mut text = String::from("Command cancelled.");
//     if !stdout.is_empty() || !stderr.is_empty() {
//         text.push_str("\nOutput before cancellation:\n");
//         text.push_str(stdout);
//         if !stderr.is_empty() {
//             if !stdout.is_empty() {
//                 text.push('\n');
//             }
//             text.push_str("[stderr]\n");
//             text.push_str(stderr);
//         }
//     }
//     text
// }

/// Send a `tool_result` callback.
async fn send_tool_result<C: CallbackClient>(
    client: &C,
    invocation: &RapInvocation,
    text: &str,
    display_as: Option<String>,
    subscription: bool,
) {
    let result = RapToolResult {
        r#type: "tool_result".to_string(),
        group_id: invocation.group_id.clone(),
        id: invocation.id.clone(),
        call_id: invocation.call_id.clone(),
        text: text.to_string(),
        display_as,
        subscription: if subscription { Some(true) } else { None },
    };
    let body = serde_json::to_string(&result).unwrap();
    if let Err(e) = client.post_json(&invocation.callback_url, &body).await {
        tracing::error!("failed to send tool result: {e}");
    }
}

/// Send a `subscription_event` callback.
async fn send_subscription_event<C: CallbackClient>(
    client: &C,
    invocation: &RapInvocation,
    text: &str,
    associative: bool,
) {
    let event = RapSubscriptionEvent {
        r#type: "subscription_event".to_string(),
        group_id: invocation.group_id.clone(),
        tool_call_id: invocation.id.clone(),
        text: text.to_string(),
        associative,
    };
    let body = serde_json::to_string(&event).unwrap();
    if let Err(e) = client.post_json(&invocation.callback_url, &body).await {
        tracing::error!("failed to send subscription event: {e}");
    }
}

/// Push sandbox changes (extracted from `with_sandbox`).
async fn push_sandbox_changes<B: SandboxBackend, M: MetadataStore, C: CallbackClient>(
    state: &AppState<B, M, C>,
    sandbox_dir: &std::path::Path,
    group_id: &str,
    _repo_state: &RepoState,
) -> Result<(), SandboxError> {
    state.backend.push_sandbox(sandbox_dir, group_id).await?;
    tracing::info!(group_id = %group_id, "sandbox operation complete");
    Ok(())
}

/// Top-level streaming handler for execute_command. Manages its own callbacks.
async fn handle_execute_command_streaming<
    B: SandboxBackend + 'static,
    M: MetadataStore,
    C: CallbackClient,
>(
    state: &Arc<AppState<B, M, C>>,
    invocation: &RapInvocation,
    cancel_rx: tokio::sync::oneshot::Receiver<()>,
) {
    if let Err(e) = handle_execute_command_streaming_inner(state, invocation, cancel_rx).await {
        send_tool_result(
            &state.callback_client,
            invocation,
            &format!("Error: {e}"),
            None,
            false,
        )
        .await;
    }
}

/// Inner implementation: creates sandbox, spawns process, streams output.
///
/// Cancellation is handled via `cancel_rx`: when the oneshot fires the handler
/// sends SIGTERM to the child process and returns a cancellation result.
/// Because the oneshot sender is registered in `invoke_handler` *before* the
/// task is spawned, there is no window where a cancel can be lost.
async fn handle_execute_command_streaming_inner<
    B: SandboxBackend + 'static,
    M: MetadataStore,
    C: CallbackClient,
>(
    state: &Arc<AppState<B, M, C>>,
    invocation: &RapInvocation,
    mut cancel_rx: tokio::sync::oneshot::Receiver<()>,
) -> Result<(), SandboxError> {
    let args: ExecuteCommandArgs = serde_json::from_value(invocation.arguments.clone())
        .map_err(|e| SandboxError::Other(format!("invalid arguments: {e}")))?;

    // Set up sandbox
    let repo_state = state
        .metadata
        .get(&invocation.group_id)
        .await?
        .ok_or_else(|| SandboxError::RepoNotFound(invocation.group_id.clone()))?;

    // Reject commands that `cd` to the original repo directory — this escapes
    // the sandbox and can hang on file locks (e.g. cargo build competing for
    // the same target directory).
    if let Some(error_msg) = detect_cd_to_original_repo(&args.command, &repo_state.remote_uri) {
        state.in_flight.lock().await.remove(&invocation.id);
        send_tool_result(&state.callback_client, invocation, &error_msg, None, false).await;
        return Ok(());
    }

    let sandbox_dir = state.backend.create_sandbox(&repo_state).await?;
    run_jj(&sandbox_dir, &["describe", "-m", &args.command]).await?;

    // Check for early cancellation before spawning the process.
    if cancel_rx.try_recv().is_ok() {
        let _ =
            push_sandbox_changes(state, &sandbox_dir, &invocation.group_id, &repo_state).await;
        if let Err(e) = state.backend.cleanup_sandbox(&sandbox_dir).await {
            tracing::warn!("failed to cleanup sandbox: {e}");
        }
        send_tool_result(
            &state.callback_client,
            invocation,
            "Command cancelled.",
            None,
            false,
        )
        .await;
        return Ok(());
    }

    // Spawn the process
    let mut spawned = match state
        .backend
        .spawn_command(&sandbox_dir, &["bash", "-c", &args.command])
        .await
    {
        Ok(s) => s,
        Err(e) => {
            state.in_flight.lock().await.remove(&invocation.id);
            let _ =
                push_sandbox_changes(state, &sandbox_dir, &invocation.group_id, &repo_state)
                    .await;
            if let Err(ce) = state.backend.cleanup_sandbox(&sandbox_dir).await {
                tracing::warn!("failed to cleanup sandbox: {ce}");
            }
            return Err(e);
        }
    };

    let child_pid = spawned.child.id();

    let stdout = spawned
        .child
        .stdout
        .take()
        .ok_or_else(|| SandboxError::Other("failed to capture stdout".to_string()))?;
    let stderr = spawned
        .child
        .stderr
        .take()
        .ok_or_else(|| SandboxError::Other("failed to capture stderr".to_string()))?;

    // Channel for output events from reader tasks
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<OutputEvent>();

    // Spawn stdout reader
    let tx_out = tx.clone();
    let out_handle = tokio::spawn(async move {
        let mut stdout = stdout;
        let mut buf = vec![0u8; 8192];
        loop {
            match stdout.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let _ = tx_out.send(OutputEvent::Stdout(
                        String::from_utf8_lossy(&buf[..n]).to_string(),
                    ));
                }
            }
        }
    });

    // Spawn stderr reader
    let tx_err = tx.clone();
    let err_handle = tokio::spawn(async move {
        let mut stderr = stderr;
        let mut buf = vec![0u8; 8192];
        loop {
            match stderr.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let _ = tx_err.send(OutputEvent::Stderr(
                        String::from_utf8_lossy(&buf[..n]).to_string(),
                    ));
                }
            }
        }
    });

    // Spawn exit waiter — waits for process exit AND for readers to finish,
    // so by the time we receive Exit all stdout/stderr data is already in the channel.
    tokio::spawn(async move {
        let status = spawned.child.wait().await;
        let code = status.map(|s| s.code().unwrap_or(-1)).unwrap_or(-1);
        let _ = out_handle.await;
        let _ = err_handle.await;
        let _ = tx.send(OutputEvent::Exit(code));
    });

    // ── Phase 1: Wait up to 5 seconds for process completion ──
    let mut stdout_buf = String::new();
    let mut stderr_buf = String::new();
    let mut exit_code: Option<i32> = None;
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);

    loop {
        tokio::select! {
            biased;
            _ = &mut cancel_rx => {
                tracing::info!(tool_call_id = %invocation.id, "command cancelled during phase 1");
                #[cfg(unix)]
                kill_process(child_pid);
                // let text = format_cancel_output(&stdout_buf, &stderr_buf);
                let _ = push_sandbox_changes(state, &sandbox_dir, &invocation.group_id, &repo_state).await;
                // TODO: determine if sending this would be in-spec
                // send_subscription_event(&state.callback_client, invocation, &text, true).await;
                if let Err(e) = state.backend.cleanup_sandbox(&sandbox_dir).await {
                    tracing::warn!("failed to cleanup sandbox: {e}");
                }
                return Ok(());
            }
            _ = tokio::time::sleep_until(deadline) => {
                break;
            }
            event = rx.recv() => {
                match event {
                    Some(OutputEvent::Stdout(s)) => stdout_buf.push_str(&s),
                    Some(OutputEvent::Stderr(s)) => stderr_buf.push_str(&s),
                    Some(OutputEvent::Exit(code)) => {
                        exit_code = Some(code);
                        break;
                    }
                    None => break,
                }
            }
        }
    }

    if let Some(code) = exit_code {
        // Process finished within 5 seconds — return a normal tool_result.
        state.in_flight.lock().await.remove(&invocation.id);
        let text = format_exec_output(&stdout_buf, &stderr_buf, code);
        let _ =
            push_sandbox_changes(state, &sandbox_dir, &invocation.group_id, &repo_state).await;
        send_tool_result(&state.callback_client, invocation, &text, None, false).await;
        if let Err(e) = state.backend.cleanup_sandbox(&sandbox_dir).await {
            tracing::warn!("failed to cleanup sandbox: {e}");
        }
        return Ok(());
    }

    // ── Phase 2: Process still running — send initial result, start streaming ──
    let mut initial_text = format!(
        "Command is still running. Output will be streamed via subscription events. Subscription ID: {}",
        invocation.id
    );
    if !stdout_buf.is_empty() || !stderr_buf.is_empty() {
        initial_text.push_str("\nOutput so far:\n");
        initial_text.push_str(&stdout_buf);
        if !stderr_buf.is_empty() {
            if !stdout_buf.is_empty() {
                initial_text.push('\n');
            }
            initial_text.push_str("[stderr]\n");
            initial_text.push_str(&stderr_buf);
        }
    }
    send_tool_result(
        &state.callback_client,
        invocation,
        &initial_text,
        None,
        true,
    )
    .await;

    // ── Fixed-interval subscription event loop ──
    let mut accumulated = String::new();
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(10));
    interval.tick().await; // consume first immediate tick

    loop {
        tokio::select! {
            biased;
            _ = &mut cancel_rx => {
                tracing::info!(tool_call_id = %invocation.id, "command cancelled during streaming");
                #[cfg(unix)]
                kill_process(child_pid);

                if !accumulated.is_empty() {
                    accumulated.push_str("\n[cancelled]");
                } else {
                    accumulated.push_str("[cancelled]");
                }

                let _ = push_sandbox_changes(
                    state,
                    &sandbox_dir,
                    &invocation.group_id,
                    &repo_state,
                )
                .await;
                send_subscription_event(
                    &state.callback_client,
                    invocation,
                    &accumulated,
                    true,
                )
                .await;
                if let Err(e) = state.backend.cleanup_sandbox(&sandbox_dir).await {
                    tracing::warn!("failed to cleanup sandbox: {e}");
                }
                return Ok(());
            }
            _ = interval.tick() => {
                if !accumulated.is_empty() {
                    send_subscription_event(
                        &state.callback_client,
                        invocation,
                        &accumulated,
                        true,
                    )
                    .await;
                    accumulated.clear();
                }
            }
            event = rx.recv() => {
                match event {
                    Some(OutputEvent::Stdout(s) | OutputEvent::Stderr(s)) => {
                        accumulated.push_str(&s);
                    }
                    Some(OutputEvent::Exit(code)) => {
                        state.in_flight.lock().await.remove(&invocation.id);

                        if accumulated.is_empty() {
                            accumulated.push_str(&format!("[exit code: {code}]"));
                        } else {
                            accumulated.push_str(&format!("\n[exit code: {code}]"));
                        }

                        let _ = push_sandbox_changes(
                            state,
                            &sandbox_dir,
                            &invocation.group_id,
                            &repo_state,
                        )
                        .await;
                        send_subscription_event(
                            &state.callback_client,
                            invocation,
                            &accumulated,
                            true,
                        )
                        .await;
                        if let Err(e) = state.backend.cleanup_sandbox(&sandbox_dir).await {
                            tracing::warn!("failed to cleanup sandbox: {e}");
                        }
                        return Ok(());
                    }
                    None => {
                        state.in_flight.lock().await.remove(&invocation.id);

                        let _ = push_sandbox_changes(
                            state,
                            &sandbox_dir,
                            &invocation.group_id,
                            &repo_state,
                        )
                        .await;
                        if !accumulated.is_empty() {
                            send_subscription_event(
                                &state.callback_client,
                                invocation,
                                &accumulated,
                                true,
                            )
                            .await;
                        }
                        if let Err(e) = state.backend.cleanup_sandbox(&sandbox_dir).await {
                            tracing::warn!("failed to cleanup sandbox: {e}");
                        }
                        return Ok(());
                    }
                }
            }
        }
    }
}

async fn handle_read_file<B: SandboxBackend, M: MetadataStore, C: CallbackClient>(
    state: &AppState<B, M, C>,
    invocation: &RapInvocation,
) -> Result<(String, Option<String>), SandboxError> {
    let args: ReadFileArgs = serde_json::from_value(invocation.arguments.clone())
        .map_err(|e| SandboxError::Other(format!("invalid arguments: {e}")))?;

    with_sandbox(
        state,
        &invocation.group_id,
        None,
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
            let display = format!("Read {} lines;", read_count);

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

    with_sandbox(
        state,
        &invocation.group_id,
        None,
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

async fn handle_create_file<B: SandboxBackend, M: MetadataStore, C: CallbackClient>(
    state: &AppState<B, M, C>,
    invocation: &RapInvocation,
) -> Result<(String, Option<String>), SandboxError> {
    let args: CreateFileArgs = serde_json::from_value(invocation.arguments.clone())
        .map_err(|e| SandboxError::Other(format!("invalid arguments: {e}")))?;

    with_sandbox(
        state,
        &invocation.group_id,
        None,
        |sandbox_dir| async move {
            let file_path = sandbox_dir.join(&args.path);

            // Fail if the file already exists
            if file_path.exists() {
                return Err(SandboxError::Other(format!(
                    "file already exists: {}. Use edit_file to modify existing files.",
                    args.path
                )));
            }

            // Create parent directories if needed
            if let Some(parent) = file_path.parent() {
                tokio::fs::create_dir_all(parent)
                    .await
                    .map_err(|e| SandboxError::Io(e))?;
            }

            tokio::fs::write(&file_path, &args.content)
                .await
                .map_err(|e| SandboxError::Io(e))?;

            let line_count = args.content.lines().count();
            let display = format!("Created {} ({} lines)", args.path, line_count);

            Ok((format!("Created file {}", args.path), Some(display)))
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
        None,
        |sandbox_dir| async move {
            let exclude_glob: Option<String>;
            let mut cmd_parts = vec!["rg", "--line-number"];

            // Context lines
            cmd_parts.push("-C");
            cmd_parts.push("2");

            // Max count to avoid huge output
            cmd_parts.push("--max-count");
            cmd_parts.push("50");

            if args.case_sensitive != Some(true) {
                cmd_parts.push("--ignore-case");
            }

            if let Some(ref pattern) = args.include_pattern {
                cmd_parts.push("--glob");
                cmd_parts.push(pattern);
            }

            if let Some(ref pattern) = args.exclude_pattern {
                exclude_glob = Some(format!("!{pattern}"));
                cmd_parts.push("--glob");
                cmd_parts.push(exclude_glob.as_ref().unwrap());
            }

            cmd_parts.push("--");
            cmd_parts.push(&args.query);

            let exec_result = backend.execute_command(&sandbox_dir, &cmd_parts).await?;

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

async fn handle_describe_overall_changes<B: SandboxBackend, M: MetadataStore, C: CallbackClient>(
    state: &AppState<B, M, C>,
    invocation: &RapInvocation,
) -> Result<(String, Option<String>), SandboxError> {
    let args: DescribeOverallChangesArgs = serde_json::from_value(invocation.arguments.clone())
        .map_err(|e| SandboxError::Other(format!("invalid arguments: {e}")))?;

    with_sandbox(
        state,
        &invocation.group_id,
        Some(&args.message),
        |_sandbox_dir| async move { Ok(("Edits described.".to_string(), None)) },
    )
    .await
}

async fn handle_squash_sandbox<B: SandboxBackend, M: MetadataStore, C: CallbackClient>(
    state: &AppState<B, M, C>,
    invocation: &RapInvocation,
) -> Result<(String, Option<String>), SandboxError> {
    let args: SquashSandboxArgs = serde_json::from_value(invocation.arguments.clone())
        .map_err(|e| SandboxError::Other(format!("invalid arguments: {e}")))?;

    let from_bookmark = format!("sandbox-{}", args.from_thread_id);

    with_sandbox(
        state,
        &invocation.group_id,
        None,
        |sandbox_dir| async move {
            run_jj(
                &sandbox_dir,
                &[
                    "squash",
                    "--from",
                    &from_bookmark,
                    "--use-destination-message",
                ],
            )
            .await?;
            run_jj(&sandbox_dir, &["bookmark", "delete", &from_bookmark]).await?;
            Ok((
                format!("Squashed changes from {from_bookmark}."),
                Some(format!("Squashed from {from_bookmark}")),
            ))
        },
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
                        },
                        "base_thread_id": {
                            "type": "string",
                            "description": "Optional thread ID to base this sandbox on top of. The new sandbox will be rebased onto that thread's bookmark."
                        }
                    },
                    "required": ["repo"]
                }),
                display_script: None,
            },
            ToolDef {
                name: "execute_command".to_string(),
                description: "Execute a bash command in a sandboxed copy of the repository. The sandbox is an isolated temporary directory with the repo's current state. There is no need to cd to folders like /tmp before running commands. This overwrites `describe_overall_changes`, so after you complete the task you should update it again.".to_string(),
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
                display_script: Some(r#""$ " + args.command"#.to_string()),
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
                display_script: Some(r#"let s = "Read " + args.path; if args.start_line != () { s += ":" + args.start_line; if args.end_line != () { s += "-" + args.end_line; } } s"#.to_string()),
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
                display_script: Some(r#"let n = args.new_str.split("\n").len(); "Edit " + args.path + " (" + n + " lines)""#.to_string()),
            },
            ToolDef {
                name: "create_file".to_string(),
                description: "Create a new file with the given content. Fails if the file already exists. Parent directories are created automatically if needed.".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "Path for the new file, relative to the repository root"
                        },
                        "content": {
                            "type": "string",
                            "description": "The content to write to the new file"
                        }
                    },
                    "required": ["path", "content"]
                }),
                display_script: None,
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
                display_script: Some(r#"let s = "Grep: " + args.query; if args.includePattern != () { s += " in " + args.includePattern; } s"#.to_string()),
            },
            ToolDef {
                name: "describe_overall_changes".to_string(),
                description: "Call this after finishing a coding task or subtask to describe the overall changes. Use a git-style commit message: a short one-line summary, followed by a blank line, then detailed explanations of what was changed and why.".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "message": {
                            "type": "string",
                            "description": "A git-style description of the edits: a short one-line summary, followed by a blank line, then detailed explanations"
                        }
                    },
                    "required": ["message"]
                }),
                display_script: None,
            },
            ToolDef {
                name: "squash_sandbox".to_string(),
                description: "Squash changes from a child thread's sandbox into the current thread's sandbox. This runs `jj squash` to merge the child's changes. Use this after a child thread completes its work to incorporate its edits.".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "from_thread_id": {
                            "type": "string",
                            "description": "The thread ID of the child sandbox to squash from"
                        }
                    },
                    "required": ["from_thread_id"]
                }),
                display_script: None,
            },
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_cd_to_exact_repo_path() {
        let uri = "/Users/foo/my-repo";
        assert!(detect_cd_to_original_repo("cd /Users/foo/my-repo && cargo build", uri).is_some());
    }

    #[test]
    fn detects_cd_with_trailing_slash() {
        let uri = "/Users/foo/my-repo";
        assert!(detect_cd_to_original_repo("cd /Users/foo/my-repo/ && cargo build", uri).is_some());
    }

    #[test]
    fn detects_cd_with_double_quotes() {
        let uri = "/Users/foo/my-repo";
        assert!(
            detect_cd_to_original_repo("cd \"/Users/foo/my-repo\" && cargo build", uri).is_some()
        );
    }

    #[test]
    fn detects_cd_with_single_quotes() {
        let uri = "/Users/foo/my-repo";
        assert!(
            detect_cd_to_original_repo("cd '/Users/foo/my-repo' && cargo build", uri).is_some()
        );
    }

    #[test]
    fn detects_cd_with_semicolon() {
        let uri = "/Users/foo/my-repo";
        assert!(detect_cd_to_original_repo("cd /Users/foo/my-repo; ls", uri).is_some());
    }

    #[test]
    fn detects_cd_to_subdirectory() {
        let uri = "/Users/foo/my-repo";
        assert!(
            detect_cd_to_original_repo("cd /Users/foo/my-repo/src && cargo build", uri).is_some()
        );
    }

    #[test]
    fn detects_cd_alone() {
        let uri = "/Users/foo/my-repo";
        assert!(detect_cd_to_original_repo("cd /Users/foo/my-repo", uri).is_some());
    }

    #[test]
    fn allows_relative_cd() {
        let uri = "/Users/foo/my-repo";
        assert!(detect_cd_to_original_repo("cd src && cargo build", uri).is_none());
    }

    #[test]
    fn allows_different_absolute_path() {
        let uri = "/Users/foo/my-repo";
        assert!(detect_cd_to_original_repo("cd /tmp && ls", uri).is_none());
    }

    #[test]
    fn allows_non_cd_commands() {
        let uri = "/Users/foo/my-repo";
        assert!(detect_cd_to_original_repo("cargo build", uri).is_none());
    }

    #[test]
    fn allows_non_cd_command_containing_path() {
        let uri = "/Users/foo/my-repo";
        assert!(detect_cd_to_original_repo("ls /Users/foo/my-repo", uri).is_none());
    }

    #[test]
    fn ignores_s3_remote_uri() {
        let uri = "s3://bucket/my-repo";
        assert!(detect_cd_to_original_repo("cd s3://bucket/my-repo && ls", uri).is_none());
    }

    #[test]
    fn does_not_match_prefix_of_different_repo() {
        let uri = "/Users/foo/my-repo";
        assert!(
            detect_cd_to_original_repo("cd /Users/foo/my-repo-other && cargo build", uri).is_none()
        );
    }

    #[test]
    fn handles_uri_with_trailing_slash() {
        let uri = "/Users/foo/my-repo/";
        assert!(detect_cd_to_original_repo("cd /Users/foo/my-repo && cargo build", uri).is_some());
    }
}
