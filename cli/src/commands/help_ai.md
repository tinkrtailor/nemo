# Nautiloop — LLM Operator Guide

Nautiloop is a convergent-loop orchestrator that takes a specification (spec) and
drives it to a merged pull request through adversarial implement → test → review
cycles. It is self-hosted, model-agnostic (Claude, OpenAI, or mixed), and runs
every agent job in an isolated Kubernetes pod with no access to secrets.

## State Machine

A loop progresses through the following states. Each node is a state; arrows
show transitions and their triggers.

```
                       ┌──────────────────────────────────────────────────┐
                       │                   PENDING                        │
                       └──┬──────────────┬───────────────┬────────────────┘
                          │              │               │
                (harden)  │  (no-harden) │  (ship/auto)  │
                          ▼              │               │
                    ┌───────────┐        │               │
                    │ HARDENING │──────┐ │               │
                    └──┬────┬───┘      │ │               │
           (start mode)│    │          │ │               │
                       │    │(harden   │ │               │
                       │    │ _only)   │ │               │
                       │    ▼          │ │               │
                       │  ┌──────────┐ │ │               │
                       │  │ HARDENED  │ │ │               │
                       │  │(terminal) │ │ │               │
                       │  └──────────┘ │ │               │
                       ▼               ▼ ▼               │
            ┌─────────────────────────────────┐          │
            │       AWAITING_APPROVAL          │          │
            │  (engineer runs `nemo approve`)  │          │
            └──────────────┬──────────────────┘          │
                           │              ▲               │
                           ▼              │ (judge        │
                                          │  escalates)   │
    ┌───────────────────────────────────────────────────┐ │
    │                    IMPLEMENTING                    │◀┘
    │  (agent writes code in isolated pod)              │──┐
    └──────────┬───────────────────────────────────────┘  │
               │                          ▲          ▲     │
               ▼                          │          │     │
    ┌────────────────┐    (tests fail) ───┘          │     │
    │    TESTING      │──────────────────────────────────┐ │
    └───────┬────────┘                               │   │ │
            │ (tests pass)                           │   │ │
            ▼                                        │   │ │
    ┌────────────────┐   (reviewer requests changes) │   │ │
    │   REVIEWING     │──────────────────────────────┘   │ │
    └───┬───┬────────┘                                   │ │
        │   │ (reviewer approves)                        │ │
        │   ▼                                            │ │
        │ ┌────────────────┐   `nemo ship`   ┌────────┐ │ │
        │ │   CONVERGED     │──────────────▶ │SHIPPED │ │ │
        │ └────────────────┘                 └────────┘ │ │
        │                                                │ │
        │ (max rounds exceeded from IMPLEMENTING,        │ │
        │  TESTING, REVIEWING, or HARDENING)             ▼ ▼
        │                                    ┌────────────────────┐
        └───────────────────────────────────▶│       FAILED       │
                                             │  (terminal, but    │
                                             │   recoverable via  │
                                             │   `nemo extend`)   │
                                             └────────┬───────────┘
                                                      │
                                    (nemo extend) ────┘──▶ IMPLEMENTING

    ┌─────────────────────────────────────────────────────────────────────┐
    │  PAUSED, AWAITING_REAUTH, CANCELLED — reachable from any active    │
    │  state (see transitions table below).                              │
    │                                                                     │
    │  • Any non-terminal ──(nemo cancel)──▶ CANCELLED (terminal)        │
    │  • Any non-terminal ──(internal)─────▶ PAUSED ──(nemo resume)──▶   │
    │    (previous state)                                                 │
    │  • Any active ──(expired creds)──▶ AWAITING_REAUTH                 │
    │    ──(nemo auth + nemo resume)──▶ (previous state)                 │
    └─────────────────────────────────────────────────────────────────────┘
```

### Terminal States

| State       | Meaning |
|-------------|---------|
| CONVERGED   | Review approved; PR is open and ready to merge. |
| HARDENED    | Spec hardened (harden-only mode); no implementation was run. |
| FAILED      | Max rounds exceeded or unrecoverable error. Recoverable via `nemo extend`. |
| CANCELLED   | Operator cancelled the loop via `nemo cancel`. |
| SHIPPED     | PR auto-merged after convergence (ship mode). |

