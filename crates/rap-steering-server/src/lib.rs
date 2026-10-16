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

/// Steering files discovered by [`list_steering_files`], split into
/// project-specific files and user-level (non-project) files.
#[derive(Debug, Default)]
pub struct SteeringFiles {
    /// Files found under the project root, as sorted relative paths.
    pub project: Vec<String>,
    /// Files found under the user's home directory, as sorted `~/`-prefixed paths.
    pub user: Vec<String>,
}

impl SteeringFiles {
    pub fn is_empty(&self) -> bool {
        self.project.is_empty() && self.user.is_empty()
    }

    /// Render the discovered files as tool-result text, suggesting that the
    /// agent read the non-project (user-level) steering files.
    pub fn render(&self) -> String {
        if self.is_empty() {
            return "No steering files found.".to_owned();
        }

        let mut sections = Vec::new();
        if !self.project.is_empty() {
            sections.push(format!(
                "Project steering files:\n{}",
                self.project.join("\n")
            ));
        }
        if !self.user.is_empty() {
            sections.push(format!(
                "User-level steering files (global preferences, not project-specific — \
                 usually already embedded in your system prompt; load any whose content \
                 you have not seen):\n{}",
                self.user.join("\n")
            ));
        }
        sections.join("\n\n")
    }
}

/// Discover all steering files under `root` and the user's home directory,
/// canonicalize to dedup symlinks, and return sorted relative paths.
/// Home directory files are prefixed with `~/`.
pub async fn list_steering_files(root: &Path) -> Result<SteeringFiles, String> {
    let canon_root = fs::canonicalize(root)
        .await
        .map_err(|e| format!("cannot canonicalize root: {e}"))?;

    let mut seen_canonical = HashSet::new();
    let mut results = SteeringFiles::default();

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
            results.project.push(rel.to_string_lossy().into_owned());
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
                results.user.push(format!("~/{}", rel.to_string_lossy()));
            }
        }
    }

    results.project.sort();
    results.user.sort();
    Ok(results)
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

/// Per-file budget (bytes, cut on a char boundary) for embedding a global
/// steering file into the system prompt.
pub const EMBED_PER_FILE_BUDGET: usize = 8 * 1024;
/// Total budget (bytes) for all embedded global steering files combined.
/// Kept below the daemon's per-server fragment budget (16 KiB) so the
/// fragment is never truncated mid-file by the runtime.
pub const EMBED_TOTAL_BUDGET: usize = 14 * 1024;

/// Whether a global steering file's *content* should be embedded into the
/// system prompt. Prose instruction files are embedded; structured config
/// (e.g. `settings.json`) is not — such files are not instructions and may
/// contain tokens/secrets. Non-embedded files remain discoverable via
/// `list_steering` / `load_steering`.
fn is_embeddable(rel: &str) -> bool {
    !rel.ends_with(".json")
}

/// Build the system-prompt fragment embedding the user's GLOBAL (home
/// directory) steering files. Project files are excluded: they depend on a
/// repository that may not be cloned yet, and remain on the
/// `list_steering`/`load_steering` flow.
pub async fn build_global_steering_prompt() -> Option<String> {
    build_global_steering_prompt_from(&home_dir()?).await
}

