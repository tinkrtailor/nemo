# Rust Rewrite: Auth Sidecar

## Overview

Rewrite `images/sidecar/` from Go to Rust as a new workspace crate. Goal: behavior-identical replacement that passes a parity test harness against the current Go binary, then the Go implementation is deleted. Consolidates the entire codebase on one language, one toolchain, one security review surface.

## Problem Statement

Nautiloop is a Rust project with one exception: `images/sidecar/` (843 lines of Go, `golang.org/x/crypto/ssh` + `net/http`). The sidecar was originally Go because:

1. `net/http` + `httputil.ReverseProxy` made the HTTP proxy piece trivial.
2. `golang.org/x/crypto/ssh` was the most battle-tested server-side SSH library.
3. Goroutines + blocking I/O are simpler than Rust async for a proxy-shaped workload.
4. A `FROM scratch` Go binary is ~10 MB.

All four reasons were valid in 2025. They're no longer compelling enough to justify the cost of maintaining two languages:

### Costs of the split today

- **Two supply chains.** Cargo + Go modules. `cso` security audits have to walk both. CI has to install and cache both toolchains.
- **Two test runners.** `cargo test --workspace` does not cover the sidecar. Someone has to remember to also run `go test ./images/sidecar/`. When they forget, regressions land silently.
- **No shared types.** The sidecar has its own `egressLogEntry` struct. Any schema change has to be manually kept in sync with any control-plane code that parses those logs. Drift is a matter of time.
- **Cognitive cost.** Every contributor to the sidecar has to context-switch between `.claude/CLAUDE.md` Rust rules (clippy gate, thiserror, sqlx, kube-rs) and Go conventions.
- **Clippy gate doesn't cover it.** `cargo clippy --workspace -- -D warnings` is one of the highest-leverage rules in this repo. The sidecar is completely exempt.

### Why this is doable in Rust now

- **`russh`** (async, tokio-based, actively maintained) is production-ready for server-side SSH. Used by Pijul and Kraken CI. Supports channel forwarding, exec requests, and custom authentication hooks — everything the current Go sidecar uses from `x/crypto/ssh`.
- **`hyper`** + **`hyper-util`** cover both the model API proxy (with Bearer/x-api-key header injection) and the egress HTTP/CONNECT proxy. `axum` optional on top for the model proxy routing.
- **`tokio`** + **musl target** produce a static binary in the same size class as the Go one (~15–20 MB vs. ~10 MB). Not a meaningful differentiator for a per-pod sidecar.
- **`tracing`** + **`tracing-subscriber`** with a JSON formatter replaces the hand-rolled `egressLogEntry` marshaling with a tested, type-safe approach.

## Dependencies

- **Requires:** nothing — the Go sidecar continues to work throughout. This is an additive rewrite with a flip-the-switch final step.
- **Enables:** single-language codebase, single clippy gate, shared types crate option in the future.
- **Blocks:** nothing.

## Requirements

### Functional Requirements — Behavior Parity

The Rust sidecar shall be **behavior-identical** to the Go implementation at `images/sidecar/main.go` as of commit `17b3a6a` (release 0.2.7). "Behavior-identical" means: for every input the Go sidecar accepts, the Rust sidecar produces the same observable output (HTTP status, response body, log line, exit code, proxy forward).

