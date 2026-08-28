//! Compiles the Slack bot's Hydro dataflow (from `infinity-slack-dataflow`)
//! into plain Rust with Hydro's embedded mode. The generated file exposes
//!
//! ```ignore
//! pub fn slack_bot(
//!     daemon_events: impl Stream<Item = DaemonEvent> + Unpin,
//!     slack_events: impl Stream<Item = SlackEvent> + Unpin,
//!     outputs: &mut slack_bot::EmbeddedOutputs<impl FnMut(DaemonCommand), impl FnMut(SlackAction)>,
//! ) -> Dfir
//! ```
//!
//! which `src/main.rs` includes and drives around its own event loop.

use hydro_lang::location::Location;

/// Marker type for the single Hydro process hosting the bot's dataflow.
struct SlackBot;

fn main() {
    println!("cargo::rerun-if-changed=build.rs");

    let out_dir = std::env::var("OUT_DIR").expect("bug: OUT_DIR not set in build script");

    let mut flow = hydro_lang::compile::builder::FlowBuilder::new();
    let process = flow.process::<SlackBot>();

    let (slack_actions, daemon_commands) = infinity_slack_dataflow::flow::slack_dataflow(
        process.embedded_input("slack_events"),
        process.embedded_input("daemon_events"),
    );
    slack_actions.embedded_output("slack_actions");
    daemon_commands.embedded_output("daemon_commands");

    let code = flow
        .with_process(&process, "slack_bot")
        .generate_embedded("infinity-slack-dataflow");

    std::fs::write(
        format!("{out_dir}/slack_bot_dfir.rs"),
        prettyplease::unparse(&code),
    )
    .expect("failed to write generated embedded dataflow");
}
