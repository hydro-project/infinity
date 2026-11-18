//! The streaming state machine behind [`Renderer`].
//!
//! Input is processed one `char` at a time. Two kinds of state exist:
//!
//! * **Line state** ([`Line`]): at the start of each line the first few
//!   characters are held until the block kind (heading, list item, quote,
//!   fence, rule, paragraph) is known; after that the line body is streamed.
//!   Inside a fenced code block, line starts are held just long enough to
//!   recognize the closing fence.
//! * **Inline state** ([`Inline`]): emphasis, code spans and strikethrough
//!   are rendered *optimistically* — the style is switched on as soon as an
//!   opening delimiter is seen (based on CommonMark's flanking rules, which
//!   only need the previous and next character) and switched off at the
//!   closing delimiter or the end of the line. Links are the one inline
//!   construct that is held, because the URL follows the text.
//! * **Tables** ([`Line::TableCandidate`], [`Line::Table`]) are the
//!   exception to streaming: column widths depend on every cell, so a
//!   table is held until its last row and then drawn with box characters.

use unicode_width::UnicodeWidthStr;

use crate::style::{Segment, Style, Theme};
use crate::table::{
    Align, border, column_widths, is_delimiter_char, pad_line, parse_delimiter_row, parse_row,
    wrap_cell,
};

/// Width assumed before [`Renderer::set_width`] is called.
const DEFAULT_WIDTH: usize = 80;

/// Upper bound on text held back while waiting for a construct to resolve
/// (block prefixes, link text). Past this the text is flushed as-is.
const MAX_HOLD: usize = 512;

/// Upper bound on link URL text held back while waiting for the closing `)`.
const MAX_URL_HOLD: usize = 4096;

/// Streaming Markdown renderer.
///
/// Feed text with [`push`](Renderer::push) as it arrives; every call returns
/// the styled segments that could be emitted so far. Call
/// [`finish`](Renderer::finish) at the end of a message to flush anything
/// still held back and reset for the next message.
///
/// ```
/// use infinity_markdown::Renderer;
///
/// let mut md = Renderer::new();
/// let mut out = md.push("some **bo");
/// out.extend(md.push("ld** text\n"));
/// out.extend(md.finish());
/// let text: String = out.iter().map(|s| s.text.as_str()).collect();
/// assert_eq!(text, "some bold text\n");
/// // "bo" and "ld" arrived in different pushes, each styled bold.
/// let bold: String = out.iter().filter(|s| s.style.bold).map(|s| s.text.as_str()).collect();
/// assert_eq!(bold, "bold");
/// ```
pub struct Renderer {
    width: usize,
    theme: Theme,
    out: Vec<Segment>,
    line: Line,
    fence: Option<Fence>,
    inline: Inline,
}

enum Line {
    /// Start of a line: `buf` holds characters whose block meaning is not
    /// decided yet.
    Prefix(String),
    /// Inside the inline content of a line.
    Body,
    /// Start of a line inside a fenced code block: `buf` may turn out to be
    /// the closing fence.
    FenceStart(String),
    /// Inside a line of a fenced code block.
    FenceBody,
    /// A complete line starting with `|` was seen; `delim` accumulates the
    /// next line, which makes it a table if it is a delimiter row.
    TableCandidate {
        indent: usize,
        header: String,
        delim: String,
    },
    /// Inside a table: `cur` accumulates the current (possible) row.
    Table { table: TableAcc, cur: String },
}

/// Raw lines of a table being accumulated.
struct TableAcc {
    indent: usize,
    header: String,
    delim: String,
    rows: Vec<String>,
}

impl Default for Line {
    fn default() -> Self {
        Line::Prefix(String::new())
    }
}

struct Fence {
    ch: char,
    len: usize,
}

#[derive(Default)]
struct Inline {
    /// Block-level style of the current line (heading, quote).
    base: Style,
    /// Delimiter char of the open bold span, if any.
    bold: Option<char>,
    /// Delimiter char of the open italic span, if any.
    italic: Option<char>,
    strike: bool,
    /// Backtick count of the open code span, if any.
    code: Option<usize>,
    pending: Pending,
    /// Last character consumed on this line (for flanking rules); `None` at
    /// the line start.
    prev: Option<char>,
    link: Option<LinkHold>,
    /// URL of the link whose text is currently being emitted.
    in_link: Option<String>,
    /// Set while re-feeding held text as literal, so `[` doesn't start a
    /// new link hold.
    literal_brackets: bool,
}

