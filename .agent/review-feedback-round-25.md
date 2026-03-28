# Adversarial Review: Round 25 (OpenCode GPT-5.4, read-only)

1 finding. Same pattern as the PAUSED resume fix, but for AWAITING_REAUTH.

## FINDINGS

N88. **MEDIUM** - AWAITING_REAUTH resume doesn't refresh current_sha. PAUSED resume does (driver.rs:728) but AWAITING_REAUTH resume (driver.rs:747) doesn't. Branch tip can advance during reauth wait, causing false divergence on next tick. Fix: add the same current_sha refresh to handle_awaiting_reauth that handle_paused has. One line.
