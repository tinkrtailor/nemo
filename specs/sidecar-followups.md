# Sidecar: Round-2 Codex Followups

## Overview

Three P2 followups from the final Codex v3 review of PR #63 (Rust sidecar rewrite). All are refinements of fixes that already landed at commit `393a112`. None are catastrophic. All are tracked as GitHub issues:

- #68 — SSH priority channel for control messages
- #69 — Further restrict `test-utils` feature visibility
- #70 — E2E test coverage for SSH reject paths

This spec implements all three in a single batch. Skipping #71 (CI `cargo test --features` update) — the repo has no test workflow to update yet.

## Baseline

Main at merge of PR #63 (`8c4753d`). The Rust sidecar crate is at `sidecar/` in the workspace. The codex v3 GATE passed but flagged these three P2s. This spec closes them.

## Problem Statement

Three residual gaps from the codex v3 pass:

### Problem 1: `channel_close` can sit behind buffered pack data

From codex v3 finding 1:

> `channel_close` enqueues `AgentToUpstream::Close` through the same bounded FIFO used for pack data and awaits that send after the queue may already be full, so the callback itself blocks until capacity frees up. The proxy loop then processes `Close` only after earlier `Data` messages, which means an abrupt agent disconnect can still sit behind buffered writes instead of tearing the upstream session down promptly.

Files: `sidecar/src/git_ssh_proxy.rs:83`, `:557`, `:636`, `:861`

**Manifestation:** rare corner case (saturated queue AND abrupt close). Upstream session lingers for a few extra seconds on disconnect-during-push. Not data loss. Not security. Just a semantic correctness issue that the round-2 fix missed.

### Problem 2: `test-utils` feature is publicly reachable

From codex v3 finding 2:

> `test-utils` is declared as a normal public feature and `SshAuthPaths::with_test_override_addr` remains `pub`, while `git_ssh_proxy` and `serve_with_auth` are also public. Default release builds avoid it, but any downstream crate or `cargo build --all-features` build can re-enable the bypass path, so the original "public API" concern is reduced rather than eliminated.

Files: `sidecar/Cargo.toml:103`, `sidecar/src/git_ssh_proxy.rs:137`, `:270`, `sidecar/src/lib.rs:18`

**Manifestation:** default release builds genuinely don't contain the override (the round-2 fix verified this via release binary symbol-table inspection). But `cargo build --all-features` or a downstream crate that opts into `test-utils` can re-enable the bypass path. The feature gate is a weaker boundary than `#[cfg(test)]`.

### Problem 3: Reject paths only unit-tested, not e2e

From codex v3 finding 3:

> The updated e2e test only models the success case by having the mock upstream emit `ExitStatus(0)` and asserting that exact value on the client. The wire-level `exit_status_request(channel, 1)` paths for not-allowlisted commands, missing repo arguments, and repo mismatches are still only covered by pure parsing/path-matching unit tests, so a regression there would not be caught by the new e2e coverage.

Files: `sidecar/tests/git_ssh_proxy_e2e.rs:28`, `:139`, `:360`, `sidecar/src/git_ssh_proxy.rs:592`, `:613`

**Manifestation:** pure test coverage gap. The code is correct — earlier passes verified the reject logic. But a refactor that silently broke wire-level exit status propagation on a reject path would not fail the current e2e test.

## Dependencies

- **Requires:** PR #63 merged to main (commit `8c4753d`).
- **Enables:** tighter security posture around `test-utils`, correct SSH teardown on abrupt close, CI-level regression protection against future SSH exit-status bugs.
- **Blocks:** nothing.

## Requirements

### Problem 1 — Priority channel for control messages

- FR-1: The SSH proxy pump shall use a **separate unbounded `mpsc` channel dedicated to control messages** (`Close`, `Eof`), distinct from the bounded `mpsc::channel(256)` used for `Data` messages.
- FR-2: The pump's `tokio::select!` shall poll both channels fairly. When a `Close` message is available, the pump shall call `upstream_channel.close().await` and exit the loop immediately, regardless of outstanding `Data` messages in the data channel.
- FR-3: `Handler::channel_close` shall send `Close` to the control channel, not the data channel. The send is non-blocking (unbounded channel).
- FR-4: `Handler::channel_eof` shall send `Eof` to the control channel, not the data channel. Non-blocking.
- FR-5: `Handler::data` continues to use the bounded data channel (FR-1 leaves the existing backpressure behavior intact).
- FR-6: When the control channel's `Close` variant is received, the pump shall NOT drain outstanding `Data` messages from the data channel. The agent has disconnected; upstream writes after this point are pointless and the upstream session must tear down immediately.
- FR-7: When the control channel's `Eof` variant is received, the pump shall call `upstream_channel.eof().await` and continue processing data messages until the upstream responds with its own EOF.