/// One-character lookahead buffer for inline delimiters.
#[derive(Default, Clone, Copy)]
enum Pending {
    #[default]
    None,
    /// A run of `n` identical delimiter characters.
    Run(char, usize),
    Backslash,
    /// `!`, which starts an image if followed by `[`.
    Bang,
}

enum LinkHold {
    /// After `[`, collecting link text.
    Text { text: String, image: bool },
    /// After `]`, waiting to see whether `(` follows.
    AfterText { text: String, image: bool },
    /// After `](`, collecting the URL.
    Url {
        text: String,
        url: String,
        image: bool,
    },
}

impl Default for Renderer {
    fn default() -> Self {
        Self::new()
    }
}

impl Renderer {
    /// A renderer using [`Theme::default`].
    pub fn new() -> Self {
        Self::with_theme(Theme::default())
    }

    pub fn with_theme(theme: Theme) -> Self {
        Renderer {
            width: DEFAULT_WIDTH,
            theme,
            out: Vec::new(),
            line: Line::default(),
            fence: None,
            inline: Inline::default(),
        }
    }

    /// Set the terminal width used for full-width elements (rules, fence
    /// frames). Takes effect for elements emitted after the call.
    pub fn set_width(&mut self, width: usize) {
        self.width = width;
    }

    /// Feed a chunk of Markdown text and return the segments that can be
    /// emitted so far. Chunk boundaries may fall anywhere, including in the
    /// middle of a multi-character delimiter.
    pub fn push(&mut self, text: &str) -> Vec<Segment> {
        for ch in text.chars() {
            self.feed(ch);
        }
        std::mem::take(&mut self.out)
    }

    /// Flush everything still held back and reset to the start of a new
    /// message. Does not emit a trailing newline.
    pub fn finish(&mut self) -> Vec<Segment> {
        match std::mem::take(&mut self.line) {
            Line::Prefix(buf) => {
                if !buf.is_empty() {
                    // Resolve the held line start as if the line ended here,
                    // then drop the newline that resolution produced.
                    self.line = Line::Prefix(buf);
                    self.feed('\n');
                    if let Line::TableCandidate { header, .. } = std::mem::take(&mut self.line) {
                        // A lone `| ... |` line is just text.
                        self.paragraph(&header);
                    }
                    self.pop_trailing_newline();
                }
            }
            Line::Body | Line::FenceBody => {}
            Line::FenceStart(buf) => {
                self.emit(&buf, self.theme.code_block);
            }
            Line::TableCandidate { header, delim, .. } => {
                self.paragraph(&header);
                self.paragraph(&delim);
            }
            Line::Table { mut table, cur } => {
                if cur.trim_start().starts_with('|') {
                    // A last row without a trailing newline: the table
                    // shouldn't end with one either.
                    table.rows.push(cur);
                    self.render_table(table);
                    self.pop_trailing_newline();
                } else {
                    self.render_table(table);
                    self.paragraph(&cur);
                }
            }
        }
        self.finish_inline();
        self.fence = None;
        self.reset_line();
        std::mem::take(&mut self.out)
    }

    fn pop_trailing_newline(&mut self) {
        let Some(last) = self.out.last_mut() else {
            return;
        };
        if last.text.ends_with('\n') {
            last.text.pop();
            if last.text.is_empty() {
                self.out.pop();
            }
        }
    }

    // ----------------------------------------------------------------------
    // Output helpers

    fn emit(&mut self, text: &str, style: Style) {
        self.emit_href(text, style, None);
    }

    fn emit_href(&mut self, text: &str, style: Style, href: Option<&str>) {
        if text.is_empty() {
            return;
        }
        if let Some(last) = self.out.last_mut()
            && last.style == style
            && last.href.as_deref() == href
        {
            last.text.push_str(text);
            return;
        }
        self.out.push(Segment {
            text: text.to_owned(),
            style,
            href: href.map(str::to_owned),
        });
    }

    fn emit_newline(&mut self) {
        self.emit("\n", Style::PLAIN);
    }

    fn emit_indent(&mut self, indent: usize) {
        if indent > 0 {
            self.emit(&" ".repeat(indent), Style::PLAIN);
        }
    }

