use crate::component::{Component, KeyResult};
use ratatui::{
    buffer::Buffer,
    crossterm::event::{KeyCode, KeyEvent},
    layout::Rect,
    style::{Color, Modifier, Style},
    widgets::Widget,
};

pub enum ChoicePickerResult {
    Selected(usize),
}

pub struct ChoicePicker {
    pub prompt: String,
    pub choices: Vec<String>,
    pub default: usize,
    pub selected: usize,
    result: Option<ChoicePickerResult>,
}

impl ChoicePicker {
    pub fn new(prompt: String, choices: Vec<String>, default: usize) -> Self {
        Self {
            selected: default,
            prompt,
            choices,
            default,
            result: None,
        }
    }

    pub fn take_result(&mut self) -> Option<ChoicePickerResult> {
        self.result.take()
    }

    pub fn preferred_height(&self) -> u16 {
        1 + self.choices.len() as u16
    }

    pub fn render(&self, area: Rect, buf: &mut Buffer) {
        if area.height == 0 || area.width == 0 {
            return;
        }

        // Render prompt on first line
        let prompt = format!(" {}", self.prompt);
        for (col, ch) in prompt.chars().enumerate() {
            let x = area.x + col as u16;
            if x >= area.right() {
                break;
            }
            buf[(x, area.y)]
                .set_char(ch)
                .set_fg(Color::Yellow)
                .set_style(Style::default().add_modifier(Modifier::BOLD));
        }

        // Render choices
        for (i, choice) in self.choices.iter().enumerate() {
            let y = area.y + 1 + i as u16;
            if y >= area.bottom() {
                break;
            }

            let is_selected = i == self.selected;
            let is_default = i == self.default;

            let default_tag = if is_default { " (default)" } else { "" };
            let marker = if is_selected { "▸" } else { " " };
            let line = format!(" {} {}{}", marker, choice, default_tag);

            let (fg, bg, modifier) = if is_selected {
                (Color::Black, Color::White, Modifier::BOLD)
            } else {
                (Color::DarkGray, Color::Reset, Modifier::empty())
            };

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

impl Component for ChoicePicker {
    fn handle_keystroke(&mut self, key: KeyEvent) -> KeyResult {
        match key.code {
            KeyCode::Up if self.selected > 0 => {
                self.selected -= 1;
            }
            KeyCode::Down if self.selected < self.choices.len().saturating_sub(1) => {
                self.selected += 1;
            }
            KeyCode::Enter => {
                self.result = Some(ChoicePickerResult::Selected(self.selected));
            }
            KeyCode::Esc => {
                self.result = Some(ChoicePickerResult::Selected(self.default));
            }
            _ => {}
        }
        KeyResult::Captured
    }
}

pub struct ChoicePickerWidget<'a> {
    picker: &'a ChoicePicker,
}

impl<'a> ChoicePickerWidget<'a> {
    pub fn new(picker: &'a ChoicePicker) -> Self {
        Self { picker }
    }
}

impl<'a> Widget for ChoicePickerWidget<'a> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        self.picker.render(area, buf);
    }
}
