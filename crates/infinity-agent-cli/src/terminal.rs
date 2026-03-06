use crate::{
    component::{Component, KeyResult},
    inline_viewport::InlineViewport,
    modifier_diff::ModifierDiff,
    session_picker::{SessionPicker, SessionPickerResult, SessionPickerWidget},
    session_store::SessionEntry,
    text_input::{TextInput, TextInputWidget},
};
use infinity_agent_core::message::{InputMessage, InputMessageContent};
use ratatui::{
    crossterm::{
        Command, cursor,
        event::{self, Event, KeyCode, KeyModifiers},
        queue,
        style::{
            Attribute as CAttribute, Color as CColor, Colors, Print, SetAttribute,
            SetBackgroundColor, SetColors, SetForegroundColor,
        },
        terminal::{self as cterm},
    },
    layout::{Constraint, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders},
};
use rig::message::UserContent;
use rig_bedrock::streaming::BedrockStreamingResponse;
use std::collections::{BTreeMap, HashSet};
use std::fmt;
use std::io::{self, Write};
use std::time::Instant;
use tokio::sync::mpsc;

/// The current UI mode determines which component is active.
enum UiMode {
    /// Normal input mode.
    Normal,
    /// Session picker overlay is visible.
    SessionPicker,
}

type BoxError = Box<dyn std::error::Error + Send + Sync>;

const VIEWPORT_HEIGHT: u16 = 2;

pub enum DisplayEvent<R> {
    StartOutput {
        prefix: Option<String>,
    },
    TextChunk {
        prefix: Option<String>,
        chunk: String,
    },
    ToolCall {
        name: String,
        args: serde_json::Value,
        prefix: Option<String>,
    },
    ToolResult {
        text: String,
        display_as: Option<String>,
        prefix: Option<String>,
    },
    Info(String),
    ResponseDone(Option<String>, R),
    UserInput(String),
    SubscriptionEvent {
        name: String,
        text: String,
        prefix: Option<String>,
    },
    ThinkingStart,
    ThinkingEnd,
    ThinkingChunk {
        prefix: Option<String>,
        chunk: String,
    },
}

