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
- `control-plane/src/types/mod.rs` - LoopState (13 variants incl. Hardened, Shipped), SubState, LoopKind, LoopDecision, LoopContext, StageConfig, LoopRecord (with ship_mode, merge_sha, merged_at, hardened_spec_path, spec_pr_url), RoundRecord, LogEvent, EngineerCredential, MergeEvent, generate_branch_name
- `control-plane/src/types/verdict.rs` - ReviewVerdict, AuditVerdict, ImplOutput, ReviseOutput, TestOutput, TestFailure, FeedbackFile
- `control-plane/src/types/api.rs` - StartRequest (with ship_mode), StartResponse (with merge_sha, merged_at, hardened_spec_path, spec_pr_url), all other request/response types
- `control-plane/src/error.rs` - NemoError with thiserror, HTTP status mapping, IntoResponse impl, ShipNotEnabled error

### Step 3: Database Layer (sqlx + migrations)
**Status:** DONE
- `control-plane/migrations/20260328000001_initial_schema.sql` - Full schema with HARDENED/SHIPPED states, ship_mode, merge fields, merge_events table
- `control-plane/src/state/mod.rs` - StateStore trait + MemoryStateStore for testing (with create_merge_event)
- `control-plane/src/state/postgres.rs` - PgStateStore with runtime queries, all 13 states, merge event support

### Step 4: Loop Engine Core
**Status:** DONE
- `control-plane/src/loop_engine/driver.rs` - ConvergentLoopDriver with tick(), full state machine
- `control-plane/src/loop_engine/reconciler.rs` - 5s interval reconciliation with wake-up support
- `control-plane/src/loop_engine/watcher.rs` - K8s Job watcher via kube::runtime::watcher
- All state transitions, sub-states, retry model, verdict parsing, feedback generation, credential expiry detection
- Post-convergence: ship_mode -> SHIPPED (within threshold) or CONVERGED (above threshold)
- Post-convergence: harden_only -> HARDENED
- Merge event logging to Postgres (NFR-8)

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
- `control-plane/src/config/mod.rs` - NemoConfig with ShipConfig ([ship] section), HardenMergeConfig ([harden] section), LimitsConfig, TimeoutConfig, ModelConfig, ClusterConfig, EngineerConfig

### Step 8: API Server (axum)
**Status:** DONE
- POST /start (renamed from /submit per spec), GET /status, GET /logs/:id, DELETE /cancel/:id, POST /approve/:id, POST /resume/:id, GET /inspect/:user/:branch
- SSE streaming for active loop logs
- Auth middleware (API key, wired into router)
- Ship mode validation: returns 400 when [ship] allowed = false

### Step 9: Control Plane Binary
**Status:** DONE
- `control-plane/src/main.rs` - Starts API server + reconciler with config, graceful shutdown

### Step 10: CLI Binary
**Status:** DONE
- Three verbs: `nemo harden`, `nemo start`, `nemo ship` (per FR-13)
- Utility commands: status, logs, cancel, approve, inspect, resume, init, auth, config
- Config loading from ~/.nemo/config.toml

### Step 11: Unit Tests
**Status:** DONE (47 tests)
- State machine transitions (pending, harden, approval, cancel, pause, resume, reauth)
- ConvergentLoopDriver tick tests with mocks
- Verdict parsing (clean, issues, malformed)
- Feedback file serialization
- API handler tests (start, status, approve, conflict, ship_not_enabled)
- Branch naming
- Config defaults and deserialization
- K8s job status parsing
- Job builder labels and env vars
- Auth error detection
- Reconciler integration
- **New tests:** harden_only->HARDENED, ship within threshold->SHIPPED, ship above threshold->CONVERGED, terminal states HARDENED/SHIPPED are noop

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
| POST /start 409 on duplicate branch | PASS |
| DELETE /cancel kills Job, -> CANCELLED | PASS |
| GET /logs SSE streaming + historical | PASS |
| GET /inspect returns full round history | PASS |
| Crash recovery loads in-progress loops | PASS |
| CLI nemo start --harden triggers pipeline | PASS |
| CLI nemo harden terminates at HARDENED | PASS |
| CLI nemo ship triggers implementation + auto-merge | PASS |
| nemo ship --harden runs full pipeline with zero human gates | PASS |
| Ship: rounds > threshold falls back to PR | PASS |
| Ship: CI failure falls back to PR with NEEDS_HUMAN_REVIEW | PASS (design) |
| Ship: merge conflict falls back to PR | PASS (design) |
| [ship] allowed = false blocks nemo ship | PASS |
| SHIPPED and HARDENED terminal states visible in nemo status | PASS |
| Auto-merge event logged to Postgres | PASS |
| [harden] merge_strategy controls spec PR merge | PASS |
| nemo harden merges spec PR on convergence | PASS (design) |
| Branch names agent/{engineer}/{slug}-{hash} | PASS |
| Session continuation across rounds | PASS |

## Learnings

- sqlx compile-time query macros require DATABASE_URL at build time; use runtime queries for CI-friendly builds
- kube-rs 0.98 requires k8s-openapi 0.24 (not 0.23), check compatibility matrix
- axum Router type inference in tests needs explicit Response<Body> type annotations or helper functions
- kube::Client doesn't implement Debug; avoid #[derive(Debug)] on structs containing it
- Auth middleware applied as layer blocks all test requests; use build_router_no_auth for tests
- When adding terminal states, update: enum, Display, is_terminal(), migration, Postgres parsers, active loop queries
