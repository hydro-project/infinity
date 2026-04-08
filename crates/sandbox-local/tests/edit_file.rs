mod common;

use common::{invoke, jj_init_with_file, start_test_server};
use rap_client::callback_server::start_callback_channel;

#[tokio::test]
async fn edit_file_exact_match() {
    let tmp = jj_init_with_file("main.rs", "fn main() {\n    println!(\"hello\");\n}\n");
    let repo = tmp.path();
    let server_url = start_test_server(&repo.join(".test-metadata")).await;
    let (callback_url, mut rx) = start_callback_channel()
        .await
        .expect("start callback channel");
    let group = "exact-match";
    let repo_str = repo.to_str().expect("repo path to str");

    invoke(
        &server_url,
        &callback_url,
        group,
        "clone_repo",
        serde_json::json!({ "repo": repo_str }),
        &mut rx,
        None,
    )
    .await;

    let text = invoke(
        &server_url,
        &callback_url,
        group,
        "edit_file",
        serde_json::json!({
            "path": "main.rs",
            "old_str": "    println!(\"hello\");",
            "new_str": "    println!(\"world\");"
        }),
        &mut rx,
        None,
    )
    .await;
    assert!(text.contains("Replaced"), "got: {text}");

    let text = invoke(
        &server_url,
        &callback_url,
        group,
        "read_file",
        serde_json::json!({ "path": "main.rs" }),
        &mut rx,
        None,
    )
    .await;
    assert!(text.contains("println!(\"world\")"), "got: {text}");
}

#[tokio::test]
async fn edit_file_whitespace_fallback() {
    // File uses 4-space indent, old_str uses mixed/wrong indentation
    let file_content = concat!(
        "fn main() {\n",
        "    let x = 1;\n",
        "    let y = 2;\n",
        "    println!(\"{}\", x + y);\n",
        "}\n",
    );
    let tmp = jj_init_with_file("main.rs", file_content);
    let repo = tmp.path();
    let server_url = start_test_server(&repo.join(".test-metadata")).await;
    let (callback_url, mut rx) = start_callback_channel()
        .await
        .expect("start callback channel");
    let group = "ws-fallback";
    let repo_str = repo.to_str().expect("repo path to str");

    invoke(
        &server_url,
        &callback_url,
        group,
        "clone_repo",
        serde_json::json!({ "repo": repo_str }),
        &mut rx,
        None,
    )
    .await;

    // old_str has 2-space indent instead of 4-space; spans multiple lines
    let text = invoke(
        &server_url,
        &callback_url,
        group,
        "edit_file",
        serde_json::json!({
            "path": "main.rs",
            "old_str": "  let x = 1;\n  let y = 2;\n  println!(\"{}\", x + y);",
            "new_str": "    let x = 10;\n    let y = 20;\n    println!(\"{}\", x + y);"
        }),
        &mut rx,
        None,
    )
    .await;
    assert!(text.contains("Replaced"), "got: {text}");

    let text = invoke(
        &server_url,
        &callback_url,
        group,
        "read_file",
        serde_json::json!({ "path": "main.rs" }),
        &mut rx,
        None,
    )
    .await;
    assert!(text.contains("let x = 10;"), "got: {text}");
    assert!(text.contains("let y = 20;"), "got: {text}");
}

#[tokio::test]
async fn edit_file_whitespace_fallback_ambiguous() {
    // Two identical multiline blocks that match when trimmed — should fail
    let file_content = concat!(
        "fn a() {\n",
        "    let x = 1;\n",
        "    let y = 2;\n",
        "}\n",
        "fn b() {\n",
        "    let x = 1;\n",
        "    let y = 2;\n",
        "}\n",
    );
    let tmp = jj_init_with_file("dup.rs", file_content);
    let repo = tmp.path();
    let server_url = start_test_server(&repo.join(".test-metadata")).await;
    let (callback_url, mut rx) = start_callback_channel()
        .await
        .expect("start callback channel");
    let group = "ws-ambiguous";
    let repo_str = repo.to_str().expect("repo path to str");

    invoke(
        &server_url,
        &callback_url,
        group,
        "clone_repo",
        serde_json::json!({ "repo": repo_str }),
        &mut rx,
        None,
    )
    .await;

    let text = invoke(
        &server_url,
        &callback_url,
        group,
        "edit_file",
        serde_json::json!({
            "path": "dup.rs",
            "old_str": "  let x = 1;\n  let y = 2;",
            "new_str": "    let x = 99;\n    let y = 99;"
        }),
        &mut rx,
        None,
    )
    .await;
    assert!(text.contains("matches multiple locations"), "got: {text}");
}

#[tokio::test]
async fn edit_file_no_match() {
    let tmp = jj_init_with_file("f.txt", "aaa\nbbb\n");
    let repo = tmp.path();
    let server_url = start_test_server(&repo.join(".test-metadata")).await;
    let (callback_url, mut rx) = start_callback_channel()
        .await
        .expect("start callback channel");
    let group = "no-match";
    let repo_str = repo.to_str().expect("repo path to str");

    invoke(
        &server_url,
        &callback_url,
        group,
        "clone_repo",
        serde_json::json!({ "repo": repo_str }),
        &mut rx,
        None,
    )
    .await;

    let text = invoke(
        &server_url,
        &callback_url,
        group,
        "edit_file",
        serde_json::json!({
            "path": "f.txt",
            "old_str": "zzz",
            "new_str": "yyy"
        }),
        &mut rx,
        None,
    )
    .await;
    assert!(text.contains("not found"), "got: {text}");
}