pub async fn run(
    input_tx: mpsc::UnboundedSender<(InputMessage, String)>,
    mut display_rx: mpsc::UnboundedReceiver<DisplayEvent<BedrockStreamingResponse>>,
    thread_id: String,
    model_name: String,
    new_session_tx: mpsc::UnboundedSender<String>,
    sessions: Vec<SessionEntry>,
    load_session_tx: mpsc::UnboundedSender<String>,
) -> Result<usize, BoxError> {
    cterm::enable_raw_mode()?;

    let mut viewport = InlineViewport::new(VIEWPORT_HEIGHT)?;
    let mut thread_id = thread_id;
    let has_sessions = !sessions.is_empty();

    print_above(&mut viewport, |w| {
        write!(w, "\r\nInfinity Agent CLI — thread {}\r\n", thread_id)?;
        write!(
            w,
            "Type your messages below. Ctrl+C to exit. Ctrl+N for new session.\r\n"
        )
    })?;
    if has_sessions {
        print_line_above(
            &mut viewport,
            Line::from(Span::styled(
                "Existing sessions found, Ctrl+L to load.",
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            )),
        )?;
    }

    let mut input = TextInput::new();
    let mut ui_mode = UiMode::Normal;
    let mut session_picker: Option<SessionPicker> = None;
    let available_sessions = sessions;
    let mut mid_stream = false;
    let mut stream_start = true;
    let mut thinking = false;
    let mut thinking_start = Instant::now();
    let mut total_tokens_used = 0;
    let mut thread_buffers: BTreeMap<String, String> = BTreeMap::new();
    let mut thread_tool_call_active: HashSet<String> = HashSet::new();
    let mut thinking_text_buffer = String::new();

    draw_viewport(
        &mut viewport,
        &input,
        &session_picker,
        &ui_mode,
        thinking,
        &thinking_start,
        &model_name,
        total_tokens_used,
        &thread_buffers,
        &thinking_text_buffer,
    )?;

    loop {
        // When thinking, tick every 50ms for animation; otherwise wait indefinitely
        let tick_timeout = if thinking {
            tokio::time::sleep(tokio::time::Duration::from_millis(16))
        } else {
            // Sleep forever (effectively disabled)
            tokio::time::sleep(tokio::time::Duration::from_secs(86400))
        };
        tokio::pin!(tick_timeout);

        tokio::select! {
            biased;

            evt = display_rx.recv() => {
                let evt = evt.expect("agent loop terminated unexpectedly");
                match evt {
                    DisplayEvent::ThinkingStart => {
                        thinking = true;
                        thinking_start = Instant::now();
                        thinking_text_buffer.clear();
                    }
                    DisplayEvent::ThinkingEnd => {
                        thinking_text_buffer.clear();
                    }
                    DisplayEvent::ThinkingChunk { prefix, chunk } => {
                        if prefix.is_none() {
                            thinking_text_buffer.push_str(&chunk);
                        }
                    }
                    DisplayEvent::StartOutput { prefix } => {
                        if prefix.is_none() {
                            end_stream(&mut viewport, &mut mid_stream)?;
                            stream_start = true;
                        } else if let Some(p) = prefix {
                            thread_buffers.entry(p).or_default();
                        }
                    }
                    DisplayEvent::TextChunk {
                        prefix,
                        chunk
                    } => {
                        if prefix.is_none() {
                            let chunk = if stream_start {
                                stream_start = false;
                                chunk.trim_start().to_string()
                            } else {
                                chunk
                            };

                            if !chunk.is_empty() {
                                let first_chunk = !mid_stream;
                                mid_stream = true;
                                let sanitized = chunk.replace('\n', "\r\n");
                                print_above(&mut viewport, |w| {
                                    if first_chunk {
                                        write!(w, "\r\n")?;
                                    }
                                    write!(w, "{}", sanitized)
                                })?;
                            }
                        } else if let Some(p) = prefix {
                            thread_buffers.entry(p).or_default().push_str(&chunk);
                        }
                    }
                    DisplayEvent::ResponseDone(prefix, r) => {
                        if prefix.is_none() {
                            let usage = r.usage.unwrap();
                            total_tokens_used = usage.total_tokens as usize;
                            // end_stream(&mut viewport, &mut mid_stream)?;
                            thinking = false;
                        } else if let Some(p) = prefix {
                            if !thread_tool_call_active.contains(&p) {
                                thread_buffers.remove(&p);
                            }
                        }
                    }
                    DisplayEvent::ToolCall { name, args, prefix } => {
                        end_stream(&mut viewport, &mut mid_stream)?;

                        if prefix.is_none() {
                            print_line_above(&mut viewport, Line::from(vec![
                                Span::styled(format!("◆ {}({})", name, args), Style::default().fg(Color::Blue)),
                            ]))?;
                            mid_stream = true;
                        } else if let Some(p) = prefix {
                            *thread_buffers.entry(p.clone()).or_default() = format!("\n◆ {}({})", name, args);

                            if name != "close_thread" { // never gets a response
                                thread_tool_call_active.insert(p);
                            } else {
                                thread_buffers.remove(&p);
                            }
                        }
                    }
                    DisplayEvent::ToolResult { text, display_as, prefix } => {
                        if prefix.is_none() {
                            let display_text = display_as.as_deref().unwrap_or(&text);
                            let lines: Vec<&str> = display_text.lines().collect();

                            if lines.len() <= 1 {
                                // Single line: print directly after tool call on same line
                                let first = lines.first().copied().unwrap_or("");
                                let result_line = Line::from(vec![
                                    Span::styled(format!(" ✓ {}", first), Style::default().fg(Color::Green)),
                                ]);
                                print_above(&mut viewport, |w| {
                                    write_spans(w, result_line.iter())
                                })?;
                                mid_stream = false;
                            } else {
                                // Multi line: end stream, print check + first line, then remaining lines
                                // end_stream(&mut viewport, &mut mid_stream)?;
                                mid_stream = false;
                                let first = lines.first().copied().unwrap_or("");
                                print_line_above(&mut viewport, Line::from(vec![
                                    Span::styled("✓ ", Style::default().fg(Color::Green)),
                                    Span::styled(first.to_string(), Style::default().fg(Color::DarkGray)),
                                ]))?;
                                print_continuation_lines(
                                    &mut viewport,
                                    &lines[1..],
                                    2,
                                    Style::default().fg(Color::DarkGray),
                                )?;
                                print_line_above(&mut viewport, Line::from(vec![]))?;
                            }

                            thinking = true;
                            thinking_start = Instant::now();
                        } else if let Some(p) = prefix {
                            thread_tool_call_active.remove(&p);
                        }
                    }
                    DisplayEvent::Info(text) => {
                        end_stream(&mut viewport, &mut mid_stream)?;
                        print_line_above(&mut viewport, Line::from(text))?;
                    }
                    DisplayEvent::UserInput(text) => {
                        let sanitized = text.replace('\n', "\r\n");
                        end_stream(&mut viewport, &mut mid_stream)?;
                        print_line_above(&mut viewport, Line::from(vec![
                            Span::styled(format!("> {}", sanitized), Style::default().add_modifier(Modifier::BOLD)),
                        ]))?;

                        thinking = true;
                        thinking_start = Instant::now();
                    }
                    DisplayEvent::SubscriptionEvent { name, text, prefix } => {
                        end_stream(&mut viewport, &mut mid_stream)?;
                        let pfx = prefix.map(|p| format!("[{}] ", p)).unwrap_or_default();
                        let lines: Vec<&str> = text.lines().collect();
                        if lines.len() <= 1 {
                            // Single line: print inline
                            let first = lines.first().copied().unwrap_or("");
                            print_line_above(&mut viewport, Line::from(vec![
                                Span::raw(pfx),
                                Span::styled(format!("⚡{}: {}", name, first), Style::default().fg(Color::Indexed(208))),
                            ]))?;
                        } else {
                            // Multi line: print header, then all lines from next line
                            print_line_above(&mut viewport, Line::from(vec![
                                Span::raw(pfx),
                                Span::styled(format!("⚡{}:", name), Style::default().fg(Color::Indexed(208))),
                            ]))?;
                            print_continuation_lines(
                                &mut viewport,
                                &lines,
                                2,
                                Style::default().fg(Color::Indexed(208)),
                            )?;
                        }

                        thinking = true;
                        thinking_start = Instant::now();
                    }
                }
                draw_viewport(&mut viewport, &input, &session_picker, &ui_mode, thinking, &thinking_start, &model_name, total_tokens_used, &thread_buffers, &thinking_text_buffer)?;
            }

            _ = poll_crossterm_event() => {
                let mut got_resize = false;
                let mut any_change = false;
                while event::poll(std::time::Duration::ZERO)? {
                    any_change = true;
                    let event = event::read()?;
                    if got_resize {
                        if matches!(event, Event::Resize(_, _)) {
                            continue;
                        } else {
                            viewport.handle_resize()?;
                            got_resize = false;
                        }
                    }

                    match event {
                        Event::Resize(_, _) => {
                            got_resize = true;
                        }
                        Event::Key(key) => {
                            // Route keystrokes based on the current UI mode.
                            match ui_mode {
                                UiMode::SessionPicker => {
                                    if let Some(ref mut picker) = session_picker {
                                        picker.handle_keystroke(key);
                                        // Check if the picker produced a result.
                                        if let Some(result) = picker.take_result() {
                                            match result {
                                                SessionPickerResult::Selected(entry) => {
                                                    let selected_thread = entry.thread_id.clone();
                                                    let selected_tokens = entry.total_tokens_used;
                                                    thread_id = selected_thread.clone();
                                                    total_tokens_used = selected_tokens;
                                                    let _ = load_session_tx.send(selected_thread.clone());
                                                    print_line_above(&mut viewport, Line::from(vec![
                                                        Span::styled(
                                                            format!("✦ Loaded session — thread {}", selected_thread),
                                                            Style::default().fg(Color::Yellow),
                                                        ),
                                                    ]))?;
                                                }
                                                SessionPickerResult::Cancelled => {}
                                            }
                                            session_picker = None;
                                            ui_mode = UiMode::Normal;
                                        }
                                    }
                                }
                                UiMode::Normal => {
                                    // Let the input area handle the keystroke first.
                                    if matches!(input.handle_keystroke(key), KeyResult::NotCaptured) {
                                        // Input didn't consume it — handle at the terminal level.
                                        match (key.code, key.modifiers) {
                                            (KeyCode::Char('c') | KeyCode::Char('d'), m) if m.contains(KeyModifiers::CONTROL) => {
                                                cleanup()?;
                                                return Ok(total_tokens_used);
                                            }
                                            (KeyCode::Char('h'), m) if m.contains(KeyModifiers::CONTROL) => {
                                                const W: usize = 47;
                                                let bar: String = "─".repeat(W);
                                                let rows = [
                                                    "",
                                                    "  Navigation",
                                                    "    Ctrl+C / Ctrl+D    Exit",
                                                    "    Ctrl+N             New session",
                                                    "    Ctrl+L             Load session",
                                                    "    Ctrl+H             Show this help",
                                                    "    Enter              Send message",
                                                    "",
                                                    "  Editing",
                                                    "    Alt+Enter          Insert newline",
                                                    "    Up                 Move cursor up a line",
                                                    "    Down               Move cursor down a line",
                                                    "    Ctrl+A             Move to line start",
                                                    "    Ctrl+E             Move to line end",
                                                    "    Alt+Left / Alt+B   Move word left",
                                                    "    Alt+Right / Alt+F  Move word right",
                                                    "    Alt+Backspace      Delete word left",
                                                    "    Ctrl+C             Clear input (non-empty)",
                                                    "",
                                                ];
                                                let mut help: Vec<String> = Vec::new();
                                                help.push(format!("╭{bar}╮"));
                                                for row in rows {
                                                    help.push(format!("│{:<W$}│", row));
                                                }
                                                help.push(format!("╰{bar}╯"));
                                                for line in &help {
                                                    print_line_above(&mut viewport, Line::from(vec![
                                                        Span::styled(line.clone(), Style::default().fg(Color::Cyan)),
                                                    ]))?;
                                                }
                                            }
                                            (KeyCode::Char('l'), m) if m.contains(KeyModifiers::CONTROL) => {
                                                if !available_sessions.is_empty() {
                                                    session_picker = Some(SessionPicker::new(available_sessions.clone()));
                                                    ui_mode = UiMode::SessionPicker;
                                                }
                                            }
                                            (KeyCode::Char('n'), m) if m.contains(KeyModifiers::CONTROL) => {
                                                let new_id = uuid::Uuid::new_v4().to_string();
                                                let _ = new_session_tx.send(new_id.clone());
                                                thread_id = new_id.clone();
                                                print_line_above(&mut viewport, Line::from(vec![
                                                    Span::styled(
                                                        format!("✦ New session created — thread {}", new_id),
                                                        Style::default().fg(Color::Yellow),
                                                    ),
                                                ]))?;
                                                total_tokens_used = 0;
                                            }
                                            (KeyCode::Enter, _) => {
                                                if !input.is_empty() {
                                                    let text = input.take_text();
                                                    let trimmed = text.trim().to_string();
                                                    let msg = InputMessage {
                                                        content: InputMessageContent::User(UserContent::text(&trimmed)),
                                                        group_id: thread_id.clone(),
                                                        metadata: None,
                                                        synthetic: None,
                                                        display_as: None,
                                                        subscription: false,
                                                    };
                                                    let _ = input_tx.send((msg, uuid::Uuid::new_v4().to_string()));
                                                }
                                            }
                                            _ => {}
                                        }
                                    }
                                }
                            }
                        }
                        _ => {}
                    }
                }

                if got_resize {
                    viewport.handle_resize()?;
                }

                if got_resize || any_change || thinking {
                    draw_viewport(&mut viewport, &input, &session_picker, &ui_mode, thinking, &thinking_start, &model_name, total_tokens_used, &thread_buffers, &thinking_text_buffer)?;
                }
            }

            _ = &mut tick_timeout, if thinking => {
                // Animation tick — redraw the input bar to advance the gradient

            }
        }
    }
}

