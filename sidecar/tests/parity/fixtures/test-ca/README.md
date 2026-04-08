# Parity Harness Test CA

**TEST-ONLY. NEVER USE IN PRODUCTION. NEVER COPY OUTSIDE THIS
DIRECTORY. NEVER TRUST THIS CA IN ANY NON-HARNESS CONTEXT.**

This directory holds a deliberately-committed 10-year self-signed
Certificate Authority used only by the containerized sidecar parity
harness under `sidecar/tests/parity/`.

## Files

- `ca.pem`: public CA certificate (committed).
- `ca.key`: **private key**, also committed. Only kept in this
  directory because the harness's TLS setup is fully hermetic —
  no external network access, no production trust stores touched.
  See SR-1 of `specs/sidecar-parity-harness.md`.
- `regenerate-test-ca.sh`: one-shot script to rotate this CA. SR-8.

## Why this is safe to commit

The harness runs entirely inside docker containers on a CGNAT bridge
network (`100.64.0.0/24` by default). The certs signed by this CA
are only valid for the mock service hostnames (`api.openai.com`,
`api.anthropic.com`) which resolve via docker `extra_hosts` to
internal bridge IPs. No client outside the harness ever reaches a
service presenting one of these certs.

The Rust sidecar loads this CA into its trust store ONLY when
`NAUTILOOP_EXTRA_CA_BUNDLE=/test-ca/ca.pem` is set — which happens
exclusively in `sidecar/tests/parity/docker-compose.yml` and the
`.github/workflows/parity.yml` CI job. A repo-wide lint
(`sidecar/scripts/lint-no-test-utils-in-prod.sh`) enforces this
allowlist.

## What this CA signs

- `fixtures/mock-openai/cert.pem`: SAN = `api.openai.com`
- `fixtures/mock-anthropic/cert.pem`: SAN = `api.anthropic.com`

## Regeneration

To rotate the CA (e.g. before the 10-year expiry), run the script
from this directory and then re-sign the mock service certs:

```bash
cd sidecar/tests/parity/fixtures/test-ca
./regenerate-test-ca.sh
```

The script re-runs the exact openssl invocations from SR-7 of the
spec. After regeneration you MUST re-run the mock cert signing
commands (see `regenerate-test-ca.sh` comments).
