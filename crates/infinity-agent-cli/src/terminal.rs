use crossterm::{
    cursor,
    event::{self, Event, KeyCode, KeyModifiers},
    queue, terminal,
};
use infinity_agent_core::message::{InputMessage, InputMessageContent};
use rig::message::UserContent;
use std::io::Write;
use tokio::sync::mpsc;

type BoxError = Box<dyn std::error::Error + Send + Sync>;

pub enum DisplayEvent {
    /// Signals the start of a new model output stream.
    StartOutput {
        prefix: Option<String>,
    },
    TextChunk(String),
    ToolCall {
        name: String,
        args: serde_json::Value,
        prefix: Option<String>,
    },
    ToolResult {
        text: String,
        prefix: Option<String>,
    },
    Info(String),
    ResponseDone,
    UserInput(String),
    SubscriptionEvent {
        name: String,
        text: String,
        prefix: Option<String>,
    },
}

pub async fn run(
    input_tx: mpsc::UnboundedSender<(InputMessage, String)>,
    mut display_rx: mpsc::UnboundedReceiver<DisplayEvent>,
    thread_id: String,
) -> Result<(), BoxError> {
    let mut stdout = std::io::stdout();

    terminal::enable_raw_mode()?;
    let rows = terminal::size()?.1;
    set_scroll_region(&mut stdout, 0, rows.saturating_sub(3))?;
    queue!(stdout, cursor::MoveTo(0, rows.saturating_sub(3)))?;
    stdout.flush()?;

    output_line(
        &mut stdout,
        &format!("Infinity Agent CLI — thread {}", thread_id),
    )?;
    output_line(&mut stdout, "Type your messages below. Ctrl+C to exit.")?;
    output_line(&mut stdout, "")?;

    let mut input_buf = String::new();
    let mut mid_stream = false;

    draw_input_bar(&mut stdout, &input_buf)?;

    loop {
        tokio::select! {
            biased;

            evt = display_rx.recv() => {
                match evt.expect("agent loop terminated unexpectedly") {
                    DisplayEvent::StartOutput { prefix } => {
                        let mut buf = Vec::new();
                        queue!(buf, cursor::Hide)?;
                        if mid_stream {
                            queue!(buf, cursor::RestorePosition)?;
                            write!(buf, "\r\n")?;
                        }
                        queue!(buf, cursor::MoveTo(0, scroll_region_bottom()))?;
                        write!(buf, "\r\n")?;
                        if let Some(p) = prefix {
                            write!(buf, "{} ", p)?;
                        }
                        queue!(buf, cursor::SavePosition)?;
                        mid_stream = true;
                        draw_input_bar_into(&mut buf, &input_buf)?;
                        queue!(buf, cursor::Show)?;
                        stdout.write_all(&buf)?;
                        stdout.flush()?;
                    }
                    DisplayEvent::TextChunk(chunk) => {
                        let mut buf = Vec::new();
                        queue!(buf, cursor::Hide)?;
                        queue!(buf, cursor::RestorePosition)?;
                        let sanitized = chunk.replace('\n', "\r\n");
                        write!(buf, "{}", sanitized)?;
                        queue!(buf, cursor::SavePosition)?;
                        draw_input_bar_into(&mut buf, &input_buf)?;
                        queue!(buf, cursor::Show)?;
                        stdout.write_all(&buf)?;
                        stdout.flush()?;
                    }
                    DisplayEvent::ResponseDone => {
                        if mid_stream {
                            let mut buf = Vec::new();
                            queue!(buf, cursor::Hide)?;
                            queue!(buf, cursor::RestorePosition)?;
                            write!(buf, "\r\n")?;
                            mid_stream = false;
                            draw_input_bar_into(&mut buf, &input_buf)?;
                            queue!(buf, cursor::Show)?;
                            stdout.write_all(&buf)?;
                            stdout.flush()?;
                        }
                    }
                    DisplayEvent::ToolCall { name, args, prefix } => {
                        let mut buf = Vec::new();
                        queue!(buf, cursor::Hide)?;
                        if mid_stream {
                            queue!(buf, cursor::RestorePosition)?;
                            write!(buf, "\r\n")?;
                            mid_stream = false;
                        }
                        let prefix_str = prefix.map(|p| format!("{} ", p)).unwrap_or_default();
                        let line = format!("\n{}\x1b[34m◆ {}({})\x1b[0m", prefix_str, name, args);
                        output_line_into(&mut buf, &line)?;
                        draw_input_bar_into(&mut buf, &input_buf)?;
                        queue!(buf, cursor::Show)?;
                        stdout.write_all(&buf)?;
                        stdout.flush()?;
                    }
                    DisplayEvent::ToolResult { text, prefix } => {
                        let mut buf = Vec::new();
                        queue!(buf, cursor::Hide)?;
                        if mid_stream {
                            queue!(buf, cursor::RestorePosition)?;
                            write!(buf, "\r\n")?;
                            mid_stream = false;
                        }
                        let prefix_str = prefix.map(|p| format!("{} ", p)).unwrap_or_default();
                        output_line_into(&mut buf, &format!("\n{}\x1b[32m✓ {}\x1b[0m", prefix_str, text))?;
                        draw_input_bar_into(&mut buf, &input_buf)?;
                        queue!(buf, cursor::Show)?;
                        stdout.write_all(&buf)?;
                        stdout.flush()?;
                    }
                    DisplayEvent::Info(line) => {
                        let mut buf = Vec::new();
                        queue!(buf, cursor::Hide)?;
                        if mid_stream {
                            queue!(buf, cursor::RestorePosition)?;
                            write!(buf, "\r\n")?;
                            mid_stream = false;
                        }
                        output_line_into(&mut buf, &line)?;
                        draw_input_bar_into(&mut buf, &input_buf)?;
                        queue!(buf, cursor::Show)?;
                        stdout.write_all(&buf)?;
                        stdout.flush()?;
                    }
                    DisplayEvent::UserInput(text) => {
                        let mut buf = Vec::new();
                        queue!(buf, cursor::Hide)?;
                        if mid_stream {
                            queue!(buf, cursor::RestorePosition)?;
                            write!(buf, "\r\n")?;
                            mid_stream = false;
                        }
                        output_line_into(&mut buf, &format!("\n\x1b[1m> {}\x1b[0m", text))?;
                        draw_input_bar_into(&mut buf, &input_buf)?;
                        queue!(buf, cursor::Show)?;
                        stdout.write_all(&buf)?;
                        stdout.flush()?;
                    }
                    DisplayEvent::SubscriptionEvent { name, text, prefix } => {
                        let mut buf = Vec::new();
                        queue!(buf, cursor::Hide)?;
                        if mid_stream {
                            queue!(buf, cursor::RestorePosition)?;
                            write!(buf, "\r\n")?;
                            mid_stream = false;
                        }
                        let prefix_str = prefix.map(|p| format!("[{}] ", p)).unwrap_or_default();
                        output_line_into(&mut buf, &format!("\n{}\x1b[38;5;208m⚡{}: {}\x1b[0m", prefix_str, name, text))?;
                        draw_input_bar_into(&mut buf, &input_buf)?;
                        queue!(buf, cursor::Show)?;
                        stdout.write_all(&buf)?;
                        stdout.flush()?;
                    }
                }
            }

            _ = poll_crossterm_event() => {
                while event::poll(std::time::Duration::ZERO)? {
                    if let Event::Key(key) = event::read()? {
                        match (key.code, key.modifiers) {
                            (KeyCode::Char('c') | KeyCode::Char('d'), m) if m.contains(KeyModifiers::CONTROL) => {
                                cleanup(&mut stdout)?;
                                return Ok(());
                            }
                            (KeyCode::Enter, _) => {
                                let text = input_buf.trim().to_string();
                                input_buf.clear();
                                if !text.is_empty() {
                                    let msg = InputMessage {
                                        content: InputMessageContent::User(UserContent::text(&text)),
                                        group_id: thread_id.clone(),
                                        metadata: None,
                                        synthetic: None,
                                    };
                                    let _ = input_tx.send((msg, uuid::Uuid::new_v4().to_string()));
                                }
                                draw_input_bar(&mut stdout, &input_buf)?;
                            }
                            (KeyCode::Backspace, _) => {
                                input_buf.pop();
                                draw_input_bar(&mut stdout, &input_buf)?;
                            }
                            (KeyCode::Char(ch), _) => {
                                input_buf.push(ch);
                                draw_input_bar(&mut stdout, &input_buf)?;
                            }
                            _ => {}
                        }
                    }
                }
            }
        }
    }
}

