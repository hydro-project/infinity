use crate::{
    inline_viewport::InlineViewport,
    modifier_diff::ModifierDiff,
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
use std::fmt;
use std::io::{self, Write};
use std::time::Instant;
use tokio::sync::mpsc;

type BoxError = Box<dyn std::error::Error + Send + Sync>;

const VIEWPORT_HEIGHT: u16 = 2;

pub enum DisplayEvent<R> {
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
}

pub async fn run(
    input_tx: mpsc::UnboundedSender<(InputMessage, String)>,
    mut display_rx: mpsc::UnboundedReceiver<DisplayEvent<BedrockStreamingResponse>>,
    thread_id: String,
    model_name: String,
) -> Result<(), BoxError> {
    cterm::enable_raw_mode()?;

    let mut viewport = InlineViewport::new(VIEWPORT_HEIGHT)?;

    print_above(&mut viewport, |w| {
        write!(w, "\r\nInfinity Agent CLI — thread {}\r\n", thread_id)?;
        write!(w, "Type your messages below. Ctrl+C to exit.\r\n")
    })?;

    let mut input = TextInput::new();
    let mut mid_stream = false;
    let mut thinking = false;
    let mut thinking_start = Instant::now();
    let mut total_tokens_used = 0;

    draw_input_bar(
        &mut viewport,
        &input,
        thinking,
        &thinking_start,
        &model_name,
        total_tokens_used,
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
                    }
                    DisplayEvent::ThinkingEnd => {}
                    DisplayEvent::StartOutput { prefix } => {
                        end_stream(&mut viewport, &mut mid_stream)?;
                        mid_stream = true;
                        print_above(&mut viewport, |w| {
                            write!(w, "\r\n")?;
                            if let Some(p) = prefix {
                                write!(w, "{} ", p)?;
                            }
                            Ok(())
                        })?;
                    }
                    DisplayEvent::TextChunk(chunk) => {
                        let sanitized = chunk.replace('\n', "\r\n");
                        print_above(&mut viewport, |w| write!(w, "{}", sanitized))?;
                    }
                    DisplayEvent::ResponseDone(prefix, r) => {
                        if prefix.is_none() {
                            let usage = r.usage.unwrap();
                            total_tokens_used += usage.total_tokens as usize;
                        }
                        end_stream(&mut viewport, &mut mid_stream)?;
                        thinking = false;
                    }
                    DisplayEvent::ToolCall { name, args, prefix } => {
                        end_stream(&mut viewport, &mut mid_stream)?;
                        let pfx = prefix.map(|p| format!("{} ", p)).unwrap_or_default();
                        print_line_above(&mut viewport, Line::from(vec![
                            Span::raw(pfx),
                            Span::styled(format!("◆ {}({})", name, args), Style::default().fg(Color::Blue)),
                        ]))?;
                    }
                    DisplayEvent::ToolResult { text, display_as, prefix } => {
                        end_stream(&mut viewport, &mut mid_stream)?;
                        let pfx = prefix.map(|p| format!("{} ", p)).unwrap_or_default();
                        let display_text = display_as.as_deref().unwrap_or(&text);
                        let lines: Vec<&str> = display_text.lines().collect();
                        if let Some((first, rest)) = lines.split_first() {
                            print_line_above(&mut viewport, Line::from(vec![
                                Span::raw(pfx.clone()),
                                Span::styled(format!("✓ {}", first), Style::default().fg(Color::Green)),
                            ]))?;
                            let indent = format!("{}  ", pfx);
                            for line in rest {
                                print_line_above(&mut viewport, Line::from(vec![
                                    Span::raw(indent.clone()),
                                    Span::styled(line.to_string(), Style::default().fg(Color::Green)),
                                ]))?;
                            }
                        } else {
                            print_line_above(&mut viewport, Line::from(vec![
                                Span::raw(pfx),
                                Span::styled("✓", Style::default().fg(Color::Green)),
                            ]))?;
                        }

                        thinking = true;
                        thinking_start = Instant::now();
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
                        print_line_above(&mut viewport, Line::from(vec![
                            Span::raw(pfx),
                            Span::styled(format!("⚡{}: {}", name, text), Style::default().fg(Color::Indexed(208))),
                        ]))?;
                    }
                }
                draw_input_bar(&mut viewport, &input, thinking, &thinking_start, &model_name, total_tokens_used)?;
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
                            // Let the input area handle the keystroke first.
                            if !input.handle_keystroke(key) {
                                // Input didn't consume it — handle at the terminal level.
                                match (key.code, key.modifiers) {
                                    (KeyCode::Char('c') | KeyCode::Char('d'), m) if m.contains(KeyModifiers::CONTROL) => {
                                        cleanup()?;
                                        return Ok(());
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
                                            };
                                            let _ = input_tx.send((msg, uuid::Uuid::new_v4().to_string()));
                                        }
                                    }
                                    _ => {}
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
                    draw_input_bar(&mut viewport, &input, thinking, &thinking_start, &model_name, total_tokens_used)?;
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

fn end_stream(viewport: &mut InlineViewport, mid_stream: &mut bool) -> Result<(), BoxError> {
    if *mid_stream {
        print_above(viewport, |w| write!(w, "\r\n"))?;
        *mid_stream = false;
    }
    Ok(())
}

// ── Viewport drawing ────────────────────────────────────────────────────────

fn draw_input_bar(
    viewport: &mut InlineViewport,
    input: &TextInput,
    thinking: bool,
    thinking_start: &Instant,
    model_name: &str,
    total_tokens_used: usize,
) -> Result<(), BoxError> {
    const MAX_CONTEXT: usize = 200_000;

    let current_width = viewport.area().width;
    let input_height = input.preferred_height(current_width);
    let mut desired_lines = 1 + input_height + 1; // border + input + status row

    // Add one row for the thinking animation bar
    if thinking {
        desired_lines += 1;
    }

    let pct = if MAX_CONTEXT > 0 {
        ((total_tokens_used as f64 / MAX_CONTEXT as f64) * 100.0).min(100.0)
    } else {
        0.0
    };
    let status_right = format!("{:.0}% context used", pct);
    let status_left = model_name.to_string();

    viewport.draw(desired_lines, |frame| {
        let area = frame.area();

        if thinking {
            let [thinking_area, sep_area, input_area, status_area] = Layout::vertical([
                Constraint::Length(1),
                Constraint::Length(1),
                Constraint::Min(1),
                Constraint::Length(1),
            ])
            .areas(area);

            render_thinking_bar(frame, thinking_area, thinking_start);
            frame.render_widget(Block::default().borders(Borders::TOP), sep_area);

            let mut cursor_pos = None;
            frame.render_widget(TextInputWidget::new(input, &mut cursor_pos), input_area);
            if let Some(pos) = cursor_pos {
                frame.set_cursor_position(pos);
            }

            render_status_row(frame, status_area, &status_left, &status_right);
        } else {
            let [sep_area, input_area, status_area] = Layout::vertical([
                Constraint::Length(1),
                Constraint::Min(1),
                Constraint::Length(1),
            ])
            .areas(area);

            frame.render_widget(Block::default().borders(Borders::TOP), sep_area);

            let mut cursor_pos = None;
            frame.render_widget(TextInputWidget::new(input, &mut cursor_pos), input_area);
            if let Some(pos) = cursor_pos {
                frame.set_cursor_position(pos);
            }

            render_status_row(frame, status_area, &status_left, &status_right);
        }
    })?;
    Ok(())
}

/// Render an 8-column pulsing purple gradient bar fixed at the left edge.
/// Each column uses a different block character to simulate wave height,
/// with a sine wave rippling across the columns over time.
fn render_thinking_bar(
    frame: &mut crate::inline_viewport::ViewportFrame,
    area: ratatui::layout::Rect,
    thinking_start: &Instant,
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

    struct ThinkingWidget {
        elapsed_s: f64,
    }

    impl Widget for ThinkingWidget {
        fn render(self, area: ratatui::layout::Rect, buf: &mut Buffer) {
            let cols = (area.width as usize).min(GRADIENT.len());
            let y = area.y;
            for col in 0..cols {
                // Sine wave: each column is phase-shifted so it ripples across
                let phase = self.elapsed_s * std::f64::consts::TAU / 1.2 - (col as f64) * 0.7;
                let wave = (phase.sin() * 0.5 + 0.5) as f32; // 0.0 → 1.0

                // Pick block character based on wave height
                let block_idx = (wave * (BLOCKS.len() - 1) as f32).round() as usize;
                let ch = BLOCKS[block_idx.min(BLOCKS.len() - 1)];

                // Brightness also follows the wave
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

    frame.render_widget(ThinkingWidget { elapsed_s }, area);
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
