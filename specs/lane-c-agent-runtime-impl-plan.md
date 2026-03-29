# Implementation Plan: Agent Runtime Layer (Lane C)

**Spec:** `specs/lane-c-agent-runtime.md`
**Branch:** `feat/lane-c-agent-runtime`
**Status:** In Progress
**Created:** 2026-03-29

## Codebase Analysis

### Existing Implementations Found

| Component | Location | Status |
|-----------|----------|--------|
| K8s job builder | `control-plane/src/k8s/job_builder.rs` | Basic - needs major enhancement |
| K8s dispatcher | `control-plane/src/k8s/client.rs` | Complete |
| K8s types/trait | `control-plane/src/k8s/mod.rs` | Complete |
| Types (verdict, feedback) | `control-plane/src/types/verdict.rs` | Complete |
| Config (NemoConfig) | `control-plane/src/config/mod.rs` | Needs `services` section |
| Git operations | `control-plane/src/git/mod.rs` | Complete |
| Error types | `control-plane/src/error.rs` | Complete |

### Patterns to Follow

| Pattern | Location | Description |
|---------|----------|-------------|
| Error handling | `control-plane/src/error.rs` | thiserror enum, NemoError |
| Serialization | `control-plane/src/types/` | serde derive on all structs |
| K8s types | `control-plane/src/k8s/` | kube-rs, k8s-openapi |
| Testing | `control-plane/src/k8s/client.rs` | Mock trait impls |

### Files to Create

| File | Purpose |
|------|---------|
| `images/base/Dockerfile` | Base agent image (FR-1 through FR-3) |
| `images/base/nemo-agent-entry` | Agent entrypoint script (FR-4 through FR-13) |
| `images/sidecar/main.go` | Auth sidecar binary (FR-14 through FR-23) |
| `images/sidecar/go.mod` | Go module for sidecar |
| `images/sidecar/Dockerfile` | Sidecar image build |
| `.nemo/prompts/implement.md` | Implement prompt template (FR-35) |
| `.nemo/prompts/review.md` | Review prompt template (FR-36) |
| `.nemo/prompts/spec-audit.md` | Spec audit prompt template (FR-37) |
| `.nemo/prompts/spec-revise.md` | Spec revise prompt template (FR-38) |
| `.nemo/prompts/test.md` | Test stage (informational, no template needed) |
| `terraform/main.tf` | Terraform main config (FR-43 through FR-56) |
| `terraform/variables.tf` | Terraform input variables |
| `terraform/outputs.tf` | Terraform outputs |
| `terraform/k8s.tf` | K8s resources (namespaces, RBAC, etc.) |
| `terraform/postgres.tf` | Postgres deployment |

### Files to Modify

| File | Change |
|------|--------|
| `control-plane/src/k8s/job_builder.rs` | Full rewrite: two containers, sidecar, volumes, secrets, init container, security context, resource limits per FR-24 through FR-32 |
| `control-plane/src/types/mod.rs` | Add NEMO_RESULT output types, stage name mapping |
| `control-plane/src/config/mod.rs` | Add services section for nemo.toml |

## Plan

### Step 1: NEMO_RESULT output types and stage name mapping

**Why this first:** Foundation types needed by job builder, entrypoint, and control plane
**Files:** `control-plane/src/types/mod.rs`, `control-plane/src/types/verdict.rs`
**Approach:** Add NemoResult envelope, stage-specific data types, stage name mapping functions
**Tests:** Unit tests for serialization roundtrips
**Depends on:** nothing
**Blocks:** Steps 2, 4

### Step 2: Enhanced K8s Job Builder

**Why this first:** Core Rust code that must compile and pass tests
**Files:** `control-plane/src/k8s/job_builder.rs`
**Approach:** Rewrite to include two containers (agent + sidecar), init container for iptables, all volume mounts per FR-24-32, resource limits per FR-28, security contexts, env vars per FR-27
**Tests:** Unit tests verifying job spec structure
**Depends on:** Step 1
**Blocks:** Step 3

### Step 3: Config services section

**Why this first:** Needed for TEST stage affected_services computation
**Files:** `control-plane/src/config/mod.rs`
**Approach:** Add `[services.<name>]` section with `path` and `test` fields
**Tests:** Config deserialization tests
**Depends on:** nothing
**Blocks:** Step 4

### Step 4: Prompt Templates

**Why this first:** Static files, no compilation, referenced by job builder
**Files:** `.nemo/prompts/implement.md`, `.nemo/prompts/review.md`, `.nemo/prompts/spec-audit.md`, `.nemo/prompts/spec-revise.md`
**Approach:** Create templates per FR-33 through FR-40 with `{{PLACEHOLDER}}` syntax
**Tests:** N/A (static files)
**Depends on:** nothing
**Blocks:** Step 5

### Step 5: Base Agent Image (Dockerfile + Entrypoint)

**Why this first:** Docker build files, no Rust compilation needed
**Files:** `images/base/Dockerfile`, `images/base/nemo-agent-entry`
**Approach:** Multi-stage Dockerfile per FR-1-3, entrypoint script per FR-4-13
**Tests:** Dockerfile syntax validation (docker build dry-run if available)
**Depends on:** Step 4
**Blocks:** Step 6

### Step 6: Auth Sidecar (Go)

**Why this first:** Separate binary, independent build
**Files:** `images/sidecar/main.go`, `images/sidecar/go.mod`, `images/sidecar/Dockerfile`
**Approach:** Single Go binary with three HTTP servers per FR-14-23
**Tests:** Go unit tests
**Depends on:** nothing
**Blocks:** Step 7

### Step 7: Terraform Module

**Why this first:** Infrastructure provisioning, references all other components
**Files:** `terraform/main.tf`, `terraform/variables.tf`, `terraform/outputs.tf`, `terraform/k8s.tf`, `terraform/postgres.tf`
**Approach:** Complete Terraform module per FR-43-56
**Tests:** `terraform validate`
**Depends on:** Steps 5, 6
**Blocks:** nothing

## Acceptance Criteria Status

| Criterion | Status |
|-----------|--------|
| docker build of base agent image succeeds | Pending |
| Auth sidecar starts in under 2s | Pending |
| Auth sidecar injects correct headers | Pending |
| K8s Job with both containers correct | Pending |
| Prompt template variable injection | Pending |
| terraform init && terraform apply works | Pending |
| Job resource limits match FR-28 | Pending |
| iptables init container configured | Pending |
| Agent runs as non-root UID 1000 | Pending |
| TEST stage reads AFFECTED_SERVICES | Pending |

## Progress Log

| Date | Step | Status | Notes |
|------|------|--------|-------|
| 2026-03-29 | -- | Started | Created branch and plan |
