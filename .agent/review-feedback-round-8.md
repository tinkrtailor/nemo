# Adversarial Review: Round 8 (OpenCode GPT-5.4)

3 findings. All real, all narrow.

## FINDINGS

N28. **HIGH** - create_branch uses `git branch <name> HEAD` but HEAD is local, not the freshly fetched remote tip. git fetch doesn't move HEAD. Loops start from stale code (git/mod.rs:96, handlers.rs:27). Fix: use `git branch <name> origin/main` (or whatever the default branch is) instead of HEAD.

N29. **MEDIUM** - nemo auth uploads the local credential FILE PATH as credential_ref. The control plane stores the string but can't read the caller's workstation path. Reauth registration reports success but doesn't actually work (auth.rs:25, client.rs:92, handlers.rs:384). Fix: for V1, auth should upload the CONTENT of the credential files (or copy them to K8s secrets), not just the path. Or mark as known V1 limitation.

N30. **MEDIUM** - Resumed implement rounds always reconstruct feedback_path as review-feedback-round-N.json. But test failure rounds write test-feedback-round-N.json (driver.rs:1075 vs driver.rs:455). Resume after test failure points at wrong file. Fix: persist the actual feedback_path (review or test) in the round record, restore it on resume.