/// [`build_global_steering_prompt`] with an explicit home directory (for
/// tests).
///
/// Files are embedded in sorted path order, each delimited with its `~/`
/// path. [`EMBED_PER_FILE_BUDGET`] and [`EMBED_TOTAL_BUDGET`] bound the
/// fragment; truncated or skipped files get a marker telling the agent to
/// read the rest via `load_steering`. Returns `None` when there is nothing
/// to embed.
pub async fn build_global_steering_prompt_from(home: &Path) -> Option<String> {
    let canon_home = fs::canonicalize(home).await.ok()?;

    let mut seen_canonical = HashSet::new();
    let mut files: Vec<String> = Vec::new();
    for rel in collect_candidates(home).await {
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
            files.push(rel.to_string_lossy().into_owned());
        }
    }
    files.sort();

    let mut sections: Vec<String> = Vec::new();
    let mut used = 0usize;
    let mut deferred: Vec<String> = Vec::new();
    for rel in &files {
        if !is_embeddable(rel) {
            continue;
        }
        let Ok(content) = fs::read_to_string(home.join(rel)).await else {
            continue;
        };
        let content = content.trim();
        if content.is_empty() {
            continue;
        }
        if used >= EMBED_TOTAL_BUDGET {
            deferred.push(format!("~/{rel}"));
            continue;
        }
        let remaining = (EMBED_TOTAL_BUDGET - used).min(EMBED_PER_FILE_BUDGET);
        let (body, truncated) = if content.len() > remaining {
            let mut cut = remaining;
            while !content.is_char_boundary(cut) {
                cut -= 1;
            }
            (&content[..cut], true)
        } else {
            (content, false)
        };
        used += body.len();
        let mut section = format!("### ~/{rel}\n\n{body}");
        if truncated {
            section.push_str(&format!(
                "\n\n[... truncated — read the rest with `load_steering` (path: \"~/{rel}\")]"
            ));
        }
        sections.push(section);
    }

    if sections.is_empty() {
        return None;
    }

    let mut prompt = String::from(
        "The user's global steering files (apply to ALL projects) are embedded \
         below. Follow them without being asked. Project-specific steering files \
         are NOT embedded — discover them with `list_steering` after cloning.\n",
    );
    for section in sections {
        prompt.push('\n');
        prompt.push_str(&section);
        prompt.push('\n');
    }
    if !deferred.is_empty() {
        prompt.push_str(&format!(
            "\nAdditional global steering files did not fit the embedding budget — \
             read them with `load_steering`: {}\n",
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

    #[test]
    fn render_empty() {
        assert_eq!(
            SteeringFiles::default().render(),
            "No steering files found."
        );
    }

    #[test]
    fn render_project_only() {
        let files = SteeringFiles {
            project: vec!["AGENTS.md".to_owned()],
            user: vec![],
        };
        assert_eq!(files.render(), "Project steering files:\nAGENTS.md");
    }

    #[test]
    fn render_suggests_reading_user_files() {
        let files = SteeringFiles {
            project: vec!["AGENTS.md".to_owned()],
            user: vec!["~/INFINITY.md".to_owned()],
        };
        let rendered = files.render();
        assert!(rendered.contains("Project steering files:\nAGENTS.md"));
        assert!(rendered.contains("not project-specific"));
        assert!(rendered.contains("load any whose content"));
        assert!(rendered.contains("~/INFINITY.md"));
    }

    #[tokio::test]
    async fn global_prompt_embeds_only_global_prose_files() {
        let home = tempfile::tempdir().expect("create fake home");
        std::fs::write(home.path().join("INFINITY.md"), "Global rule: be kind.")
            .expect("write INFINITY.md");
        std::fs::create_dir_all(home.path().join(".kiro/steering")).expect("mkdir steering");
        std::fs::write(
            home.path().join(".kiro/steering/security.md"),
            "Never commit secrets.",
        )
        .expect("write steering file");
        std::fs::create_dir_all(home.path().join(".claude")).expect("mkdir .claude");
        std::fs::write(
            home.path().join(".claude/settings.json"),
            r#"{"token": "SECRET"}"#,
        )
        .expect("write settings.json");

        let prompt = build_global_steering_prompt_from(home.path())
            .await
            .expect("globals exist, prompt builds");
        assert!(prompt.contains("### ~/INFINITY.md"));
        assert!(prompt.contains("Global rule: be kind."));
        assert!(prompt.contains("### ~/.kiro/steering/security.md"));
        assert!(prompt.contains("Never commit secrets."));
        // Structured config is never embedded (may hold secrets).
        assert!(!prompt.contains("settings.json"));
        assert!(!prompt.contains("SECRET"));
        // Project discovery guidance is present.
        assert!(prompt.contains("list_steering"));
    }

    #[tokio::test]
    async fn global_prompt_is_none_without_steering_files() {
        let home = tempfile::tempdir().expect("create fake home");
        assert_eq!(build_global_steering_prompt_from(home.path()).await, None);
    }

    #[tokio::test]
    async fn global_prompt_applies_budgets_with_markers() {
        let home = tempfile::tempdir().expect("create fake home");
        // One file over the per-file budget...
        std::fs::write(
            home.path().join("INFINITY.md"),
            "x".repeat(EMBED_PER_FILE_BUDGET + 100),
        )
        .expect("write big file");
        // ...and enough others to blow the total budget.
        std::fs::create_dir_all(home.path().join(".kiro/steering")).expect("mkdir steering");
        for i in 0..4 {
            std::fs::write(
                home.path().join(format!(".kiro/steering/f{i}.md")),
                "y".repeat(4 * 1024),
            )
            .expect("write filler");
        }

        let prompt = build_global_steering_prompt_from(home.path())
            .await
            .expect("prompt builds");
        assert!(
            prompt.contains("[... truncated — read the rest with `load_steering`"),
            "per-file truncation marker missing"
        );
        assert!(
            prompt.contains("did not fit the embedding budget"),
            "total-budget deferral note missing"
        );
        // The fragment stays under the daemon's 16 KiB per-server budget.
        assert!(
            prompt.len() < 16 * 1024,
            "fragment too large: {}",
            prompt.len()
        );
    }
}
