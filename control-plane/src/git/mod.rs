use async_trait::async_trait;

use crate::error::Result;

/// Trait abstracting git operations on the bare repo.
#[async_trait]
pub trait GitOperations: Send + Sync + 'static {
    /// Check if a file exists in the repo at the given ref (default branch).
    async fn spec_exists(&self, spec_path: &str) -> Result<bool>;

    /// Get the current SHA of a branch.
    async fn get_branch_sha(&self, branch: &str) -> Result<Option<String>>;

    /// Create a new branch from the default branch HEAD.
    async fn create_branch(&self, branch: &str) -> Result<String>;

    /// Read a file's content from the repo at the given ref.
    async fn read_file(&self, path: &str, git_ref: &str) -> Result<String>;

    /// Fetch from the remote.
    async fn fetch(&self) -> Result<()>;

    /// Detect if a branch has diverged from the expected SHA.
    async fn has_diverged(&self, branch: &str, expected_sha: &str) -> Result<bool>;

    /// Write a file to the worktree for a branch and commit it.
    async fn write_file(&self, branch: &str, path: &str, content: &str) -> Result<()>;

    /// Delete a branch (cleanup on failure).
    async fn delete_branch(&self, branch: &str) -> Result<()>;

    /// Get the PR state for a branch (OPEN, MERGED, CLOSED). Returns None if no PR exists.
    async fn get_pr_state(&self, branch: &str) -> Option<String>;

    /// Remove a path from the branch (git rm) and commit. Used to clean up artifacts before PR.
    async fn remove_path(&self, branch: &str, path: &str) -> Result<()>;

    /// Check if CI checks have passed on a branch/PR. Returns true if all checks pass.
    async fn ci_passed(&self, branch: &str) -> Result<bool>;

    /// Create a pull request. Returns the PR URL.
    async fn create_pr(&self, branch: &str, title: &str, body: &str) -> Result<String>;

    /// Merge a pull request by branch name using the given strategy. Returns merge SHA.
    async fn merge_pr(&self, branch: &str, strategy: &str) -> Result<String>;
}

/// Real git operations on a bare repository.
pub mod bare {
    use super::*;
    use std::path::PathBuf;
    use tokio::process::Command;

    #[derive(Debug, Clone)]
    pub struct BareRepoGitOperations {
        repo_path: PathBuf,
    }

    impl BareRepoGitOperations {
        pub fn new(path: &str) -> Self {
            Self {
                repo_path: PathBuf::from(path),
            }
        }

        async fn run_git(&self, args: &[&str]) -> std::result::Result<String, crate::error::NemoError> {
            let output = Command::new("git")
                .args(args)
                .current_dir(&self.repo_path)
                .output()
                .await
                .map_err(|e| crate::error::NemoError::Git(format!("Failed to run git: {e}")))?;

            if output.status.success() {
                Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
            } else {
                let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
                Err(crate::error::NemoError::Git(stderr))
            }
        }
    }

    #[async_trait]
    impl GitOperations for BareRepoGitOperations {
        async fn spec_exists(&self, spec_path: &str) -> Result<bool> {
            match self.run_git(&["cat-file", "-e", &format!("HEAD:{spec_path}")]).await {
                Ok(_) => Ok(true),
                Err(_) => Ok(false),
            }
        }

        async fn get_branch_sha(&self, branch: &str) -> Result<Option<String>> {
            match self.run_git(&["rev-parse", branch]).await {
                Ok(sha) => Ok(Some(sha)),
                Err(_) => Ok(None),
            }
        }