### Problem 2 — Restrict `test-utils` feature visibility

- FR-8: The `test-utils` feature in `sidecar/Cargo.toml` shall be renamed to `__test_utils` (double-underscore prefix) to signal privacy, matching the convention used by other crates that have test-only features not intended for downstream consumption.
- FR-9: A doc comment on the feature in `Cargo.toml` shall explicitly state:
  ```toml
  # Internal test infrastructure only. Not part of the public API.
  # Downstream crates MUST NOT enable this feature. Setting it in a
  # production build re-enables the SSH SSRF bypass path used by
  # integration tests.
  ```
- FR-10: The `sidecar/tests/git_ssh_proxy_e2e.rs` `required-features = ["__test_utils"]` declaration shall be updated to match the new feature name.
- FR-11: The `SshAuthPaths::with_test_override_addr` constructor shall remain `pub` (required so test code can call it) but shall be gated behind `#[cfg(feature = "__test_utils")]`. The field `test_override_addr` shall remain behind the same gate.
- FR-12: The rename shall be documented in the commit message so the CI maintenance followup (issue #71) uses the new feature name when it lands.
- FR-13: A CI-level grep check (shell script committed to the repo) shall exist at `sidecar/scripts/lint-no-test-utils-in-prod.sh` that greps `.github/workflows/` for any `--features nautiloop-sidecar/__test_utils` on a release build step. The script exits non-zero if any match is found. This script is not wired into CI in this spec (CI work is deferred to #71), but it lives in-repo as a drop-in.

### Problem 3 — E2E reject path tests

- FR-14: The existing integration test file `sidecar/tests/git_ssh_proxy_e2e.rs` shall gain four new test functions, each asserting that the SSH proxy sends `exit_status(1)` on the wire for a specific reject path:
  - `test_e2e_rejects_non_git_exec_with_exit_status_1` — sends `exec` with command `ls /etc`, asserts `ChannelMsg::ExitStatus` received with code `1`
  - `test_e2e_rejects_bare_git_upload_pack_with_exit_status_1` — sends `exec` with command `git-upload-pack` (no path argument), asserts exit status 1 (validates the fix for the Go bare-exec bypass bug at the wire level, not just unit level)
  - `test_e2e_rejects_bare_git_receive_pack_with_exit_status_1` — same for `git-receive-pack`
  - `test_e2e_rejects_mismatched_repo_path_with_exit_status_1` — sends `exec` with command `git-upload-pack 'wrong/repo.git'`, asserts exit status 1
- FR-15: Each new test shall capture the exit status via `ChannelMsg::ExitStatus(code)` and assert `code == 1`. Timing out waiting for the exit status shall fail the test with a clear message identifying which reject path was being tested.
- FR-16: Each new test shall reuse the existing mock russh upstream server from `git_ssh_proxy_e2e.rs` rather than introducing a new mock.
- FR-17: The existing happy-path test and the new reject-path tests shall share a common helper that drives the SSH proxy end-to-end given an input command, eliminating copy-paste across the five tests.

### Non-Functional Requirements

- NFR-1: `cargo fmt --all -- --check` green.
- NFR-2: `cargo clippy --workspace --all-targets -- -D warnings` green (no features).
- NFR-3: `cargo clippy --workspace --all-targets --features nautiloop-sidecar/__test_utils -- -D warnings` green.
- NFR-4: `cargo test --workspace --features nautiloop-sidecar/__test_utils` passes all tests. Integration tests should show **5 passed** (1 existing happy path + 4 new reject path).
- NFR-5: Single commit per problem, three commits total. Commit messages:
  - `fix(sidecar): separate priority channel for SSH control messages (closes #68)`
  - `fix(sidecar): restrict test-utils feature with double-underscore prefix (closes #69)`
  - `test(sidecar): add e2e coverage for SSH reject path exit statuses (closes #70)`
- NFR-6: All three commits land on a single branch `fix/sidecar-followups` and a single PR closes all three issues.

### Security Requirements

- SR-1: The `__test_utils` feature shall not expand the attack surface of default release builds. Verified via `cargo build --release -p nautiloop-sidecar` + symbol inspection of the release binary: `test_override_addr` and `with_test_override_addr` must be physically absent.
- SR-2: The lint script (`sidecar/scripts/lint-no-test-utils-in-prod.sh`) shall also check for any reference to the old `test-utils` feature name in CI workflows, to catch future accidental re-introduction.

## Architecture

### Priority channel pattern (Problem 1)

The current `AgentToUpstream` enum and the single bounded `mpsc::channel(256)` become:

```rust
enum AgentData {
    Data(Vec<u8>),
}

enum AgentControl {
    Eof,
    Close,
}

struct PumpChannels {
    data_tx: mpsc::Sender<AgentData>,          // bounded 256
    data_rx: mpsc::Receiver<AgentData>,
    control_tx: mpsc::UnboundedSender<AgentControl>,  // unbounded
    control_rx: mpsc::UnboundedReceiver<AgentControl>,
}
```

The pump loop:

```rust
loop {
    tokio::select! {
        // Control messages get priority via select ordering is NOT enough
        // because select! is fair, not priority. We structure the loop so
        // that control is checked first on each iteration.
        control = control_rx.recv() => {
            match control {
                Some(AgentControl::Close) => {
                    upstream_channel.close().await.ok();
                    break; // exit immediately, discard any pending data
                }
                Some(AgentControl::Eof) => {
                    upstream_channel.eof().await.ok();
                    // continue loop; upstream may still send data
                }
                None => break, // control channel closed
            }
        }
        data = data_rx.recv(), if !control_closed => {
            match data {
                Some(AgentData::Data(bytes)) => {
                    upstream_channel.data(&bytes[..]).await.ok();
                }
                None => { /* data channel drained */ }
            }
        }
        upstream_msg = upstream_channel.wait() => {
            // forward upstream -> agent as today
        }
    }
}
```

**Note on "priority":** `tokio::select!` is fair by default; removing `biased` was the round-2 fix. To get actual control-message priority, we don't use `biased` (which brings back the starvation risk). Instead, we rely on the unbounded control channel — `Close` sends from `Handler::channel_close` cannot block, so the callback returns immediately even if the data channel is full. The pump's next `select!` iteration will see the control message within one scheduling slice and act on it.

If finer-grained priority is needed in the future, a dedicated loop structure or `FuturesUnordered` with explicit prioritization can replace this. Not required for this spec.

### Feature rename (Problem 2)

Straightforward rename across:
- `sidecar/Cargo.toml` `[features]` block
- `sidecar/Cargo.toml` `[[test]]` `required-features`
- Every `#[cfg(feature = "test-utils")]` attribute in `sidecar/src/` becomes `#[cfg(feature = "__test_utils")]`

The lint script:

```bash
#!/usr/bin/env bash
# sidecar/scripts/lint-no-test-utils-in-prod.sh
# Fails if any CI workflow references __test_utils or test-utils on a
# release build step.
set -euo pipefail

WORKFLOWS=".github/workflows"
if [ ! -d "$WORKFLOWS" ]; then
  echo "No $WORKFLOWS directory; nothing to check"
  exit 0
fi

# Both old and new feature names
BAD_PATTERN='--features[[:space:]]*nautiloop-sidecar/(__)?test[-_]utils'

if grep -rEn "$BAD_PATTERN" "$WORKFLOWS" 2>/dev/null; then
  echo "ERROR: CI workflows reference the internal test-utils feature."
  echo "This feature is test-only and must NOT be enabled in release builds."
  exit 1
fi

echo "OK: no test-utils feature references in CI workflows"
```

The script is executable (mode 0755). Not wired into any CI job in this spec — issue #71 (deferred) will do the wiring.

### E2E reject path tests (Problem 3)

The existing `git_ssh_proxy_e2e.rs` has one test (`test_e2e_git_proxy_pipes_bytes_bidirectionally_and_propagates_exit_status` or similar). It sets up:
- A mock russh upstream server that accepts `git-upload-pack` and replies with fixed bytes + `ExitStatus(0)`
- A test driver that connects to the sidecar's SSH proxy, sends an `exec` request, captures the response

The refactor extracts the setup + driver into a helper:

```rust
async fn drive_ssh_proxy_with_command(command: &str) -> ProxyResult {
    // spin up mock upstream
    // spin up sidecar proxy configured to talk to mock
    // connect a test SSH client, send exec(command)
    // capture bytes and exit status
    // return ProxyResult { bytes, exit_status, error }
}

struct ProxyResult {
    bytes_received: Vec<u8>,
    exit_status: Option<u32>,
    error: Option<String>,
}
```

Then each test becomes:

```rust
#[tokio::test]
async fn test_e2e_rejects_non_git_exec_with_exit_status_1() {
    let result = drive_ssh_proxy_with_command("ls /etc").await;
    assert_eq!(result.exit_status, Some(1),
        "expected exit status 1 for non-git exec, got {:?}", result.exit_status);
}

#[tokio::test]
async fn test_e2e_rejects_bare_git_upload_pack_with_exit_status_1() {
    let result = drive_ssh_proxy_with_command("git-upload-pack").await;
    assert_eq!(result.exit_status, Some(1),
        "expected exit status 1 for bare git-upload-pack (Go bypass bug fix), got {:?}",
        result.exit_status);
}

// ... same for git-receive-pack and mismatched repo
```

The happy-path test also moves to use the helper, keeping assertions intact but eliminating duplication.

## Migration Plan

Three ordered commits on a single branch:

### Commit 1 — Priority channel for control messages
- `sidecar/src/git_ssh_proxy.rs`: split `AgentToUpstream` into `AgentData` and `AgentControl`, add unbounded control channel, update pump loop, update `Handler::data` / `channel_eof` / `channel_close` to send to the right channel.
- Existing unit tests continue to pass without modification.
- A new unit or integration test asserts: filling the data channel to capacity and then calling `channel_close` results in `upstream_channel.close()` being called within 50ms, not blocking on the data drain.
- **Ship criterion:** `cargo test --workspace --features nautiloop-sidecar/__test_utils` green; new backpressure-under-close test present and passing.

### Commit 2 — Feature rename + lint script
- `sidecar/Cargo.toml`: rename `test-utils` → `__test_utils` feature, add doc comment.
- All `#[cfg(feature = "test-utils")]` attributes updated.
- `sidecar/tests/git_ssh_proxy_e2e.rs` `required-features` updated.
- New file: `sidecar/scripts/lint-no-test-utils-in-prod.sh`, mode 0755.
- **Ship criterion:**
  - `cargo test --workspace --features nautiloop-sidecar/__test_utils` green
  - `cargo build --release -p nautiloop-sidecar` green
  - `nm -a target/release/nautiloop-sidecar | grep test_override_addr` returns nothing (symbol absent)
  - `bash sidecar/scripts/lint-no-test-utils-in-prod.sh` exits 0 against current CI state

### Commit 3 — E2E reject path tests
- Extract `drive_ssh_proxy_with_command` helper in `sidecar/tests/git_ssh_proxy_e2e.rs`.
- Add the 4 new reject-path test functions.
- Refactor the existing happy-path test to use the same helper.
- **Ship criterion:** `cargo test --workspace --features nautiloop-sidecar/__test_utils -- git_ssh_proxy_e2e` shows **5 tests passing** (1 happy + 4 reject).

All three commits pushed as branch `fix/sidecar-followups` and opened as a single PR titled "fix(sidecar): address codex v3 followups (closes #68, #69, #70)".

## Test Plan

### Unit tests (new)

**`sidecar/src/git_ssh_proxy.rs`:**
- `test_channel_close_propagates_while_data_channel_saturated` — fills data channel to capacity with `Data` messages, then invokes `channel_close`, asserts the upstream channel's `close()` is called within 50ms regardless of backlog.

### Integration tests (new, in `sidecar/tests/git_ssh_proxy_e2e.rs`)

- `test_e2e_rejects_non_git_exec_with_exit_status_1`
- `test_e2e_rejects_bare_git_upload_pack_with_exit_status_1`
- `test_e2e_rejects_bare_git_receive_pack_with_exit_status_1`
- `test_e2e_rejects_mismatched_repo_path_with_exit_status_1`

Total integration test count after this spec: **5** (1 prior + 4 new).

### Full workspace gate

```
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo clippy --workspace --all-targets --features nautiloop-sidecar/__test_utils -- -D warnings
cargo test --workspace --features nautiloop-sidecar/__test_utils
cargo build --release -p nautiloop-sidecar
bash sidecar/scripts/lint-no-test-utils-in-prod.sh
```

All six commands must succeed.

## Out of Scope

- **Wiring the lint script into CI.** Tracked as followup to #71.
- **Priority queue beyond Data/Control split.** If in the future we need Eof to be higher priority than Data but lower than Close, a third channel or explicit priority queue is needed. Not required today.
- **Refactoring `serve_with_auth` or module visibility.** Codex v3 noted `pub mod git_ssh_proxy` is still public because the binary depends on it. The feature gate is the security boundary; changing module visibility would require restructuring the binary/library boundary and is a separate concern.
- **Any unrelated sidecar changes.** This spec is strictly scoped to closing issues #68, #69, #70.

## Open Questions

None. The fixes are mechanical and the designs are derived directly from codex's finding citations.