- FR-1: **Model API proxy on `127.0.0.1:9090`** routes `/openai/*` to `https://api.openai.com` with `Authorization: Bearer <key>` injection, and `/anthropic/*` to `https://api.anthropic.com` with `x-api-key: <key>` injection and a default `anthropic-version: 2023-06-01` header if not present. All other paths return HTTP 403 with body `{"error":"only /openai/* and /anthropic/* routes are supported"}`.
- FR-2: Model proxy shall re-read the credential file (`/secrets/model-credentials/openai` or `/secrets/model-credentials/anthropic`) on **each** request. Trim trailing whitespace. No in-memory caching. (Parity with Go `readCredentialFile`.)
- FR-3: Model proxy shall pass through all request headers from the agent to upstream, then overwrite only `Authorization` (for OpenAI) or `x-api-key` + `anthropic-version` (for Anthropic). Existing client-supplied auth is replaced, not merged.
- FR-4: Model proxy shall stream the response body from upstream to client without buffering (no `Content-Length` rewrite, no compression, no chunked-to-sized conversion). Required for SSE streaming from the model APIs.
- FR-5: **Git SSH proxy on `127.0.0.1:9091`** shall accept SSH connections from the agent with `NoClientAuth` (safe because the listener is loopback-only and the pod is single-agent). An ephemeral Ed25519 host key is generated on startup.
- FR-6: Git SSH proxy shall accept only `session` channels. All other channel types are rejected with `UnknownChannelType`.
- FR-7: On a `session` channel, only `exec` requests with command `git-upload-pack <path>` or `git-receive-pack <path>` are accepted. `env`, `pty-req`, `subsystem`, and all other request types are rejected with `req.Reply(false, ...)` and exit status 1.
- FR-8: Git SSH proxy shall validate that `<path>` in the exec request matches the repo path derived from `GIT_REPO_URL`. Mismatches are rejected with a warning log and exit status 1. (Parity with Finding 6 validation in `handleSSHSession`.)
- FR-9: On an accepted command, the sidecar shall open an SSH client connection to the git remote host derived from `GIT_REPO_URL`, authenticating with `/secrets/ssh-key/id_ed25519` and verifying the host key against `/secrets/ssh-known-hosts/known_hosts`. No `InsecureIgnoreHostKey` fallback — missing or empty `known_hosts` is a hard failure with exit status 1 and an error log.
- FR-10: Git SSH proxy shall pipe stdin/stdout/stderr bidirectionally between the agent's channel and the upstream SSH session for the duration of the git command, and propagate the upstream exit status back to the agent channel.
- FR-11: **Egress logger on `127.0.0.1:9092`** shall implement both HTTP `CONNECT` (for HTTPS tunneling) and plain HTTP forwarding. For CONNECT it hijacks the connection and pipes bytes bidirectionally; for HTTP it forwards the request and streams the response.
- FR-12: Egress logger shall perform **SSRF protection** on the destination host: resolve via DNS and reject if *any* returned IP is in RFC1918 (`10.0.0.0/8`, `172.16.0.0/12`, `192.168.0.0/16`), link-local (`169.254.0.0/16`), loopback (`127.0.0.0/8`), IPv6 loopback/link-local, or `fc00::/7`. Rejection returns HTTP 403 and logs a warning.
- FR-13: Model proxy shall also perform the same SSRF check on `api.openai.com` / `api.anthropic.com` before forwarding. (Parity with Go — the check is not redundant; it protects against DNS-rebinding pointing at internal IPs.)
- FR-14: Egress logger shall emit one JSON line per completed request to stdout with fields `{timestamp, destination, method, bytes_sent, bytes_recv, prefix}`. `timestamp` is RFC3339Nano UTC. `prefix` is the constant string `"NAUTILOOP_SIDECAR"`. Field names and types are locked — any downstream log parser must not need to change.
- FR-15: **Health endpoint on `127.0.0.1:9093`** shall serve `GET /healthz`. Until all four ports (9090, 9091, 9092, 9093) are confirmed listening, it returns `503` with body `{"status":"starting"}`. After readiness is flipped, it returns `200` with body `{"status":"ok"}`. Content-Type is `application/json`.
- FR-16: On startup, after all four listeners are bound, the sidecar shall verify readiness by dialing each port on `127.0.0.1` with a 100ms timeout, retrying up to 100 times at 20ms intervals (2s total budget per port). If any port fails to bind within budget, the process exits with a fatal error log. (Parity with Go `main()` port verification loop.)
- FR-17: After readiness verification, the sidecar shall flip the `ready` flag AND write a readiness file at `/tmp/shared/ready` with content `"ready"` and mode `0644`, creating `/tmp/shared` with mode `0755` if missing. (Belt-and-braces with the K8s startupProbe — the agent entrypoint historically polls this file.)
- FR-18: On `SIGTERM` or `SIGINT`, the sidecar shall initiate graceful shutdown: stop accepting new SSH connections, call shutdown on all three HTTP servers with a 5-second deadline, and wait for in-flight SSH sessions to complete. If SSH drain exceeds the 5s budget, log a warning (`"SSH session drain timed out, proceeding with shutdown"`) and exit anyway.
- FR-19: The sidecar reads `GIT_REPO_URL` from environment at startup. Missing or unparseable URL is a fatal error. Parsing shall handle three formats: `ssh://user@host[:port]/path`, `user@host:path` (scp-style), and `https://host/path`. The derived `host:port` is the upstream SSH destination; the derived `repo_path` is the allowlist for FR-8.
- FR-20: All logs shall be JSON lines on stdout with fields `{timestamp, level, message, prefix}`. `prefix` is `"NAUTILOOP_SIDECAR"`. `level` is `"info" | "warn" | "error"`. This is the non-egress logging channel (startup, shutdown, errors). Egress logs are a separate schema per FR-14.

