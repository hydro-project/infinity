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
const DIRECTORIES: &[&str] = &[
    ".kiro/steering",
    ".cursor/rules",
    ".ai/rules",
    ".claude/skills",
];

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

/// Steering files that exist under `base` (canonicalized as `canon_base`), as
/// paths relative to `base` in [`collect_candidates`] order. Files resolving
/// outside `base` or already in `seen` are skipped.
async fn existing_steering_files(
    base: &Path,
    canon_base: &Path,
    seen: &mut HashSet<PathBuf>,
) -> Vec<String> {
    let mut files = Vec::new();
    for rel in collect_candidates(base).await {
        let abs = base.join(&rel);
        if !abs.exists() {
            continue;
        }
        let Ok(canonical) = fs::canonicalize(&abs).await else {
            continue;
        };
        // Verify it's under base (safety check)
        if canonical.starts_with(canon_base) && seen.insert(canonical) {
            files.push(rel.to_string_lossy().into_owned());
        }
    }
    files
}

/// Discover all steering files under `root` and the user's home directory,
/// canonicalize to dedup symlinks, and return sorted relative paths.
/// Home directory files are prefixed with `~/`.
pub async fn list_steering_files(root: &Path) -> Result<Vec<String>, String> {
    let canon_root = fs::canonicalize(root)
        .await
        .map_err(|e| format!("cannot canonicalize root: {e}"))?;

    let mut seen_canonical = HashSet::new();
    let mut results = existing_steering_files(root, &canon_root, &mut seen_canonical).await;

    if let Some(home) = home_dir()
        && let Ok(canon_home) = fs::canonicalize(&home).await
        && canon_home != canon_root
    {
        let home_files = existing_steering_files(&home, &canon_home, &mut seen_canonical).await;
        results.extend(home_files.into_iter().map(|rel| format!("~/{rel}")));
    }

    results.sort();
    Ok(results)
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

/// Size budget (bytes) for the files embedded by [`global_steering_prompt`].
const EMBED_BUDGET: usize = 16 * 1024;

/// System-prompt fragment embedding the user's global (home-directory)
/// steering files, or `None` if there are none. Structured config (`.json`,
/// may hold secrets) and skills (loaded on demand) are not embedded; files
/// that do not fit [`EMBED_BUDGET`] are listed for `load_steering` instead.
pub async fn global_steering_prompt() -> Option<String> {
    global_steering_prompt_in(&home_dir()?).await
}

async fn global_steering_prompt_in(home: &Path) -> Option<String> {
    let canon_home = fs::canonicalize(home)
        .await
        .inspect_err(|e| tracing::warn!("cannot canonicalize home {home:?}: {e}"))
        .ok()?;

    let mut embedded = String::new();
    let mut deferred = Vec::new();
    for rel in existing_steering_files(home, &canon_home, &mut HashSet::new()).await {
        if rel.ends_with(".json") || rel.starts_with(".claude/skills") {
            continue;
        }
        let Ok(content) = fs::read_to_string(home.join(&rel))
            .await
            .inspect_err(|e| tracing::warn!("failed to read ~/{rel}: {e}"))
        else {
            continue;
        };
        let content = content.trim();
        if content.is_empty() {
            continue;
        }
        if embedded.len() + content.len() > EMBED_BUDGET {
            deferred.push(format!("~/{rel}"));
        } else {
            embedded.push_str(&format!("\n### ~/{rel}\n\n{content}\n"));
        }
    }

    if embedded.is_empty() && deferred.is_empty() {
        return None;
    }
    let mut prompt = format!(
        "The user's global steering files (apply to ALL projects) are embedded below. \
         Follow them without being asked. Project-specific steering files are NOT \
         embedded; discover them with `list_steering`.\n{embedded}"
    );
    if !deferred.is_empty() {
        prompt.push_str(&format!(
            "\nThese global steering files did not fit and are not embedded; read them \
             with `load_steering`: {}\n",
            deferred.join(", ")
        ));
    }
    Some(prompt)
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

#[cfg(test)]
mod tests {
    use super::*;

    fn write(home: &Path, rel: &str, content: &str) {
        let path = home.join(rel);
        std::fs::create_dir_all(path.parent().expect("bug: rel has a parent"))
            .expect("create parent dir");
        std::fs::write(path, content).expect("write steering file");
    }

    #[tokio::test]
    async fn global_prompt_is_none_without_steering_files() {
        let home = tempfile::tempdir().expect("create fake home");
        assert_eq!(global_steering_prompt_in(home.path()).await, None);
    }

    #[tokio::test]
    async fn global_prompt_embeds_prose_but_not_config_or_skills() {
        let home = tempfile::tempdir().expect("create fake home");
        write(home.path(), "INFINITY.md", "Global rule: be kind.");
        write(
            home.path(),
            ".kiro/steering/security.md",
            "Never commit secrets.",
        );
        write(
            home.path(),
            ".claude/settings.json",
            r#"{"token": "SECRET"}"#,
        );
        write(home.path(), ".claude/skills/demo/SKILL.md", "Skill body.");

        let prompt = global_steering_prompt_in(home.path())
            .await
            .expect("globals exist");
        assert!(prompt.contains("### ~/INFINITY.md\n\nGlobal rule: be kind."));
        assert!(prompt.contains("### ~/.kiro/steering/security.md\n\nNever commit secrets."));
        assert!(!prompt.contains("SECRET"));
        assert!(!prompt.contains("Skill body."));
    }

    #[tokio::test]
    async fn global_prompt_lists_files_over_budget() {
        let home = tempfile::tempdir().expect("create fake home");
        write(home.path(), "INFINITY.md", "Global rule: be kind.");
        write(home.path(), "CLAUDE.md", &"x".repeat(EMBED_BUDGET));

        let prompt = global_steering_prompt_in(home.path())
            .await
            .expect("globals exist");
        assert!(prompt.contains("Global rule: be kind."));
        assert!(!prompt.contains("xxx"));
        assert!(prompt.contains("`load_steering`: ~/CLAUDE.md"));
    }
}
