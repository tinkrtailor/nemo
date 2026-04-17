# Helm TUI: Log Pane Polish

## Overview

Three tightly-scoped improvements to the `nemo helm` TUI log pane: scrollback, compressed stage-result lines, and per-line timestamps. No API changes. Goal: `nemo helm` becomes usable as a primary "watch my loop converge" surface instead of a status-only dashboard.

A follow-up spec (`specs/helm-timeline-tui.md`, deferred) will add a gantt-style timeline pane driven by per-stage timing data the API doesn't yet expose. This spec intentionally avoids that dependency.

## Baseline

Current helm TUI: `cli/src/commands/helm.rs`.

- Log pane auto-scrolls to bottom. No way to pause or scroll back.
- Raw `NAUTILOOP_RESULT:{"stage":"review","data":{"verdict":...}}` JSON lines are printed verbatim — each is 1–3 KB of single-line JSON that blows past the pane width.
- Log lines have no timestamp. Impossible to tell elapsed time between stages or round transitions by eye.
- Line buffer: `MAX_LOG_LINES = 500`, `VecDeque<String>`.
- Key handling in `handle_key` dispatches `char`/`Enter`/arrow keys to selection + command triggers. No log-pane-specific keys.

## Problem Statement

### Problem 1: Cannot scroll the log pane

The log pane renders newest-at-bottom. When a stage emits a burst of output (e.g. a claude stream-json `NAUTILOOP_RESULT` line, or a traceback), earlier lines scroll off-screen and are unrecoverable within helm. The user has to `Ctrl-C`, run `nemo logs <id>` in another terminal, and piece the context together manually.

**Manifestation:** user reported "I don't think I can scroll the logs" during dogfooding session 2026-04-17.

### Problem 2: `NAUTILOOP_RESULT` JSON dominates the pane

Every stage ends with a single line of the form:

```
NAUTILOOP_RESULT:{"stage":"review","data":{"verdict":{"clean":false,"confidence":0.88,"issues":[{"severity":"medium","category":"correctness","file":"control-plane/src/api/mod.rs","line":116,"description":"...","suggestion":"..."}, ...],"summary":"..."},"token_usage":{"input":5,"output":1712},"exit_code":0,"session_id":"4603dacd-..."}}
```

That's the terminal verdict for the stage — important — but as one 1–3 KB line it renders as either truncated garbage or a multi-line JSON vomit depending on wrap settings. Either way it drowns the actual progress lines (`[review/r1] Starting review with claude...`).

**Manifestation:** log pane is effectively illegible once any stage completes; users can't tell at a glance whether a stage passed or failed.

### Problem 3: No timestamps on log lines

Every line looks temporally identical. A user watching helm can't tell whether the last progress line arrived 2 seconds ago (normal) or 2 minutes ago (stuck). `nemo logs` via SSE has the timestamp available in the `LogEventResponse.timestamp` field but helm discards it when it converts to `String`.

**Manifestation:** hard to tell a hung stage from a healthy one; users restart loops prematurely.

## Functional Requirements

### FR-1: Log pane scrollback

**FR-1a.** The log pane supports the following key bindings when focused (i.e. a loop is selected):

| Key         | Action                                    |
| ----------- | ----------------------------------------- |
| `PgUp`      | Scroll up one pane height                 |
| `PgDn`      | Scroll down one pane height               |
| `Home`      | Jump to oldest line in buffer             |
| `End`       | Jump to newest line (resume auto-scroll)  |
| `k` / `↑`   | Scroll up one line (only when scrolled)   |
| `j` / `↓`   | Scroll down one line (only when scrolled) |

**FR-1b.** Default behavior is auto-scroll to bottom (unchanged). The first `PgUp` or `Home` press pins the view and disables auto-scroll. New log lines continue to accumulate in the buffer but do not move the viewport. `End` resumes auto-scroll.

**FR-1c.** While scrolled (not at bottom), the pane title shows a `[paused]` indicator so the user knows new lines are arriving off-screen.

**FR-1d.** `↑` / `↓` / `k` / `j` on the log pane must not be hijacked from the loops-list selection. They apply to the log pane only when the log pane has focus. Focus model: `Tab` cycles focus between `loops-list` and `log-pane`. Default focus is `loops-list` (current behavior).

### FR-2: Compressed `NAUTILOOP_RESULT` lines

**FR-2a.** When a log line matches the prefix `NAUTILOOP_RESULT:`, parse the JSON body and render a single compressed summary line instead of the raw JSON. Shape depends on stage:

| Stage        | Summary format                                                                      |
| ------------ | ----------------------------------------------------------------------------------- |
| `implement`  | `✓ implement r{round} · {output_tokens} tokens · {duration_or_blank}`                |
| `revise`     | `✓ revise r{round} · {output_tokens} tokens · {duration_or_blank}`                   |
| `test`       | `{✓ or ✗} test r{round} · {ci_status} · {services_count} service(s)`                 |
| `review`     | `{✓ or ✗} review r{round} · clean={clean} · {issue_count} issue(s) · conf={conf}`    |
| `audit`      | `{✓ or ✗} audit r{round} · clean={clean} · {issue_count} issue(s) · conf={conf}`     |

Color: `✓` in green, `✗` in red. Stage name in bold. `clean=false`, non-zero issues, or a non-passed `ci_status` triggers `✗`.

**FR-2b.** On JSON parse failure, fall back to rendering the first 200 chars of the raw line (not the full line). Never panic the TUI on a malformed `NAUTILOOP_RESULT:`.

