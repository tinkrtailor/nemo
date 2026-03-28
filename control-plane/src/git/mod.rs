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

    /// Delete a branch (cleanup on failure).
    async fn delete_branch(&self, branch: &str) -> Result<()>;

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
            let head_sha = self
                .run_git(&["rev-parse", "HEAD"])
                .await
                .map_err(|e| crate::error::NemoError::Git(format!("Failed to get HEAD: {e}")))?;

            self.run_git(&["branch", branch, "HEAD"])
                .await
                .map_err(|e| crate::error::NemoError::Git(format!("Failed to create branch: {e}")))?;

            Ok(head_sha)
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

        async fn delete_branch(&self, branch: &str) -> Result<()> {
            let _ = self.run_git(&["branch", "-D", branch]).await;
            Ok(())
        }

        async fn create_pr(&self, branch: &str, title: &str, body: &str) -> Result<String> {
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

            let output = Command::new("gh")
                .args(["pr", "merge", branch, merge_flag, "--auto"])
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

            // Get the merge commit SHA from the target branch
            let sha = self.run_git(&["rev-parse", "HEAD"]).await?;
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

        async fn delete_branch(&self, branch: &str) -> Result<()> {
            let mut branches = self.branches.write().await;
            branches.remove(branch);
            Ok(())
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
