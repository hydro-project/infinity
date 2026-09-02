//! Memory-scaling benchmark for the local agent runtime.
//!
//! Launches `AGENTS` agents on one `LocalAgentSystem` and runs each of them
//! through `TURNS` synthetic turns against an in-process scripted model. Every
//! turn is a full runtime round trip: the user message enters through the
//! input queue, the model streams a ~1 KB completion that calls a tool, the
//! tool result comes back through the queue, and a second completion closes
//! the turn. After each wave of agents finishes its turns, the process RSS is
//! sampled — at that point every agent is idle, so the measurement reflects
//! what a resident agent actually costs: its conversation history in the
//! stores, and nothing else (no task, no stack, no connection).
//!
//! Run with:
//!
//! ```sh
//! cargo run --release -p infinity-agent-core --example agent_scale
//! ```
//!
//! Environment variables: `AGENTS` (default 10000), `TURNS` (default 20),
//! `WAVE` (default 500). Prints a CSV of `agents,rss_bytes` to stdout.

use std::cell::Cell;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use infinity_provider_protocol::completion::{
    CompletionError, CompletionRequest, FinalResponse, ModelStream, StreamChunk, Usage,
};
use infinity_provider_protocol::message::{
    Message, ToolCall, ToolResult, ToolResultContent, UserContent,
};

use infinity_agent_core::message::{InputMessage, InputMessageContent};
use infinity_agent_core::stores::{InMemoryConversationStore, InMemoryStateStore};
use infinity_agent_core::system::local::ChannelSender;
use infinity_agent_core::system::{
    AgentEvent, AgentSystemBuilder, ReplaySnapshot, StaticModel, ThreadObserver,
};
use infinity_agent_core::tools::{Tool, ToolContext};
use infinity_agent_core::traits::InputSender;
use infinity_provider_protocol::{ModelEntry, SingleModelProvider};

/// ~100 characters of assistant text per streamed chunk.
const CHUNK_TEXT: &str = "Reviewed the module and updated the failing case; the assertion now covers the boundary path. ";
/// Chunks per completion (~1.2 KB of assistant text each round).
const CHUNKS_PER_COMPLETION: usize = 12;

// ── Scripted model ──
//
// A self-driving stand-in for a model provider: every `stream()` call
// immediately streams a fixed-shape response through the runtime's real
// streaming pipeline. If the last history message is a tool result, the
// response is text that ends the turn; otherwise it is text plus a tool
// call, so each turn exercises two completion rounds and one tool dispatch.

#[derive(Clone)]
struct ScriptedModel;

#[async_trait]
impl infinity_provider_protocol::CompletionModel for ScriptedModel {
    async fn stream(&self, request: CompletionRequest) -> Result<ModelStream, CompletionError> {
        let after_tool_result = matches!(
            request.chat_history.last(),
            Some(Message::User { content }) if content
                .iter()
                .any(|c| matches!(c, UserContent::ToolResult(_)))
        );

        let mut chunks: Vec<Result<StreamChunk, CompletionError>> =
            Vec::with_capacity(CHUNKS_PER_COMPLETION + 2);
        for _ in 0..CHUNKS_PER_COMPLETION {
            chunks.push(Ok(StreamChunk::Text(CHUNK_TEXT.to_owned())));
        }
        if !after_tool_result {
            chunks.push(Ok(StreamChunk::ToolCall(ToolCall::new(
                uuid::Uuid::new_v4().to_string(),
                "run_command",
                serde_json::json!({"command": "cargo test --workspace"}),
            ))));
        }
        chunks.push(Ok(StreamChunk::Final(FinalResponse {
            usage: Some(Usage::default()),
        })));

        Ok(Box::pin(futures_util::stream::iter(chunks)))
    }
}

// ── Synthetic async tool ──
//
// Delivers its result back through the input queue, like every real
// asynchronous tool: the slice that dispatched the call ends, and the result
// starts a new one.

struct RunCommand;

#[async_trait]
impl Tool<ChannelSender> for RunCommand {
    fn name(&self) -> &str {
        "run_command"
    }

    fn description(&self) -> &str {
        "Run a shell command."
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {"command": {"type": "string"}},
            "required": ["command"]
        })
    }

    async fn execute(
        &self,
        _args: serde_json::Value,
        id: String,
        call_id: Option<String>,
        context: &ToolContext<ChannelSender>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let result =
            InputMessage {
                content: InputMessageContent::User(UserContent::ToolResult(ToolResult {
                    id,
                    call_id,
                    content:
                        vec![ToolResultContent::Text(infinity_provider_protocol::message::Text {
                    text: "test result: ok. 148 passed; 0 failed; 3 ignored; finished in 21.38s"
                        .repeat(5),
                })],
                })),
                group_id: context.group_id.clone(),
                metadata: None,
                synthetic: None,
                display_as: None,
                subscription: false,
            };
        context
            .message_sender
            .send_to_input_queue(result, &uuid::Uuid::new_v4().to_string())
            .await?;
        Ok(())
    }
}

