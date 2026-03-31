use crate::{
    choice_picker::{ChoicePicker, ChoicePickerResult, ChoicePickerWidget},
    component::{Component, KeyResult},
    inline_viewport::{InlineViewport, ResetScrollRegion},
    model_picker::{ModelPicker, ModelPickerResult, ModelPickerWidget},
    quit_picker::{QuitPicker, QuitPickerResult, QuitPickerWidget},
    session_picker::{SessionPicker, SessionPickerResult, SessionPickerWidget},
    text_input::{TextInput, TextInputWidget},
};
use infinity_agent_core::batch_processor::DisplayEvent;
use ratatui::{
    crossterm::{
        cursor,
        event::{self, Event, KeyCode, KeyModifiers},
        queue,
        terminal::{self as cterm},
    },
    layout::{Constraint, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders},
};
use rig::completion::GetTokenUsage;
use std::collections::{BTreeMap, HashSet, VecDeque};
use std::io::{self, Write};
use std::time::Instant;
use tokio::sync::mpsc;

/// The current UI mode determines which component is active.
#[derive(PartialEq, Eq)]
enum UiMode {
    /// Normal input mode.
    Normal,
    /// Session picker overlay is visible.
    SessionPicker,
    /// Model picker overlay is visible.
    ModelPicker,
    /// Quit/switch picker overlay is visible.
    QuitPicker,
    /// User choice picker overlay is visible.
    ChoicePicker,
}

/// Spinner animation state — only affected by root-thread events.
#[derive(Clone, Copy, PartialEq)]
pub(crate) enum SpinnerState {
    /// After StartOutput but before any thinking/text/tool call.
    LoadingContext,
    /// Model is thinking or emitting text.
    Thinking,
    /// Waiting for a tool call result.
    WaitingToolCall,
}

type BoxError = Box<dyn std::error::Error + Send + Sync>;

const VIEWPORT_HEIGHT: u16 = 2;

/// Slash commands available for autocomplete hints.
const SLASH_COMMANDS: &[(&str, &str)] = &[
    ("/help", "Show help"),
    ("/quit", "Exit"),
    ("/new", "New session"),
    ("/load", "Load session"),
    ("/model", "Switch model"),
    ("/compact", "Trigger compaction"),
    ("/stop", "Stop agent"),
];

pub struct SessionChanged {
    pub session_id: String,
    pub title: Option<String>,
    pub total_tokens_used: usize,
}

/// Result of a SoftDetach attempt, sent back from daemon_client to terminal.
pub enum DetachResult {
    /// Agent was idle — session already detached, proceed directly.
    Idle,
    /// Agent is not idle — show the quit picker.
    NotIdle,
}

enum SoftDetachAction {
    Quit,
    SwitchSession(Option<String>),
}

/// A queued user choice request waiting to be shown in the TUI.
struct PendingChoice {
    id: String,
    prompt: String,
    choices: Vec<String>,
    default: usize,
}