**FR-2c.** The compressed line is emitted exactly once per `NAUTILOOP_RESULT:` line received. The raw JSON is NOT retained in the visible buffer. (If diagnostic access to the raw JSON is ever needed, the user can read it via `nemo inspect` — that's the canonical surface for verdict data.)

**FR-2d.** Lines that do not start with `NAUTILOOP_RESULT:` are rendered unchanged (post FR-3 timestamp prefix).

### FR-3: Per-line timestamps

**FR-3a.** The helm log buffer stores `(timestamp: chrono::DateTime<Utc>, line: String)` instead of just `String`. Timestamp is taken from the SSE `LogEventResponse.timestamp` field (persisted source) or `Utc::now()` at receive time (pod-logs source).

**FR-3b.** Each rendered line is prefixed with `HH:MM:SS` in MUTED color (the existing `MUTED` palette constant), followed by a single space, then the log content. Example:

```
10:11:42  [implement/r1] Starting implement (loop fd6c013b...)
10:12:15  ✓ implement r1 · 1,712 tokens
10:12:18  [test/r1] Starting test
10:12:21  ✓ test r1 · passed · 0 service(s)
```

**FR-3c.** Timestamp format is local time (not UTC), 24-hour, `HH:MM:SS` only (no date). Date context is implicit for a session-length helm view.

### FR-4: No regressions to existing behavior

**FR-4a.** `nemo helm` still connects, polls status every 2s, and opens an SSE stream for the selected loop (current behavior — no changes to `spawn_status_task`, `stream_logs_for_selection`, or the API).

**FR-4b.** `--tail` mode via `AgentPod` / `SidecarPod` `LogSource` still works. Pod-sourced lines get FR-3b timestamps at receive time.

**FR-4c.** Switching log source (agent / sidecar / persisted) preserves scrollback state per source, OR resets it cleanly. Either is acceptable; the spec does not require per-source independent scroll state.

**FR-4d.** `q`/`Esc` still quits. Existing command-trigger keys (approve / cancel / resume) still work regardless of log-pane focus state.

## Non-Functional Requirements

### NFR-1: No API changes

All changes land entirely in `cli/src/commands/helm.rs` and any adjacent CLI-only modules. No control-plane changes. No new endpoints. No schema changes.

### NFR-2: Buffer size unchanged

`MAX_LOG_LINES = 500` stays. The FR-2a compression naturally reduces buffer pressure; FR-3 only adds a tiny per-line overhead (8 bytes for a `DateTime<Utc>`). No memory budget concerns.

### NFR-3: No external dependencies added

Rendering uses existing `ratatui` widgets. `ratatui::widgets::Paragraph` already supports scroll offsets via `.scroll((y, x))`. No new crate dependencies.

### NFR-4: Test coverage

Unit tests in `cli/src/commands/helm.rs` (as `#[cfg(test)] mod tests`) for:

- FR-2a: each stage's compression produces the expected summary string from a representative `NAUTILOOP_RESULT:` fixture.
- FR-2b: malformed JSON returns a truncated-raw fallback and does not panic.
- FR-3b: a given `(DateTime, &str)` renders with the expected `HH:MM:SS ` prefix.
- FR-1b: a scroll-paused state correctly reports `[paused]`.

No TUI-integration tests required; the ratatui rendering layer is exercised manually.

## Acceptance Criteria

A reviewer can verify by:

1. **Scroll:** run `nemo helm`, select a loop with >1 round of activity, press `PgUp`. Viewport pauses, title shows `[paused]`. Press `End`. Auto-scroll resumes.
2. **Compression:** trigger a completed loop (`nemo start specs/health-json-body.md; nemo approve <id>`). Watch helm. Each stage completion shows as a single colored summary line, not a multi-line JSON blob.
3. **Timestamps:** every log line starts with `HH:MM:SS` in muted gray. The time between consecutive lines is visually obvious.
4. **Regression check:** `nemo helm --team`, `nemo helm` with no active loops, `Tab` cycling focus, `q` to quit — all still work.
5. **Unit tests:** `cargo test -p nemo cli::commands::helm` passes with the four tests above.

## Out of Scope

- **Timeline / gantt pane.** Deferred to `specs/helm-timeline-tui.md`. Needs the API to expose per-stage `started_at` / `completed_at` / `duration_secs` from the `rounds` table (those columns already exist in the schema — migration `20260328000001_initial_schema.sql` — but `InspectResponse.rounds[].implement/test/review/audit/revise` currently only carry the opaque `output` JSON, not timing).
- **Search / filter in the log pane.** Would be nice; separate spec.
- **Multi-loop log multiplexing** (watching N loops' logs in one pane). Current single-selected-loop model stays.
- **Mouse wheel scroll.** ratatui does surface mouse events, but wiring them up is orthogonal to keyboard scrollback and can ride in a follow-up.

## Files Likely Touched

- `cli/src/commands/helm.rs` — all of FR-1, FR-2, FR-3. Change `VecDeque<String>` → `VecDeque<(DateTime<Utc>, LogLine)>` where `LogLine` is either `Raw(String)` or `Summary{...}`. Add `scroll_offset: usize` + `paused: bool` app state. Extend `handle_key` with log-pane bindings behind a `focus == LogPane` gate.
- Possibly `cli/src/api_types.rs` — no changes expected; existing `LogEventResponse` already carries `timestamp`.

## Baseline Branch

`main` at the time this spec is hardened. Implementation branch will be auto-generated by the loop engine as `agent/<engineer>/helm-tui-log-polish-<hash>`.