    /// Width available for full-width elements. One column short of the
    /// terminal width so the line never triggers the terminal's auto-wrap.
    fn line_width(&self) -> usize {
        self.width.saturating_sub(1).max(1)
    }

    fn current_style(&self) -> Style {
        let mut s = self.inline.base;
        s.bold |= self.inline.bold.is_some();
        s.italic |= self.inline.italic.is_some();
        s.strikethrough |= self.inline.strike;
        if self.inline.code.is_some() {
            s = s.merge(self.theme.code);
        }
        if self.inline.in_link.is_some() {
            s = s.merge(self.theme.link);
        }
        s
    }

    fn emit_char(&mut self, ch: char) {
        let style = self.current_style();
        let href = self.inline.in_link.clone();
        let mut buf = [0u8; 4];
        self.emit_href(ch.encode_utf8(&mut buf), style, href.as_deref());
        self.inline.prev = Some(ch);
    }

    fn emit_literal_run(&mut self, ch: char, n: usize) {
        if n == 0 {
            return;
        }
        let style = self.current_style();
        let href = self.inline.in_link.clone();
        let text: String = std::iter::repeat_n(ch, n).collect();
        self.emit_href(&text, style, href.as_deref());
        self.inline.prev = Some(ch);
    }

    // ----------------------------------------------------------------------
    // Character dispatch

    fn feed(&mut self, ch: char) {
        match &mut self.line {
            Line::Prefix(buf) => {
                buf.push(ch);
                self.resolve_prefix();
            }
            Line::Body => {
                if ch == '\n' {
                    self.end_line();
                } else {
                    self.inline_char(ch);
                }
            }
            Line::FenceStart(buf) => {
                buf.push(ch);
                self.resolve_fence_start();
            }
            Line::FenceBody => {
                if ch == '\n' {
                    self.emit_newline();
                    self.line = Line::FenceStart(String::new());
                } else {
                    let mut buf = [0u8; 4];
                    self.emit(ch.encode_utf8(&mut buf), self.theme.code_block);
                }
            }
            Line::TableCandidate { .. } => self.table_candidate_char(ch),
            Line::Table { .. } => self.table_char(ch),
        }
    }

    fn feed_str(&mut self, text: &str) {
        for ch in text.chars() {
            self.feed(ch);
        }
    }

    /// Reset per-line state at the start of a new line.
    fn reset_line(&mut self) {
        self.inline = Inline::default();
        self.line = Line::Prefix(String::new());
    }

    fn end_line(&mut self) {
        self.finish_inline();
        self.emit_newline();
        self.reset_line();
    }

    // ----------------------------------------------------------------------
    // Block prefixes

    /// Decide what the held line start is, or keep holding.
    fn resolve_prefix(&mut self) {
        let Line::Prefix(buf) = std::mem::take(&mut self.line) else {
            unreachable!("bug: resolve_prefix outside Prefix state");
        };

        let indent = buf.chars().take_while(|&c| c == ' ').count();
        let rest = &buf[indent..];
        let Some(first) = rest.chars().next() else {
            self.line = Line::Prefix(buf);
            return;
        };

        // Fallback for pathological input: stop holding and stream it.
        if buf.chars().count() > MAX_HOLD {
            self.paragraph(&buf);
            return;
        }

        match first {
            '\n' => {
                // Blank line (trailing spaces dropped).
                self.emit_newline();
                self.reset_line();
            }
            '#' => self.resolve_heading(&buf, indent),
            '>' => {
                if rest.len() == 1 {
                    // Need to see whether a space follows the marker.
                    self.line = Line::Prefix(buf);
                    return;
                }
                self.emit_indent(indent);
                self.emit("│ ", self.theme.quote_gutter);
                self.inline.base = self.inline.base.merge(self.theme.quote);
                let tail = rest[1..].strip_prefix(' ').unwrap_or(&rest[1..]);
                self.line = Line::Prefix(String::new());
                self.feed_str(tail);
            }
            '-' | '*' | '+' | '_' => self.resolve_marker_line(&buf, indent),
            '0'..='9' => self.resolve_ordered(&buf, indent),
            '`' | '~' => self.resolve_fence_open(&buf, indent),
            '|' => {
                if rest.ends_with('\n') {
                    self.line = Line::TableCandidate {
                        indent,
                        header: buf,
                        delim: String::new(),
                    };
                } else {
                    // Hold the whole line: it's a table header if a
                    // delimiter row follows.
                    self.line = Line::Prefix(buf);
                }
            }
            _ => self.paragraph(&buf),
        }
    }

