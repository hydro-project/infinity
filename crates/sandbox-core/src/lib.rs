pub mod callback;
pub mod error;
pub mod git;
pub mod jj;
pub mod metadata;
pub mod sandbox;
pub mod server;
pub mod types;

pub const DEFAULT_SANDBOX_NAME: &str = "Infinity 🤖";
pub const DEFAULT_SANDBOX_EMAIL: &str = "infinity@hydro.run";

/// Walk up from `start` to find the repository root: the closest ancestor
/// (including `start` itself) containing a `.jj` directory or a `.git`
/// entry (directory, or file for worktrees/submodules).
///
/// This allows repositories to be detected even when the provided path is
/// nested inside the repo — important for jj repos, where subdirectories
/// carry no marker of their own (unlike git, where commands walk up
/// themselves).
pub fn find_repo_root(start: &std::path::Path) -> Option<std::path::PathBuf> {
    start
        .ancestors()
        .find(|dir| dir.join(".jj").is_dir() || dir.join(".git").exists())
        .map(std::path::Path::to_path_buf)
}
