//! `infinity-agent-cli rap install/update` — install RAP crates and register in rap.json.

use crate::inline_viewport::InlineViewport;
use crate::term_io::{CrosstermEvents, CrosstermTerm, EventSource, TermOut as _};
use crate::terminal;
use infinity_agent_core::tools::config::ToolsConfig;
use ratatui::layout::{Constraint, Layout};
use ratatui::{
    crossterm::event::{Event, KeyCode, KeyModifiers},
    style::{Color, Style},
    text::{Line, Span},
};
use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use tokio::sync::mpsc;
use tokio::time::Instant;

type BoxError = Box<dyn std::error::Error + Send + Sync>;

pub struct InstallArgs {
    pub crate_name: String,
    pub git: Option<String>,
    pub path: Option<String>,
}

/// Arguments for `infinity provider install <id> --crate ...`.
pub struct ProviderInstallArgs {
    /// Provider id to register in providers.json (e.g. "bedrock").
    pub id: String,
    pub crate_name: String,
    pub git: Option<String>,
    pub path: Option<String>,
}

pub use infinity_daemon::config::{load_config, providers_config_path, user_config_path};
use infinity_daemon::models::{ProviderConfig, ProvidersConfig, load_providers_config};

fn draw_spinner(
    viewport: &mut InlineViewport<CrosstermTerm>,
    start: &Instant,
    status: &str,
) -> Result<(), BoxError> {
    viewport.draw(2, |frame| {
        let areas =
            Layout::vertical([Constraint::Length(1), Constraint::Length(1)]).split(frame.area());
        terminal::render_thinking_bar(
            frame,
            areas[1],
            start,
            status,
            terminal::SpinnerState::Thinking,
        );
    })?;
    Ok(())
}

/// Stream `cargo install` output through the TUI viewport, with Ctrl+C support.
async fn run_cargo_install(
    viewport: &mut InlineViewport<CrosstermTerm>,
    crate_name: &str,
    git: Option<&str>,
    path: Option<&str>,
    features: &[&str],
) -> Result<(), BoxError> {
    let mut cmd = Command::new("cargo");
    cmd.arg("install");
    cmd.env("CARGO_NET_GIT_FETCH_WITH_CLI", "true");
    if let Some(git) = git {
        cmd.args(["--git", git]);
    } else if let Some(path) = path {
        cmd.args(["--path", path]);
    }
    cmd.arg(crate_name);
    cmd.arg("--force");
    if !features.is_empty() {
        cmd.args(["--features", &features.join(",")]);
    }
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    cmd.stdin(Stdio::null());

    let mut child = cmd
        .spawn()
        .map_err(|e| format!("failed to spawn cargo: {e}"))?;
    let stderr = child.stderr.take().ok_or("failed to capture stderr")?;
    let (line_tx, mut line_rx) = mpsc::unbounded_channel::<String>();

    std::thread::spawn(move || {
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            let _ = line_tx.send(line);
        }
    });

    let spinner_start = Instant::now();
    let status = format!("installing {crate_name}...");
    draw_spinner(viewport, &spinner_start, &status)?;

    let mut events = CrosstermEvents;

    loop {
        tokio::select! {
            biased;
            line = line_rx.recv() => {
                match line {
                    Some(text) => {
                        viewport.print_line_above(
                            Line::from(Span::styled(&text, Style::default().fg(Color::DarkGray))),
                        )?;
                        draw_spinner(viewport, &spinner_start, &status)?;
                    }
                    None => break,
                }
            }
            _ = events.wait_for_event() => {
                while let Some(event) = events.try_read_event()? {
                    if let Event::Key(key) = event
                        && matches!(key.code, KeyCode::Char('c'))
                            && key.modifiers.contains(KeyModifiers::CONTROL)
                        {
                            let _ = child.kill();
                            return Err("installation cancelled".into());
                        }
                }

                draw_spinner(viewport, &spinner_start, &status)?;
            }
        }
    }

    let status = child.wait()?;
    if !status.success() {
        return Err(format!("cargo install failed (exit code: {:?})", status.code()).into());
    }
    Ok(())
}