### Non-Functional Requirements

- NFR-1: **No behavioral regressions.** The parity test harness (see Test Plan) must pass before the Go implementation is removed.
- NFR-2: **Binary size ≤ 25 MB.** Measured on `x86_64-unknown-linux-musl` `--release` with `strip = true`, `lto = "fat"`, `codegen-units = 1`, `panic = "abort"`. The Go version is ~10 MB; Rust with musl + aggressive optimization typically lands at 15–20 MB. 25 MB is the hard ceiling; anything over triggers a profile review.
- NFR-3: **Startup time ≤ 500ms to `ready=true`.** Measured from process start to the `ready.Store(true)` equivalent. The Go version is typically <100ms. We allow a 5x budget for Rust async runtime init and TLS config parsing; any more than that indicates something is wrong.
- NFR-4: **Memory RSS ≤ 50 MB steady-state** under idle (no active proxying). Go version is ~8 MB RSS idle. 50 MB is generous for tokio + rustls + russh.
- NFR-5: **Zero runtime dependencies.** Final image is `FROM scratch` with only the compiled binary and any CA certs bundle needed by rustls. No libc, no shell, no package manager.
- NFR-6: **No panic paths in request handlers.** Every `unwrap`, `expect`, `panic!`, and `unimplemented!` in request-serving code is a bug. Startup and config parsing may panic on fatal errors (matches Go `log.Fatalf` behavior) but request handlers must propagate errors as HTTP responses / SSH exit statuses.
- NFR-7: **Log format stability.** The JSON schemas from FR-14 and FR-20 are ABI. Downstream log processors depend on them. Any change requires a version bump on the log line and a coordinated parser update — out of scope for this spec.
- NFR-8: **Clippy clean.** `cargo clippy -p nautiloop-sidecar --all-targets -- -D warnings` is green before merge. Same bar as the rest of the workspace.

### Security Requirements