Note: FAILED is terminal but recoverable — `nemo extend --add N <id>` resets
max_rounds and resumes the loop from its `failed_from_state`.

### All Transitions

| From               | To                  | Trigger |
|--------------------|---------------------|---------|
| PENDING            | HARDENING           | Reconciler picks up loop (harden mode) |
| PENDING            | AWAITING_APPROVAL   | Reconciler picks up loop (no-harden mode) |
| PENDING            | IMPLEMENTING        | Reconciler picks up loop (ship mode / auto-approve) |
| HARDENING          | AWAITING_APPROVAL   | Harden job completes (start mode — proceed to implement) |
| HARDENING          | HARDENED            | Harden job completes (harden_only mode — terminal) |
| HARDENING          | FAILED              | Harden job fails / max rounds exceeded / audit issues |
| AWAITING_APPROVAL  | IMPLEMENTING        | Engineer approves (`nemo approve`) |
| IMPLEMENTING       | TESTING             | Implementation job completes |
| TESTING            | REVIEWING           | Test job completes (tests pass) |
| TESTING            | IMPLEMENTING        | Test job completes (tests fail) |
| REVIEWING          | CONVERGED           | Reviewer approves |
| REVIEWING          | IMPLEMENTING        | Reviewer requests changes |
| REVIEWING          | AWAITING_APPROVAL   | Judge escalates during review |
| IMPLEMENTING       | FAILED              | Max rounds exceeded |
| REVIEWING          | FAILED              | Max rounds exceeded |
| TESTING            | FAILED              | Max rounds exceeded |
| FAILED             | IMPLEMENTING        | `nemo extend` (resumes from failed_from_state) |
| CONVERGED          | SHIPPED             | `nemo ship` auto-merge completes |
| Any non-terminal   | CANCELLED           | `nemo cancel` |
| Any non-terminal   | PAUSED              | Internal pause trigger |
| PAUSED             | (previous state)    | `nemo resume` |
| Any active         | AWAITING_REAUTH     | Loop engine detects expired model credentials |
| AWAITING_REAUTH    | (previous state)    | `nemo auth` + `nemo resume` |

## Typical Workflows

### Workflow 1: Implement (with hardening, the default)

```
$ nemo start spec.md                 # Submit spec; harden phase runs first
  Loop ID: 8cb88352-...
  Phase plan: HARDEN → APPROVE → IMPLEMENT

$ nemo status                        # Wait for AWAITING_APPROVAL
$ nemo approve 8cb88352-...          # Approve after reviewing hardened spec PR
$ nemo logs 8cb88352-...             # Watch implement → test → review cycles
  ... loop converges ...
  # PR is ready for merge
```

### Workflow 2: Implement (skip hardening)

```
$ nemo start spec.md --no-harden     # Skip harden, go straight to approval gate
$ nemo approve <id>
$ nemo logs <id>
```

### Workflow 3: Ship (fully autonomous)

```
$ nemo ship spec.md                  # No approval, no human; auto-merges PR
  # Ship skips hardening by default.
  # Use `nemo ship --harden spec.md` to harden first.
```

### Workflow 4: Harden-only

```
$ nemo harden spec.md               # Harden the spec, then stop
  # Lifecycle: PENDING → HARDENING → HARDENED (terminal)
  # Review the hardened spec PR; no implementation runs.
```

## Recovery Playbooks

### AWAITING_REAUTH — model credentials expired

The loop engine detected an expired model token (e.g., Claude OAuth) and paused
the loop. This surfaces as the AWAITING_REAUTH state, not as an HTTP error.

```
$ nemo auth --claude                 # Re-push fresh Claude credentials
$ nemo resume <id>                   # Resume the loop
```

### PAUSED — loop paused internally

```
$ nemo resume <id>                   # Resume the loop
```

### FAILED — max rounds exceeded

```
$ nemo inspect <branch>              # Check round history and verdicts
$ nemo extend --add 10 <id>          # Add 10 more rounds and resume
  # OR investigate the root cause in the spec/tests
```

## Configuration Hierarchy

Configuration is resolved in priority order (highest first):

