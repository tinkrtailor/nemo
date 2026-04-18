# Helm TUI Phase 2

## Overview

Second pass on `nemo helm` after the Phase 1 polish (#126: scrollback / compressed summaries / timestamps) and live introspection pane (#129) have landed. Focus: make helm the primary surface engineers reach for, not `nemo status` + `nemo logs` + `kubectl` in three tabs. Delight is the goal; cost visibility, actionability, and a compact always-visible "what is nautiloop doing right now" summary are the mechanism.

Not scoped: new core features. This spec is purely about TUI surface area over capabilities that already exist on the server.

## Baseline

Main at PR #137 (pod-live-introspection) merge.

Current helm TUI (post-phase-1):
- Loops list (left) + log pane (right).
- Scrollback, timestamps, compressed `NAUTILOOP_RESULT:` summaries.
- Introspect pane toggle (`p`) — shows live `ps` output, CPU/mem, worktree SHA.
- Status polling every 2s. SSE log stream per selected loop.
- No cost visibility, no PR hyperlinks, no cancel/approve keybinds, no multi-loop view, no at-a-glance dashboard, no diff preview, no notifications, no theming.

Observed pain during the 5-spec parallel dogfood run today:
- User had to switch between `nemo helm`, `nemo status` (another terminal), `gh pr list` (another), and `kubectl top` (another) to answer "what is happening."
- Convergence events (PR opening) gave no signal beyond a state transition in the loops list — easy to miss while looking at the log pane.
- Cost tracking was entirely absent; multi-hour loops consumed thousands of tokens with no running total visible.
- Approve/cancel always meant `Ctrl-Z` out of helm → run CLI → `fg` back in.

## Problem Statement

### Problem 1: No at-a-glance "what's nautiloop doing right now"

The loops list shows state per loop, but no roll-up. Engineer watching 4 parallel loops has to scan 4 rows + mentally aggregate. A single compact header summary ("4 loops: 2 implementing, 1 reviewing, 1 converged — 3h 22m cumulative compute, $1.47 tokens") collapses that into a glance.

### Problem 2: Cost is invisible

Nautiloop emits token usage in every `NAUTILOOP_RESULT` verdict (`token_usage.input`, `token_usage.output`). The server can sum these per round / per loop. Helm shows none of it. Engineers can't tell a cheap converged loop ($0.05) from a punishing churn loop ($1.50) without reading per-stage JSON by hand.

### Problem 3: Actionable events require leaving helm

Approving a PENDING loop, cancelling a runaway loop, extending a FAILED loop — all require typing a second command in another terminal. Helm knows the loop ID and state already; should be one keybind.

### Problem 4: Convergence events are silent

A loop finishes at round 7 in the middle of a 3-hour wait. The only signal: a row in the loops list changes color. If the user's eyes aren't on that pane, they miss it by hours. Desktop notifications / terminal bell fix this for a marginal cost.

### Problem 5: Reviewing the actual code change requires leaving helm

When a PR opens, the only way to see what was changed is `gh pr view --web` → browser. A diff pane (showing the last round's commits, file by file) keeps the loop inside the terminal.

### Problem 6: One loop at a time

Four loops running in parallel, one visible set of logs. If the user wants to see what the other three are doing, they switch selection and lose their place on the first. A vertical split showing 2-4 loops' recent log summary lines simultaneously is a lot more useful.

## Functional Requirements

### FR-1: Compact header summary

**FR-1a.** A one-line header at the top of helm ALWAYS shows:

```
nautiloop · 4 active · 2 impl · 1 review · 0 harden · 1 awaiting · 1.2M tokens · $0.84 · 3h 22m
```

Fields:
- `N active`: non-terminal loops count
- stage breakdown: counts per current stage
- `X tokens`: cumulative input+output tokens across all non-terminal loops (sum of `usage.input_tokens + usage.output_tokens` from every round's `NAUTILOOP_RESULT` across all stages)
- `$X.XX`: estimated cost, computed from the per-model token prices in a new `nemo.toml` `[pricing]` section (see FR-7)
- `Xh Xm`: cumulative wall-clock time across all non-terminal loops (sum of `rounds[].duration_secs` from the inspect endpoint)

**FR-1b.** Header updates every 2s in line with status polling. No new endpoint — derived client-side from `/status` + per-loop `/inspect` calls (batched).

**FR-1c.** When helm is launched with `--team`, header shows all engineers' loops and labels accordingly: `nautiloop · team view · 12 active · ...`.

**FR-1d.** When no loops are active, header shows `nautiloop · no active loops · press s to start a new spec`.

### FR-2: Cost + token columns in loops list

**FR-2a.** Loops list gains two new columns: `tokens` (short K/M format: `52K`, `1.2M`) and `cost` (`$0.34`).

**FR-2b.** Values computed from the same `token_usage` sum + `[pricing]` lookup as FR-1a.

**FR-2c.** Terminal loops show their final cost; active loops show running total. Both use the same `$X.XX` format.

### FR-3: In-TUI actions

**FR-3a.** When a loop is selected, these keybinds are bound:

| Key | Action | Condition |
|---|---|---|
| `a` | Approve | state == `AWAITING_APPROVAL` |
| `x` | Cancel (confirm with `y`) | state is non-terminal |
| `r` | Resume | state in {`PAUSED`, `AWAITING_REAUTH`, transient `FAILED`} |
| `e` | Extend `--add 10` | state == `FAILED` with `failed_from_state` |
| `o` | Open PR in browser | `spec_pr_url.is_some()` |
| `i` | Toggle inspect pane | always |

**FR-3b.** Action taken → status line (below log pane) shows `✓ approved a3a83333` or `✗ approve failed: <reason>` for 3s, then clears.

**FR-3c.** Actions gated by state: if keybind is invalid for current state, status line shows `cannot <action> in state <X>`, no API call made.

**FR-3d.** `x` (cancel) requires a confirmation keypress (`y` within 3s). No other destructive action requires confirm (approve/resume/extend/open are all reversible or read-only).

### FR-4: Convergence notifications

**FR-4a.** When any loop transitions to `CONVERGED`, `HARDENED`, `SHIPPED`, or `FAILED`, helm:
- Emits a terminal bell (`\a`). Defeats inattention without requiring desktop integration.
- Writes a one-line status-bar notification: `✓ CONVERGED: agent/dev/foo-a1b2c3d4 → https://github.com/org/repo/pull/137`.
- Highlights the now-terminal loop's row with a one-second color flash, then returns to normal muted.

**FR-4b.** Optional desktop notification via `notify-rust` crate (Linux/macOS native). Gated by `~/.nemo/config.toml` `[helm] desktop_notifications = true` (default false so there's no surprise).

**FR-4c.** No audio beyond the bell. No notification for in-progress state transitions (only terminal).

### FR-5: Diff preview pane

**FR-5a.** New keybind `d` toggles a diff preview pane. Shows the most recent round's commits against the loop's branch base (origin/main).

**FR-5b.** Source of truth: a new API endpoint `GET /diff/:loop_id?round=<n>` that returns unified diff text from `git diff origin/main...<branch>` scoped to the round's commit range. Server renders to text; client displays verbatim in a ratatui `Paragraph` with syntax-light color for `+`/`-` lines.

**FR-5c.** Diff pane respects scrollback (same keybinds as log pane).

**FR-5d.** For diffs > 100KB, the endpoint returns a truncation line and the client shows `[truncated — open PR for full diff]`. Avoids pulling 10MB diffs into the terminal.

### FR-6: Multi-loop log split view

**FR-6a.** New keybind `m` toggles a multi-loop view: the log pane splits horizontally into N slots (2, 3, or 4 based on terminal height), each showing the top 5-10 lines of the N most recently-active loops' log streams.

**FR-6b.** Each slot has its own compact header (`helm-polish · implement r3`) and auto-scrolls. No interaction per slot — this is a read-only dashboard.

**FR-6c.** Pressing `m` again returns to single-selected-loop view.

### FR-7: Pricing config

**FR-7a.** New `[pricing]` section in `nemo.toml` with per-model input/output token prices:

```toml
[pricing]
"claude-opus-4-6" = { input_per_1m = 15.00, output_per_1m = 75.00 }
"claude-haiku-4-5" = { input_per_1m = 1.00, output_per_1m = 5.00 }
"gpt-4o-mini" = { input_per_1m = 0.15, output_per_1m = 0.60 }
```

**FR-7b.** If a model has no entry, cost for that model is treated as unknown; the cost column shows `$?.??` and the header summary cost excludes that loop's contribution, with a footnote indicator (`†` after the header cost: `$0.84†`).

**FR-7c.** Prices ship with a sane default set for Claude Haiku/Sonnet/Opus 4.x and common OpenAI models. Engineer / repo can override.

### FR-8: Theming

**FR-8a.** New `[helm] theme` config setting: `"dark"` (current, default), `"light"`, or `"high-contrast"`.

**FR-8b.** Themes are built-in, not user-authored. Defined in `cli/src/commands/helm/themes.rs` as three `Theme { bg, surface, border, text, muted, teal, amber, green, red, blue }` structs.

**FR-8c.** Keybind `T` cycles themes at runtime (no restart).

### FR-9: Rounds table pane

**FR-9a.** New keybind `R` toggles a rounds-table pane for the selected loop. Replaces the log pane (not an overlay — the table is denser and benefits from full width).

**FR-9b.** Table shape, one row per round, columns:

| # | Stages | Verdict | Issues | Conf | Tokens | $ | Duration |
|---|--------|---------|--------|------|--------|---|----------|
| 1 | ✓I ✓T ✗R | not clean | 3 (1m 2l) | 0.88 | 52K | $0.18 | 3m 22s |
| 2 | ✓I ✓T ✓R | **clean** | 0 | 0.92 | 48K | $0.17 | 2m 41s |

Columns:
- `#` — round number
- `Stages` — compact per-stage marker: `I` implement, `T` test, `R` review (or `A` audit, `V` revise for harden loops). Prefixed with `✓`/`✗` based on stage exit. Muted if stage didn't run that round.
- `Verdict` — final review/audit verdict for the round: `clean` (green, bold) or `not clean` (amber). Blank if review didn't run.
- `Issues` — issue count by severity: `3 (1h 1m 1l)` = 3 total, 1 high, 1 medium, 1 low. `critical` shown with `c` prefix. Empty → `0`.
- `Conf` — reviewer/auditor confidence (0.00-1.00).
- `Tokens` — sum of input+output tokens across all stages this round (K/M format).
- `$` — cost per round, using the same `[pricing]` lookup as FR-2.
- `Duration` — wall-clock time for the round (sum of per-stage `duration_secs` from `InspectResponse`).

**FR-9c.** The table scrolls (same keybinds as log pane: PgUp/PgDn/Home/End). For a round with >20 findings, only counts are shown; selecting the row with Enter drops into a detail view (FR-9d).

**FR-9d.** Pressing `Enter` on a selected row opens a detail pane: full verdict summary + list of issues (severity · category · file:line · description, one per line). Press `Esc` or `R` again to return.

**FR-9e.** Harden-loop variant: for loops in `Hardening` state, the `Stages` column uses `A` (audit) / `V` (revise) markers and the `Verdict` column reflects the audit's `clean` field. Same table shape otherwise. This is the primary use case when deciding whether to approve a hardened spec — you see the full convergence trace at a glance before accepting.

**FR-9f.** The rounds table always shows up to the loop's final round. For an active loop, the current round's row shows stages in progress as `~I` / `~T` / `~R` (tilde prefix = running).

### FR-10: Post-harden approval context

**FR-10a.** When a loop is in `AWAITING_APPROVAL` (either post-harden before implement starts, or at the convergence gate), helm's header adds an approval hint: `[a] approve  [x] cancel  [R] see rounds`. Same for `CONVERGED` loops where a PR is open: `[o] open PR  [R] see rounds`.

**FR-10b.** The rounds table view (FR-9) is the natural landing spot before hitting `a` — engineers can inspect what they're approving. Specifically: for a harden loop, the rounds table shows which audit findings were addressed in which revise round, with durations and costs.

**FR-10c.** Acceptance criterion: an engineer who has never seen a spec before can press `R`, scan the table, and decide whether to approve in under 30s for a typical 3-round harden loop.

## Non-Functional Requirements

### NFR-1: No new server-side surface beyond FR-5

`/diff/:loop_id` is the only new endpoint. Everything else (pricing, theming, keybinds, header summary) is CLI-only.

### NFR-2: Polling budget

Header + loop list refresh stays at 2s. Introspect pane stays at 2s. The new `/diff` endpoint is only called on demand (keybind `d`), not polled.

### NFR-3: Startup time

Helm must still reach interactive state within 1 second of invocation. Theme, pricing, and config loading all happen synchronously at startup from `~/.nemo/config.toml` and `nemo.toml`; no network calls on init beyond the existing `/status` call.

### NFR-4: Backward compatibility

`nemo helm` invoked with no new config lands in the same behavior as today, just with:
- Header summary (additive)
- Token/cost columns (show `$?.??` if pricing not configured, no crash)
- Keybinds (additive; no existing keybind changes meaning)

### NFR-5: Tests

- **Unit** (`cli/src/commands/helm/summary.rs`): header string generation given a mock status response.
- **Unit** (`cli/src/commands/helm/cost.rs`): token → cost calc with / without pricing entries.
- **Unit** (`cli/src/commands/helm/actions.rs`): keybind → action dispatch respects state gates.
- **Integration**: none required; ratatui rendering is validated manually.

## Acceptance Criteria

A reviewer can verify by:

1. Launch `nemo helm` during 4 parallel loops. Header shows live count + stage breakdown + cumulative tokens + cost + wall time. Updates every 2s.
2. Loops list shows `tokens` and `cost` columns with live running totals.
3. Select an AWAITING_APPROVAL loop, press `a`. Status line shows `✓ approved <id>`. Loop transitions to IMPLEMENTING within 15s.
4. Select a FAILED-with-max-rounds loop, press `e`. Loop extended by 10 rounds and resumed. Status line confirms.
5. Let a loop converge. Terminal beeps; status line shows the PR URL; the row flashes briefly.
6. Press `d` on a converged loop. Diff pane shows the merged commits. Scroll works.
7. Press `m`. Log pane splits into 2-4 sub-panes showing recent lines per active loop. Press `m` again: back to single.
8. Press `T`. Theme cycles. Colors change cleanly without artifacts.
9. Select a multi-round loop, press `R`. Table shows one row per round with stage markers, verdict, issue counts, tokens, cost, duration. Press `Enter` on row 1 to see the full issue list. `Esc` returns to table. `R` returns to log pane.
10. Select a harden-phase loop in `AWAITING_APPROVAL`. Header shows `[a] approve  [R] see rounds`. Press `R`, see full audit/revise trace. Press `a`, loop transitions forward.

## Out of Scope

- **Mouse support.** Keyboard-first TUI. Click-to-open is a future nice-to-have (would need ratatui mouse event wiring).
- **Custom user themes.** Built-ins only for this pass.
- **Per-model cost aggregation chart.** Delightful but scope creep. Header shows totals.
- **Historical cost reporting** (cost by engineer this week, etc.). Needs a new endpoint aggregating across closed loops — separate spec.
- **Diff syntax highlighting beyond `+`/`-` coloring.** Language-aware highlighting is heavy; revisit if real usage demands it.
- **Keyboard shortcuts palette (`?`)**. Would be nice; follow-up.

## Files Likely Touched

- `cli/src/commands/helm.rs` — add header, keybinds, multi-loop view.
- `cli/src/commands/helm/summary.rs` — new: compact-header rendering.
- `cli/src/commands/helm/cost.rs` — new: token → cost math.
- `cli/src/commands/helm/actions.rs` — new: keybind dispatcher.
- `cli/src/commands/helm/themes.rs` — new: built-in themes.
- `cli/src/commands/helm/diff_pane.rs` — new: diff rendering.
- `cli/src/commands/helm/multi_view.rs` — new: split-pane renderer.
- `cli/src/commands/helm/rounds_table.rs` — new: FR-9 rounds table + FR-10 detail pane.
- `cli/src/client.rs` — new client methods (approve, cancel, resume, extend, diff).
- `control-plane/src/api/mod.rs` + `handlers.rs` — new `/diff/:loop_id` route.
- `Cargo.toml` — new dep `notify-rust` (optional, gated by FR-4b config flag).
- Tests per NFR-5.

## Baseline Branch

`main` at PR #137 merge.