pub async fn run_install(args: InstallArgs) -> Result<(), BoxError> {
    let mut term = CrosstermTerm::new();
    term.enable_raw_mode()?;
    let mut viewport = InlineViewport::new(term, 2)?;

    viewport.print_line_above(Line::from(Span::styled(
        format!("Installing {}...", args.crate_name),
        Style::default().fg(Color::Yellow),
    )))?;

    if let Err(e) = run_cargo_install(
        &mut viewport,
        &args.crate_name,
        args.git.as_deref(),
        args.path.as_deref(),
        &[],
    )
    .await
    {
        viewport.print_line_above(Line::from(Span::styled(
            format!("✗ {e}"),
            Style::default().fg(Color::Red),
        )))?;
        viewport.draw(2, |_| {})?;
        terminal::cleanup(viewport.term_mut())?;
        return Err(e);
    }

    // Update user rap.json
    let config_path = user_config_path()?;
    if let Some(parent) = config_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let mut config = if config_path.exists() {
        load_config(&config_path)?
    } else {
        ToolsConfig::empty() // init if empty
    };

    // Remove existing entry with same id, then re-add with source info
    let had_existing = config
        .tool_sets
        .iter()
        .any(|ts| ts.id() == Some(&args.crate_name));
    if had_existing {
        config
            .tool_sets
            .retain(|ts| ts.id() != Some(&args.crate_name));
        viewport.print_line_above(Line::from(Span::styled(
            format!("Replacing existing entry for {}", args.crate_name),
            Style::default().fg(Color::Yellow),
        )))?;
    }
    config.add_installed_command(
        args.crate_name.clone(),
        args.crate_name.clone(),
        args.git.clone(),
        args.path.clone().map(|p| {
            std::fs::canonicalize(p)
                .expect("failed to canonicalize install path")
                .to_string_lossy()
                .to_string()
        }),
    );
    std::fs::write(&config_path, serde_json::to_string_pretty(&config)?)?;

    viewport.print_line_above(Line::from(Span::styled(
        format!(
            "✓ Installed and registered {} in {}",
            args.crate_name,
            config_path.display()
        ),
        Style::default().fg(Color::Green),
    )))?;
    viewport.draw(2, |_| {})?;
    terminal::cleanup(viewport.term_mut())?;
    Ok(())
}

/// Install a model provider crate and register it under the given id in
/// `~/.infinity/providers.json`. The crate's binary (assumed to share the
/// crate's name) becomes the provider's command.
pub async fn run_provider_install(args: ProviderInstallArgs) -> Result<(), BoxError> {
    let mut term = CrosstermTerm::new();
    term.enable_raw_mode()?;
    let mut viewport = InlineViewport::new(term, 2)?;

    viewport.print_line_above(Line::from(Span::styled(
        format!(
            "Installing {} as provider '{}'...",
            args.crate_name, args.id
        ),
        Style::default().fg(Color::Yellow),
    )))?;

    if let Err(e) = run_cargo_install(
        &mut viewport,
        &args.crate_name,
        args.git.as_deref(),
        args.path.as_deref(),
        &[],
    )
    .await
    {
        viewport.print_line_above(Line::from(Span::styled(
            format!("✗ {e}"),
            Style::default().fg(Color::Red),
        )))?;
        viewport.draw(2, |_| {})?;
        terminal::cleanup(viewport.term_mut())?;
        return Err(e);
    }

    // Update user providers.json (created on first install).
    let config_path = providers_config_path()?;
    if let Some(parent) = config_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut config = if config_path.exists() {
        load_providers_config(&config_path)?
    } else {
        ProvidersConfig::default()
    };

    if config.get(&args.id).is_some() {
        viewport.print_line_above(Line::from(Span::styled(
            format!("Replacing existing entry for provider '{}'", args.id),
            Style::default().fg(Color::Yellow),
        )))?;
    }
    let canonical_path = match args.path {
        Some(p) => Some(
            std::fs::canonicalize(&p)
                .map_err(|e| format!("failed to canonicalize install path {p}: {e}"))?
                .to_string_lossy()
                .to_string(),
        ),
        None => None,
    };
    config.upsert(
        args.id.clone(),
        ProviderConfig {
            command: vec![args.crate_name.clone()],
            crate_name: Some(args.crate_name.clone()),
            git: args.git.clone(),
            path: canonical_path,
        },
    );
    std::fs::write(&config_path, serde_json::to_string_pretty(&config)?)?;

    viewport.print_line_above(Line::from(Span::styled(
        format!(
            "✓ Installed and registered provider '{}' in {}",
            args.id,
            config_path.display()
        ),
        Style::default().fg(Color::Green),
    )))?;
    viewport.draw(2, |_| {})?;
    terminal::cleanup(viewport.term_mut())?;
    Ok(())
}