| Level    | Location                  | Description |
|----------|---------------------------|-------------|
| Engineer | `~/.nemo/config.toml`     | Personal config: server URL, API key, engineer name, model preferences. |
| Repo     | `nemo.toml` (on main)     | Per-repository defaults: default models, pricing config. |
| Cluster  | Control plane ConfigMap   | Cluster-wide defaults set by the platform admin. |

Engineer-level settings override repo-level, which override cluster-level.

**Tip: multiple clusters.** If the control plane URL has changed or you manage
multiple clusters, use `--server` to target a specific control plane per command
instead of editing your config: `nemo status --server https://my-other-cluster:8080`.

## Command Catalog

### Loop Lifecycle

| Command        | Description |
|----------------|-------------|
| `harden`       | Harden spec, merge spec PR. Terminal: HARDENED |
| `start`        | Implement spec, create PR. Terminal: CONVERGED |
| `ship`         | Implement + auto-merge. Terminal: SHIPPED |
| `approve`      | Approve a loop awaiting approval |
| `cancel`       | Cancel a running loop |
| `resume`       | Resume a PAUSED, AWAITING_REAUTH, or transient-FAILED loop |
| `extend`       | Extend a FAILED loop's max_rounds and resume |

### Observability

| Command    | Description |
|------------|-------------|
| `status`   | Show your running loops |
| `logs`     | Stream logs for a loop |
| `ps`       | Show live processes and runtime state of a loop's pod |
| `inspect`  | Show detailed loop state, round history, and verdicts |
| `helm`     | K9s-style loop overview with live logs (TUI) |
| `cache`    | Show cache configuration and disk usage |

### Identity

| Command  | Description |
|----------|-------------|
| `auth`   | Push local model credentials to cluster |
| `models` | Show authenticated providers and available models |

### Config

| Command        | Description |
|----------------|-------------|
| `init`         | Scan monorepo, generate nemo.toml |
| `config`       | Edit ~/.nemo/config.toml |
| `capabilities` | Show CLI version and supported features (JSON) |
| `help`         | Show help for nemo or a specific command |

## Spec Structure

A spec is a Markdown document that describes the feature or fix to implement.
Minimum skeleton:

```markdown
# Feature Title

## Overview
One-paragraph description of what to build and why.

## Functional Requirements
- FR-1: First requirement
- FR-2: Second requirement

## Acceptance Criteria
1. First criterion that must be true when the loop converges.
2. Second criterion.
```

More detail produces better results. Include API contracts, edge cases, error
handling expectations, and test scenarios when available.

## Known Failure Modes

### Reviewer nitpick-loops

**Detection**: Round count climbs without convergence; review verdicts keep
requesting minor stylistic changes.

**Recovery**: Check `nemo inspect <branch>` for reviewer verdicts. Consider
adjusting the reviewer model or extending rounds. If the reviewer is too strict,
update the spec with explicit acceptance criteria to guide convergence.

### Max rounds exhaustion

**Detection**: Loop enters FAILED state. `nemo inspect <branch>` shows
`failed_from_state` indicating which stage it was in when it failed.

**Recovery**: `nemo extend --add 10 <id>` to give it more rounds. If the loop
keeps failing in the same stage, the spec may need clarification.

### Network drops mid-pod

**Detection**: Pod shows as terminated in `nemo ps <id>`. Logs may be incomplete.

**Recovery**: The loop engine detects pod failures and will retry on the next
reconciliation tick. If the loop is stuck, check `nemo status` for the current
state and `nemo logs <id>` for the last output.

## JSON Output

Most stateful commands support `--json` for machine-readable output:

```
$ nemo status --json          # Array of loop summaries
$ nemo approve <id> --json    # Action confirmation with loop state
$ nemo cancel <id> --json     # Action confirmation
$ nemo resume <id> --json     # Action confirmation
$ nemo extend <id> --json     # Round extension details
$ nemo models --json          # Provider and model availability
$ nemo auth --json            # Per-provider push results
$ nemo inspect <branch>       # Always JSON (no flag needed)
$ nemo cache show --json      # Cache configuration
$ nemo capabilities           # Always JSON (no flag needed)
```
