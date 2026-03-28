# Adversarial Review: Round 13 (OpenCode GPT-5.4, read-only)

2 findings.

## FINDINGS

N50. **MEDIUM** - Revise output never parsed. ReviseOutput.updated_spec_path is ignored. If harden revise renames/moves the spec, next audit uses stale record.spec_path (driver.rs:372). Fix: after revise job completes, parse output and update record.spec_path if the spec was moved.

N51. **MEDIUM** - SSE log cursor uses (timestamp, random UUID). UUID is not monotonic, so same-timestamp logs with lower UUID than last_id are skipped forever (postgres.rs:518). Fix: use a serial/BIGSERIAL id column as the cursor instead of UUID. Or use timestamp-only cursor with inclusive query and client-side dedup by id.
