# go-secrets/ (TEST-ONLY)

**TEST-ONLY. NEVER COPY OUTSIDE THIS DIRECTORY. NEVER USE IN PRODUCTION.**

Committed fixture data mounted at `/secrets/` on the Go parity sidecar
container. The identical layout is mirrored under `../rust-secrets/`
and mounted at `/secrets/` on the Rust parity sidecar container.

Two separate mounts (rather than one shared) so each sidecar has its
own write-observable secret path. The
`openai_credential_refresh_per_request` case mutates files on both
sides; isolation prevents cross-talk between the two writes.

## Layout (SR-2, SR-3)

- `model-credentials/openai` — literal `sk-test-openai-key\n`
- `model-credentials/anthropic` — literal `sk-ant-test-key\n`
- `ssh-key/id_ed25519` — harness client SSH private key (TEST-ONLY)
- `ssh-key/id_ed25519.pub` — corresponding public key
- `ssh-known-hosts/known_hosts` — trusts `mock-github-ssh` under all
  hostnames the sidecars reach it by (`github.com`, `100.64.0.12`)

## Why these are safe to commit

The keys and credentials here ONLY unlock the containerized
paramiko-based mock at `mock-github-ssh`, which runs on the same
docker bridge network as the sidecars and has no external reachability.
The mock model credentials are obvious non-production values.

No fixture in this directory is valid for any real service on the
public internet.