- SR-1: The SSH host key is generated ephemerally on each startup and never written to disk. (Parity with Go `ed25519.GenerateKey`.)
- SR-2: SSH client connections to the upstream git remote MUST verify the host key against `/secrets/ssh-known-hosts/known_hosts`. Missing or empty file is a hard refusal. No `InsecureIgnoreHostKey`, no "verify on first use", no fallback. This matches the Go implementation's current hardening and is a load-bearing security property.
- SR-3: Model credential files are read fresh per request. They are never cached in memory across requests. (Parity with FR-21 in the original Go spec.)
- SR-4: The only upstream hosts the model proxy is allowed to reach are `api.openai.com` and `api.anthropic.com`. Any other path returns 403 before any network activity.
- SR-5: SSRF protection (FR-12, FR-13) is mandatory and runs before any outbound connection. DNS rebinding is mitigated by resolving once and checking all returned IPs.
- SR-6: Ed25519 host key generation shall use `ring::rand::SystemRandom` or `rand::rngs::OsRng` (the OS CSPRNG). No custom RNGs, no `StdRng::from_seed`.
- SR-7: SSH env/pty/subsystem requests are rejected at the request-type level, not just unhandled. Explicit `req.respond(false)` for each.
- SR-8: The repo path validation (FR-8) matches on the trimmed path string. Leading slashes are stripped from both sides before comparison. Quote characters `'` and `"` are stripped from the requested repo before comparison. (Parity with Go `strings.Trim` + `TrimPrefix`.)

## Architecture

### Workspace layout

Add a new workspace member:

```
.
├── cli/
├── control-plane/
└── sidecar/                    ← NEW
    ├── Cargo.toml
    ├── src/
    │   ├── main.rs             # startup, shutdown, port verification
    │   ├── model_proxy.rs      # FR-1 to FR-4
    │   ├── git_ssh_proxy.rs    # FR-5 to FR-10 (russh server + client)
    │   ├── egress.rs           # FR-11 to FR-14 (CONNECT + HTTP proxy)
    │   ├── health.rs           # FR-15 (atomic readiness flag + handler)
    │   ├── ssrf.rs             # FR-12, FR-13 (private IP check, reusable)
    │   ├── git_url.rs          # FR-19 (parse ssh://, scp, https)
    │   └── logging.rs          # FR-14, FR-20 (two JSON schemas)
    └── tests/
        └── parity/             # see Test Plan
```

Workspace `Cargo.toml` gets a new `members = [..., "sidecar"]` entry. The crate name is `nautiloop-sidecar` for consistency with `nautiloop-control-plane`.

### Dependency list (pinned in the spec so reviewers can see the surface)

```toml
[dependencies]
tokio = { version = "1", features = ["rt-multi-thread", "macros", "net", "io-util", "signal", "sync", "time", "fs"] }
hyper = { version = "1", features = ["server", "client", "http1"] }
hyper-util = { version = "0.1", features = ["tokio", "server", "client-legacy"] }
http-body-util = "0.1"
russh = "0.45"           # or latest; version lock during implementation
russh-keys = "0.45"
rustls = "0.23"
rustls-pemfile = "2"
webpki-roots = "0.26"
tokio-rustls = "0.26"
serde = { version = "1", features = ["derive"] }
serde_json = "1"
thiserror = "1"
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["json", "env-filter"] }
ed25519-dalek = { version = "2", features = ["rand_core"] }
rand = "0.8"
url = "2"
ipnet = "2"              # private IP CIDR checks
```

Rationale for picks:
- `hyper` + `hyper-util` over `axum`: the model proxy is small enough (~100 LOC) that axum's router is overhead. Direct hyper gives us precise control over streaming semantics (FR-4).
- `russh` over `thrussh`: actively maintained fork, tokio-native, matches our runtime.
- `rustls` over `native-tls`: no OpenSSL dependency, musl-friendly, no dynamic linker surprises.
- `ed25519-dalek` for host key generation: matches the Go implementation byte-for-byte and is the most common Rust choice.
- `tracing` for structured logs — the JSON format output is identical to the Go one (same field names, same timestamp format).

### Binary profile

`sidecar/Cargo.toml`:

```toml
[profile.release]
lto = "fat"
codegen-units = 1
strip = true
panic = "abort"
opt-level = "z"          # optimize for size; proxy is I/O-bound
```

Target: `x86_64-unknown-linux-musl` (built via a rust:alpine or rust:musl builder image).

### Dockerfile

`images/sidecar/Dockerfile` gets rewritten:

