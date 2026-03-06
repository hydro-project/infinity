use crate::component::{Component, KeyResult};
use ratatui::{
    buffer::Buffer,
    crossterm::event::{KeyCode, KeyEvent},
    layout::Rect,
    style::{Color, Modifier, Style},
    widgets::Widget,
};

/// Maximum visible rows in the model picker.
pub const MAX_VISIBLE_ROWS: u16 = 5;

/// Default model index (the 1m context model).
pub const DEFAULT_MODEL_INDEX: usize = 0;

/// A model available for selection.
pub struct ModelEntry {
    /// Human-readable name shown in the picker and status bar.
    pub display_name: &'static str,
    /// Bedrock model identifier used for API calls.
    pub bedrock_model_id: &'static str,
    /// Extra fields merged into `additional_params` on every completion request
    /// (e.g. beta headers). `None` means no extra fields.
    pub additional_request_params: Option<serde_json::Value>,
    /// Context window size in tokens, used for the status-bar percentage.
    pub context_window: usize,
}

/// Return the hardcoded list of available models.
/// The default model is at index [`DEFAULT_MODEL_INDEX`].
pub fn available_models() -> Vec<ModelEntry> {
    vec![
        ModelEntry {
            display_name: "claude-opus-4-6 1m",
            bedrock_model_id: "global.anthropic.claude-opus-4-6-v1",
            additional_request_params: Some(serde_json::json!({
                "anthropic_beta": ["context-1m-2025-08-07"]
            })),
            context_window: 1_000_000,
        },
        ModelEntry {
            display_name: "claude-opus-4-6",
            bedrock_model_id: "global.anthropic.claude-opus-4-6-v1",
            additional_request_params: None,
            context_window: 200_000,
        },
    ]
}

/// Result of the model picker interaction.
pub enum ModelPickerResult {
    /// User selected a model (index into `available_models()`).
    Selected(usize),
    /// User cancelled (Escape).
    Cancelled,
}

/// A scrollable model picker overlay.
pub struct ModelPicker {
    models: Vec<ModelEntry>,
    /// Index of the currently highlighted model.
    selected: usize,
    /// Scroll offset for the visible window.
    scroll_offset: usize,
    /// Pending result to be consumed by the caller.
    result: Option<ModelPickerResult>,
}

impl ModelPicker {
    pub fn new(models: Vec<ModelEntry>) -> Self {
        Self {
            models,
            selected: 0,
            scroll_offset: 0,
            result: None,
        }
    }

    /// Take the pending result, if any.
    pub fn take_result(&mut self) -> Option<ModelPickerResult> {
        self.result.take()
    }

    /// The number of rows this picker wants to display (capped at MAX_VISIBLE_ROWS).
    pub fn visible_rows(&self) -> u16 {
        (self.models.len() as u16).min(MAX_VISIBLE_ROWS)
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
        if self.selected + 1 < self.models.len() {
            self.selected += 1;
            let visible = MAX_VISIBLE_ROWS as usize;
            if self.selected >= self.scroll_offset + visible {
                self.scroll_offset = self.selected + 1 - visible;
            }
        }
    }

    fn confirm(&mut self) {
        if self.selected < self.models.len() {
            self.result = Some(ModelPickerResult::Selected(self.selected));
        }
    }

    fn cancel(&mut self) {
        self.result = Some(ModelPickerResult::Cancelled);
    }

    /// Render the model list into the given area.
    pub fn render(&self, area: Rect, buf: &mut Buffer) {
        if area.height == 0 || area.width == 0 {
            return;
        }

        let visible = self.visible_rows() as usize;
        let end = (self.scroll_offset + visible).min(self.models.len());

        for (i, model) in self.models[self.scroll_offset..end].iter().enumerate() {
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

            let ctx_str = if model.context_window >= 1_000_000 {
                format!("{}m ctx", model.context_window / 1_000_000)
            } else {
                format!("{}k ctx", model.context_window / 1_000)
            };

            let line = format!(" {:<24} {:>10}", model.display_name, ctx_str);

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

impl Component for ModelPicker {
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

/// Widget adapter for rendering the ModelPicker.
pub struct ModelPickerWidget<'a> {
    picker: &'a ModelPicker,
}

impl<'a> ModelPickerWidget<'a> {
    pub fn new(picker: &'a ModelPicker) -> Self {
        Self { picker }
    }
}

impl<'a> Widget for ModelPickerWidget<'a> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        self.picker.render(area, buf);
    }
}
