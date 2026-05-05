use std::path::Path;

fn main() {
    let ui_dir = Path::new("../../infinity-ui");
    let web_dir = Path::new("../../infinity-web");
    println!("cargo:rerun-if-changed={}", ui_dir.join("src").display());
    println!("cargo:rerun-if-changed={}", web_dir.join("src").display());
    println!(
        "cargo:rerun-if-changed={}",
        web_dir.join("index.html").display()
    );
    println!(
        "cargo:rerun-if-changed={}",
        ui_dir.join("package.json").display()
    );
    println!(
        "cargo:rerun-if-changed={}",
        web_dir.join("package.json").display()
    );
    println!(
        "cargo:rerun-if-changed={}",
        ui_dir.join("package-lock.json").display()
    );
    println!(
        "cargo:rerun-if-changed={}",
        web_dir.join("package-lock.json").display()
    );
    println!(
        "cargo:rerun-if-changed={}",
        ui_dir.join("tsconfig.json").display()
    );
    println!(
        "cargo:rerun-if-changed={}",
        web_dir.join("tsconfig.json").display()
    );
    println!(
        "cargo:rerun-if-changed={}",
        web_dir.join("vite.config.ts").display()
    );

    if std::env::var("CARGO_FEATURE_BUNDLED_WEB").is_err() {
        return;
    }

    let npm = which_npm();
    let status_ui = std::process::Command::new(&npm)
        .args(["ci", "--prefix"])
        .arg(ui_dir)
        .status()
        .unwrap_or_else(|e| panic!("failed to run npm ci: {e}"));
    assert!(status_ui.success(), "npm ci failed (ui)");
    let status_web = std::process::Command::new(&npm)
        .args(["ci", "--prefix"])
        .arg(web_dir)
        .status()
        .unwrap_or_else(|e| panic!("failed to run npm ci: {e}"));
    assert!(status_web.success(), "npm ci failed (web)");

    let status = std::process::Command::new(&npm)
        .args(["run", "build", "--prefix"])
        .arg(web_dir)
        .status()
        .unwrap_or_else(|e| panic!("failed to run npm run build: {e}"));
    assert!(status.success(), "npm run build failed");
}

fn which_npm() -> String {
    // Prefer the npm found on PATH
    for name in ["npm"] {
        if std::process::Command::new(name)
            .arg("--version")
            .output()
            .is_ok()
        {
            return name.to_owned();
        }
    }
    panic!("bundled-web feature requires npm to be installed and on PATH");
}