```dockerfile
FROM rust:1.83-alpine AS builder
RUN apk add --no-cache musl-dev
WORKDIR /build
# Copy just the sidecar crate and minimal workspace context needed to build it
COPY Cargo.toml Cargo.lock ./
COPY sidecar/ sidecar/
# Stub out the other workspace members to avoid pulling them into the build
# (or use cargo build -p nautiloop-sidecar --locked)
RUN cargo build -p nautiloop-sidecar --release --target x86_64-unknown-linux-musl

FROM scratch
COPY --from=builder /build/target/x86_64-unknown-linux-musl/release/nautiloop-sidecar /auth-sidecar
COPY --from=builder /etc/ssl/certs/ca-certificates.crt /etc/ssl/certs/
ENTRYPOINT ["/auth-sidecar"]
```

**Note:** the Go binary is named `auth-sidecar`. The Rust binary keeps the same name at the container layer (`/auth-sidecar`) so K8s manifests, startup probes, and existing scripts don't need to change. Internally the crate is `nautiloop-sidecar`.

### SSH server implementation notes

Using `russh` the structure is:

```rust
struct GitSshServer {
    remote_host: String,         // e.g. "github.com:22"
    allowed_repo: String,        // e.g. "reitun/virdismat-mono.git"
    host_key: russh::keys::KeyPair,
}

#[async_trait]
impl russh::server::Server for GitSshServer { /* ... */ }

struct Session {
    remote_host: String,
    allowed_repo: String,
}

#[async_trait]
impl russh::server::Handler for Session {
    // auth_none: always accept (loopback only)
    // channel_open_session: accept
    // exec_request: parse command, validate, spawn client connection, pipe
    // env_request, pty_request, subsystem_request: return Err(rejected)
}
```

The client-side (connecting to `github.com:22` with the mounted key) uses `russh::client` with a `russh::client::Handler` that implements `check_server_key` against the `known_hosts` file. `russh::keys::known_hosts` has helpers for this — verify they're still in the crate during implementation; if not, parse manually.

### Repo URL parser

`git_url.rs` handles three shapes:

```rust
pub struct GitRemote {
    pub host: String,
    pub port: u16,
    pub repo_path: String,   // with leading slashes stripped
}

pub fn parse(url: &str) -> Result<GitRemote, GitUrlError>;
```

Match order (first match wins):
1. `ssh://[user@]host[:port]/path` — use `url::Url::parse`
2. `user@host:path` — scp-style, split on `@` and `:`
3. `https://host/path` — use `url::Url::parse`, port defaults to 22

This mirrors the Go `extractGitRemote` function.

### SSRF module

`ssrf.rs` is a pure function: given a hostname, resolve via `tokio::net::lookup_host` and return `Err(SsrfError::PrivateIp(ip))` if any resolved IP is private. Reusable by both the model proxy and the egress logger.

```rust
pub async fn check_host_not_private(host: &str) -> Result<(), SsrfError>;
```

The private IP ranges are defined as `ipnet::IpNet` constants for compile-time correctness (no `parseCIDR` panics at startup).

## Migration Plan

Five phases, each independently revertable. The Go sidecar is deleted only in phase 5.

### Phase 1: Scaffold (one PR)
- Add `sidecar/` workspace member with `main.rs` that starts four stub HTTP servers (one per port) and flips readiness after 2s.
- Add CI job: `cargo build -p nautiloop-sidecar --target x86_64-unknown-linux-musl`.
- Add `sidecar/Dockerfile` producing a scratch image.
- Do not yet wire into K8s manifests or replace the Go image.
- **Ship criterion:** `cargo build` green, `cargo clippy` green, Docker image builds and runs (container stays up, `/healthz` returns 200 after 2s).

### Phase 2: Proxies implemented (one PR per concern, or one big PR)
- Implement model proxy (FR-1 to FR-4, FR-13).
- Implement egress logger (FR-11 to FR-14).
- Implement health endpoint (FR-15 to FR-17).
- Implement SSRF module, git URL parser, logging module.
- Unit tests for each module.
- **Ship criterion:** Rust sidecar passes unit tests. Still not wired into agent pods.

