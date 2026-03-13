use crate::component::{Component, KeyResult};
use crate::session_store::SessionEntry;
use ratatui::{
    buffer::Buffer,
    crossterm::event::{KeyCode, KeyEvent},
    layout::Rect,
    style::{Color, Modifier, Style},
    widgets::Widget,
};

/// Maximum visible rows in the session picker.
pub const MAX_VISIBLE_ROWS: u16 = 5;

/// Result of the session picker interaction.
pub enum SessionPickerResult {
    /// User selected a session.
    Selected(SessionEntry),
    /// User cancelled (Escape).
    Cancelled,
}

/// A scrollable session picker overlay.
pub struct SessionPicker {
    sessions: Vec<SessionEntry>,
    /// Index of the currently highlighted session.
    selected: usize,
    /// Scroll offset for the visible window.
    scroll_offset: usize,
    /// Pending result to be consumed by the caller.
    result: Option<SessionPickerResult>,
}

impl SessionPicker {
    pub fn new(sessions: Vec<SessionEntry>) -> Self {
        Self {
            sessions,
            selected: 0,
            scroll_offset: 0,
            result: None,
        }
    }

    /// Take the pending result, if any.
    pub fn take_result(&mut self) -> Option<SessionPickerResult> {
        self.result.take()
    }

    /// The number of rows this picker wants to display (capped at MAX_VISIBLE_ROWS).
    pub fn visible_rows(&self) -> u16 {
        (self.sessions.len() as u16).min(MAX_VISIBLE_ROWS)
    }

    /// Total height including the top border row.
    pub fn preferred_height(&self) -> u16 {
        self.visible_rows() + 1 // +1 for border
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
        if let Some(entry) = self.sessions.get(self.selected) {
            self.result = Some(SessionPickerResult::Selected(entry.clone()));
        }
    }

    fn cancel(&mut self) {
        self.result = Some(SessionPickerResult::Cancelled);
    }

    /// Render the session list into the given area.
    pub fn render(&self, area: Rect, buf: &mut Buffer) {
        if area.height == 0 || area.width == 0 {
            return;
        }

        let visible = self.visible_rows() as usize;
        let end = (self.scroll_offset + visible).min(self.sessions.len());

        for (i, session) in self.sessions[self.scroll_offset..end].iter().enumerate() {
            let y = area.y + i as u16;
            if y >= area.bottom() {
                break;
            }

            let abs_idx = self.scroll_offset + i;
            let is_selected = abs_idx == self.selected;

            let (fg, bg, modifier) = if is_selected {
                (Color::Black, Color::White, Modifier::BOLD)
            } else {
                (Color::DarkGray, Color::Reset, Modifier::empty())
            };

            // Format: "  title_or_id  |  tokens  |  last updated  "
            let name_display = if let Some(ref title) = session.title {
                if title.len() > 24 {
                    format!("{}…", &title[..23])
                } else {
                    title.clone()
                }
            } else if session.thread_id.len() > 16 {
                format!("{}…", &session.thread_id[..15])
            } else {
                session.thread_id.clone()
            };

            let time_str = session.last_updated_display();
            let tokens_str = format!("{}tok", session.total_tokens_used);
            let line = format!(" {:<26} {:>10}  {}", name_display, tokens_str, time_str);

            // Fill the row background
            for x in area.x..area.right() {
                buf[(x, y)].set_char(' ').set_bg(bg);
            }

            // Write text
            for (col, ch) in line.chars().enumerate() {
                let x = area.x + col as u16;
                if x >= area.right() {
                    break;
                }
                buf[(x, y)]
                    .set_char(ch)
                    .set_fg(fg)
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
            _ => KeyResult::Captured, // swallow all keys while picker is open
        }
    }
}

/// Widget adapter for rendering the SessionPicker.
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
