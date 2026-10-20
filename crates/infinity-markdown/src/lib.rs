//! Streaming Markdown renderer for terminals.
//!
//! Converts Markdown text, fed incrementally as it arrives from a language
//! model, into [`Segment`]s of text with terminal-friendly [`Style`]s. The
//! consumer maps the styles onto SGR escape sequences (or any other styled
//! output).
//!
//! The renderer is built for output that **cannot be redrawn** once printed
//! (terminal scrollback): it decides what each character means with at most
//! one character of lookahead, so text streams through with essentially no
//! delay. The trade-off is that an unclosed `**` styles the rest of its line
//! instead of being shown literally — the rare cost of never buffering a
//! paragraph.
//!
//! Supported: ATX headings, bullet/ordered/task lists, block quotes,
//! thematic breaks, fenced code blocks, emphasis (`*`/`_`), strong
//! emphasis, strikethrough (`~~`), code spans, backslash escapes, inline
//! links and images. Anything else passes through verbatim.

mod render;
mod style;

pub use render::Renderer;
pub use style::{Color, Segment, Style, Theme};

#[cfg(test)]
mod tests;
