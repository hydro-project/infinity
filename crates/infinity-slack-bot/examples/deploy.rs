use hydro_deploy::Deployment;
use hydro_lang::prelude::*;
use tracing_subscriber::EnvFilter;

type BoxError = Box<dyn std::error::Error + Send + Sync>;

struct SlackProcess;

#[tokio::main]
async fn main() -> Result<(), BoxError> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .init();

    tracing::info!("infinity-slack-bot deployer starting");

    let mut deployment = Deployment::new();
    let mut flow = FlowBuilder::new();
    let process = flow.process::<SlackProcess>();

    // Sidecar 1: Slack WebSocket I/O.
    let (slack_inbound, slack_response) = process
        .sidecar_bidi::<infinity_slack_bot::sidecar::SlackEvent, infinity_slack_bot::sidecar::SlackAction, _>(
            q!(|| { infinity_slack_bot::sidecar::create() }),
        );

    // Sidecar 2: Daemon connection management.
    let (daemon_inbound, daemon_response) = process
        .sidecar_bidi::<infinity_slack_bot::daemon_sidecar::DaemonEvent, infinity_slack_bot::daemon_sidecar::DaemonCommand, _>(
            q!(|| { infinity_slack_bot::daemon_sidecar::create() }),
        );

    // Dataflow: process Slack events + daemon responses, produce Slack actions + daemon commands.
    let (slack_actions, daemon_commands) =
        infinity_slack_bot::flow::slack_dataflow(slack_inbound, daemon_inbound);
    slack_response.complete(slack_actions);
    daemon_response.complete(daemon_commands);

    let _nodes = flow
        .with_default_optimize()
        .with_process(&process, deployment.Localhost())
        .deploy(&mut deployment);

    deployment
        .deploy()
        .await
        .map_err(|e| -> BoxError { e.to_string().into() })?;

    tracing::info!("Hydro deployment ready, starting...");

    deployment
        .start()
        .await
        .map_err(|e| -> BoxError { e.to_string().into() })?;

    tracing::info!("infinity-slack-bot running (Hydro + sidecar_bidi)");
    tokio::signal::ctrl_c().await?;

    deployment
        .stop()
        .await
        .map_err(|e| -> BoxError { e.to_string().into() })?;
    Ok(())
}
