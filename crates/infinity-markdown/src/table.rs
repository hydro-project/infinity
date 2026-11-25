//! GFM table parsing and layout.

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::style::{Segment, Style};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Align {
    #[default]
    Left,
    Center,
    Right,
}

/// Split a table row into trimmed cell texts. Leading/trailing pipes are
/// dropped and `\|` becomes a literal `|`.
pub(crate) fn parse_row(line: &str) -> Vec<String> {
    let line = line.trim();
    let line = line.strip_prefix('|').unwrap_or(line);
    let line = match line.strip_suffix('|') {
        // Keep an escaped trailing pipe (`... \|`).
        Some(stripped) if !stripped.ends_with('\\') => stripped,
        _ => line,
    };

    let mut cells = vec![String::new()];
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' if chars.peek() == Some(&'|') => {
                chars.next();
                cells.last_mut().expect("bug: cells never empty").push('|');
            }
            '|' => cells.push(String::new()),
            _ => cells.last_mut().expect("bug: cells never empty").push(c),
        }
    }
    cells.into_iter().map(|c| c.trim().to_owned()).collect()
}

/// Parse a delimiter row (`| --- | :-: | --: |`) into per-column
/// alignments, or `None` if the line isn't one.
pub(crate) fn parse_delimiter_row(line: &str) -> Option<Vec<Align>> {
    let trimmed = line.trim();
    if !trimmed.starts_with('|') {
        return None;
    }
    let cells = parse_row(trimmed);
    cells
        .iter()
        .map(|cell| {
            let left = cell.starts_with(':');
            let right = cell.ends_with(':');
            let dashes = cell.trim_start_matches(':').trim_end_matches(':');
            if dashes.is_empty() || !dashes.chars().all(|c| c == '-') {
                return None;
            }
            Some(match (left, right) {
                (true, true) => Align::Center,
                (false, true) => Align::Right,
                _ => Align::Left,
            })
        })
        .collect()
}

/// Whether `ch` can appear in a delimiter row (used to reject a candidate
/// table as early as possible while streaming).
pub(crate) fn is_delimiter_char(ch: char) -> bool {
    matches!(ch, '|' | ':' | '-' | ' ' | '\n')
}

pub(crate) fn segments_width(segments: &[Segment]) -> usize {
    segments.iter().map(|s| s.text.width()).sum()
}

/// Column content widths: each column's natural width (widest cell), shrunk
/// widest-first until the sum fits in `available`. `None` when not even one
/// column of content per cell fits.
pub(crate) fn column_widths<'a>(
    rows: impl IntoIterator<Item = &'a Vec<Vec<Segment>>>,
    ncols: usize,
    available: usize,
) -> Option<Vec<usize>> {
    if available < ncols {
        return None;
    }
    let mut widths = vec![1; ncols];
    for row in rows {
        for (i, cell) in row.iter().enumerate().take(ncols) {
            widths[i] = widths[i].max(segments_width(cell));
        }
    }
    while widths.iter().sum::<usize>() > available {
        let (i, &w) = widths
            .iter()
            .enumerate()
            .max_by_key(|(_, w)| **w)
            .expect("bug: ncols >= 1 here");
        if w <= 1 {
            break;
        }
        widths[i] -= 1;
    }
    Some(widths)
}

/// Word-wrap a cell's segments into lines of at most `width` columns.
/// Words wider than `width` are split by character. Always returns at
/// least one (possibly empty) line.
pub(crate) fn wrap_cell(segments: &[Segment], width: usize) -> Vec<Vec<Segment>> {
    let width = width.max(1);
    // (char, index of the segment it came from)
    let chars: Vec<(char, usize)> = segments
        .iter()
        .enumerate()
        .flat_map(|(i, s)| s.text.chars().map(move |c| (c, i)))
        .collect();

    let mut lines: Vec<Vec<(char, usize)>> = vec![Vec::new()];
    let mut cur_w = 0;
    let push_hard =
        |lines: &mut Vec<Vec<(char, usize)>>, cur_w: &mut usize, word: &[(char, usize)]| {
            for &(c, seg) in word {
                let cw = c.width().unwrap_or(0);
                if *cur_w + cw > width && *cur_w > 0 {
                    lines.push(Vec::new());
                    *cur_w = 0;
                }
                lines
                    .last_mut()
                    .expect("bug: lines never empty")
                    .push((c, seg));
                *cur_w += cw;
            }
        };

    let mut i = 0;
    // Segment of the most recently skipped space, so a re-inserted space
    // keeps the style it had in the source.
    let mut space_seg = 0;
    while i < chars.len() {
        if chars[i].0 == ' ' {
            space_seg = chars[i].1;
            i += 1;
            continue;
        }
        let start = i;
        while i < chars.len() && chars[i].0 != ' ' {
            i += 1;
        }
        let word = &chars[start..i];
        let ww: usize = word.iter().map(|(c, _)| c.width().unwrap_or(0)).sum();

        if cur_w > 0 {
            if cur_w + 1 + ww <= width {
                lines
                    .last_mut()
                    .expect("bug: lines never empty")
                    .push((' ', space_seg));
                cur_w += 1;
            } else {
                lines.push(Vec::new());
                cur_w = 0;
            }
        }
        if ww <= width {
            lines
                .last_mut()
                .expect("bug: lines never empty")
                .extend_from_slice(word);
            cur_w += ww;
        } else {
            push_hard(&mut lines, &mut cur_w, word);
        }
    }

    lines
        .into_iter()
        .map(|line| {
            let mut out: Vec<Segment> = Vec::new();
            for (c, seg) in line {
                let src = &segments[seg];
                match out.last_mut() {
                    Some(last) if last.style == src.style && last.href == src.href => {
                        last.text.push(c)
                    }
                    _ => out.push(Segment {
                        text: c.to_string(),
                        style: src.style,
                        href: src.href.clone(),
                    }),
                }
            }
            out
        })
        .collect()
}

