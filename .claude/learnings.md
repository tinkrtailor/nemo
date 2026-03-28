# Learnings

Discoveries and patterns found during Nemo development. Persists across all work sessions.

## Rust / Cargo

- sqlx compile-time query macros (`query!`, `query_as!`) require `DATABASE_URL` at build time. Use runtime `sqlx::query()` with `.bind()` for CI-friendly builds without a running Postgres.
- kube-rs 0.98 requires k8s-openapi 0.24 (not 0.23). Always check the kube-rs compatibility matrix for the correct k8s-openapi version.
- `kube::Client` does not implement `Debug`. Structs containing it cannot use `#[derive(Debug)]`.
- axum `Router` type inference in tests is fragile. Use an explicit `async fn send_request(app: Router, req: Request<Body>) -> Response<Body>` helper rather than inline `.oneshot()`.
- Rust edition 2024 supports `option.is_none_or()` and `option.is_some_and()` natively.

## k8s / kube-rs

- `kube::runtime::watcher` returns a stream that needs `futures::StreamExt::boxed()` for pinning. Import both `StreamExt` and `TryStreamExt`.
- Job status is best determined by checking conditions first (Complete/Failed), then active pod count, then succeeded/failed counts.
- Use `PropagationPolicy::Background` when deleting Jobs to clean up pods.

## Control Plane

- The state store trait (`StateStore`) with an in-memory implementation (`MemoryStateStore`) is the primary testing strategy. All driver and API tests use it.
- The loop driver's `tick()` method is the core primitive: one call per loop per reconciliation interval. All state transitions happen within a single tick.
- Credential expiry detection works by pattern-matching on job failure reasons (e.g., "unauthorized", "token expired"). This is heuristic-based until agents report structured exit codes.

## CLI

- CLI config lives at `~/.nemo/config.toml`. The `toml` crate handles both serialization and deserialization.
- `reqwest::Client` with `danger_accept_invalid_certs(true)` is needed for dev environments with self-signed TLS certs.
