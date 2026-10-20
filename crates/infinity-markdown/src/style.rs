//! Styling vocabulary produced by the renderer.

/// A terminal color from the standard 16-color palette. The consumer maps
/// these onto whatever color type its terminal layer uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Color {
    Red,
    Green,
    Yellow,
    Blue,
    Magenta,
    Cyan,
    Gray,
}

/// Text attributes for one segment of output. All attributes map directly
/// onto SGR terminal styling.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Style {
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub strikethrough: bool,
    pub dim: bool,
    pub fg: Option<Color>,
}

impl Style {
    /// Plain, unstyled text.
    pub const PLAIN: Style = Style {
        bold: false,
        italic: false,
        underline: false,
        strikethrough: false,
        dim: false,
        fg: None,
    };

    /// Overlay `other` on top of `self`: boolean attributes are OR-ed, and
    /// `other`'s foreground color wins when set.
    pub fn merge(self, other: Style) -> Style {
        Style {
            bold: self.bold || other.bold,
            italic: self.italic || other.italic,
            underline: self.underline || other.underline,
            strikethrough: self.strikethrough || other.strikethrough,
            dim: self.dim || other.dim,
            fg: other.fg.or(self.fg),
        }
    }
}

/// A run of text sharing one style.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Segment {
    pub text: String,
    pub style: Style,
    /// Link target when this segment is the text of a Markdown link, for
    /// consumers that can emit terminal hyperlinks (OSC 8).
    pub href: Option<String>,
}

/// Styles applied to each Markdown construct.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Theme {
    /// Heading text (all levels); the `#` markers are dropped.
    pub heading: Style,
    /// Inline code spans.
    pub code: Style,
    /// Lines inside fenced code blocks.
    pub code_block: Style,
    /// The rules framing a fenced code block (including the language label).
    pub fence: Style,
    /// Link text.
    pub link: Style,
    /// The ` (url)` shown after link text when it differs from the URL.
    pub link_url: Style,
    /// Text inside a block quote.
    pub quote: Style,
    /// The `│` gutter in front of block-quote lines.
    pub quote_gutter: Style,
    /// List bullets and numbers.
    pub bullet: Style,
    /// Thematic breaks (`---`).
    pub rule: Style,
}

impl Default for Theme {
    fn default() -> Self {
        Theme {
            heading: Style {
                bold: true,
                fg: Some(Color::Magenta),
                ..Style::PLAIN
            },
            code: Style {
                fg: Some(Color::Cyan),
                ..Style::PLAIN
            },
            code_block: Style::PLAIN,
            fence: Style {
                dim: true,
                ..Style::PLAIN
            },
            link: Style {
                underline: true,
                fg: Some(Color::Blue),
                ..Style::PLAIN
            },
            link_url: Style {
                dim: true,
                ..Style::PLAIN
            },
            quote: Style {
                italic: true,
                ..Style::PLAIN
            },
            quote_gutter: Style {
                dim: true,
                ..Style::PLAIN
            },
            bullet: Style::PLAIN,
            rule: Style {
                dim: true,
                ..Style::PLAIN
            },
        }
    }
}
