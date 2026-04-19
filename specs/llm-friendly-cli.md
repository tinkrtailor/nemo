# LLM-Friendly nemo CLI

## Overview

Make `nemo` self-describing enough that any LLM (Claude Code, Cursor, codex, opencode, an arbitrary agent built on a raw API) can learn to operate nautiloop end-to-end by reading the CLI's own output. Standard `--help` is a starting point; this spec adds examples, workflows, mental-model primers, a mega-help command, and machine-readable JSON output.

Goal: `nemo help ai` is the single command an agent runs to understand what nautiloop is, what state a loop can be in, and how to drive a complete workflow without reading source or docs.

## Baseline

Main at PR #159 merge. Implementation should begin after #159 is merged to main. If starting earlier, rebase onto main after #159 lands.

Current state:
- `nemo --help` / `nemo -h` / `nemo help` — flat list of 17 commands with one-line descriptions. (This spec adds `capabilities` as a new top-level command and replaces clap's built-in `help` with a custom `help` subcommand, bringing the user-visible count to 19: 17 existing + `capabilities` + custom `help`. This matches the FR-6b `commands` array.)
- `nemo <command> --help` — shows flags + positional args for that command. No examples. No context.
- `nemo help <command>` — same as `nemo <command> --help` (clap's default).
- No machine-readable output. No workflow-level documentation. No mental-model primer.
- No way to dump everything in one call.

What works today: an LLM with repo access can `cat cli/src/main.rs` and figure it out. What doesn't: an LLM without repo access, or one limited to shell-only tool access, has to probe 17 commands to build a mental model.

## Problem Statement

### Problem 1: Help text is accurate but context-free

`nemo approve --help` says:

```
Approve a loop awaiting approval
Usage: nemo approve [OPTIONS] <LOOP_ID>
Arguments:  <LOOP_ID>  Loop ID
Options:    --server <SERVER>
```

An LLM reads this and learns: "there's an approve command that takes a loop ID." It does NOT learn: **when** approve is needed (only on loops in `AWAITING_APPROVAL`), **what happens** on successful approve (state transitions to implement dispatch), **how to get a loop ID** (`nemo status`), or **what error you get** if the state is wrong.

### Problem 2: No overall mental model anywhere in the CLI

Nautiloop has a non-trivial state machine (PENDING → AWAITING_APPROVAL → IMPLEMENTING → TESTING → REVIEWING → {CONVERGED, Implementing-again, FAILED}). There's a separate harden flow. There are terminal states vs resumable states. There's an engineer/cluster/repo config hierarchy. None of this is discoverable from `--help`.

An LLM without this context guesses or fails. An LLM with this context reasons correctly.

### Problem 3: No examples

A single worked example per command — "here's the output, here's the typical next step" — teaches an LLM more than 10 lines of flag descriptions. We have zero examples anywhere.

### Problem 4: No JSON / parseable output

Every CLI output is free-form text. Agents parsing `nemo status` to find a loop ID have to regex the table. `nemo status --json` would be a one-line win.

### Problem 5: No "what went wrong" context on errors

When `nemo approve` fails because the loop is in `IMPLEMENTING`, the error is:

```
Error: API error (409 Conflict): {"error":"Cannot approve: loop is in IMPLEMENTING, not AWAITING_APPROVAL"}
```

An LLM can parse that. But a friendlier error would name the recovery path: "Loops in IMPLEMENTING are already running; no approve needed. Use `nemo logs <id>` to watch." That converts an error into a workflow cue.

## Functional Requirements

### FR-1: `nemo help ai` — the mega-primer

**FR-1a.** New subcommand `nemo help ai` (also `nemo help llm`) prints a single comprehensive Markdown document covering:

> **Alias mechanism**: `llm` is implemented as a second accepted value for the positional argument (i.e., `match "ai" | "llm" => ...` in the handler), not as a clap alias. This keeps the help output clean — only `ai` appears in the subcommand listing.

> **Note on state names**: The authoritative state enum is `LoopState` in `control-plane/src/types/mod.rs`. At time of writing it contains: Pending, Hardening, AwaitingApproval, Implementing, Testing, Reviewing, Converged, Failed, Cancelled, Paused, AwaitingReauth, Hardened, Shipped. The `help_ai.md` template must reflect whatever states exist in that enum at implementation time.

- **What nautiloop is**: one paragraph. Convergent loop orchestrator, cross-model adversarial review, self-hosted.
- **State machine diagram** (ASCII): the full loop lifecycle with every transition. The authoritative transitions to include (derived from the loop engine driver's reconcile logic) are:

  | From | To | Trigger |
  |---|---|---|
  | PENDING | HARDENING | Reconciler picks up loop (harden mode) |
  | PENDING | AWAITING_APPROVAL | Reconciler picks up loop (no-harden mode) |
  | PENDING | IMPLEMENTING | Reconciler picks up loop (ship mode / auto-approve) |
  | HARDENING | AWAITING_APPROVAL | Harden job completes (start mode — proceed to implement) |
  | HARDENING | HARDENED | Harden job completes (harden_only mode — terminal) |
  | HARDENING | FAILED | Harden job fails / max rounds exceeded / audit issues |
  | AWAITING_APPROVAL | IMPLEMENTING | Engineer approves (`nemo approve`) |
  | IMPLEMENTING | TESTING | Implementation job completes |
  | TESTING | REVIEWING | Test job completes (tests pass) |
  | TESTING | IMPLEMENTING | Test job completes (tests fail) |
  | REVIEWING | CONVERGED | Reviewer approves |
  | REVIEWING | IMPLEMENTING | Reviewer requests changes |
  | REVIEWING | AWAITING_APPROVAL | Judge escalates during review (requires engineer re-approval) |
  | IMPLEMENTING | FAILED | Max rounds exceeded |
  | REVIEWING | FAILED | Max rounds exceeded |
  | TESTING | FAILED | Max rounds exceeded |
  | FAILED | IMPLEMENTING | `nemo extend` (resumes from failed_from_state) |
  | CONVERGED | SHIPPED | `nemo ship` auto-merge completes |
  | Any non-terminal | CANCELLED | `nemo cancel` |
  | Any non-terminal | PAUSED | Internal pause trigger |
  | PAUSED | (previous state) | `nemo resume` |
  | Any active | AWAITING_REAUTH | Loop engine detects expired model credentials |
  | AWAITING_REAUTH | (previous state) | `nemo auth` + `nemo resume` |

  The implementer should render this as an ASCII diagram in the Markdown template. The exact visual layout is left to the implementer, but all transitions above must be represented.
- **Terminal states**: CONVERGED, HARDENED, FAILED, CANCELLED, SHIPPED — what each means. Note: FAILED is terminal but recoverable via `nemo extend` (which resets max_rounds and resumes the loop from its failed_from_state). The mega-primer should clearly distinguish "terminal unless explicitly extended" so LLM consumers can reason about state reachability.
- **Typical workflow 1 (implement)**: `nemo start spec.md` → `nemo approve <id>` → `nemo logs <id>` → wait → PR.
- **Typical workflow 2 (harden-first)**: `nemo start spec.md` → review hardened spec PR → `nemo approve <id>` → watch → PR. (Harden is the default; use `--no-harden` to skip it. The `--harden` flag is deprecated and emits a warning.)
- **Typical workflow 3 (ship)**: `nemo ship spec.md` → (no approval, no human) → auto-merged PR. Ship skips hardening by default. Use `nemo ship --harden spec.md` to harden first. (This differs from `start`, which hardens by default.)
- **Typical workflow 4 (harden-only)**: `nemo harden spec.md` → loop hardens the spec → review hardened spec PR → loop terminates at HARDENED. Use this for spec refinement without implementation. The lifecycle is PENDING → HARDENING → HARDENED (terminal).
- **Recovery playbooks**: AWAITING_REAUTH → `nemo auth --claude` then `nemo resume <id>` (Claude token expiry surfaces as this state, not as an HTTP error — the loop engine detects it internally and transitions the loop). PAUSED → `nemo resume`. FAILED (max rounds) → `nemo extend --add 10 <id>` OR investigate. Stale kubectl context? Don't switch, use `--context=<name>` per command.
- **Config hierarchy**: engineer (`~/.nemo/config.toml`) > repo (`nemo.toml` on main) > cluster (control plane ConfigMap). Explain which lives where.
- **Command catalog**: full list with one-line descriptions, same as `nemo --help`, but grouped into categories: loop lifecycle, observability, identity, config.
- **Example spec structure**: the minimum skeleton a spec needs — overview, FRs, acceptance criteria. Points at `docs/spec-authoring.md` (future) if it exists.
- **Known failure modes**: reviewer nitpick-loops, max_rounds exhaustion, network drops mid-pod. For each, how to detect and recover.

**FR-1b.** Output is Markdown, ~200-400 lines, readable end-to-end. Generated from a template file embedded in the binary via `include_str!`. Maintained in `cli/src/commands/help_ai.md` — not duplicated per-platform.

**FR-1c.** `nemo help ai --format=json` emits the same information as structured JSON. For agents that prefer to parse rather than read prose.

> **Why `--format` here vs `--json` on stateful commands**: The `help` subcommand's default output is Markdown prose, not structured data, so a format selector (`--format=json`) is more semantically appropriate than a boolean toggle. Stateful commands default to plain-text tables where `--json` is a natural boolean switch. In v1, `--format` accepts only `json` (and defaults to Markdown when omitted). Future formats (e.g., `yaml`) may be added later but are out of scope.

The JSON schema for `nemo help ai --format=json`:

```json
{
  "overview": "string — what nautiloop is",
  "state_machine": {
    "states": [
      { "name": "PENDING", "terminal": false, "description": "string" }
    ],
    "transitions": [
      { "from": "PENDING", "to": "AWAITING_APPROVAL", "trigger": "string" }
    ]
  },
  "workflows": [
    {
      "name": "implement",
      "description": "string",
      "steps": [
        { "command": "nemo start spec.md", "description": "string" }
      ]
    }
  ],
  "recovery_playbooks": [
    {
      "state": "AWAITING_REAUTH",
      "description": "string",
      "commands": ["nemo auth --claude", "nemo resume <id>"]
    }
  ],
  "config_hierarchy": {
    "levels": [
      { "name": "engineer", "path": "~/.nemo/config.toml", "description": "string" }
    ]
  },
  "command_catalog": {
    "loop_lifecycle": [
      { "command": "start", "short": "string" }
    ],
    "observability": [],
    "identity": [],
    "config": []
  },
  "spec_structure": "string — minimum skeleton description",
  "known_failure_modes": [
    { "name": "string", "detection": "string", "recovery": "string" }
  ]
}
```

All top-level keys are required. The ASCII state machine diagram from the Markdown version is represented as structured `states` and `transitions` arrays rather than a rendered diagram.

### FR-2: Per-command `long_about` with examples

**FR-2a.** Every command in `cli/src/main.rs` gets a clap `#[command(long_about = ...)]` attribute containing:

- The existing short description (unchanged)
- A blank line
- **Example:** section with 1-3 realistic invocations and their expected outputs
- **See also:** list of related commands

Example for `nemo approve`:

```
Approve a loop awaiting approval.

Moves a loop from AWAITING_APPROVAL to the next active stage. Required for:
- Loops started with `nemo start` (PENDING → AWAITING_APPROVAL → approve → IMPLEMENTING)
- Loops that hardened first and are waiting for engineer review of the hardened spec

Does nothing useful on any other state; errors with 409 Conflict.

Example:
  $ nemo approve 8cb88352-5cf4-4dda-9cd0-6a0d6851ba92
  Approved loop 8cb88352-5cf4-4dda-9cd0-6a0d6851ba92
    State: AWAITING_APPROVAL
    Implementation will start on next reconciliation tick.

See also: nemo status (find loop IDs), nemo logs (watch after approve).
```

**FR-2b.** Both `nemo <cmd> --help` and `nemo help <cmd>` show the full `long_about` with examples. This is clap's natural behavior when `long_about` is set.

> **Implementation note**: Clap displays `long_about` for both `--help` and `help <cmd>` by default. No custom help template is needed for this requirement. The short description (from `about`) is shown only in the parent command's subcommand listing (e.g., `nemo --help`).

**FR-2c.** Applied to ALL subcommands: harden, start, ship, status, helm, logs, ps, cancel, approve, inspect, resume, extend, init, auth, models, config, capabilities, cache show (the actionable subcommand — the `cache` parent uses clap's auto-generated subcommand listing and does not need a custom `long_about`). The `help` subcommand itself does not need a custom `long_about` since it IS the help system.

### FR-3: `--json` output mode on every stateful command

**FR-3a.** Commands whose output an agent might parse get a `--json` flag:
- `nemo status --json` — **already implemented**; preserve existing output schema (loops array with `loop_id`, `state`, etc.)
- `nemo inspect <branch> --json` — already emits JSON by default; add `--json` as a no-op flag for consistency so agents can pass `--json` uniformly without error
- `nemo approve <id> --json` — structured response object
- `nemo cancel <id> --json` — structured response
- `nemo resume <id> --json` — structured response
- `nemo extend <id> --json` — structured response
- `nemo models --json` — providers + available models as JSON
- `nemo auth --json` — push results as JSON
- `nemo cache show --json` — **already implemented**; preserve existing output schema

> **Note on excluded commands**: `ps`, `logs`, `helm`, `start`, `ship`, `harden`, `init`, and `config` do not get `--json` in this spec. `ps` output is ephemeral pod-level data primarily useful for debugging, not for agent automation workflows. `logs` streams text. `helm` is a TUI. `start`/`ship`/`harden` are fire-and-forget submission commands whose response is a simple acknowledgment (the loop ID is printed to stdout and is trivially parseable). `init` and `config` are local setup commands. If agent demand emerges for structured `ps` output, it can be added as a follow-up.

> **Note**: `status` and `cache show` already support `--json` with established output schemas. Do not change their existing field names or structure. New `--json` implementations on other commands should follow the same conventions (snake_case keys, `serde_json::to_string_pretty`).

**FR-3b.** JSON output schema documented in `nemo help ai` (FR-1). Stable field names, no presentation-level keys. The JSON output schemas for newly added `--json` commands are:

**`nemo approve <id> --json`:**
```json
{
  "loop_id": "uuid-string",
  "state": "AWAITING_APPROVAL",
  "approve_requested": true,
  "message": "Approved loop — implementation will start on next reconciliation tick."
}
```

> **Field mapping note**: The `state` and `approve_requested` fields mirror the server's `ApproveResponse` struct directly. `state` is the loop's state at the time the action was processed. `approve_requested` is `true` when the approve was accepted. The CLI does not synthesize `previous_state`/`new_state` fields — the server API does not return transition information, and a pre-fetch would introduce a race condition. The `message` field is synthesized CLI-side and is not part of the server response — the CLI constructs this human-readable string after a successful API call.

> **Note on `message` field**: All `--json` response schemas for action commands (`approve`, `cancel`, `resume`, `extend`) include a `message` field. This field is always CLI-synthesized (not from the server response) and provides a human-readable summary of the action taken. The remaining fields are passed through from the server's response struct.

**`nemo cancel <id> --json`:**
```json
{
  "loop_id": "uuid-string",
  "state": "IMPLEMENTING",
  "cancel_requested": true,
  "message": "Loop cancelled."
}
```

> Same field convention as `approve`: `state` is the loop's state at action time; `cancel_requested` confirms the cancellation was accepted.

**`nemo resume <id> --json`:**
```json
{
  "loop_id": "uuid-string",
  "state": "PAUSED",
  "resume_requested": true,
  "message": "Loop resumed."
}
```

> Same field convention: `state` at action time; `resume_requested` confirms acceptance.

**`nemo extend <id> --json`:**
```json
{
  "loop_id": "uuid-string",
  "prior_max_rounds": 10,
  "new_max_rounds": 20,
  "resumed_to_state": "IMPLEMENTING",
  "message": "Extended by 10 rounds."
}
```

> **Field mapping note**: Field names mirror the server's `ExtendResponse` struct directly (`prior_max_rounds`, `new_max_rounds`, `resumed_to_state`). The CLI passes these through without renaming to avoid a translation layer and reduce confusion when debugging against API responses.

**`nemo models --json`:**
```json
{
  "providers": [
    {
      "name": "claude",
      "models": ["claude-opus-4", "claude-sonnet-4", "claude-haiku-4"],
      "valid": true,
      "updated_at": "2025-01-15T10:30:00Z"
    },
    {
      "name": "openai",
      "models": ["gpt-5.4", "gpt-4o", "o1-preview", "o1-mini"],
      "valid": false,
      "updated_at": null
    }
  ]
}
```

> **Provider naming**: Uses `"claude"` (not `"anthropic"`) to match the codebase's internal provider naming convention (consistent with `nemo auth --claude`). Models are listed as flat string arrays from the hardcoded `CLAUDE_MODELS` / `OPENAI_MODELS` constants — there is no per-model role assignment since any model can serve any role. The `valid` and `updated_at` fields are passed through directly from the server's `ProviderInfo` struct (flat fields, not nested) — consistent with the spec's principle of avoiding translation layers between server responses and CLI JSON output.

**`nemo auth --json`:**
```json
{
  "results": [
    { "provider": "claude", "status": "ok", "messages": ["Token pushed to cluster."] },
    { "provider": "openai", "status": "skipped", "messages": ["No local token found."] },
    { "provider": "ssh", "status": "ok", "messages": ["Key pushed to cluster."] }
  ]
}
```

> **SSH provider**: `nemo auth` pushes credentials for three providers by default: `claude`, `openai`, and `ssh`. All three appear in the JSON output. The `messages` field is an array (not a single string) because the auth flow can produce multiple diagnostic messages per provider (e.g., `["disk credentials stale", "using fresh keychain entry"]`).

> **Provider filtering**: When provider-specific flags are passed (e.g., `nemo auth --claude --json`), only the requested providers appear in the `results` array. When no provider flags are passed (default: all providers), all three providers (`claude`, `openai`, `ssh`) appear in the output.

> All schemas use snake_case keys and are emitted via `serde_json::to_string_pretty`, consistent with existing `status` and `cache show` output.

**FR-3c.** `--json` always emits JSON regardless of TTY status. Without `--json`, output is always plain text regardless of TTY status (no auto-detection). There is no implicit format switching based on whether stdout is a terminal.

### FR-4: Error messages include recovery hints

**FR-4a.** The API returns specific error codes for state-transition violations. The CLI catches the common ones and adds a recovery hint line:

| HTTP Status | Server error pattern | CLI-added recovery hint |
|---|---|---|
| 409 | "Cannot approve: loop is in IMPLEMENTING" | "Loops in IMPLEMENTING are already running. Run `nemo logs <id>` to watch." |
| 409 | "Cannot cancel: loop is in CONVERGED" | "This loop has already completed. Check the PR with `nemo inspect <branch>`." |
| 409 | "Cannot approve: loop is in PENDING" | "Wait ~5s for the reconciler to advance PENDING → AWAITING_APPROVAL, then retry." |
| 401 | "Authentication failed" | "Check your API key with `nemo config`. If expired, regenerate and update ~/.nemo/config.toml." |
| 401 | "Unknown engineer" | "Run `nemo auth` to register your engineer identity with the cluster." |
| 404 | "Spec not found" / "not found" (on start/ship) | "Ensure the spec file exists at the given path. Run `nemo start --help` for usage." |

> **Note on Claude token expiry**: The server error "Claude token expired" is internal to the loop engine's auth-error detection (in `driver.rs`) and surfaces as an AWAITING_REAUTH state transition, not as a 401 HTTP error to the CLI. Recovery for token expiry is covered in the AWAITING_REAUTH recovery playbook in FR-1a, not here.

**FR-4b.** Recovery hints are CLI-side; server doesn't change. Each hint lives in `cli/src/commands/error_hints.rs` as `(pattern, hint)` pairs. Unknown errors pass through unchanged.

Matching rules:
- Patterns are **case-insensitive substring** matches against the error message body.
- Patterns are checked **in definition order**; **first match wins** (no accumulation).
- Where possible, combine substring matching with HTTP status code (e.g., 409 + "approve" → state conflict hint) to reduce fragility.
- If the server changes its error message format in a future version, hints gracefully degrade: unmatched errors pass through with no hint rather than showing a wrong hint.

> **Fragility note**: String-based pattern matching is inherently coupled to server error message wording. This is acceptable for v1 since the server and CLI are co-versioned. If the server adds structured error codes in the future, hints should migrate to code-based matching.

**FR-4c.** `--no-hints` is a **global flag** on the top-level `Cli` struct (alongside `--server` and `--insecure`), since error hints can appear on any command that hits the API. It suppresses recovery hints for scripting contexts where stable error output matters.

> **Integration point**: The hint system is a top-level error handler in `main()`. When the CLI's `run()` function returns an error, `main()` downcasts the `anyhow` error to `ApiError` (see below), calls `error_hints::find_hint(status_code, &error_message)`, and appends any matching hint to stderr before exiting. When `--no-hints` is set, the hint lookup is skipped and the raw error is printed unchanged. This keeps the hint logic out of individual command handlers and the client layer.

> **Structured error type**: To provide `find_hint` with structured fields (HTTP status code and error body) without fragile string parsing, introduce an `ApiError` struct in `cli/src/client.rs`:
>
> ```rust
> #[derive(Debug, thiserror::Error)]
> #[error("API error ({status}): {body}")]
> pub struct ApiError {
>     pub status: u16,
>     pub body: String,
> }
> ```
>
> The client returns `ApiError` wrapped in `anyhow::Error` on non-2xx responses (replacing the current `anyhow!("API error ...")` string). In `main()`, the hint system downcasts via `err.downcast_ref::<ApiError>()` to extract `status` and `body`. Non-API errors (network failures, config errors, etc.) are not `ApiError` and skip the hint system entirely. This is an internal refactor to `client.rs` error construction — the `Display` output is identical to the current format, so external CLI behavior (NFR-1) is preserved.

### FR-5: `nemo help --all`

**FR-5a.** New flag: `nemo help --all` dumps every subcommand's long_about in one shot. One-call total CLI documentation.

**FR-5b.** Output format: Markdown, headings per command. Same prose as individual `nemo help <cmd>` but concatenated.

**FR-5c.** `nemo help --all --format=json` returns a single JSON object with the following schema:

```json
{
  "commands": {
    "approve": {
      "short": "Approve a loop awaiting approval",
      "long": "Full long_about text including examples...",
      "options": [
        {
          "name": "--server",
          "short": "-s",
          "type": "string",
          "required": false,
          "description": "Control plane server URL"
        }
      ],
      "positional_args": [
        {
          "name": "LOOP_ID",
          "required": true,
          "description": "Loop ID"
        }
      ]
    }
  }
}
```

- `commands` is a map from command name to command descriptor.
- `short`: the one-line `about` string.
- `long`: the full `long_about` text (including examples, see-also).
- `options`: array of flag/option descriptors. `short` is null if no short flag. `type` is one of `"bool"` or `"string"` — derived from clap's `ArgAction`: `SetTrue`/`SetFalse` → `"bool"`, everything else (`Set`, `Append`, `Count`) → `"string"`. There is no `"integer"` type; distinguishing integers from strings would require inspecting clap's value parser internals, which do not reliably expose type names at runtime. `Count` actions (e.g., verbosity flags like `-v`) are also mapped to `"string"` since they are rare — no current nemo commands use `Count`. Consumers that need numeric types should parse based on the argument name and context.
- `positional_args`: array of positional argument descriptors. Omitted (or empty array) if the command takes no positional args.

### FR-6: Version + capability report

**FR-6a.** `nemo --version` (existing) unchanged.

**FR-6b.** `nemo capabilities` (new) prints JSON describing which features this CLI version supports:

```json
{
  "version": "0.6.0",
  "commands": ["harden", "start", "ship", "status", "helm", "logs", "ps", "cancel",
               "approve", "inspect", "resume", "extend", "init", "auth", "models",
               "config", "cache", "help", "capabilities"],
  "features": {
    "qa_stage": false,
    "orchestrator_judge": true,
    "pluggable_cache": true,
    "harden_by_default": true,
    "nemo_extend": true,
    "pod_introspect": true,
    "dashboard": false
  }
}
```

**FR-6c.** Lets an agent check `nemo capabilities` once at startup and know what it can and cannot rely on in this CLI version. Avoids version-sniffing via `nemo --version` + external lookup.

> **Implementation note**: Feature flags in `cli/src/capabilities.rs` are hardcoded boolean constants, updated manually when features ship. They are NOT Cargo feature gates — they represent server-side/product-level capability presence, not compile-time conditional compilation. The `commands` array is derived from the clap `Command` definition at runtime (iterate `Cli::command().get_subcommands()`), so it is always self-consistent and automatically includes new subcommands like `capabilities` itself. The clap `Command` tree is constructed at runtime via the derive macro's generated code — this is runtime iteration, not a build script or proc macro.

> **Feature flag definitions**:
> - `qa_stage`: true when a dedicated QA/testing stage exists as a separate loop phase (not yet shipped)
> - `orchestrator_judge`: true when the judge-based review escalation path is active in the loop engine (judge can escalate REVIEWING → AWAITING_APPROVAL)
> - `pluggable_cache`: true when the cache backend is configurable (not hardcoded to a single provider)
> - `harden_by_default`: true when `nemo start` hardens specs before implementation by default
> - `nemo_extend`: true when the `nemo extend` command is available to resume failed loops with additional rounds
> - `pod_introspect`: true when `nemo ps` can inspect individual pod state and logs
> - `dashboard`: true when a web-based dashboard UI is available (not yet shipped)

### Implementation Note: Custom Help Subcommand

The built-in clap `help` subcommand must be replaced with a custom `Help` subcommand (using `#[command(name = "help")]` and `#[command(disable_help_subcommand = true)]` on the parent) that handles:

- `help ai` / `help llm` — FR-1 mega-primer
- `help --all` — FR-5 full dump
- `help --format=json` — JSON output for FR-1c and FR-5c
- `help <cmd>` — falls back to rendering the matching subcommand's `long_about` (or `about` if no `long_about` is set)
- `help` (no args) — renders the same output as `nemo --help` (subcommand listing)

> **Implementation note for `help <cmd>`**: Use `Cli::command().find_subcommand(name)` to retrieve the target subcommand's `Command` object, then render its help via `cmd.render_long_help()`. This uses clap's own rendering engine, so the output is identical to what `nemo <cmd> --help` would produce. No separate help text registry is needed.

> **Nested subcommand handling**: The positional argument for `help` accepts multiple values (e.g., `nemo help cache show`). For lookup, tokens are resolved by chaining `find_subcommand` calls: first `find_subcommand("cache")`, then on the result `find_subcommand("show")`. If the chain resolves to a valid subcommand, render its help. If only the parent resolves (e.g., `nemo help cache`), render the parent's help (which includes its subcommand listing via clap's default rendering). If no subcommand matches, emit an error: `Unknown command: <tokens>. Run 'nemo help' for a list of commands.`

The `--help` flag on individual subcommands continues to use clap's built-in `--help` handler, which naturally displays `long_about` when set.

**Flag interaction rules for the `help` subcommand:**

| Invocation | Behavior |
|---|---|
| `nemo help` | Renders the same output as `nemo --help` (subcommand listing) |
| `nemo help ai` | FR-1 mega-primer (Markdown) |
| `nemo help ai --format=json` | FR-1 mega-primer (JSON) |
| `nemo help <cmd>` | Renders the subcommand's `long_about` |
| `nemo help --all` | FR-5 full dump (Markdown) |
| `nemo help --all --format=json` | FR-5 full dump (JSON) |
| `nemo help --format=json` (no `--all`, no positional) | Error: `--format requires --all or a topic (e.g., 'ai')` |
| `nemo help --all ai` | Error: `--all and a specific topic are mutually exclusive` |
| `nemo help --all <cmd>` | Error: `--all and a specific command are mutually exclusive` |
| `nemo help cache` | Renders `cache`'s help (includes its subcommand listing) |
| `nemo help <cmd> --format=json` | Error: `--format is only supported with --all or 'ai'` |
| `nemo help cache show` | Renders `cache show`'s `long_about` (drills into nested subcommand) |

## Non-Functional Requirements

### NFR-1: No behavior change for existing commands

All existing command invocations produce identical stdout/stderr, except that `nemo --help` will list additional subcommands (`capabilities`) and the custom `help` subcommand with its new flags. The new flags (`--json`, `--no-hints`, `--all`, `--format=json`) are additive. The existing one-line-per-command help output format is preserved.

### NFR-2: `help` and `capabilities` bypass config and auth

`nemo help` (all variants: `help ai`, `help --all`, `help <cmd>`) and `nemo capabilities` must be dispatched **before** config loading and API key validation in `main()`. Currently, `main()` calls `config::load_config()` and exits with an error if `api_key` is missing (main.rs:260-272). Only `Init` and `Config` are special-cased before this gate. The `Help` and `Capabilities` commands must be added to this early-exit pattern so that unconfigured users and agents — the primary audience for these commands — can run them without a valid config file or API key.

### NFR-3: Tests

- **Unit** (`cli/src/commands/help_ai.rs`): `nemo help ai` renders with no errors; contains section headings for state machine, workflows, recovery.
- **Unit** (`cli/src/commands/*.rs`): each command's `long_about` contains "Example:" substring.
- **Integration**: `nemo help --all --format=json` parses as valid JSON with expected keys.
- **Unit** (`cli/src/commands/error_hints.rs`): for each `(pattern, hint)` pair, assert that a synthetic error message containing the pattern produces the expected hint. Also assert that an unrecognized error message produces no hint (passthrough).

## Acceptance Criteria

1. **LLM can operate nautiloop from `nemo help ai` alone**: an operator gives a fresh LLM (no prior nautiloop knowledge, no repo access) only the output of `nemo help ai`. The LLM can correctly describe how to submit a spec, approve it, watch it, and recover from AWAITING_REAUTH.
2. **Per-command examples**: `nemo help approve` shows an example invocation with expected output.
3. **JSON everywhere stateful**: `nemo status --json | jq '.[0].loop_id'` returns a UUID string. (Note: `status --json` emits a bare JSON array, not a `{"loops": [...]}` wrapper.)
4. **Error hints present**: invoke `nemo approve` on an IMPLEMENTING loop; stderr includes a recovery hint directing the user to `nemo logs`.
5. **Mega-help reachable**: `nemo help --all` prints all command docs. `nemo help --all --format=json` parses as valid JSON.
6. **Capabilities reflect build**: `nemo capabilities` returns a JSON object; `features.qa_stage` is false until #159 ships.
7. **No regressions**: all pre-existing `nemo` command invocations produce the same output as before, except that `nemo --help` lists the newly added subcommands.

## Out of Scope

- **Man pages.** Could auto-generate from clap, but `nemo help` is the better discovery surface for LLMs.
- **Shell completions.** Separate concern; easy follow-up with `clap_complete`.
- **Interactive TUI help browser.** `nemo helm` is already a TUI; a `?` keybind to open help docs inside helm is a phase-3 helm spec, not CLI.
- **Translating help to languages other than English.** English-only in v1.
- **Auto-generating docs site from help text.** Nice future; out of scope for this spec.
- **Examples that actually execute** (doctest-style). Too brittle when cluster state varies. Examples are illustrative text.

## Files Likely Touched

- `cli/src/main.rs` — add `long_about` to every subcommand; add `--json` flag to stateful ones; new `help ai`, `help --all`, `capabilities` subcommands.
- `cli/src/commands/help_ai.md` — new: embedded primer document.
- `cli/src/commands/help_ai.rs` — new: renders the primer (text / JSON).
- `cli/src/commands/error_hints.rs` — new: pattern → hint table + wrapping logic.
- `cli/src/client.rs` — surface error patterns to the hint system.
- `cli/src/commands/status.rs`, `inspect.rs`, `approve.rs`, `cancel.rs`, `resume.rs`, `extend.rs`, `models.rs`, `auth.rs` — add `--json` output paths.
- `cli/src/capabilities.rs` — new: compile-time feature flags → JSON.
- Tests per NFR-2.

## Baseline Branch

`main` at PR #159 merge. Implementation should begin after #159 is merged to main. If starting earlier, rebase onto main after #159 lands.