### Phase 3: Git SSH proxy (one PR)
- Implement git SSH server (FR-5 to FR-10).
- Implement upstream SSH client with known_hosts verification (SR-2).
- This is the highest-risk phase — isolate it.
- **Ship criterion:** unit tests for command validation, host key verification, repo path matching. Plus a manual smoke test against a real GitHub remote in a test env.

### Phase 4: Parity test harness (one PR, may be bundled with phase 3)
- Build `sidecar/tests/parity/` harness (see Test Plan).
- Both binaries run side by side, same inputs, diff outputs.
- CI job runs the parity suite on every PR touching `sidecar/`.
- **Ship criterion:** parity suite passes against commit `17b3a6a` of the Go sidecar.

### Phase 5: Cut over (one PR)
- Change `images/sidecar/Dockerfile` to build the Rust binary. (If kept in place) or switch the K8s manifest's image reference to the Rust-built image tag.
- Keep the Go source in tree for one release as a rollback option.
- Monitor production for one week.
- **Ship criterion:** one week of clean production run (no sidecar-related alerts, no user reports, no log anomalies).

### Phase 6: Deletion (one PR, gated on phase 5 + one-week bake)
- `rm -rf images/sidecar/main.go images/sidecar/main_test.go images/sidecar/go.mod images/sidecar/go.sum`
- Remove Go from CI.
- Remove Go from `cso` audit scope.
- Update `CLAUDE.md`, `README`, and any docs that mention Go.
- **Ship criterion:** green CI, no Go references left.

## Test Plan

### Unit tests (per module)

**`model_proxy.rs`:**
- `test_openai_route_injects_bearer_token`
- `test_anthropic_route_injects_x_api_key_and_version`
- `test_anthropic_respects_existing_anthropic_version_header`
- `test_unknown_route_returns_403`
- `test_credential_file_read_fresh_per_request` (mutate file between requests, assert second request sees new value)
- `test_passthrough_headers_preserved`
- `test_response_streamed_without_buffering` (assert server writes to client before upstream completes)

**`egress.rs`:**
- `test_http_get_forwarded_and_logged`
- `test_connect_tunneled_and_logged`
- `test_private_ip_blocked_returns_403`
- `test_log_line_schema_matches_go` (parse emitted JSON, assert field set)

**`ssrf.rs`:**
- `test_rfc1918_blocked` (10.0.0.0/8, 172.16.0.0/12, 192.168.0.0/16)
- `test_loopback_blocked` (127.0.0.0/8, ::1)
- `test_link_local_blocked` (169.254.0.0/16, fe80::/10)
- `test_ipv6_ula_blocked` (fc00::/7)
- `test_public_ip_allowed`
- `test_hostname_resolves_to_multiple_ips_any_private_blocks`

**`git_url.rs`:**
- `test_parse_scp_style` (`git@github.com:reitun/virdismat-mono.git`)
- `test_parse_ssh_url` (`ssh://git@github.com/foo/bar.git`)
- `test_parse_ssh_url_with_port` (`ssh://git@github.com:2222/foo/bar.git`)
- `test_parse_https_url` (`https://github.com/foo/bar.git`)
- `test_parse_invalid_returns_error`

**`git_ssh_proxy.rs`:**
- `test_rejects_non_session_channel`
- `test_rejects_env_request`
- `test_rejects_pty_request`
- `test_rejects_subsystem_request`
- `test_rejects_non_git_exec` (e.g. `ls /etc`)
- `test_accepts_git_upload_pack`
- `test_accepts_git_receive_pack`
- `test_rejects_mismatched_repo_path`
- `test_strips_quotes_and_leading_slash_from_repo_path`
- `test_refuses_missing_known_hosts`
- `test_refuses_empty_known_hosts`

