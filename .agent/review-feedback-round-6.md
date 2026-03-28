# Adversarial Review: Round 6 (OpenCode GPT-5.4)

4 findings. All real bugs, not style.

## FINDINGS

N19. **HIGH** - /start uses local HEAD for spec existence and branch creation without fetching first. If origin/main has advanced, a newly pushed spec is rejected as missing, and valid runs branch from stale code (handlers.rs:27, 34, 45, git/mod.rs:93). Fix: git fetch before spec validation and branch creation in the /start handler.

N20. **HIGH** - Ship mode checks CI only once, immediately after PR creation. Async CI is usually still pending, so the loop marks CONVERGED instead of SHIPPED even when checks would pass later (driver.rs:503). Fix: poll CI status with backoff (e.g., check every 30s for up to 30 min) before deciding CONVERGED vs SHIPPED.

N21. **MEDIUM** - get_loops_for_engineer returns terminal loops too, but GET /status is "show running loops." Users see completed/failed/shipped mixed into active status (postgres.rs:297). Fix: filter to non-terminal states in the status query, or add a --all flag.

N22. **MEDIUM** - Branch force-reset on restart (git branch -f) silently rewrites an old branch/PR instead of creating fresh work (git/mod.rs:99, handlers.rs:38, types/mod.rs:228). Fix: if branch exists AND has a merged/closed PR, create a new branch with incremented suffix (e.g., -v2). If branch exists with no PR, force-reset is fine.
