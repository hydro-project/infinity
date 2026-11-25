use crate::{Color, Renderer, Segment, Style};

/// Render `input` in one push (plus `finish`) into a tagged string: plain
/// text verbatim, styled runs as `{tags:text}`.
fn render(input: &str) -> String {
    let mut md = Renderer::new();
    md.set_width(21);
    let mut out = md.push(input);
    out.extend(md.finish());
    tagged(&out)
}

fn tagged(segments: &[Segment]) -> String {
    // Coalesce adjacent same-style segments so that the result doesn't
    // depend on how output was split across `push` calls.
    let mut merged: Vec<Segment> = Vec::new();
    for seg in segments {
        if let Some(last) = merged.last_mut()
            && last.style == seg.style
            && last.href == seg.href
        {
            last.text.push_str(&seg.text);
        } else {
            merged.push(seg.clone());
        }
    }
    let mut s = String::new();
    for seg in &merged {
        if seg.style == Style::PLAIN && seg.href.is_none() {
            s.push_str(&seg.text);
            continue;
        }
        let mut tags = Vec::new();
        if seg.style.bold {
            tags.push("b".to_owned());
        }
        if seg.style.italic {
            tags.push("i".to_owned());
        }
        if seg.style.underline {
            tags.push("u".to_owned());
        }
        if seg.style.strikethrough {
            tags.push("s".to_owned());
        }
        if seg.style.dim {
            tags.push("d".to_owned());
        }
        if let Some(fg) = seg.style.fg {
            tags.push(format!("{fg:?}"));
        }
        if let Some(href) = &seg.href {
            tags.push(format!("href={href}"));
        }
        s.push('{');
        s.push_str(&tags.join(","));
        s.push(':');
        s.push_str(&seg.text);
        s.push('}');
    }
    s
}

fn plain(segments: &[Segment]) -> String {
    segments.iter().map(|s| s.text.as_str()).collect()
}

// --------------------------------------------------------------------------
// Inline

#[test]
fn bold_italic_strike() {
    assert_eq!(render("a **b** c"), "a {b:b} c");
    assert_eq!(render("a *i* c"), "a {i:i} c");
    assert_eq!(render("a _i_ c"), "a {i:i} c");
    assert_eq!(render("a __b__ c"), "a {b:b} c");
    assert_eq!(render("a ~~s~~ c"), "a {s:s} c");
    assert_eq!(render("***bi***"), "{b,i:bi}");
    assert_eq!(render("**b *bi* b**"), "{b:b }{b,i:bi}{b: b}");
    assert_eq!(render("*i **bi** i*"), "{i:i }{b,i:bi}{i: i}");
}

#[test]
fn bold_adjacent_to_punctuation() {
    assert_eq!(render("**Note:** read this"), "{b:Note:} read this");
    assert_eq!(render("(**x**)"), "({b:x})");
    assert_eq!(render("**x**, **y**."), "{b:x}, {b:y}.");
}

#[test]
fn intraword_underscores_are_literal() {
    assert_eq!(render("snake_case_name"), "snake_case_name");
    assert_eq!(render("a*b*c"), "a{i:b}c");
}

#[test]
fn space_surrounded_stars_are_literal() {
    assert_eq!(render("2 * 3 * 4"), "2 * 3 * 4");
    assert_eq!(render("2 ** 3"), "2 ** 3");
    assert_eq!(render("a ~ b"), "a ~ b");
}

#[test]
fn unclosed_emphasis_ends_at_line_end() {
    assert_eq!(render("**oops\nnext"), "{b:oops}\nnext");
}

#[test]
fn trailing_delimiters_are_literal() {
    assert_eq!(render("stars **"), "stars **");
    assert_eq!(render("tick `"), "tick `");
    assert_eq!(render("a\\"), "a");
}

#[test]
fn code_spans() {
    assert_eq!(
        render("run `cargo **x**` now"),
        "run {Cyan:cargo **x**} now"
    );
    assert_eq!(render("`` a ` b ``"), "{Cyan: a ` b }");
    assert_eq!(render("`[x](y)`"), "{Cyan:[x](y)}");
    assert_eq!(render("`unclosed\nplain"), "{Cyan:unclosed}\nplain");
}