/// Pad one wrapped line to `width` columns according to `align`.
pub(crate) fn pad_line(line: &[Segment], width: usize, align: Align) -> Vec<Segment> {
    let w = segments_width(line);
    let fill = width.saturating_sub(w);
    let (left, right) = match align {
        Align::Left => (0, fill),
        Align::Right => (fill, 0),
        Align::Center => (fill / 2, fill - fill / 2),
    };
    let mut out = Vec::with_capacity(line.len() + 2);
    if left > 0 {
        out.push(Segment {
            text: " ".repeat(left),
            style: Style::PLAIN,
            href: None,
        });
    }
    out.extend_from_slice(line);
    if right > 0 {
        out.push(Segment {
            text: " ".repeat(right),
            style: Style::PLAIN,
            href: None,
        });
    }
    out
}

/// A horizontal border line: `left`, then per-column `─` runs joined by
/// `mid`, then `right`. Each column run is the content width plus one space
/// of padding on each side.
pub(crate) fn border(widths: &[usize], left: char, mid: char, right: char) -> String {
    let mut s = String::new();
    s.push(left);
    for (i, w) in widths.iter().enumerate() {
        if i > 0 {
            s.push(mid);
        }
        s.extend(std::iter::repeat_n('─', w + 2));
    }
    s.push(right);
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seg(text: &str) -> Segment {
        Segment {
            text: text.to_owned(),
            style: Style::PLAIN,
            href: None,
        }
    }

    fn texts(lines: &[Vec<Segment>]) -> Vec<String> {
        lines
            .iter()
            .map(|l| l.iter().map(|s| s.text.as_str()).collect())
            .collect()
    }

    #[test]
    fn parse_rows() {
        assert_eq!(parse_row("| a | b |"), ["a", "b"]);
        assert_eq!(parse_row("|a|b|\n"), ["a", "b"]);
        assert_eq!(parse_row("| a \\| b | c |"), ["a | b", "c"]);
        assert_eq!(parse_row("| a | b \\|"), ["a", "b |"]);
        assert_eq!(parse_row("| a | | c |"), ["a", "", "c"]);
        assert_eq!(parse_row("| `x|y` |"), ["`x", "y`"]);
    }

    #[test]
    fn delimiter_rows() {
        assert_eq!(
            parse_delimiter_row("|---|:-:|--:|:--|"),
            Some(vec![Align::Left, Align::Center, Align::Right, Align::Left])
        );
        assert_eq!(parse_delimiter_row("| - |"), Some(vec![Align::Left]));
        assert_eq!(parse_delimiter_row("| : |"), None);
        assert_eq!(parse_delimiter_row("| a |"), None);
        assert_eq!(parse_delimiter_row("---|---"), None);
        assert_eq!(parse_delimiter_row("|"), None);
    }

    #[test]
    fn widths_shrink_widest_first() {
        let rows = vec![vec![vec![seg("abcdefgh")], vec![seg("ab")]]];
        assert_eq!(column_widths(&rows, 2, 100), Some(vec![8, 2]));
        assert_eq!(column_widths(&rows, 2, 6), Some(vec![4, 2]));
        assert_eq!(column_widths(&rows, 2, 2), Some(vec![1, 1]));
        assert_eq!(column_widths(&rows, 2, 1), None);
    }

    #[test]
    fn wrap_words_and_long_words() {
        let cell = [seg("one two three")];
        assert_eq!(texts(&wrap_cell(&cell, 20)), ["one two three"]);
        assert_eq!(texts(&wrap_cell(&cell, 7)), ["one two", "three"]);
        assert_eq!(texts(&wrap_cell(&cell, 3)), ["one", "two", "thr", "ee"]);
        assert_eq!(texts(&wrap_cell(&[seg("")], 3)), [""]);
        assert_eq!(texts(&wrap_cell(&[seg("日本語")], 4)), ["日本", "語"]);
    }

    #[test]
    fn wrap_keeps_styles() {
        let bold = Segment {
            text: "a b".to_owned(),
            style: Style {
                bold: true,
                ..Style::PLAIN
            },
            href: None,
        };
        let lines = wrap_cell(&[bold, seg(" c")], 10);
        assert_eq!(lines.len(), 1);
        // Spaces keep the style they had in the source.
        assert_eq!(lines[0][0].text, "a b");
        assert!(lines[0][0].style.bold);
        assert_eq!(lines[0][1].text, " c");
    }

    #[test]
    fn padding() {
        let p = |a| -> String {
            pad_line(&[seg("ab")], 6, a)
                .iter()
                .map(|s| s.text.as_str())
                .collect()
        };
        assert_eq!(p(Align::Left), "ab    ");
        assert_eq!(p(Align::Right), "    ab");
        assert_eq!(p(Align::Center), "  ab  ");
    }

    #[test]
    fn borders() {
        assert_eq!(border(&[1, 2], '┌', '┬', '┐'), "┌───┬────┐");
    }
}