// ── Scroll-region helpers ───────────────────────────────────────────────────

fn print_above(
    viewport: &mut InlineViewport,
    writer: impl FnOnce(&mut io::Stdout) -> io::Result<()>,
) -> Result<(), BoxError> {
    let vp_top = viewport.scroll_region_bottom();
    let mut stdout = io::stdout();

    queue!(stdout, cursor::Hide)?;
    queue!(stdout, SetScrollRegion(1..vp_top))?;
    queue!(stdout, cursor::RestorePosition)?;

    writer(&mut stdout)?;

    queue!(stdout, cursor::SavePosition)?;
    queue!(stdout, ResetScrollRegion)?;
    queue!(stdout, cursor::RestorePosition)?;

    stdout.flush()?;

    let cursor_position = cursor::position().unwrap();
    viewport.viewport_y = cursor_position.1 + 1;

    // don't show the cursor here or flush, that will be handled in printing input bar

    Ok(())
}

fn print_line_above(viewport: &mut InlineViewport, line: Line<'_>) -> Result<(), BoxError> {
    print_above(viewport, |w| {
        write!(w, "\r\n")?;
        write_spans(w, line.iter())
    })
}

/// Print continuation lines with consistent indentation and diff-aware coloring.
fn print_continuation_lines(
    viewport: &mut InlineViewport,
    lines: &[&str],
    indent: usize,
    base_style: Style,
) -> Result<(), BoxError> {
    for line in lines {
        let style = if line.starts_with("- ") {
            Style::default().fg(Color::Red)
        } else if line.starts_with("+ ") {
            Style::default().fg(Color::Green)
        } else if line.starts_with("@@") {
            Style::default().fg(Color::Cyan)
        } else {
            base_style
        };
        print_line_above(
            viewport,
            Line::from(vec![
                Span::raw(" ".repeat(indent)),
                Span::styled(line.to_string(), style),
            ]),
        )?;
    }
    Ok(())
}

