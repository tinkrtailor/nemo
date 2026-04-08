# Sidecar: Containerized Parity Test Harness

## Overview

Phase 4 of the Rust sidecar migration plan. Build a containerized parity test harness that runs the Go sidecar and the Rust sidecar side-by-side against the same hermetic inputs, diffs their outputs, and gates the cutover decision.

This is the single biggest remaining production-readiness blocker before phase 5 (K8s cutover). Harness runs locally AND in CI on every PR touching `sidecar/` — without CI enforcement, the harness is ceremony.

## Baseline

Main at merge of PR #73 (`63d61e8`). The Rust sidecar is committed at `sidecar/` with 93 unit tests + 7 integration tests passing. The Go sidecar is kept per the migration plan until phase 6. Both binaries compile.

## Problem Statement

After three review passes (codex v1, v2, v3) and one followup batch review (PR #73), the Rust sidecar compiles, passes clippy, passes 100+ tests, and has been adversarially reviewed to the point of diminishing returns. **What we still lack is evidence of behavior parity against the Go implementation under realistic conditions** AND a mechanism that prevents future regressions from slipping through.

The harness this spec builds is the difference between "we think it's behavior-identical" and "we have evidence it's behavior-identical AND CI rejects any change that breaks it."

### Key design decision up front: use CGNAT addresses (RFC6598) for the Docker network

A naïve design would use Docker's default bridge network, which allocates IPs in `172.16.0.0/12` (RFC1918). But both sidecars' SSRF checks fail-closed on RFC1918 addresses, so mapping `api.openai.com` → `172.x` via `extra_hosts` would make every test return HTTP 403 SSRF-block, not upstream responses.

The clean fix: **use a custom Docker bridge network in `100.64.0.0/10` (RFC6598 CGNAT)**. Neither sidecar blocks CGNAT. Verified in `sidecar/src/ssrf.rs:94-99` (explicit comment: "we DO NOT block 100.64.0.0/10") and `images/sidecar/main.go:43-48` (privateRanges list only has RFC1918 + link-local + loopback). So mock service IPs like `100.64.0.10` are indistinguishable from public internet addresses to the sidecars' SSRF logic, and the harness proceeds as intended.

**No code changes to either sidecar are required.** No test-only feature flags. Just a `subnet: 100.64.0.0/24` line in the compose file's network section.

## Dependencies

- **Requires:** PR #63 (Rust sidecar merged), PR #73 (followups merged including `__test_utils` feature), PR #56 (Go sidecar health bind fix). All three are on main.
- **Enables:** phase 5 cutover with actual parity evidence + CI enforcement. Unblocks retiring the Go sidecar.
- **Blocks:** nothing.

## Requirements

### Functional Requirements

#### Harness layout and services

- FR-1: The harness lives at `sidecar/tests/parity/` with this structure:
  ```
  sidecar/tests/parity/
  ├── docker-compose.yml                # orchestrates all services
  ├── Dockerfile.go-sidecar             # builds Go sidecar (see FR-9)
  ├── Dockerfile.go-with-test-ca        # Go sidecar + baked test CA
  ├── fixtures/
  │   ├── test-ca/
  │   │   ├── ca.pem                    # test CA cert (committed)
  │   │   ├── ca.key                    # test CA key (committed, test-only)
  │   │   └── README.md                 # loud "test-only, never used in prod" warning
  │   ├── mock-openai/
  │   │   ├── server.py                 # minimal HTTPS server
  │   │   ├── Dockerfile
  │   │   ├── cert.pem                  # signed by test-ca, SAN = api.openai.com
  │   │   └── key.pem
  │   ├── mock-anthropic/               # same shape for api.anthropic.com
  │   ├── mock-github-ssh/
  │   │   ├── server.py                 # paramiko-based SSH server
  │   │   ├── Dockerfile
  │   │   ├── host_key
  │   │   └── authorized_keys
  │   ├── mock-example-http/            # plain HTTP server for egress cases
  │   │   ├── server.py
  │   │   └── Dockerfile
  │   ├── mock-tcp-echo/                # raw TCP echo server for CONNECT drain test
  │   │   ├── server.py
  │   │   └── Dockerfile
  │   ├── go-secrets/
  │   │   ├── model-credentials/openai          # "sk-test-openai-key"
  │   │   ├── model-credentials/anthropic       # "sk-ant-test-key"
  │   │   ├── ssh-key/id_ed25519                # harness client key
  │   │   └── ssh-known-hosts/known_hosts       # trusts mock-github-ssh
  │   └── rust-secrets/                         # identical content, separate mount
  ├── corpus/
  │   └── *.json                        # one file per test case
  ├── src/
  │   └── main.rs                       # harness driver binary
  ├── README.md
  └── Cargo.toml                        # crate: nautiloop-sidecar-parity-harness
  ```

- FR-2: `docker-compose.yml` shall define a custom bridge network `parity-net` with `subnet: 100.64.0.0/24` and at minimum these services. The count is not fixed — all services below are required, plus the harness driver can start ad-hoc extras as needed:
  1. **`sidecar-go`** — Go binary built from `Dockerfile.go-with-test-ca`. Ports: `19090:9090`, `19091:9091`, `19092:9092`, `19093:9093` (host:container). Mounts `go-secrets/` at `/secrets/`. Env: `GIT_REPO_URL=git@github.com:test/repo.git`. `networks: parity-net: ipv4_address: 100.64.0.20`. `extra_hosts`: `api.openai.com:100.64.0.10`, `api.anthropic.com:100.64.0.11`, `github.com:100.64.0.12`, `mock-example:100.64.0.13`, `mock-tcp-echo:100.64.0.14`.
  2. **`sidecar-rust`** — Rust binary from `images/sidecar/Dockerfile`. Ports: `29090-29093:9090-9093`. Mounts `rust-secrets/` at `/secrets/` AND `fixtures/test-ca/ca.pem` read-only at `/test-ca/ca.pem`. Env: `GIT_REPO_URL=git@github.com:test/repo.git`, `NAUTILOOP_EXTRA_CA_BUNDLE=/test-ca/ca.pem`. Same `extra_hosts` entries, `ipv4_address: 100.64.0.21`.
  3. **`mock-openai`** — HTTPS server on `100.64.0.10:443`. TLS cert signed by test-ca, SAN includes `api.openai.com`.
  4. **`mock-anthropic`** — HTTPS server on `100.64.0.11:443`. SAN includes `api.anthropic.com`.
  5. **`mock-github-ssh`** — paramiko SSH server on `100.64.0.12:22`.
  6. **`mock-example`** — plain HTTP server on `100.64.0.13:80`. Used for egress plain-HTTP cases.
  7. **`mock-tcp-echo`** — raw TCP echo server on `100.64.0.14:443`. Used ONLY by the CONNECT drain test. Separate so the CONNECT test can target it without colliding with mock-openai.

- FR-3: Both sidecars shall depend on all mock services being healthy before starting, via Compose `depends_on` with `condition: service_healthy`. The sidecars themselves do NOT get Docker-level healthchecks — the harness driver polls `/healthz` via `reqwest` with exponential backoff (200ms initial, up to 10s total) to wait for readiness. This preserves the ability to test the 503 startup window per FR-17.

- FR-4: The `Dockerfile.go-sidecar` rebuilds the Go sidecar from source. Because PR #63 replaced `images/sidecar/Dockerfile` with the Rust build, the Go Dockerfile must be resurrected:
  ```dockerfile
  FROM golang:1.22-alpine AS builder
  WORKDIR /build
  COPY images/sidecar/main.go images/sidecar/main_test.go images/sidecar/go.mod images/sidecar/go.sum ./
  RUN go build -o auth-sidecar .

  FROM scratch
  COPY --from=builder /build/auth-sidecar /auth-sidecar
  COPY --from=builder /etc/ssl/certs/ca-certificates.crt /etc/ssl/certs/
  ENTRYPOINT ["/auth-sidecar"]
  ```
  Note: `images/sidecar/go.mod`, `go.sum`, and `main.go` still exist on main per phase 6 schedule (Go code kept until phase 6). If any of those files is missing when this spec is implemented, the implementer must file an issue and stop; the Go source is a hard prerequisite.

- FR-5: The `Dockerfile.go-with-test-ca` builds on top of `Dockerfile.go-sidecar` and appends the harness test CA to the CA bundle:
  ```dockerfile
  FROM golang:1.22-alpine AS builder
  # ... same as Dockerfile.go-sidecar ...

  FROM alpine:3.20 AS ca-builder
  RUN apk add --no-cache ca-certificates
  COPY fixtures/test-ca/ca.pem /tmp/test-ca.pem
  RUN cat /tmp/test-ca.pem >> /etc/ssl/certs/ca-certificates.crt

  FROM scratch
  COPY --from=builder /build/auth-sidecar /auth-sidecar
  COPY --from=ca-builder /etc/ssl/certs/ca-certificates.crt /etc/ssl/certs/
  ENTRYPOINT ["/auth-sidecar"]
  ```
  The Go binary reads `/etc/ssl/certs/ca-certificates.crt` via the standard library's `crypto/x509.SystemCertPool()` on Linux, which is populated from that file on Alpine-derived bases. The resulting scratch image trusts the test CA in addition to whatever Mozilla roots ship with Alpine's `ca-certificates` package.

- FR-6: The Rust sidecar image (production `images/sidecar/Dockerfile`) is used AS-IS. The test CA is loaded at runtime via `NAUTILOOP_EXTRA_CA_BUNDLE=/test-ca/ca.pem`, exercising the production code path from `sidecar/src/tls.rs:60` onward. Verified: rustls APPENDS the extra CA to the `webpki-roots` default store rather than replacing it.

#### Mock services behavior

- FR-7: **`mock-openai`** shall respond to these paths (and ONLY these paths; anything else → 404):
  - `GET /_healthz` → 200 `{"ok":true}` (plain HTTP on an additional port, NOT served over TLS)
  - `GET /v1/models` → 200 with deterministic JSON body
  - `POST /v1/chat/completions` (non-streaming, no `stream:true` in body) → 200 with deterministic JSON body
  - `POST /v1/chat/completions` with `stream: true` in the JSON body → 200 `Content-Type: text/event-stream`, streams 3 SSE events with deterministic content, then `data: [DONE]\n\n` and closes. Server MUST flush between events (see FR-10 for the Python implementation note).

- FR-8: **`mock-anthropic`** shall respond to:
  - `GET /_healthz` → 200 (plain HTTP)
  - `POST /v1/messages` (non-streaming) → 200 with deterministic JSON body
  - `POST /v1/messages` with `stream: true` → 200 SSE stream with 3 deterministic events
  - Anything else → 404

- FR-9: **`mock-github-ssh`** (paramiko-based):
  - Accepts any SSH client key (no auth challenge beyond key type parsing)
  - Recognizes `exec` requests with commands `git-upload-pack <path>` and `git-receive-pack <path>`
  - For `git-upload-pack 'test/repo.git'` or `git-upload-pack test/repo.git`: write deterministic bytes to channel stdout, send `ExitStatus(0)`, close channel
  - For `git-receive-pack 'test/repo.git'`: consume input bytes from channel stdin, write a deterministic acknowledgement to stdout, send `ExitStatus(0)`, close channel
  - For any other exec command OR any other repo path: write an error message to stderr, send `ExitStatus(128)`, close channel
  - `env`, `pty-req`, `subsystem`, `shell`, `x11-req` requests: reject via `channel.send_exit_status(0)` is wrong — reject via returning `False` from the paramiko request handler
  - Healthcheck: separate plain TCP listener on port 2200 that accepts + closes, used for `depends_on` healthcheck

- FR-10: **Mock service Python implementation constraints.** Because Python's stdlib `http.server` does not reliably flush SSE chunks, ALL mock HTTP services shall use `hypercorn` or `uvicorn` with `asyncio` and explicit `await writer.drain()` between SSE events. Each mock is dockerized with `python:3.12-slim` + `pip install hypercorn uvicorn starlette` (or equivalent lightweight async ASGI). The implementer must verify flush behavior with a manual `curl -N` test against each mock before writing any harness cases.

- FR-11: **`mock-example`** (plain HTTP): serves:
  - `GET /_healthz` → 200
  - `GET /foo` → 200 with fixed body
  - `GET /redirect` → 302 with `Location: /foo` (used to verify sidecars do NOT follow redirects)
  - Anything else → 404

- FR-12: **`mock-tcp-echo`** (raw TCP): accepts connections on port 443, echoes every byte back, never closes until the client closes. Used exclusively by the CONNECT drain test (FR-22 item 4).

- FR-13: **Mock log introspection.** Every mock service (except mock-tcp-echo which has no request/response semantics beyond echoing) exposes an HTTP introspection endpoint on a dedicated plain-HTTP port:
  - `GET http://<mock>:9999/__harness/logs` → JSON array of every observed request since the mock started, with fields `{id, timestamp, method, path, host_header, headers, body_b64, source_ip}`
  - `POST http://<mock>:9999/__harness/reset` → clears the in-memory log array
  - `GET http://<mock>:9999/__harness/healthcheck-only` → JSON count of the subset of the log array that was `/_healthz` requests (for the harness driver to verify that its resets aren't racing healthchecks)

  The introspection endpoint is a separate ASGI app on port 9999, not mixed into the main HTTPS app, so healthcheck and test traffic are not intermingled. Docker-level healthchecks hit the main app's `/_healthz` over HTTP on port 80 (or 443 non-TLS in a separate route — the implementer picks whichever is simpler for the mock's framework). The introspection port is NOT exposed to the host — only reachable from within `parity-net`.

- FR-14: **Mock-github-ssh log introspection.** The SSH mock exposes the same introspection API via an HTTP sidecar listener on port 9999 (paramiko doesn't cleanly expose an HTTP interface, so the mock runs both a paramiko SSH server on 22 and an asyncio HTTP server on 9999). `logs` returns SSH exec commands observed, bytes read/written, channel request types, and any authentication events.

#### Harness driver

- FR-15: The harness driver is a new workspace crate at `sidecar/tests/parity/` named `nautiloop-sidecar-parity-harness`. It is a separate member of the Cargo workspace. Only its own `cargo run -p nautiloop-sidecar-parity-harness` or `cargo test -p nautiloop-sidecar-parity-harness` invokes it. The crate is binary-only (`src/main.rs`), with no `lib.rs` required — Cargo supports bin-only workspace members.

- FR-16: **Image freshness is enforced by default.** The driver shall run `docker compose build` before every run unless `--no-rebuild` is passed. The rebuild operates on the harness's local `docker-compose.yml` only (the sidecar-rust image rebuild uses `images/sidecar/Dockerfile`, and the sidecar-go image uses `Dockerfile.go-with-test-ca`). This guarantees stale binaries cannot produce false-green results.

- FR-17: The driver shall:
  1. Run `docker compose build` (unless `--no-rebuild`)
  2. Run `docker compose up -d`
  3. Poll each mock service's `/_healthz` and wait for 200 responses (via `reqwest` to `100.64.0.X:80/_healthz`), fail with clear error if any service isn't healthy within 60s
  4. **Test the 503 startup window BEFORE flipping readiness:** immediately after `docker compose up -d`, poll both sidecars' `/healthz` once and record the response. The 503 `{"status":"starting"}` case asserts what was observed here. Then continue polling with backoff until both sidecars return 200 `{"status":"ok"}` — this becomes the readiness barrier for the rest of the corpus.
  5. Run the filtered corpus
  6. If `--stop` is set OR all tests passed: `docker compose down -v`. Otherwise leave the stack up for inspection.

- FR-18: For each test case, the driver shall:
  1. POST `http://<each-mock>:9999/__harness/reset` to clear their in-memory log arrays
  2. Issue the test input to BOTH sidecars in parallel (against host ports 19090-19093 for Go, 29090-29093 for Rust)
  3. Capture from each side: HTTP status code, response headers (subset), response body, emitted container log lines (via `docker logs <container> --since <test-start-time> --timestamps`), SSH exit codes, SSH stderr
  4. After the request completes, GET `http://<each-mock>:9999/__harness/logs` and capture what the mock observed from BOTH sidecars — this is per-test-case state that the harness driver attributes to each sidecar by `source_ip` (Go = 100.64.0.20, Rust = 100.64.0.21)
  5. Normalize per FR-19 and diff Go vs Rust
  6. For the documented divergences (FR-22), flip the assertion

- FR-19: Normalization rules applied before comparing outputs:
  - Log lines: strip `timestamp` field entirely. All other fields compared verbatim.
  - HTTP response headers: compare `Content-Type` + case config. Strip `Date`, `Server`, `Via`, `X-Request-Id`, `Connection`, `Content-Length` (because streaming chunked vs fixed-length can differ).
  - Response bodies: per-test config can specify fields to strip (e.g. request IDs inside JSON bodies).
  - SSH stderr: trim trailing whitespace.
  - Mock log entries: strip `id` and `timestamp` fields; sort by `(path, method, source_ip)` so concurrent interleaving doesn't affect comparison.
  - Docker log stdout/stderr: sort by normalized content within the per-test time window (acknowledges FR-14 concurrent-log-order limitation from the parent rust-sidecar spec).

- FR-20: The driver shall support filtering:
  - `cargo run -p nautiloop-sidecar-parity-harness -- --category model_proxy` — runs only the model_proxy cases
  - `cargo run -p nautiloop-sidecar-parity-harness -- --case <case_name>` — runs a single case
  - `cargo run -p nautiloop-sidecar-parity-harness -- --only-divergence` — runs only the 4 divergence tests
  - `cargo run -p nautiloop-sidecar-parity-harness -- --stop` — tear down the stack after the run regardless of outcome
  - `cargo run -p nautiloop-sidecar-parity-harness -- --no-rebuild` — skip `docker compose build` (developer workflow for iterating on test cases)

#### Test corpus

- FR-21: The corpus lives in `sidecar/tests/parity/corpus/` as JSON files. Each file is one test case. The JSON schema:
  ```json
  {
    "name": "test_case_name",
    "category": "model_proxy" | "egress" | "git_ssh" | "health" | "divergence",
    "description": "human-readable",
    "input": { ... category-specific ... },
    "expected_parity": true | false,
    "divergence": null | { "description": "...", "go_expected": "...", "rust_expected": "..." },
    "normalize": { "body_strip_fields": ["id"], "extra_header_strip": ["X-Whatever"] },
    "order_hint": "first" | "last" | null
  }
  ```
  `order_hint: "last"` forces a case to run last. Used by the CONNECT drain test, which SIGTERMs containers and therefore must run after all other tests. Only one test case may have `order_hint: "last"`; the harness driver panics at startup if more than one case has this hint.

- FR-22: Corpus contents — updated to match actual sidecar endpoints and mock contracts:

  **Model proxy parity:**
  - `openai_get_v1_models` — GET `/openai/v1/models` → mock returns model list JSON. Assert upstream received `Host: api.openai.com`, `Authorization: Bearer sk-test-openai-key`.
  - `openai_post_chat_completions_nonstream` — POST `/openai/v1/chat/completions` body `{"model":"gpt-4","messages":[{"role":"user","content":"ping"}]}`. Assert upstream received the exact body + auth header.
  - `openai_post_chat_completions_stream` — POST `/openai/v1/chat/completions` body `{"model":"gpt-4","stream":true,"messages":[...]}`. Assert client receives SSE chunks incrementally (timestamps of chunk arrivals differ, not all at end). Assert mock-openai observed the request with `stream: true` in the body.
  - `anthropic_post_v1_messages_nonstream` — POST `/anthropic/v1/messages` body `{"model":"claude","messages":[...]}`. Assert upstream received `x-api-key: sk-ant-test-key`, `anthropic-version: 2023-06-01`, `Host: api.anthropic.com`.
  - `anthropic_post_v1_messages_stream` — POST with `stream:true`. Assert incremental SSE + mock observed body.
  - `openai_bare_prefix` — GET `/openai` (no trailing `/`) → mock should see upstream path `/` (no /openai prefix). Verifies bare-route handling in both sidecars.
  - `anthropic_bare_prefix` — GET `/anthropic` (no trailing `/`) → same, upstream `/`.
  - `unknown_route_returns_403` — GET `/some/unknown/path` → 403 with body `{"error":"only /openai/* and /anthropic/* routes are supported"}`. Assert mock services observed NO incoming request.
  - `openai_client_auth_header_overwritten` — GET `/openai/v1/models` with `Authorization: Bearer client-supplied-fake`. Assert mock-openai observed `Authorization: Bearer sk-test-openai-key` (the server credential), NOT the client's.
  - `anthropic_client_api_key_overwritten` — POST `/anthropic/v1/messages` with `x-api-key: client-supplied-fake`. Assert mock-anthropic observed `x-api-key: sk-ant-test-key`.
  - `anthropic_client_version_passthrough` — POST `/anthropic/v1/messages` with `anthropic-version: 2022-01-01`. Assert mock-anthropic observed `anthropic-version: 2022-01-01` (client value passed through, not overwritten).
  - `openai_credential_refresh_per_request` — issue request 1, mutate `fixtures/go-secrets/model-credentials/openai` and `fixtures/rust-secrets/model-credentials/openai` to a new value, issue request 2, assert request 2's mock-observed Authorization matches the new credential on BOTH sides.

  **Egress parity:**
  - `egress_connect_github` — CONNECT `github.com:443` via port 19092/29092. Assert tunnel established (bytes flow both directions using mock-github-ssh as the target). Log line `destination: "github.com:443"`.
  - `egress_connect_github_no_port` — CONNECT `github.com` (no port specified). Log destination `"github.com:443"` (synthesized).
  - `egress_http_get_example` — GET `http://mock-example/foo` via port 19092/29092 (plain HTTP proxy, absolute-form request URI). Log destination `"mock-example"` (no port, matches Go's `URL.Host`).
  - `egress_http_get_example_with_port` — GET `http://mock-example:8080/foo`. Log destination `"mock-example:8080"`.
  - `egress_http_strips_proxy_connection` — GET with `Proxy-Connection: keep-alive` header. Assert mock-example observed NO `Proxy-Connection` header.
  - `egress_http_no_redirect_follow` — GET `http://mock-example/redirect`. Assert client received 302 with `Location: /foo` (not the final /foo body). Assert mock-example observed only ONE request, not two.

  **Git SSH parity:**
  - `ssh_upload_pack_matching_repo` — exec `git-upload-pack 'test/repo.git'`. Assert exit status 0, bytes match mock's deterministic pack.
  - `ssh_receive_pack_matching_repo` — exec `git-receive-pack 'test/repo.git'` with pushing pack bytes. Exit status 0.
  - `ssh_wrong_repo_path_rejected_locally` — exec `git-upload-pack 'wrong/repo.git'`. Assert exit status 1 from BOTH sidecars (both reject via local allowlist; the mock never sees the request because the sidecar rejects first). Assert mock-github-ssh log shows zero exec events.
  - `ssh_rejects_non_git_exec` — exec `ls /etc`. Exit status 1. Mock observed zero.
  - `ssh_rejects_env_request` — send `env` channel request before exec. Assert channel_failure response, no exit status. Mock observed zero (channel never proceeded to exec).

  **Health parity:**
  - `healthz_pre_ready_returns_503` — poll `/healthz` IMMEDIATELY after `docker compose up -d`, before the readiness loop. Assert 503 `{"status":"starting"}`. This test is run implicitly by the driver during startup (FR-17 step 4), not via the normal corpus driver. Corpus file exists so the `--category health` filter finds it and can re-run a synthetic version if needed, but the actual 503 window observation happens once at startup.
  - `healthz_post_ready_returns_200` — after readiness barrier. GET `/healthz`. Assert 200 `{"status":"ok"}`.
  - `healthz_head_method_parity` — after readiness. HEAD `/healthz`. Assert 200 (Go's mux doesn't method-check; Rust matches per FR-21 of parent spec).

  **Documented divergences (4 cases, must FAIL if Go and Rust match):**
  - `divergence_ssrf_dns_error_failclosed` — issue an egress GET to a hostname whose mock DNS responder returns SERVFAIL. **Implementation of this case is DEFERRED** (see FR-23 scope reduction). For now, this case asserts a weaker property: issuing an egress GET to `http://deliberately-unresolvable.invalid/` returns 502 from BOTH sidecars (parity on the error path). The "fail-closed vs fail-open" divergence is verified by unit tests in `sidecar/src/ssrf.rs`, not by this harness case. Marked as a known scope reduction; true DNS-mock-based divergence testing is a followup.
  - `divergence_bare_exec_upload_pack_rejection` — exec `git-upload-pack` (no path argument) via the SSH proxy. Assert Go exits with code 128 (mock received and errored), Rust exits with code 1 (sidecar rejected locally). Assert mock-github-ssh logs show 1 observed exec from Go, 0 from Rust.
  - `divergence_bare_exec_receive_pack_rejection` — same for `git-receive-pack`.
  - `divergence_connect_drain_on_sigterm` — establish a CONNECT tunnel through port 19092/29092 to `mock-tcp-echo:443`. Begin sending bytes at 1 byte per 100ms. After 500ms of steady traffic, send SIGTERM to each sidecar container via `docker kill --signal SIGTERM`. Measure time from SIGTERM to when the tunnel bytes stop flowing. Assert Go stops within 200ms, Rust continues for 2-5 seconds (drain up to 5s deadline). **`order_hint: "last"`** — this test kills the containers so it must run after all parity tests. The harness driver restarts the stack at the end if `--stop` is not set, or tears it down if `--stop` is set.

- FR-23: **Scope reductions documented explicitly in the corpus:**
  - `divergence_ssrf_dns_error_failclosed` is weakened per FR-22 above. True hermetic DNS divergence testing requires a DNS mock that SERVFAILs specific hostnames and is configured via the sidecar's DNS resolver. Deferred to followup issue after the harness lands.
  - The parent rust-sidecar spec's FR-18 guarantees "resolve once, pass SocketAddr to dialer, never redial by hostname." This harness does NOT verify the resolve-once property — it verifies upstream routing smoke (the request arrived at the expected mock). True rebinding testing requires a DNS mock that returns different IPs on successive calls. Deferred as a followup — the rebinding property is currently enforced by code review of `sidecar/src/ssrf_connector.rs`, not by this harness.

#### CI integration

- FR-24: The harness shall be wired into a NEW `.github/workflows/ci.yml` workflow that runs on every pull request AND every push to main. The workflow contains these jobs:
  1. **`rust-checks`**: installs rust stable, runs `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`.
  2. **`rust-checks-with-test-utils`**: same as above but with `--features nautiloop-sidecar/__test_utils` (this covers the integration tests from PR #73 that were previously silently skipped — closes issue #71).
  3. **`parity-harness`**: installs Docker (GitHub runners have docker pre-installed on `ubuntu-latest`), installs rust stable, runs `cargo run -p nautiloop-sidecar-parity-harness --release -- --stop`. Fails the CI run on any harness failure. Runs only on changes touching `sidecar/**`, `images/sidecar/**`, or `specs/rust-sidecar*.md` (path filter).
  4. **`prod-leak-lint`**: runs `sidecar/scripts/lint-no-test-utils-in-prod.sh`. Fails the build on any hit (as extended in FR-28 for `NAUTILOOP_EXTRA_CA_BUNDLE`).

- FR-25: The `parity-harness` CI job has a 10-minute timeout. If the harness runs longer (likely a stuck container or healthcheck), the job fails with a clear error. Typical runtime target: <5 minutes per NFR-2.

- FR-26: The `parity-harness` job uploads `sidecar/tests/parity/harness-run.log` and `docker compose logs --timestamps` output as artifacts on failure, so the failure can be debugged from the CI UI without re-running locally.

#### Cargo-deny

- FR-27: The harness crate's dependencies are gated by the existing workspace `deny.toml` (if it exists) or a new one added in this spec. The new deps (`reqwest`, `russh` client-side, `clap`, `anyhow`) inherit the license allowlist from the rest of the workspace. Any new advisory against them fails CI.

#### Security lint extension

- FR-28: The existing `sidecar/scripts/lint-no-test-utils-in-prod.sh` from PR #73 shall be extended to ALSO check for `NAUTILOOP_EXTRA_CA_BUNDLE` references in `.github/workflows/` and in production K8s manifests under `terraform/`. Fail the script if any production workflow or manifest references the variable. The `ci.yml` workflow itself IS allowed to set it in the `parity-harness` job, so the script whitelists any `.github/workflows/ci.yml` matches that are in a job named `parity-harness` (pattern match on the context). The script's exit code is consumed by the `prod-leak-lint` CI job.

### Non-Functional Requirements

- NFR-1: The harness shall be runnable on any developer workstation with Docker Desktop + Rust stable.
- NFR-2: Full corpus run under 5 minutes on a 2024-era laptop (cold compose build excluded; `--no-rebuild` path target).
- NFR-3: `cargo clippy --workspace -- -D warnings` green, including the harness crate.
- NFR-4: Hermetic — no external network access during test runs. Both sidecars resolve `api.openai.com`, `api.anthropic.com`, `github.com`, `mock-example`, `mock-tcp-echo` via `extra_hosts` to `parity-net` IPs.
- NFR-5: Failures produce diffs with the exact fields that differ, not "outputs don't match." Diff points at the corpus JSON filename.
- NFR-6: **Determinism** — running the same corpus twice against unchanged code produces identical pass/fail. No flakes from ordering, timing, or container state.
- NFR-7: **Isolation** — the harness MUST NOT leave dangling containers, volumes, or networks on failure. On any panic or early exit in the driver, a `Drop` guard runs `docker compose down -v --remove-orphans` to clean up.
- NFR-8: **Resource bounds** — the harness configures a per-container memory limit of 512MB and logs a warning if any container approaches it.
- NFR-9: **Observability** — on failure, the harness dumps full mock logs AND full `docker compose logs` to `sidecar/tests/parity/harness-run.log` in a deterministic format, so post-mortem is possible without re-running. The CI uploads this file as an artifact.

### Security Requirements

- SR-1: The test CA private key is committed under `sidecar/tests/parity/fixtures/test-ca/ca.key` with a loud header comment identifying it as test-only. The directory also has a `README.md` with the same warning plus an expiration reminder.
- SR-2: The harness client SSH key is committed similarly under `fixtures/go-secrets/ssh-key/id_ed25519` and `rust-secrets/ssh-key/id_ed25519`. Same headers.
- SR-3: Mock model credentials are obviously non-production strings (`sk-test-openai-key`, `sk-ant-test-key`). Committed.
- SR-4: The harness MUST NOT reuse any production certificate, key, or credential. All harness secrets are freshly generated for this spec.
- SR-5: `NAUTILOOP_EXTRA_CA_BUNDLE` is set ONLY in the harness compose file and ONLY in the CI `parity-harness` job. FR-28's lint enforces this at CI time.
- SR-6: `NAUTILOOP_SSRF_TEST_ALLOWLIST` does NOT exist in this spec — CGNAT addresses handle SSRF transparently, so no test-only env var bypasses the SSRF check.
- SR-7: Test CA generation is pinned:
  ```bash
  openssl req -x509 -newkey rsa:2048 -keyout ca.key -out ca.pem \
    -sha256 -days 3650 -nodes \
    -subj "/CN=Nautiloop Parity Harness Test CA" \
    -addext "basicConstraints=critical,CA:TRUE"
  ```
  Mock service cert generation is pinned:
  ```bash
  # Per mock (openai, anthropic, example):
  openssl req -newkey rsa:2048 -keyout key.pem -out csr.pem -nodes \
    -sha256 \
    -subj "/CN=api.openai.com" \
    -addext "subjectAltName=DNS:api.openai.com"
  openssl x509 -req -in csr.pem -CA ca.pem -CAkey ca.key \
    -CAcreateserial -out cert.pem -days 3650 -sha256 \
    -copy_extensions copy
  ```
  RSA-2048 + SHA-256 is acceptable to rustls-webpki 0.103.x and Go's `crypto/tls` without any extra config. All certs MUST include a `subjectAltName` DNS entry matching the hostname the sidecar will try to verify — otherwise rustls rejects with `NameMismatch` and Go's verifier rejects with a `SAN` error.
- SR-8: The test CA has a 10-year validity (3650 days). A `sidecar/tests/parity/fixtures/test-ca/README.md` notes the expiration date and points to a regeneration script committed alongside (`regenerate-test-ca.sh`) so the future operator can rotate without guessing openssl flags.
- SR-9: The harness SSH key (`fixtures/go-secrets/ssh-key/id_ed25519`) has a fixed fingerprint. Mock-github-ssh's `authorized_keys` trusts only this fingerprint. This is a footgun (anyone who obtains the committed key can spoof tests locally) but acceptable because the key's trust boundary is the test mock only. The README documents this clearly.

## Architecture

### Network topology

```
parity-net (custom bridge, subnet 100.64.0.0/24)
│
├── sidecar-go       100.64.0.20
├── sidecar-rust     100.64.0.21
├── mock-openai      100.64.0.10  (:443 HTTPS, :9999 introspection, :80 healthz)
├── mock-anthropic   100.64.0.11  (:443 HTTPS, :9999 introspection, :80 healthz)
├── mock-github-ssh  100.64.0.12  (:22 SSH, :9999 introspection, :2200 healthz)
├── mock-example     100.64.0.13  (:80 HTTP, :9999 introspection)
└── mock-tcp-echo    100.64.0.14  (:443 raw TCP, no introspection, no healthz — harness uses TCP connect check)
```

`extra_hosts` on both sidecars maps `api.openai.com → 100.64.0.10`, `api.anthropic.com → 100.64.0.11`, `github.com → 100.64.0.12`. These are CGNAT addresses; neither sidecar's SSRF logic blocks them (verified against `sidecar/src/ssrf.rs:94-99` and `images/sidecar/main.go:43-48`).

Host port mappings:
- sidecar-go: 19090 (model), 19091 (ssh), 19092 (egress), 19093 (health)
- sidecar-rust: 29090, 29091, 29092, 29093

The harness driver runs on the host and connects to the sidecars via these mapped ports, AND directly to the mock services' introspection endpoints at `http://100.64.0.X:9999` by being attached to `parity-net` itself (see FR-18). The driver attaches by running inside a container for the CI job, or on the host with a secondary bridge connection for developer runs. **Developer workflow alternative:** the introspection ports can additionally be mapped to host ports (e.g. `49999-49999:9999`) so the host-based driver can hit them via `localhost`. The driver uses whichever is available, detected at startup.

### Crate structure

```toml
# Cargo.toml (workspace root)
[workspace]
members = [
    "cli",
    "control-plane",
    "sidecar",
    "sidecar/tests/parity",  # NEW
]
```

```toml
# sidecar/tests/parity/Cargo.toml
[package]
name = "nautiloop-sidecar-parity-harness"
version = "0.1.0"
edition = "2021"
publish = false

[[bin]]
name = "nautiloop-sidecar-parity-harness"
path = "src/main.rs"

[dependencies]
tokio = { version = "1.40", features = ["full"] }
reqwest = { version = "0.12", default-features = false, features = ["json", "rustls-tls", "stream"] }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
clap = { version = "4", features = ["derive"] }
anyhow = "1"
thiserror = "1"
russh = { version = "0.60", default-features = false, features = ["ring"] }
russh-keys = "0.60"
tracing = "0.1"
tracing-subscriber = ">=0.3.20"
rustls-pemfile = "2"
rustls = { version = "0.23", default-features = false, features = ["ring"] }
```

The harness driver uses `reqwest` with a custom `rustls::ClientConfig` that loads the test CA from `fixtures/test-ca/ca.pem` — so the driver itself trusts the mock TLS services exactly the way the sidecars do. The egress raw-TCP cases (CONNECT, absolute-form, origin-form) use `tokio::net::TcpStream` directly and write bytes by hand (FR-4 — `reqwest` is inadequate for raw proxy semantics). The `russh` client-side is used for the git_ssh cases, connecting to sidecar-go:19091 / sidecar-rust:29091 and issuing exec requests.

### Driver program structure

`sidecar/tests/parity/src/main.rs` pseudocode:

```rust
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let corpus = load_corpus("corpus/")?;
    let filtered = filter_corpus(&corpus, &args);

    let compose = ComposeStack::new("docker-compose.yml");
    if !args.no_rebuild { compose.build().await?; }
    compose.up().await?;
    let _guard = ComposeGuard::new(&compose, args.stop); // Drop impl tears down on panic (NFR-7)

    compose.wait_mock_healthchecks(Duration::from_secs(60)).await?;

    // FR-17 step 4: capture the 503 startup window BEFORE sidecar readiness flips.
    let startup_503_capture = capture_503_window(&compose).await?;

    compose.wait_sidecar_readiness(Duration::from_secs(60)).await?;

    let mut results = Vec::new();
    results.push(assert_startup_503(&startup_503_capture));

    // Run parity tests first (order-independent), then any order_hint=last.
    let (ordered_last, rest): (Vec<_>, Vec<_>) = filtered.iter().partition(|c| c.order_hint == Some("last"));
    for case in rest {
        let r = run_case(case, &compose).await;
        print_case_result(&r);
        results.push(r);
    }
    for case in ordered_last {
        let r = run_case(case, &compose).await;
        print_case_result(&r);
        results.push(r);
    }

    let summary = summarize(&results);
    print_summary(&summary);
    dump_logs_to_artifact_file(&compose, &summary, "sidecar/tests/parity/harness-run.log").await?;

    if args.stop || summary.all_passed {
        drop(_guard); // triggers compose down
    }
    if !summary.all_passed { std::process::exit(1); }
    Ok(())
}
```

Each `run_case` dispatches on `case.category` to a dedicated driver module (`model_proxy_driver.rs`, `egress_driver.rs`, `git_ssh_driver.rs`, `health_driver.rs`). Each module uses the appropriate client technique (reqwest for HTTPS, raw tokio TCP for egress, russh client for SSH) and returns a structured `CaseResult`.

`CaseResult` includes both sidecars' captured outputs AND the mock logs observed during the test window. Comparison logic (in a separate `compare.rs` module) runs normalization and diffing.

### Normalization + diffing

See FR-19. Output format on failure:

```
FAIL: openai_post_chat_completions_stream (model_proxy)
  Description: POST /openai/v1/chat/completions with stream:true

  HTTP status: Go=200, Rust=200 (match)

  Response body (normalized, SSE events):
    - event 0: match
    - event 1: DIFF
        Go:   data: {"choices":[{"delta":{"content":"p"}}]}
        Rust: data: {"choices":[{"delta":{"content":"p" }}]}   # trailing space
    - event 2: match
    - [DONE]: match

  Mock logs (mock-openai):
    Go saw request:   {method:POST, path:/v1/chat/completions, host:api.openai.com, body:...}
    Rust saw request: {method:POST, path:/v1/chat/completions, host:api.openai.com, body:...}
    Match.

  Docker logs: match

Corpus file: sidecar/tests/parity/corpus/openai_post_chat_completions_stream.json
```

## Migration plan

Six commits on branch `feat/sidecar-parity-harness`:

### Commit 1 — scaffolding + fixtures
- New workspace member at `sidecar/tests/parity/`
- `Cargo.toml` with full dep list
- Stub `src/main.rs` printing "harness not implemented"
- `fixtures/test-ca/` with ca.pem + ca.key + README.md generated by the committed `regenerate-test-ca.sh`
- `fixtures/mock-*/cert.pem` + `key.pem` for HTTPS mocks, signed by test-ca, SANs pinned
- `fixtures/go-secrets/` and `fixtures/rust-secrets/` populated
- Harness README with the overview
- Top-level workspace `Cargo.toml` member added
- `cargo build -p nautiloop-sidecar-parity-harness` green, `cargo clippy -p nautiloop-sidecar-parity-harness --all-targets -- -D warnings` green
- All committed files with test-only header comments

### Commit 2 — mock services + Dockerfiles
- `fixtures/mock-openai/server.py` implementing FR-7 (hypercorn-based)
- `fixtures/mock-anthropic/server.py` implementing FR-8
- `fixtures/mock-github-ssh/server.py` implementing FR-9 (paramiko) + HTTP introspection on 9999 (asyncio)
- `fixtures/mock-example/server.py` implementing FR-11
- `fixtures/mock-tcp-echo/server.py` implementing FR-12
- `Dockerfile.go-sidecar` (FR-4) — resurrect Go sidecar build
- `Dockerfile.go-with-test-ca` (FR-5) — Go sidecar with test CA appended
- Each mock has its own Dockerfile under `fixtures/mock-*/Dockerfile`
- `docker compose -f sidecar/tests/parity/docker-compose.yml build` succeeds for all images
- Manual smoke: `curl -k --cacert fixtures/test-ca/ca.pem https://localhost:<mapped-port>/v1/models` against mock-openai (after spinning up the compose stack) returns a deterministic response

### Commit 3 — docker-compose.yml + driver scaffolding
- Full `docker-compose.yml` with all services, `parity-net` network, `extra_hosts`, mounts, ports, env
- `docker compose up` brings the full stack up; all mock healthchecks pass
- Driver's `ComposeStack` helper implemented (build, up, down, wait_healthy)
- Driver's `ComposeGuard` implemented (NFR-7 Drop-based teardown)
- Driver can run `cargo run -p nautiloop-sidecar-parity-harness -- --stop` and cleanly bring up + tear down the stack

### Commit 4 — corpus + driver cases
- `corpus/` populated with all cases from FR-22
- Driver modules for model_proxy, egress, git_ssh, health implemented
- Normalization + diffing in `compare.rs`
- Mock log introspection client code
- First full `cargo run -p nautiloop-sidecar-parity-harness` succeeds — all parity tests green, divergence tests green in their divergent direction, artifact log file written

### Commit 5 — CI workflow + lint script extension
- `.github/workflows/ci.yml` per FR-24 — 4 jobs: rust-checks, rust-checks-with-test-utils, parity-harness, prod-leak-lint
- `sidecar/scripts/lint-no-test-utils-in-prod.sh` extended per FR-28 to also check for `NAUTILOOP_EXTRA_CA_BUNDLE` outside the parity-harness job
- CI run on the branch itself should pass all 4 jobs — this is the final gate before merge

### Commit 6 — polish, README, followups
- `sidecar/tests/parity/README.md` final version: prerequisites, how to run, how to debug failures, how to add a case, known limitations (DNS mock deferred per FR-23, rebinding test deferred)
- Followup issues filed for the deferred DNS mock work and rebinding coverage
- Workspace-level `deny.toml` check that the new deps are clean
- Any remaining TODO comments in the harness source resolved

## Test plan

### Harness self-checks

- `cargo build --workspace` green
- `cargo clippy --workspace --all-targets -- -D warnings` green
- `cargo clippy --workspace --all-targets --features nautiloop-sidecar/__test_utils -- -D warnings` green
- `cargo test --workspace` — no regressions in sidecar crate unit/integration tests
- `cargo test --workspace --features nautiloop-sidecar/__test_utils` — integration tests that require the feature (7 currently)
- `bash sidecar/scripts/lint-no-test-utils-in-prod.sh` — exits 0

### End-to-end runs

- `cargo run -p nautiloop-sidecar-parity-harness --release -- --stop` (cold, first run with build) completes in <10 minutes
- `cargo run -p nautiloop-sidecar-parity-harness --release -- --stop --no-rebuild` (warm) completes in <5 minutes per NFR-2
- All parity tests pass, all divergence tests pass
- Intentional regression check: temporarily modify mock-openai to drop a required header, re-run, harness reports a readable diff pointing at the correct corpus file
- Intentional regression check: temporarily flip the expected divergence direction in one divergence test, re-run, that test fails

### Divergence assertion validity checks

For each of the 3 active divergence cases (DNS-error case is scope-reduced), verify that:
- If the Rust sidecar were "fixed" to match Go (bug-compatible behavior), the corresponding test would fail
- Verified manually by temporarily hacking the harness assertion to check the wrong direction

### Manual smoke (out of harness)

- After `docker compose up`, manually:
  - `curl http://localhost:19090/openai/v1/models -H 'Authorization: Bearer x'` returns the expected mock response with sidecar-injected Bearer
  - `ssh -p 19091 -i fixtures/go-secrets/ssh-key/id_ed25519 git@localhost git-upload-pack 'test/repo.git'` returns mock pack bytes
  - Same against 29090/29091 hits the Rust sidecar and returns identical results
- Manually verify `curl http://localhost:19093/healthz` returns 200 before the harness flips to ready (proves the 503 window is observable)

### CI

- Push the harness branch and confirm all 4 CI jobs pass:
  - `rust-checks`
  - `rust-checks-with-test-utils`
  - `parity-harness`
  - `prod-leak-lint`
- On a deliberate failure injection, confirm the CI job fails AND uploads the artifact log file
- On a PR that doesn't touch `sidecar/`, confirm the `parity-harness` job is SKIPPED via the path filter (saves CI minutes)

## Security considerations

This spec introduces test infrastructure that ships test certificates, keys, and mock credentials. Every piece is test-only, committed with loud headers, and cannot run against production hosts because the hostnames are overridden via `extra_hosts` inside a sandboxed Docker network.

### Non-negotiables (no reviewer leniency)

1. **No production certificates, keys, or credentials appear anywhere in `sidecar/tests/parity/fixtures/`.** Every file is freshly generated per SR-7 and committed with a test-only header comment.
2. **`NAUTILOOP_EXTRA_CA_BUNDLE` is set only in the harness compose file and only in the parity-harness CI job.** FR-28's lint enforces this with a CI gate.
3. **No `NAUTILOOP_SSRF_TEST_ALLOWLIST` or equivalent bypass exists.** SSRF is handled via CGNAT network addressing instead of code bypass.
4. **Test CA private key is committed but clearly marked test-only.** README + header comments + file path. Cannot sign certificates for real hostnames (CA is self-signed and untrusted by anything outside the harness).
5. **The harness SSH key's trust boundary is mock-github-ssh only.** The mock's `authorized_keys` trusts exactly one fingerprint. That key, if obtained, allows spoofing tests locally but not against real GitHub.

### New risks specific to this spec

1. **CGNAT network address leakage.** If the host machine has a VPN or other network interface in 100.64.0.0/10, there could be routing confusion. Mitigation: use 100.64.0.0/24 (smaller subnet), document the collision risk in the README, and verify at Docker network creation time that the subnet doesn't overlap existing routes.
2. **Python dependency supply chain.** The mock services use Python with hypercorn, uvicorn, starlette, paramiko. These are new to the repo. The Dockerfiles pin exact versions (`pip install hypercorn==0.17.3 paramiko==3.4.0 ...`) to prevent drift. A new advisory against any of these packages should trigger a rebuild.
3. **Docker compose network teardown on panic.** If the harness driver panics mid-run, the Drop guard in NFR-7 runs `docker compose down -v`. If that also panics, dangling containers linger. Mitigation: the CI job runs `docker compose down -v --remove-orphans` in an `always()` step as belt-and-braces.
4. **CGNAT collision with real CGNAT users.** Some ISPs use CGNAT (100.64.0.0/10) for customer-facing NAT. If the developer is on such an ISP AND docker's bridge networking conflicts with the VPN/ISP route, there could be issues. Workaround: the README documents a `subnet` override flag so developers can pick a different non-private range if needed.

### What this spec does NOT change

- Production sidecar behavior — zero code changes to `sidecar/src/` or `images/sidecar/main.go`.
- Production Dockerfile — unchanged; Go sidecar Dockerfile is NEW (harness-only), but does not affect production.
- K8s manifests — unchanged.

## Out of scope

- **True DNS rebinding testing.** Deferred to followup (see FR-23). The harness tests upstream routing smoke only.
- **True hermetic DNS SERVFAIL divergence testing.** Deferred to followup. The SSRF fail-closed property is verified by unit tests in `sidecar/src/ssrf.rs`.
- **Fault injection testing.** The harness doesn't inject random mock failures. That's a followup for chaos testing.
- **Performance benchmarking.** Not a performance harness. Parity only.
- **Real upstream smoke tests.** Covered by phase 5's manual checklist, not this harness.
- **`cargo-deny` for the harness crate.** Inherits workspace `deny.toml`; no new deny config in this spec.

## Open questions

1. **Python version pinning.** The mock Dockerfiles use `python:3.12-slim`. If 3.12 reaches EOL before this harness is retired, the images need a bump. Tracked implicitly via Docker base image freshness; no blocker.
2. **paramiko SSH server flush behavior.** Paramiko is a client-first library; its server-side channel flush after `ExitStatus` has been known to race on some versions. Phase 2 of the migration plan verifies this works; if it doesn't, the implementer falls back to `asyncssh` (also Python, more server-oriented). Not blocking the spec.
3. **Mock introspection port exposure in CI.** The FR-18 note about host-mapping the introspection ports for developer workflows — in CI, the harness driver runs INSIDE the same Docker network as a separate container, so it has direct access to `100.64.0.X:9999`. For developer workflows on the host, the mapped ports are used. The compose file defines both, and the driver auto-detects. Not blocking.
4. **CI runtime budget.** The 10-minute timeout in FR-25 is based on the NFR-2 target. If the first few CI runs exceed this, it's tuned. Not blocking.

None are blocking. The spec is implementable as written.

## Changelog

### v1 → v2: Codex adversarial review (14 findings: 3 P0, 11 P1)

- **P0-1 (extra_hosts + SSRF collision):** FIXED. v2 uses a custom Docker bridge in 100.64.0.0/24 (CGNAT) instead of the default RFC1918 bridge. Neither sidecar's SSRF logic blocks CGNAT (verified against `sidecar/src/ssrf.rs:94-99` and `images/sidecar/main.go:43-48`). Zero code changes to either sidecar required. Much cleaner than the test-only env var I initially considered.
- **P0-2 (service count contradiction):** FIXED. v2 lists 7 concrete services (sidecar-go, sidecar-rust, mock-openai, mock-anthropic, mock-github-ssh, mock-example, mock-tcp-echo) with no "exactly N" claim. The DNS mock from v1's FR-15 is deferred to followup (see FR-23).
- **P0-3 (SidecarOutput doesn't include mock logs):** FIXED. FR-13 defines mock introspection endpoints on port 9999 per mock. FR-18 steps 1 and 4 explicitly reset and capture mock logs per test case. FR-19 normalizes and diffs mock logs alongside sidecar outputs.
- **P1-4 (reqwest can't do raw proxy semantics):** FIXED. FR-4 discussion + Architecture notes the driver uses `reqwest` for HTTPS (model_proxy) and raw `tokio::net::TcpStream` for egress cases. Each category has its own driver module.
- **P1-5 (health/startup impossibility):** FIXED. FR-3 explicitly removes Docker healthchecks from the sidecars. FR-17 step 4 captures the 503 window BEFORE the readiness poll loop. The harness driver, not Docker, gates on sidecar readiness.
- **P1-6 (mock log reset nondeterminism):** FIXED. FR-13 separates healthcheck traffic (`/_healthz` on main port) from test traffic (mock introspection on port 9999) into distinct ASGI apps, so healthcheck probes don't pollute the per-test log arrays.
- **P1-7 (CONNECT drain lacks upstream):** FIXED. v2 adds `mock-tcp-echo` as a dedicated raw-TCP service at 100.64.0.14:443. The CONNECT drain test targets it explicitly. Ordering is enforced via `order_hint: "last"` in the corpus JSON schema (FR-21), not by filename ordering.
- **P1-8 (image freshness undefined):** FIXED. FR-16 mandates `docker compose build` before every run unless `--no-rebuild` is passed.
- **P1-9 (Go Dockerfile reference wrong):** FIXED. FR-4 defines a new `Dockerfile.go-sidecar` that resurrects the Go build from `images/sidecar/main.go` (still on main per phase 6). FR-5 layers the test CA on top of it. v1's reference to the current `images/sidecar/Dockerfile` (which is now the Rust build after PR #63) is no longer relied on.
- **P1-10 (TLS fixture params under-specified):** FIXED. SR-7 pins RSA-2048 + SHA-256 + `basicConstraints=critical,CA:TRUE` for the CA and RSA-2048 + SAN matching the hostname for each mock service cert. Exact openssl commands committed.
- **P1-11 (rebinding case mislabeled):** FIXED. FR-23 documents the scope reduction. The case is renamed from "rebinding" to explicit upstream-routing smoke, and true rebinding coverage is explicitly deferred to a followup.
- **P1-12 (CI deferral removes enforcement):** FIXED. v2 adds FR-24 defining a new `.github/workflows/ci.yml` with 4 jobs including `parity-harness`. The harness runs on every PR touching sidecar code. Without CI enforcement, the harness was ceremony — v2 fixes this.
- **P1-13 (SR-6 mitigation is fake without CI):** FIXED. FR-28 extends the existing lint script to check `NAUTILOOP_EXTRA_CA_BUNDLE` references, AND FR-24's `prod-leak-lint` CI job runs the script on every PR. The enforcement is real.
- **P1-14 (corpus and mock API contracts misaligned):** FIXED. FR-22's corpus has been rewritten to match the actual sidecar routing exactly: `POST /v1/chat/completions` (with `stream:true` in body, not a separate `/stream-sse` path), `GET /v1/models` (not `GET /v1/messages` for anthropic). Every case's HTTP method + path now matches a mock endpoint defined in FR-7, FR-8, or FR-11.