**`health.rs`:**
- `test_healthz_returns_503_before_ready`
- `test_healthz_returns_200_after_ready`

### Integration tests

**`sidecar/tests/integration.rs`:**
- Spawn the full sidecar binary in a subprocess. Issue requests against localhost. Assert behavior.
- `test_all_four_ports_bind_within_2s`
- `test_readiness_file_written_after_ready`
- `test_sigterm_triggers_graceful_shutdown`
- `test_sigterm_during_active_ssh_session_waits_up_to_5s`

### Parity test harness

This is the most important piece. `sidecar/tests/parity/` contains a harness that:

1. Spins up the Go sidecar binary in one subprocess, the Rust sidecar in another, each with isolated credential files, each on a different port range (Go on 9090-9093, Rust on 9190-9193).
2. Issues a corpus of requests against both in parallel:
   - **Model proxy:** GET `/openai/v1/models`, POST `/openai/v1/chat/completions` (streaming), GET `/anthropic/v1/messages`, unknown paths, requests with/without existing auth headers, requests with/without `anthropic-version`.
   - **Egress:** CONNECT `github.com:443`, GET `http://example.com`, CONNECT to a hostname resolving to 127.0.0.1 (SSRF), GET to 192.168.1.1 (SSRF).
   - **Git SSH:** simulate an agent SSH client issuing `git-upload-pack 'reitun/virdismat-mono.git'`, `git-receive-pack 'reitun/virdismat-mono.git'`, `git-upload-pack 'wrong/repo.git'`, `ls /etc`, `env` request, `pty-req` request, non-session channel.
   - **Health:** GET `/healthz` immediately (both should 503), wait 3s, GET `/healthz` (both should 200).
3. For each input, compares:
   - HTTP status code
   - Response body (normalized — upstream responses vary; what we care about is that both sidecars produced the same locally-generated error payloads, and both forwarded the same upstream response)
   - Emitted JSON log lines (normalize timestamps, compare field sets and values)
   - SSH exit codes
   - SSH stderr content (for rejection cases)
4. Fails the test if any diff is non-trivial.

**Intentional differences that are NOT diffs:**
- Process IDs in logs
- Timestamps
- Memory addresses in error messages (rare but possible)
- Exact wording of Rust vs. Go error messages for *fatal* errors (where the Rust version may be more descriptive) — parity is on the output schema, not the English content, for log fields marked `level=error`.

The harness prints a readable diff for any failing case and exits non-zero.

### Smoke test (manual, pre-phase-5)

Before cutting over in phase 5, run against a real nautiloop instance:
- [ ] `nemo harden specs/foo.md` runs end-to-end against the Rust sidecar.
- [ ] Agent successfully pulls + pushes to GitHub through the SSH proxy.
- [ ] Agent successfully hits Claude API through the model proxy (both streaming and non-streaming).
- [ ] Agent successfully hits OpenAI API through the model proxy (reviewer role).
- [ ] `/healthz` returns 200 after startup.
- [ ] Kill the sidecar pod mid-loop — K8s restarts it cleanly, agent recovers.

## Security Considerations

The sidecar is the most security-sensitive component in the project. It sits between agent containers (which run arbitrary LLM-generated code) and model providers / GitHub. Getting the rewrite wrong means leaking credentials or allowing internal network pivot.

### Non-negotiables (no reviewer leniency)

1. **Host key verification is mandatory.** `check_server_key` must return `Err` if the presented key is not in `/secrets/ssh-known-hosts/known_hosts`. The Go implementation hardened this recently — *do not regress*.
2. **SSRF protection runs before any network call.** DNS rebind is mitigated by resolving once and checking all IPs; the resolved IP is then passed to the dialer, not the hostname.
3. **Credential files are read fresh per request.** Even a 1-second cache is a regression: it means rotated credentials take up to 1s to take effect, and it means a memory-reading attack has a longer window.
4. **Only `/openai/*` and `/anthropic/*` routes.** No wildcard routing. A future "gemini" path is a separate commit, not a regex expansion.
5. **`git-upload-pack` and `git-receive-pack` only.** Shelling out `ls`, `env`, or any other command must be impossible. Reject at the request-type level.
6. **No `InsecureIgnoreHostKey` equivalent anywhere in the codebase.** `grep` for it in review.