fn end_stream(viewport: &mut InlineViewport, mid_stream: &mut bool) -> Result<(), BoxError> {
    if *mid_stream {
        print_above(viewport, |w| write!(w, "\r\n"))?;
        *mid_stream = false;
    }
    Ok(())
}

// ── Viewport drawing ────────────────────────────────────────────────────────

fn draw_viewport(
    viewport: &mut InlineViewport,
    input: &TextInput,
    session_picker: &Option<SessionPicker>,
    ui_mode: &UiMode,
    thinking: bool,
    thinking_start: &Instant,
    model_name: &str,
    total_tokens_used: usize,
    thread_buffers: &BTreeMap<String, String>,
    thinking_text: &str,
) -> Result<(), BoxError> {
    const MAX_CONTEXT: usize = 200_000;
    let current_width = viewport.area().width;
    let thread_rows = thread_buffers.len() as u16;

    // Determine status bar text based on mode.
    let pct = if MAX_CONTEXT > 0 {
        ((total_tokens_used as f64 / MAX_CONTEXT as f64) * 100.0).min(100.0)
    } else {
        0.0
    };
    let status_right = format!("{:.0}% context used", pct);
    let status_left = match ui_mode {
        UiMode::SessionPicker => "↑↓ navigate  enter select  esc cancel".to_string(),
        UiMode::Normal => format!("{} (ctrl-h for help)", model_name),
    };

    // Snapshot thread lines for the closure.
    let thread_lines: Vec<Line<'_>> = thread_buffers
        .iter()
        .map(|(id, buf)| {
            let prefix_len = id.chars().count() + 1;
            let avail = (current_width as usize).saturating_sub(prefix_len);
            let tail = wrap_tail(buf, avail);
            Line::from(vec![
                Span::styled(
                    format!("{} ", id),
                    Style::default().fg(Color::Rgb(130, 90, 200)),
                ),
                Span::styled(tail, Style::default().fg(Color::DarkGray)),
            ])
        })
        .collect();

    // Compute desired height based on mode.
    let (content_height, is_picker) = match ui_mode {
        UiMode::SessionPicker => {
            let picker_height = session_picker
                .as_ref()
                .map(|p| p.preferred_height())
                .unwrap_or(1);
            (picker_height, true)
        }
        UiMode::Normal => (input.preferred_height(current_width), false),
    };

    let mut desired_lines = thread_rows + 1 + content_height + 1; // threads + border + content + status
    if thinking {
        desired_lines += 1;
    }

    viewport.draw(desired_lines, |frame| {
        let area = frame.area();

        // Build constraints dynamically
        let mut constraints: Vec<Constraint> = Vec::new();
        if thinking {
            constraints.push(Constraint::Length(1));
        }
        if thread_rows > 0 {
            constraints.push(Constraint::Length(thread_rows));
        }
        constraints.push(Constraint::Length(1)); // border
        constraints.push(Constraint::Min(1)); // content (input or picker)
        constraints.push(Constraint::Length(1)); // status

        let areas = Layout::vertical(constraints).split(area);
        let mut idx = 0;

        // Thinking bar
        if thinking {
            render_thinking_bar(frame, areas[idx], thinking_start, thinking_text);
            idx += 1;
        }

        // Thread rows
        if thread_rows > 0 {
            let threads_area = areas[idx];
            idx += 1;
            for (i, line) in thread_lines.iter().enumerate() {
                if (i as u16) < threads_area.height {
                    let row = ratatui::layout::Rect {
                        x: threads_area.x,
                        y: threads_area.y + i as u16,
                        width: threads_area.width,
                        height: 1,
                    };
                    frame.render_widget(line.clone(), row);
                }
            }
        }

        // Border
        frame.render_widget(Block::default().borders(Borders::TOP), areas[idx]);
        idx += 1;

        // Content area — either session picker or input
        if is_picker {
            if let Some(picker) = session_picker {
                frame.render_widget(SessionPickerWidget::new(picker), areas[idx]);
            }
            // No cursor in picker mode
        } else {
            let mut cursor_pos = None;
            frame.render_widget(TextInputWidget::new(input, &mut cursor_pos), areas[idx]);
            if let Some(pos) = cursor_pos {
                frame.set_cursor_position(pos);
            }
        }
        idx += 1;

        // Status
        render_status_row(frame, areas[idx], &status_left, &status_right);
    })?;
    Ok(())
}

