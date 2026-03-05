use ratatui::crossterm::event::KeyEvent;

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