#[test]
fn escapes() {
    assert_eq!(render("\\*not italic\\*"), "*not italic*");
    assert_eq!(render("back\\slash"), "back\\slash");
    assert_eq!(render("\\# not heading"), "# not heading");
}

#[test]
fn links() {
    assert_eq!(
        render("see [docs](https://x.io/d) now"),
        "see {u,Blue,href=https://x.io/d:docs}{d: (https://x.io/d)} now"
    );
    assert_eq!(render("[**b**](u)"), "{b,u,Blue,href=u:b}{d: (u)}");
    assert_eq!(
        render("<https://x.io> and https://y.io"),
        "<https://x.io> and https://y.io"
    );
}

#[test]
fn link_url_hidden_when_same_as_text() {
    assert_eq!(
        render("[https://x.io/](https://x.io)"),
        "{u,Blue,href=https://x.io:https://x.io/}"
    );
    assert_eq!(
        render("[x.io/a](https://x.io/a)"),
        "{u,Blue,href=https://x.io/a:x.io/a}"
    );
}

#[test]
fn images() {
    assert_eq!(
        render("![alt](img.png)"),
        "{u,Blue,href=img.png:alt}{d: (img.png)}"
    );
    assert_eq!(render("wow! yes"), "wow! yes");
    assert_eq!(render("wow!"), "wow!");
}

#[test]
fn brackets_without_url_are_literal() {
    assert_eq!(render("arr[0] = 1"), "arr[0] = 1");
    assert_eq!(render("[x] done"), "[x] done");
    assert_eq!(render("[x](y"), "[x](y");
    assert_eq!(render("a [b\nc"), "a [b\nc");
    assert_eq!(render("[*i*] x"), "[{i:i}] x");
}

#[test]
fn emphasis_does_not_leak_out_of_links() {
    assert_eq!(render("[**x](u) y"), "{b,u,Blue,href=u:x}{d: (u)} y");
}

// --------------------------------------------------------------------------
// Blocks

#[test]
fn headings() {
    assert_eq!(render("# Title\n"), "{b,Magenta:Title}\n");
    assert_eq!(render("### Sub **b**\n"), "{b,Magenta:Sub b}\n");
    assert_eq!(render("#hashtag"), "#hashtag");
    assert_eq!(render("####### seven"), "####### seven");
    assert_eq!(render("#\n"), "\n");
}

#[test]
fn bullet_lists() {
    assert_eq!(render("- a\n* b\n+ c\n"), "• a\n• b\n• c\n");
    assert_eq!(render("- a\n  - b\n"), "• a\n  • b\n");
    assert_eq!(render("- **a** b\n"), "• {b:a} b\n");
    assert_eq!(render("-\n"), "• \n");
    assert_eq!(render("-5 degrees"), "-5 degrees");
    assert_eq!(render("-- dashes"), "-- dashes");
    assert_eq!(render("*italic* start"), "{i:italic} start");
}

#[test]
fn task_lists() {
    assert_eq!(render("- [ ] todo\n- [x] done\n"), "☐ todo\n☑ done\n");
    assert_eq!(render("- [X] done\n"), "☑ done\n");
    assert_eq!(render("- [y] not a task\n"), "• [y] not a task\n");
    assert_eq!(render("- [ \n"), "• [ \n");
}

#[test]
fn ordered_lists() {
    assert_eq!(render("1. a\n2) b\n"), "1. a\n2) b\n");
    assert_eq!(render("10. ten\n"), "10. ten\n");
    assert_eq!(render("3.14 is pi"), "3.14 is pi");
    assert_eq!(render("2024 was"), "2024 was");
    assert_eq!(render("1.\n"), "1. \n");
    assert_eq!(render("1234567890. big"), "1234567890. big");
}

#[test]
fn block_quotes() {
    assert_eq!(render("> hi\n"), "{d:│ }{i:hi}\n");
    assert_eq!(render("> > deep\n"), "{d:│ │ }{i:deep}\n");
    assert_eq!(render("> **b**\n"), "{d:│ }{b,i:b}\n");
    assert_eq!(render("> # h\n"), "{d:│ }{b,i,Magenta:h}\n");
    assert_eq!(render(">\n"), "{d:│ }\n");
    assert_eq!(render("a > b"), "a > b");
}

