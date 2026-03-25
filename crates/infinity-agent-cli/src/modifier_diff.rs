use std::io;

use ratatui::{
    crossterm::{queue, style::Attribute as CrosstermAttribute, style::SetAttribute},
    style::Modifier,
};

/// The `ModifierDiff` struct is used to calculate the difference between two `Modifier`
/// values. This is useful when updating the terminal display, as it allows for more
/// efficient updates by only sending the necessary changes.
///
/// Clone of private API from ratatui
pub struct ModifierDiff {
    pub from: Modifier,
    pub to: Modifier,
}

impl ModifierDiff {
    pub fn queue<W>(self, w: &mut W) -> io::Result<()>
    where
        W: io::Write,
    {
        let removed = self.from - self.to;
        if removed.contains(Modifier::REVERSED) {
            queue!(w, SetAttribute(CrosstermAttribute::NoReverse))?;
        }

        let reset_intensity = removed.contains(Modifier::BOLD) || removed.contains(Modifier::DIM);
        if reset_intensity {
            // Bold and Dim are both reset by applying the Normal intensity
            queue!(w, SetAttribute(CrosstermAttribute::NormalIntensity))?;

            // The remaining Bold and Dim attributes must be
            // reapplied after the intensity reset above.
            if self.to.contains(Modifier::DIM) {
                queue!(w, SetAttribute(CrosstermAttribute::Dim))?;
            }

            if self.to.contains(Modifier::BOLD) {
                queue!(w, SetAttribute(CrosstermAttribute::Bold))?;
            }
        }

        if removed.contains(Modifier::ITALIC) {
            queue!(w, SetAttribute(CrosstermAttribute::NoItalic))?;
        }
        if removed.contains(Modifier::UNDERLINED) {
            queue!(w, SetAttribute(CrosstermAttribute::NoUnderline))?;
        }
        if removed.contains(Modifier::CROSSED_OUT) {
            queue!(w, SetAttribute(CrosstermAttribute::NotCrossedOut))?;
        }
        if removed.contains(Modifier::HIDDEN) {
            queue!(w, SetAttribute(CrosstermAttribute::NoHidden))?;
        }
        if removed.contains(Modifier::SLOW_BLINK) || removed.contains(Modifier::RAPID_BLINK) {
            queue!(w, SetAttribute(CrosstermAttribute::NoBlink))?;
        }

        let added = self.to - self.from;
        if added.contains(Modifier::REVERSED) {
            queue!(w, SetAttribute(CrosstermAttribute::Reverse))?;
        }
        if added.contains(Modifier::BOLD) && !reset_intensity {
            queue!(w, SetAttribute(CrosstermAttribute::Bold))?;
        }
        if added.contains(Modifier::ITALIC) {
            queue!(w, SetAttribute(CrosstermAttribute::Italic))?;
        }
        if added.contains(Modifier::UNDERLINED) {
            queue!(w, SetAttribute(CrosstermAttribute::Underlined))?;
        }
        if added.contains(Modifier::DIM) && !reset_intensity {
            queue!(w, SetAttribute(CrosstermAttribute::Dim))?;
        }
        if added.contains(Modifier::CROSSED_OUT) {
            queue!(w, SetAttribute(CrosstermAttribute::CrossedOut))?;
        }
        if added.contains(Modifier::HIDDEN) {
            queue!(w, SetAttribute(CrosstermAttribute::Hidden))?;
        }
        if added.contains(Modifier::SLOW_BLINK) {
            queue!(w, SetAttribute(CrosstermAttribute::SlowBlink))?;
        }
        if added.contains(Modifier::RAPID_BLINK) {
            queue!(w, SetAttribute(CrosstermAttribute::RapidBlink))?;
        }

        Ok(())
    }
}
