use crate::component::{Component, KeyResult};
use ratatui::{
    buffer::Buffer,
    crossterm::event::{KeyCode, KeyEvent},
    layout::Rect,
    style::{Color, Modifier, Style},
    widgets::Widget,
};

const OPTIONS: [&str; 2] = ["Shut down agent", "Continue running agent in background"];

pub enum QuitPickerResult {
    ShutDown,
    KeepRunning,
    Cancelled,
}

pub struct QuitPicker {
    selected: usize,
    result: Option<QuitPickerResult>,
}

impl QuitPicker {
    pub fn new() -> Self {
        Self {
            selected: 0,
            result: None,
        }
    }

    pub fn take_result(&mut self) -> Option<QuitPickerResult> {
        self.result.take()
    }

    pub fn preferred_height(&self) -> u16 {
        3 // 2 rows + 1 border
    }

    pub fn render(&self, area: Rect, buf: &mut Buffer) {
        if area.height == 0 || area.width == 0 {
            return;
        }
        for (i, label) in OPTIONS.iter().enumerate() {
            let y = area.y + i as u16;
            if y >= area.bottom() {
                break;
            }
            let is_selected = i == self.selected;
            let (fg, bg, modifier) = if is_selected {
                (Color::Black, Color::White, Modifier::BOLD)
            } else {
                (Color::DarkGray, Color::Reset, Modifier::empty())
            };
            let line = format!(" {}", label);
            for x in area.x..area.right() {
                buf[(x, y)].set_char(' ').set_bg(bg);
            }
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

impl Component for QuitPicker {
    fn handle_keystroke(&mut self, key: KeyEvent) -> KeyResult {
        match key.code {
            KeyCode::Up => {
                if self.selected > 0 {
                    self.selected -= 1;
                }
            }
            KeyCode::Down => {
                if self.selected < 1 {
                    self.selected += 1;
                }
            }
            KeyCode::Enter => {
                self.result = Some(if self.selected == 0 {
                    QuitPickerResult::ShutDown
                } else {
                    QuitPickerResult::KeepRunning
                });
            }
            KeyCode::Esc => {
                self.result = Some(QuitPickerResult::Cancelled);
            }
            _ => {}
        }
        KeyResult::Captured
    }
}

pub struct QuitPickerWidget<'a> {
    picker: &'a QuitPicker,
}

impl<'a> QuitPickerWidget<'a> {
    pub fn new(picker: &'a QuitPicker) -> Self {
        Self { picker }
    }
}

impl<'a> Widget for QuitPickerWidget<'a> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        self.picker.render(area, buf);
    }
}
