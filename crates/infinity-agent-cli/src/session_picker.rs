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
}

impl SessionPicker {
    pub fn new(sessions: Vec<(String, SessionInfo)>) -> Self {
        Self {
            sessions,
            selected: 0,
            scroll_offset: 0,
            result: None,
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

            let name = if let Some(ref title) = info.title {
                if title.len() > name_width {
                    format!("{}…", &title[..name_width - 1])
                } else {
                    title.clone()
                }
            } else if id.len() > name_width {
                format!("{}…", &id[..name_width - 1])
            } else {
                id.clone()
            };

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
