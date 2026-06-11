//! Snapshot tests hunting for bugs around wide characters (CJK/emoji):
//! display-width vs char-count confusion in `wrap_tail`, the text input,
//! and the status row.
//!
//! These document current (possibly buggy) behavior — inspect the
//! snapshots before trusting them as "correct". Rendered through the
//! reflowing (`alacritty_terminal`) backend.

mod common;

use common::{HarnessOptions, TuiHarness};
use infinity_agent_core::batch_processor::DisplayEvent;
use rig_mock::MockStreamingResponse;

type Evt = DisplayEvent<MockStreamingResponse>;

// ── Wide characters (CJK / emoji) ───────────────────────────────────────────

/// Child-thread activity rows trim their buffers with `wrap_tail`, which
/// counts *characters* rather than display width — CJK text (2 columns per
/// char) can overflow the row.
#[tokio::test(start_paused = true)]
async fn thread_rows_with_cjk_overflow() {
    let h = TuiHarness::spawn_reflowing(80, 14).await;

    h.display(Evt::UserInput("translate things".to_owned()));
    h.display(Evt::StartOutput);
    h.display_for_thread("child-cjk", Evt::StartOutput);
    h.display_for_thread(
        "child-cjk",
        Evt::ThinkingChunk {
            chunk: "这是一个很长的中文句子用来测试宽字符在线程行中的显示效果应该会溢出".repeat(2),
        },
    );
    h.settle().await;
    // BUG: wrap_tail counts characters, not display columns — the 2-column
    // CJK tail overflows the row and the freshest text (what the user most
    // wants to see) is clipped off the right edge.
    insta::assert_snapshot!("thread_row_cjk", h.screen_with_scrollback());
}

/// Thinking text next to the spinner goes through the same char-counted
/// `wrap_tail`; emoji are 2 columns wide.
#[tokio::test(start_paused = true)]
async fn thinking_text_with_emoji() {
    let h = TuiHarness::spawn_reflowing(80, 12).await;

    h.display(Evt::UserInput("react".to_owned()));
    h.display(Evt::StartOutput);
    h.display(Evt::ThinkingStart);
    h.display(Evt::ThinkingChunk {
        chunk: "🚀🚀🚀 thinking with emoji 🎉🎉🎉 ".repeat(3),
    });
    h.settle().await;
    // BUG: same char-vs-width confusion as the thread rows — the emoji tail
    // is clipped at the right edge, hiding the most recent thinking text.
    insta::assert_snapshot!("thinking_emoji", h.screen_with_scrollback());
}

/// Streaming CJK/emoji text into scrollback: print_above wraps wide chars
/// at the right margin; the saved cursor must stay consistent.
#[tokio::test(start_paused = true)]
async fn streaming_cjk_text_wraps() {
    let h = TuiHarness::spawn_reflowing(40, 12).await;

    h.display(Evt::UserInput("写一首诗".to_owned()));
    h.display(Evt::StartOutput);
    h.display(Evt::TextChunk {
        chunk: "春眠不觉晓，处处闻啼鸟。夜来风雨声，花落知多少。".repeat(2),
    });
    h.display(Evt::ResponseDone(None));
    h.settle().await;
    insta::assert_snapshot!("streaming_cjk", h.screen_with_scrollback());
}

/// Typing emoji into the input box, with the cursor placed after them —
/// cursor column math vs display width.
#[tokio::test(start_paused = true)]
async fn emoji_in_input_box() {
    let h = TuiHarness::spawn_reflowing(80, 12).await;

    h.type_str("deploy 🚀 the 🎉 rockets");
    h.settle().await;
    // BUG: the space typed after each emoji is swallowed
    // ("deploy 🚀the 🎉rockets") — input cursor/insert math treats the
    // 2-column emoji as 1 column.
    insta::assert_snapshot!("emoji_input", h.screen_with_scrollback());
}

/// Emoji in the model name unbalances the status row, whose padding is
/// computed from char counts rather than display width.
#[tokio::test(start_paused = true)]
async fn emoji_model_name_status_row() {
    let h = TuiHarness::spawn_with(HarnessOptions {
        cols: 80,
        rows: 12,
        model_name: "🚀 rocket-model 🚀".to_owned(),
        backend: common::Backend::Alacritty,
        ..HarnessOptions::default()
    })
    .await;
    // BUG: status-row padding is computed from char counts, so the 2-column
    // emoji push the right side off the edge ("0% context us").
    insta::assert_snapshot!("emoji_status_row", h.screen_with_scrollback());
}