    /// Treat a held line start as ordinary paragraph text.
    fn paragraph(&mut self, buf: &str) {
        let indent = buf.chars().take_while(|&c| c == ' ').count();
        self.emit_indent(indent);
        self.line = Line::Body;
        self.feed_str(&buf[indent..]);
    }

    fn resolve_heading(&mut self, buf: &str, indent: usize) {
        let rest = &buf[indent..];
        let hashes = rest.chars().take_while(|&c| c == '#').count();
        match rest.chars().nth(hashes) {
            None => self.line = Line::Prefix(buf.to_owned()),
            Some(' ' | '\n') if hashes <= 6 => {
                self.emit_indent(indent);
                self.inline.base = self.inline.base.merge(self.theme.heading);
                self.line = Line::Body;
                let tail = &rest[hashes..];
                let tail = tail.strip_prefix(' ').unwrap_or(tail);
                self.feed_str(tail);
            }
            Some(_) => self.paragraph(buf),
        }
    }

    /// A line starting with `-`, `*`, `+` or `_`: thematic break, bullet
    /// list item, task list item, or just text.
    fn resolve_marker_line(&mut self, buf: &str, indent: usize) {
        let rest = &buf[indent..];
        let first = rest
            .chars()
            .next()
            .expect("bug: resolve_marker_line on empty rest");

        if first != '+' {
            let only_marks = rest.chars().all(|c| c == first || c == ' ' || c == '\n');
            if only_marks {
                if !rest.ends_with('\n') {
                    // Could still become `---`.
                    self.line = Line::Prefix(buf.to_owned());
                    return;
                }
                if rest.chars().filter(|&c| c == first).count() >= 3 {
                    self.thematic_break(indent);
                    return;
                }
            }
        }

        if first != '_' {
            match rest.chars().nth(1) {
                None => {
                    self.line = Line::Prefix(buf.to_owned());
                    return;
                }
                Some(' ') => {
                    let after = &rest[2..];
                    const TASKS: [(&str, &str); 3] =
                        [("[ ] ", "☐ "), ("[x] ", "☑ "), ("[X] ", "☑ ")];
                    if let Some((_, marker)) = TASKS.iter().find(|(m, _)| after.starts_with(m)) {
                        self.list_item(indent, marker, &after[4..]);
                        return;
                    }
                    if TASKS.iter().any(|(m, _)| m.starts_with(after)) {
                        self.line = Line::Prefix(buf.to_owned());
                        return;
                    }
                    self.list_item(indent, "• ", after);
                    return;
                }
                Some('\n') => {
                    self.list_item(indent, "• ", &rest[1..]);
                    return;
                }
                Some(_) => {}
            }
        }

        self.paragraph(buf);
    }

    fn resolve_ordered(&mut self, buf: &str, indent: usize) {
        let rest = &buf[indent..];
        let digits = rest.chars().take_while(char::is_ascii_digit).count();
        if digits > 9 {
            self.paragraph(buf);
            return;
        }
        let mut it = rest.chars().skip(digits);
        match it.next() {
            None => self.line = Line::Prefix(buf.to_owned()),
            Some(delim @ ('.' | ')')) => match it.next() {
                None => self.line = Line::Prefix(buf.to_owned()),
                Some(' ' | '\n') => {
                    let marker = format!("{}{} ", &rest[..digits], delim);
                    let tail = &rest[digits + 1..];
                    let tail = tail.strip_prefix(' ').unwrap_or(tail);
                    self.list_item(indent, &marker, tail);
                }
                Some(_) => self.paragraph(buf),
            },
            Some(_) => self.paragraph(buf),
        }
    }

    /// Emit a list marker; the rest of the line is parsed as a fresh block
    /// start so nested items, headings, etc. inside list items work.
    fn list_item(&mut self, indent: usize, marker: &str, tail: &str) {
        self.emit_indent(indent);
        self.emit(marker, self.theme.bullet);
        self.line = Line::Prefix(String::new());
        self.feed_str(tail);
    }

    fn thematic_break(&mut self, indent: usize) {
        self.emit_indent(indent);
        let w = self.line_width().saturating_sub(indent).max(1);
        self.emit(&"─".repeat(w), self.theme.rule);
        self.emit_newline();
        self.reset_line();
    }

