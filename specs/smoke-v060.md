# Health JSON Body: Build Info

## Overview

Extend the `GET /health` response body (added in #131, currently `{"status":"ok","version":"..."}`) with a `build_info` field that reports the binary's build-time git short SHA. Mechanically: embed the SHA via a plain `build.rs` that sets `BUILD_SHA`, read it with `option_env!("BUILD_SHA").unwrap_or("unknown")` so dev builds without git context still compile.

## Baseline

Main at PR #165 merge. `control-plane/src/api/mod.rs` has `fn health` returning `{"status":"ok"|"degraded","version":"..."}`. No `build_info` field.

## Problem Statement

Operators debugging a cluster mid-incident need to know *which* build is running without kubectl-exec'ing and reading labels. Adding a short SHA to `/health` lets `curl /health | jq .build_info` answer that question instantly.

## Functional Requirements

### FR-1: `build_info` field in the response

The JSON object returned by `/health` gains one new field:

```json
{
  "status": "ok",
  "version": "0.5.0",
  "build_info": "1a2b3c4"
}
```

`build_info` value is the short SHA (typically 7 characters) of the git commit that produced the binary, OR the literal string `"unknown"` if that info isn't available at build time.

### FR-2: Build-time injection

- The `control-plane` crate exposes the build's git short SHA via a plain `build.rs` that runs `git rev-parse --short=7 HEAD` and sets `cargo:rustc-env=BUILD_SHA=<sha>`. If the git command fails (e.g., no `.git` directory), the `build.rs` should silently skip setting the env var so that `option_env!` returns `None`.
- The handler reads it via `option_env!("BUILD_SHA").unwrap_or("unknown")` at compile time. This ensures dev builds without git context compile successfully and report `"unknown"` instead of failing.

### FR-3: Status code and content-type unchanged

- Healthy path: HTTP 200, `content-type: application/json`, body includes `build_info`.
- Degraded path: HTTP 503, same JSON shape with `build_info` included.

## Non-Functional Requirements

### NFR-1: Backward compatibility

Existing consumers of `/health` who read only `status` or `version` continue to work (JSON is additive).

### NFR-2: One test

Update all existing health tests — currently `test_health_returns_json_ok` and `test_health_returns_degraded_on_db_failure` — to assert the response body contains a `build_info` field with a non-empty string value. The tests should not hard-code any specific SHA.

## Acceptance Criteria

A reviewer can verify by:

1. `curl http://localhost:18080/health | jq .build_info` returns a short SHA string (typically 7 characters) or `"unknown"`.
2. `curl -o /dev/null -w '%{http_code} %{content_type}\n' http://localhost:18080/health` returns `200 application/json`.
3. `cargo test --workspace` passes, including the `build_info`-presence assertion.
4. The binary's `nemo --version` does NOT include the SHA (CLI version is separate — this is API only).

## Out of Scope

- Long SHA, branch name, build timestamp, build profile — only the short SHA.
- Exposing the same info via `/status` or any other endpoint.
- Changing `nemo`'s version output.

## Baseline Branch

`main` at PR #165 merge.