pub async fn run<R>(
    input_tx: mpsc::UnboundedSender<String>,
    mut display_rx: mpsc::UnboundedReceiver<(Option<String>, DisplayEvent<R>)>,
    mut model_name: String,
    mut context_window: usize,
    initial_sessions: std::collections::HashMap<String, infinity_protocol::SessionInfo>,
    load_session_tx: mpsc::UnboundedSender<(Option<String>, bool)>,
    model_switch_tx: mpsc::UnboundedSender<usize>,
    available_models: Vec<crate::model_picker::ModelEntry>,
    initial_message: Option<String>,
    mut session_rx: mpsc::UnboundedReceiver<SessionChanged>,
    mut sessions_updated_rx: mpsc::UnboundedReceiver<
        std::collections::HashMap<String, infinity_protocol::SessionInfo>,
    >,
    soft_detach_tx: mpsc::UnboundedSender<()>,
    mut detach_result_rx: mpsc::UnboundedReceiver<DetachResult>,
    choice_answered_tx: mpsc::UnboundedSender<(String, usize)>,
) -> Result<bool, BoxError>
where
    R: GetTokenUsage,
{
    cterm::enable_raw_mode()?;

    // Enable bracketed paste so multi-line pastes arrive as a single
    // Event::Paste rather than a stream of individual key events (which
    // would submit on the first newline).
    {
        let mut stdout_init = io::stdout();
        ratatui::crossterm::execute!(stdout_init, event::EnableBracketedPaste)?;
    }

    let mut viewport = InlineViewport::new(VIEWPORT_HEIGHT)?;
    let has_sessions = !initial_sessions.is_empty();
    let mut thread_id = None;

    viewport.print_above(|w| {
        write!(w, "\r\nInfinity Agent CLI\r\n")?;
        write!(
            w,
            "Type your messages below. /help for commands. Ctrl+C to exit.\r\n"
        )
    })?;
    if has_sessions {
        viewport.print_line_above(Line::from(Span::styled(
            "Existing sessions found, /load or Ctrl+L to load.",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )))?;
    }

    let mut input = TextInput::new();
    let mut ui_mode = UiMode::Normal;
    let mut session_picker: Option<SessionPicker> = None;
    let mut model_picker: Option<ModelPicker> = None;
    let mut quit_picker: Option<QuitPicker> = None;
    let mut choice_picker: Option<ChoicePicker> = None;
    let mut choice_queue: VecDeque<PendingChoice> = VecDeque::new();
    let mut sessions = initial_sessions;
    let mut mid_stream = false;
    let mut stream_start = true;
    let mut spinner_state: Option<SpinnerState> = None;
    let mut thinking_start = Instant::now();
    let mut total_tokens_used = 0;
    let mut thread_buffers: BTreeMap<String, String> = BTreeMap::new();
    let mut thread_tool_call_active: HashSet<String> = HashSet::new();
    let mut thinking_text_buffer = String::new();
    // Tab-completion state: (prefix that was typed, current selection index)
    let mut tab_complete: Option<(String, usize)> = None;
    let mut pending_soft_detach: Option<SoftDetachAction> = None;

    draw_viewport(
        &mut viewport,
        &input,
        &session_picker,
        &model_picker,
        &quit_picker,
        &choice_picker,
        &ui_mode,
        spinner_state,
        &thinking_start,
        &model_name,
        total_tokens_used,
        context_window,
        &thread_buffers,
        &thinking_text_buffer,
        &tab_complete,
        &thread_id,
    )?;

    // Send the initial message if provided via --message/-m.
    if let Some(text) = initial_message {
        let trimmed = text.trim().to_string();
        if !trimmed.is_empty() {
            let _ = input_tx.send(trimmed);
        }
    }

    loop {
        tokio::select! {
            biased;

            change = session_rx.recv() => {
                if let Some(change) = change {
                    thread_id = Some(change.session_id.clone());
                    total_tokens_used = change.total_tokens_used;
                    thread_buffers.clear(); // for now, we can't properly restore themn
                    set_terminal_title(change.title.as_deref().unwrap_or(""));
                    viewport.print_line_above(Line::from(""))?;
                    viewport.print_line_above(Line::from(Span::styled(
                        format!("✦ Loading session — thread {}", change.session_id),
                        Style::default().fg(Color::Cyan),
                    )))?;
                }
            }

            updates = sessions_updated_rx.recv() => {
                if let Some(updates) = updates {
                    // Update terminal title if the current session's title changed
                    if let Some(ref tid) = thread_id
                        && let Some(info) = updates.get(tid)
                            && let Some(ref title) = info.title {
                                set_terminal_title(title);
                            }

                    sessions.extend(updates);

                    if let Some(session_picker) = session_picker.as_mut() {
                        let last_picked_id = session_picker.sessions[session_picker.selected].0.clone();

                        let mut all_sessions: Vec<(String, infinity_protocol::SessionInfo)> = sessions.iter()
                            .map(|(id, info)| (id.clone(), info.clone()))
                            .collect();
                        all_sessions.sort_by(|a, b| b.1.last_updated.cmp(&a.1.last_updated));

                        session_picker.sessions = all_sessions;
                        if let Some(found) = session_picker.sessions.iter().position(|s| s.0 == last_picked_id) {
                            session_picker.selected = found;
                        }
                    }
                }
            }

            result = detach_result_rx.recv() => {
                if let Some(result) = result {
                    let action = pending_soft_detach.take();
                    match (result, action) {
                        (DetachResult::Idle, Some(SoftDetachAction::Quit)) => {
                            cleanup()?;
                            return Ok(true);
                        }
                        (DetachResult::Idle, Some(SoftDetachAction::SwitchSession(target))) => {
                            let _ = load_session_tx.send((target.clone(), false));
                            if target.is_none() {
                                viewport.print_line_above(Line::from(vec![
                                    Span::styled(
                                        "✦ Lazily creating a new session",
                                        Style::default().fg(Color::Yellow),
                                    ),
                                ]))?;
                            }
                            total_tokens_used = 0;
                            thread_buffers.clear();
                            thread_id = None;
                            spinner_state = None;
                        }
                        (DetachResult::NotIdle, Some(SoftDetachAction::Quit)) => {
                            quit_picker = Some(QuitPicker::new());
                            ui_mode = UiMode::QuitPicker;
                        }
                        (DetachResult::NotIdle, Some(SoftDetachAction::SwitchSession(target))) => {
                            let _ = load_session_tx.send((target.clone(), false));
                            if target.is_none() {
                                viewport.print_line_above(Line::from(vec![
                                    Span::styled(
                                        "✦ Lazily creating a new session",
                                        Style::default().fg(Color::Yellow),
                                    ),
                                ]))?;
                            }
                            total_tokens_used = 0;
                            thread_buffers.clear();
                            thread_id = None;
                            spinner_state = None;
                            ui_mode = UiMode::Normal;
                        }
                        _ => {}
                    }
                    draw_viewport(&mut viewport, &input, &session_picker, &model_picker, &quit_picker, &choice_picker, &ui_mode, spinner_state, &thinking_start, &model_name, total_tokens_used, context_window, &thread_buffers, &thinking_text_buffer, &tab_complete, &thread_id)?;
                }
            }

            evt = display_rx.recv() => {
                let Some((evt_thread_id, evt)) = evt else {
                    if pending_soft_detach.is_some() {
                        cleanup()?;
                        return Ok(true);
                    }
                    panic!("agent loop terminated unexpectedly");
                };
                let is_root = evt_thread_id.is_none() || evt_thread_id.as_ref() == thread_id.as_ref();
                // For non-root events, the thread_id is always Some.
                let child_tid = evt_thread_id.unwrap_or_default();
                match evt {
                    DisplayEvent::ThinkingStart => {
                        if is_root {
                            if spinner_state == Some(SpinnerState::LoadingContext) {
                                spinner_state = Some(SpinnerState::Thinking);
                            }
                            thinking_start = Instant::now();
                            thinking_text_buffer.clear();
                        }
                    }
                    DisplayEvent::ThinkingEnd => {
                        if is_root {
                            thinking_text_buffer.clear();
                        }
                    }
                    DisplayEvent::ThinkingChunk { chunk } => {
                        if is_root {
                            thinking_text_buffer.push_str(&chunk);
                        } else {
                            thread_buffers.entry(child_tid).or_default().push_str(&chunk);
                        }
                    }
                    DisplayEvent::StartOutput => {
                        if is_root {
                            end_stream(&mut viewport, &mut mid_stream)?;
                            stream_start = true;

                            thinking_text_buffer.clear();
                            if spinner_state == Some(SpinnerState::WaitingToolCall) {
                                spinner_state = Some(SpinnerState::Thinking);
                            } else {
                                spinner_state = Some(SpinnerState::LoadingContext);
                            }
                            thinking_start = Instant::now();
                        } else {
                            thread_buffers.entry(child_tid).or_default();
                        }
                    }
                    DisplayEvent::TextChunk {
                        chunk
                    } => {
                        if is_root {
                            if spinner_state.is_none() {
                                thinking_start = Instant::now();
                            }

                            spinner_state = Some(SpinnerState::Thinking);

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
                                viewport.print_above(|w| {
                                    if first_chunk {
                                        write!(w, "\r\n")?;
                                    }
                                    write!(w, "{}", sanitized)
                                })?;
                            }
                        } else {
                            thread_buffers.entry(child_tid).or_default().push_str(&chunk);
                        }
                    }
                    DisplayEvent::ResponseDone(r) => {
                        if is_root {
                            if let Some(r) = r {
                                total_tokens_used = r.token_usage().map_or(0, |u| u.total_tokens as usize);
                            }
                            if spinner_state != Some(SpinnerState::WaitingToolCall) {
                                spinner_state = None;
                            }
                        } else if !thread_tool_call_active.contains(&child_tid) {
                                thread_buffers.remove(&child_tid);
                            }
                    }
                    DisplayEvent::ToolCall { name, args, display_as } => {
                        end_stream(&mut viewport, &mut mid_stream)?;
                        thinking_text_buffer.clear();

                        let display_text = display_as
                            .unwrap_or_else(|| format!("{}({})", name, args));

                        if is_root {
                            spinner_state = Some(SpinnerState::WaitingToolCall);
                            thinking_text_buffer.push_str("waiting for tool call result");
                            thinking_start = Instant::now();
                            viewport.print_line_above(Line::from(vec![
                                Span::styled(format!("◆ {}", display_text), Style::default().fg(Color::Blue)),
                            ]))?;
                            mid_stream = true;
                        } else {
                            *thread_buffers.entry(child_tid.clone()).or_default() = format!("\n◆ {}", display_text);

                            if name != "close_thread" { // never gets a response
                                thread_tool_call_active.insert(child_tid);
                            } else {
                                thread_buffers.remove(&child_tid);
                            }
                        }
                    }
                    DisplayEvent::ToolResult { text, display_as } => {
                        if is_root {
                            let display_text = display_as.as_deref().unwrap_or(&text);
                            let lines: Vec<&str> = display_text.lines().collect();

                            if lines.len() <= 1 {
                                // Single line: print directly after tool call on same line
                                let first = lines.first().copied().unwrap_or("");
                                let result_line = Line::from(vec![
                                    Span::styled(format!(" ✓ {}", first), Style::default().fg(Color::Green)),
                                ]);
                                viewport.print_spans_above(result_line)?;
                                mid_stream = false;
                            } else {
                                // Multi line: end stream, print check + first line, then remaining lines
                                // end_stream(&mut viewport, &mut mid_stream)?;
                                mid_stream = false;
                                let first = lines.first().copied().unwrap_or("");
                                viewport.print_line_above(Line::from(vec![
                                    Span::styled("✓ ", Style::default().fg(Color::Green)),
                                    Span::styled(first.to_string(), Style::default().fg(Color::DarkGray)),
                                ]))?;
                                print_continuation_lines(
                                    &mut viewport,
                                    &lines[1..],
                                    2,
                                    Style::default().fg(Color::DarkGray),
                                )?;
                                viewport.print_line_above(Line::from(vec![]))?;
                            }
                        } else {
                            thread_tool_call_active.remove(&child_tid);
                        }
                    }
                    DisplayEvent::Info(text) => {
                        end_stream(&mut viewport, &mut mid_stream)?;
                        viewport.print_line_above(Line::from(text))?;
                    }
                    DisplayEvent::CompactionApplied => {
                        if is_root {
                            end_stream(&mut viewport, &mut mid_stream)?;
                            viewport.print_line_above(Line::from(vec![
                                Span::styled("✦ Compaction applied", Style::default().fg(Color::Magenta)),
                            ]))?;
                        }
                    }
                    DisplayEvent::UserInput(text) => {
                        let sanitized = text.replace('\n', "\r\n");
                        end_stream(&mut viewport, &mut mid_stream)?;
                        viewport.print_line_above(Line::from(vec![
                            Span::styled(format!("> {}", sanitized), Style::default().add_modifier(Modifier::BOLD)),
                        ]))?;

                        spinner_state = Some(SpinnerState::LoadingContext);
                        thinking_start = Instant::now();
                        stream_start = true; // any new assistant output is a start
                    }
                    DisplayEvent::SubscriptionEvent { name, text } => {
                        end_stream(&mut viewport, &mut mid_stream)?;
                        let pfx = if !is_root { format!("[{}] ", &child_tid[..child_tid.len().min(8)]) } else { String::new() };
                        let lines: Vec<&str> = text.lines().collect();
                        if lines.len() <= 1 {
                            // Single line: print inline
                            let first = lines.first().copied().unwrap_or("");
                            viewport.print_line_above(Line::from(vec![
                                Span::raw(pfx),
                                Span::styled(format!("⚡{}: {}", name, first), Style::default().fg(Color::Indexed(208))),
                            ]))?;
                        } else {
                            // Multi line: print header, then all lines from next line
                            viewport.print_line_above(Line::from(vec![
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

                        spinner_state = Some(SpinnerState::LoadingContext);
                        thinking_start = Instant::now();
                    }
                    DisplayEvent::OAuthRequired { auth_url } => {
                        end_stream(&mut viewport, &mut mid_stream)?;
                        viewport.print_line_above(Line::from(vec![
                            Span::styled(
                                format!("OAuth required — open this URL:\n  {}", auth_url),
                                Style::default().fg(Color::Yellow),
                            ),
                        ]))?;
                    }
                    DisplayEvent::UserChoiceRequired { id, prompt, choices, default, .. } => {
                        choice_queue.push_back(PendingChoice { id, prompt, choices, default });
                        // Show the first queued choice if no picker is active
                        if choice_picker.is_none() && ui_mode != UiMode::ChoicePicker
                            && let Some(pending) = choice_queue.front() {
                                choice_picker = Some(ChoicePicker::new(pending.prompt.clone(), pending.choices.clone(), pending.default));
                                ui_mode = UiMode::ChoicePicker;
                            }
                    }
                }
                draw_viewport(&mut viewport, &input, &session_picker, &model_picker, &quit_picker, &choice_picker, &ui_mode, spinner_state, &thinking_start, &model_name, total_tokens_used, context_window, &thread_buffers, &thinking_text_buffer, &tab_complete, &thread_id)?;
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
                        Event::Paste(text) => {
                            if matches!(ui_mode, UiMode::Normal) {
                                input.insert_str(&text);
                            }
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
                                                SessionPickerResult::Selected(session_id) => {
                                                    if thread_id.is_some() {
                                                        pending_soft_detach = Some(SoftDetachAction::SwitchSession(Some(session_id)));
                                                        let _ = soft_detach_tx.send(());
                                                    } else {
                                                        let _ = load_session_tx.send((Some(session_id), false));
                                                    }
                                                }
                                                SessionPickerResult::Cancelled => {
                                                    ui_mode = UiMode::Normal;
                                                }
                                            }
                                            session_picker = None;
                                            if ui_mode == UiMode::SessionPicker {
                                                ui_mode = UiMode::Normal;
                                            }
                                        }
                                    }
                                }
                                UiMode::ModelPicker => {
                                    if let Some(ref mut picker) = model_picker {
                                        picker.handle_keystroke(key);
                                        if let Some(result) = picker.take_result() {
                                            match result {
                                                ModelPickerResult::Selected(idx) => {
                                                    if let Some(entry) = available_models.get(idx) {
                                                        model_name = entry.display_name.to_string();
                                                        context_window = entry.context_window;
                                                        let _ = model_switch_tx.send(idx);
                                                        viewport.print_line_above(Line::from(vec![
                                                            Span::styled(
                                                                format!("✦ Switched to model: {}", entry.display_name),
                                                                Style::default().fg(Color::Yellow),
                                                            ),
                                                        ]))?;
                                                    }
                                                }
                                                ModelPickerResult::Cancelled => {}
                                            }
                                            model_picker = None;
                                            ui_mode = UiMode::Normal;
                                        }
                                    }
                                }
                                UiMode::QuitPicker => {
                                    if let Some(ref mut picker) = quit_picker {
                                        picker.handle_keystroke(key);
                                        if let Some(result) = picker.take_result() {
                                            quit_picker = None;
                                            match result {
                                                QuitPickerResult::ShutDown => {
                                                    cleanup()?;
                                                    return Ok(false);
                                                }
                                                QuitPickerResult::KeepRunning => {
                                                    cleanup()?;
                                                    return Ok(true);
                                                }
                                                QuitPickerResult::Cancelled => {
                                                    ui_mode = UiMode::Normal;
                                                }
                                            }
                                        }
                                    }
                                }
                                UiMode::ChoicePicker => {
                                    if let Some(ref mut picker) = choice_picker {
                                        picker.handle_keystroke(key);
                                        if let Some(result) = picker.take_result() {
                                            let ChoicePickerResult::Selected(idx) = result;
                                            if let Some(pending) = choice_queue.pop_front() {
                                                let _ = choice_answered_tx.send((pending.id, idx));
                                            }
                                            choice_picker = None;
                                            // Show next queued choice, or return to normal
                                            if let Some(next) = choice_queue.front() {
                                                choice_picker = Some(ChoicePicker::new(next.prompt.clone(), next.choices.clone(), next.default));
                                            } else {
                                                ui_mode = UiMode::Normal;
                                            }
                                        }
                                    }
                                }
                                UiMode::Normal => {
                                    // Handle Tab for autocomplete cycling.
                                    if key.code == KeyCode::Tab {
                                        let prefix = tab_complete.as_ref().map(|(p, _)| p.clone())
                                            .unwrap_or_else(|| input.text().trim().to_string());
                                        if prefix.starts_with('/') && !prefix.contains(' ') && !prefix.is_empty() {
                                            let matches: Vec<&str> = SLASH_COMMANDS
                                                .iter()
                                                .filter(|(cmd, _)| cmd.starts_with(&prefix))
                                                .map(|(cmd, _)| *cmd)
                                                .collect();
                                            if !matches.is_empty() {
                                                let idx = tab_complete.as_ref().map(|(_, i)| (i + 1) % matches.len()).unwrap_or(0);
                                                input.set_text(matches[idx]);
                                                tab_complete = Some((prefix, idx));
                                            }
                                        }
                                    } else {
                                    // Let the input area handle the keystroke first.
                                    let captured = input.handle_keystroke(key);
                                    if matches!(captured, KeyResult::Captured) {
                                        tab_complete = None;
                                    }
                                    if matches!(captured, KeyResult::NotCaptured) {
                                        // Map both Ctrl shortcuts and slash commands to a
                                        // canonical command, then handle in one place.
                                        let (command, user_text): (Option<&str>, Option<String>) = match (key.code, key.modifiers) {
                                            (KeyCode::Char('c') | KeyCode::Char('d'), m) if m.contains(KeyModifiers::CONTROL) => (Some("/quit"), None),
                                            (KeyCode::Char('h'), m) if m.contains(KeyModifiers::CONTROL) => (Some("/help"), None),
                                            (KeyCode::Char('l'), m) if m.contains(KeyModifiers::CONTROL) => (Some("/load"), None),
                                            (KeyCode::Char('m'), m) if m.contains(KeyModifiers::CONTROL) => (Some("/model"), None),
                                            (KeyCode::Char('n'), m) if m.contains(KeyModifiers::CONTROL) => (Some("/new"), None),
                                            (KeyCode::Char('k'), m) if m.contains(KeyModifiers::CONTROL) => (Some("/compact"), None),
                                            (KeyCode::Char('s'), m) if m.contains(KeyModifiers::CONTROL) => (Some("/stop"), None),
                                            (KeyCode::Enter, _) if !input.is_empty() => {
                                                tab_complete = None;
                                                let trimmed = input.take_text().trim().to_string();
                                                match trimmed.as_str() {
                                                    "/help" | "/h" => (Some("/help"), None),
                                                    "/quit" | "/exit" | "/q" => (Some("/quit"), None),
                                                    "/new" | "/n" => (Some("/new"), None),
                                                    "/load" | "/l" => (Some("/load"), None),
                                                    "/model" | "/m" => (Some("/model"), None),
                                                    "/compact" | "/k" => (Some("/compact"), None),
                                                    "/stop" | "/s" => (Some("/stop"), None),
                                                    _ => (None, Some(trimmed)),
                                                }
                                            }
                                            _ => (None, None),
                                        };

                                        match command {
                                            Some("/help") => {
                                                show_help(&mut viewport)?;
                                            }
                                            Some("/quit") => {
                                                if thread_id.is_some() {
                                                    pending_soft_detach = Some(SoftDetachAction::Quit);
                                                    let _ = soft_detach_tx.send(());
                                                } else {
                                                    cleanup()?;
                                                    return Ok(true);
                                                }
                                            }
                                            Some("/new") => {
                                                if thread_id.is_some() {
                                                    pending_soft_detach = Some(SoftDetachAction::SwitchSession(None));
                                                    let _ = soft_detach_tx.send(());
                                                } else {
                                                    let _ = load_session_tx.send((None, false));
                                                    viewport.print_line_above(Line::from(vec![
                                                        Span::styled(
                                                            "✦ Lazily creating a new session",
                                                            Style::default().fg(Color::Yellow),
                                                        ),
                                                    ]))?;
                                                    total_tokens_used = 0;
                                                    thread_buffers.clear();
                                                    thread_id = None;
                                                    spinner_state = None;
                                                }
                                            }
                                            Some("/load") => {
                                                let mut all_sessions: Vec<(String, infinity_protocol::SessionInfo)> = sessions.iter()
                                                    .map(|(id, info)| (id.clone(), info.clone()))
                                                    .collect();
                                                all_sessions.sort_by(|a, b| b.1.last_updated.cmp(&a.1.last_updated));
                                                session_picker = Some(SessionPicker::new(all_sessions, thread_id.clone()));
                                                ui_mode = UiMode::SessionPicker;
                                            }
                                            Some("/model") => {
                                                model_picker = Some(ModelPicker::new(available_models.clone()));
                                                ui_mode = UiMode::ModelPicker;
                                            }
                                            Some("/compact") => {
                                                let _ = input_tx.send("__compact__".to_string());
                                                viewport.print_line_above(Line::from(vec![
                                                    Span::styled("✦ Compaction triggered", Style::default().fg(Color::Yellow)),
                                                ]))?;
                                            }
                                            Some("/stop") => {
                                                if thread_id.is_some() {
                                                    let _ = load_session_tx.send((None, true));
                                                    viewport.print_line_above(Line::from(vec![
                                                        Span::styled("✦ Agent stopped", Style::default().fg(Color::Yellow)),
                                                    ]))?;
                                                    total_tokens_used = 0;
                                                    thread_buffers.clear();
                                                    thread_id = None;
                                                    spinner_state = None;
                                                } else {
                                                    viewport.print_line_above(Line::from(vec![
                                                        Span::styled("No active session to stop", Style::default().fg(Color::DarkGray)),
                                                    ]))?;
                                                }
                                            }
                                            _ => {
                                                if let Some(trimmed) = user_text {
                                                    let _ = input_tx.send(trimmed);
                                                }
                                            }
                                        }
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

                if got_resize || any_change || spinner_state.is_some() || ui_mode == UiMode::SessionPicker {
                    draw_viewport(&mut viewport, &input, &session_picker, &model_picker, &quit_picker, &choice_picker, &ui_mode, spinner_state, &thinking_start, &model_name, total_tokens_used, context_window, &thread_buffers, &thinking_text_buffer, &tab_complete, &thread_id)?;
                }
            }
        }
    }
}

// ── Scroll-region helpers ───────────────────────────────────────────────────

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
        viewport.print_line_above(Line::from(vec![
            Span::raw(" ".repeat(indent)),
            Span::styled(line.to_string(), style),
        ]))?;
    }
    Ok(())
}

fn end_stream(viewport: &mut InlineViewport, mid_stream: &mut bool) -> Result<(), BoxError> {
    if *mid_stream {
        viewport.print_above(|w| write!(w, "\r\n"))?;
        *mid_stream = false;
    }
    Ok(())
}

fn show_help(viewport: &mut InlineViewport) -> Result<(), BoxError> {
    const W: usize = 55;
    let bar: String = "─".repeat(W);
    let rows = [
        "",
        "  Slash Commands",
        "    /help, /h          Show this help",
        "    /quit, /q          Exit",
        "    /new, /n           New session",
        "    /load, /l          Load session",
        "    /model, /m         Switch model",
        "    /compact, /k       Trigger compaction",
        "    /stop, /s          Stop agent",
        "",
        "  Keyboard Shortcuts",
        "    Ctrl+C / Ctrl+D    Exit",
        "    Ctrl+N             New session",
        "    Ctrl+L             Load session",
        "    Ctrl+M             Switch model",
        "    Ctrl+K             Trigger compaction",
        "    Ctrl+S             Stop agent",
        "    Ctrl+H             Show this help",
        "    Enter              Send message",
        "",
        "  Editing",
        "    Alt+Enter/Ctrl+J   Insert newline",
        "    Up / Down          Move cursor up/down a line",
        "    Ctrl+A / Ctrl+E    Move to line start/end",
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
        viewport.print_line_above(Line::from(vec![Span::styled(
            line.clone(),
            Style::default().fg(Color::Cyan),
        )]))?;
    }
    Ok(())
}

// ── Viewport drawing ────────────────────────────────────────────────────────

fn draw_viewport(
    viewport: &mut InlineViewport,
    input: &TextInput,
    session_picker: &Option<SessionPicker>,
    model_picker: &Option<ModelPicker>,
    quit_picker: &Option<QuitPicker>,
    choice_picker: &Option<ChoicePicker>,
    ui_mode: &UiMode,
    spinner_state: Option<SpinnerState>,
    thinking_start: &Instant,
    model_name: &str,
    total_tokens_used: usize,
    context_window: usize,
    thread_buffers: &BTreeMap<String, String>,
    thinking_text: &str,
    tab_complete: &Option<(String, usize)>,
    thread_id: &Option<String>,
) -> Result<(), BoxError> {
    let max_context = context_window;
    let current_width = viewport.area().width;
    let thread_rows = thread_buffers.len() as u16;

    // Determine status bar text based on mode.
    let pct = if max_context > 0 {
        ((total_tokens_used as f64 / max_context as f64) * 100.0).min(100.0)
    } else {
        0.0
    };
    let status_right = if let Some(tid) = thread_id {
        let short_id = if tid.len() > 8 {
            &tid[..8]
        } else {
            tid.as_str()
        };
        format!("{} | {:.0}% context used", short_id, pct)
    } else {
        format!("{:.0}% context used", pct)
    };
    let status_left = match ui_mode {
        UiMode::SessionPicker | UiMode::ModelPicker | UiMode::QuitPicker | UiMode::ChoicePicker => {
            "↑↓ navigate  enter select  esc cancel".to_string()
        }
        UiMode::Normal => format!("{} (/help for commands)", model_name),
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
        UiMode::ModelPicker => {
            let picker_height = model_picker
                .as_ref()
                .map(|p| p.preferred_height())
                .unwrap_or(1);
            (picker_height, true)
        }
        UiMode::QuitPicker => {
            let picker_height = quit_picker
                .as_ref()
                .map(|p| p.preferred_height())
                .unwrap_or(1);
            (picker_height, true)
        }
        UiMode::ChoicePicker => {
            let picker_height = choice_picker
                .as_ref()
                .map(|p| p.preferred_height())
                .unwrap_or(1);
            (picker_height, true)
        }
        UiMode::Normal => (input.preferred_height(current_width), false),
    };

    // Compute autocomplete hints for slash commands.
    let (autocomplete, ac_selected): (Vec<(&str, &str)>, Option<usize>) =
        if matches!(ui_mode, UiMode::Normal) {
            let prefix = tab_complete
                .as_ref()
                .map(|(p, _)| p.as_str())
                .unwrap_or_else(|| input.text().trim());
            if prefix.starts_with('/') && !prefix.contains(' ') && !prefix.is_empty() {
                let matches: Vec<(&str, &str)> = SLASH_COMMANDS
                    .iter()
                    .filter(|(cmd, _)| cmd.starts_with(prefix))
                    .copied()
                    .collect();
                let sel = tab_complete.as_ref().map(|(_, i)| *i);
                (matches, sel)
            } else {
                (Vec::new(), None)
            }
        } else {
            (Vec::new(), None)
        };
    // Layout autocomplete as a table: multiple columns per row.
    const AC_COL_WIDTH: usize = 30; // fixed width per command cell
    let ac_cols = (current_width as usize / AC_COL_WIDTH).max(1);
    let ac_data_rows = autocomplete.len().div_ceil(ac_cols);
    let autocomplete_rows = if autocomplete.is_empty() {
        0u16
    } else {
        ac_data_rows as u16 + 1
    }; // +1 for blank line

    let mut desired_lines = 1 + thread_rows + 1 + autocomplete_rows + content_height + 1; // gap + threads + border + autocomplete + content + status
    if spinner_state.is_some() {
        desired_lines += 1;
    }

    viewport.draw(desired_lines, |frame| {
        let area = frame.area();

        // Build constraints dynamically
        let mut constraints: Vec<Constraint> = Vec::new();
        constraints.push(Constraint::Length(1));
        if spinner_state.is_some() {
            constraints.push(Constraint::Length(1));
        }
        if thread_rows > 0 {
            constraints.push(Constraint::Length(thread_rows));
        }
        constraints.push(Constraint::Length(1)); // border
        if autocomplete_rows > 0 {
            constraints.push(Constraint::Length(autocomplete_rows));
        }
        constraints.push(Constraint::Min(1)); // content (input or picker)
        constraints.push(Constraint::Length(1)); // status

        let areas = Layout::vertical(constraints).split(area);
        let mut idx = 1; // skip gap

        // Thinking bar
        if let Some(state) = spinner_state {
            render_thinking_bar(frame, areas[idx], thinking_start, thinking_text, state);
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

        // Autocomplete hints (table layout)
        if autocomplete_rows > 0 {
            let ac_area = areas[idx];
            idx += 1;
            for row_i in 0..ac_data_rows {
                if (row_i as u16) >= ac_area.height {
                    break;
                }
                let rect = ratatui::layout::Rect {
                    x: ac_area.x,
                    y: ac_area.y + row_i as u16,
                    width: ac_area.width,
                    height: 1,
                };
                let mut spans = Vec::new();
                for col_i in 0..ac_cols {
                    let entry_idx = row_i * ac_cols + col_i;
                    if entry_idx >= autocomplete.len() {
                        break;
                    }
                    let (cmd, desc) = autocomplete[entry_idx];
                    let selected = ac_selected == Some(entry_idx);
                    let cmd_w = 10;
                    let desc_w = AC_COL_WIDTH - cmd_w;
                    let cmd_style = if selected {
                        Style::default()
                            .fg(Color::Yellow)
                            .bg(Color::DarkGray)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(Color::Cyan)
                    };
                    let desc_style = if selected {
                        Style::default().fg(Color::White).bg(Color::DarkGray)
                    } else {
                        Style::default().fg(Color::DarkGray)
                    };
                    spans.push(Span::styled(format!("{:<cmd_w$}", cmd), cmd_style));
                    spans.push(Span::styled(format!("{:<desc_w$}", desc), desc_style));
                }
                frame.render_widget(Line::from(spans), rect);
            }
            // blank line is the last row of ac_area, left empty
        }

        // Content area — either session picker, model picker, quit picker, choice picker, or input
        if is_picker {
            if let Some(picker) = session_picker {
                frame.render_widget(SessionPickerWidget::new(picker), areas[idx]);
            } else if let Some(picker) = model_picker {
                frame.render_widget(ModelPickerWidget::new(picker), areas[idx]);
            } else if let Some(picker) = quit_picker {
                frame.render_widget(QuitPickerWidget::new(picker), areas[idx]);
            } else if let Some(picker) = choice_picker {
                frame.render_widget(ChoicePickerWidget::new(picker), areas[idx]);
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

/// Render a 10-column animated spinner bar with state-dependent visuals.
///
/// - **LoadingContext**: orange/red bars bouncing up and down (warming up).
/// - **Thinking**: full-height bars with a sliding hydro-color gradient (swimming).
/// - **WaitingToolCall**: slow breathing dark-to-light blue bars.
///
/// Thinking text from the root thread is shown to the right of the spinner.
pub(crate) fn render_thinking_bar(
    frame: &mut crate::inline_viewport::ViewportFrame,
    area: ratatui::layout::Rect,
    thinking_start: &Instant,
    thinking_text: &str,
    state: SpinnerState,
) {
    use ratatui::buffer::Buffer;
    use ratatui::widgets::Widget;

    const NUM_COLS: usize = 10;
    const BLOCKS: [char; 5] = ['▁', '▂', '▄', '▆', '█'];

    // Hydro gradient: smooth loop from #0096FF to #0FDBA2 and back (16 stops)
    const HYDRO: [(u8, u8, u8); 16] = [
        (0, 150, 255),
        (1, 158, 243),
        (3, 167, 231),
        (5, 175, 220),
        (7, 184, 208),
        (9, 193, 196),
        (11, 201, 185),
        (13, 210, 173),
        (15, 219, 162),
        (13, 210, 173),
        (11, 201, 185),
        (9, 193, 196),
        (7, 184, 208),
        (5, 175, 220),
        (3, 167, 231),
        (1, 158, 243),
    ];

    if area.width == 0 {
        return;
    }

    let elapsed_s = thinking_start.elapsed().as_secs_f64();
    let spinner_width = (NUM_COLS as u16).min(area.width);
    let areas = Layout::horizontal([
        Constraint::Length(spinner_width),
        Constraint::Length(1),
        Constraint::Min(0),
    ])
    .split(area);
    let spinner_area = areas[0];
    let text_area = areas[2];

    struct SpinnerWidget {
        elapsed_s: f64,
        state: SpinnerState,
    }

    impl Widget for SpinnerWidget {
        fn render(self, area: ratatui::layout::Rect, buf: &mut Buffer) {
            let cols = (area.width as usize).min(NUM_COLS);
            let y = area.y;

            for col in 0..cols {
                let (ch, r, g, b) = match self.state {
                    SpinnerState::LoadingContext => {
                        // Orange/red bars bouncing up and down — fast, like warming up
                        const WARM: [(u8, u8, u8); 8] = [
                            (180, 60, 20),
                            (210, 80, 25),
                            (240, 120, 40),
                            (255, 160, 60),
                            (255, 180, 80),
                            (255, 160, 60),
                            (240, 120, 40),
                            (210, 80, 25),
                        ];
                        let phase = self.elapsed_s * std::f64::consts::TAU / 0.8;
                        let wave = (phase.sin() * 0.5 + 0.5) as f32;
                        let idx = (wave * (BLOCKS.len() - 1) as f32).round() as usize;
                        let ch = BLOCKS[idx.min(BLOCKS.len() - 1)];
                        let (br, bg, bb) = WARM
                            [std::cmp::min((phase * WARM.len() as f64) as usize, WARM.len() - 1)];
                        let dim = 0.3_f32;
                        let t = dim + (1.0 - dim) * wave;
                        (
                            ch,
                            (br as f32 * t) as u8,
                            (bg as f32 * t) as u8,
                            (bb as f32 * t) as u8,
                        )
                    }
                    SpinnerState::Thinking => {
                        // Full-height bars — small sliding window into gradient
                        // 0.4 step per column
                        let pos = col as f64 * 0.5 + self.elapsed_s * 12.0;
                        let len = HYDRO.len() as f64;
                        let pos = ((pos % len) + len) % len;
                        let idx_a = pos.floor() as usize % HYDRO.len();
                        let idx_b = (idx_a + 1) % HYDRO.len();
                        let frac = (pos - pos.floor()) as f32;
                        let (ar, ag, ab) = HYDRO[idx_a];
                        let (br2, bg2, bb2) = HYDRO[idx_b];
                        let hr = (ar as f32 + (br2 as f32 - ar as f32) * frac) as u8;
                        let hg = (ag as f32 + (bg2 as f32 - ag as f32) * frac) as u8;
                        let hb = (ab as f32 + (bb2 as f32 - ab as f32) * frac) as u8;
                        ('█', hr, hg, hb)
                    }
                    SpinnerState::WaitingToolCall => {
                        // Slow breath cycle (~3s)
                        let phase = self.elapsed_s * std::f64::consts::TAU / 3.0;
                        let wave = (phase.sin() * 0.5 + 0.5) as f32;
                        let (dr, dg, db) = (25, 0, 150);
                        let (lr, lg, lb) = (125, 100, 225);
                        let r = (dr as f32 + (lr as f32 - dr as f32) * wave) as u8;
                        let g = (dg as f32 + (lg as f32 - dg as f32) * wave) as u8;
                        let b = (db as f32 + (lb as f32 - db as f32) * wave) as u8;
                        ('█', r, g, b)
                    }
                };

                let x = area.x + col as u16;
                buf[(x, y)].set_char(ch).set_fg(Color::Rgb(r, g, b));
            }
        }
    }

    frame.render_widget(SpinnerWidget { elapsed_s, state }, spinner_area);

    // Render thinking text in the text area
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

pub(crate) fn cleanup() -> Result<(), BoxError> {
    let mut stdout = io::stdout();
    // Reset terminal title
    write!(stdout, "\x1b]0;\x07")?;
    queue!(stdout, event::DisableBracketedPaste)?;
    queue!(stdout, ResetScrollRegion)?;
    cterm::disable_raw_mode()?;
    let rows = cterm::size()?.1;
    queue!(stdout, cursor::MoveTo(0, rows))?;
    queue!(stdout, cursor::Show)?;
    writeln!(stdout)?;
    stdout.flush()?;
    Ok(())
}

fn set_terminal_title(title: &str) {
    let mut stdout = io::stdout();
    let _ = write!(stdout, "\x1b]0;{}\x07", title);
    let _ = stdout.flush();
}

pub(crate) async fn poll_crossterm_event() {
    tokio::task::spawn_blocking(|| {
        let _ = event::poll(std::time::Duration::from_millis(16));
    })
    .await
    .ok();
}
