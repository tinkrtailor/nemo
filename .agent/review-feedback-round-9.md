# Adversarial Review: Round 9 (OpenCode GPT-5.4)

6 findings.

## FINDINGS

N31. **CRITICAL** - harden_only requests never set record.harden = true, but driver only runs hardening when that flag is true. Harden-only submission skips hardening entirely (handlers.rs:61, driver.rs:80). Fix: set harden = true when harden_only = true in the /start handler.

N32. **HIGH** - Feedback rounds commit a file and advance branch tip, but record.current_sha not refreshed. Next divergence check sees stale SHA and falsely pauses the loop (driver.rs:150, 888). Fix: after write_file commits, update record.current_sha to the new branch tip.

N33. **HIGH** - NemoConfig::load() only checks NEMO_CONFIG_PATH or /etc/nemo/nemo.toml. Repo-local nemo.toml ignored (config/mod.rs:21). Fix: also check ./nemo.toml or load repo config path from a separate mechanism.

N34. **HIGH** - nemo auth exits successfully even when no credential file exists or all uploads fail (auth.rs:22). Fix: check file existence before upload, propagate upload errors, exit non-zero on failure.

N35. **MEDIUM** - Fresh config defaults engineer to empty string. Multiple commands use it without validation (config.rs:23, main.rs:184). Fix: validate engineer is non-empty at CLI startup for commands that need it.

N36. **MEDIUM** - Engineer name interpolated into query string without URL encoding (status.rs:24). Fix: use url::form_urlencoded or percent-encode the value.
