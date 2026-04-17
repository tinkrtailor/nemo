# Local Spec Upload

## Overview

Let `nemo start` / `nemo ship` / `nemo harden` operate on a spec file from the engineer's local working tree without requiring the spec to exist on `main` first. The CLI reads the file, ships the content to the control plane, and the loop engine commits it onto the agent branch as the first commit. The spec rides along inside the PR the loop produces.

This unblocks the product vision of "nautiloop converges on a spec end to end": draft locally, hand it to nautiloop, get back a hardened spec PLUS the implementation in a single PR.

## Baseline

Main at PR #126 merge.

Current `POST /start` flow (`control-plane/src/api/handlers.rs:23`):

```
1. CLI sends { spec_path, engineer, ... }
2. API: git.read_file(spec_path, default_ref)         ← requires spec on main
3. API: generate_branch_name(engineer, spec_path, content)
4. API: create_branch from default_ref
5. API: insert loop row, return 201
```

The spec file must already be committed to the default branch. If it isn't, step 2 returns a NotFound and the loop never starts.

CLI `nemo start`, `nemo ship`, `nemo harden` all send `{ spec_path }` and assume the server can resolve it on the default branch.

## Problem Statement

### Problem: Draft specs must round-trip through main

To use nautiloop, the engineer has to:

1. Write the spec locally.
2. Open a PR with just the spec file.
3. Get it reviewed / self-approve / admin-merge.
4. Wait for main to update.
5. Run `nemo start specs/foo.md`.

This is actively hostile to the product's core use case. The spec is the input — it shouldn't need to be pre-merged to ask nautiloop to work on it. The whole point of the harden loop is that specs are drafts until the machine polishes them. Forcing a merge of a draft *before* hardening defeats the purpose:

- The main branch fills with WIP specs that may or may not produce working implementations.
- Iterating on a spec requires repeated commits to main.
- `nemo ship` (harden + implement + auto-merge) can't actually take a draft spec as input — it requires an already-reviewed spec.

**Manifestation:** noted during dogfood session 2026-04-17 while trying to submit `specs/helm-tui-log-polish.md`. The PR-to-main step is pure friction.

## Functional Requirements

### FR-1: CLI sends spec content, not just path

**FR-1a.** `nemo start <SPEC_PATH>`, `nemo ship <SPEC_PATH>`, `nemo harden <SPEC_PATH>` read the file at `<SPEC_PATH>` relative to the engineer's current working directory. If the file does not exist locally, error immediately with a clear message — do NOT fall back to the server's default-branch lookup.

**FR-1b.** The file's full UTF-8 content is included in the `StartRequest` body as a new optional field `spec_content: Option<String>`. When present, it takes precedence over the server's default-branch read.

**FR-1c.** The `spec_path` in the request is preserved as-is (e.g. `specs/foo.md`). This becomes the path on the agent branch where the spec is written.

**FR-1d.** Max size: 1 MB per spec (enforced server-side; see FR-3b). Larger specs error with HTTP 413 and a clear message.

### FR-2: API accepts spec_content and commits it to the agent branch

**FR-2a.** `POST /start` treats `spec_content` as authoritative when present:

- Skip the `git.read_file(spec_path, default_ref)` call.
- Use `spec_content` directly for the content hash (branch name generation).
- After `create_branch` from the default ref succeeds, call `git.write_file(agent_branch, spec_path, spec_content)` to commit the spec onto the agent branch. Commit message: `chore(spec): add {spec_path}`.
- The first round's `current_sha` is the SHA of this spec commit, not the default-branch tip.

**FR-2b.** When `spec_content` is absent (legacy callers or server-side automation), existing behavior is preserved: read from default branch, fail if absent. This keeps a zero-breaking-change API surface.

**FR-2c.** If the spec already exists on the default branch at `spec_path`, the local content still wins when `spec_content` is provided — the agent branch starts with the engineer's local version. No merge conflict. No warning.

**FR-2d.** `generate_branch_name(engineer, spec_path, spec_content)` already uses a content hash, so local-vs-remote content divergence produces different branch names. No collision.

### FR-3: Safety and limits

**FR-3a.** `spec_content` MUST be valid UTF-8. Binary content is rejected with HTTP 400.

**FR-3b.** `spec_content.len() > 1_048_576` (1 MB) → HTTP 413. Spec templates fit in a few KB; 1 MB is a sanity cap, not a feature.

