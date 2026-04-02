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

/// Discover all steering files under `root`, canonicalize to dedup symlinks,
/// and return sorted relative paths.
pub async fn list_steering_files(root: &Path) -> Result<Vec<String>, String> {
    let canon_root = fs::canonicalize(root)
        .await
        .map_err(|e| format!("cannot canonicalize root: {e}"))?;

    let mut seen_canonical = HashSet::new();
    let mut results = Vec::new();

    // Collect candidate relative paths
    let mut candidates: Vec<PathBuf> = Vec::new();

    for f in SINGLE_FILES {
        candidates.push(PathBuf::from(f));
    }

    for dir in DIRECTORIES {
        let dir_path = root.join(dir);
        if dir_path.is_dir() {
            candidates.extend(collect_files_recursive(root, &dir_path).await);
        }
    }

    for rel in candidates {
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

    results.sort();
    Ok(results)
}

/// Load a steering file, with path traversal prevention.
pub async fn load_steering_file(root: &Path, rel_path: &str) -> Result<String, String> {
    let canon_root = fs::canonicalize(root)
        .await
        .map_err(|e| format!("cannot canonicalize root: {e}"))?;

    let abs = root.join(rel_path);
    let canonical = fs::canonicalize(&abs)
        .await
        .map_err(|e| format!("file not found: {e}"))?;

    if !canonical.starts_with(&canon_root) {
        return Err("path traversal denied".to_owned());
    }

    fs::read_to_string(&canonical)
        .await
        .map_err(|e| format!("failed to read file: {e}"))
}
