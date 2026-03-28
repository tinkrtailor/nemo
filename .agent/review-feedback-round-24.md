# Adversarial Review: Round 24 (OpenCode GPT-5.4, read-only)

1 finding.

## FINDINGS

N87. **HIGH** - Completed jobs skip divergence check. handle_job_completed refreshes current_sha from live branch tip before reading artifacts. If someone pushes between job exit and reconcile tick, Nemo ingests verdict from wrong commit and can CONVERGE/SHIP unreviewed code (driver.rs:169, 217). Fix: in handle_job_completed, compare the job's expected SHA (stored in the round record or loop state) against the branch tip BEFORE ingesting output. If diverged, pause instead of ingesting.
