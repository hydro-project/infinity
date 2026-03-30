use crate::component::{Component, KeyResult};
use infinity_protocol::{SessionInfo, SessionStatus};
use ratatui::{
    buffer::Buffer,
    crossterm::event::{KeyCode, KeyEvent},
    layout::Rect,
    style::{Color, Modifier, Style},
    widgets::Widget,
};

pub const MAX_VISIBLE_ROWS: u16 = 5;

pub enum SessionPickerResult {
    /// User selected a session (id).
    Selected(String),
    Cancelled,
}

pub struct SessionPicker {
    pub sessions: Vec<(String, SessionInfo)>,
    pub selected: usize,
    scroll_offset: usize,
    result: Option<SessionPickerResult>,
    pub current_session_id: Option<String>,
}

impl SessionPicker {
    pub fn new(sessions: Vec<(String, SessionInfo)>, current_session_id: Option<String>) -> Self {
        let selected = current_session_id.as_ref()
            .and_then(|cid| sessions.iter().position(|s| &s.0 == cid))
            .unwrap_or(0);
        let scroll_offset = selected.saturating_sub(MAX_VISIBLE_ROWS as usize - 1);
        Self {
            sessions,
            selected,
            scroll_offset,
            result: None,
            current_session_id,
        }
    }

    pub fn take_result(&mut self) -> Option<SessionPickerResult> {
        self.result.take()
    }

    pub fn visible_rows(&self) -> u16 {
        (self.sessions.len() as u16).min(MAX_VISIBLE_ROWS)
    }

    pub fn preferred_height(&self) -> u16 {
        self.visible_rows() + 1
    }

    fn move_up(&mut self) {
        if self.selected > 0 {
            self.selected -= 1;
            if self.selected < self.scroll_offset {
                self.scroll_offset = self.selected;
            }
        }
    }

    fn move_down(&mut self) {
        if self.selected + 1 < self.sessions.len() {
            self.selected += 1;
            let visible = MAX_VISIBLE_ROWS as usize;
            if self.selected >= self.scroll_offset + visible {
                self.scroll_offset = self.selected + 1 - visible;
            }
        }
    }

    fn confirm(&mut self) {
        if let Some((id, _)) = self.sessions.get(self.selected) {
            self.result = Some(SessionPickerResult::Selected(id.clone()));
        }
    }

    fn cancel(&mut self) {
        self.result = Some(SessionPickerResult::Cancelled);
    }

    pub fn render(&self, area: Rect, buf: &mut Buffer) {
        if area.height == 0 || area.width == 0 {
            return;
        }
        let visible = self.visible_rows() as usize;
        let end = (self.scroll_offset + visible).min(self.sessions.len());

        // Compute dynamic name column width from available area
        // Fixed parts: 1 (leading space) + 1 (gap) + (status) + 1 (gap) + 10 (tokens) + 2 (gap) = 15 + (status), plus time
        let fixed_parts = 15 + "waiting for choice".len();
        let max_time_len = self.sessions[self.scroll_offset..end]
            .iter()
            .map(|(_, info)| info.last_updated.len())
            .max()
            .unwrap_or(0);
        let name_width = (area.width as usize)
            .saturating_sub(fixed_parts + max_time_len)
            .max(8);

        for (i, (id, info)) in self.sessions[self.scroll_offset..end].iter().enumerate() {
            let y = area.y + i as u16;
            if y >= area.bottom() {
                break;
            }

            let is_selected = self.scroll_offset + i == self.selected;
            let (fg, bg, modifier) = if is_selected {
                (Color::Black, Color::White, Modifier::BOLD)
            } else {
                (Color::DarkGray, Color::Reset, Modifier::empty())
            };

            let is_current = self.current_session_id.as_deref() == Some(id.as_str());
            let current_suffix = if is_current { " (current)" } else { "" };
            let suffix_len = current_suffix.len();

            let name = if let Some(ref title) = info.title {
                if title.len() + suffix_len > name_width {
                    let trunc = name_width.saturating_sub(suffix_len + 1);
                    format!("{}…{}", &title[..trunc], current_suffix)
                } else {
                    format!("{}{}", title, current_suffix)
                }
            } else if id.len() + suffix_len > name_width {
                let trunc = name_width.saturating_sub(suffix_len + 1);
                format!("{}…{}", &id[..trunc], current_suffix)
            } else {
                format!("{}{}", id, current_suffix)
            };
            // Column range for the "(current)" suffix (1 for leading space)
            let name_char_len = name.chars().count();
            let current_label_start = 1 + name_char_len - suffix_len;
            let current_label_end = 1 + name_char_len;

            let status_str = match info.status {
                SessionStatus::Running => "running",
                SessionStatus::Idle => "idle",
                SessionStatus::Stopped => "stopped",
                SessionStatus::WaitingForChoice => "waiting for choice",
            };
            let status_fg = match info.status {
                SessionStatus::Running => Color::Green,
                SessionStatus::Idle => Color::Yellow,
                SessionStatus::Stopped => Color::DarkGray,
                SessionStatus::WaitingForChoice => Color::Magenta,
            };
            let time_str = &info.last_updated;
            let tokens_str = format!("{}tok", info.total_tokens_used);
            let status_start = 1 + name_width + 1;
            let status_width = "waiting for choice".len();
            let status_end = status_start + status_width;
            let line = format!(
                " {:<name_width$} {:>status_width$} {:>10}  {}",
                name, status_str, tokens_str, time_str,
            );

            for x in area.x..area.right() {
                buf[(x, y)].set_char(' ').set_bg(bg);
            }
            for (col, ch) in line.chars().enumerate() {
                let x = area.x + col as u16;
                if x >= area.right() {
                    break;
                }
                let char_fg = if col >= status_start && col < status_end {
                    status_fg
                } else if is_current && !is_selected && col >= current_label_start && col < current_label_end {
                    Color::Cyan
                } else {
                    fg
                };
                buf[(x, y)]
                    .set_char(ch)
                    .set_fg(char_fg)
                    .set_bg(bg)
                    .set_style(Style::default().add_modifier(modifier));
            }
        }
    }
}

impl Component for SessionPicker {
    fn handle_keystroke(&mut self, key: KeyEvent) -> KeyResult {
        match key.code {
            KeyCode::Up => {
                self.move_up();
                KeyResult::Captured
            }
            KeyCode::Down => {
                self.move_down();
                KeyResult::Captured
            }
            KeyCode::Enter => {
                self.confirm();
                KeyResult::Captured
            }
            KeyCode::Esc => {
                self.cancel();
                KeyResult::Captured
            }
            _ => KeyResult::Captured,
        }
    }
}

pub struct SessionPickerWidget<'a> {
    picker: &'a SessionPicker,
}

impl<'a> SessionPickerWidget<'a> {
    pub fn new(picker: &'a SessionPicker) -> Self {
        Self { picker }
    }
}

impl<'a> Widget for SessionPickerWidget<'a> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        self.picker.render(area, buf);
    }
}