/// Update RAP tools, printing progress into an existing viewport. Returns names of failures.
async fn update_rap_tools(
    viewport: &mut InlineViewport<CrosstermTerm>,
) -> Result<Vec<String>, BoxError> {
    let config_path = user_config_path()?;
    let config = if config_path.exists() {
        load_config(&config_path)?
    } else {
        viewport.print_line_above(Line::from(Span::styled(
            "No user-level RAP configuration found",
            Style::default().fg(Color::DarkGray),
        )))?;
        return Ok(vec![]);
    };
    let installable = config.installable_commands();

    if installable.is_empty() {
        viewport.print_line_above(Line::from(Span::styled(
            format!(
                "No RAP tools with recorded sources in {}",
                config_path.display()
            ),
            Style::default().fg(Color::DarkGray),
        )))?;
        return Ok(vec![]);
    }

    viewport.print_line_above(Line::from(Span::styled(
        format!("Updating {} RAP tool(s)...", installable.len()),
        Style::default().fg(Color::Yellow),
    )))?;

    let mut failed = Vec::new();
    for (_command, crate_name, git, path) in &installable {
        viewport.print_line_above(Line::from(Span::styled(
            format!("→ Updating {crate_name}..."),
            Style::default().fg(Color::Cyan),
        )))?;
        viewport.draw(2, |_| {})?;

        match run_cargo_install(viewport, crate_name, git.as_deref(), path.as_deref(), &[]).await {
            Ok(()) => {
                viewport.print_line_above(Line::from(Span::styled(
                    format!("  ✓ {crate_name}"),
                    Style::default().fg(Color::Green),
                )))?;
            }
            Err(e) => {
                viewport.print_line_above(Line::from(Span::styled(
                    format!("  ✗ {crate_name}: {e}"),
                    Style::default().fg(Color::Red),
                )))?;
                failed.push(crate_name.clone());
            }
        }
        viewport.draw(2, |_| {})?;
    }
    Ok(failed)
}

/// Update model providers that have a recorded source, printing progress
/// into an existing viewport. Returns ids of failures.
async fn update_providers(
    viewport: &mut InlineViewport<CrosstermTerm>,
) -> Result<Vec<String>, BoxError> {
    let config_path = providers_config_path()?;
    if !config_path.exists() {
        viewport.print_line_above(Line::from(Span::styled(
            "No model providers configuration found",
            Style::default().fg(Color::DarkGray),
        )))?;
        return Ok(vec![]);
    }
    let config = load_providers_config(&config_path)?;
    let installable: Vec<(String, String, Option<String>, Option<String>)> = config
        .providers
        .iter()
        .filter_map(|(id, provider)| {
            provider.crate_name.as_ref().map(|crate_name| {
                (
                    id.clone(),
                    crate_name.clone(),
                    provider.git.clone(),
                    provider.path.clone(),
                )
            })
        })
        .collect();

    if installable.is_empty() {
        viewport.print_line_above(Line::from(Span::styled(
            format!(
                "No model providers with recorded sources in {}",
                config_path.display()
            ),
            Style::default().fg(Color::DarkGray),
        )))?;
        return Ok(vec![]);
    }

    viewport.print_line_above(Line::from(Span::styled(
        format!("Updating {} model provider(s)...", installable.len()),
        Style::default().fg(Color::Yellow),
    )))?;

    let mut failed = Vec::new();
    for (id, crate_name, git, path) in &installable {
        viewport.print_line_above(Line::from(Span::styled(
            format!("→ Updating provider '{id}' ({crate_name})..."),
            Style::default().fg(Color::Cyan),
        )))?;
        viewport.draw(2, |_| {})?;

        match run_cargo_install(viewport, crate_name, git.as_deref(), path.as_deref(), &[]).await {
            Ok(()) => {
                viewport.print_line_above(Line::from(Span::styled(
                    format!("  ✓ {id}"),
                    Style::default().fg(Color::Green),
                )))?;
            }
            Err(e) => {
                viewport.print_line_above(Line::from(Span::styled(
                    format!("  ✗ {id}: {e}"),
                    Style::default().fg(Color::Red),
                )))?;
                failed.push(id.clone());
            }
        }
        viewport.draw(2, |_| {})?;
    }
    Ok(failed)
}