        async fn create_branch(&self, branch: &str) -> Result<String> {
            // Use origin/main (the fetched remote tip) not bare-repo HEAD
            let base_ref = match self.run_git(&["rev-parse", "origin/main"]).await {
                Ok(sha) => sha,
                Err(_) => self.run_git(&["rev-parse", "HEAD"]).await
                    .map_err(|e| crate::error::NemoError::Git(format!("Failed to resolve base ref: {e}")))?,
            };

            // Try to create the branch
            match self.run_git(&["branch", branch, &base_ref]).await {
                Ok(_) => {}
                Err(_) => {
                    // Branch exists — check PR state before reusing
                    let pr_state = self.get_pr_state(branch).await;
                    match pr_state.as_deref() {
                        Some("OPEN") => {
                            // Open PR exists: refuse reuse, caller should not silently
                            // invalidate an active PR. Return error.
                            return Err(crate::error::NemoError::Git(format!(
                                "Branch {branch} has an open PR. Close or merge it before restarting."
                            )));
                        }
                        Some("MERGED") | Some("CLOSED") => {
                            // Old PR is done: delete branch and recreate fresh
                            let _ = self.run_git(&["branch", "-D", branch]).await;
                            self.run_git(&["branch", branch, &base_ref])
                                .await
                                .map_err(|e| crate::error::NemoError::Git(
                                    format!("Failed to recreate branch {branch}: {e}")
                                ))?;
                        }
                        _ => {
                            // No PR — safe to force-reset
                            self.run_git(&["branch", "-f", branch, &base_ref])
                                .await
                                .map_err(|e| crate::error::NemoError::Git(
                                    format!("Failed to reset existing branch {branch}: {e}")
                                ))?;
                        }
                    }
                }
            }

            Ok(base_ref)
        }

        async fn read_file(&self, path: &str, git_ref: &str) -> Result<String> {
            self.run_git(&["show", &format!("{git_ref}:{path}")]).await
        }

        async fn fetch(&self) -> Result<()> {
            self.run_git(&["fetch", "origin"]).await?;
            Ok(())
        }

        async fn has_diverged(&self, branch: &str, expected_sha: &str) -> Result<bool> {
            match self.get_branch_sha(branch).await? {
                Some(sha) => Ok(sha != expected_sha),
                None => Ok(false),
            }
        }

        async fn write_file(&self, branch: &str, path: &str, content: &str) -> Result<()> {
            // Create a temporary worktree, write the file, commit, and clean up
            let worktree_dir = format!("/tmp/nemo-wt-{}", uuid::Uuid::new_v4());
            self.run_git(&["worktree", "add", &worktree_dir, branch])
                .await
                .map_err(|e| crate::error::NemoError::Git(format!(
                    "Failed to create worktree for {branch}: {e}"
                )))?;

            // Write the file
            let file_path = std::path::Path::new(&worktree_dir).join(path);
            if let Some(parent) = file_path.parent() {
                tokio::fs::create_dir_all(parent).await.map_err(|e| {
                    crate::error::NemoError::Git(format!("Failed to create dirs: {e}"))
                })?;
            }
            tokio::fs::write(&file_path, content).await.map_err(|e| {
                crate::error::NemoError::Git(format!("Failed to write file: {e}"))
            })?;

            // Stage, commit, and clean up worktree
            let add = Command::new("git")
                .args(["add", path])
                .current_dir(&worktree_dir)
                .output()
                .await
                .map_err(|e| crate::error::NemoError::Git(format!("git add spawn failed: {e}")))?;
            if !add.status.success() {
                let stderr = String::from_utf8_lossy(&add.stderr).trim().to_string();
                let _ = self.run_git(&["worktree", "remove", "--force", &worktree_dir]).await;
                return Err(crate::error::NemoError::Git(format!("git add failed: {stderr}")));
            }

            let commit = Command::new("git")
                .args([
                    "-c", "user.name=nemo-control-plane",
                    "-c", "user.email=nemo@nemo.dev",
                    "commit", "-m", &format!("chore(agent): add {path}"),
                ])
                .current_dir(&worktree_dir)
                .output()
                .await
                .map_err(|e| crate::error::NemoError::Git(format!("git commit spawn failed: {e}")))?;
            if !commit.status.success() {
                let stderr = String::from_utf8_lossy(&commit.stderr).trim().to_string();
                let _ = self.run_git(&["worktree", "remove", "--force", &worktree_dir]).await;
                return Err(crate::error::NemoError::Git(format!("git commit failed: {stderr}")));
            }

            // Clean up worktree
            let _ = self.run_git(&["worktree", "remove", "--force", &worktree_dir]).await;
            Ok(())
        }

