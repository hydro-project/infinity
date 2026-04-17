use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// Whether the key event represents a cancel gesture (Esc or Ctrl+C).
pub fn is_cancel(key: &KeyEvent) -> bool {
    key.code == KeyCode::Esc
        || (key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL))
}

/// Whether a keystroke was consumed by a component.
pub enum KeyResult {
    /// The component handled the keystroke.
    Captured,
    /// The component did not handle the keystroke; pass it along.
    NotCaptured,
}

/// Trait for TUI components that can handle keyboard input.
pub trait Component {
    fn handle_keystroke(&mut self, key: KeyEvent) -> KeyResult;
}