### New risks introduced by the rewrite

1. **`russh` vs. `x/crypto/ssh` divergence.** `russh` is well-maintained but has a smaller production footprint. A subtle protocol mismatch could make git operations silently misbehave — e.g. wrong cipher negotiation, wrong channel flow control. The parity test harness is the main mitigation; the smoke test is the backup.
2. **`rustls` vs. Go TLS divergence.** The model proxy uses TLS to reach OpenAI/Anthropic. `rustls` is strict about certificate validation in ways Go's `crypto/tls` sometimes isn't. Bundle `webpki-roots` and verify in phase 2.
3. **Tokio runtime panics.** A panic in any task can bring down the whole runtime if not caught. Use `tokio::spawn` with explicit error handling, not panic-on-error.
4. **Supply chain surface.** The dependency list in Architecture is long. Every dep needs to land in the `cso` audit scope. Keep the list as tight as possible — drop anything that doesn't pull its weight during implementation.

### What this rewrite does NOT change

- File paths: `/secrets/model-credentials/*`, `/secrets/ssh-key/id_ed25519`, `/secrets/ssh-known-hosts/known_hosts`, `/tmp/shared/ready` — all identical.
- Environment variables: only `GIT_REPO_URL` is required.
- Port numbers: 9090, 9091, 9092, 9093 — identical.
- Log schemas (FR-14, FR-20) — identical.
- K8s manifest: no change needed if the image tag swap is invisible to the spec.

## Out of Scope

- **Gemini route.** The spec locks `/openai/*` and `/anthropic/*`. Gemini support is a future spec.
- **Shared types crate.** A `nautiloop-types` workspace crate containing log schemas, config types, etc., is a natural next step but is a separate spec.
- **Telemetry / metrics.** The sidecar today emits only logs. Prometheus metrics are a separate spec.
- **Per-request credential files.** The sidecar reads from fixed paths. Dynamic credential routing (e.g. per-model keys, per-engineer keys) is a separate spec.
- **Performance optimization beyond the NFR targets.** If the Rust sidecar is 3x faster than the Go one, great; if it's the same speed, also fine. We are not rewriting to get faster, we are rewriting to consolidate.
- **Moving the sidecar out of the workspace.** Some rewrites in this shape prefer a separate repo to keep the sidecar's supply chain isolated. We keep it in-workspace for the shared clippy gate and shared CI. Revisit only if CI build times become a problem.

## Open Questions

1. **Does `russh` support the exact set of ciphers/kex/MACs that GitHub expects?** Needs to be validated in phase 3 against `ssh -v git@github.com` output from the current Go sidecar. If there's a mismatch, the parity suite will catch it — but it could mean a russh config tweak or a version bump.
2. **Is there a way to share the Docker build cache between Rust workspace members?** `cargo build -p nautiloop-sidecar` in the Dockerfile may rebuild a lot of shared deps from scratch. The existing control-plane Dockerfile has patterns for this — check if they apply here.
3. **Should phase 5 use a feature flag at the K8s level (two image tags in rotation) or a hard cutover?** I lean hard cutover with fast rollback (keep the Go tag pushed, switch back via K8s manifest). Two images in rotation during a migration is extra complexity for a single-replica sidecar. **Not blocking — decide in phase 5 review.**
4. **Does the parity harness need to cover the readiness file path?** Phase 1 stub writes it; phase 2 actual impl writes it. Harness can just assert the file exists after startup rather than racing with the Go version. **Not blocking.**

None of the above are blocking. The spec is implementable as written.
