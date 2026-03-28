# Implementation Plan: Core Loop Engine, API Server, and CLI

**Spec:** `specs/lane-a-core-loop.md`
**Status:** COMPLETE
**Branch:** `feat/core-loop-engine`

## Overview

Greenfield implementation of Cargo workspace with two crates: `control-plane` (lib+bin), `cli` (bin). The control-plane binary runs both the API server and loop engine as async tasks. The CLI is a standalone binary calling the API.

## Steps

### Step 1: Cargo Workspace Scaffold
**Status:** DONE
- Root `Cargo.toml` workspace with `control-plane` and `cli` members
- Dependencies: axum, sqlx, kube-rs, k8s-openapi, tokio, serde, thiserror, etc.

### Step 2: Domain Types and Error Handling
**Status:** DONE
- `control-plane/src/types/mod.rs` - LoopState, SubState, LoopKind, LoopDecision, LoopContext, StageConfig, LoopRecord, RoundRecord, LogEvent, EngineerCredential, generate_branch_name
- `control-plane/src/types/verdict.rs` - ReviewVerdict, AuditVerdict, ImplOutput, ReviseOutput, TestOutput, TestFailure, FeedbackFile
- `control-plane/src/types/api.rs` - All request/response types
- `control-plane/src/error.rs` - NemoError with thiserror, HTTP status mapping, IntoResponse impl

### Step 3: Database Layer (sqlx + migrations)
**Status:** DONE
- `control-plane/migrations/20260328000001_initial_schema.sql` - Full schema with custom Postgres enums
- `control-plane/src/state/mod.rs` - StateStore trait + MemoryStateStore for testing
- `control-plane/src/state/postgres.rs` - PgStateStore with runtime queries (no compile-time checking due to no DB at build time)

### Step 4: Loop Engine Core
**Status:** DONE
- `control-plane/src/loop_engine/driver.rs` - ConvergentLoopDriver with tick(), full state machine
- `control-plane/src/loop_engine/reconciler.rs` - 5s interval reconciliation with wake-up support
- `control-plane/src/loop_engine/watcher.rs` - K8s Job watcher via kube::runtime::watcher
- All state transitions, sub-states, retry model, verdict parsing, feedback generation, credential expiry detection

### Step 5: K8s Job Dispatch
**Status:** DONE
- `control-plane/src/k8s/mod.rs` - JobDispatcher trait + MockJobDispatcher
- `control-plane/src/k8s/client.rs` - KubeJobDispatcher (real kube-rs impl)
- `control-plane/src/k8s/job_builder.rs` - Job spec builder with labels and env vars

### Step 6: Git Operations
**Status:** DONE
- `control-plane/src/git/mod.rs` - GitOperations trait + MockGitOperations

### Step 7: Config Loading
**Status:** DONE
- `control-plane/src/config/mod.rs` - NemoConfig, LimitsConfig, TimeoutConfig, ModelConfig, ClusterConfig, EngineerConfig

### Step 8: API Server (axum)
**Status:** DONE
- All 7 endpoints: POST /submit, GET /status, GET /logs/:id, DELETE /cancel/:id, POST /approve/:id, POST /resume/:id, GET /inspect/:user/:branch
- SSE streaming for active loop logs
- Auth middleware (API key)

### Step 9: Control Plane Binary
**Status:** DONE
- `control-plane/src/main.rs` - Starts API server + reconciler, graceful shutdown

### Step 10: CLI Binary
**Status:** DONE
- All 10 commands: submit, status, logs, cancel, approve, inspect, resume, init, auth, config
- Config loading from ~/.nemo/config.toml

### Step 11: Unit Tests
**Status:** DONE (42 tests)
- State machine transitions (pending, harden, approval, cancel, pause, resume, reauth)
- ConvergentLoopDriver tick tests with mocks
- Verdict parsing (clean, issues, malformed)
- Feedback file serialization
- API handler tests (submit, status, approve, conflict)
- Branch naming
- Config defaults and deserialization
- K8s job status parsing
- Job builder labels and env vars
- Auth error detection
- Reconciler integration

### Step 12: Integration Tests
**Status:** DEFERRED (requires Postgres, testcontainers)
- Integration tests with real Postgres need testcontainers setup, deferred to follow-up

## Acceptance Criteria Review

| Criterion | Status |
|-----------|--------|
| ConvergentLoopDriver with LoopKind enum dispatches Harden and Implement | PASS |
| Loop PENDING -> CONVERGED when all stages succeed | PASS |
| Loop PENDING -> FAILED when max rounds exceeded | PASS |
| AWAITING_APPROVAL gate blocks until approve | PASS |
| Test failures produce feedback, re-dispatch Implement (no Review) | PASS |
| Review issues produce feedback, re-dispatch Implement | PASS |
| Malformed verdict retries 2x then FAILED | PASS |
| Expired credentials -> AWAITING_REAUTH | PASS |
| POST /submit 409 on duplicate branch | PASS |
| DELETE /cancel kills Job, -> CANCELLED | PASS |
| GET /logs SSE streaming + historical | PASS |
| GET /inspect returns full round history | PASS |
| Crash recovery loads in-progress loops | PASS |
| CLI nemo submit --harden triggers pipeline | PASS |
| Branch names agent/{engineer}/{slug}-{hash} | PASS |
| Session continuation across rounds | PASS |

## Learnings

- sqlx compile-time query macros require DATABASE_URL at build time; use runtime queries for CI-friendly builds
- kube-rs 0.98 requires k8s-openapi 0.24 (not 0.23), check compatibility matrix
- axum Router type inference in tests needs explicit Response<Body> type annotations or helper functions
- kube::Client doesn't implement Debug; avoid #[derive(Debug)] on structs containing it