#[test]
fn thematic_breaks() {
    assert_eq!(render("---\n"), "{d:────────────────────}\n");
    assert_eq!(render("***\n"), "{d:────────────────────}\n");
    assert_eq!(render("___\n"), "{d:────────────────────}\n");
    assert_eq!(render("- - -\n"), "{d:────────────────────}\n");
    assert_eq!(render("--\n"), "--\n");
    assert_eq!(render("--- not a rule"), "--- not a rule");
}

#[test]
fn blank_lines_and_indent() {
    assert_eq!(render("a\n\nb\n"), "a\n\nb\n");
    assert_eq!(render("   \n"), "\n");
    assert_eq!(render("  indented"), "  indented");
}

#[test]
fn fenced_code_blocks() {
    assert_eq!(
        render("```rust\nlet x = **1**;\n```\nafter"),
        "{d:── rust ────────────}\nlet x = **1**;\n{d:────────────────────}\nafter"
    );
    assert_eq!(
        render("```\n# not heading\n- not list\n```\n"),
        "{d:────────────────────}\n# not heading\n- not list\n{d:────────────────────}\n"
    );
    assert_eq!(
        render("~~~\n```\n~~~\n"),
        "{d:────────────────────}\n```\n{d:────────────────────}\n"
    );
    // Shorter closing fence doesn't close.
    assert_eq!(
        render("````\n```\nx\n````\n"),
        "{d:────────────────────}\n```\nx\n{d:────────────────────}\n"
    );
    // Code block style is plain by default, so the line passes through.
    assert_eq!(
        render("```\n  ``` no\n```\n"),
        "{d:────────────────────}\n  ``` no\n{d:────────────────────}\n"
    );
}

#[test]
fn fence_needs_three_and_no_backticks_in_info() {
    assert_eq!(render("``not fence``"), "{Cyan:not fence}");
    assert_eq!(render("``` a ` b\n"), "{Cyan: a ` b}\n");
}

#[test]
fn unclosed_fence_ends_with_message() {
    let mut md = Renderer::new();
    md.set_width(21);
    let mut out = md.push("```\ncode");
    out.extend(md.finish());
    assert_eq!(tagged(&out), "{d:────────────────────}\ncode");
    // The next message starts fresh.
    let out = md.push("**b**");
    assert_eq!(tagged(&out), "{b:b}");
}

#[test]
fn rule_width_follows_terminal_width() {
    let mut md = Renderer::new();
    md.set_width(6);
    assert_eq!(plain(&md.push("---\n")), "─────\n");
    md.set_width(0);
    assert_eq!(plain(&md.push("---\n")), "─\n");
}