// ── Helpers ─────────────────────────────────────────────────────────────────

fn set_scroll_region(w: &mut impl Write, top: u16, bottom: u16) -> Result<(), BoxError> {
    write!(w, "\x1b[{};{}r", top + 1, bottom + 1)?;
    w.flush()?;
    Ok(())
}

fn output_line(w: &mut impl Write, text: &str) -> Result<(), BoxError> {
    write!(w, "{}\r\n", text)?;
    w.flush()?;
    Ok(())
}

fn output_line_into(buf: &mut Vec<u8>, text: &str) -> Result<(), BoxError> {
    let scroll_bottom = scroll_region_bottom();
    queue!(buf, cursor::MoveTo(0, scroll_bottom))?;
    write!(buf, "{}\r\n", text)?;
    Ok(())
}

fn scroll_region_bottom() -> u16 {
    let rows = terminal::size().map(|(_, r)| r).unwrap_or(24);
    rows.saturating_sub(3)
}

fn draw_input_bar(w: &mut impl Write, input_buf: &str) -> Result<(), BoxError> {
    let mut buf = Vec::new();
    draw_input_bar_into(&mut buf, input_buf)?;
    w.write_all(&buf)?;
    w.flush()?;
    Ok(())
}

fn draw_input_bar_into(buf: &mut Vec<u8>, input_buf: &str) -> Result<(), BoxError> {
    let (cols, rows) = terminal::size()?;
    let cols = cols.max(1) as usize;
    let prompt = format!("> {}", input_buf);
    // The cursor sits *after* the last character. If the text fills the line
    // exactly, the cursor wraps to column 0 of the next row, so we need an
    // extra row reserved for that.
    let cursor_pos = prompt.len();
    let cursor_row_offset = cursor_pos / cols;
    let text_lines = ((prompt.len().max(1) + cols - 1) / cols) as u16;
    // We need enough rows for the text *and* the cursor row.
    let input_lines = text_lines.max(cursor_row_offset as u16 + 1);
    let reserved = 1 + input_lines;
    let scroll_bottom = rows.saturating_sub(reserved + 1);

    write!(buf, "\x1b[{};{}r", 1, scroll_bottom + 1)?;

    let sep_row = rows.saturating_sub(reserved);
    queue!(buf, cursor::MoveTo(0, sep_row))?;
    let sep: String = "─".repeat(cols);
    write!(buf, "{}", sep)?;

    for i in 0..input_lines {
        let row = sep_row + 1 + i;
        let start = i as usize * cols;
        let end = ((i as usize + 1) * cols).min(prompt.len());
        let slice = if start < prompt.len() {
            &prompt[start..end]
        } else {
            ""
        };
        queue!(buf, cursor::MoveTo(0, row))?;
        write!(buf, "{:<width$}", slice, width = cols)?;
    }

    let cursor_row = sep_row + 1 + cursor_row_offset as u16;
    let cursor_col = (cursor_pos % cols) as u16;
    queue!(buf, cursor::MoveTo(cursor_col, cursor_row))?;
    Ok(())
}

fn cleanup(w: &mut impl Write) -> Result<(), BoxError> {
    write!(w, "\x1b[r")?;
    terminal::disable_raw_mode()?;
    let rows = terminal::size()?.1;
    queue!(w, cursor::MoveTo(0, rows))?;
    writeln!(w)?;
    w.flush()?;
    Ok(())
}

async fn poll_crossterm_event() {
    tokio::task::spawn_blocking(|| {
        let _ = event::poll(std::time::Duration::from_millis(50));
    })
    .await
    .ok();
}