// ── Completion-counting observer ──

struct CountObserver {
    finished: Rc<Cell<u64>>,
}

#[async_trait(?Send)]
impl ThreadObserver for CountObserver {
    type SubscribeRequest = ();

    fn on_event(&self, _thread_id: &str, event: &AgentEvent) {
        if matches!(event, AgentEvent::CompletionFinished { .. }) {
            self.finished.set(self.finished.get() + 1);
        }
    }

    fn on_subscribe(&self, _thread_id: &str, _request: (), _snapshot: ReplaySnapshot) {}
}

// ── Measurement helpers ──

fn rss_bytes() -> u64 {
    let status = std::fs::read_to_string("/proc/self/status").expect("read /proc/self/status");
    let line = status
        .lines()
        .find(|l| l.starts_with("VmRSS:"))
        .expect("VmRSS line in /proc/self/status");
    let kb: u64 = line
        .trim_start_matches("VmRSS:")
        .trim()
        .trim_end_matches("kB")
        .trim()
        .parse()
        .expect("parse VmRSS value");
    kb * 1024
}

/// Return freed allocator caches to the OS so RSS reflects live data.
fn trim_allocator() {
    #[cfg(target_os = "linux")]
    unsafe {
        libc::malloc_trim(0);
    }
}

fn env_usize(name: &str, default: usize) -> usize {
    match std::env::var(name) {
        Ok(v) => v.parse().expect("numeric environment variable"),
        Err(_) => default,
    }
}

fn user_text(thread_id: &str, turn: usize) -> InputMessage {
    InputMessage {
        content: InputMessageContent::User(UserContent::text(format!(
            "Turn {turn}: re-run the affected tests for {thread_id} and summarize any failures \
             along with the modules they belong to."
        ))),
        group_id: thread_id.to_owned(),
        metadata: None,
        synthetic: None,
        display_as: None,
        subscription: false,
    }
}

async fn wait_for(finished: &Rc<Cell<u64>>, expected: u64) {
    let mut last = finished.get();
    let mut stalled_for = 0u32;
    while finished.get() < expected {
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        let now = finished.get();
        if now == last {
            stalled_for += 1;
            assert!(
                stalled_for < 15_000,
                "bug: stalled at {now}/{expected} completions"
            );
        } else {
            stalled_for = 0;
            last = now;
        }
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let agents = env_usize("AGENTS", 10_000);
    let turns = env_usize("TURNS", 20);
    let wave = env_usize("WAVE", 500);

    tokio::task::LocalSet::new()
        .run_until(async move {
            let entry = ModelEntry {
                model_id: "scripted".to_owned(),
                display_name: "scripted".to_owned(),
                // Disables the auto-compaction threshold so history growth
                // stays deterministic across the run.
                context_window: 0,
                max_output_tokens: None,
                supports_image_input: false,
            };
            let provider = Arc::new(SingleModelProvider::new(entry.clone(), ScriptedModel));
            let model = StaticModel::from_entry(provider, &entry);

            let finished = Rc::new(Cell::new(0u64));
            let observer_count = finished.clone();
            let mut running = AgentSystemBuilder::new_local(
                InMemoryConversationStore::new(),
                InMemoryStateStore::new(),
                model,
            )
            .tool(Box::new(RunCommand))
            .start_with_observer(move |_| CountObserver {
                finished: observer_count.clone(),
            });
            let sender = running.sender();

            trim_allocator();
            println!("agents,rss_bytes");
            println!("0,{}", rss_bytes());

            let start = Instant::now();
            let mut launched = 0usize;
            let mut expected = 0u64;
            while launched < agents {
                let batch = wave.min(agents - launched);
                let ids: Vec<String> = (launched..launched + batch)
                    .map(|i| format!("agent-{i}"))
                    .collect();

                // Drive the whole wave turn by turn: each turn is two
                // completion rounds (tool call + follow-up after its result).
                for turn in 0..turns {
                    for id in &ids {
                        sender
                            .send_to_input_queue(
                                user_text(id, turn),
                                &uuid::Uuid::new_v4().to_string(),
                            )
                            .await
                            .expect("send user text");
                    }
                    expected += (batch * 2) as u64;
                    wait_for(&finished, expected).await;
                }

                launched += batch;
                // Let idle drivers finish exiting before sampling.
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                // Drain lifecycle notifications like a real embedding would;
                // otherwise they accumulate in the channel for the whole run
                // and get counted as per-agent memory.
                while running.try_next_lifecycle_event().is_ok() {}
                trim_allocator();
                println!("{launched},{}", rss_bytes());
            }

            let elapsed = start.elapsed();
            let total = rss_bytes();
            eprintln!(
                "{agents} agents x {turns} turns ({} completions) in {:.1}s; \
                 rss {:.1} MiB, {:.1} KiB per agent",
                expected,
                elapsed.as_secs_f64(),
                total as f64 / (1024.0 * 1024.0),
                total as f64 / 1024.0 / agents as f64,
            );

            running.shutdown().await;
        })
        .await;
}
