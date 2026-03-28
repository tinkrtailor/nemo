# Adversarial Review: Round 12 (OpenCode GPT-5.4, read-only)

3 findings.

## FINDINGS

N47. **MEDIUM** - generate_branch_name interpolates raw engineer and spec filename into git ref. Spaces, .., ~, ^, :, trailing .lock produce invalid refs. Same-stem specs from different dirs collide (types/mod.rs:244). Fix: slugify both segments, add path hash for uniqueness, validate against git check-ref-format rules.

N48. **LOW** - nemo inspect help says "alice/..." but server expects "agent/alice/...". CLI forwards raw input, documented command returns not found (main.rs:117, inspect.rs:6, handlers.rs:329). Fix: CLI prepends "agent/" automatically, or server accepts both forms.

N49. **LOW** - nemo auth stores raw credential JSON in Postgres as credential_ref. Raw secrets in application DB (auth.rs:45, client.rs:96, handlers.rs:394). Fix: for V1, accept this as known limitation. For V2, use K8s Secrets or KMS. Add a comment noting this is a V1 shortcut.