/// Render an 8-column pulsing purple gradient bar fixed at the left edge.
/// Each column uses a different block character to simulate wave height,
/// with a sine wave rippling across the columns over time.
/// Thinking text from the root thread is shown to the right of the spinner
/// in gray, using the same character-level wrapping as thread buffers.
fn render_thinking_bar(
    frame: &mut crate::inline_viewport::ViewportFrame,
    area: ratatui::layout::Rect,
    thinking_start: &Instant,
    thinking_text: &str,
) {
    use ratatui::buffer::Buffer;
    use ratatui::widgets::Widget;

    // Base purple gradient across 8 columns (dark edges → bright center)
    const GRADIENT: [(u8, u8, u8); 8] = [
        (60, 20, 80),
        (90, 30, 120),
        (130, 50, 170),
        (170, 70, 210),
        (190, 90, 240),
        (170, 70, 210),
        (130, 50, 170),
        (90, 30, 120),
    ];

    // Block characters from shortest to tallest
    const BLOCKS: [char; 5] = ['▁', '▂', '▄', '▆', '█'];

    if area.width == 0 {
        return;
    }

    let elapsed_s = thinking_start.elapsed().as_secs_f64();

    // Split the area: fixed spinner on the left, flexible text on the right.
    let spinner_width = (GRADIENT.len() as u16).min(area.width);
    let areas = Layout::horizontal([
        Constraint::Length(spinner_width),
        Constraint::Length(1), // gap
        Constraint::Min(0),    // text
    ])
    .split(area);
    let spinner_area = areas[0];
    let text_area = areas[2];

    // Render the spinner gradient (single-cell block chars written directly)
    struct SpinnerWidget {
        elapsed_s: f64,
    }

    impl Widget for SpinnerWidget {
        fn render(self, area: ratatui::layout::Rect, buf: &mut Buffer) {
            let cols = (area.width as usize).min(GRADIENT.len());
            let y = area.y;
            for col in 0..cols {
                let phase = self.elapsed_s * std::f64::consts::TAU / 1.2 - (col as f64) * 0.7;
                let wave = (phase.sin() * 0.5 + 0.5) as f32;

                let block_idx = (wave * (BLOCKS.len() - 1) as f32).round() as usize;
                let ch = BLOCKS[block_idx.min(BLOCKS.len() - 1)];

                let (r, g, b) = GRADIENT[col];
                let dim = 0.3_f32;
                let t = dim + (1.0 - dim) * wave;
                let r = (r as f32 * t) as u8;
                let g = (g as f32 * t) as u8;
                let b = (b as f32 * t) as u8;

                let x = area.x + col as u16;
                buf[(x, y)].set_char(ch).set_fg(Color::Rgb(r, g, b));
            }
        }
    }

    frame.render_widget(SpinnerWidget { elapsed_s }, spinner_area);

    // Render thinking text in the text area using Line/Span
    if text_area.width > 0 && !thinking_text.is_empty() {
        let tail = wrap_tail(thinking_text, text_area.width as usize);
        let line = Line::from(Span::styled(tail, Style::default().fg(Color::DarkGray)));
        frame.render_widget(line, text_area);
    }
}