/// After an update, ensure the daemon runs the freshly installed binaries:
/// boot one if none is running, or warn that the running instance is still
/// executing pre-update code and needs `infinity daemon restart`. Returns
/// `Ok(false)` if the daemon needed to be started but failed to boot (the
/// error is printed to the viewport), so callers can reflect it in their
/// exit code after cleaning up the terminal.
async fn ensure_fresh_daemon(
    viewport: &mut InlineViewport<CrosstermTerm>,
) -> Result<bool, BoxError> {
    if let Some(pid) = crate::daemon_client::running_daemon_pid() {
        viewport.print_line_above(Line::from(Span::styled(
            format!(
                "⚠ The daemon (pid {pid}) is still running the previous version — run `infinity daemon restart` to pick up the update"
            ),
            Style::default().fg(Color::Yellow),
        )))?;
        return Ok(true);
    }

    viewport.print_line_above(Line::from(Span::styled(
        "Daemon is not running — starting it...",
        Style::default().fg(Color::Yellow),
    )))?;
    viewport.draw(2, |_| {})?;
    match crate::daemon_client::launch_daemon().await {
        Ok(()) => {
            viewport.print_line_above(Line::from(Span::styled(
                "✓ Daemon started",
                Style::default().fg(Color::Green),
            )))?;
            Ok(true)
        }
        Err(e) => {
            viewport.print_line_above(Line::from(Span::styled(
                format!("✗ Failed to start daemon: {e}"),
                Style::default().fg(Color::Red),
            )))?;
            Ok(false)
        }
    }
}

/// `infinity provider update` — re-install all providers with recorded sources.
pub async fn run_provider_update() -> Result<(), BoxError> {
    let mut term = CrosstermTerm::new();
    term.enable_raw_mode()?;
    let mut viewport = InlineViewport::new(term, 2)?;

    let failed = update_providers(&mut viewport).await?;
    let daemon_ok = ensure_fresh_daemon(&mut viewport).await?;

    let summary = if failed.is_empty() {
        Span::styled("✓ All providers updated", Style::default().fg(Color::Green))
    } else {
        Span::styled(
            format!("✗ {} provider(s) failed to update", failed.len()),
            Style::default().fg(Color::Red),
        )
    };
    viewport.print_line_above(Line::from(summary))?;
    viewport.draw(2, |_| {})?;
    terminal::cleanup(viewport.term_mut())?;

    if !failed.is_empty() {
        Err("some providers failed to update".into())
    } else if !daemon_ok {
        Err("failed to start daemon after update".into())
    } else {
        Ok(())
    }
}

/// Returns the list of cargo features this binary was compiled with.
fn installed_features() -> Vec<&'static str> {
    let mut features = Vec::new();
    if cfg!(feature = "bundled-web") {
        features.push("bundled-web");
    }
    features
}

/// Update the CLI binary itself, printing progress into an existing viewport.
async fn update_cli(
    viewport: &mut InlineViewport<CrosstermTerm>,
    features: &[&str],
) -> Result<(), BoxError> {
    let (git, path) = detect_install_source()?;

    viewport.print_line_above(Line::from(Span::styled(
        "Updating infinity-agent-cli...",
        Style::default().fg(Color::Yellow),
    )))?;

    if let Err(e) = run_cargo_install(
        viewport,
        "infinity-agent-cli",
        git.as_deref(),
        path.as_deref(),
        features,
    )
    .await
    {
        viewport.print_line_above(Line::from(Span::styled(
            format!("✗ {e}"),
            Style::default().fg(Color::Red),
        )))?;
        return Err(e);
    }

    viewport.print_line_above(Line::from(Span::styled(
        "✓ infinity-agent-cli updated",
        Style::default().fg(Color::Green),
    )))?;
    Ok(())
}