        async fn delete_branch(&self, branch: &str) -> Result<()> {
            let _ = self.run_git(&["branch", "-D", branch]).await;
            Ok(())
        }

        async fn get_pr_state(&self, branch: &str) -> Option<String> {
            let output = Command::new("gh")
                .args(["pr", "view", branch, "--json", "state", "--jq", ".state"])
                .current_dir(&self.repo_path)
                .output()
                .await
                .ok()?;

            if !output.status.success() {
                return None;
            }

            let state = String::from_utf8_lossy(&output.stdout).trim().to_uppercase();
            if state.is_empty() { None } else { Some(state) }
        }

        async fn remove_path(&self, branch: &str, path: &str) -> Result<()> {
            // Check if path exists in the branch before creating a worktree
            if self.run_git(&["cat-file", "-e", &format!("{branch}:{path}")]).await.is_err() {
                // Path doesn't exist in the branch — nothing to remove
                return Ok(());
            }

            let worktree_dir = format!("/tmp/nemo-wt-{}", uuid::Uuid::new_v4());
            self.run_git(&["worktree", "add", &worktree_dir, branch])
                .await
                .map_err(|e| crate::error::NemoError::Git(format!(
                    "Failed to create worktree for {branch}: {e}"
                )))?;

            // git rm -rf the path
            let rm = Command::new("git")
                .args(["rm", "-rf", path])
                .current_dir(&worktree_dir)
                .output()
                .await;

            if let Ok(ref output) = rm
                && output.status.success()
            {
                // Only commit if git rm actually staged changes
                let _ = Command::new("git")
                    .args([
                        "-c", "user.name=nemo-control-plane",
                        "-c", "user.email=nemo@nemo.dev",
                        "commit", "-m", &format!("chore(agent): remove {path} artifacts"),
                    ])
                    .current_dir(&worktree_dir)
                    .output()
                    .await;
            }

            let _ = self.run_git(&["worktree", "remove", "--force", &worktree_dir]).await;
            Ok(())
        }

        async fn ci_passed(&self, branch: &str) -> Result<bool> {
            let output = Command::new("gh")
                .args(["pr", "checks", branch, "--required"])
                .current_dir(&self.repo_path)
                .output()
                .await
                .map_err(|e| crate::error::NemoError::Git(format!("Failed to run gh: {e}")))?;

            // Exit code 0 means all required checks passed
            Ok(output.status.success())
        }

        async fn create_pr(&self, branch: &str, title: &str, body: &str) -> Result<String> {
            // Push branch to origin before creating PR
            self.run_git(&["push", "-u", "origin", branch])
                .await
                .map_err(|e| crate::error::NemoError::Git(format!(
                    "Failed to push {branch} to origin: {e}"
                )))?;

            let output = Command::new("gh")
                .args(["pr", "create", "--head", branch, "--title", title, "--body", body])
                .current_dir(&self.repo_path)
                .output()
                .await
                .map_err(|e| crate::error::NemoError::Git(format!("Failed to run gh: {e}")))?;

            if output.status.success() {
                Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
            } else {
                let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
                Err(crate::error::NemoError::Git(format!(
                    "Failed to create PR for {branch}: {stderr}"
                )))
            }
        }

        async fn merge_pr(&self, branch: &str, strategy: &str) -> Result<String> {
            let merge_flag = match strategy {
                "rebase" => "--rebase",
                "merge" => "--merge",
                _ => "--squash",
            };

            // No --auto: block until merge completes so state and merge_sha are accurate
            let output = Command::new("gh")
                .args(["pr", "merge", branch, merge_flag, "--delete-branch"])
                .current_dir(&self.repo_path)
                .output()
                .await
                .map_err(|e| crate::error::NemoError::Git(format!("Failed to run gh: {e}")))?;

            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
                return Err(crate::error::NemoError::Git(format!(
                    "Failed to merge PR for {branch}: {stderr}"
                )));
            }

