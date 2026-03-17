use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=scripts/install-bwrap.sh");

    if !cfg!(target_os = "linux") {
        return;
    }

    // Check if bwrap is already available
    if Command::new("bwrap")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
    {
        return;
    }

    println!("cargo:warning=bwrap not found, running install-bwrap.sh...");

    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    let script = std::path::Path::new(&manifest_dir).join("scripts/install-bwrap.sh");

    let status = Command::new("bash")
        .arg(&script)
        .status()
        .expect("failed to run install-bwrap.sh");

    if !status.success() {
        panic!("install-bwrap.sh failed with exit code: {}", status);
    }
}
