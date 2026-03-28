# Adversarial Review: Round 11 (OpenCode GPT-5.4)

5 findings.

## FINDINGS

N42. **HIGH** - Test-stage retries inherit implement-stage retry debt. retry_count is per-loop not per-stage, so testing can fail too early after prior transient implement failures (driver.rs:392, 727). Fix: reset retry_count when stage transitions (implement → test → review). Retries should be per-stage.

N43. **HIGH** - K8s job failure parsing keeps only condition.reason, drops condition.message. Auth error text lost, expired creds misclassified as generic failure (k8s/client.rs:95, driver.rs:704, 1141). Fix: include both reason and message in the failure info. Check message for auth keywords too.

N44. **HIGH** - Temp-worktree commits rely on ambient git identity. On clean hosts/containers with no global git config, git commit fails (git/mod.rs:158, 191). Fix: pass -c user.name and -c user.email from the loop's engineer config to all git commit commands.

N45. **MEDIUM** - nemo auth exits successfully when one provider succeeds and another fails. Partial auth reported as success (auth.rs:22, 66). Fix: track success/failure per provider, exit non-zero if any provider fails, report which succeeded and which failed.

N46. **LOW** - nemo config --get api_key masks by byte slicing, panics on non-ASCII (config.rs:12). Fix: use .chars() iterator instead of byte indexing, or just show first 4 and last 4 chars with ... in between.
