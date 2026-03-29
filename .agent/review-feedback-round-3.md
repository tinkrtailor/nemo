Not converged. I read all Rust source under `control-plane/src` and `cli/src` and found 4 real production bugs.

- `control-plane/src/loop_engine/driver.rs:301` + `control-plane/src/loop_engine/driver.rs:327` + `control-plane/src/loop_engine/driver.rs:460` + `control-plane/src/loop_engine/driver.rs:516` + `control-plane/src/loop_engine/driver.rs:408`  
  Completed jobs are decoded with the wrong JSON shape. `extract_nemo_result()` stores only the `data` payload, but the evaluators still parse legacy top-level structs (`AuditVerdict`, `ReviewVerdict`, `TestOutput`, `ReviseOutput`). Real impact: successful audit/review/test/revise jobs get treated as malformed/no output, so loops retry or fail instead of advancing.

- `control-plane/src/k8s/client.rs:125`  
  `get_job_logs()` only requests `tail_lines: Some(100)`. If the agent emits more than 100 log lines and `NEMO_RESULT:` is not in that tail window, a successful job is misclassified as having no result. Real impact: false failures on noisy runs.

- `control-plane/src/api/handlers.rs:454`  
  Credential secret updates ignore failure on the 409/replace path. The API returns 200 even if the Kubernetes Secret update fails after Postgres metadata is updated. Real impact: `nemo auth` can appear successful while resumed jobs still run with stale or expired credentials.

- `control-plane/src/loop_engine/driver.rs:229` + `control-plane/src/loop_engine/driver.rs:1125` + `control-plane/src/k8s/job_builder.rs:252`  
  Stage outputs include `session_id`, but ingestion never persists it back onto the loop record. Later rounds and resume/reauth paths therefore stop sending `SESSION_ID`. Real impact: session continuity is broken across retries/rounds, especially for multi-round review/implement flows.

If you want, I can do a round 4 read-only pass after these are fixed.
