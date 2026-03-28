# Adversarial Review: Round 16 (OpenCode GPT-5.4, read-only)

2 findings. Past max_rounds, engineer override.

## FINDINGS

N57. **HIGH** - Concurrent /start for same branch: request B can force-reset A's branch, fail DB insert, then delete the branch. A's loop now points at a deleted branch (handlers.rs:118, git/mod.rs:132). Fix: acquire a Postgres advisory lock on the branch name before git operations. Or: do git operations AFTER the DB insert succeeds (DB is the source of truth, git follows).

N58. **LOW** - remove_path() with --allow-empty creates bogus empty commits when .agent/ already absent. Changes SHAs, retriggers CI for nothing (git/mod.rs:258). Fix: check if .agent/ exists before running git rm. Skip the commit if nothing changed.