/// Given a text buffer and an available column width, flatten newlines and
/// return the trailing tail that fits. Snaps to the nearest word boundary so
/// the display never starts mid-word. Shared by thread buffer lines and the
/// thinking text display.
fn wrap_tail(text: &str, avail: usize) -> String {
    let avail = avail.max(1);
    let flat = text.replace('\n', " ");
    let chars: Vec<char> = flat.chars().collect();
    if chars.len() <= avail {
        return flat;
    }

    // Start of the last `avail` characters.
    let cut = chars.len() - avail;

    // Scan forward from the cut to the next space so we don't start mid-word.
    let mut start = cut;
    while start < chars.len() && chars[start] != ' ' {
        start += 1;
    }
    // Skip past the space(s).
    while start < chars.len() && chars[start] == ' ' {
        start += 1;
    }

    // If we couldn't find a boundary (one giant word), hard-cut.
    if start >= chars.len() {
        return chars[cut..].iter().collect();
    }
    chars[start..].iter().collect()
}

fn render_status_row(
    frame: &mut crate::inline_viewport::ViewportFrame,
    area: ratatui::layout::Rect,
    left: &str,
    right: &str,
) {
    let w = area.width as usize;
    if w == 0 {
        return;
    }

    // Pad the middle so left and right are flush to their edges
    let left_len = left.chars().count().min(w);
    let right_len = right.chars().count().min(w.saturating_sub(left_len));
    let pad = w.saturating_sub(left_len + right_len);

    let line = Line::from(vec![
        Span::styled(
            &left[..left
                .char_indices()
                .nth(left_len)
                .map(|(i, _)| i)
                .unwrap_or(left.len())],
            Style::default().fg(Color::Rgb(140, 140, 140)),
        ),
        Span::raw(" ".repeat(pad)),
        Span::styled(
            &right[..right
                .char_indices()
                .nth(right_len)
                .map(|(i, _)| i)
                .unwrap_or(right.len())],
            Style::default().fg(Color::Rgb(140, 140, 140)),
        ),
    ]);

    frame.render_widget(line, area);
}

