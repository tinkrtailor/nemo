# Nautiloop Auth Sidecar: Parity Harness

Containerized test harness that runs the Go auth sidecar and the Rust
auth sidecar side-by-side under docker compose, issues deterministic
test inputs to both, and diffs their outputs.

Spec: `specs/sidecar-parity-harness.md` (phase 4 of the Rust sidecar
migration plan).

## What this harness proves

- **Behavioral parity** on 25 cases covering model proxy, egress
  logger, git SSH proxy, and `/healthz`.
- **5 documented divergences** where the Rust sidecar's behavior
  differs intentionally (SSE streaming fixes #66, bare-exec is
  rejected locally, CONNECT tunnels drain on SIGTERM).
- **SSRF guardrails** remain intact: the harness uses a CGNAT
  (RFC6598 `100.64.0.0/10`) docker bridge, which neither sidecar's
  SSRF blocklist rejects — no test-only bypass features needed.

## Running

From the repo root:

```bash
# Full run with rebuild + auto-teardown:
cargo run -p nautiloop-sidecar-parity-harness --release -- --stop

# Fast iteration: skip the docker build (assumes images are up to date):
cargo run -p nautiloop-sidecar-parity-harness --release -- --no-rebuild --stop

# Run only one case for debugging:
cargo run -p nautiloop-sidecar-parity-harness -- --case openai_get_v1_models

# Run a whole category:
cargo run -p nautiloop-sidecar-parity-harness -- --category divergence
```

## Test CA and committed credentials

**All TLS certs, SSH keys, and mock credentials under
`fixtures/` are TEST-ONLY.** Loud warnings are in each fixture
subdirectory's `README.md` (or inline file headers). They are
deliberately committed so the harness is hermetic: no network, no
out-of-band setup, no secret management.

- `fixtures/test-ca/ca.pem` / `ca.key`: 10-year self-signed CA used
  to sign the mock service TLS certs. SR-1.
- `fixtures/mock-openai/cert.pem`: SAN=`api.openai.com`, signed by
  the test CA. SR-4.
- `fixtures/mock-anthropic/cert.pem`: SAN=`api.anthropic.com`.
- `fixtures/go-secrets/model-credentials/openai`: literal string
  `sk-test-openai-key`. SR-3.
- `fixtures/go-secrets/model-credentials/anthropic`: literal
  `sk-ant-test-key`.
- `fixtures/go-secrets/ssh-key/id_ed25519`: harness client private
  key used to talk to `mock-github-ssh`. SR-2.
- `fixtures/rust-secrets/...`: identical content mounted separately
  so each sidecar gets its own write-observable secret path.

## CGNAT rationale (FR-29)

Docker's default bridge allocates from `172.16.0.0/12`. Both sidecars'
SSRF blocklists reject that range, so every test case would fail
closed before reaching any mock. The harness solves this by declaring
a custom bridge in RFC6598 CGNAT space (`100.64.0.0/24` by default),
which neither sidecar blocks:

- Rust: see `sidecar/src/ssrf.rs:94-99` ("we DO NOT block 100.64.0.0/10").
- Go: see `images/sidecar/main.go:43-48` (privateRanges only lists
  RFC1918 + link-local + loopback).

If the host already uses `100.64.0.0/24` on another bridge (or the
operator's ISP routes CGNAT), pass `--subnet <cidr>` (or set
`PARITY_NET_SUBNET`). The driver validates the override against a
whitelist of four safe ranges:

- `100.64.0.0/10` (RFC6598)
- `192.0.2.0/24` (RFC5737 TEST-NET-1)
- `198.51.100.0/24` (RFC5737 TEST-NET-2)
- `203.0.113.0/24` (RFC5737 TEST-NET-3)

Anything outside these ranges is refused.

## Manual smoke checks (FR-10)

Before running the full harness for the first time, verify that
hypercorn flushes SSE chunks against the openai and anthropic mocks.
Both commands should show three `data:` lines appearing at ~100ms
intervals:

```bash
docker compose -f sidecar/tests/parity/docker-compose.yml up -d
curl -N --cacert sidecar/tests/parity/fixtures/test-ca/ca.pem \
  https://localhost:50011/v1/chat/completions \
  -H 'Content-Type: application/json' \
  -d '{"stream":true,"model":"gpt-4","messages":[{"role":"user","content":"ping"}]}'
curl -N --cacert sidecar/tests/parity/fixtures/test-ca/ca.pem \
  https://localhost:50021/v1/messages \
  -H 'Content-Type: application/json' \
  -d '{"stream":true,"model":"claude","messages":[{"role":"user","content":"ping"}]}'
```

If either curl buffers the entire stream until the connection closes,
fix the mock's flush call before proceeding — the divergence case
assertions rely on observable inter-chunk delays.

## Published ports reference

See `src/compose.rs::ports` for the authoritative list. Quick lookup:

| Service              | Published host ports                                     |
| -------------------- | -------------------------------------------------------- |
| `sidecar-go`         | 19090 (model), 19091 (ssh), 19092 (egress), 19093 (health) |
| `sidecar-rust`       | 29090-29093                                              |
| `mock-openai`        | 50010 (healthz), 50011 (https smoke), 49990 (introspect) |
| `mock-anthropic`     | 50020, 50021, 49991                                      |
| `mock-github-ssh`    | 50030 (tcp health), 49992 (introspect)                   |
| `mock-example`       | 50040, 50041, 49993                                      |
| `mock-tcp-echo`      | 50050 (tcp health)                                       |

## Failure diagnosis

- `sidecar/tests/parity/harness-run.log` — dumped on every run,
  contains the summary, per-case results, and `docker compose logs`.
- `docker compose -f sidecar/tests/parity/docker-compose.yml logs <service>` —
  live service logs.
- `curl http://localhost:49990/__harness/logs | jq .` — raw
  introspection log from mock-openai.

## Where this fits

- CI job `parity-harness` in `.github/workflows/parity.yml` runs the
  full harness on every PR touching `sidecar/**`, `images/sidecar/**`,
  or `specs/sidecar-*.md`.
- The per-PR CI (`ci.yml`) runs `cargo test` + `cargo clippy` +
  `cargo test --features __test_utils` + the no-leak lint script.
- When phase 5 retires the Go sidecar, this harness is the safety net
  that lets us delete `images/sidecar/main.go` confident that the Rust
  implementation covers every observed behavior.