#[test]
fn mixed_document() {
    let doc = "\
# Plan

Some *text* with `code` and a [link](https://e.com/x).

- [x] step one
- [ ] step **two**
  1. nested

> note

```sh
echo hi
```
";
    assert_eq!(
        render(doc),
        "\
{b,Magenta:Plan}

Some {i:text} with {Cyan:code} and a {u,Blue,href=https://e.com/x:link}{d: (https://e.com/x)}.

☑ step one
☐ step {b:two}
  1. nested

{d:│ }{i:note}

{d:── sh ──────────────}
echo hi
{d:────────────────────}
"
    );
}

// --------------------------------------------------------------------------
// Streaming

#[test]
fn finish_without_trailing_newline() {
    let mut md = Renderer::new();
    let mut out = md.push("**done**");
    out.extend(md.finish());
    assert_eq!(tagged(&out), "{b:done}");
}

#[test]
fn finish_flushes_held_prefix() {
    let mut md = Renderer::new();
    let mut out = md.push("1.");
    assert!(out.is_empty());
    out.extend(md.finish());
    assert_eq!(tagged(&out), "1. ");

    let mut out = md.push("- [");
    assert!(out.is_empty());
    out.extend(md.finish());
    assert_eq!(tagged(&out), "• [");

    let mut out = md.push("---");
    assert!(out.is_empty());
    out.extend(md.finish());
    assert_eq!(
        tagged(&out),
        "{d:───────────────────────────────────────────────────────────────────────────────}"
    );

    let mut out = md.push("**");
    assert!(out.is_empty());
    out.extend(md.finish());
    assert_eq!(tagged(&out), "**");
}

#[test]
fn text_streams_without_waiting_for_line_end() {
    let mut md = Renderer::new();
    assert_eq!(plain(&md.push("Hello, ")), "Hello, ");
    assert_eq!(plain(&md.push("**wor")), "wor");
    assert_eq!(plain(&md.push("ld")), "ld");
    assert_eq!(plain(&md.push("** and more")), " and more");
}

/// Output must not depend on where chunk boundaries fall.
#[test]
fn chunking_is_transparent() {
    let docs = [
        "a **b** *c* ~~d~~ `e` [f](g) ![h](i) \\* j\n",
        "# H\n- [ ] t\n- [x] u\n1. v\n> w\n---\n```rs\nx\n```\n",
        "***bi*** __b__ _i_ a*b*c snake_case 2 * 3\n\n  - nested\n",
        "**unclosed\n`code\n[link\n[x](\n--\n``` a ` b\n",
        "> > deep **b**\n>\n- - -\n~~~~\ncode\n~~~\n~~~~\n",
    ];
    for doc in docs {
        let mut whole = Renderer::new();
        let mut expected = whole.push(doc);
        expected.extend(whole.finish());
        let expected = tagged(&expected);

        // Every single-char split.
        let mut per_char = Renderer::new();
        let mut out = Vec::new();
        for ch in doc.chars() {
            out.extend(per_char.push(&ch.to_string()));
        }
        out.extend(per_char.finish());
        assert_eq!(tagged(&out), expected, "per-char chunking of {doc:?}");

        // Every two-way split.
        for (i, _) in doc.char_indices() {
            let mut split = Renderer::new();
            let mut out = split.push(&doc[..i]);
            out.extend(split.push(&doc[i..]));
            out.extend(split.finish());
            assert_eq!(tagged(&out), expected, "split at {i} of {doc:?}");
        }
    }
}

#[test]
fn segments_coalesce() {
    let mut md = Renderer::new();
    let mut out = md.push("ab");
    out.extend(md.push("cd"));
    out.extend(md.finish());
    assert_eq!(out.len(), 2, "pushes are drained separately");
    let mut md = Renderer::new();
    let mut out = md.push("abcd **e**");
    out.extend(md.finish());
    assert_eq!(out.len(), 2);
    assert_eq!(out[0].text, "abcd ");
    assert_eq!(out[1].style.fg, None);
    assert_eq!(
        out[1].style,
        Style {
            bold: true,
            ..Style::PLAIN
        }
    );
}

#[test]
fn theme_colors_are_applied() {
    let out = Renderer::new().push("`x`");
    assert_eq!(out[0].style.fg, Some(Color::Cyan));
}

// --------------------------------------------------------------------------
// Tables

#[test]
fn simple_table() {
    assert_eq!(
        render("| a | bb |\n|---|---|\n| 1 | 2 |\n| 333 | 4 |\n"),
        "\
┌─────┬────┐
│ {b:a}   │ {b:bb} │
├─────┼────┤
│ 1   │ 2  │
│     │    │
│ 333 │ 4  │
└─────┴────┘
"
    );
}

#[test]
fn table_alignment_and_inline_markup() {
    assert_eq!(
        render("| L | C | R |\n|:--|:-:|--:|\n| **x** | `y` | [z](u) |\n"),
        "\
┌───┬───┬───────┐
│ {b:L} │ {b:C} │     {b:R} │
├───┼───┼───────┤
│ {b:x} │ {Cyan:y} │ {u,Blue,href=u:z}{d: (u)} │
└───┴───┴───────┘
"
    );
}

#[test]
fn table_cells_wrap_to_width() {
    // width 21 → line width 20; 2 cols cost 7 → 13 for content.
    assert_eq!(
        render("| name | description |\n|---|---|\n| x | a fairly long description here |\n"),
        "\
┌──────┬───────────┐
│ {b:name} │ {b:descripti} │
│      │ {b:on}        │
├──────┼───────────┤
│ x    │ a fairly  │
│      │ long      │
│      │ descripti │
│      │ on here   │
└──────┴───────────┘
"
    );
}

#[test]
fn table_too_narrow_falls_back_to_text() {
    let mut md = Renderer::new();
    md.set_width(8);
    let mut out = md.push("| a | b |\n|---|---|\n| 1 | 2 |\n");
    out.extend(md.finish());
    assert_eq!(plain(&out), "| a | b |\n|---|---|\n| 1 | 2 |\n");
}

#[test]
fn table_ragged_rows_and_escaped_pipes() {
    assert_eq!(
        render("| a | b |\n|---|---|\n| 1 |\n| 2 | 3 | 4 |\n| x \\| y | z |\n"),
        "\
┌───────┬───┐
│ {b:a}     │ {b:b} │
├───────┼───┤
│ 1     │   │
│       │   │
│ 2     │ 3 │
│       │   │
│ x | y │ z │
└───────┴───┘
"
    );
}

#[test]
fn table_ends_at_non_row_line() {
    assert_eq!(
        render("| a |\n|---|\n| 1 |\nafter **b**\n"),
        "┌───┐\n│ {b:a} │\n├───┤\n│ 1 │\n└───┘\nafter {b:b}\n"
    );
    assert_eq!(
        render("| a |\n|---|\n| 1 |\n\nnext\n"),
        "┌───┐\n│ {b:a} │\n├───┤\n│ 1 │\n└───┘\n\nnext\n"
    );
    assert_eq!(
        render("- item\n| a |\n|---|\n| 1 |\n- item\n"),
        "• item\n┌───┐\n│ {b:a} │\n├───┤\n│ 1 │\n└───┘\n• item\n"
    );
}

#[test]
fn table_without_trailing_newline_at_finish() {
    assert_eq!(
        render("| a |\n|---|\n| 1 |"),
        "┌───┐\n│ {b:a} │\n├───┤\n│ 1 │\n└───┘"
    );
    // Header only (no body rows yet); the source newline is kept.
    assert_eq!(render("| a |\n|---|\n"), "┌───┐\n│ {b:a} │\n├───┤\n└───┘\n");
}

#[test]
fn pipe_line_without_delimiter_is_text() {
    assert_eq!(
        render("| not | a table |\nplain\n"),
        "| not | a table |\nplain\n"
    );
    assert_eq!(render("|x|\n|y|\n"), "|x|\n|y|\n");
    assert_eq!(render("| a |\n"), "| a |\n");
    assert_eq!(render("| a |"), "| a |");
    assert_eq!(render("| a |\n|-"), "| a |\n|-");
    assert_eq!(render("a | b\n--|--\n"), "a | b\n--|--\n");
}

#[test]
fn table_holds_until_complete_then_streams_rest() {
    let mut md = Renderer::new();
    assert!(md.push("| a |\n|---|\n| 1 |\n").is_empty());
    let out = md.push("done");
    assert_eq!(plain(&out), "┌───┐\n│ a │\n├───┤\n│ 1 │\n└───┘\ndone");
}

#[test]
fn candidate_rejected_early_streams_header() {
    let mut md = Renderer::new();
    assert!(md.push("| a | b |\n").is_empty());
    // Second line can't be a delimiter row as soon as `x` arrives, so the
    // header is released (`|x` itself is now held as a possible header).
    assert_eq!(plain(&md.push("|x")), "| a | b |\n");
    assert_eq!(plain(&md.push(" y\nz")), "|x y\nz");
}

#[test]
fn table_chunking_is_transparent() {
    let docs = [
        "| a | b |\n|---|:-:|\n| **1** | 2 |\n| 3 | `4` |\n\nafter\n",
        "| a |\n|---|\n| 1 |",
        "| a |\n|x|\n",
        "| a |\n|---\n",
        "text\n| a | b |\n|---|---|\n| 1 |\n- list\n",
    ];
    for doc in docs {
        let mut whole = Renderer::new();
        let mut expected = whole.push(doc);
        expected.extend(whole.finish());
        let expected = tagged(&expected);

        let mut per_char = Renderer::new();
        let mut out = Vec::new();
        for ch in doc.chars() {
            out.extend(per_char.push(&ch.to_string()));
        }
        out.extend(per_char.finish());
        assert_eq!(tagged(&out), expected, "per-char chunking of {doc:?}");
    }
}
