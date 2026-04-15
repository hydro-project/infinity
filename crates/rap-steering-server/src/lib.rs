use std::collections::HashSet;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use tokio::fs;

/// Single-file steering locations (relative to project root).
const SINGLE_FILES: &[&str] = &[
    "INFINITY.md",
    "CLAUDE.md",
    ".claude/settings.json",
    "AGENTS.md",
    ".cursorrules",
    ".github/copilot-instructions.md",
    ".windsurfrules",
    "CONVENTIONS.md",
];

/// Directory steering locations — all files recursively.
const DIRECTORIES: &[&str] = &[".kiro/steering", ".cursor/rules", ".ai/rules"];

#[derive(Debug, Deserialize)]
pub struct ListSteeringArgs {
    pub root: String,
}

#[derive(Debug, Deserialize)]
pub struct LoadSteeringArgs {
    pub root: String,
    pub path: String,
}

/// Recursively collect all files under `dir`, returning paths relative to `root`.
async fn collect_files_recursive(root: &Path, dir: &Path) -> Vec<PathBuf> {
    let mut result = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(current) = stack.pop() {
        let Ok(mut entries) = fs::read_dir(&current).await else {
            continue;
        };
        while let Ok(Some(entry)) = entries.next_entry().await {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if let Ok(rel) = path.strip_prefix(root) {
                result.push(rel.to_path_buf());
            }
        }
    }
    result
}

/// Collect steering candidates from a base directory, returning paths relative to that base.
async fn collect_candidates(base: &Path) -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    for f in SINGLE_FILES {
        candidates.push(PathBuf::from(f));
    }
    for dir in DIRECTORIES {
        let dir_path = base.join(dir);
        if dir_path.is_dir() {
            candidates.extend(collect_files_recursive(base, &dir_path).await);
        }
    }
    candidates
}

/// Discover all steering files under `root` and the user's home directory,
/// canonicalize to dedup symlinks, and return sorted relative paths.
/// Home directory files are prefixed with `~/`.
pub async fn list_steering_files(root: &Path) -> Result<Vec<String>, String> {
    let canon_root = fs::canonicalize(root)
        .await
        .map_err(|e| format!("cannot canonicalize root: {e}"))?;

    let mut seen_canonical = HashSet::new();
    let mut results = Vec::new();

    // Scan project root
    for rel in collect_candidates(root).await {
        let abs = root.join(&rel);
        if !abs.exists() {
            continue;
        }
        let Ok(canonical) = fs::canonicalize(&abs).await else {
            continue;
        };
        // Verify it's under root (safety check)
        if !canonical.starts_with(&canon_root) {
            continue;
        }
        if seen_canonical.insert(canonical) {
            results.push(rel.to_string_lossy().into_owned());
        }
    }

    // Scan home directory
    if let Some(home) = home_dir()
        && let Ok(canon_home) = fs::canonicalize(&home).await
        && canon_home != canon_root
    {
        for rel in collect_candidates(&home).await {
            let abs = home.join(&rel);
            if !abs.exists() {
                continue;
            }
            let Ok(canonical) = fs::canonicalize(&abs).await else {
                continue;
            };
            if !canonical.starts_with(&canon_home) {
                continue;
            }
            if seen_canonical.insert(canonical) {
                results.push(format!("~/{}", rel.to_string_lossy()));
            }
        }
    }

    results.sort();
    Ok(results)
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

/// Check that `rel` is under a known steering location.
fn is_known_steering_location(rel: &str) -> bool {
    let rel_path = Path::new(rel);
    SINGLE_FILES.contains(&rel) || DIRECTORIES.iter().any(|d| rel_path.starts_with(d))
}

/// Load a steering file, with path traversal prevention.
/// Paths prefixed with `~/` are resolved relative to the home directory.
pub async fn load_steering_file(root: &Path, rel_path: &str) -> Result<String, String> {
    let (base, rel) = if let Some(stripped) = rel_path.strip_prefix("~/") {
        let home = home_dir().ok_or_else(|| "HOME not set".to_owned())?;
        (home, stripped)
    } else {
        (root.to_path_buf(), rel_path)
    };

    if !is_known_steering_location(rel) {
        return Err("not a steering file location".to_owned());
    }

    let canon_base = fs::canonicalize(&base)
        .await
        .map_err(|e| format!("cannot canonicalize base: {e}"))?;

    let abs = base.join(rel);
    let canonical = fs::canonicalize(&abs)
        .await
        .map_err(|e| format!("file not found: {e}"))?;

    if !canonical.starts_with(&canon_base) {
        return Err("path traversal denied".to_owned());
    }

    fs::read_to_string(&canonical)
        .await
        .map_err(|e| format!("failed to read file: {e}"))
}
