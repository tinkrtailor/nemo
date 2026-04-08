# Sidecar: Containerized Parity Test Harness

## Overview

Phase 4 of the Rust sidecar migration plan. Build a containerized parity test harness that runs the Go sidecar and the Rust sidecar side-by-side against the same hermetic inputs, diffs their outputs, and gates the cutover decision.

This is the single biggest remaining production-readiness blocker before phase 5 (K8s cutover). Harness runs locally AND in CI on every PR touching `sidecar/` — without CI enforcement, the harness is ceremony.

## Baseline

Main at merge of PR #73 (`63d61e8`). The Rust sidecar is committed at `sidecar/` with 93 unit tests + 7 integration tests passing. The Go sidecar is kept per the migration plan until phase 6. Both binaries compile.

## Problem Statement

After three review passes on the rust-sidecar spec (codex v1, v2, v3) and one followup batch review (PR #73), the Rust sidecar compiles, passes clippy, passes 100+ tests, and has been adversarially reviewed to the point of diminishing returns. **What we still lack is evidence of behavior parity against the Go implementation under realistic conditions** AND a mechanism that prevents future regressions from slipping through.

The harness this spec builds is the difference between "we think it's behavior-identical" and "we have evidence it's behavior-identical AND CI rejects any change that breaks it."

### Key design decision up front: use CGNAT addresses (RFC6598) for the Docker network

A naïve design would use Docker's default bridge network, which allocates IPs in `172.16.0.0/12` (RFC1918). But both sidecars' SSRF checks fail-closed on RFC1918 addresses, so mapping `api.openai.com` → `172.x` via `extra_hosts` would make every test return HTTP 403 SSRF-block, not upstream responses.

The clean fix: **use a custom Docker bridge network in `100.64.0.0/10` (RFC6598 CGNAT)**. Neither sidecar blocks CGNAT. Verified in `sidecar/src/ssrf.rs:94-99` (explicit comment: "we DO NOT block 100.64.0.0/10") and `images/sidecar/main.go:43-48` (privateRanges list only has RFC1918 + link-local + loopback). So mock service IPs like `100.64.0.10` are indistinguishable from public internet addresses to the sidecars' SSRF logic, and the harness proceeds as intended.

**No code changes to either sidecar are required.** No test-only feature flags. Just a `subnet: 100.64.0.0/24` line in the compose file's network section (overridable via env var — see FR-29).

## Dependencies

- **Requires (committed):** PR #63 (Rust sidecar merged), PR #73 (followups merged including `__test_utils` feature), PR #56 (Go sidecar health bind fix). All three are on main.

### Issue #66: Go SSE streaming — NOT FIXED, DOCUMENTED DIVERGENCE

Issue [#66](https://github.com/tinkrtailor/nautiloop/issues/66) — Go sidecar `modelProxyHandler` does not flush SSE streaming responses. `io.Copy(w, resp.Body)` keeps bytes in the ~4 KiB `http.ResponseWriter` buffer until upstream closes. Breaks every review/audit loop on v0.2.10.

**The Go sidecar will NOT be fixed.** The earlier parallel branch that was developing a `streamBody` helper (`fix/sidecar-sse-flush`) is being abandoned. The **Rust sidecar is THE fix for #66** — it uses hyper 1.x `BoxBody` which streams frames immediately via the http1 codec, without the ResponseWriter buffer-then-flush-on-full behavior that bit Go.

**Implications:**
- The harness's SSE streaming cases are reframed as **documented divergences**, not parity tests (see FR-22 divergence category): Rust delivers SSE chunks incrementally before upstream close, Go delivers nothing until upstream close (or delivers all buffered bytes at once at close time).
- The complete divergence set is **5 cases total**: `divergence_sse_streaming_openai`, `divergence_sse_streaming_anthropic`, `divergence_bare_exec_upload_pack_rejection`, `divergence_bare_exec_receive_pack_rejection`, `divergence_connect_drain_on_sigterm`. The earlier SSRF-DNS-fail-closed and DNS-rebinding cases are NOT in the divergence list — they've been moved out of harness scope (covered by unit tests and code review respectively, see FR-23).
- **Phase 5 cutover urgency is elevated**: the current release (v0.2.10) is broken for opencode/SSE, and the only working path is to retire the Go sidecar. The parity harness does double duty: (a) proves Rust is behavior-identical where it should be, (b) proves Rust fixes Go's broken streaming in the documented-divergence direction. Once this spec lands, phase 5 is a P0 production fix — not a cleanup migration.

### Rust-side streaming — trust and verify

Per the design decision to proceed without a separate manual verification step, **this spec trusts that hyper 1.x's `BoxBody` streams frames immediately through the http1 codec**. Evidence for the trust: `sidecar/src/model_proxy.rs:266-278` uses `body.boxed()` and passes the result directly to `Response::builder().body(...)`, with an inline comment `"Each frame flows through hyper's client transport as it arrives"`. Hyper 1.x has no equivalent to Go's ResponseWriter 4 KiB buffer-then-flush-on-full behavior.

**But the harness is where this trust becomes evidence.** The `divergence_sse_streaming_openai` and `divergence_sse_streaming_anthropic` cases below are the FIRST real proof that Rust streams correctly under wall-clock conditions. If Rust turns out to also be buffered, the harness catches it on the first full run — not in production. That's acceptable risk because:

1. The harness runs in a hermetic test environment before any production impact.
2. If Rust is broken, we file a new P0 issue immediately and fix it — blocking the harness merge, not the cutover.
3. The hyper 1.x body-streaming design is well-documented and used in countless production services.

The alternative (manually verifying Rust streaming before writing the spec) was considered and rejected: it would block spec progress on a manual step that the harness itself is designed to replace.

- **Enables:** phase 5 cutover with actual parity evidence + CI enforcement + documented evidence that Rust fixes #66. Unblocks retiring the Go sidecar AND fixes v0.2.10's streaming breakage.
- **Blocks:** nothing.

## Requirements

### Functional Requirements

#### Harness layout and services

- FR-1: The harness lives at `sidecar/tests/parity/` with this structure:
  ```
  sidecar/tests/parity/
  ├── docker-compose.yml                # orchestrates all services
  ├── Dockerfile.go-sidecar             # builds Go sidecar (see FR-4)
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
  │   ├── mock-tcp-echo/                # raw TCP echo server, dual use:
  │   │   ├── server.py                 #   - CONNECT drain test target
  │   │   └── Dockerfile                #   - egress CONNECT tunneling target
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

- FR-2: `docker-compose.yml` shall define a custom bridge network `parity-net` with `subnet: ${PARITY_NET_SUBNET:-100.64.0.0/24}` (see FR-29 for the override mechanism) and these 7 services. **Every port the harness driver needs from the host is published explicitly** (healthcheck, manual smoke, introspection). The driver reaches mocks via `localhost:<published-port>`; the sidecars reach mocks internally via `parity-net` IPs.
  1. **`sidecar-go`** — Go binary built from `Dockerfile.go-with-test-ca`. Container ports 9090-9093 published to host ports 19090-19093. Mounts `go-secrets/` at `/secrets/`. Env: `GIT_REPO_URL=git@github.com:test/repo.git`. `networks: parity-net: ipv4_address: 100.64.0.20`. `extra_hosts`: `api.openai.com:100.64.0.10`, `api.anthropic.com:100.64.0.11`, `github.com:100.64.0.12`, `mock-example:100.64.0.13`, `egress-target:100.64.0.14` (dedicated hostname for egress CONNECT tests, points at mock-tcp-echo).
  2. **`sidecar-rust`** — Rust binary from `images/sidecar/Dockerfile`. Container ports 9090-9093 published to host ports 29090-29093. Mounts `rust-secrets/` at `/secrets/` AND `fixtures/test-ca/ca.pem` read-only at `/test-ca/ca.pem`. Env: `GIT_REPO_URL=git@github.com:test/repo.git`, `NAUTILOOP_EXTRA_CA_BUNDLE=/test-ca/ca.pem`. Same `extra_hosts`. `ipv4_address: 100.64.0.21`.
  3. **`mock-openai`** — HTTPS server on `100.64.0.10:443` (TLS cert SAN = api.openai.com), plain HTTP on `:80` (healthcheck + introspection mux). Published host ports: `:80 → 50010` (driver health polling), `:443 → 50011` (manual smoke `curl`), `:9999 → 49990` (introspection).
  4. **`mock-anthropic`** — HTTPS server on `100.64.0.11:443` (TLS cert SAN = api.anthropic.com), plain HTTP on `:80`. Published: `:80 → 50020`, `:443 → 50021`, `:9999 → 49991`.
  5. **`mock-github-ssh`** — paramiko SSH server on `100.64.0.12:22`. Health TCP listener on `:2200`. Published: `:2200 → 50030` (driver health polling via TCP connect), `:9999 → 49992` (introspection). **`:22` is NOT published** — the sidecars reach it via `parity-net`, the driver never needs to connect directly.
  6. **`mock-example`** — plain HTTP server on `100.64.0.13:80` AND `100.64.0.13:8080` (same handlers on both, used for the `with_port` egress case). Published: `:80 → 50040` (driver health polling), `:8080 → 50041` (reserved for potential host-side manual tests; not required for core operation), `:9999 → 49993` (introspection).
  7. **`mock-tcp-echo`** — raw TCP echo server on `100.64.0.14:443`. Dual use: the egress CONNECT tests target it via the `egress-target` hostname from within the sidecars, and the CONNECT drain SIGTERM test uses it. Published: `:443 → 50050` (driver health polling via TCP connect only; no application-level use from host). No introspection (echo has nothing to log beyond byte counts, which the driver captures through its sidecar-client connection).

- FR-3: Both sidecars shall depend on all mock services being healthy before starting, via Compose `depends_on` with `condition: service_healthy`. The sidecars themselves do NOT get Docker-level healthchecks. The harness driver polls `/healthz` via `reqwest` to gate on sidecar readiness AFTER compose reports mock services healthy.

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
  The Go binary reads `/etc/ssl/certs/ca-certificates.crt` via `crypto/x509.SystemCertPool()` on Linux, populated from that file on Alpine-derived bases. The scratch image trusts the test CA in addition to whatever Mozilla roots ship with Alpine's `ca-certificates` package.

- FR-6: The Rust sidecar image (production `images/sidecar/Dockerfile`) is used AS-IS. The test CA is loaded at runtime via `NAUTILOOP_EXTRA_CA_BUNDLE=/test-ca/ca.pem`, exercising the production code path from `sidecar/src/tls.rs:60` onward. Verified: rustls APPENDS the extra CA to the `webpki-roots` default store rather than replacing it.

#### Mock services behavior

- FR-7: **`mock-openai`** shall respond to these paths (and ONLY these paths; anything else → 404):
  - `GET /_healthz` → 200 `{"ok":true}` (plain HTTP on port 80)
  - `GET /v1/models` → 200 with deterministic JSON body
  - `POST /v1/chat/completions` (non-streaming, no `stream:true` in body) → 200 with deterministic JSON body
  - `POST /v1/chat/completions` with `stream: true` in the JSON body → 200 `Content-Type: text/event-stream`, streams 3 SSE events with deterministic content spaced 100ms apart (to make wall-clock assertions on inter-chunk delays meaningful), then `data: [DONE]\n\n` and closes. Server MUST flush between events.

- FR-8: **`mock-anthropic`** shall respond to:
  - `GET /_healthz` → 200 (plain HTTP on port 80)
  - `POST /v1/messages` (non-streaming) → 200 with deterministic JSON body
  - `POST /v1/messages` with `stream: true` → 200 SSE stream with 3 deterministic events spaced 100ms apart
  - Anything else → 404

- FR-9: **`mock-github-ssh`** (paramiko-based):
  - Accepts any SSH client key
  - Recognizes `exec` requests with commands `git-upload-pack <path>` and `git-receive-pack <path>`
  - For `git-upload-pack 'test/repo.git'` or `git-upload-pack test/repo.git`: write deterministic bytes to channel stdout, send `ExitStatus(0)`, close channel
  - For `git-receive-pack 'test/repo.git'`: consume input bytes from channel stdin, write a deterministic acknowledgement to stdout, send `ExitStatus(0)`, close channel
  - For any other exec command OR any other repo path: write an error message to stderr, send `ExitStatus(128)`, close channel
  - `env`, `pty-req`, `subsystem`, `shell`, `x11-req` requests: reject by returning `False` from the paramiko request handler
  - Healthcheck: separate plain TCP listener on port 2200 that accepts + closes. Used by docker-compose `depends_on` healthcheck via `nc -z localhost 2200`.

- FR-10: **Mock service Python implementation constraints.** All mock HTTP services use `hypercorn` with `asyncio` for reliable SSE flushing (Python's stdlib `http.server` does not flush streaming chunks). Python version pinned: `python:3.12-slim`. Dependencies pinned exactly in each mock's Dockerfile:
  ```dockerfile
  RUN pip install --no-cache-dir \
      hypercorn==0.17.3 \
      uvicorn==0.30.6 \
      starlette==0.38.6 \
      paramiko==3.4.0
  ```
  The implementer MUST manually verify SSE flush behavior by running `curl -N --cacert fixtures/test-ca/ca.pem https://localhost:50011/v1/chat/completions -H 'Content-Type: application/json' -d '{"stream":true,"model":"gpt-4","messages":[{"role":"user","content":"ping"}]}'` (and the equivalent `https://localhost:50021/v1/messages` for anthropic) before writing any harness cases and confirming inter-chunk delays are observable (~100ms as configured).

- FR-11: **`mock-example`** (plain HTTP, dual port):
  - Listens on BOTH `:80` AND `:8080` inside the container. Same routes on both ports.
  - `GET /_healthz` → 200 (docker-compose healthcheck target)
  - `GET /foo` → 200 with fixed body
  - `GET /redirect` → 302 with `Location: /foo` (used to verify sidecars do NOT follow redirects)
  - Anything else → 404

- FR-12: **`mock-tcp-echo`** (raw TCP on port 443): accepts connections, echoes every byte back, never closes until the client closes. Dual use:
  - **Egress CONNECT tests** target it via the `egress-target` hostname (maps to `100.64.0.14` via `extra_hosts`) or directly as `mock-tcp-echo:443`.
  - **CONNECT drain on SIGTERM test** (the `order_hint: "last"` case) tunnels through the sidecar to this target.

  Healthcheck: docker-compose uses a TCP connect probe to `127.0.0.1:443` (the service port) — no `/_healthz` endpoint needed for a raw TCP service.

- FR-13: **Mock log introspection.** Every mock service that has request/response semantics exposes an HTTP introspection endpoint on a dedicated plain-HTTP port `:9999`:
  - `GET http://localhost:<published-port>/__harness/logs` → JSON array of every observed request since last reset, with fields `{id, timestamp, method, path, host_header, headers, body_b64, source_ip}`
  - `POST http://localhost:<published-port>/__harness/reset` → clears the in-memory log array
  - Healthcheck requests (`/_healthz` on port 80 of the same mock) are **NEVER logged** to the introspection store. The main app explicitly skips the `/_healthz` route when recording, so health probes cannot pollute test traffic.

  **Published host ports for introspection** (per FR-2):
  - mock-openai → host port 49990
  - mock-anthropic → host port 49991
  - mock-github-ssh → host port 49992
  - mock-example → host port 49993

  The harness driver runs on the host and connects to introspection via `http://localhost:4999X`. This is a single, portable access strategy that works on Docker Desktop for macOS, Windows, and Linux without requiring the driver to attach to `parity-net` as a container.

  `mock-tcp-echo` has NO introspection endpoint because it has nothing interesting to log beyond byte counts, which the harness driver captures directly by measuring bytes through its client connection.

- FR-14: **Mock-github-ssh log introspection.** The SSH mock runs the paramiko SSH server on port 22 AND a separate asyncio HTTP server on port 9999 (in the same process, different event loops or threads). The HTTP server implements the same `/__harness/logs` + `/__harness/reset` API from FR-13. `logs` returns SSH exec commands observed, bytes read/written, channel request types, and any authentication events.

#### Harness driver

- FR-15: The harness driver is a new workspace crate at `sidecar/tests/parity/` named `nautiloop-sidecar-parity-harness`. Binary-only, `src/main.rs`, no `lib.rs`. Separate Cargo workspace member.

- FR-16: **Image freshness is enforced by default.** The driver runs `docker compose build` before every run unless `--no-rebuild` is passed. Rebuilds the sidecar-rust image from `images/sidecar/Dockerfile`, the sidecar-go image from `Dockerfile.go-with-test-ca`, and each mock image from its respective Dockerfile.

- FR-17: The driver shall:
  1. Run `docker compose build` (unless `--no-rebuild`)
  2. Run `docker compose up -d`
  3. Wait for mock services to be healthy via these concrete polls (host-port addresses from FR-2):
     - `mock-openai`: HTTP GET `http://localhost:50010/_healthz` → expect 200
     - `mock-anthropic`: HTTP GET `http://localhost:50020/_healthz` → expect 200
     - `mock-example`: HTTP GET `http://localhost:50040/_healthz` → expect 200
     - `mock-github-ssh`: TCP connect to `localhost:50030` → expect successful connect
     - `mock-tcp-echo`: TCP connect to `localhost:50050` → expect successful connect

     Fail with a clear error naming the specific mock if any service isn't healthy within 60s.
  4. Wait for sidecar readiness by polling `http://localhost:19093/healthz` and `http://localhost:29093/healthz` with 200ms exponential backoff up to 30s. Expect both to return 200 `{"status":"ok"}`. **There is no 503 startup-window observation in the parity suite.** See "Test cases removed" at the bottom of FR-22.
  5. Run the filtered corpus
  6. If `--stop` is set OR all tests passed: `docker compose down -v --remove-orphans`. Otherwise leave the stack up for inspection.

- FR-18: For each test case, the driver shall:
  1. POST `http://localhost:49990-49993/__harness/reset` (skip mock-tcp-echo which has no introspection) to clear per-mock log arrays
  2. Issue the test input to BOTH sidecars in parallel (against host ports 19090-19093 for Go, 29090-29093 for Rust)
  3. Capture from each side: HTTP status code, response headers (subset), response body, container log lines (via `docker logs <container> --since <test-start-time> --timestamps`), SSH exit codes, SSH stderr, AND wall-clock timestamps for each captured chunk (required for the SSE streaming divergence case — FR-22)
  4. After the request completes, GET `http://localhost:49990-49993/__harness/logs` and capture what each mock observed. Attribute observed requests to Go vs Rust by `source_ip` (Go = 100.64.0.20, Rust = 100.64.0.21).
  5. Normalize per FR-19 and diff Go vs Rust
  6. For the documented divergences (FR-22), flip the assertion

- FR-19: Normalization rules applied before comparing outputs:
  - Log lines: strip `timestamp` field entirely. All other fields compared verbatim.
  - HTTP response headers: compare `Content-Type` + case config. Strip `Date`, `Server`, `Via`, `X-Request-Id`, `Connection`, `Content-Length`.
  - Response bodies: per-test config can specify fields to strip.
  - SSH stderr: trim trailing whitespace.
  - Mock log entries: strip `id` and `timestamp` fields; sort by `(path, method, source_ip)` so concurrent interleaving doesn't affect comparison.
  - Docker log stdout/stderr: sort by normalized content within the per-test time window.

- FR-20: The driver shall support filtering:
  - `--category model_proxy` — runs only the model_proxy cases
  - `--category divergence` (formerly `--only-divergence`) — runs only the divergence cases (5 cases total — see FR-22)
  - `--case <case_name>` — runs a single case
  - `--stop` — tear down the stack after the run regardless of outcome
  - `--no-rebuild` — skip `docker compose build`
  - `--subnet <cidr>` — override the CGNAT subnet (wraps `PARITY_NET_SUBNET` env var — see FR-29)

#### Test corpus

- FR-21: The corpus lives in `sidecar/tests/parity/corpus/` as JSON files. One case per file. Schema:
  ```json
  {
    "name": "test_case_name",
    "category": "model_proxy" | "egress" | "git_ssh" | "health" | "divergence",
    "description": "human-readable",
    "input": { ... category-specific ... },
    "expected_parity": true | false,
    "divergence": null | { "description": "...", "go_expected": "...", "rust_expected": "..." },
    "normalize": { "body_strip_fields": ["id"], "extra_header_strip": ["X-Whatever"] },
    "order_hint": "last" | null
  }
  ```
  `order_hint: "last"` forces a case to run after all others. Used only by `divergence_connect_drain_on_sigterm` which kills containers. At most one case may have `order_hint: "last"`; the driver panics at startup if more than one case has this hint.

- FR-22: **Corpus contents (5 categories, 5 documented divergences):**

  **Model proxy parity (10 cases — streaming cases moved to divergence):**
  - `openai_get_v1_models` — GET `/openai/v1/models`. Assert upstream received `Host: api.openai.com`, `Authorization: Bearer sk-test-openai-key`.
  - `openai_post_chat_completions_nonstream` — POST `/openai/v1/chat/completions` body `{"model":"gpt-4","messages":[{"role":"user","content":"ping"}]}`. Assert upstream received the exact body + auth header. (Non-streaming is parity because both sidecars handle it identically — the #66 bug only affects streaming bodies.)
  - `anthropic_post_v1_messages_nonstream` — POST `/anthropic/v1/messages` body `{"model":"claude","messages":[...]}`. Assert upstream received `x-api-key: sk-ant-test-key`, `anthropic-version: 2023-06-01`, `Host: api.anthropic.com`.
  - `openai_bare_prefix` — GET `/openai` → upstream path `/`
  - `anthropic_bare_prefix` — GET `/anthropic` → upstream path `/`
  - `unknown_route_returns_403` — GET `/some/unknown/path` → 403 with exact Go error body. Assert mock services observed NO request.
  - `openai_client_auth_header_overwritten` — GET with client `Authorization: Bearer client-supplied-fake`. Assert mock observed sidecar-injected key.
  - `anthropic_client_api_key_overwritten` — POST with client `x-api-key: client-supplied-fake`. Assert mock observed sidecar-injected value.
  - `anthropic_client_version_passthrough` — POST with client `anthropic-version: 2022-01-01`. Assert mock observed client value (passed through).
  - `openai_credential_refresh_per_request` — request 1, mutate credentials file on disk, request 2, assert request 2 uses new credentials on BOTH sides.

  **Egress parity (6 cases):**
  - `egress_connect_egress_target` — CONNECT `egress-target:443` via the egress port. Assert tunnel established (bytes echo through mock-tcp-echo). Log destination `"egress-target:443"`.
  - `egress_connect_egress_target_no_port` — CONNECT `egress-target` (no port). Log destination `"egress-target:443"` (synthesized).
  - `egress_http_get_example` — GET `http://mock-example/foo` via egress port (plain HTTP proxy, absolute-form request URI). Log destination `"mock-example"` (no port, matches Go's `URL.Host`).
  - `egress_http_get_example_with_port` — GET `http://mock-example:8080/foo`. Log destination `"mock-example:8080"`. (mock-example listens on both :80 and :8080 per FR-11.)
  - `egress_http_origin_form_repair` — raw write of `GET /foo HTTP/1.1\r\nHost: mock-example\r\n\r\n` to the egress port. No scheme, no host in the request line (origin-form URI). Assert the sidecar repairs `URL.scheme=http` and `URL.host=mock-example` and forwards to `http://mock-example/foo`. Assert mock-example observed exactly one GET for `/foo`. (This is the case that motivated raw `tokio::net::TcpStream` in the driver — `reqwest` won't let you send malformed proxy requests.)
  - `egress_http_strips_proxy_connection` — GET via egress with `Proxy-Connection: keep-alive`. Assert mock-example observed NO `Proxy-Connection` header.
  - `egress_http_no_redirect_follow` — GET `http://mock-example/redirect`. Assert client received 302, mock-example observed exactly 1 request (not 2).

  **Also parity (was v2 divergence, scope-reduced):**
  - `egress_dns_error_both_fail_502` — egress GET to `http://deliberately-unresolvable.invalid/`. Assert BOTH sidecars return 502. This is **no longer a divergence case** — the DNS-error SSRF fail-closed property is verified by unit tests in `sidecar/src/ssrf.rs`, not the harness. This case just asserts basic error-path parity.

  **Git SSH parity (5 cases):**
  - `ssh_upload_pack_matching_repo` — exec `git-upload-pack 'test/repo.git'`. Exit status 0, bytes match mock pack.
  - `ssh_receive_pack_matching_repo` — exec `git-receive-pack 'test/repo.git'` with push bytes. Exit status 0.
  - `ssh_wrong_repo_path_rejected_locally` — exec `git-upload-pack 'wrong/repo.git'`. Exit status 1 from BOTH. Mock observes zero exec events.
  - `ssh_rejects_non_git_exec` — exec `ls /etc`. Exit status 1. Mock observes zero.
  - `ssh_rejects_env_request` — send `env` request before exec. Channel failure, no exit status. Mock observes zero.

  **Health parity (2 cases — `healthz_pre_ready_returns_503` removed, see note below):**
  - `healthz_post_ready_returns_200` — GET `/healthz` after readiness barrier. Expect 200 `{"status":"ok"}`.
  - `healthz_head_method_parity` — HEAD `/healthz` after readiness. Expect 200 (matches Go's mux which does not method-check).

  **Documented divergences (5 distinct cases, each with its own corpus file per FR-21 one-file-per-case schema, must FAIL if Go and Rust match):**
  - `divergence_sse_streaming_openai` **(NEW — covers #66, OpenAI path)** — POST `/openai/v1/chat/completions` with body `{"model":"gpt-4","stream":true,"messages":[{"role":"user","content":"ping"}]}`. Mock-openai streams 3 SSE events spaced 100ms apart (total stream duration ~300ms).

    **Primary assertion: wall-clock time from request send to FIRST chunk received on the client side.**
    - **Rust expected: first chunk arrives within 200ms of request send** (the mock emits the first chunk almost immediately after receiving the request, so if Rust streams, the first chunk should reach the client in one round-trip time — ~10-50ms — plus any trivial processing overhead).
    - **Go expected: first chunk arrives ≥250ms after request send** (because Go's `http.ResponseWriter` buffers until either the ~4 KiB buffer fills OR the upstream closes; with 3 small SSE events, the buffer never fills, so Go doesn't emit anything until upstream close at ~300ms — minus a small margin for scheduling the "upstream closed → flush response" path).

    The 50ms gap between the Rust bound (<200ms) and the Go bound (≥250ms) is the decision boundary. On a quiet machine this is comfortably wide; on a noisy CI runner we may see wider variance. If CI flakes, widen the gap by slowing the mock spacing (e.g. 200ms per chunk, Rust bound <300ms, Go bound ≥450ms) rather than blurring the assertion.

    The inter-chunk spacing (whether Rust's chunks 1-2-3 arrive with ~100ms between them) is a **secondary confirmation check** — nice to have but not the normative assertion. The normative check is time-to-first-chunk.

    **Not a valid pass:** Rust's chunks arriving with ≥200ms inter-chunk spacing means Rust is streaming but slowly; that's still a PASS for "Rust streams." Rust's first chunk arriving ≥200ms after request send means Rust is buffered and has the same bug as Go; that's a FAIL and triggers a NEW P0 issue against Rust.
  - `divergence_sse_streaming_anthropic` **(NEW — covers #66, Anthropic path)** — same test shape as the OpenAI case, but POST `/anthropic/v1/messages` with `stream:true` against mock-anthropic. Separate corpus file, same assertion thresholds. Testing both providers independently because Rust's model_proxy has a separate routing branch per provider (`UpstreamKind::OpenAi` vs `UpstreamKind::Anthropic`) and either path could regress independently.
  - `divergence_bare_exec_upload_pack_rejection` — exec `git-upload-pack` (no path argument). Rust exits with 1 (sidecar reject), Go exits with 128 (mock reject, because Go's SSH proxy forwards). Mock-github-ssh logs: 1 exec observed from Go (source_ip 100.64.0.20), 0 from Rust (source_ip 100.64.0.21).
  - `divergence_bare_exec_receive_pack_rejection` — same for `git-receive-pack`.
  - `divergence_connect_drain_on_sigterm` — establish CONNECT tunnel through egress port to `egress-target:443` (mock-tcp-echo). Begin trickling bytes at 1 byte per 100ms. After 500ms steady traffic, `docker kill --signal SIGTERM` each sidecar container. Measure time from SIGTERM to tunnel bytes stopping. Assert Go stops within 200ms, Rust continues for 2-5 seconds (up to 5s drain deadline). **`order_hint: "last"`** — this test kills the containers and must run last. After this test, the stack is dead; the driver either tears down (if `--stop`) or reports the stack needs manual restart.

  **Test cases removed from v2 that are NOT in this spec:**
  - `healthz_pre_ready_returns_503` — dropped. Observing the 503 startup window is timing-dependent (sidecars only start after mocks are healthy, by which point the window may or may not be observable on the driver's first poll). The 503 behavior is already covered by unit tests in `sidecar/src/health.rs::test_healthz_returns_503_before_ready`. Not worth the nondeterminism.

- FR-23: **Scope reductions documented:**
  - The DNS-error SSRF fail-closed property is NOT verified by the harness. Unit tests in `sidecar/src/ssrf.rs` cover it. The v2 divergence case was scope-reduced to parity (`egress_dns_error_both_fail_502`), which asserts error-path parity but not the fail-closed property.
  - The parent rust-sidecar spec's FR-18 guarantees "resolve once, pass SocketAddr to dialer, never redial by hostname." This harness does NOT verify the resolve-once property end-to-end. True rebinding testing requires a DNS mock that returns different IPs on successive calls. Deferred as a followup; currently enforced by code review of `sidecar/src/ssrf_connector.rs`.

#### CI integration

- FR-24: The harness shall be wired into CI via TWO workflow files (not one, so GitHub Actions' workflow-level `paths:` filter can be applied cleanly):

  **`.github/workflows/ci.yml`** — runs on every PR and every push to main. No path filter. Jobs:
  1. **`rust-checks`**: rust stable, `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`
  2. **`rust-checks-with-test-utils`**: same but with `--features nautiloop-sidecar/__test_utils` (covers the integration tests from PR #73 that would otherwise be silently skipped — closes issue #71)
  3. **`prod-leak-lint`**: runs `sidecar/scripts/lint-no-test-utils-in-prod.sh`. Fails on any hit.

  **`.github/workflows/parity.yml`** — runs only on PRs and pushes that touch `sidecar/**`, `images/sidecar/**`, or `specs/sidecar-*.md`. Uses workflow-level `on.pull_request.paths` and `on.push.paths`. One job:
  1. **`parity-harness`**: installs Docker + rust stable, runs `cargo run -p nautiloop-sidecar-parity-harness --release -- --stop`. 10-minute timeout. Uploads `sidecar/tests/parity/harness-run.log` + `docker compose logs` as artifacts on failure.

- FR-25: The `parity-harness` job has a 10-minute timeout.

- FR-26: On failure, the parity job uploads artifacts for post-mortem.

#### Cargo-deny

- FR-27: The harness crate's dependencies inherit the workspace `deny.toml`. No new deny config.

#### Security lint extension

- FR-28: The existing `sidecar/scripts/lint-no-test-utils-in-prod.sh` from PR #73 shall be extended to ALSO check for `NAUTILOOP_EXTRA_CA_BUNDLE` references anywhere in the repo EXCEPT the two specific files allowed by SR-5. The allowed-file list is hardcoded in the script, not a directory glob — a broad directory exclusion would let future references leak in under other parity-tree files:
  ```bash
  MATCHES=$(git grep -l NAUTILOOP_EXTRA_CA_BUNDLE \
    -- ':!sidecar/tests/parity/docker-compose.yml' \
       ':!.github/workflows/parity.yml')
  if [ -n "$MATCHES" ]; then
    echo "ERROR: NAUTILOOP_EXTRA_CA_BUNDLE referenced outside the allowlist:"
    echo "$MATCHES"
    echo "The only files allowed to reference this env var are:"
    echo "  - sidecar/tests/parity/docker-compose.yml"
    echo "  - .github/workflows/parity.yml"
    exit 1
  fi
  ```
  Narrow file-level exclusion; robust to formatting changes because the allowlist is two exact file paths. Match to SR-5 exactly — any drift between SR-5 and this script's allowlist is a spec/implementation bug.

#### Network subnet override

- FR-29: The `parity-net` subnet is configurable via the `PARITY_NET_SUBNET` environment variable. Default: `100.64.0.0/24`. The docker-compose.yml uses `${PARITY_NET_SUBNET:-100.64.0.0/24}`. The harness driver has a `--subnet <cidr>` flag that sets the env var before calling `docker compose`. Override use cases: (a) developer is on an ISP that routes 100.64.0.0/10 for CGNAT and docker can't claim it, (b) another docker bridge on the host already claimed 100.64.0.0/24.

  **Subnet validation uses a WHITELIST of safe CIDR ranges, not a blacklist sample.** A blacklist approach is unsound: a straddling subnet like `9.255.0.0/15` (spanning 9.255.0.0–10.0.255.255) has public first and last addresses but includes `10.x.x.x` in the middle, and Docker can assign any address in the subnet to a container. Checking a single address, or even first-and-last, misses embedded private ranges (e.g., `168.0.0.0/7` includes link-local `169.254.0.0/16`).

  The correct approach: the harness driver accepts an override subnet ONLY IF it is entirely within one of these known-safe ranges that neither sidecar blocks:
  - `100.64.0.0/10` (RFC6598 CGNAT — the default)
  - `192.0.2.0/24` (RFC5737 TEST-NET-1)
  - `198.51.100.0/24` (RFC5737 TEST-NET-2)
  - `203.0.113.0/24` (RFC5737 TEST-NET-3)

  These are all non-globally-routable test/documentation ranges that neither Go nor Rust SSRF logic blocks. The driver validates the proposed subnet via `ipnet::Ipv4Net::contains` semantics: `is_safe(user_cidr) := any(safe_range.contains(user_cidr) for safe_range in WHITELIST)`. The `contains` check is strict subset, so `user_cidr` must be entirely inside one of the whitelisted ranges — no straddling allowed. If the check fails, the driver exits with a clear error listing the allowed ranges.

  **This validator is independent of the sidecar's SSRF blocklist** — no path dependency on `nautiloop-sidecar`, no import, no drift risk. The whitelist is simpler and sound. If a future Rust sidecar change tightens SSRF to also block one of these test ranges, the harness naturally fails at `compose up` time (the sidecar will block mock traffic) and the operator updates the whitelist. That's a loud failure mode, not a silent one.

  **Sidecar code change required for FR-29: NONE.** The earlier v4 proposal required exposing `is_private_ip` from `sidecar/src/lib.rs` via a `pub use` addition. That's no longer needed — the whitelist approach doesn't import anything from the sidecar crate. This keeps the spec's "no code changes to either sidecar" guarantee true.

### Non-Functional Requirements

- NFR-1: The harness shall be runnable on any developer workstation with Docker Desktop + Rust stable.
- NFR-2: Full corpus run under 5 minutes on a 2024-era laptop (cold compose build excluded; `--no-rebuild` path target).
- NFR-3: `cargo clippy --workspace -- -D warnings` green, including the harness crate.
- NFR-4: Hermetic — no external network access during test runs. Both sidecars resolve `api.openai.com`, `api.anthropic.com`, `github.com`, `mock-example`, `egress-target` via `extra_hosts` to `parity-net` IPs.
- NFR-5: Failures produce diffs with the exact fields that differ. Diff points at the corpus JSON filename.
- NFR-6: **Determinism** — running the same corpus twice against unchanged code produces identical pass/fail.
- NFR-7: **Isolation** — on any panic or early exit, a `Drop` guard runs `docker compose down -v --remove-orphans`.
- NFR-8: **Resource bounds** — per-container memory limit of 512MB.
- NFR-9: **Observability** — on failure, dump full mock logs AND `docker compose logs` to `sidecar/tests/parity/harness-run.log` in a deterministic format.

### Security Requirements

- SR-1: Test CA private key committed under `sidecar/tests/parity/fixtures/test-ca/ca.key` with loud header + README.
- SR-2: Harness client SSH key committed similarly with same headers.
- SR-3: Mock model credentials obviously non-production (`sk-test-openai-key`, `sk-ant-test-key`).
- SR-4: No production certs/keys/credentials reused.
- SR-5: `NAUTILOOP_EXTRA_CA_BUNDLE` set only in `sidecar/tests/parity/docker-compose.yml` and `.github/workflows/parity.yml`. FR-28 enforces via CI lint.
- SR-6: No `NAUTILOOP_SSRF_TEST_ALLOWLIST` bypass — CGNAT handles SSRF transparently.
- SR-7: Test CA generation pinned:
  ```bash
  openssl req -x509 -newkey rsa:2048 -keyout ca.key -out ca.pem \
    -sha256 -days 3650 -nodes \
    -subj "/CN=Nautiloop Parity Harness Test CA" \
    -addext "basicConstraints=critical,CA:TRUE"
  ```
  Mock service cert generation:
  ```bash
  openssl req -newkey rsa:2048 -keyout key.pem -out csr.pem -nodes \
    -sha256 \
    -subj "/CN=api.openai.com" \
    -addext "subjectAltName=DNS:api.openai.com"
  openssl x509 -req -in csr.pem -CA ca.pem -CAkey ca.key \
    -CAcreateserial -out cert.pem -days 3650 -sha256 \
    -copy_extensions copy
  ```
  All certs MUST include a `subjectAltName` DNS entry — otherwise rustls rejects with `NameMismatch`.
- SR-8: 10-year validity on test CA. `regenerate-test-ca.sh` committed for future rotation.
- SR-9: Harness SSH key trust boundary is mock-github-ssh only.

## Architecture

### Network topology

```
parity-net (custom bridge, subnet ${PARITY_NET_SUBNET:-100.64.0.0/24})
│
├── sidecar-go       100.64.0.20
├── sidecar-rust     100.64.0.21
├── mock-openai      100.64.0.10  (:443 HTTPS, :80 healthz, :9999 introspection → host :49990)
├── mock-anthropic   100.64.0.11  (:443 HTTPS, :80 healthz, :9999 introspection → host :49991)
├── mock-github-ssh  100.64.0.12  (:22 SSH, :2200 TCP healthz, :9999 introspection → host :49992)
├── mock-example     100.64.0.13  (:80 HTTP + :8080 HTTP, :9999 introspection → host :49993)
└── mock-tcp-echo    100.64.0.14  (:443 raw TCP, no /_healthz, no introspection)
```

`extra_hosts` on both sidecars:
- `api.openai.com → 100.64.0.10`
- `api.anthropic.com → 100.64.0.11`
- `github.com → 100.64.0.12`
- `mock-example → 100.64.0.13`
- `egress-target → 100.64.0.14` (dedicated hostname for egress CONNECT tests)

CGNAT verification: `sidecar/src/ssrf.rs:94-99` and `images/sidecar/main.go:43-48`.

Host port mappings (every port the harness driver or manual smoke needs; see FR-2 for the authoritative list):
- sidecar-go: 19090 (model), 19091 (ssh), 19092 (egress), 19093 (health)
- sidecar-rust: 29090-29093
- mock-openai: 50010 (:80 healthz HTTP), 50011 (:443 HTTPS smoke), 49990 (:9999 introspection)
- mock-anthropic: 50020, 50021, 49991
- mock-github-ssh: 50030 (:2200 health TCP), 49992 (:9999 introspection) — `:22` NOT published (sidecars reach via parity-net)
- mock-example: 50040 (:80), 50041 (:8080), 49993 (introspection)
- mock-tcp-echo: 50050 (:443 health TCP) — published ONLY for driver health gating; application traffic reaches it via sidecar tunnels from within parity-net

### Crate structure

```toml
# Cargo.toml (workspace root)
[workspace]
members = [
    "cli",
    "control-plane",
    "sidecar",
    "sidecar/tests/parity",
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
ipnet = "2"
```

The harness uses `reqwest` with a `rustls::ClientConfig` that loads the test CA from `fixtures/test-ca/ca.pem` (same pattern as the sidecar). Egress raw-TCP cases use `tokio::net::TcpStream` directly. `russh` client drives git_ssh cases. FR-29 subnet whitelist validation uses `ipnet::Ipv4Net::contains` against hardcoded test-range constants — no path dependency on the sidecar crate.

**No sidecar code changes.** The previous v4 proposal required adding `pub use ssrf::is_private_ip` to `sidecar/src/lib.rs`. That's no longer needed because FR-29 uses a whitelist, not a re-used blacklist classifier.

### Driver program structure

```rust
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let corpus = load_corpus("corpus/")?;
    let filtered = filter_corpus(&corpus, &args);

    if let Some(subnet) = &args.subnet {
        std::env::set_var("PARITY_NET_SUBNET", subnet);
        validate_subnet_not_ssrf_blocked(subnet)?;  // FR-29 guard
    }

    let compose = ComposeStack::new("docker-compose.yml");
    if !args.no_rebuild { compose.build().await?; }
    compose.up().await?;
    let _guard = ComposeGuard::new(&compose, args.stop); // NFR-7

    compose.wait_mock_healthchecks(Duration::from_secs(60)).await?;
    compose.wait_sidecar_readiness(Duration::from_secs(30)).await?;

    let mut results = Vec::new();

    // Run parity tests first (order-independent), then any order_hint=last.
    let (ordered_last, rest): (Vec<_>, Vec<_>) =
        filtered.iter().partition(|c| c.order_hint.as_deref() == Some("last"));
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

    if args.stop || summary.all_passed { drop(_guard); }
    if !summary.all_passed { std::process::exit(1); }
    Ok(())
}
```

Each `run_case` dispatches on `case.category` to a dedicated driver module. `CaseResult` includes both sidecars' captured outputs AND mock logs from introspection AND per-chunk wall-clock timestamps (for the SSE streaming divergence case).

## Migration plan

Six commits on branch `feat/sidecar-parity-harness`:

### Commit 1 — scaffolding + fixtures
- Workspace member, Cargo.toml, stub main.rs, fixtures/test-ca/ + regenerate script, per-mock Dockerfiles, README, go-secrets/rust-secrets populated
- `cargo build` + `cargo clippy` green

### Commit 2 — mock services
- All 5 mock service implementations (hypercorn + paramiko)
- Dockerfile.go-sidecar + Dockerfile.go-with-test-ca
- `docker compose build` succeeds
- Manual smoke: `curl -N --cacert fixtures/test-ca/ca.pem https://localhost:<published>/v1/chat/completions -d '{"stream":true}'` observes ~100ms inter-chunk delays on mock-openai (proves mock flushes)

### Commit 3 — docker-compose + driver scaffolding
- Full compose file with parity-net, extra_hosts, mounts, ports, env
- ComposeStack/ComposeGuard helpers, subnet validation
- `cargo run` cleanly brings up + tears down

### Commit 4 — corpus + driver cases
- corpus/ populated with all FR-22 cases
- model_proxy/egress/git_ssh/health driver modules
- Normalization + diffing
- Mock introspection client
- First full run: all 10 model_proxy parity + 6 egress parity + 5 ssh parity + 2 health parity + 1 dns-error parity = 24 parity cases GREEN; all 5 divergence cases GREEN in divergent direction (including the two SSE streaming cases that prove Rust fixes #66)

### Commit 5 — CI workflows + lint extension
- `.github/workflows/ci.yml` (3 jobs, no path filter)
- `.github/workflows/parity.yml` (1 job, path-filtered)
- `sidecar/scripts/lint-no-test-utils-in-prod.sh` extended per FR-28
- CI green on the branch

### Commit 6 — polish, README, followups
- Final README
- Followup issues for deferred work (true DNS mock, rebinding coverage)

## Test plan

### Harness self-checks

- `cargo build --workspace` green
- `cargo clippy --workspace --all-targets -- -D warnings` green
- `cargo clippy --workspace --all-targets --features nautiloop-sidecar/__test_utils -- -D warnings` green
- `cargo test --workspace` — no regressions
- `cargo test --workspace --features nautiloop-sidecar/__test_utils` — 7 integration tests pass
- `bash sidecar/scripts/lint-no-test-utils-in-prod.sh` — exits 0

### End-to-end runs

- Cold (`--stop`, full build) < 10 minutes
- Warm (`--stop --no-rebuild`) < 5 minutes per NFR-2
- All 24 parity cases green, all 5 divergence cases green in their divergent direction
- **Streaming divergence specifically**: Rust chunks arrive ~100ms apart (±50ms); Go delivers nothing or all at once at upstream-close. Both directions are observable within a 1s window.
- Intentional regression check: modify mock-openai to drop a header, re-run, diff points at the right case
- Intentional regression check: flip a divergence assertion, that case fails

### Divergence assertion validity

For each of the 5 divergence cases:
- SSE streaming (openai + anthropic): if Rust were buffered, case would fail; if Go were flush-fixed, case would fail. Both asymmetries are load-bearing.
- Bare exec (upload + receive): if Rust proxied through like Go, case would fail.
- CONNECT drain: if Rust closed immediately like Go, case would fail.

### Manual smoke

- `docker compose up`, then manually:
  - `curl http://localhost:19090/openai/v1/models -H 'Authorization: Bearer x'` → mock response with sidecar-injected Bearer
  - `ssh -p 19091 -i fixtures/go-secrets/ssh-key/id_ed25519 git@localhost git-upload-pack 'test/repo.git'` → mock pack bytes
  - Same against 29090/29091 → identical results
  - `curl -N http://localhost:29090/openai/v1/chat/completions -d '{"stream":true,...}'` observes incremental chunks (Rust works)
  - `curl -N http://localhost:19090/openai/v1/chat/completions -d '{"stream":true,...}'` observes no chunks or all-at-once (Go is broken, as expected)

### CI

- Push branch, confirm `ci.yml` jobs pass (always runs)
- Confirm `parity.yml` passes on the branch
- On a PR that doesn't touch sidecar/, confirm `parity.yml` is SKIPPED via the path filter

## Security considerations

### Non-negotiables

1. No production certs/keys/credentials in `sidecar/tests/parity/fixtures/`.
2. `NAUTILOOP_EXTRA_CA_BUNDLE` only in the two allowed locations; FR-28 lint enforces.
3. No `NAUTILOOP_SSRF_TEST_ALLOWLIST` or equivalent code bypass.
4. Test CA private key marked test-only in file header, README, and path.
5. Harness SSH key trust boundary is mock-github-ssh only.

### Supply chain

- Python pinning in FR-10 (hypercorn 0.17.3, uvicorn 0.30.6, starlette 0.38.6, paramiko 3.4.0)
- `python:3.12-slim` base image
- Docker base image freshness tracked implicitly

### New risks

1. **CGNAT collision.** If the host's VPN or ISP uses 100.64.0.0/10, docker bridge creation may fail. Mitigation: FR-29 `PARITY_NET_SUBNET` override + `--subnet` CLI flag + harness driver guard against overlap with SSRF blocklist.
2. **Docker network teardown on panic.** NFR-7 Drop guard; CI job has `docker compose down -v --remove-orphans` in an `always()` step.
3. **Rust streaming unverified pre-harness.** Trust declared in Dependencies section. First harness run is the verification. If Rust is broken, file P0 and fix before merging.

### What this spec does NOT change

- Zero code changes to `sidecar/src/` or `images/sidecar/main.go`.
- Production Dockerfile unchanged. Go sidecar Dockerfile is NEW (harness-only).
- K8s manifests unchanged.

## Out of scope

- True DNS rebinding testing (followup)
- True hermetic DNS SERVFAIL divergence testing (followup; covered by unit tests)
- Fault injection testing
- Performance benchmarking
- Real upstream smoke tests (phase 5 manual checklist)
- `cargo-deny` standalone config for the harness (inherits workspace)
- **Fixing the Go sidecar SSE bug** — explicitly not done; Rust is the fix. See Dependencies section on issue #66.

## Open questions

1. **Python version EOL.** `python:3.12-slim` is fine for years. Not blocking.
2. **paramiko server-side flush races.** Verified in Commit 2 manual smoke. Fallback: `asyncssh`. Not blocking.
3. **CI runtime budget.** 10-minute FR-25 timeout; tune after first runs.
4. **SSE divergence timing tolerance.** ±50ms is the initial proposal; may need widening if CI runners are noisy. Tune in commit 4.

None are blocking.

## Changelog

### v1 → v2: Codex adversarial review (14 findings: 3 P0, 11 P1)

All 14 addressed. See v2 changelog section (no longer duplicated here for brevity). Key v2 fixes that stick: CGNAT network, 7 concrete services, mock log introspection, raw TCP for egress, Dockerfile.go-sidecar resurrected, TLS params pinned.

### v2 → v3: Codex v2 review (12 findings: 3 P0, 5 P1, 4 P2) + plan change on issue #66

**Plan change (dominant):**
- The earlier plan was "wait for the parallel Go SSE flush fix (#66) to merge, then implement the parity harness." **That fix is abandoned.** The new plan: Rust sidecar IS the fix for #66. The harness treats SSE streaming as a documented divergence — Rust delivers chunks incrementally, Go does not — and the test corpus asserts this divergence in its documented direction. This adds a 5th entry to the divergence list and accelerates phase 5 cutover urgency (Go is now known-broken for opencode/SSE on v0.2.10; Rust is the only working path).

**P0 fixes from codex v2:**
- **P0-1 (egress_connect_github impossibility):** FIXED. Egress CONNECT tests now target `egress-target:443` (a dedicated hostname pointing at mock-tcp-echo via extra_hosts), not `github.com:443` (which would have resolved to mock-github-ssh on :22 with no :443 listener).
- **P0-2 (mock health gating inconsistent):** FIXED. FR-17 step 3 now enumerates per-mock healthcheck mechanisms: HTTP `/_healthz` for openai/anthropic/example, TCP connect for mock-github-ssh (:2200) and mock-tcp-echo (:443 its service port).
- **P0-3 (host-side introspection contradictory):** FIXED. FR-13 now specifies introspection ports are published to host ports 49990-49993 via docker-compose. Driver accesses via `http://localhost:4999X`. Single strategy, portable on Docker Desktop for all OSes.

**P1 fixes from codex v2:**
- **P1-4 (raw TCP egress without origin-form case):** FIXED. FR-22 adds `egress_http_origin_form_repair` test that exercises the raw-TCP path. Justifies the raw-TCP driver split.
- **P1-5 (503 startup window nondeterministic):** FIXED BY REMOVAL. Test case dropped from the parity suite. 503 behavior is covered by existing unit tests in `sidecar/src/health.rs`. No parity observation of a timing-dependent window.
- **P1-6 (mock reset semantics contradictory):** FIXED. `/__harness/healthcheck-only` endpoint removed. Mocks explicitly do NOT log `/_healthz` requests to the introspection store. `/__harness/reset` clears only the introspection log; there's nothing else to clear.
- **P1-14 (egress_http_get_example_with_port port mismatch):** FIXED. FR-11 now specifies mock-example listens on BOTH :80 and :8080 with the same handlers. The `:8080` test case is reachable.
- **NEW-8 (divergence count: DNS case was weakened to parity but still categorized as divergence):** FIXED. `divergence_ssrf_dns_error_failclosed` moved out of the divergence category into `egress` as `egress_dns_error_both_fail_502`. Divergence count goes from 4 to 5 (streaming adds two, DNS moves out).

**P2 fixes from codex v2:**
- **P2-9 (CI path filter underspecified):** FIXED. FR-24 splits into TWO workflow files: `.github/workflows/ci.yml` (always runs, 3 jobs) and `.github/workflows/parity.yml` (1 job, path-filtered at workflow level).
- **P2-10 (NAUTILOOP_EXTRA_CA_BUNDLE whitelist hand-wavy):** FIXED. FR-28 uses a hardcoded file-level `git grep` exclusion for `.github/workflows/parity.yml`. Robust to formatting changes because the allowlist is file-level, not job-level.
- **P2-11 (CGNAT collision mitigation not a real FR):** FIXED. New FR-29 defines the `PARITY_NET_SUBNET` env var + `--subnet` CLI flag + a validation guard that rejects SSRF-blocked subnets.
- **P2-12 (Python pinning incomplete):** FIXED. FR-10 now pins all four Python dependencies exactly: `hypercorn==0.17.3 uvicorn==0.30.6 starlette==0.38.6 paramiko==3.4.0`.

### v3 → v4: Codex v3 review (5 findings: 1 P0 V1, 2 P1, 2 P2)

Convergence pattern: 14 → 12 → 5.

- **P0-1 (mock host ports unspecified):** FIXED. FR-2 now enumerates EVERY host port the driver or manual smoke needs: `mock-openai :80→50010, :443→50011, :9999→49990`; `mock-anthropic :80→50020, :443→50021, :9999→49991`; `mock-github-ssh :2200→50030, :9999→49992` (no `:22` publishing — sidecars reach it via parity-net); `mock-example :80→50040, :8080→50041, :9999→49993`; `mock-tcp-echo :443→50050`. FR-17 step 3 was updated to cite concrete `localhost:500XX` addresses. FR-10 manual smoke example was updated to use `localhost:50011` and `localhost:50021`.

- **P1-2 (SSE divergence assertion weak / contradictory):** FIXED. The SSE divergence cases now assert **time-to-first-chunk** as the normative discriminator, not inter-chunk spacing. Rust expected: first chunk within 200ms of request send. Go expected: first chunk ≥250ms after request send (because Go buffers until upstream close at ~300ms). The 50ms gap is the decision boundary. Contradictory "100ms ±50ms" and ">200ms also passes" language removed. Inter-chunk spacing is documented as a secondary confirmation check, not the normative assertion.

- **P1-3 (divergence count/naming inconsistent):** FIXED.
  - Dependencies section's "Implications" bullet now lists the 5 actual divergence case names: `divergence_sse_streaming_openai`, `divergence_sse_streaming_anthropic`, `divergence_bare_exec_upload_pack_rejection`, `divergence_bare_exec_receive_pack_rejection`, `divergence_connect_drain_on_sigterm`. Explicitly notes that SSRF DNS fail-closed and DNS rebinding are NOT in the divergence list (moved out of scope per FR-23).
  - FR-22's SSE divergence entry is now split into TWO distinct cases with distinct names (`divergence_sse_streaming_openai` and `divergence_sse_streaming_anthropic`), matching FR-21's one-file-per-case schema. Both providers are tested independently because Rust's `UpstreamKind::OpenAi` and `UpstreamKind::Anthropic` routing branches could regress independently.

- **P2-4 (lint allowlist too broad):** FIXED. FR-28 now uses file-level hardcoded exclusion for exactly two files: `sidecar/tests/parity/docker-compose.yml` and `.github/workflows/parity.yml`. Matches SR-5 exactly.

- **P2-5 (FR-29 subnet validator drift risk):** FIXED. FR-29 now specifies that the harness reuses the sidecar's own SSRF classifier via `nautiloop_sidecar::ssrf::is_private_ip`, imported through a workspace path dependency. No prose duplication; any change to `sidecar/src/ssrf.rs` automatically propagates to the harness validator. Requires a one-line `pub use` addition to `sidecar/src/lib.rs` (noted in the Crate structure section).

### v4 → v5: Codex v4 review (5 findings: 1 P1, 4 P2)

Convergence pattern: 14 → 12 → 5 → 5 (severity dropping: v3 had 1 P0 V1; v4 has 0 P0, only 1 P1).

- **P1-1 (FR-29 sample-address validation unsound):** FIXED. Replaced the blacklist-sample approach with a **whitelist**: the harness accepts an override subnet only if it is entirely contained within one of four known-safe ranges (`100.64.0.0/10` CGNAT, plus the three RFC5737 TEST-NET ranges). Checked via `ipnet::Ipv4Net::contains` (strict subset; no straddling). Simpler AND sound — a straddling subnet like `9.255.0.0/15` is rejected unambiguously because it isn't contained in any whitelisted range.

- **P2-2 (path-dep compile-cost claim false):** FIXED BY REMOVAL. Because FR-29 no longer depends on the sidecar crate (the whitelist approach is self-contained), the path dependency is dropped entirely from the harness `Cargo.toml`. The false claim about "already built by rust-checks" is moot.

- **P2-3 (contradictory "no sidecar changes" vs "add pub use"):** FIXED. The `pub use ssrf::is_private_ip` addition to `sidecar/src/lib.rs` is no longer needed — FR-29's whitelist doesn't import anything from the sidecar crate. The spec's "no sidecar code changes" guarantee is now true.

- **P2-4 (Architecture section stale):** FIXED. The Architecture section's topology annotation and host-port-mapping list now match FR-2 exactly: mock-tcp-echo publishes :443 → 50050, and the complete 50010-50050 + 49990-49993 map is documented in one place.

- **P2-5 (stale case name `divergence_sse_streaming_incremental_chunks`):** FIXED. Replaced the single stale reference on line 46 with the two actual case names (`divergence_sse_streaming_openai` and `divergence_sse_streaming_anthropic`).