fn cleanup() -> Result<(), BoxError> {
    let mut stdout = io::stdout();
    queue!(stdout, ResetScrollRegion)?;
    cterm::disable_raw_mode()?;
    let rows = cterm::size()?.1;
    queue!(stdout, cursor::MoveTo(0, rows))?;
    writeln!(stdout)?;
    stdout.flush()?;
    Ok(())
}

async fn poll_crossterm_event() {
    tokio::task::spawn_blocking(|| {
        let _ = event::poll(std::time::Duration::from_millis(16));
    })
    .await
    .ok();
}

// ── Span writing ────────────────────────────────────────────────────────────

fn write_spans<'a>(
    w: &mut impl Write,
    spans: impl Iterator<Item = &'a Span<'a>>,
) -> io::Result<()> {
    let mut fg = Color::Reset;
    let mut bg = Color::Reset;
    let mut mods = Modifier::empty();

    for span in spans {
        let mut next = mods;
        next.insert(span.style.add_modifier);
        next.remove(span.style.sub_modifier);
        if next != mods {
            ModifierDiff {
                from: mods,
                to: next,
            }
            .queue(w)?;
            mods = next;
        }

        let nfg = span.style.fg.unwrap_or(Color::Reset);
        let nbg = span.style.bg.unwrap_or(Color::Reset);
        if nfg != fg || nbg != bg {
            queue!(w, SetColors(Colors::new(nfg.into(), nbg.into())))?;
            fg = nfg;
            bg = nbg;
        }

        queue!(w, Print(&span.content))?;
    }

    queue!(
        w,
        SetForegroundColor(CColor::Reset),
        SetBackgroundColor(CColor::Reset),
        SetAttribute(CAttribute::Reset),
    )
}

// ── Custom crossterm commands ───────────────────────────────────────────────

struct SetScrollRegion(std::ops::Range<u16>);

impl Command for SetScrollRegion {
    fn write_ansi(&self, f: &mut impl fmt::Write) -> fmt::Result {
        write!(f, "\x1b[{};{}r", self.0.start, self.0.end)
    }
}

struct ResetScrollRegion;

impl Command for ResetScrollRegion {
    fn write_ansi(&self, f: &mut impl fmt::Write) -> fmt::Result {
        write!(f, "\x1b[r")
    }
}
