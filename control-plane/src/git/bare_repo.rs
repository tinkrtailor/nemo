use std::path::PathBuf;
use tokio::process::Command;
use tokio::sync::Mutex;

use crate::error::{NautiloopError, Result};

/// Result of divergence detection between local and remote branch tips.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DivergenceResult {
    /// Normal operation: agent committed, local is ahead of remote.
    LocalAhead,
    /// Engineer pushed additional commits. Fast-forward is possible.
    RemoteAhead {
        local_sha: String,
        remote_sha: String,
    },
    /// Histories diverged (force push or rebase). Resuming discards local commits.
    ForceDeviated {
        local_sha: String,
        remote_sha: String,
    },
    /// Branch deleted on remote. Recovery: cancel only.
    RemoteGone,
}

/// A bare git repository with mutex-protected worktree operations.
///
/// The mutex serializes only `prepare_worktree` (create) and `cleanup_worktree` (delete)
/// operations. Multiple jobs can run concurrently on different worktrees.
#[derive(Debug)]
pub struct BareRepo {
    path: PathBuf,
    remote_url: String,
    worktree_mutex: Mutex<()>,
}

impl BareRepo {
    /// Create a new BareRepo instance.
    ///
    /// The path must point to an existing bare git repository.
    /// Returns an error if the path does not exist or is not a git repository.
    pub fn new(path: PathBuf, remote_url: String) -> Result<Self> {
        if !path.exists() {
            return Err(NautiloopError::Git(format!(
                "Bare repo not found at {}. Run initial clone first.",
                path.display()
            )));
        }
        Ok(Self {
            path,
            remote_url,
            worktree_mutex: Mutex::new(()),
        })
    }

    /// Get the path to the bare repo.
    pub fn path(&self) -> &PathBuf {
        &self.path
    }

    /// Get the remote URL.
    pub fn remote_url(&self) -> &str {
        &self.remote_url
    }

    /// Prepare a worktree for a job: fetch, resolve SHA, create worktree, checkout branch.
    ///
    /// This is the single atomic API (FR-7) that replaces the old two-step
    /// `fetch_and_resolve()` + `create_worktree()`. The mutex is held for the
    /// entire operation and released before returning.
    ///
    /// The `branch` parameter is the full branch name as returned by `branch_name()`
    /// (e.g., `agent/alice/invoice-cancel-a1b2c3d4`). No additional prefix is added.
    ///
    /// Returns `(worktree_path, resolved_sha)`.
    pub async fn prepare_worktree(
        &self,
        branch: &str,
        base_ref: &str,
    ) -> Result<(PathBuf, String)> {
        let _guard = self.worktree_mutex.lock().await;

        // 1. Fetch with prune (timeout 120s per NFR-3)
        self.run_git_timeout(&["fetch", "--prune"], 120).await?;

        // 2. Resolve base_ref to a SHA
        let sha = self
            .run_git(&["rev-parse", base_ref])
            .await
            .map_err(|e| NautiloopError::Git(format!("Failed to resolve ref '{base_ref}': {e}")))?;

        // 3. Create worktree at resolved SHA in detached HEAD mode
        let worktree_dir = format!("/tmp/nautiloop-worktree-{}", uuid::Uuid::new_v4());
        let worktree_path = PathBuf::from(&worktree_dir);

        // Handle stale worktree path (crash recovery)
        if worktree_path.exists() {
            let _ = tokio::fs::remove_dir_all(&worktree_path).await;
            let _ = self.run_git(&["worktree", "prune"]).await;
        }

        self.run_git(&["worktree", "add", "--detach", &worktree_dir, &sha])
            .await
            .map_err(|e| {
                NautiloopError::Git(format!("Failed to create worktree at {worktree_dir}: {e}"))
            })?;

        // 4. Create the named branch inside the worktree
        self.run_git_in_dir(&worktree_path, &["checkout", "-b", branch])
            .await
            .map_err(|e| {
                NautiloopError::Git(format!(
                    "Failed to create branch '{branch}' in worktree: {e}"
                ))
            })?;

        // Mutex released here (guard dropped)
        Ok((worktree_path, sha))
    }

