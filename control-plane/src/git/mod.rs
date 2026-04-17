pub mod bare_repo;
pub mod branch;

use async_trait::async_trait;

use crate::error::Result;

/// Trait abstracting git operations on the bare repo.
#[async_trait]
pub trait GitOperations: Send + Sync + 'static {
    /// Check if a file exists in the repo at the given ref (default branch).
    async fn spec_exists(&self, spec_path: &str) -> Result<bool>;

    /// Get the current SHA of a branch.
    async fn get_branch_sha(&self, branch: &str) -> Result<Option<String>>;

    /// Create a new branch from the given base ref (e.g., "origin/main").
    async fn create_branch(&self, branch: &str, base_remote_ref: &str) -> Result<String>;

    /// Read a file's content from the repo at the given ref.
    async fn read_file(&self, path: &str, git_ref: &str) -> Result<String>;

    /// Fetch from the remote.
    async fn fetch(&self) -> Result<()>;

    /// Detect if a branch has diverged from the expected SHA.
    async fn has_diverged(&self, branch: &str, expected_sha: &str) -> Result<bool>;

    /// Write a file to the worktree for a branch and commit it.
    async fn write_file(&self, branch: &str, path: &str, content: &str) -> Result<()>;

    /// Write a file to the worktree for a branch and commit it with custom author and message.
    /// Used for spec commits that must be attributed to the engineer (FR-3d).
    async fn write_file_as(
        &self,
        branch: &str,
        path: &str,
        content: &str,
        author_name: &str,
        author_email: &str,
        commit_message: &str,
    ) -> Result<()>;

    /// Delete a branch (cleanup on failure).
    async fn delete_branch(&self, branch: &str) -> Result<()>;

    /// Get the PR state for a branch (OPEN, MERGED, CLOSED). Returns None if no PR exists.
    async fn get_pr_state(&self, branch: &str) -> Option<String>;

    /// Remove a path from the branch (git rm) and commit. Used to clean up artifacts before PR.
    async fn remove_path(&self, branch: &str, path: &str) -> Result<()>;

    /// Check CI status. Returns Ok(Some(true)) if passed, Ok(Some(false)) if failed,
    /// Ok(None) if still pending.
    async fn ci_status(&self, branch: &str) -> Result<Option<bool>>;

    /// Create a pull request targeting `base_branch`. Returns the PR URL.
    async fn create_pr(
        &self,
        branch: &str,
        title: &str,
        body: &str,
        base_branch: &str,
    ) -> Result<String>;

    /// Merge a pull request by branch name using the given strategy. Returns merge SHA.
    /// `default_branch` is the target branch name (e.g., "main").
    async fn merge_pr(&self, branch: &str, strategy: &str, default_branch: &str) -> Result<String>;

    /// Ensure a persistent worktree exists for a branch at the given sub-path.
    /// Creates the worktree if it doesn't exist. Used before job dispatch so the
    /// agent pod can mount the worktree via subPath.
    async fn ensure_worktree(&self, branch: &str, worktree_path: &str) -> Result<()>;

    /// List files changed on a branch relative to the default branch.
    /// Used by the TEST stage to determine affected services (FR-42a).
    async fn changed_files(&self, branch: &str, default_branch: &str) -> Result<Vec<String>>;

    /// Push a branch to the remote. Used after the initial spec commit so the
    /// agent branch exists on the remote before returning 201.
    async fn push_branch(&self, branch: &str) -> Result<()>;
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

        async fn run_git(
            &self,
            args: &[&str],
        ) -> std::result::Result<String, crate::error::NautiloopError> {
            let output = Command::new("git")
                .args(args)
                .current_dir(&self.repo_path)
                .output()
                .await
                .map_err(|e| {
                    crate::error::NautiloopError::Git(format!("Failed to run git: {e}"))
                })?;

            if output.status.success() {
                Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
            } else {
                let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
                Err(crate::error::NautiloopError::Git(stderr))
            }
        }
    }

    impl BareRepoGitOperations {
        /// Compute the persistent worktree directory for a branch.
        /// Matches the path derived in loop_engine/driver.rs build_context().
        /// Uses "wt/" prefix to avoid colliding with git's internal worktrees/ metadata.
        fn persistent_worktree_dir(&self, branch: &str) -> PathBuf {
            let worktree_name = branch.replace('/', "-");
            self.repo_path.join("wt").join(worktree_name)
        }

        /// Remove a persistent worktree for a branch if it exists.
        /// Must be called before `git branch -D` to avoid "branch is checked out" errors.
        async fn cleanup_stale_worktree(&self, branch: &str) {
            let wt_dir = self.persistent_worktree_dir(branch);
            if wt_dir.exists() {
                let _ = self
                    .run_git(&["worktree", "remove", "--force", &wt_dir.to_string_lossy()])
                    .await;
            }
        }

        /// Inner write_file logic; caller handles worktree cleanup.
        async fn write_file_in_worktree(
            &self,
            worktree_dir: &str,
            path: &str,
            content: &str,
            author_name: &str,
            author_email: &str,
            commit_message: &str,
        ) -> Result<()> {
            // Reject control characters (newlines, NUL, etc.) in author fields to prevent
            // git config injection or malformed commits.
            fn has_control_chars(s: &str) -> bool {
                s.bytes().any(|b| b < 0x20 || b == 0x7f)
            }
            if has_control_chars(author_name) {
                return Err(crate::error::NautiloopError::Git(
                    "author_name contains control characters".to_string(),
                ));
            }
            if has_control_chars(author_email) {
                return Err(crate::error::NautiloopError::Git(
                    "author_email contains control characters".to_string(),
                ));
            }

            let file_path = std::path::Path::new(worktree_dir).join(path);
            if let Some(parent) = file_path.parent() {
                tokio::fs::create_dir_all(parent).await.map_err(|e| {
                    crate::error::NautiloopError::Git(format!("Failed to create dirs: {e}"))
                })?;
            }
            tokio::fs::write(&file_path, content).await.map_err(|e| {
                crate::error::NautiloopError::Git(format!("Failed to write file: {e}"))
            })?;

            let add = Command::new("git")
                .args(["add", path])
                .current_dir(worktree_dir)
                .output()
                .await
                .map_err(|e| {
                    crate::error::NautiloopError::Git(format!("git add spawn failed: {e}"))
                })?;
            if !add.status.success() {
                let stderr = String::from_utf8_lossy(&add.stderr).trim().to_string();
                return Err(crate::error::NautiloopError::Git(format!(
                    "git add failed: {stderr}"
                )));
            }

            let user_name_arg = format!("user.name={author_name}");
            let user_email_arg = format!("user.email={author_email}");
            let commit = Command::new("git")
                .args([
                    "-c",
                    &user_name_arg,
                    "-c",
                    &user_email_arg,
                    "commit",
                    "-m",
                    commit_message,
                ])
                .current_dir(worktree_dir)
                .output()
                .await
                .map_err(|e| {
                    crate::error::NautiloopError::Git(format!("git commit spawn failed: {e}"))
                })?;
            if !commit.status.success() {
                let stderr = String::from_utf8_lossy(&commit.stderr).trim().to_string();
                return Err(crate::error::NautiloopError::Git(format!(
                    "git commit failed: {stderr}"
                )));
            }

            Ok(())
        }
    }

    #[async_trait]
    impl GitOperations for BareRepoGitOperations {
        async fn spec_exists(&self, spec_path: &str) -> Result<bool> {
            match self
                .run_git(&["cat-file", "-e", &format!("HEAD:{spec_path}")])
                .await
            {
                Ok(_) => Ok(true),
                Err(e) => {
                    let msg = e.to_string();
                    // "fatal: Not a valid object name" / empty stderr = not found
                    if msg.contains("Not a valid object")
                        || msg.is_empty()
                        || msg.contains("does not exist")
                    {
                        Ok(false)
                    } else {
                        // Real git error (corruption, permission, etc.)
                        Err(e)
                    }
                }
            }
        }

        async fn get_branch_sha(&self, branch: &str) -> Result<Option<String>> {
            match self.run_git(&["rev-parse", branch]).await {
                Ok(sha) => Ok(Some(sha)),
                Err(e) => {
                    let msg = e.to_string();
                    // "unknown revision" = branch doesn't exist
                    if msg.contains("unknown revision") || msg.contains("bad revision") {
                        Ok(None)
                    } else {
                        Err(e)
                    }
                }
            }
        }

        async fn create_branch(&self, branch: &str, base_remote_ref: &str) -> Result<String> {
            // Use the configured remote ref (e.g., origin/main) not bare-repo HEAD
            let base_ref = match self.run_git(&["rev-parse", base_remote_ref]).await {
                Ok(sha) => sha,
                Err(_) => self.run_git(&["rev-parse", "HEAD"]).await.map_err(|e| {
                    crate::error::NautiloopError::Git(format!("Failed to resolve base ref: {e}"))
                })?,
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
                            return Err(crate::error::NautiloopError::Git(format!(
                                "Branch {branch} has an open PR. Close or merge it before restarting."
                            )));
                        }
                        Some("MERGED") | Some("CLOSED") => {
                            // Old PR is done: clean up worktree + branch, recreate fresh
                            self.cleanup_stale_worktree(branch).await;
                            let _ = self.run_git(&["branch", "-D", branch]).await;
                            let _ = self.run_git(&["push", "origin", "--delete", branch]).await;
                            self.run_git(&["branch", branch, &base_ref])
                                .await
                                .map_err(|e| {
                                    crate::error::NautiloopError::Git(format!(
                                        "Failed to recreate branch {branch}: {e}"
                                    ))
                                })?;
                        }
                        Some("UNKNOWN") => {
                            // Transient gh failure — don't delete, reuse as-is
                            tracing::info!(branch, "PR state unknown (transient), reusing branch");
                        }
                        None => {
                            // No PR — stale leftover. Clean up worktree + branch, recreate.
                            self.cleanup_stale_worktree(branch).await;
                            let _ = self.run_git(&["branch", "-D", branch]).await;
                            let _ = self.run_git(&["push", "origin", "--delete", branch]).await;
                            self.run_git(&["branch", branch, &base_ref])
                                .await
                                .map_err(|e| {
                                    crate::error::NautiloopError::Git(format!(
                                        "Failed to recreate branch {branch}: {e}"
                                    ))
                                })?;
                        }
                        _ => unreachable!(),
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
            // Fetch first so we compare against the actual remote state,
            // not stale local tracking refs.
            let _ = self.run_git(&["fetch", "--prune", "origin"]).await;

            // Check the remote branch tip, not the local branch.
            // The local branch may be stale if the engineer pushed remotely.
            let remote_ref = format!("origin/{branch}");
            let tip = match self.get_branch_sha(&remote_ref).await? {
                Some(sha) => sha,
                // No remote tracking ref — branch not pushed yet or deleted
                None => return Ok(false),
            };

            // If remote tip equals expected, no divergence
            if tip == expected_sha {
                return Ok(false);
            }

            // `merge-base --is-ancestor A B` exits 0 if A is ancestor of B.
            match self
                .run_git(&["merge-base", "--is-ancestor", expected_sha, &tip])
                .await
            {
                Ok(_) => Ok(false), // expected is ancestor of remote tip -> not diverged
                Err(_) => Ok(true), // not an ancestor -> diverged (force push)
            }
        }

        async fn write_file(&self, branch: &str, path: &str, content: &str) -> Result<()> {
            self.write_file_as(
                branch,
                path,
                content,
                "nautiloop-control-plane",
                "nautiloop@nautiloop.dev",
                &format!("chore(agent): add {path}"),
            )
            .await
        }

        async fn write_file_as(
            &self,
            branch: &str,
            path: &str,
            content: &str,
            author_name: &str,
            author_email: &str,
            commit_message: &str,
        ) -> Result<()> {
            // Use the persistent worktree if it exists (created by ensure_worktree).
            // Git forbids the same branch in two worktrees, so we must not create
            // a second temporary worktree for a branch that already has one.
            let persistent = self.persistent_worktree_dir(branch);
            if persistent.exists() {
                return self
                    .write_file_in_worktree(
                        &persistent.to_string_lossy(),
                        path,
                        content,
                        author_name,
                        author_email,
                        commit_message,
                    )
                    .await;
            }

            // No persistent worktree — create a temporary one
            let worktree_dir = format!("/tmp/nautiloop-wt-{}", uuid::Uuid::new_v4());
            self.run_git(&["worktree", "add", &worktree_dir, branch])
                .await
                .map_err(|e| {
                    crate::error::NautiloopError::Git(format!(
                        "Failed to create worktree for {branch}: {e}"
                    ))
                })?;

            let result = self
                .write_file_in_worktree(
                    &worktree_dir,
                    path,
                    content,
                    author_name,
                    author_email,
                    commit_message,
                )
                .await;

            let _ = self
                .run_git(&["worktree", "remove", "--force", &worktree_dir])
                .await;

            result
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
                let stderr = String::from_utf8_lossy(&output.stderr);
                if stderr.contains("no pull requests found") {
                    // Definitive: no PR exists for this branch
                    return None;
                }
                // Transient failure (network, auth, rate limit) — return UNKNOWN
                // so callers don't treat this the same as "no PR exists".
                tracing::warn!(
                    branch = branch,
                    stderr = stderr.trim(),
                    "gh pr view failed (transient?), returning UNKNOWN"
                );
                return Some("UNKNOWN".to_string());
            }

            let state = String::from_utf8_lossy(&output.stdout)
                .trim()
                .to_uppercase();
            if state.is_empty() { None } else { Some(state) }
        }

        async fn remove_path(&self, branch: &str, path: &str) -> Result<()> {
            // Check if path exists in the branch before creating a worktree
            if self
                .run_git(&["cat-file", "-e", &format!("{branch}:{path}")])
                .await
                .is_err()
            {
                // Path doesn't exist in the branch — nothing to remove
                return Ok(());
            }

            // Use persistent worktree if it exists to avoid branch conflict.
            let persistent = self.persistent_worktree_dir(branch);
            let (worktree_dir, is_temp) = if persistent.exists() {
                (persistent.to_string_lossy().to_string(), false)
            } else {
                let tmp = format!("/tmp/nautiloop-wt-{}", uuid::Uuid::new_v4());
                self.run_git(&["worktree", "add", &tmp, branch])
                    .await
                    .map_err(|e| {
                        crate::error::NautiloopError::Git(format!(
                            "Failed to create worktree for {branch}: {e}"
                        ))
                    })?;
                (tmp, true)
            };

            // git rm -rf the path
            let rm = Command::new("git")
                .args(["rm", "-rf", path])
                .current_dir(&worktree_dir)
                .output()
                .await
                .map_err(|e| {
                    crate::error::NautiloopError::Git(format!("git rm spawn failed: {e}"))
                })?;

            if !rm.status.success() {
                let stderr = String::from_utf8_lossy(&rm.stderr).trim().to_string();
                if is_temp {
                    let _ = self
                        .run_git(&["worktree", "remove", "--force", &worktree_dir])
                        .await;
                }
                return Err(crate::error::NautiloopError::Git(format!(
                    "git rm {path} failed: {stderr}"
                )));
            }

            // Commit the removal
            let commit = Command::new("git")
                .args([
                    "-c",
                    "user.name=nautiloop-control-plane",
                    "-c",
                    "user.email=nautiloop@nautiloop.dev",
                    "commit",
                    "-m",
                    &format!("chore(agent): remove {path} artifacts"),
                ])
                .current_dir(&worktree_dir)
                .output()
                .await
                .map_err(|e| {
                    crate::error::NautiloopError::Git(format!("git commit spawn failed: {e}"))
                })?;
            if !commit.status.success() {
                let stderr = String::from_utf8_lossy(&commit.stderr).trim().to_string();
                if is_temp {
                    let _ = self
                        .run_git(&["worktree", "remove", "--force", &worktree_dir])
                        .await;
                }
                return Err(crate::error::NautiloopError::Git(format!(
                    "git commit failed: {stderr}"
                )));
            }

            if is_temp {
                let _ = self
                    .run_git(&["worktree", "remove", "--force", &worktree_dir])
                    .await;
            }
            Ok(())
        }

        async fn ci_status(&self, branch: &str) -> Result<Option<bool>> {
            let output = Command::new("gh")
                .args(["pr", "checks", branch, "--required"])
                .current_dir(&self.repo_path)
                .output()
                .await
                .map_err(|e| crate::error::NautiloopError::Git(format!("Failed to run gh: {e}")))?;

            let stdout = String::from_utf8_lossy(&output.stdout).to_lowercase();
            let stderr = String::from_utf8_lossy(&output.stderr).to_lowercase();

            if output.status.success() {
                // Exit 0: all checks passed. Check for failure keywords in case
                // gh reports partial results on success (shouldn't happen, but defensive).
                if stdout.contains("fail") || stdout.contains("cancelled") {
                    return Ok(Some(false));
                }
                return Ok(Some(true));
            }

            // Non-zero exit: could be CI failure, pending checks, or gh tool error.
            // Only classify as definitively failed if stdout shows actual CI results
            // with failure indicators. gh tool errors (auth, network) should be
            // treated as unknown to avoid permanently blocking auto-merge.

            // No required checks configured = pass
            if stderr.contains("no required checks") || stdout.contains("no required checks") {
                return Ok(Some(true));
            }

            // CI results present with failure indicators = definitively failed
            let has_ci_results = stdout.contains("pass")
                || stdout.contains("fail")
                || stdout.contains("pending")
                || stdout.contains("queued");
            if has_ci_results && (stdout.contains("fail") || stdout.contains("cancelled")) {
                Ok(Some(false))
            } else {
                // Non-zero without clear CI failure = unknown (transient/pending)
                Ok(None)
            }
        }

        async fn create_pr(
            &self,
            branch: &str,
            title: &str,
            body: &str,
            base_branch: &str,
        ) -> Result<String> {
            // Push branch to origin before creating PR
            self.run_git(&["push", "-u", "origin", branch])
                .await
                .map_err(|e| {
                    crate::error::NautiloopError::Git(format!(
                        "Failed to push {branch} to origin: {e}"
                    ))
                })?;

            // Check if a PR already exists for this branch (idempotent on retry)
            let existing = Command::new("gh")
                .args(["pr", "view", branch, "--json", "url", "--jq", ".url"])
                .current_dir(&self.repo_path)
                .output()
                .await;
            if let Ok(ref out) = existing
                && out.status.success()
            {
                let url = String::from_utf8_lossy(&out.stdout).trim().to_string();
                if !url.is_empty() {
                    return Ok(url);
                }
            }

            let output = Command::new("gh")
                .args([
                    "pr",
                    "create",
                    "--head",
                    branch,
                    "--base",
                    base_branch,
                    "--title",
                    title,
                    "--body",
                    body,
                ])
                .current_dir(&self.repo_path)
                .output()
                .await
                .map_err(|e| crate::error::NautiloopError::Git(format!("Failed to run gh: {e}")))?;

            if output.status.success() {
                Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
            } else {
                let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
                Err(crate::error::NautiloopError::Git(format!(
                    "Failed to create PR for {branch}: {stderr}"
                )))
            }
        }

        async fn merge_pr(
            &self,
            branch: &str,
            strategy: &str,
            default_branch: &str,
        ) -> Result<String> {
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
                .map_err(|e| crate::error::NautiloopError::Git(format!("Failed to run gh: {e}")))?;

            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
                return Err(crate::error::NautiloopError::Git(format!(
                    "Failed to merge PR for {branch}: {stderr}"
                )));
            }

            // Fetch to get the merge commit, then read the target branch SHA
            let _ = self.run_git(&["fetch", "origin"]).await;
            let remote_ref = format!("origin/{default_branch}");
            let sha = match self.run_git(&["rev-parse", &remote_ref]).await {
                Ok(s) => s,
                Err(_) => self.run_git(&["rev-parse", "HEAD"]).await?,
            };
            Ok(sha)
        }

        async fn ensure_worktree(&self, branch: &str, worktree_path: &str) -> Result<()> {
            let full_path = self.repo_path.join(worktree_path);
            if full_path.exists() {
                // Worktree already exists
                return Ok(());
            }
            // Create parent directories
            if let Some(parent) = full_path.parent() {
                tokio::fs::create_dir_all(parent).await.map_err(|e| {
                    crate::error::NautiloopError::Git(format!(
                        "Failed to create worktree parent dir: {e}"
                    ))
                })?;
            }
            self.run_git(&["worktree", "add", &full_path.to_string_lossy(), branch])
                .await
                .map_err(|e| {
                    crate::error::NautiloopError::Git(format!(
                        "Failed to create worktree for {branch} at {worktree_path}: {e}"
                    ))
                })?;
            Ok(())
        }

        async fn changed_files(&self, branch: &str, default_branch: &str) -> Result<Vec<String>> {
            let base_ref = format!("origin/{default_branch}");
            match self
                .run_git(&["diff", "--name-only", &format!("{base_ref}...{branch}")])
                .await
            {
                Ok(output) => Ok(output.lines().map(|l| l.to_string()).collect()),
                Err(_) => Ok(vec![]), // Can't determine diff — caller tests all services
            }
        }

        async fn push_branch(&self, branch: &str) -> Result<()> {
            self.run_git(&["push", "-u", "origin", branch])
                .await
                .map_err(|e| {
                    crate::error::NautiloopError::Git(format!(
                        "Failed to push {branch} to origin: {e}"
                    ))
                })?;
            Ok(())
        }
    }
}

