//! `infinity-agent-cli rap install/update` — install RAP crates and register in rap.json.

use crate::inline_viewport::InlineViewport;
use crate::terminal;
use infinity_agent_core::tools::config::ToolsConfig;
use ratatui::layout::{Constraint, Layout};
use ratatui::{
    crossterm::{
        event::{self, Event, KeyCode, KeyModifiers},
        terminal as cterm,
    },
    style::{Color, Style},
    text::{Line, Span},
};
use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::time::Instant;
use tokio::sync::mpsc;

type BoxError = Box<dyn std::error::Error + Send + Sync>;

pub struct InstallArgs {
    pub crate_name: String,
    pub git: Option<String>,
    pub path: Option<String>,
}

pub use infinity_daemon::config::{load_config, user_config_path};

fn draw_spinner(
    viewport: &mut InlineViewport,
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
    viewport: &mut InlineViewport,
    crate_name: &str,
    git: Option<&str>,
    path: Option<&str>,
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
            _ = terminal::poll_crossterm_event() => {
                while event::poll(std::time::Duration::ZERO)? {
                    if let Event::Key(key) = event::read()?
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
    cterm::enable_raw_mode()?;
    let mut viewport = InlineViewport::new(2)?;

    viewport.print_line_above(Line::from(Span::styled(
        format!("Installing {}...", args.crate_name),
        Style::default().fg(Color::Yellow),
    )))?;

    if let Err(e) = run_cargo_install(
        &mut viewport,
        &args.crate_name,
        args.git.as_deref(),
        args.path.as_deref(),
    )
    .await
    {
        viewport.print_line_above(Line::from(Span::styled(
            format!("✗ {e}"),
            Style::default().fg(Color::Red),
        )))?;
        viewport.draw(2, |_| {})?;
        terminal::cleanup()?;
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
    terminal::cleanup()?;
    Ok(())
}

/// Update RAP tools, printing progress into an existing viewport. Returns names of failures.
async fn update_rap_tools(viewport: &mut InlineViewport) -> Result<Vec<String>, BoxError> {
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

        match run_cargo_install(viewport, crate_name, git.as_deref(), path.as_deref()).await {
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

/// Update the CLI binary itself, printing progress into an existing viewport.
async fn update_cli(viewport: &mut InlineViewport) -> Result<(), BoxError> {
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
    cterm::enable_raw_mode()?;
    let mut viewport = InlineViewport::new(2)?;

    let failed = update_rap_tools(&mut viewport).await?;

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
    terminal::cleanup()?;

    if failed.is_empty() {
        Ok(())
    } else {
        Err("some tools failed to update".into())
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
        Ok((Some(url.to_string()), None))
    } else if let Some(path) = source.strip_prefix("path+file://") {
        Ok((None, Some(path.to_string())))
    } else {
        // registry install
        Ok((None, None))
    }
}

pub async fn run_self_update() -> Result<(), BoxError> {
    cterm::enable_raw_mode()?;
    let mut viewport = InlineViewport::new(2)?;

    if let Err(e) = update_cli(&mut viewport).await {
        viewport.draw(2, |_| {})?;
        terminal::cleanup()?;
        return Err(e);
    }

    let rap_failed = update_rap_tools(&mut viewport).await?;

    let summary = if rap_failed.is_empty() {
        Span::styled("✓ Everything updated", Style::default().fg(Color::Green))
    } else {
        Span::styled(
            format!("✗ {} RAP tool(s) failed to update", rap_failed.len()),
            Style::default().fg(Color::Red),
        )
    };
    viewport.print_line_above(Line::from(summary))?;
    viewport.draw(2, |_| {})?;
    terminal::cleanup()?;

    if rap_failed.is_empty() {
        Ok(())
    } else {
        Err("some tools failed to update".into())
    }
}
