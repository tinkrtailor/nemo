# Learnings

Discoveries and patterns found during Nemo development. Persists across all work sessions.

## Rust / Cargo

- sqlx compile-time query macros (`query!`, `query_as!`) require `DATABASE_URL` at build time. Use runtime `sqlx::query()` with `.bind()` for CI-friendly builds without a running Postgres.
- kube-rs 0.98 requires k8s-openapi 0.24 (not 0.23). Always check the kube-rs compatibility matrix for the correct k8s-openapi version.
- `kube::Client` does not implement `Debug`. Structs containing it cannot use `#[derive(Debug)]`.
- axum `Router` type inference in tests is fragile. Use an explicit `async fn send_request(app: Router, req: Request<Body>) -> Response<Body>` helper rather than inline `.oneshot()`.
- Rust edition 2024 supports `option.is_none_or()` and `option.is_some_and()` natively.
- The GitHub `ci` workflow is stricter than the repo's default local gate: it runs `cargo fmt --all -- --check` and `cargo clippy --workspace --all-targets --features nautiloop-sidecar/__test_utils -- -D warnings`. If local `cargo clippy --workspace -- -D warnings` passes but CI is still red, run the exact CI commands locally before assuming the failure is remote-only.

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

## Sidecar parity harness

- Python `cryptography.hazmat.serialization.PrivateFormat.OpenSSH` wraps PEM base64 at 76 chars; russh's `PrivateKey::from_openssh` (via the `ssh-key` crate) rejects that as `Encoding(Pem(Base64(InvalidEncoding)))`. The canonical OpenSSH format wraps at **70 chars**. When generating committed test fixtures for SSH keys, build the PROTOCOL.key blob manually and wrap at 70. See `sidecar/tests/parity/fixtures/regenerate-ssh-fixtures.py` for the reference implementation.
- The repo's `.gitignore` has global `*.pem` and `*.key` rules for safety. When committing test-only certs/keys under `sidecar/tests/parity/fixtures/`, add explicit unignore patterns (`!sidecar/tests/parity/fixtures/**/*.pem`, `!sidecar/tests/parity/fixtures/**/*.key`) — otherwise `git add fixtures/` silently skips the cert files and the harness fails at runtime with confusing errors.
- Docker forbids parent-directory COPYs. The parity harness Dockerfiles (Go sidecar + mock services) all set `build.context` to the repo root (`../../..` relative to `docker-compose.yml`) and then use absolute-from-context paths like `images/sidecar/main.go` and `sidecar/tests/parity/fixtures/mock-openai/server.py`.
- RFC6598 CGNAT (`100.64.0.0/10`) is the clean workaround for dockerizing SSRF-aware services that block RFC1918. Both the Go and Rust auth sidecars explicitly leave CGNAT unblocked, so a custom bridge with `subnet: 100.64.0.0/24` reaches mock services without any test-only code bypass. See `sidecar/src/ssrf.rs:94-99` and `images/sidecar/main.go:43-48`.
- Docker published host ports do not reliably reach services bound only to `127.0.0.1` inside the container. The parity harness needs the sidecars' private listeners reachable from the host, so it sets parity-only `NAUTILOOP_BIND_ALL_INTERFACES=true` in `sidecar/tests/parity/docker-compose.yml` and keeps that env var allowlisted in `sidecar/scripts/lint-no-test-utils-in-prod.sh`.
- In `sidecar/src/git_ssh_proxy.rs`, agent EOF must not overtake already-buffered `AgentData::Data` frames. Flush queued data before `upstream_channel.eof().await` or `git-receive-pack` can observe EOF first and treat the push as empty.
- Clippy at Rust edition 2024 rejects `assert!(CONST_EXPR)` as `assertions_on_constants` — use `const _: () = assert!(...)` for compile-time assertions. Also rejects `if let Some(x) = ... { if cond { ... } }` as `collapsible_if` — collapse with `if let Some(x) = ... && cond { ... }`.
- russh 0.60's `Channel::data` takes `R: tokio::io::AsyncRead + Unpin`, not `&[u8]`. Wrap bytes in `std::io::Cursor::new(bytes)` to satisfy the bound.
- russh 0.60's env request API is `Channel::set_env(want_reply, name, value)`, not `request_env`. A server that rejects env returns `ChannelMsg::Failure` on the channel's next `wait()` when `want_reply == true`.