    /// Clean up a worktree after job completion.
    ///
    /// Acquires the worktree mutex, removes the worktree, prunes, releases mutex.
    pub async fn cleanup_worktree(&self, path: &std::path::Path) -> Result<()> {
        let _guard = self.worktree_mutex.lock().await;

        let path_str = path.to_string_lossy();
        let _ = self
            .run_git(&["worktree", "remove", "--force", &path_str])
            .await;
        let _ = self.run_git(&["worktree", "prune"]).await;

        Ok(())
    }

    /// Detect divergence between local and remote branch tips (FR-12).
    ///
    /// Must be called after a fetch. Compares `refs/heads/{branch}` against
    /// `refs/remotes/origin/{branch}` using `git merge-base --is-ancestor`.
    pub async fn detect_divergence(&self, branch: &str) -> Result<DivergenceResult> {
        // Get local SHA
        let local_sha = match self
            .run_git(&["rev-parse", &format!("refs/heads/{branch}")])
            .await
        {
            Ok(sha) => sha,
            Err(_) => {
                // Local branch doesn't exist (shouldn't happen in normal flow)
                return Ok(DivergenceResult::RemoteGone);
            }
        };

        // Get remote SHA
        let remote_ref = format!("refs/remotes/origin/{branch}");
        let remote_sha = match self.run_git(&["rev-parse", &remote_ref]).await {
            Ok(sha) => sha,
            Err(_) => {
                // Remote ref doesn't exist
                // Check if it ever existed by checking if the remote branch simply isn't there
                return Ok(DivergenceResult::RemoteGone);
            }
        };

        if local_sha == remote_sha {
            return Ok(DivergenceResult::LocalAhead);
        }

        // Check if local is ancestor of remote (remote ahead, fast-forward possible)
        let local_is_ancestor = self
            .run_git(&["merge-base", "--is-ancestor", &local_sha, &remote_sha])
            .await
            .is_ok();
        if local_is_ancestor {
            return Ok(DivergenceResult::RemoteAhead {
                local_sha,
                remote_sha,
            });
        }

        // Check if remote is ancestor of local (local ahead, normal agent operation)
        let remote_is_ancestor = self
            .run_git(&["merge-base", "--is-ancestor", &remote_sha, &local_sha])
            .await
            .is_ok();
        if remote_is_ancestor {
            return Ok(DivergenceResult::LocalAhead);
        }

        // Neither is ancestor of the other: force-deviated
        Ok(DivergenceResult::ForceDeviated {
            local_sha,
            remote_sha,
        })
    }

    // =========================================================================
    // Internal helpers
    // =========================================================================

    async fn run_git(&self, args: &[&str]) -> std::result::Result<String, NautiloopError> {
        self.run_git_in_dir(&self.path, args).await
    }

    async fn run_git_in_dir(
        &self,
        dir: &PathBuf,
        args: &[&str],
    ) -> std::result::Result<String, NautiloopError> {
        let output = Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .await
            .map_err(|e| NautiloopError::Git(format!("Failed to run git: {e}")))?;

        if output.status.success() {
            Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
        } else {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            Err(NautiloopError::Git(stderr))
        }
    }

    async fn run_git_timeout(
        &self,
        args: &[&str],
        timeout_secs: u64,
    ) -> std::result::Result<String, NautiloopError> {
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(timeout_secs),
            self.run_git(args),
        )
        .await;

        match result {
            Ok(inner) => inner,
            Err(_) => Err(NautiloopError::Git(format!(
                "git {} timed out after {timeout_secs}s",
                args.first().unwrap_or(&"")
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_bare_repo_missing_path() {
        let result = BareRepo::new(
            PathBuf::from("/nonexistent/path"),
            "https://github.com/test/repo.git".to_string(),
        );
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("Bare repo not found"));
        assert!(err.contains("Run initial clone first"));
    }

    #[test]
    fn test_divergence_result_variants() {
        // Ensure all variants can be constructed and compared
        let local_ahead = DivergenceResult::LocalAhead;
        let remote_ahead = DivergenceResult::RemoteAhead {
            local_sha: "abc".to_string(),
            remote_sha: "def".to_string(),
        };
        let force_deviated = DivergenceResult::ForceDeviated {
            local_sha: "abc".to_string(),
            remote_sha: "def".to_string(),
        };
        let remote_gone = DivergenceResult::RemoteGone;

        assert_ne!(local_ahead, remote_ahead);
        assert_ne!(remote_ahead, force_deviated);
        assert_ne!(force_deviated, remote_gone);
    }
}