            // Fetch to get the merge commit, then read the target branch SHA
            let _ = self.run_git(&["fetch", "origin"]).await;
            let sha = match self.run_git(&["rev-parse", "origin/main"]).await {
                Ok(s) => s,
                Err(_) => self.run_git(&["rev-parse", "HEAD"]).await?,
            };
            Ok(sha)
        }
    }
}

/// In-memory mock for testing.
pub mod mock {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Arc;
    use tokio::sync::RwLock;

    #[derive(Debug, Clone)]
    pub struct MockGitOperations {
        files: Arc<RwLock<HashMap<String, String>>>,
        branches: Arc<RwLock<HashMap<String, String>>>,
        default_sha: String,
    }

    impl MockGitOperations {
        pub fn new() -> Self {
            Self {
                files: Arc::new(RwLock::new(HashMap::new())),
                branches: Arc::new(RwLock::new(HashMap::new())),
                default_sha: "0000000000000000000000000000000000000000".to_string(),
            }
        }

        /// Add a file to the mock repo.
        pub async fn add_file(&self, path: &str, content: &str) {
            let mut files = self.files.write().await;
            files.insert(path.to_string(), content.to_string());
        }

        /// Set a branch SHA.
        pub async fn set_branch_sha(&self, branch: &str, sha: &str) {
            let mut branches = self.branches.write().await;
            branches.insert(branch.to_string(), sha.to_string());
        }
    }

    impl Default for MockGitOperations {
        fn default() -> Self {
            Self::new()
        }
    }

    #[async_trait]
    impl GitOperations for MockGitOperations {
        async fn spec_exists(&self, spec_path: &str) -> Result<bool> {
            let files = self.files.read().await;
            Ok(files.contains_key(spec_path))
        }

        async fn get_branch_sha(&self, branch: &str) -> Result<Option<String>> {
            let branches = self.branches.read().await;
            Ok(branches.get(branch).cloned())
        }

        async fn create_branch(&self, branch: &str) -> Result<String> {
            let sha = self.default_sha.clone();
            let mut branches = self.branches.write().await;
            branches.insert(branch.to_string(), sha.clone());
            Ok(sha)
        }

        async fn read_file(&self, path: &str, _git_ref: &str) -> Result<String> {
            let files = self.files.read().await;
            files.get(path).cloned().ok_or_else(|| {
                crate::error::NemoError::Git(format!("File not found: {path}"))
            })
        }

        async fn fetch(&self) -> Result<()> {
            Ok(())
        }

        async fn has_diverged(&self, branch: &str, expected_sha: &str) -> Result<bool> {
            let branches = self.branches.read().await;
            match branches.get(branch) {
                Some(sha) => Ok(sha != expected_sha),
                None => Ok(false),
            }
        }

        async fn write_file(&self, _branch: &str, path: &str, content: &str) -> Result<()> {
            let mut files = self.files.write().await;
            files.insert(path.to_string(), content.to_string());
            Ok(())
        }

        async fn delete_branch(&self, branch: &str) -> Result<()> {
            let mut branches = self.branches.write().await;
            branches.remove(branch);
            Ok(())
        }

        async fn get_pr_state(&self, _branch: &str) -> Option<String> {
            None
        }

        async fn remove_path(&self, _branch: &str, path: &str) -> Result<()> {
            let mut files = self.files.write().await;
            files.retain(|k, _| !k.starts_with(path));
            Ok(())
        }

        async fn ci_passed(&self, _branch: &str) -> Result<bool> {
            Ok(true)
        }

        async fn create_pr(&self, branch: &str, _title: &str, _body: &str) -> Result<String> {
            Ok(format!("https://github.com/mock/repo/pull/{branch}"))
        }

        async fn merge_pr(&self, branch: &str, _strategy: &str) -> Result<String> {
            let branches = self.branches.read().await;
            Ok(branches
                .get(branch)
                .cloned()
                .unwrap_or_else(|| "merge-sha-mock".to_string()))
        }
    }
}
