# Adversarial Review: Round 18 (OpenCode GPT-5.4, read-only)

6 findings.

## FINDINGS

N62. **HIGH** - Cancel can overwrite a completed loop as CANCELLED. tick() honors cancel_requested before terminal-state check (driver.rs:50, 717, handlers.rs:264). Fix: check is_terminal() FIRST in tick(). If terminal, clear the flag and return early. Never transition out of a terminal state.

N63. **HIGH** - Resuming divergence-paused loop re-pauses immediately. Resume redispatches without refreshing expected SHA. Next tick sees same branch-tip mismatch and pauses again (driver.rs:150, 665, 1152). Fix: on resume, update record.current_sha to the current branch tip BEFORE redispatching.

N64. **MEDIUM** - get_credentials().await.unwrap_or_default() masks DB errors. Launches jobs unauthenticated on store failure, causing false auth-expired outcomes (driver.rs:1137, 737). Fix: propagate the error. If credential lookup fails, retry or mark loop as FAILED with clear reason, don't silently proceed without creds.

N65. **MEDIUM** - NEMO_INSECURE=false still disables TLS. Check is presence-based (is_ok()), any set value enables insecure (main.rs:171). Fix: check for "true"/"1" explicitly, not just presence.

N66. **MEDIUM** - CLI writes ~/.nemo/config.toml with default umask perms. API keys may be world-readable (config.rs:52). Fix: set file permissions to 0600 after write, or use a tempfile with restrictive perms and rename.

N67. **LOW** - nemo auth stops on one unreadable credential file instead of skipping and continuing (auth.rs:38). Fix: log warning for unreadable files, continue with other providers.