    fn resolve_fence_open(&mut self, buf: &str, indent: usize) {
        let rest = &buf[indent..];
        let first = rest
            .chars()
            .next()
            .expect("bug: resolve_fence_open on empty rest");
        let run = rest.chars().take_while(|&c| c == first).count();
        if run == rest.chars().count() {
            // All fence characters so far: might still reach three.
            self.line = Line::Prefix(buf.to_owned());
            return;
        }
        if run < 3 {
            self.paragraph(buf);
            return;
        }
        if !rest.ends_with('\n') {
            // Need the whole info string.
            self.line = Line::Prefix(buf.to_owned());
            return;
        }
        let info = rest[run..].trim();
        if first == '`' && info.contains('`') {
            self.paragraph(buf);
            return;
        }

        self.emit_indent(indent);
        let w = self.line_width().saturating_sub(indent).max(1);
        let header = if info.is_empty() {
            "─".repeat(w)
        } else {
            let label = format!("── {} ", info);
            let fill = w.saturating_sub(label.width());
            format!("{label}{}", "─".repeat(fill))
        };
        self.emit(&header, self.theme.fence);
        self.emit_newline();
        self.fence = Some(Fence {
            ch: first,
            len: run,
        });
        self.inline = Inline::default();
        self.line = Line::FenceStart(String::new());
    }

    /// Inside a fence: decide whether the held line start is the closing
    /// fence, or stream it as code.
    fn resolve_fence_start(&mut self) {
        let Line::FenceStart(buf) = std::mem::take(&mut self.line) else {
            unreachable!("bug: resolve_fence_start outside FenceStart state");
        };
        let Some(fence) = &self.fence else {
            unreachable!("bug: FenceStart without an open fence");
        };

        enum Verdict {
            More,
            Flush,
            Close,
        }

        let spaces = buf.chars().take_while(|&c| c == ' ').count();
        let rest = &buf[spaces..];
        let run = rest.chars().take_while(|&c| c == fence.ch).count();
        let tail = &rest[run..];
        let verdict = if buf.len() > MAX_HOLD || spaces > 3 || (run == 0 && !rest.is_empty()) {
            Verdict::Flush
        } else if rest.is_empty() || tail.is_empty() || tail.chars().all(|c| c == ' ') {
            Verdict::More
        } else if run >= fence.len
            && tail.ends_with('\n')
            && tail[..tail.len() - 1].chars().all(|c| c == ' ')
        {
            Verdict::Close
        } else {
            Verdict::Flush
        };

        match verdict {
            Verdict::More => self.line = Line::FenceStart(buf),
            Verdict::Flush => {
                if let Some(body) = buf.strip_suffix('\n') {
                    self.emit(body, self.theme.code_block);
                    self.emit_newline();
                    self.line = Line::FenceStart(String::new());
                } else {
                    self.emit(&buf, self.theme.code_block);
                    self.line = Line::FenceBody;
                }
            }
            Verdict::Close => {
                let w = self.line_width();
                self.emit(&"─".repeat(w), self.theme.fence);
                self.emit_newline();
                self.fence = None;
                self.reset_line();
            }
        }
    }

    // ----------------------------------------------------------------------
    // Tables

    fn table_candidate_char(&mut self, ch: char) {
        let Line::TableCandidate {
            indent,
            header,
            mut delim,
        } = std::mem::take(&mut self.line)
        else {
            unreachable!("bug: table_candidate_char outside TableCandidate state");
        };
        delim.push(ch);
        if ch == '\n' && parse_delimiter_row(&delim).is_some() {
            self.line = Line::Table {
                table: TableAcc {
                    indent,
                    header,
                    delim,
                    rows: Vec::new(),
                },
                cur: String::new(),
            };
        } else if ch == '\n' || !is_delimiter_char(ch) || delim.len() > MAX_HOLD {
            // Not a table after all: the held line was ordinary text.
            self.paragraph(&header);
            self.feed_str(&delim);
        } else {
            self.line = Line::TableCandidate {
                indent,
                header,
                delim,
            };
        }
    }

