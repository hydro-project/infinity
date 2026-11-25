//! Bridges the streaming Markdown renderer to the inline viewport.
//!
//! Model text arrives as [`DisplayEvent::TextChunk`](crate::display::DisplayEvent::TextChunk)s
//! and is fed through an [`infinity_markdown::Renderer`]; the styled
//! segments it produces are printed into the scrollback here.

use infinity_markdown::{Color as MdColor, Segment, Style as MdStyle};
use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};

use crate::{inline_viewport::InlineViewport, term_io::TermOut};

type BoxError = Box<dyn std::error::Error + Send + Sync>;

pub fn to_style(s: MdStyle) -> Style {
    let mut style = Style::default();
    if s.bold {
        style = style.add_modifier(Modifier::BOLD);
    }
    if s.italic {
        style = style.add_modifier(Modifier::ITALIC);
    }
    if s.underline {
        style = style.add_modifier(Modifier::UNDERLINED);
    }
    if s.strikethrough {
        style = style.add_modifier(Modifier::CROSSED_OUT);
    }
    if s.dim {
        style = style.add_modifier(Modifier::DIM);
    }
    if let Some(fg) = s.fg {
        style = style.fg(match fg {
            MdColor::Red => Color::Red,
            MdColor::Green => Color::Green,
            MdColor::Yellow => Color::Yellow,
            MdColor::Blue => Color::Blue,
            MdColor::Magenta => Color::Magenta,
            MdColor::Cyan => Color::Cyan,
            MdColor::Gray => Color::DarkGray,
        });
    }
    style
}

/// Print rendered Markdown segments as streaming model output.
///
/// Starts a new line before the first text of a stream and keeps
/// `mid_stream` in sync so that [`end_stream`](crate::terminal) can close the
/// line later. Does nothing when the segments carry no text (the renderer
/// may be holding input back).
pub fn print_segments<T: TermOut>(
    viewport: &mut InlineViewport<T>,
    mid_stream: &mut bool,
    segments: &[Segment],
) -> Result<(), BoxError> {
    if segments.iter().all(|s| s.text.is_empty()) {
        return Ok(());
    }
    let mut spans = Vec::with_capacity(segments.len() + 1);
    if !*mid_stream {
        spans.push(Span::raw("\r\n"));
    }
    *mid_stream = true;
    for seg in segments {
        // Raw LF would not return the cursor to column 0 in raw mode.
        let text = seg.text.replace('\n', "\r\n");
        spans.push(Span::styled(text, to_style(seg.style)));
    }
    viewport.print_spans_above(Line::from(spans))
}
