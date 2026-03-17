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

pub fn user_config_path() -> Result<std::path::PathBuf, BoxError> {
    let home = dirs::home_dir().ok_or("could not determine home directory")?;
    Ok(home.join(".infinity").join("rap.json"))
}

pub fn load_config(path: &std::path::Path) -> ToolsConfig {
    ToolsConfig::from_file(&path.to_string_lossy()).unwrap_or_else(|_| ToolsConfig::empty())
}

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
            areas[0],
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
        for line in BufReader::new(stderr).lines().flatten() {
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
                    if let Event::Key(key) = event::read()? {
                        if matches!(key.code, KeyCode::Char('c'))
                            && key.modifiers.contains(KeyModifiers::CONTROL)
                        {
                            let _ = child.kill();
                            return Err("installation cancelled".into());
                        }
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

    viewport.print_line_above(
        Line::from(Span::styled(
            format!("Installing {}...", args.crate_name),
            Style::default().fg(Color::Yellow),
        )),
    )?;

    if let Err(e) = run_cargo_install(
        &mut viewport,
        &args.crate_name,
        args.git.as_deref(),
        args.path.as_deref(),
    )
    .await
    {
        viewport.print_line_above(
            Line::from(Span::styled(
                format!("✗ {e}"),
                Style::default().fg(Color::Red),
            )),
        )?;
        viewport.draw(2, |_| {})?;
        terminal::cleanup()?;
        return Err(e);
    }

    // Update user rap.json
    let config_path = user_config_path()?;
    if let Some(parent) = config_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut config = load_config(&config_path);

    // Remove existing entry with same id, then re-add with source info
    let had_existing = config
        .tool_sets
        .iter()
        .any(|ts| ts.id() == Some(&args.crate_name));
    if had_existing {
        config
            .tool_sets
            .retain(|ts| ts.id() != Some(&args.crate_name));
        viewport.print_line_above(
            Line::from(Span::styled(
                format!("Replacing existing entry for {}", args.crate_name),
                Style::default().fg(Color::Yellow),
            )),
        )?;
    }
    config.add_installed_command(
        args.crate_name.clone(),
        args.crate_name.clone(),
        args.git.clone(),
        args.path.clone().map(|p| {
            std::fs::canonicalize(p)
                .unwrap()
                .to_string_lossy()
                .to_string()
        }),
    );
    std::fs::write(&config_path, serde_json::to_string_pretty(&config)?)?;

    viewport.print_line_above(
        Line::from(Span::styled(
            format!(
                "✓ Installed and registered {} in {}",
                args.crate_name,
                config_path.display()
            ),
            Style::default().fg(Color::Green),
        )),
    )?;
    viewport.draw(2, |_| {})?;
    terminal::cleanup()?;
    Ok(())
}

pub async fn run_update() -> Result<(), BoxError> {
    let config_path = user_config_path()?;
    let config = load_config(&config_path);
    let installable = config.installable_commands();

    if installable.is_empty() {
        eprintln!(
            "No RAP tools with recorded sources found in {}",
            config_path.display()
        );
        return Ok(());
    }

    cterm::enable_raw_mode()?;
    let mut viewport = InlineViewport::new(2)?;

    viewport.print_line_above(
        Line::from(Span::styled(
            format!("Updating {} RAP tool(s)...", installable.len()),
            Style::default().fg(Color::Yellow),
        )),
    )?;

    let mut failed = Vec::new();
    for (_command, crate_name, git, path) in &installable {
        viewport.print_line_above(
            Line::from(Span::styled(
                format!("→ Updating {crate_name}..."),
                Style::default().fg(Color::Cyan),
            )),
        )?;
        viewport.draw(2, |_| {})?;

        match run_cargo_install(&mut viewport, crate_name, git.as_deref(), path.as_deref()).await {
            Ok(()) => {
                viewport.print_line_above(
                    Line::from(Span::styled(
                        format!("  ✓ {crate_name}"),
                        Style::default().fg(Color::Green),
                    )),
                )?;
            }
            Err(e) => {
                viewport.print_line_above(
                    Line::from(Span::styled(
                        format!("  ✗ {crate_name}: {e}"),
                        Style::default().fg(Color::Red),
                    )),
                )?;
                failed.push(crate_name.clone());
            }
        }
        viewport.draw(2, |_| {})?;
    }

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
