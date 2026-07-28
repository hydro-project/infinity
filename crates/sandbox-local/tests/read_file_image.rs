//! Tests for `read_file` returning structured image content for image files.

mod common;

use base64::Engine as _;
use common::{invoke_raw, jj_init_with_binary_file, start_test_server};
use rap_client::callback_server::start_callback_channel;
use rap_protocol::RapToolResultContent;

/// A 1x1 transparent PNG.
const PIXEL_PNG_BASE64: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==";

fn pixel_png_bytes() -> Vec<u8> {
    base64::engine::general_purpose::STANDARD
        .decode(PIXEL_PNG_BASE64)
        .expect("decode test png")
}

#[tokio::test]
async fn read_file_returns_image_content_for_png() {
    let png = pixel_png_bytes();
    let tmp = jj_init_with_binary_file("pixel.png", &png);
    let repo = tmp.path();
    let server_url = start_test_server(&repo.join(".test-metadata")).await;
    let (callback_url, mut rx) = start_callback_channel()
        .await
        .expect("start callback channel");
    let group = "read-image";
    let repo_str = repo.to_str().expect("repo path to str");

    invoke_raw(
        &server_url,
        &callback_url,
        group,
        "clone_repo",
        serde_json::json!({ "repo": repo_str }),
        &mut rx,
        None,
    )
    .await;

    let result = invoke_raw(
        &server_url,
        &callback_url,
        group,
        "read_file",
        serde_json::json!({ "path": "pixel.png" }),
        &mut rx,
        None,
    )
    .await;

    // Image reads emit structured `content` only — no top-level `text`.
    assert!(
        result.text.is_none(),
        "image reads should not set top-level text; got: {:?}",
        result.text
    );

    // The structured content carries a text item (describing the image)
    // followed by the image itself.
    let content = result.content.expect("image read should return content");
    assert_eq!(content.len(), 2);
    match &content[0] {
        RapToolResultContent::Text { text } => {
            assert!(
                text.contains("Read image file \"pixel.png\"") && text.contains("image/png"),
                "text content item should describe the image; got: {text}"
            );
        }
        other => panic!("expected text content first, got: {other:?}"),
    }
    match &content[1] {
        RapToolResultContent::Image { data, media_type } => {
            assert_eq!(media_type, "image/png");
            let decoded = base64::engine::general_purpose::STANDARD
                .decode(data)
                .expect("decode returned image data");
            assert_eq!(decoded, png, "image bytes should round-trip unchanged");
        }
        other => panic!("expected image content, got: {other:?}"),
    }

    // Display segments: image first (for clients that render images), then a
    // text summary fallback.
    let display = result.display_as.expect("display segments");
    assert_eq!(display.len(), 2);
    match &display[0] {
        rap_protocol::DisplaySegment::Image(img) => {
            assert_eq!(img.media_type, "image/png");
            let decoded = base64::engine::general_purpose::STANDARD
                .decode(&img.data)
                .expect("decode display image data");
            assert_eq!(decoded, png, "display image bytes should round-trip");
        }
        other => panic!("expected image display segment first, got: {other:?}"),
    }
    assert!(
        matches!(&display[1], rap_protocol::DisplaySegment::Text(t) if t.contains("image")),
        "got: {display:?}"
    );
}

#[tokio::test]
async fn read_file_detects_image_by_content_not_extension() {
    // A PNG whose filename has a non-image extension is still detected as an
    // image by its magic bytes (detection is content-based, not extension-based).
    let png = pixel_png_bytes();
    let tmp = jj_init_with_binary_file("mislabeled.txt", &png);
    let repo = tmp.path();
    let server_url = start_test_server(&repo.join(".test-metadata")).await;
    let (callback_url, mut rx) = start_callback_channel()
        .await
        .expect("start callback channel");
    let group = "read-image-mislabeled";
    let repo_str = repo.to_str().expect("repo path to str");

    invoke_raw(
        &server_url,
        &callback_url,
        group,
        "clone_repo",
        serde_json::json!({ "repo": repo_str }),
        &mut rx,
        None,
    )
    .await;

    let result = invoke_raw(
        &server_url,
        &callback_url,
        group,
        "read_file",
        serde_json::json!({ "path": "mislabeled.txt" }),
        &mut rx,
        None,
    )
    .await;

    // Detected as an image despite the `.txt` extension.
    assert!(result.text.is_none(), "got: {:?}", result.text);
    let content = result.content.expect("image read should return content");
    assert!(
        matches!(&content[0], RapToolResultContent::Text { text }
            if text.contains("image/png")),
        "got: {content:?}"
    );
    assert!(
        matches!(&content[1], RapToolResultContent::Image { media_type, .. } if media_type == "image/png"),
        "got: {content:?}"
    );
}

#[tokio::test]
async fn read_file_text_file_has_no_structured_content() {
    let tmp = jj_init_with_binary_file("notes.txt", b"hello world\n");
    let repo = tmp.path();
    let server_url = start_test_server(&repo.join(".test-metadata")).await;
    let (callback_url, mut rx) = start_callback_channel()
        .await
        .expect("start callback channel");
    let group = "read-text";
    let repo_str = repo.to_str().expect("repo path to str");

    invoke_raw(
        &server_url,
        &callback_url,
        group,
        "clone_repo",
        serde_json::json!({ "repo": repo_str }),
        &mut rx,
        None,
    )
    .await;

    let result = invoke_raw(
        &server_url,
        &callback_url,
        group,
        "read_file",
        serde_json::json!({ "path": "notes.txt" }),
        &mut rx,
        None,
    )
    .await;

    assert!(
        result
            .text
            .as_deref()
            .unwrap_or_default()
            .contains("hello world"),
        "got: {:?}",
        result.text
    );
    assert!(
        result.content.is_none(),
        "text reads should not set structured content, got: {:?}",
        result.content
    );
}
