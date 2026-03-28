# Adversarial Review: Round 20 (OpenCode GPT-5.4, read-only)

1 finding.

## FINDINGS

N73. **MEDIUM** - Control plane only handles ctrl_c(), not SIGTERM. K8s sends SIGTERM on pod shutdown (deploys, evictions). API server, reconciler, and watcher won't drain gracefully (main.rs:122). Fix: use tokio::signal::unix::signal(SignalKind::terminate()) alongside ctrl_c(). Select on both. On either signal, trigger graceful shutdown (stop accepting new requests, drain in-flight reconciliation, flush state).
