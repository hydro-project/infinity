//! Sanitizing of externally sourced text before it reaches the terminal.

use std::borrow::Cow;

/// Parser state for [`strip_ansi`], mirroring the escape-sequence states of
/// a VT parser (<https://vt100.net/emu/dec_ansi_parser>) closely enough to
/// find the end of each sequence.
enum State {
    /// Plain text.
    Ground,
    /// Seen ESC, deciding the sequence type from the next byte.
    Esc,
    /// Inside `ESC` + intermediate bytes (an nF sequence), waiting for the
    /// final byte.
    EscIntermediate,
    /// Inside a CSI sequence (`ESC [`), waiting for the final byte.
    Csi,
    /// Inside an OSC/DCS/SOS/PM/APC string, waiting for BEL or ST (`ESC \`).
    EscString,
    /// Seen ESC inside an [`State::EscString`]: either the start of the ST
    /// terminator or an aborting new escape sequence.
    EscStringEsc,
}

/// Strip ANSI escape sequences and non-whitespace control characters from
/// text that originates outside the TUI (tool results, model output,
/// subscription events, subprocess output, ...).
///
/// The inline viewport tracks the cursor position by parsing everything it
/// prints (see `OutputTracker` in `inline_viewport`), and deliberately
/// understands only the small set of sequences the TUI itself emits —
/// anything else panics as a bug in our code. External text can contain
/// arbitrary escape sequences (e.g. `cat`ing a file with captured TUI
/// output), so it must be sanitized before printing.
///
/// Removes:
/// * CSI sequences (`ESC [ … final`), including SGR styling;
/// * OSC/DCS/SOS/PM/APC string sequences (terminated by BEL or `ESC \`);
/// * all other escape sequences (`ESC` + intermediates + final byte);
/// * C0 control characters and DEL, except `\t`, `\n`, and `\r`.
pub fn strip_ansi(text: &str) -> Cow<'_, str> {
    let needs_stripping = text
        .bytes()
        .any(|b| (b < 0x20 && b != b'\t' && b != b'\n' && b != b'\r') || b == 0x7f);
    if !needs_stripping {
        return Cow::Borrowed(text);
    }

    // Dispatch for a byte following ESC (from ground or aborting a string).
    let after_esc = |ch: char| match ch {
        '[' => State::Csi,
        // OSC / DCS / SOS / PM / APC: consume until BEL or ST.
        ']' | 'P' | 'X' | '^' | '_' => State::EscString,
        '\x20'..='\x2f' => State::EscIntermediate,
        // Any other byte completes a two-byte sequence (e.g. `ESC 7`).
        _ => State::Ground,
    };

    let mut out = String::with_capacity(text.len());
    let mut state = State::Ground;
    for ch in text.chars() {
        state = match state {
            State::Ground => match ch {
                '\x1b' => State::Esc,
                '\t' | '\n' | '\r' => {
                    out.push(ch);
                    State::Ground
                }
                '\x00'..='\x1f' | '\x7f' => State::Ground,
                _ => {
                    out.push(ch);
                    State::Ground
                }
            },
            State::Esc => after_esc(ch),
            State::EscIntermediate => match ch {
                '\x20'..='\x2f' => State::EscIntermediate,
                _ => State::Ground, // final byte
            },
            State::Csi => match ch {
                '\x40'..='\x7e' => State::Ground, // final byte
                _ => State::Csi,                  // parameter/intermediate bytes
            },
            State::EscString => match ch {
                '\x07' => State::Ground, // BEL terminator
                '\x1b' => State::EscStringEsc,
                _ => State::EscString,
            },
            State::EscStringEsc => match ch {
                '\\' => State::Ground, // ST terminator
                // ESC not starting ST aborts the string and begins a new
                // escape sequence (matching VT parser behavior).
                _ => after_esc(ch),
            },
        };
    }
    Cow::Owned(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_text_is_borrowed() {
        assert!(matches!(
            strip_ansi("hello world\nwith\ttabs\r\n"),
            Cow::Borrowed(_)
        ));
    }

    #[test]
    fn strips_sgr() {
        assert_eq!(strip_ansi("a \x1b[31mred\x1b[0m word"), "a red word");
    }

    #[test]
    fn strips_cursor_movement() {
        assert_eq!(strip_ansi("\x1b[2J\x1b[10;20Htext\x1b[1A"), "text");
    }

    #[test]
    fn strips_osc_with_bel_terminator() {
        assert_eq!(strip_ansi("\x1b]0;window title\x07after"), "after");
    }

    #[test]
    fn strips_osc_with_st_terminator() {
        assert_eq!(
            strip_ansi("\x1b]8;;http://x\x1b\\link\x1b]8;;\x1b\\"),
            "link"
        );
    }

    #[test]
    fn strips_dcs_string() {
        assert_eq!(strip_ansi("\x1bPq#0;2;0;0;0\x1b\\after"), "after");
    }

    #[test]
    fn esc_aborts_unterminated_osc() {
        // An ESC that doesn't start ST aborts the string and starts a new
        // sequence; the text after that sequence is kept.
        assert_eq!(strip_ansi("\x1b]0;title\x1b[31mred"), "red");
    }

    #[test]
    fn strips_two_byte_escapes() {
        assert_eq!(strip_ansi("\x1b7saved\x1b8restored\x1bc"), "savedrestored");
    }

    #[test]
    fn strips_nf_sequences() {
        // Designate character set: ESC + intermediate + final.
        assert_eq!(strip_ansi("\x1b(Btext"), "text");
    }

    #[test]
    fn strips_control_chars_keeps_whitespace() {
        assert_eq!(strip_ansi("a\x00b\x08c\td\ne\rf\x7fg"), "abc\td\ne\rfg");
    }

    #[test]
    fn keeps_unicode() {
        assert_eq!(strip_ansi("\x1b[1m✓ naïve 日本語\x1b[m"), "✓ naïve 日本語");
    }
}