    fn table_char(&mut self, ch: char) {
        let Line::Table { mut table, mut cur } = std::mem::take(&mut self.line) else {
            unreachable!("bug: table_char outside Table state");
        };
        let at_row_start = cur.chars().all(|c| c == ' ');
        if at_row_start && ch != ' ' && ch != '|' {
            // Anything but a row ends the table; the line is then parsed
            // normally (a blank line stays a blank line).
            self.render_table(table);
            cur.push(ch);
            self.feed_str(&cur);
            return;
        }
        cur.push(ch);
        if ch == '\n' {
            table.rows.push(std::mem::take(&mut cur));
        }
        self.line = Line::Table { table, cur };
    }

    /// Render inline Markdown in isolation (for table cells), leaving the
    /// main output and inline state untouched.
    fn render_inline(&mut self, text: &str, base: Style) -> Vec<Segment> {
        let saved_out = std::mem::take(&mut self.out);
        let saved_inline = std::mem::take(&mut self.inline);
        self.inline.base = base;
        for ch in text.chars() {
            self.inline_char(ch);
        }
        self.finish_inline();
        let cell = std::mem::replace(&mut self.out, saved_out);
        self.inline = saved_inline;
        cell
    }

    /// Draw an accumulated table with box characters, then reset to a fresh
    /// line.
    fn render_table(&mut self, table: TableAcc) {
        let header_cells = parse_row(&table.header);
        let ncols = header_cells.len();
        let mut aligns = parse_delimiter_row(&table.delim).unwrap_or_default();
        aligns.resize(ncols, Align::Left);

        let header: Vec<Vec<Segment>> = header_cells
            .iter()
            .map(|c| self.render_inline(c, self.theme.table_header))
            .collect();
        let body: Vec<Vec<Vec<Segment>>> = table
            .rows
            .iter()
            .map(|row| {
                let mut cells = parse_row(row);
                cells.resize(ncols, String::new());
                cells
                    .iter()
                    .map(|c| self.render_inline(c, Style::PLAIN))
                    .collect()
            })
            .collect();

        // Each column costs its content width plus `│ ` and ` ` framing,
        // plus the closing `│`.
        let frame = 3 * ncols + 1;
        let available = self.line_width().saturating_sub(table.indent + frame);
        let Some(widths) = column_widths(
            std::iter::once(&header).chain(body.iter()),
            ncols,
            available,
        ) else {
            // Too narrow for a box: show the source lines as text.
            self.reset_line();
            self.paragraph(&table.header);
            self.paragraph(&table.delim);
            for row in &table.rows {
                self.paragraph(row);
            }
            return;
        };

        let indent = table.indent;
        let border_style = self.theme.table_border;
        let line = |this: &mut Self, text: &str| {
            this.emit_indent(indent);
            this.emit(text, border_style);
            this.emit_newline();
        };

        line(self, &border(&widths, '┌', '┬', '┐'));
        self.emit_table_row(&header, &widths, &aligns, indent);
        line(self, &border(&widths, '├', '┼', '┤'));
        for row in &body {
            self.emit_table_row(row, &widths, &aligns, indent);
        }
        line(self, &border(&widths, '└', '┴', '┘'));
        self.reset_line();
    }

    fn emit_table_row(
        &mut self,
        cells: &[Vec<Segment>],
        widths: &[usize],
        aligns: &[Align],
        indent: usize,
    ) {
        let wrapped: Vec<Vec<Vec<Segment>>> = cells
            .iter()
            .zip(widths)
            .map(|(cell, &w)| wrap_cell(cell, w))
            .collect();
        let height = wrapped.iter().map(Vec::len).max().unwrap_or(1).max(1);
        let border_style = self.theme.table_border;
        for li in 0..height {
            self.emit_indent(indent);
            self.emit("│", border_style);
            for (ci, lines) in wrapped.iter().enumerate() {
                self.emit(" ", Style::PLAIN);
                let empty = Vec::new();
                let line = lines.get(li).unwrap_or(&empty);
                for seg in pad_line(line, widths[ci], aligns[ci]) {
                    self.emit_href(&seg.text, seg.style, seg.href.as_deref());
                }
                self.emit(" ", Style::PLAIN);
                self.emit("│", border_style);
            }
            self.emit_newline();
        }
    }

    // ----------------------------------------------------------------------
    // Inline content

    /// Resolve whatever is pending at the end of a line or message, and
    /// drop all open inline styles.
    fn finish_inline(&mut self) {
        self.resolve_pending(None);
        if let Some(hold) = self.inline.link.take() {
            self.flush_link_literal(hold);
            self.resolve_pending(None);
        }
        self.inline.bold = None;
        self.inline.italic = None;
        self.inline.strike = false;
        self.inline.code = None;
    }

