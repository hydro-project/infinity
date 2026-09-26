//! Standalone TUI demo for manual terminal-compatibility testing.
//!
//! Runs the real `terminal::run` UI against the real terminal, but feeds it
//! a scripted display stream instead of a daemon connection: a spinner, some
//! streaming assistant text, and periodic tool calls, looping forever.
//!
//! Useful for reproducing terminal/multiplexer interaction bugs (resize,
//! reflow, detach/reattach) without needing a live agent:
//!
//! ```sh
//! cargo run -p infinity-agent-cli --example tui_demo
//! ```
//!
//! Ctrl+C exits.

use infinity_agent_cli::display::DisplayEvent;
use infinity_agent_cli::term_io::{CrosstermEvents, CrosstermTerm};
use infinity_agent_cli::terminal;
use std::collections::HashMap;
use tokio::sync::mpsc;
use tokio::time::{Duration, sleep};

type BoxError = Box<dyn std::error::Error + Send + Sync>;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), BoxError> {
    let (input_tx, mut input_rx) = mpsc::unbounded_channel::<String>();
    let (display_tx, display_rx) = mpsc::unbounded_channel();
    let (load_session_tx, _load_session_rx) = mpsc::unbounded_channel();
    let (model_switch_tx, _model_switch_rx) = mpsc::unbounded_channel();
    let (_session_tx, session_rx) = mpsc::unbounded_channel();
    let (_model_switched_tx, model_switched_rx) = mpsc::unbounded_channel();
    let (_sessions_updated_tx, sessions_updated_rx) = mpsc::unbounded_channel();
    let (soft_detach_tx, _soft_detach_rx) = mpsc::unbounded_channel();
    let (_detach_result_tx, detach_result_rx) = mpsc::unbounded_channel();
    let (choice_answered_tx, _choice_answered_rx) = mpsc::unbounded_channel();

    // Drain user input so typing works but goes nowhere.
    tokio::spawn(async move { while input_rx.recv().await.is_some() {} });

    // Scripted "agent": stream numbered text lines and tool calls forever.
    let script = tokio::spawn(async move {
        let send = |evt: DisplayEvent| {
            display_tx
                .send((None, evt))
                .map_err(|_| "display channel closed")
        };
        let mut turn = 0u32;
        loop {
            turn += 1;
            send(DisplayEvent::UserInput(format!("demo prompt {turn}")))?;
            send(DisplayEvent::StartOutput)?;
            sleep(Duration::from_millis(600)).await;
            send(DisplayEvent::ThinkingStart)?;
            send(DisplayEvent::ThinkingChunk {
                chunk: format!("thinking about turn {turn}..."),
            })?;
            sleep(Duration::from_millis(600)).await;
            send(DisplayEvent::ThinkingEnd)?;
            for i in 1..=6 {
                // Long lines that wrap on typical widths, streamed in small
                // chunks like a real model response.
                let line = format!(
                    "turn {turn} assistant line {i:02} — {}",
                    "lorem ipsum dolor sit amet consectetur adipiscing elit sed do eiusmod \
                     tempor incididunt ut labore et dolore magna aliqua"
                );
                for chunk in line.as_bytes().chunks(17) {
                    send(DisplayEvent::TextChunk {
                        chunk: String::from_utf8_lossy(chunk).into_owned(),
                    })?;
                    sleep(Duration::from_millis(40)).await;
                }
                send(DisplayEvent::TextChunk {
                    chunk: "\n".to_owned(),
                })?;
            }
            send(DisplayEvent::ToolCall {
                name: "demo_tool".to_owned(),
                args: serde_json::json!({"turn": turn}),
                display_as: Some(format!("demo_tool(turn={turn})")),
            })?;
            sleep(Duration::from_millis(800)).await;
            send(DisplayEvent::ToolResult {
                segments: vec![rap_protocol::DisplaySegment::Text(format!(
                    "tool result for turn {turn}"
                ))],
            })?;
            sleep(Duration::from_millis(400)).await;
            send(DisplayEvent::ResponseDone(None))?;
            sleep(Duration::from_millis(1000)).await;
        }
        #[expect(unreachable_code, reason = "loop only exits via error")]
        Ok::<(), BoxError>(())
    });

    let ui = terminal::run(
        CrosstermTerm::new(),
        CrosstermEvents,
        input_tx,
        display_rx,
        "demo-model".to_owned(),
        "demo".to_owned(),
        100_000,
        HashMap::new(),
        load_session_tx,
        model_switch_tx,
        Vec::new(),
        None,
        session_rx,
        model_switched_rx,
        sessions_updated_rx,
        soft_detach_tx,
        detach_result_rx,
        choice_answered_tx,
    )
    .await;
    script.abort();
    ui.map(|_| ())
}
