# Adversarial Review: Round 7 (OpenCode GPT-5.4)

5 findings. All real production bugs.

## FINDINGS

N23. **HIGH** - Force-reset on branch with OPEN PR invalidates the PR's contents (git/mod.rs:117). Fix: detect open PRs (gh pr view --json state) and refuse reuse. Mint new branch with -v2 suffix if open PR exists.

N24. **HIGH** - PR creation happens without pushing branch to origin first. gh pr create --head will fail for local-only branch (git/mod.rs:229). Fix: git push -u origin <branch> before gh pr create.

N25. **HIGH** - Retry/resume drops feedback context. redispatch_current_stage rebuilds context via build_context which hardcodes feedback_path: None (driver.rs:900, 1075). Fix: persist feedback_path on loop state (or latest round record) and restore when rebuilding context.

N26. **MEDIUM** - Redispatch deletes Job with background propagation then immediately recreates same name. Old Job may still exist, AlreadyExists race (driver.rs:928, k8s/client.rs:42). Fix: use foreground deletion (propagationPolicy: Foreground) and wait for actual deletion, or append retry count to job name.

N27. **LOW** - SSE log tailing uses timestamp > $2 cursor. Multiple rows at same timestamp: later rows skipped forever (api/sse.rs:22, postgres.rs:509). Fix: use (timestamp, id) as composite cursor.