pub async fn run_update() -> Result<(), BoxError> {
    let mut term = CrosstermTerm::new();
    term.enable_raw_mode()?;
    let mut viewport = InlineViewport::new(term, 2)?;

    let failed = update_rap_tools(&mut viewport).await?;
    let daemon_ok = ensure_fresh_daemon(&mut viewport).await?;

    let summary = if failed.is_empty() {
        Span::styled("✓ All tools updated", Style::default().fg(Color::Green))
    } else {
        Span::styled(
            format!("✗ {} tool(s) failed to update", failed.len()),
            Style::default().fg(Color::Red),
        )
    };
    viewport.print_line_above(Line::from(summary))?;
    viewport.draw(2, |_| {})?;
    terminal::cleanup(viewport.term_mut())?;

    if !failed.is_empty() {
        Err("some tools failed to update".into())
    } else if !daemon_ok {
        Err("failed to start daemon after update".into())
    } else {
        Ok(())
    }
}

/// Detect how `infinity-agent-cli` was originally installed by reading
/// `~/.cargo/.crates2.json`. Returns `(git, path)` — both None means registry.
fn detect_install_source() -> Result<(Option<String>, Option<String>), BoxError> {
    let home = dirs::home_dir().ok_or("could not determine home directory")?;
    let crates_file = home.join(".cargo").join(".crates2.json");
    let data = std::fs::read_to_string(&crates_file)
        .map_err(|e| format!("failed to read {}: {e}", crates_file.display()))?;
    let json: serde_json::Value =
        serde_json::from_str(&data).map_err(|e| format!("failed to parse .crates2.json: {e}"))?;
    let installs = json
        .get("installs")
        .and_then(|v| v.as_object())
        .ok_or("unexpected .crates2.json format")?;

    let key = installs
        .keys()
        .find(|k| k.starts_with("infinity-agent-cli "))
        .ok_or(
            "infinity-agent-cli not found in .crates2.json — was it installed via cargo install?",
        )?;

    // Key format: "infinity-agent-cli 0.1.0 (source+url#hash)"
    let source = key
        .rsplit_once('(')
        .and_then(|(_, rest)| rest.strip_suffix(')'))
        .ok_or("could not parse source from .crates2.json key")?;

    if let Some(url) = source.strip_prefix("git+") {
        // Strip #commit-hash if present
        let url = url.split_once('#').map_or(url, |(u, _)| u);
        Ok((Some(url.to_owned()), None))
    } else if let Some(path) = source.strip_prefix("path+file://") {
        Ok((None, Some(path.to_owned())))
    } else {
        // registry install
        Ok((None, None))
    }
}

pub async fn run_self_update(features_override: Option<&str>) -> Result<(), BoxError> {
    let mut term = CrosstermTerm::new();
    term.enable_raw_mode()?;
    let mut viewport = InlineViewport::new(term, 2)?;

    let features: Vec<&str> = match features_override {
        Some("") => vec![],
        Some(s) => s
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .collect(),
        None => installed_features(),
    };

    if let Err(e) = update_cli(&mut viewport, &features).await {
        viewport.draw(2, |_| {})?;
        terminal::cleanup(viewport.term_mut())?;
        return Err(e);
    }

    let rap_failed = update_rap_tools(&mut viewport).await?;
    let provider_failed = update_providers(&mut viewport).await?;
    let daemon_ok = ensure_fresh_daemon(&mut viewport).await?;

    let failed_count = rap_failed.len() + provider_failed.len();
    let summary = if failed_count == 0 {
        Span::styled("✓ Everything updated", Style::default().fg(Color::Green))
    } else {
        Span::styled(
            format!("✗ {failed_count} component(s) failed to update"),
            Style::default().fg(Color::Red),
        )
    };
    viewport.print_line_above(Line::from(summary))?;
    viewport.draw(2, |_| {})?;
    terminal::cleanup(viewport.term_mut())?;

    if failed_count != 0 {
        Err("some components failed to update".into())
    } else if !daemon_ok {
        Err("failed to start daemon after update".into())
    } else {
        Ok(())
    }
}