**FR-3c.** `spec_path` MUST match the existing path validation (already applied elsewhere — no `..` traversal, no absolute paths, must end in `.md`). No new validation needed; FR-2a just reuses the existing check.

**FR-3d.** The spec commit inherits the engineer's identity (the same name/email already used for implement/revise commits). The first commit on the agent branch is attributed to the engineer, not "nautiloop-bot".

### FR-4: Harden loop preserves local-spec provenance

**FR-4a.** When the harden loop runs `revise` and modifies the spec, subsequent commits go to the agent branch as normal — no change needed here.

**FR-4b.** The hardened spec PR (when `auto_merge_spec_pr = true`) merges the agent branch's spec changes into the default branch. This is the ONLY time the spec reaches main: after hardening, not before. This is the desired behavior and flows naturally from FR-2.

**FR-4c.** `nemo ship` with a local draft spec now does what the product actually promises: harden → implement → auto-merge, starting from a draft the engineer has never pushed.

### FR-5: CLI output

**FR-5a.** `nemo start` output is unchanged on the happy path. On the error path, the error message distinguishes "file not found locally" (new, FR-1a) from "server rejected request" (existing).

**FR-5b.** A single line is added to the output after `Branch: ...`:

```
  Spec:   specs/foo.md (local, 1,234 bytes)
```

This gives the engineer a receipt that the local content was the one used.

## Non-Functional Requirements

### NFR-1: Backward compatibility

Existing CI scripts, MCP integrations, or any caller hitting `POST /start` without `spec_content` keep working via FR-2b. No schema migration.

### NFR-2: Request size

1 MB spec + ~2 KB of StartRequest overhead fits comfortably in axum's default body limit. No server tuning required.

### NFR-3: Audit logging

The existing loop-creation log line is extended with a `spec_source = "local" | "default_branch"` field so operators can trace which path produced a given loop.

### NFR-4: Tests

- **Unit** (`control-plane/src/api/handlers.rs`): `start` with `spec_content: Some("...")` uses the provided content; `start` without it falls back to `git.read_file`; oversized `spec_content` returns 413; invalid UTF-8 returns 400.
- **Integration** (`control-plane/tests/`): full flow — submit a start with local spec content that does NOT exist on main, verify the agent branch is created and the first commit is the spec file.
- **CLI** (`cli/src/commands/start.rs` tests): reading a local file and populating `spec_content`; fail fast when the file doesn't exist.

## Acceptance Criteria

A reviewer can verify by:

1. **Local-only spec:** on a fresh clone with no changes on main, create `specs/new-feature.md` locally (do NOT commit), run `nemo start specs/new-feature.md`. Loop starts. Agent branch on the remote contains the spec file. Main remains untouched.
2. **Legacy path still works:** from another machine without the local file, submit the same `spec_path` that DOES exist on main via a direct API call with no `spec_content` field. Loop starts (FR-2b).
3. **Size limit:** a 2 MB spec file is rejected client-side or server-side with a clear 413 message.
4. **Harden end-to-end from draft:** `nemo harden specs/new-feature.md` against a local-only spec runs audit → revise → audit until clean, opens a PR that merges the hardened spec into main. Main receives the spec ONLY after hardening converges.
5. **Ship end-to-end from draft:** `nemo ship specs/new-feature.md` (with `[ship] allowed = true`) harvests the draft, hardens, implements, and auto-merges a single PR that contains both the final spec and the implementation.

## Out of Scope

- **Multi-file specs** (spec + supporting design docs in one submission). A spec is one `.md` file; supporting docs should be linked or inlined.
- **Spec editing after start.** If the engineer realises the spec was wrong mid-loop, they cancel and resubmit. No "update spec on running loop" feature.
- **Remote spec branches** (`nemo start --spec-branch=foo`). Solving 80% of the pain via local-file upload first; remote branch lookup is a nice-to-have that can ride in a follow-up spec if demand materializes.
- **Web UI / spec marketplace.** Out of band.

## Files Likely Touched

- `control-plane/src/types/api.rs` — add `pub spec_content: Option<String>` to `StartRequest`.
- `control-plane/src/api/handlers.rs` — implement FR-2a/b/c/d; extend audit log (NFR-3).
- `control-plane/src/git/mod.rs` — ensure `write_file` (already used by revise) is callable from the start handler path for the initial spec commit.
- `cli/src/commands/start.rs`, `ship.rs`, `harden.rs` — read local file, populate `spec_content` (FR-1).
- `cli/src/api_types.rs` — mirror the new optional field.

## Baseline Branch

`main` at PR #126 merge.