/// In-memory mock for testing.
pub mod mock {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Arc;
    use tokio::sync::RwLock;

    /// Recorded call to `write_file_as` for test assertions (FR-3d).
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct WriteFileAsCall {
        pub branch: String,
        pub path: String,
        pub content: String,
        pub author_name: String,
        pub author_email: String,
        pub commit_message: String,
    }

    #[derive(Debug, Clone)]
    pub struct MockGitOperations {
        files: Arc<RwLock<HashMap<String, String>>>,
        branches: Arc<RwLock<HashMap<String, String>>>,
        default_sha: String,
        write_file_as_calls: Arc<RwLock<Vec<WriteFileAsCall>>>,
    }

    impl MockGitOperations {
        pub fn new() -> Self {
            Self {
                files: Arc::new(RwLock::new(HashMap::new())),
                branches: Arc::new(RwLock::new(HashMap::new())),
                default_sha: "0000000000000000000000000000000000000000".to_string(),
                write_file_as_calls: Arc::new(RwLock::new(Vec::new())),
            }
        }

        /// Return all recorded `write_file_as` calls for test assertions.
        pub async fn get_write_file_as_calls(&self) -> Vec<WriteFileAsCall> {
            self.write_file_as_calls.read().await.clone()
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

        async fn create_branch(&self, branch: &str, _base_remote_ref: &str) -> Result<String> {
            let sha = self.default_sha.clone();
            let mut branches = self.branches.write().await;
            branches.insert(branch.to_string(), sha.clone());
            Ok(sha)
        }

        async fn read_file(&self, path: &str, _git_ref: &str) -> Result<String> {
            let files = self.files.read().await;
            files
                .get(path)
                .cloned()
                .ok_or_else(|| crate::error::NautiloopError::Git(format!("File not found: {path}")))
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

        async fn write_file(&self, branch: &str, path: &str, content: &str) -> Result<()> {
            let mut files = self.files.write().await;
            files.insert(path.to_string(), content.to_string());
            drop(files);

            // Generate a distinct mock SHA so that get_branch_sha returns a new
            // value after the write, matching real git behavior (consistent with
            // write_file_as).
            use std::hash::{Hash, Hasher};
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            branch.hash(&mut hasher);
            path.hash(&mut hasher);
            content.hash(&mut hasher);
            "write_file".hash(&mut hasher);
            let hash_val = hasher.finish() as u128;
            let new_sha = format!("{:040x}", hash_val);
            let mut branches = self.branches.write().await;
            branches.insert(branch.to_string(), new_sha);

            Ok(())
        }

        async fn write_file_as(
            &self,
            branch: &str,
            path: &str,
            content: &str,
            author_name: &str,
            author_email: &str,
            commit_message: &str,
        ) -> Result<()> {
            let mut files = self.files.write().await;
            files.insert(path.to_string(), content.to_string());
            drop(files);

            // Generate a distinct mock SHA from the commit inputs so that
            // get_branch_sha returns a new value after the write, matching
            // real git behavior where each commit produces a new SHA.
            use std::hash::{Hash, Hasher};
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            branch.hash(&mut hasher);
            path.hash(&mut hasher);
            content.hash(&mut hasher);
            commit_message.hash(&mut hasher);
            author_name.hash(&mut hasher);
            author_email.hash(&mut hasher);
            let hash_val = hasher.finish() as u128;
            let new_sha = format!("{:040x}", hash_val);
            let mut branches = self.branches.write().await;
            branches.insert(branch.to_string(), new_sha);
            drop(branches);

            let mut calls = self.write_file_as_calls.write().await;
            calls.push(WriteFileAsCall {
                branch: branch.to_string(),
                path: path.to_string(),
                content: content.to_string(),
                author_name: author_name.to_string(),
                author_email: author_email.to_string(),
                commit_message: commit_message.to_string(),
            });
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

        async fn ci_status(&self, _branch: &str) -> Result<Option<bool>> {
            Ok(Some(true))
        }

        async fn create_pr(
            &self,
            branch: &str,
            _title: &str,
            _body: &str,
            _base_branch: &str,
        ) -> Result<String> {
            Ok(format!("https://github.com/mock/repo/pull/{branch}"))
        }

        async fn merge_pr(
            &self,
            branch: &str,
            _strategy: &str,
            _default_branch: &str,
        ) -> Result<String> {
            let branches = self.branches.read().await;
            Ok(branches
                .get(branch)
                .cloned()
                .unwrap_or_else(|| "merge-sha-mock".to_string()))
        }

        async fn ensure_worktree(&self, _branch: &str, _worktree_path: &str) -> Result<()> {
            Ok(())
        }

        async fn changed_files(&self, _branch: &str, _default_branch: &str) -> Result<Vec<String>> {
            Ok(vec![])
        }

        async fn push_branch(&self, _branch: &str) -> Result<()> {
            Ok(())
        }
    }
}