    fn inline_char(&mut self, ch: char) {
        if let Some(hold) = self.inline.link.take() {
            match hold {
                LinkHold::Text { mut text, image } => {
                    if ch == ']' {
                        self.inline.link = Some(LinkHold::AfterText { text, image });
                    } else if text.len() >= MAX_HOLD {
                        self.flush_link_literal(LinkHold::Text { text, image });
                        self.inline_char(ch);
                    } else {
                        text.push(ch);
                        self.inline.link = Some(LinkHold::Text { text, image });
                    }
                    return;
                }
                LinkHold::AfterText { text, image } => {
                    if ch == '(' {
                        self.inline.link = Some(LinkHold::Url {
                            text,
                            url: String::new(),
                            image,
                        });
                        return;
                    }
                    self.flush_link_literal(LinkHold::AfterText { text, image });
                    // Fall through: `ch` is ordinary text.
                }
                LinkHold::Url {
                    text,
                    mut url,
                    image,
                } => {
                    if ch == ')' {
                        self.emit_link(&text, &url, image);
                    } else if url.len() >= MAX_URL_HOLD {
                        self.flush_link_literal(LinkHold::Url { text, url, image });
                        self.inline_char(ch);
                    } else {
                        url.push(ch);
                        self.inline.link = Some(LinkHold::Url { text, url, image });
                    }
                    return;
                }
            }
        }

        match self.inline.pending {
            Pending::None => {}
            Pending::Run(d, n) => {
                if ch == d {
                    self.inline.pending = Pending::Run(d, n + 1);
                    return;
                }
                self.inline.pending = Pending::None;
                self.resolve_run(d, n, Some(ch));
            }
            Pending::Backslash => {
                self.inline.pending = Pending::None;
                if ch.is_ascii_punctuation() {
                    self.emit_char(ch);
                    return;
                }
                self.emit_char('\\');
            }
            Pending::Bang => {
                self.inline.pending = Pending::None;
                if ch == '[' && self.may_start_link() {
                    self.inline.link = Some(LinkHold::Text {
                        text: String::new(),
                        image: true,
                    });
                    return;
                }
                self.emit_char('!');
            }
        }

        if self.inline.code.is_some() {
            if ch == '`' {
                self.inline.pending = Pending::Run(ch, 1);
            } else {
                self.emit_char(ch);
            }
            return;
        }

        match ch {
            '`' | '*' | '_' | '~' => self.inline.pending = Pending::Run(ch, 1),
            '\\' => self.inline.pending = Pending::Backslash,
            '!' => self.inline.pending = Pending::Bang,
            '[' if self.may_start_link() => {
                self.inline.link = Some(LinkHold::Text {
                    text: String::new(),
                    image: false,
                });
            }
            _ => self.emit_char(ch),
        }
    }

    fn may_start_link(&self) -> bool {
        !self.inline.literal_brackets && self.inline.in_link.is_none()
    }

    /// Resolve the pending lookahead given the character that follows it
    /// (`None` at the end of the line/message).
    fn resolve_pending(&mut self, next: Option<char>) {
        match std::mem::take(&mut self.inline.pending) {
            Pending::None => {}
            Pending::Run(d, n) => self.resolve_run(d, n, next),
            // A trailing backslash is a hard line break: nothing to show.
            Pending::Backslash => {}
            Pending::Bang => self.emit_char('!'),
        }
    }

