# Rust Rewrite: Auth Sidecar

## Overview

Rewrite `images/sidecar/` from Go to Rust as a new workspace crate. Goal: behavior-parity replacement gated on a containerized parity test harness that runs both binaries against the same hermetic inputs. Consolidates the codebase on one language, one toolchain, one security review surface.

**Baseline:** parity is against the Go binary at the merge of PR #56 (`faaf732`, the health-bind-to-all-interfaces fix) on top of `17b3a6a`. Rebase the spec baseline to whatever tag contains the fix at implementation time.

## Problem Statement

Nautiloop is a Rust project with one exception: `images/sidecar/` (862 lines of Go post-#56). The sidecar was originally Go because `net/http` + `httputil.ReverseProxy` and `golang.org/x/crypto/ssh` made the HTTP and SSH proxies easy, and goroutines + blocking I/O are simpler than Rust async for a proxy workload.

All of those reasons were valid. None are compelling enough to justify the cost of maintaining two languages.

### Costs of the split today

- **Two supply chains.** Cargo + Go modules. `cso` security audits walk both. CI installs and caches both toolchains.
- **Two test runners.** `cargo test --workspace` does not cover the sidecar. Someone has to remember to also run `go test ./images/sidecar/`. When they forget, regressions land silently.
- **No shared types.** The sidecar has its own `egressLogEntry` struct. Any schema change has to be manually kept in sync with any control-plane code that parses those logs.
- **Clippy gate doesn't cover it.** `cargo clippy --workspace -- -D warnings` is one of the highest-leverage rules in this repo. The sidecar is exempt.

### Why this is doable in Rust now

- **`russh` 0.60** is current, tokio-native, actively maintained, and has everything the Go implementation uses: server-side SSH, channel forwarding, custom authentication hooks, custom host-key verification, `PrivateKey` and known-hosts helpers in `russh::keys`. Requires one of `ring` or `aws-lc-rs` as a crypto backend feature.
- **`hyper` 1.x** + **`hyper-util`** cover both proxies with explicit control over streaming semantics (required for SSE passthrough and CONNECT tunneling). Note: the default `HttpConnector` cannot be used directly — see SSRF architecture below.
- **`rustls` + `webpki-roots`** give a pure-Rust TLS stack — no OpenSSL, musl-friendly, no dynamic linker surprises. Acknowledged tradeoffs in Security Considerations below.
- **`tokio` + musl target** produce a static binary in the same size class as the Go one.

### Bugs in the Go implementation to FIX (not preserve) in the rewrite

Codex adversarial review surfaced three behavior bugs that are present in the Go source. The Rust version must fix all of them; parity is "behavior-identical except for these documented fixes":

1. **SSRF fail-open on DNS lookup error.** Go wraps the private-IP check in `if err == nil` at `main.go:117`, `main.go:210`, and `main.go:278`. If `net.LookupIP` returns an error, the SSRF check is skipped entirely and the dial proceeds. The Rust version MUST fail-closed: DNS lookup failure returns HTTP 502 with a warning log, does not dial.
2. **DNS rebinding window.** Both Go and the original spec resolve the hostname for SSRF, then re-dial by hostname. Between the check and the dial, DNS can return a different IP. The Rust version MUST resolve once, pass the vetted `SocketAddr` to the dialer, and never redial by hostname. **Implementation requires a custom hyper connector — see Architecture.**
3. **Bare `git-upload-pack` bypasses repo path validation.** Go only validates the repo path when `cmdParts` has exactly two parts (`main.go:479`). If an SSH client issues `git-upload-pack` with no argument (or `git-receive-pack` with no argument), the repo allowlist is not checked. The Rust version MUST reject any git command without a repo path argument with exit status 1.

These are the only three intentional parity deviations. Every other behavior is frozen.

## Dependencies

- **Requires:** PR #56 merged to main (health bind to all interfaces). Without it, the Go binary in the parity harness can't be probed by a Docker healthcheck either, so the harness design below doesn't work.
- **Enables:** single-language codebase, single clippy gate, shared types crate option in the future.
- **Blocks:** nothing.

## Requirements

### Functional Requirements — Behavior Parity

The Rust sidecar shall be behavior-identical to the Go implementation at the post-#56 merge commit, except for the three documented SSRF and bare-exec fixes above.

#### Model API Proxy (127.0.0.1:9090)

- FR-1: Route `/openai/*` AND bare `/openai` to `https://api.openai.com`, trimming the `/openai` prefix to produce the upstream path (bare `/openai` maps to upstream `/`). Inject `Authorization: Bearer <key>`.
- FR-2: Route `/anthropic/*` AND bare `/anthropic` to `https://api.anthropic.com`, trimming the `/anthropic` prefix. Inject `x-api-key: <key>` AND a default `anthropic-version: 2023-06-01` if the request does not already provide an `anthropic-version` header.
- FR-3: All other paths return HTTP 403 with body `{"error":"only /openai/* and /anthropic/* routes are supported"}`.
- FR-4: Credential files at `/secrets/model-credentials/openai` and `/secrets/model-credentials/anthropic` are read fresh on each request. Content is trimmed with full whitespace trimming (equivalent to Go `strings.TrimSpace`: strips leading AND trailing whitespace including ASCII space, tab, newline, carriage return, form feed, vertical tab). No in-memory caching.
- FR-5: Pass through all request headers from the agent to upstream unchanged, then overwrite `Authorization` (OpenAI) or `x-api-key` (Anthropic) with the injected value. Any client-supplied value of those specific headers is replaced.
- FR-6: Stream the response body from upstream to client without buffering. No `Content-Length` rewrite. Required for SSE streaming.
- FR-7: Perform SSRF check on the upstream hostname before dialing. See FR-18 for fail-closed semantics and the "resolve once, pass IP to dialer" requirement. **Implementation must use a custom hyper connector; the default `HttpConnector` performs its own DNS resolution and cannot satisfy FR-18.**

#### Git SSH Proxy (127.0.0.1:9091)

- FR-8: Accept SSH connections on loopback. Authentication is accepted via the russh `auth_none` handler returning `Auth::Accept`. Safe because the listener is loopback-only and the pod is single-agent. Generate an ephemeral Ed25519 host key on startup; never persist it.
- FR-9: Reject all global SSH requests by returning `Ok(false)` from the russh server `Handler` methods `tcpip_forward`, `cancel_tcpip_forward`, and any other global request handler russh 0.60 exposes. This is the Rust equivalent of Go's `ssh.DiscardRequests(reqs)` at `main.go:430`. Verify at implementation time that russh 0.60's `Handler` trait has no additional global-request methods; any unenumerated method is an attack surface.
- FR-10: Accept only `session` channels. Other channel types are rejected by returning `Ok(false)` from `channel_open_direct_tcpip`, `channel_open_forwarded_tcpip`, and any other `channel_open_*` handler that is not `channel_open_session`.
- FR-11: On a session channel, only `exec` requests with command `git-upload-pack <path>` or `git-receive-pack <path>` are accepted. Both command name AND repo path argument are required. Malformed exec payloads (missing length prefix, truncated) are rejected by calling `session.channel_failure(channel)` and the channel is closed without sending exit-status, matching Go behavior at `main.go:456`.
- FR-12: `env`, `pty-req`, `subsystem`, and all other channel request types are rejected by calling `session.channel_failure(channel)`. **No exit-status is sent for these** — matching Go behavior at `main.go:500`. Exit status 1 is sent via `session.exit_status_request(channel, 1)` only when:
  - The command name is not in the allowlist (`main.go:471`)
  - The repo path does not match the allowlist (`main.go:487`)
  - The upstream SSH session fails for any reason
- FR-13: Repo path validation. The requested repo path is extracted by splitting the command on the first space, then trimming `'`, `"`, and spaces from the argument, then trimming a leading `/`. The allowed repo path (from `GIT_REPO_URL`) is trimmed of a leading `/` only. Compare the two normalized strings for equality. **Reject the request if the command has no argument** (the fix for the Go bare-exec bypass bug).
- FR-14: Upstream SSH connection always authenticates as user `git`, regardless of any userinfo in `GIT_REPO_URL`. The upstream host is derived from `GIT_REPO_URL` (see FR-24). The upstream port defaults to 22 if not specified.
- FR-15: Upstream SSH host key is verified against `/secrets/ssh-known-hosts/known_hosts`. Missing file → hard failure with exit status 1 and error log. Empty file → hard failure with exit status 1 and error log. No `InsecureIgnoreHostKey` fallback.
- FR-16: Pipe stdin/stdout/stderr bidirectionally between the agent's channel and the upstream SSH session for the duration of the git command. Propagate the upstream exit status back to the agent channel.

#### Egress Logger (127.0.0.1:9092)

- FR-17: Implement HTTP `CONNECT` for HTTPS tunneling AND plain HTTP forwarding. HTTP CONNECT hijacks the TCP stream and pipes bytes bidirectionally. Plain HTTP: repairs missing `URL.scheme` to `http` and missing `URL.host` to the `Host` header value (matching Go at `main.go:269`), strips only `Proxy-Connection` and `Proxy-Authorization` request headers, forwards the request, and disables redirect following (matching Go at `main.go:303`).
- FR-18: **SSRF protection (fail-closed, resolve-once).** Before any outbound TCP dial from either the model proxy or the egress logger:
  1. Resolve the destination hostname via `tokio::net::lookup_host` to a set of `SocketAddr`.
  2. If lookup returns an error or zero addresses, return HTTP 502 (model proxy) or HTTP 502 (egress) with a warning log. **Do not dial.** (This fixes the Go `if err == nil` bug.)
  3. If any returned IP is in RFC1918 (`10.0.0.0/8`, `172.16.0.0/12`, `192.168.0.0/16`), link-local (`169.254.0.0/16`, `fe80::/10`), loopback (`127.0.0.0/8`, `::1`), or IPv6 ULA (`fc00::/7`), return HTTP 403 with a warning log. **Do not dial.**
  4. Pick one non-private `SocketAddr` from the resolved set and pass it directly to the dialer. **Do not redial by hostname.** (This fixes the DNS rebinding window.)

  **For HTTPS clients** (model proxy): implementation MUST use a custom connector that accepts the resolved `SocketAddr` while preserving the original hostname for TLS SNI, HTTP `Host` header, and certificate verification. See Architecture.

  **For plain TCP dials** (egress CONNECT, upstream SSH): implementation dials `tokio::net::TcpStream::connect(socket_addr)` directly.

- FR-19: Egress logger emits one JSON line per completed request to stdout. The `destination` field MUST match Go behavior exactly:
  - For `CONNECT`: `destination` is `<host>:<port>`; if the incoming `CONNECT <host>` has no port, synthesize `:443` (matches Go at `main.go:203-206`).
  - For plain HTTP: `destination` is the raw `URL.Host` string after scheme/host repair (matches Go's `r.URL.Host` at `main.go:324`). This is typically `host` without a port when the client used the default HTTP port. **Do not synthesize a port for plain HTTP.**

  Exact schema:
  ```json
  {"timestamp":"<RFC3339Nano UTC>","destination":"<host[:port]>","method":"<METHOD>","bytes_sent":<int64>,"bytes_recv":<int64>,"prefix":"NAUTILOOP_SIDECAR"}
  ```

  Field names, types, and the literal `prefix` value are frozen — this is ABI for downstream log parsers. `tracing-subscriber`'s default JSON formatter does NOT produce this shape. Implementation uses a hand-rolled `serde_json::to_string` on a `struct EgressLogEntry` with `#[serde]` attributes, written directly to stdout (not via tracing).

#### Health Endpoint (`:9093`, all interfaces)

- FR-20: **The health endpoint shall bind `0.0.0.0:9093` (all interfaces), NOT `127.0.0.1:9093`.** The kubelet startup probe connects via the pod IP, not loopback. This is the opposite asymmetry from the proxy ports. Any future reader tempted to "fix" this asymmetry by moving /healthz to loopback should read PR #56 first. The model proxy (9090), git SSH proxy (9091), and egress logger (9092) stay on 127.0.0.1 because they are pod-internal interfaces and wildcard binding would expand the attack surface for no benefit.
- FR-21: `/healthz` handler. Until all four ports are confirmed listening, return `503` with body `{"status":"starting"}` and `Content-Type: application/json`. After readiness is flipped, return `200` with body `{"status":"ok"}`. **The handler accepts any HTTP method** (GET, HEAD, POST, etc.) — matching Go's `mux.HandleFunc("/healthz", healthHandler)` which does not method-check (`main.go:729`). Other paths return 404 via the default mux. Readiness verification probes loopback for 9090/9091/9092 and loopback for 9093 (the wildcard listener also accepts on 127.0.0.1).
- FR-22: On startup, after all four listeners are bound, verify readiness by dialing each port: `127.0.0.1:9090`, `127.0.0.1:9091`, `127.0.0.1:9092`, `127.0.0.1:9093`. 100ms timeout per dial, up to 100 retries at 20ms intervals. If any port fails within 2s, exit non-zero with a plain stderr message (not JSON — matches Go `log.Fatalf` behavior; see FR-25).
- FR-23: After readiness verification passes, flip the `ready` atomic AND write a readiness file at `/tmp/shared/ready` with content `ready` and mode `0644`, creating `/tmp/shared` with mode `0755` if missing. Belt-and-braces with the kubelet startup probe.

#### Configuration and Lifecycle

- FR-24: Parse `GIT_REPO_URL` from environment at startup. Missing or unparseable URL exits non-zero with a plain stderr message. Parser handles three formats: `ssh://[user@]host[:port]/path`, `user@host:path` (scp-style), and `https://host/path`. Derived fields: `host` (string), `port` (u16, default 22), `repo_path` (string, leading slashes stripped). The upstream SSH destination is `host:port`. The repo allowlist for FR-13 is `repo_path`. Edge cases: reject URLs containing control characters (`\t`, `\n`, `\r`) or percent-encoded forms that would change the host; any such URL is treated as unparseable and causes a fatal startup error.
- FR-25: **Fatal startup errors are plain stderr messages, not JSON.** This matches Go `log.Fatalf` at `main.go:703`, `:707`, `:753`, `:760`, `:799`, `:814`. Everything emitted AFTER startup is JSON per FR-19 and FR-26.
- FR-26: Non-egress logs (startup, shutdown, errors, warnings, info) are JSON lines on stdout with this exact schema:
  ```json
  {"timestamp":"<RFC3339Nano UTC>","level":"<info|warn|error>","message":"<text>","prefix":"NAUTILOOP_SIDECAR"}
  ```
  Frozen ABI. Hand-rolled serialization like FR-19, not `tracing-subscriber`.
- FR-27: On `SIGTERM` or `SIGINT`, initiate graceful shutdown in this order:
  1. Stop accepting new SSH connections (close the listener).
  2. Stop accepting new HTTP connections on model proxy, egress logger, and health endpoint (call `hyper::server::Server::shutdown` or equivalent graceful stop). **`/healthz` continues to return 200** throughout the drain — matching Go. Do NOT drop readiness on shutdown; see "Rejected improvement" note in Architecture.
  3. Wait for in-flight SSH sessions AND in-flight CONNECT tunnels to complete, up to 5 seconds total. Both types of long-lived connections are counted in a single wait group. **This is an improvement over Go, which only tracks SSH** — the Go CONNECT path hijacks the socket at `main.go:229` but the shutdown path at `main.go:848` does not count it.
  4. If the 5-second deadline is exceeded, log a warning (`"SSH/CONNECT drain timed out, proceeding with shutdown"`) and exit.

- FR-28: **Timeouts and streaming behavior (parity with Go).** The implementation freezes the following timeout and half-close behaviors:
  - **Model proxy upstream client:** no request timeout (`Timeout: 0` in Go at `main.go:160-164`). This is required for SSE streaming of long chat completions; a timeout would cut responses mid-stream. Implement with hyper's default (no timeout) on the request layer; connection-level TCP keepalive is fine.
  - **Egress CONNECT dial:** 10s connect timeout (`main.go:221-225`).
  - **Egress plain HTTP dial:** no explicit timeout in Go (framework default via `http.Client`). Rust matches: no explicit timeout on the hyper client.
  - **Upstream Git SSH dial:** 10s connect timeout (`main.go:575-582`).
  - **CONNECT half-close sequence:** after `client → upstream` EOF, call `CloseWrite` (TCP half-close) on the upstream connection, wait for the `upstream → client` copy to complete, then close the upstream connection (matches Go at `main.go:247-258`). Rust uses `tokio::io::AsyncWriteExt::shutdown()` on the write half at EOF.
  - **Request body size limits:** none (parity with Go, which has no body limits on any proxy). Accepted tradeoff: a buggy or malicious agent can send arbitrarily large request bodies. Future hardening is out of scope for this spec; tracked as a separate hardening issue.

### Non-Functional Requirements

- NFR-1: **No behavioral regressions.** The containerized parity test harness (see Test Plan) must pass before the Go implementation is removed. The three documented bug fixes are excluded from parity checks — the harness explicitly covers them as divergence tests.
- NFR-2: **Binary size ≤ 25 MB.** Measured on `x86_64-unknown-linux-musl` `--release` with `strip = true`, `lto = "fat"`, `codegen-units = 1`. The Go version is ~10 MB; Rust with musl + aggressive optimization typically lands at 15–20 MB. 25 MB is the hard ceiling.
- NFR-3: **Startup time ≤ 500ms to `ready=true`.** Measured from process start to the readiness file being written. The Go version is typically <100ms. We allow a 5x budget for Rust async runtime init and rustls root loading.
- NFR-4: **Memory RSS ≤ 50 MB steady-state** under idle. Go version is ~8 MB RSS idle. 50 MB is generous for tokio + rustls + russh.
- NFR-5: **Zero runtime dependencies.** Final image is `FROM scratch` with only the compiled binary and a CA certs bundle. No libc, no shell, no package manager.
- NFR-6: **No panic paths in request handlers.** Every `unwrap`, `expect`, `panic!`, and `unimplemented!` in request-serving code is a bug. Startup and config parsing may abort on fatal errors (matches FR-25). Request handlers propagate errors as HTTP responses / SSH exit statuses. See the panic profile decision in Architecture.
- NFR-7: **Log format stability.** FR-19 and FR-26 schemas are ABI. Any change requires a version bump and a coordinated parser update.
- NFR-8: **Clippy clean.** `cargo clippy -p nautiloop-sidecar --all-targets -- -D warnings` is green before merge.
- NFR-9: **`cargo-deny` clean.** Workspace gains a `deny.toml` that bans yanked crates, enforces a license allowlist, and fails on any advisory in `rustsec/advisory-db`. Runs in CI on every PR touching `sidecar/`. See Security Considerations for the list of currently-known advisories that force version pins.

### Security Requirements

- SR-1: Ephemeral Ed25519 SSH host key. Generated on each startup via the OS CSPRNG (`ring::rand::SystemRandom` or `OsRng` from `rand::rngs`). Never persisted to disk, never logged.
- SR-2: Upstream SSH host key verification is mandatory against `/secrets/ssh-known-hosts/known_hosts`. Missing or empty file → hard refusal. No "verify on first use," no bypass flag.
- SR-3: Model credential files read fresh per request. No in-memory cache across requests.
- SR-4: Model proxy accepts only `/openai*` and `/anthropic*` (with or without trailing `/` + path). Other paths return 403 before any network activity.
- SR-5: SSRF check runs before any outbound dial from model proxy or egress. Fails closed on DNS error. Passes resolved `SocketAddr` to dialer, not hostname. Model proxy uses a custom connector that preserves the hostname for SNI/Host while dialing the vetted IP. See FR-18 and Architecture.
- SR-6: SSH env, pty-req, subsystem, and all other channel request types rejected at the request-type level via `session.channel_failure(channel)`. Global SSH requests (tcpip_forward et al.) rejected via `Ok(false)` returns in FR-9.
- SR-7: Repo path validation at FR-13 rejects commands with no path argument (fix for the Go bare-exec bypass bug).
- SR-8: Ed25519 RNG is OS CSPRNG. No `StdRng::from_seed`, no constant seeds.
- SR-9: All dependencies gated by `cargo-deny`. Version pins for tracing-subscriber and rustls-webpki to clear known advisories (see Security Considerations).
- SR-10: **Extra CA bundle is test-only.** The `NAUTILOOP_EXTRA_CA_BUNDLE` env var (see Architecture) is honored only by the parity test harness orchestration. Production container images must not set this env var; production K8s manifests must not set this env var. A CI lint or release script check verifies no production manifest references the variable.

## Architecture

### Workspace layout

```
.
├── cli/
├── control-plane/
└── sidecar/                    ← NEW
    ├── Cargo.toml
    ├── deny.toml               ← cargo-deny config
    ├── src/
    │   ├── main.rs             # startup, readiness verification, shutdown
    │   ├── model_proxy.rs      # FR-1 to FR-7 (uses custom connector)
    │   ├── git_ssh_proxy.rs    # FR-8 to FR-16
    │   ├── egress.rs           # FR-17 to FR-19
    │   ├── health.rs           # FR-20 to FR-23
    │   ├── ssrf.rs             # FR-18 (resolve-once, fail-closed)
    │   ├── ssrf_connector.rs   # Custom hyper Service<Uri> for model proxy
    │   ├── tls.rs              # rustls config + optional extra CA bundle
    │   ├── git_url.rs          # FR-24
    │   └── logging.rs          # FR-19, FR-26 (hand-rolled JSON)
    └── tests/
        ├── unit/
        └── parity/             # containerized, see Test Plan
```

Crate name: `nautiloop-sidecar`. Binary name at the container layer stays `/auth-sidecar` so K8s manifests and existing scripts don't need to change.

### Dependency list

Pinned in the spec so reviewers can see the supply chain surface. Version constraints are minimums to clear known advisories; implementation can pin tighter.

```toml
[dependencies]
tokio = { version = "1.40", features = ["rt-multi-thread", "macros", "net", "io-util", "signal", "sync", "time", "fs"] }
hyper = { version = "1.4", features = ["server", "client", "http1"] }
hyper-util = { version = "0.1", features = ["tokio", "server", "client-legacy"] }
http-body-util = "0.1"

# SSH server + client. russh 0.60 requires a crypto backend feature.
# Pick ring (smaller, more common) over aws-lc-rs (FIPS-capable, larger).
russh = { version = "0.60", default-features = false, features = ["ring"] }

# TLS for the model proxy upstream connections.
rustls = { version = "0.23", default-features = false, features = ["ring", "std"] }
rustls-webpki = ">=0.103.10"  # RUSTSEC-2026-0049 fix; verify latest at impl time
tokio-rustls = "0.26"
webpki-roots = "0.26"

# JSON log serialization and config parsing.
serde = { version = "1", features = ["derive"] }
serde_json = "1"

# Error types.
thiserror = "1"

# Tracing ONLY for internal instrumentation if ever needed — NOT for the
# FR-19/FR-26 log schemas. Those are hand-rolled serde_json. Version pin
# clears RUSTSEC-2025-0055.
tracing = "0.1"
tracing-subscriber = ">=0.3.20"

# OS CSPRNG for host key + random IDs.
rand = "0.8"

# URL parsing for GIT_REPO_URL and HTTP proxy paths.
url = "2"

# CIDR checks for SSRF private IP ranges.
ipnet = "2"
```

**Rationale for picks:**
- **`hyper` direct, not `axum`.** Model proxy is ~150 LOC; axum's router is overhead, and direct hyper gives precise control over streaming semantics (FR-6) and custom connector injection (FR-18 / SSRF architecture below).
- **`russh` 0.60** with `ring` backend. Current version, feature flag is mandatory. Known-hosts helpers and `PrivateKey` types are exposed via `russh::keys`; no separate `russh-keys` dependency needed. No `ed25519-dalek` — russh's own key types handle Ed25519.
- **`rustls` + `webpki-roots`** over `native-tls`. No OpenSSL, musl-friendly. Acknowledged tradeoff: `webpki-roots` ships a snapshot of Mozilla's root store and does not pick up OS-level CA trust modifications, OCSP, or CRLs. See Security Considerations.
- **`tracing-subscriber` is NOT used for the log ABI.** Spec originally claimed tracing-subscriber JSON output would be identical to Go; it is not. FR-19 and FR-26 schemas are hand-rolled with `serde_json` written directly to stdout.

### SSRF architecture (critical)

**The default `hyper_util::client::legacy::connect::HttpConnector` does its own DNS resolution from the `Uri` and cannot satisfy FR-18.** If used directly, the connector resolves hostnames independently of our SSRF check, creating both the fail-open and rebinding windows we are supposed to fix. The model proxy MUST use a custom connector.

**Custom connector design (`ssrf_connector.rs`):**

```rust
use std::net::SocketAddr;
use hyper::Uri;
use tokio::net::TcpStream;

/// A hyper connector that:
/// 1. Runs ssrf::resolve_safe(uri.host(), uri.port()) first.
/// 2. Dials the returned SocketAddr directly (no further DNS).
/// 3. Preserves the URI's hostname in the returned stream metadata so that
///    the TLS layer uses it for SNI and the HTTP layer uses it for the Host
///    header.
pub struct SsrfConnector {
    // no state; all logic is in the Service impl
}

impl tower::Service<Uri> for SsrfConnector {
    type Response = TcpStream;
    type Error = SsrfError;
    type Future = /* boxed future returning TcpStream */;
    fn call(&mut self, uri: Uri) -> Self::Future {
        let host = uri.host().expect("uri has host").to_string();
        let port = uri.port_u16().unwrap_or(443);
        Box::pin(async move {
            let socket_addr = ssrf::resolve_safe(&host, port).await?;
            TcpStream::connect(socket_addr).await
                .map_err(SsrfError::from)
        })
    }
}
```

The `SsrfConnector` is wrapped with `hyper_rustls::HttpsConnector` (or equivalent `tokio_rustls::TlsConnector` on top) that takes the TCP stream AND the original hostname for TLS SNI. Because the TLS layer still has the hostname string, certificate verification against `webpki_roots` works normally.

**For egress HTTP and CONNECT**, no custom hyper connector is needed; the egress implementation does direct `tokio::net::TcpStream::connect(socket_addr)` calls after running `ssrf::resolve_safe`. The HTTP plain-forwarding client can then be a custom thin wrapper around tokio streams rather than a full hyper client.

**Implementation note:** if hyper 1.x / hyper-util's Service trait shape differs from the sketch above at implementation time, the principle still stands: DO NOT use the default `HttpConnector`. Any connector that calls DNS after our SSRF check is a bug.

### TLS trust and the parity harness CA bundle

The parity harness needs both sidecars (Go and Rust) to trust a test CA so that `mock-openai` and `mock-anthropic` can serve TLS certs that validate. The Go sidecar does this naturally via `/etc/ssl/certs/ca-certificates.crt`, which is OS-managed and easy to replace at image build time.

The Rust sidecar uses `rustls` + `webpki-roots`, which ships a compiled-in snapshot of Mozilla's root store and **ignores** `/etc/ssl/certs/ca-certificates.crt` at runtime. Simply baking a test CA into the image's filesystem does not work.

**Solution: optional extra CA bundle via env var.**

`tls.rs` constructs the rustls `ClientConfig` as follows:

```rust
let mut root_store = rustls::RootCertStore::empty();
root_store.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());

// Test harness hook: extra CA bundle loaded from env var.
// Production images MUST NOT set this env var (enforced by SR-10).
if let Ok(extra_path) = std::env::var("NAUTILOOP_EXTRA_CA_BUNDLE") {
    let pem = std::fs::read(&extra_path)?;
    let certs = rustls_pemfile::certs(&mut pem.as_slice())
        .collect::<Result<Vec<_>, _>>()?;
    for cert in certs {
        root_store.add(cert)?;
    }
}

let config = rustls::ClientConfig::builder()
    .with_root_certificates(root_store)
    .with_no_client_auth();
```

The parity harness `docker-compose.yml` sets `NAUTILOOP_EXTRA_CA_BUNDLE=/test-ca/ca.pem` on the `sidecar-rust` service and mounts the harness CA PEM at that path. The Go sidecar gets the same CA via a modified CA bundle in a test-specific Dockerfile stage. Neither production build includes the env var or the mount.

**SR-10 enforcement:** a CI check greps production K8s manifests and Helm charts for `NAUTILOOP_EXTRA_CA_BUNDLE`. Any match fails the build. A separate grep in the release script verifies that `/Dockerfile` (production) does not reference a test CA bundle.

### Panic profile

`sidecar/Cargo.toml`:

```toml
[profile.release]
lto = "fat"
codegen-units = 1
strip = true
panic = "unwind"          # NOT abort — see below
opt-level = "z"
```

**`panic = "unwind"` is required, not `abort`.** With `panic = "abort"`, any panic in any async task terminates the entire process immediately. `tokio::task::JoinHandle::is_panic()` and the error-boundary pattern in NFR-6 require unwinding. The ~500 KB binary size cost of unwind tables is acceptable inside the 25 MB ceiling.

NFR-6 still holds: panic paths in request handlers are bugs. The `unwind` setting is insurance, not a license to panic.

### SSRF module API

```rust
use std::net::SocketAddr;

#[derive(Debug, thiserror::Error)]
pub enum SsrfError {
    #[error("DNS lookup failed: {0}")]
    LookupFailed(String),
    #[error("hostname resolved to no addresses")]
    NoAddresses,
    #[error("hostname resolved to private IP: {0}")]
    PrivateIp(std::net::IpAddr),
}

/// Resolve the hostname, verify no resolved IP is private, and return a
/// single non-private SocketAddr to dial. Fail-closed on any error.
pub async fn resolve_safe(host: &str, port: u16) -> Result<SocketAddr, SsrfError>;
```

Every call site takes the returned `SocketAddr` and passes it to the dialer. The model proxy does so via the `SsrfConnector` described above. Egress HTTP and CONNECT call `resolve_safe` directly and pass the result to `tokio::net::TcpStream::connect`.

### Git SSH proxy implementation notes

The implementation is grounded in russh 0.60 but the spec does not sketch full handler code — concrete handler method signatures and return types are the implementer's job against the actual docs. The spec locks behavior, not API shape.

Key behavior anchors for the implementer:
- `auth_none` handler returns `Ok(Auth::Accept)` (FR-8). No password or public key auth.
- `tcpip_forward` and `cancel_tcpip_forward` handlers (and any other global-request handler russh 0.60 exposes on `russh::server::Handler`) return `Ok(false)` to reject the request at the wire level. See FR-9.
- `channel_open_session` returns `Ok(true)` to accept; `channel_open_direct_tcpip` and `channel_open_forwarded_tcpip` return `Ok(false)` to reject (FR-10).
- `exec_request` parses the command bytes, validates against the allowlist and repo path per FR-11 / FR-13, and either rejects (by calling `session.channel_failure(channel)` with no exit status, or by calling `session.exit_status_request(channel, 1)` for exec-mismatch or repo-mismatch) or spawns an upstream SSH client session piping bytes per FR-16.
- `env_request`, `pty_request`, and `subsystem_request` call `session.channel_failure(channel)` and return `Ok(())`. **No exit status is sent.** This matches Go at `main.go:500`.
- Upstream SSH client uses russh's own `PrivateKey` loading and `russh::keys::known_hosts` helpers (or manual `known_hosts` parsing if the helper is missing in 0.60). **No `InsecureIgnoreHostKey` bypass ever.**

**At implementation time: verify the russh 0.60 `Handler` trait does not expose additional global request methods beyond `tcpip_forward` and `cancel_tcpip_forward`.** Any unenumerated method is an attack surface. Grep the russh source or docs for `trait Handler` and diff against FR-9.

### Rejected improvement: readiness drop on shutdown

v2 of this spec proposed dropping `/healthz` to 503 at the start of shutdown ("gives kubelet a chance to stop sending probes cleanly"). **This was wrong for this sidecar and has been removed in v3.** Reasoning:

1. **This sidecar only has a `startupProbe`, not a liveness or readiness probe.** The startup probe runs once before the main container starts; it does not run during shutdown. Dropping readiness has no probe to signal to.
2. **The agent talks to the sidecar over localhost**, not via a K8s Service. The `EndpointSlice terminating` flow that readiness drops participate in applies only to Service-routed traffic. In-pod localhost traffic is unaffected.
3. **The practical effect** of dropping `/healthz` during shutdown is: any in-pod request that arrives in the drain window gets `503`. That's strictly worse than Go's behavior, which keeps the proxy ports listening until the drain completes.

Kept improvement: **CONNECT tunnel tracking in shutdown** (FR-27 step 3). Go has a real gap here — the CONNECT hijack at `main.go:229` is not counted in the SSH-only wait group at `main.go:848`. The Rust version tracks both.

If a future version adds a readiness or liveness probe to the sidecar, revisit this decision.

### Dockerfile

`images/sidecar/Dockerfile` gets rewritten:

```dockerfile
FROM rust:1.83-alpine AS builder
RUN apk add --no-cache musl-dev
WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY sidecar/ sidecar/
RUN cargo build -p nautiloop-sidecar --release --target x86_64-unknown-linux-musl --locked

FROM scratch
COPY --from=builder /build/target/x86_64-unknown-linux-musl/release/nautiloop-sidecar /auth-sidecar
# NOTE: rustls uses webpki-roots (compiled in). The CA bundle below is NOT
# consulted by the TLS layer at runtime. It is copied for consistency with
# the Go binary only and can be removed once the Go sidecar is deleted in
# phase 6. Do NOT rely on it for trust-store changes — use the
# NAUTILOOP_EXTRA_CA_BUNDLE env var (test-only, SR-10).
COPY --from=builder /etc/ssl/certs/ca-certificates.crt /etc/ssl/certs/
ENTRYPOINT ["/auth-sidecar"]
```

The Go binary is named `auth-sidecar`. The Rust binary keeps the same container-layer path (`/auth-sidecar`) so K8s manifests, startup probes, and existing scripts don't need to change. Internally the crate is `nautiloop-sidecar`.

## Migration Plan

Six phases, each independently revertable. The Go sidecar is deleted only in phase 6.

### Phase 0: Prerequisites (already merged)
- PR #56 (health bind to all interfaces) on main. **Required** — the parity harness cannot check the Go binary's health port from a Docker healthcheck without it.

### Phase 1: Scaffold
- Add `sidecar/` workspace member with `main.rs` that starts four stub servers and flips readiness after 2s.
- Add `deny.toml` and the cargo-deny CI job.
- Add `sidecar/Dockerfile` producing a scratch image.
- **Ship criterion:** `cargo build -p nautiloop-sidecar --target x86_64-unknown-linux-musl --locked` green, `cargo clippy` green, `cargo deny check` green, Docker image builds and a container passes `curl localhost:9093/healthz` after 2s.

### Phase 2: Proxies + logging module (no SSH yet)
- Implement model proxy (FR-1 to FR-7), SSRF module (FR-18), `SsrfConnector` (critical), and TLS config with extra-CA-bundle hook.
- Implement egress logger (FR-17 to FR-19, FR-28 timeouts).
- Implement health endpoint (FR-20 to FR-23).
- Implement logging module (FR-19, FR-26 hand-rolled).
- Implement git URL parser (FR-24).
- Unit tests for each module, including SSRF fail-closed and the custom connector.
- **Ship criterion:** all unit tests pass. No live-network dependencies in the test suite.

### Phase 3: Git SSH proxy
- Implement git SSH server (FR-8 to FR-16), including FR-28 10s upstream dial timeout.
- Implement upstream SSH client with known_hosts verification (SR-2).
- This is the highest-risk phase — isolate it.
- **Ship criterion:** unit tests for command validation, host key verification, repo path matching (including the bare-exec rejection), request type rejections, global request rejection. Plus a manual smoke test against a real GitHub remote in a test env.

### Phase 4: Parity test harness
- Build containerized parity harness with stubbed upstreams and `NAUTILOOP_EXTRA_CA_BUNDLE` wired up on both containers.
- CI job runs the parity suite on every PR touching `sidecar/`.
- **Ship criterion:** parity suite passes against the current Go binary. The three documented fixes (SSRF fail-closed, DNS rebinding resolve-once, bare-exec rejection) are validated as DIVERGENCE from Go in dedicated tests. The CONNECT drain improvement is validated as a DIVERGENCE (SIGTERM during active CONNECT: Rust waits up to 5s, Go drops the tunnel immediately).

### Phase 5: Cut over
- Change the K8s manifest's image reference to the Rust-built image tag (no Dockerfile swap in `images/sidecar/` yet — keep Go source in tree).
- Deploy to a test cluster, run a full `nemo harden` end-to-end.
- Monitor production for one week.
- **Rollback:** revert the image tag in the K8s manifest. Go image still pushed, instant rollback.
- **Loop-retry safety during cutover:** a K8s image tag swap terminates old pods and starts new pods. A loop that had an in-flight `git push` against the old sidecar receives a connection error when the old container SIGTERMs. The control plane must retry the git operation on the new sidecar. **Pre-phase-5 verification:** confirm the control-plane loop engine's retry semantics handle "sidecar terminated mid-operation" as a retryable error, not a loop-FAILED terminal state. File a followup issue if not.
- **Ship criterion:** one week of clean production run.

### Phase 6: Deletion
- Delete `images/sidecar/main.go`, `main_test.go`, `go.mod`, `go.sum`.
- Delete the Go Dockerfile.
- Remove Go from CI.
- Remove Go from `cso` audit scope.
- Delete the dead `/etc/ssl/certs/ca-certificates.crt` copy from the Rust Dockerfile (no longer needed once the Go path is gone).
- Update `CLAUDE.md`, `README`, and docs.
- **Ship criterion:** green CI, no Go references left.

## Test Plan

### Unit tests (per module)

**`model_proxy.rs`:**
- `test_openai_prefix_route_injects_bearer_token`
- `test_openai_bare_route_maps_to_upstream_root`
- `test_anthropic_prefix_route_injects_x_api_key_and_version`
- `test_anthropic_bare_route_maps_to_upstream_root`
- `test_anthropic_respects_existing_anthropic_version_header`
- `test_unknown_route_returns_403`
- `test_credential_file_read_fresh_per_request`
- `test_credential_file_leading_whitespace_trimmed`
- `test_credential_file_trailing_whitespace_trimmed`
- `test_passthrough_headers_preserved`
- `test_response_streamed_without_buffering`
- `test_uses_ssrf_connector_not_default_http_connector` (reflect or whitelist check)

**`ssrf_connector.rs`:**
- `test_connector_calls_resolve_safe_before_dial`
- `test_connector_dials_returned_socket_addr_not_hostname` (mock lookup returns 1.2.3.4, connector dials 1.2.3.4)
- `test_connector_preserves_hostname_for_tls_layer` (tls layer receives original hostname)
- `test_connector_fails_closed_on_dns_error`
- `test_connector_fails_closed_on_private_ip`

**`egress.rs`:**
- `test_http_get_forwarded_and_logged`
- `test_http_get_with_origin_form_url_repaired`
- `test_http_get_strips_proxy_connection_header`
- `test_http_get_strips_proxy_authorization_header`
- `test_http_get_does_not_follow_redirects`
- `test_http_log_destination_matches_go_url_host_without_port`
- `test_connect_tunneled_and_logged`
- `test_connect_log_destination_has_port_443_when_absent_from_request`
- `test_connect_half_close_sequence_matches_go`
- `test_connect_dial_timeout_10s`
- `test_private_ip_blocked_returns_403`
- `test_log_line_schema_matches_frozen_format`

**`ssrf.rs`:**
- `test_rfc1918_blocked`
- `test_loopback_blocked`
- `test_link_local_blocked`
- `test_ipv6_ula_blocked`
- `test_public_ip_allowed`
- `test_hostname_resolving_to_mixed_ips_any_private_blocks`
- `test_dns_lookup_error_fails_closed`
- `test_zero_addresses_returned_fails_closed`
- `test_resolved_socket_addr_is_returned_for_dialer`

**`tls.rs`:**
- `test_default_client_uses_webpki_roots_only`
- `test_extra_ca_bundle_env_var_loads_additional_cas`
- `test_extra_ca_bundle_env_var_missing_file_fails_startup`
- `test_extra_ca_bundle_env_var_not_set_in_production_images` (CI check, not a Rust unit test)

**`git_url.rs`:**
- `test_parse_scp_style`
- `test_parse_ssh_url`
- `test_parse_ssh_url_with_port`
- `test_parse_https_url`
- `test_parse_rejects_control_characters`
- `test_parse_rejects_percent_encoded_host`
- `test_parse_invalid_returns_error`
- `test_upstream_user_always_git_regardless_of_userinfo`

**`git_ssh_proxy.rs`:**
- `test_rejects_non_session_channel`
- `test_rejects_env_request_via_channel_failure`
- `test_rejects_pty_request_via_channel_failure`
- `test_rejects_subsystem_request_via_channel_failure`
- `test_rejects_tcpip_forward_returns_ok_false`
- `test_rejects_cancel_tcpip_forward_returns_ok_false`
- `test_rejects_non_git_exec_with_exit_status_1`
- `test_rejects_bare_git_upload_pack_without_repo_path` (FIX for Go bypass bug)
- `test_rejects_bare_git_receive_pack_without_repo_path` (FIX for Go bypass bug)
- `test_accepts_git_upload_pack_with_matching_repo`
- `test_accepts_git_receive_pack_with_matching_repo`
- `test_rejects_mismatched_repo_path_with_exit_status_1`
- `test_strips_quotes_and_leading_slash_from_requested_repo`
- `test_refuses_missing_known_hosts`
- `test_refuses_empty_known_hosts`
- `test_malformed_exec_payload_closes_channel_without_exit_status`
- `test_upstream_dial_timeout_10s`

**`health.rs`:**
- `test_healthz_returns_503_before_ready`
- `test_healthz_returns_200_after_ready`
- `test_healthz_does_not_drop_ready_on_shutdown` (v3 correction)
- `test_healthz_binds_all_interfaces_not_loopback`
- `test_healthz_accepts_any_http_method` (v3 correction: match Go)

**`logging.rs`:**
- `test_egress_log_schema_exact_fields`
- `test_egress_log_timestamp_is_rfc3339_nano_utc`
- `test_egress_log_destination_http_no_port`
- `test_egress_log_destination_connect_with_synthesized_port`
- `test_general_log_schema_exact_fields`
- `test_general_log_level_enum_matches_go`

### Integration tests

**`sidecar/tests/integration.rs`:** spawn the full sidecar binary in a subprocess with a stubbed environment, issue requests against localhost, assert behavior.

- `test_all_four_ports_bind_within_2s`
- `test_readiness_file_written_after_ready`
- `test_sigterm_closes_listeners_without_dropping_healthz`
- `test_sigterm_waits_for_connect_tunnel_drain`
- `test_sigterm_waits_for_ssh_session_drain`
- `test_sigterm_warns_and_exits_if_drain_exceeds_5s`
- `test_startup_fatal_error_is_plain_stderr_not_json`
- `test_extra_ca_bundle_loaded_when_env_var_set`

### Parity test harness (containerized, hermetic)

**This is the highest-leverage piece. The first draft of the spec proposed running both binaries in the same process on different port ranges, which does not work — the Go binary hardcodes ports and secret paths and cannot run twice side-by-side. The harness runs each binary in its own Docker container.**

Layout: `sidecar/tests/parity/`

```
parity/
├── docker-compose.yml       # five services
├── fixtures/
│   ├── test-ca/ca.pem       # harness CA, loaded via NAUTILOOP_EXTRA_CA_BUNDLE on rust
│   ├── go-secrets/
│   │   ├── model-credentials/openai
│   │   ├── model-credentials/anthropic
│   │   ├── ssh-key/id_ed25519
│   │   └── ssh-known-hosts/known_hosts
│   ├── rust-secrets/        # identical content, separate mount
│   ├── mock-openai/         # stub TLS HTTPS server signed by test-ca
│   ├── mock-anthropic/      # stub TLS HTTPS server signed by test-ca
│   └── mock-github-ssh/     # stub SSH server accepting git-upload-pack
├── Dockerfile.go-with-test-ca   # adds test-ca/ca.pem to /etc/ssl/certs/
├── corpus.json              # list of test inputs
└── harness.rs               # Rust binary that drives both containers
```

**Services:**
1. `sidecar-go` — Go binary built from `Dockerfile.go-with-test-ca` (test CA added to the system CA bundle that Go's `crypto/tls` uses). Exposes 9090-9093 on host ports 19090-19093. Mounts `go-secrets/` at `/secrets/`.
2. `sidecar-rust` — Rust binary from the standard Dockerfile. Exposes 9090-9093 on host ports 29090-29093. Mounts `rust-secrets/` at `/secrets/` AND `test-ca/` at `/test-ca/`. Env var `NAUTILOOP_EXTRA_CA_BUNDLE=/test-ca/ca.pem` is set via compose.
3. `mock-openai` — serves `api.openai.com` on the Docker network with TLS cert signed by `test-ca`.
4. `mock-anthropic` — same for `api.anthropic.com`.
5. `mock-github-ssh` — SSH server on the Docker network listening as `github.com:22`, accepting the harness-baked SSH key, responding to `git-upload-pack` and `git-receive-pack` with fixed pack data.

Both sidecar containers have their DNS overridden (via `extra_hosts` in compose) so `api.openai.com`, `api.anthropic.com`, and `github.com` resolve to the mock service IPs. **No live internet access.**

**Harness corpus covers:**

*Model proxy:*
- GET `/openai/v1/models` → expect Bearer injection, mock response passthrough
- POST `/openai/v1/chat/completions` with SSE stream → expect chunked response, no buffering
- GET `/anthropic/v1/messages` → expect x-api-key injection, anthropic-version default added
- GET `/anthropic/v1/messages` with client-provided anthropic-version → expect passthrough
- GET `/openai` (bare) → expect route to upstream `/`
- GET `/anthropic` (bare) → expect route to upstream `/`
- GET `/unknown` → expect 403 with exact Go error body
- Credential file mutation between requests → expect second request sees new value

*Egress:*
- CONNECT `github.com:443` → expect tunnel, log line schema match, destination = `github.com:443`
- CONNECT `github.com` (no port) → expect tunnel, destination = `github.com:443` (synthesized)
- GET `http://mock-example.docker` → expect forward, destination = `mock-example.docker` (no port)
- GET `http://mock-example.docker:8080/foo` → expect forward, destination = `mock-example.docker:8080`
- GET with `Proxy-Connection` header → expect header stripped from forwarded request
- GET with redirect response → expect no redirect following
- GET origin-form URL `/foo` with `Host: mock-example.docker` → expect scheme/host repaired
- CONNECT `mock-resolves-to-private-ip:443` → expect 403 SSRF block
- GET with DNS lookup error (mock DNS returns SERVFAIL) → **expect divergence**: Go = connects, Rust = 502. Harness asserts this is the documented fix.

*Git SSH:*
- `git-upload-pack 'reitun/virdismat-mono.git'` → expect proxy to mock-github-ssh
- `git-receive-pack 'reitun/virdismat-mono.git'` → expect proxy
- `git-upload-pack 'wrong/repo.git'` → expect exit status 1
- `git-upload-pack` (no arg) → **expect divergence**: Go = proxies through, Rust = exit status 1. Harness asserts fix.
- `ls /etc` → expect exit status 1
- `env` request → expect channel_failure, no exit status
- `pty-req` request → expect channel_failure, no exit status
- `tcpip-forward` global request → expect Ok(false)
- Non-session channel (direct-tcpip) → expect reject

*Health:*
- GET `/healthz` immediately after container start → expect 503
- HEAD `/healthz` → expect 503 (v3: method parity)
- POST `/healthz` → expect 503 (v3: method parity)
- Wait 3s, GET `/healthz` → expect 200
- Send SIGTERM, GET `/healthz` within 100ms → **expect parity**: both Go and Rust keep returning 200 until the listener actually closes (v3: readiness drop removed)
- Send SIGTERM during active CONNECT tunnel → **expect divergence**: Rust waits up to 5s for drain, Go drops the tunnel immediately (improvement)

**Comparison logic:**

For each input:
1. Fire the same request to both sidecars in parallel.
2. Capture HTTP status, response body, emitted log lines.
3. Normalize: strip timestamps from log lines, strip `Date:` response header, strip any volatile body fragments defined per-test.
4. Assert equality except for the four documented divergences (SSRF fail-closed on DNS error, DNS rebinding resolve-once, bare-exec rejection, CONNECT drain on SIGTERM), which are asserted in the opposite direction.

**Known limitations acknowledged in the harness:**
- Concurrent log line ordering under load cannot be compared line-by-line. The harness serializes requests for log comparison tests.
- Go `http.Error` emits a trailing newline; Rust hyper does not. Harness normalizes trailing whitespace in error body comparisons.
- Fatal error wording (the plain stderr messages from FR-25) is NOT compared — just the fact of non-zero exit + non-empty stderr.

### Smoke test (manual, pre-phase-5)

Before cutting over in phase 5, run against a real nautiloop cluster:
- [ ] `nemo harden specs/foo.md` runs end-to-end against the Rust sidecar image.
- [ ] Agent successfully pulls + pushes to real GitHub through the SSH proxy.
- [ ] Agent successfully hits Claude API through the model proxy (both streaming and non-streaming).
- [ ] Agent successfully hits OpenAI API through the model proxy (reviewer role).
- [ ] `/healthz` returns 200 after startup (verified via pod IP, not loopback).
- [ ] Kill the sidecar pod mid-loop — K8s restarts it cleanly, agent recovers.
- [ ] SIGTERM during an active CONNECT tunnel — drain completes within 5s.
- [ ] Verify control-plane retry semantics: induce a sidecar restart during an in-flight `git push`, confirm the loop retries and reaches a terminal state.

## Security Considerations

The sidecar is the most security-sensitive component in the project. It sits between agent containers (which run LLM-generated code) and model providers / GitHub. A mistake in the rewrite leaks credentials or enables internal network pivot.

### Non-negotiables (no reviewer leniency)

1. **Host key verification is mandatory** (SR-2). Missing/empty `known_hosts` is a hard refusal. `grep` for any `InsecureIgnoreHostKey` equivalent in code review.
2. **SSRF fails closed and passes SocketAddr, not hostname, to dialer** (FR-18, SR-5). The two Go bugs are fixed here. **The model proxy uses a custom hyper connector; the default `HttpConnector` is forbidden.** Code review must verify no use of `hyper_util::client::legacy::connect::HttpConnector`.
3. **Credential files read fresh per request** (SR-3). No cache, not even a 1-second cache.
4. **Only `/openai` and `/anthropic` routes** (SR-4). No wildcard, no regex.
5. **Only `git-upload-pack` and `git-receive-pack` commands, both requiring a repo path argument** (FR-11, FR-13, SR-7). Bare-exec bypass is fixed.
6. **All SSH global requests rejected** (FR-9, SR-6). tcpip_forward, cancel_tcpip_forward, etc.
7. **`NAUTILOOP_EXTRA_CA_BUNDLE` is test-only** (SR-10). Production manifests and production Dockerfiles do not reference it. CI lint enforces.

### Supply chain acknowledgments

The Rust rewrite trades Go's stdlib surface for a graph of third-party crates. Known concerns at spec time:

| Concern | Mitigation |
|---|---|
| `tracing-subscriber` has RUSTSEC-2025-0055 on older versions | Pin to `>=0.3.20`. `cargo-deny` in CI blocks regressions. Source: https://rustsec.org/advisories/RUSTSEC-2025-0055 |
| `rustls-webpki` has RUSTSEC-2026-0049 on older versions | Pin to `>=0.103.10`. Same CI enforcement. Source: https://rustsec.org/advisories/RUSTSEC-2026-0049 |
| `webpki-roots` does not pick up OS CA trust modifications, OCSP, or CRLs | Accepted tradeoff. The sidecar only trusts public model provider endpoints and public GitHub. If a cert gets revoked mid-lifecycle and we don't rebuild, traffic continues. Mitigation: rebuild sidecar image monthly via the existing CI release cadence. |
| `russh` has a small maintainer footprint | Accepted tradeoff. Offset by: active maintenance, tokio-native, and the fact that we only use the server-side SSH piece with a narrow request surface. |
| `ring` vs `aws-lc-rs` crypto backend choice | Pick `ring`. Smaller, more common in the Rust ecosystem, fewer build dependencies. `aws-lc-rs` is FIPS-capable but we have no FIPS requirement. |

`cargo-deny` config (`deny.toml`) enforces these at CI time. Any new advisory against any transitive dep fails the build until either pinned or patched.

### New risks specific to the rewrite

1. **`russh` vs `x/crypto/ssh` protocol divergence.** A subtle cipher negotiation or flow-control mismatch could make git operations silently misbehave against real GitHub. **Mitigation:** phase 3 manual smoke test against real github.com:22 with the current cipher set, AND phase 5 one-week production bake.
2. **`rustls` stricter cert validation.** rustls rejects some certificates Go's `crypto/tls` accepts (missing SAN, chains requiring path-building through intermediates not served by the endpoint). If OpenAI or Anthropic changes their cert chain in a way rustls rejects, the sidecar breaks for every agent. **Mitigation:** rebuild monthly (keeps webpki-roots current) AND add CI test that hits the real endpoints in a nightly job.
3. **Tokio task panics with `panic = "unwind"`.** We catch panics at the task boundary, but any dep that spawns its own runtime and panics could still crash us. **Mitigation:** NFR-6 (no panic paths in handlers) plus a `catch_unwind` wrapper around each request handler as belt-and-braces.
4. **Custom SSRF connector is the single point of failure for the SSRF fix.** If a future refactor swaps it out for the default hyper `HttpConnector`, we silently regress both bugs. **Mitigation:** a clippy-lint-level check or integration test that asserts the model proxy client's connector type, AND SR-5 review item above.

### What this rewrite does NOT change

- File paths: `/secrets/model-credentials/*`, `/secrets/ssh-key/id_ed25519`, `/secrets/ssh-known-hosts/known_hosts`, `/tmp/shared/ready` — identical.
- Environment variables: only `GIT_REPO_URL` is required in production. `NAUTILOOP_EXTRA_CA_BUNDLE` is test-only.
- Port numbers: 9090, 9091, 9092, 9093 — identical.
- Bind addresses: loopback for 9090/9091/9092, all interfaces for 9093 (after PR #56).
- Log schemas (FR-19, FR-26) — identical.
- K8s manifest: no change needed if the image tag swap is invisible.

## Out of Scope

- **Gemini route.** `/openai*` and `/anthropic*` only. Gemini is a future spec.
- **Shared types crate.** A `nautiloop-types` workspace crate for log schemas and config types is a natural next step but a separate spec.
- **Prometheus metrics.** Sidecar today emits only logs. Metrics are a separate spec.
- **Per-request credential files.** Fixed paths. Dynamic credential routing is a separate spec.
- **Performance optimization beyond NFR targets.** Not rewriting for speed. Rewriting to consolidate.
- **Moving the sidecar out of the workspace.** Keep it in-workspace for the shared clippy gate and shared CI.
- **Request body size limits.** Go has none; Rust matches. Future hardening is a separate issue.
- **The "SSH handshake failed: EOF" log line from early startup** (mentioned as followup in PR #56). Harmless kubelet probe noise on :9091. Cosmetic, separate cleanup.

## Open Questions

1. **Does russh 0.60 with the `ring` backend support the exact ciphers, kex, and MACs that GitHub accepts?** Phase 3 verifies against `ssh -vvv git@github.com` output from the current Go sidecar. If there's a mismatch, configure russh's algorithm list to match. Not blocking the spec — blocking phase 3 ship.
2. **Does russh 0.60 expose a `known_hosts` parse helper or do we parse manually?** Check at implementation time. Manual parse is ~30 lines if needed. Not blocking.
3. **Does russh 0.60's `Handler` trait expose global request methods beyond `tcpip_forward` / `cancel_tcpip_forward`?** Verify at implementation time per FR-9. Any additional method is an attack surface. Not blocking the spec — blocking phase 3 ship.
4. **Phase 5 cutover strategy.** Hard cutover (single image tag swap, fast rollback via manifest revert). Requires phase 5 pre-verification that the control-plane loop engine retries cleanly on mid-operation sidecar termination. Not blocking the spec — blocking phase 5 ship.

None of these are blocking. The spec is implementable as written, pending phase 3 validation of russh's cipher + API compatibility with real GitHub and phase 5 verification of loop-retry semantics.

## Changelog

### v1 → v2: First codex adversarial review (16 findings: 3 P0, 12 P1, 1 P2)

- P0-1 (health bind): FR-20 → `:9093` wildcard, cites PR #56
- P0-2 (SSRF): FR-18 fails closed on DNS error, returns SocketAddr
- P0-3 (parity harness): containerized with stubbed upstreams
- P1-4 (fatal logs): FR-25 plain stderr for startup fatals
- P1-5 (bare routes + TrimSpace): FR-1/2/4
- P1-6/7 (SSH rejection + global requests): FR-9, FR-12
- P1-8 (git specifics): FR-14, FR-13
- P1-9 (egress HTTP specifics): FR-17
- P1-10/11 (russh API + version): dep list, removed code sketches
- P1-12 (logging schemas): hand-rolled JSON
- P1-13 (panic profile): `panic = "unwind"`
- P1-14 (parity corpus hermeticity): mocked upstreams
- P1-15 (CONNECT drain + readiness drop): FR-27 (readiness drop later reverted in v3)
- P1-16 (supply chain): cargo-deny, version pins

### v2 → v3: Second codex adversarial review (8 findings: 2 P0, 6 P1)

- **P0-1 (V1, SSRF custom connector):** Added `SsrfConnector` custom hyper connector in Architecture. FR-7 / FR-18 now explicitly forbid the default `HttpConnector`. Custom connector dials the vetted `SocketAddr` while preserving the hostname for TLS SNI and HTTP Host. New unit tests in `ssrf_connector.rs`.
- **P0-2 (V1, harness TLS trust):** Added `NAUTILOOP_EXTRA_CA_BUNDLE` env var loaded by `tls.rs` into a custom `RootCertStore`. SR-10 locks it as test-only with CI lint enforcement. Harness `docker-compose.yml` sets the env var and mounts the test CA. Go sidecar gets a `Dockerfile.go-with-test-ca` variant for the harness.
- **P1-3 (V1, advisory pins):** Updated to `tracing-subscriber >=0.3.20` and `rustls-webpki >=0.103.10` (were 0.3.19 and 0.102.9, which did NOT actually clear the advisories). Sources added to the supply chain table.
- **P1-4 (NEW, /healthz method regression):** Removed the "any method other than GET returns 404" clause from FR-21. Handler now accepts any HTTP method, matching Go. Parity corpus adds HEAD and POST tests.
- **P1-5 (NEW, egress destination format):** FR-19 now specifies destination format exactly: CONNECT synthesizes `:443` if missing; plain HTTP logs `URL.Host` as-is without synthesizing a port. Parity corpus splits the CONNECT and HTTP cases.
- **P1-6 (V1, russh handler semantics):** FR-9, FR-10, FR-12, and the Architecture implementation notes now describe russh 0.60 wire behavior explicitly: `Ok(false)` for global-request handlers and `channel_open_*` rejections; `session.channel_failure(channel)` for env/pty/subsystem/malformed-exec. No more "Err(reject)" or "empty handlers."
- **P1-7 (NEW, timeouts and CONNECT half-close):** New FR-28 freezes: model streaming no-timeout, CONNECT 10s dial, SSH upstream 10s dial, CONNECT CloseWrite half-close sequence, no body size limits. Unit tests added for each.
- **P1-8 (V1, FR-27 readiness drop):** Removed the readiness-drop-on-shutdown "improvement." Based on the wrong K8s model for a sidecar that has only a startupProbe and is talked to via localhost. A dedicated "Rejected improvement" section in Architecture explains the reasoning. Kept the CONNECT tunnel tracking (a real improvement over Go). Phase 5 now explicitly requires pre-verification of loop-retry semantics for mid-operation sidecar termination.