    /// Decide what a run of `n` delimiter characters `d` means, given the
    /// characters before and after it.
    fn resolve_run(&mut self, d: char, n: usize, next: Option<char>) {
        let before = self.inline.prev;
        match d {
            '`' => {
                match self.inline.code {
                    Some(open) if open == n => self.inline.code = None,
                    Some(_) => self.emit_literal_run(d, n),
                    // A code span can't open at the end of a line.
                    None if next.is_none() => self.emit_literal_run(d, n),
                    None => self.inline.code = Some(n),
                }
                self.inline.prev = Some(d);
            }
            '~' => {
                let (can_open, can_close) = flanking(d, before, next);
                if n >= 2 && self.inline.strike && can_close {
                    self.inline.strike = false;
                    self.emit_literal_run(d, n - 2);
                } else if n >= 2 && !self.inline.strike && can_open {
                    self.inline.strike = true;
                    self.emit_literal_run(d, n - 2);
                } else {
                    self.emit_literal_run(d, n);
                }
                self.inline.prev = Some(d);
            }
            '*' | '_' => {
                let (can_open, can_close) = flanking(d, before, next);
                let mut n = n;
                if can_close {
                    if n >= 2 && self.inline.bold == Some(d) {
                        self.inline.bold = None;
                        n -= 2;
                    }
                    if n >= 1 && self.inline.italic == Some(d) {
                        self.inline.italic = None;
                        n -= 1;
                    }
                }
                if can_open {
                    if n >= 2 && self.inline.bold.is_none() {
                        self.inline.bold = Some(d);
                        n -= 2;
                    }
                    if n >= 1 && self.inline.italic.is_none() {
                        self.inline.italic = Some(d);
                        n -= 1;
                    }
                }
                self.emit_literal_run(d, n);
                self.inline.prev = Some(d);
            }
            _ => unreachable!("bug: unexpected delimiter run {d:?}"),
        }
    }

    /// Emit a completed `[text](url)` link.
    fn emit_link(&mut self, text: &str, url: &str, image: bool) {
        // Inline styles opened inside the link text must not leak out.
        let saved = (
            self.inline.bold,
            self.inline.italic,
            self.inline.strike,
            self.inline.code,
        );
        self.inline.in_link = Some(url.to_owned());
        self.inline.literal_brackets = true;
        for ch in text.chars() {
            self.inline_char(ch);
        }
        self.resolve_pending(None);
        self.inline.literal_brackets = false;
        self.inline.in_link = None;
        (
            self.inline.bold,
            self.inline.italic,
            self.inline.strike,
            self.inline.code,
        ) = saved;

        if image || !same_target(text, url) {
            let style = self.inline.base.merge(self.theme.link_url);
            self.emit(&format!(" ({url})"), style);
        }
        self.inline.prev = Some(')');
    }

    /// Emit a held link fragment as the literal text it was.
    fn flush_link_literal(&mut self, hold: LinkHold) {
        let (text, image, suffix) = match hold {
            LinkHold::Text { text, image } => (text, image, String::new()),
            LinkHold::AfterText { text, image } => (text, image, "]".to_owned()),
            LinkHold::Url { text, url, image } => (text, image, format!("]({url}")),
        };
        self.inline.literal_brackets = true;
        if image {
            self.emit_char('!');
        }
        self.emit_char('[');
        for ch in text.chars() {
            self.inline_char(ch);
        }
        for ch in suffix.chars() {
            self.inline_char(ch);
        }
        self.inline.literal_brackets = false;
    }
}

/// CommonMark flanking rules: whether a delimiter run of `d` between
/// `before` and `after` (`None` = line boundary, treated as whitespace) can
/// open and/or close emphasis.
fn flanking(d: char, before: Option<char>, after: Option<char>) -> (bool, bool) {
    let before_ws = before.is_none_or(char::is_whitespace);
    let after_ws = after.is_none_or(char::is_whitespace);
    let before_p = before.is_some_and(is_punctuation);
    let after_p = after.is_some_and(is_punctuation);
    let left = !after_ws && (!after_p || before_ws || before_p);
    let right = !before_ws && (!before_p || after_ws || after_p);
    if d == '_' {
        // Underscores don't work intraword (`snake_case_name`).
        (left && (!right || before_p), right && (!left || after_p))
    } else {
        (left, right)
    }
}

fn is_punctuation(c: char) -> bool {
    c.is_ascii_punctuation()
        || matches!(
            c,
            '“' | '”'
                | '‘'
                | '’'
                | '—'
                | '–'
                | '…'
                | '«'
                | '»'
                | '¿'
                | '¡'
                | '§'
                | '¶'
                | '•'
        )
}

/// Whether link text and URL denote the same thing, so the URL needn't be
/// shown separately.
fn same_target(text: &str, url: &str) -> bool {
    let norm = |s: &str| {
        let s = s.trim().trim_end_matches('/');
        s.strip_prefix("https://")
            .or_else(|| s.strip_prefix("http://"))
            .or_else(|| s.strip_prefix("mailto:"))
            .unwrap_or(s)
            .to_owned()
    };
    norm(text) == norm(url)
}
