# Mobile Dashboard

## Overview

Add a mobile-first web dashboard served by the existing control plane. Each loop renders as a card; tapping a card expands a detail view with rounds table, log tail, token/cost breakdown, and one-tap approve/cancel/extend actions. No new infrastructure: the dashboard lives at `/dashboard/*` on the existing axum server, same auth model as the API.

Security inherited from the deployment: production Hetzner setups already bind the control plane to a Tailscale IPv4 address (`terraform/examples/hetzner`), so the dashboard is reachable only from tailnet-joined devices. The operator adds their phone to Tailscale, bookmarks `https://<nautiloop-ts-ipv4>/dashboard`, and has a private surface.

No new process. No new port. No new auth system. Just HTML.

## Baseline

Main at PR #144 (two-sided divergence fix) merge.

Current surfaces for watching loops:
- `nemo helm` — TUI, terminal only, desktop-bound
- `nemo status` / `nemo logs` — CLI, terminal only
- Raw API: `/status`, `/inspect`, `/logs/:id`, `/pod-introspect/:id`

The CLI and TUI are excellent for engineers at their desk. They're useless when the operator is (a) on their phone, (b) away from their terminal, or (c) showing progress to a non-engineer.

## Problem Statement

### Problem 1: Nautiloop is not observable from a phone

Loops run for minutes to hours. The operator wants to know, from anywhere: are my loops healthy, how many have converged, is anything stuck, should I intervene. A CLI tunnel + tmux + mobile SSH app chain is not a real answer — it's too fragile and user-hostile.

### Problem 2: Stakeholder visibility

Explaining what nautiloop does to a non-engineer ("here's a terminal TUI, let me scroll...") doesn't land. Showing a live dashboard with cards animating through states tells the story instantly.

### Problem 3: One-tap actions from mobile

Today's approve / cancel / extend require the CLI. If the operator is AFK and a loop hits AWAITING_APPROVAL, they either push through on their phone via a dashboard OR wait until they're back. The first one scales.

## Functional Requirements

### FR-1: Route and static assets

**FR-1a.** New axum routes under the existing control-plane router:

| Route | Method | Response |
|---|---|---|
| `/dashboard` | GET | Main dashboard HTML (card grid) |
| `/dashboard/login` | GET/POST | Login form (API key) + set cookie, then redirect |
| `/dashboard/logout` | POST | Clear auth cookie |
| `/dashboard/loops/:id` | GET | Loop detail HTML (drawer or full page) |
| `/dashboard/stream/:id` | GET (SSE or JSON) | Dashboard-specific log proxy: returns the last 200 lines as SSE for active loops (auto-scrolling tail), or a JSON array (full log dump) for terminal loops. Unlike `/logs/:id` which streams the complete log, this endpoint is optimized for the dashboard's live log pane (FR-4a) with a line cap and HTML-safe escaping. |
| `/dashboard/static/*` | GET | Embedded CSS + JS assets |

**FR-1b.** Static assets (CSS, single JS file, optional icon/font) are **embedded in the binary** via `include_str!`/`include_bytes!`. No filesystem dependencies at runtime. Total asset budget: <50 KB gzipped.

**FR-1c.** All dashboard HTML is server-rendered (axum handler returns `Html<String>`). Templating via `askama` (compile-time, already a family of the rust ecosystem) or `maud` — pick one, stick with it. No SPA framework. No React/Vue/Svelte.

### FR-2: Auth

**FR-2a.** The dashboard requires the same API key used by `nemo` CLI. Two acceptance paths:
- **Cookie** `nautiloop_api_key=<key>` (HttpOnly, Secure, SameSite=Strict). Set by `/dashboard/login` form POST. Expires in 7 days.
- **Bearer header** (`Authorization: Bearer <key>`) for programmatic access (e.g., curl, external scripts) — same as current CLI → API auth.

The dashboard auth middleware accepts **either** a valid cookie **or** a valid Bearer header on any `/dashboard/*` route (except `/login` and `/static/*`). Dashboard JS does **not** read the cookie value; instead, same-origin `fetch()` calls automatically include the HttpOnly cookie. The middleware checks the cookie first, falls back to Bearer. This preserves XSS protection (JS never touches the key) while keeping programmatic access viable.

**Local development note:** The `Secure` flag prevents cookies from being sent over plain HTTP. When the bind address is `127.0.0.1` or `localhost`, the `Secure` flag should be conditionally omitted so that authentication works without TLS. Production deployments (non-localhost bind) MUST always set `Secure`.

**FR-2b.** `/dashboard/login` renders a form with two inputs: `api_key` (password field) and `engineer_name` (text field, used for the `Mine` filter — see FR-3e). On POST, validates the API key via constant-time comparison against the `NAUTILOOP_API_KEY` environment variable (same logic as the existing auth middleware — no internal HTTP call). If valid, sets two cookies: `nautiloop_api_key=<key>` (HttpOnly, Secure, SameSite=Strict, 7-day expiry) and `nautiloop_engineer=<name>` (HttpOnly, Secure, SameSite=Strict, 7-day expiry), then redirects to `/dashboard`. If invalid, re-renders the form with an error message. The `engineer_name` field is required; it is always accepted as free-text (self-declared, not validated against loop data). This is consistent with the CLI, which accepts any engineer name on `nemo start`. The value is used solely for the `Mine` filter (FR-3e).

**FR-2c.** Unauthenticated requests to any `/dashboard/*` route (other than `/login` and `/static/*`) redirect to `/dashboard/login`.

**FR-2d.** Deployment-level security (Tailscale, VPN, or an external auth proxy like oauth2-proxy) is the primary defense. The API-key cookie is defense in depth, not the only barrier. Documented explicitly in the dashboard README: **do not expose `/dashboard` to the public internet without fronting it with auth.**

### FR-3: Card grid (mobile-first)

**FR-3a.** `/dashboard` renders a responsive card grid:
- **Mobile (< 640px)**: one column, cards full-width, tap for detail.
- **Tablet (640-1024px)**: two columns.
- **Desktop (> 1024px)**: three or four columns, adjusts on viewport.

**FR-3b.** Each card shows for one loop:
- **Header row**: state badge (colored pill — see badge color map below), loop_id short form (first 8 chars), elapsed time since `created_at` (`3m 22s` / `1h 14m`).

**Badge color map** — all 13 `LoopState` variants must have an assigned color:

| Color | States |
|---|---|
| Green | `Converged`, `Hardened`, `Shipped` |
| Red | `Failed`, `Cancelled` |
| Amber | `AwaitingApproval`, `Paused`, `AwaitingReauth` |
| Blue | `Implementing`, `Testing`, `Reviewing`, `Hardening` |
| Gray | `Pending` |
- **Title**: spec filename (`health-json-body.md`).
- **Sub-title**: branch name (muted).
- **Progress line**: `round N/M · stage: <current_stage>` for active loops; `round N` for terminal.
- **Metrics row**: tokens (`52K`), cost (`$0.18` — requires `[pricing]` config, see FR-15; displays `—` if pricing is not configured), last-round verdict (one of `clean`/`not clean`/`—`).
- **Pulse indicator**: small animated dot if `sub_state` is `Running` (i.e., the loop is in an active stage with a dispatched job currently executing), solid dot for all other states.

**FR-3c.** Clicking/tapping a card navigates (not modal — actual route change) to `/dashboard/loops/:id`.

**FR-3d.** Card grid auto-refreshes every 5s via a small vanilla-JS poll that fetches `/status` and re-renders card fields in place (no full page reload). State transitions animate with a 1s color fade on the badge.

**FR-3e.** Two rows of filter/segment chips at the top of the grid:
- **State row**: `Active (N)`, `Converged (N)`, `Failed (N)`, `All`. Tapping filters by state.
- **Engineer row**: one chip per engineer with at least one loop in the current view, plus `Mine` (default) and `Team`. `Mine` scopes to the viewer's own loops, matched by the `nautiloop_engineer` cookie value set at login (see FR-2b) against the loop's `engineer` field. `Team` flips the `/dashboard/state?team=true` query and shows all engineers. Individual-engineer chips scope to one engineer.

Chips are independent: selecting `Active` + `alice` shows Alice's active loops. Default landing state is `Active` + `Mine` — engineers see their own work first.

**FR-3f.** In team view (or any time a card belongs to a non-viewer engineer), every card displays an **engineer badge**: a small colored chip at the top-left with initials or short handle. Stable per-engineer color derived from a hash of the engineer name — alice is always the same color. Lets the CTO scan a team dashboard and see at a glance whose loop is whose without reading every label.

**FR-3g.** Auth model clarification: `team=true` is a **view filter, not a permission boundary**. The cluster has a single shared API key today; any dashboard user authenticated with that key can flip `Team` on and see every engineer's loops. Matches existing CLI behavior (`nemo status --team`). Appropriate for small, trusted teams. Per-engineer keys with an `admin` role are the path for strict RBAC — noted in Out of Scope.

### FR-4: Loop detail view

**FR-4a.** `/dashboard/loops/:id` renders a detail page:

- **Hero header**: state badge, spec filename, elapsed time, PR link (if set).
- **Action buttons** (state-gated, same logic as `nemo helm` FR-3 keybinds):
  - `Approve` if state == AWAITING_APPROVAL
  - `Cancel` if state is non-terminal (with confirm modal)
  - `Resume` if state in {PAUSED, AWAITING_REAUTH, transient FAILED}
  - `Extend +10` if state == FAILED with failed_from_state
  - `Open PR` if spec_pr_url is set (opens in new tab)
- **Rounds table**: one row per round. Columns: round number, stage (Implement/Test/Review/Revise/Harden), verdict (`clean`/`not clean`/`—`), issues count, confidence score, tokens (input + output), cost (or `—`), duration. Tapping a row expands full verdict details inline.
- **Live log pane**: last 200 lines, auto-scrolls. SSE stream via `/dashboard/stream/:id` for active loops; static dump for terminal loops.
- **Token/cost breakdown**: per-stage, per-round (bar chart optional; data table mandatory). Token data is extracted from the JSONB `output` column of each round's row. The five stage output types that may contain token data are: `ImplResultData`, `TestResultData`, `ReviewResultData`, `ReviseResultData`, and `AuditVerdict`. Each embeds a `token_usage: TokenUsage` struct with fields `input: u64` and `output: u64` (matching the `TokenUsage` struct in `control-plane/src/types/verdict.rs`). The aggregation logic must handle all five stage types and gracefully display `—` when a stage's output lacks `token_usage`. Cost is computed from token counts using the `[pricing]` config (see FR-15); if pricing is not configured, only raw token counts are shown.

**FR-4b.** The action buttons call the existing API endpoints (`POST /approve/:id`, `DELETE /cancel/:id`, etc.) via same-origin `fetch()`. The HttpOnly auth cookie is sent automatically by the browser; no Bearer header construction is needed (see FR-2a). The existing API auth middleware is extended to accept the cookie in addition to Bearer headers, so these requests authenticate transparently. Responses update the card in-place.

**FR-4c.** Layout on mobile: hero → actions (horizontal scroll if >3 buttons) → rounds table (scrollable) → log tail. On desktop: two-column layout (rounds table left, log tail right).

### FR-5: Pod-live-introspection integration

**FR-5a.** For active loops (not terminal), the detail page has an "Inspect pod" disclosure. Expanded: shows the same data as `nemo ps` / helm introspect pane (process list, CPU/mem, worktree SHA). Polls `/pod-introspect/:id` every 5s while expanded; stops when collapsed.

**FR-5b.** Collapsed by default on mobile (takes space). Displayed inline on desktop when terminal width permits.

### FR-6: Theming + dark mode

**FR-6a.** Dashboard defaults to **system color scheme** (`prefers-color-scheme` media query). Dark by default on mobile operating systems that default dark at night.

**FR-6b.** CSS uses custom properties for all colors, same palette as helm themes (dark/light/high-contrast). In v1, system `prefers-color-scheme` is the sole source of truth for theme selection — no inheritance from `[helm] theme` in nemo.toml. (The helm TUI theme is a terminal concept with no direct mapping to web CSS custom properties; bridging this is a follow-up if operators request it.)

**FR-6c.** No theme-switcher UI in v1. System-level preference is the source of truth.

### FR-7: Minimal JS behavior

**FR-7a.** Single `dashboard.js` file. Responsibilities:
- Card grid poll + diff re-render (no full reload)
- Action button fetch wiring
- SSE log stream subscription
- Tab title shows unread state: `(2) nautiloop` when 2 loops have converged since the last focus.
- Play a `\a` bell equivalent via web audio when a loop converges (opt-in, off by default).

**FR-7b.** No external JS dependencies. No bundler. Plain ES2022 modules. One file, <10 KB minified.

**FR-7c.** Graceful degradation: if JS fails to load, the page is still navigable (links work, forms POST cleanly). The polling is a progressive enhancement.

### FR-8: Server-side endpoints for dashboard needs

**FR-8a.** Reuses existing endpoints: `/status`, `/inspect?branch=<>`, `/logs/:id`, `/pod-introspect/:id`, `/approve/:id`, `/cancel/:id`, `/resume/:id`, `/extend/:id`.

**FR-8b.** One new JSON endpoint: `GET /dashboard/state` returns a single roll-up object combining **all active loops and recently-terminal loops** (terminal within the last 24 hours), plus aggregates (total tokens, total cost, counts per state). Accepts optional query parameters: `?include_terminal=all` to include all terminal loops regardless of age, `?team=true` to include all engineers' loops (default: scoped to the requesting engineer via cookie). This ensures that the card grid's filter chips (FR-3e: `Converged`, `Failed`) have data to display after a poll refresh. Lets the card grid refresh in one request instead of N+1 (one `/status` + N `/inspect`). Polled every 5s.

**Response schema for `/dashboard/state`:**

```json
{
  "loops": [
    {
      "id": "uuid",
      "spec_path": "specs/foo.md",
      "branch": "agent/alice/foo-a1b2c3d4",
      "engineer": "alice",
      "state": "Implementing",
      "sub_state": "Running",
      "round": 3,
      "max_rounds": 15,
      "current_stage": "implement",
      "created_at": "2026-04-18T12:00:00Z",
      "updated_at": "2026-04-18T12:05:00Z",
      "spec_pr_url": "https://github.com/org/repo/pull/147",
      "failed_from_state": null,
      "last_verdict": "not clean",
      "total_tokens": { "input": 42000, "output": 10000 },
      "total_cost": 0.18
    }
  ],
  "aggregates": {
    "counts_by_state": {
      "Implementing": 2,
      "Converged": 5,
      "Failed": 1
    },
    "total_tokens": { "input": 500000, "output": 120000 },
    "total_cost": 12.40,
    "total_loops": 8
  },
  "fleet_summary": {
    "window_days": 7,
    "total_loops": 47,
    "total_cost": 12.40,
    "converge_rate": 0.82,
    "avg_rounds": 4.2,
    "top_spender": { "engineer": "alice", "cost": 4.80 },
    "trends": {
      "converge_rate_delta": 0.08,
      "avg_rounds_delta": -0.3
    }
  },
  "engineers": ["alice", "bob", "dev"]
}
```

Fields with `total_cost` or per-loop `total_cost` are `null` when `[pricing]` is not configured. `trends` is `null` when insufficient historical data exists (first week).

**Response schema for `/dashboard/feed`:**

```json
{
  "events": [
    {
      "id": "uuid",
      "spec_path": "specs/foo.md",
      "engineer": "alice",
      "state": "Converged",
      "rounds": 2,
      "total_tokens": { "input": 42000, "output": 10000 },
      "total_cost": 0.18,
      "spec_pr_url": "https://github.com/org/repo/pull/147",
      "updated_at": "2026-04-18T15:47:00Z",
      "extensions": 0
    }
  ],
  "has_more": true
}
```

**Response schema for `/dashboard/specs`:**

```json
{
  "spec_path": "specs/foo.md",
  "runs": [
    {
      "id": "uuid",
      "engineer": "alice",
      "state": "Converged",
      "rounds": 2,
      "total_cost": 0.18,
      "branch": "agent/alice/foo-a1b2c3d4",
      "created_at": "2026-04-18T15:47:00Z"
    }
  ],
  "aggregates": {
    "total_runs": 3,
    "converge_rate": 0.67,
    "avg_rounds": 10.7,
    "total_cost": 4.52
  }
}
```

**Response schema for `/dashboard/stats`:**

```json
{
  "window": "7d",
  "headline": {
    "total_loops": 47,
    "total_cost": 12.40,
    "converge_rate": 0.82,
    "avg_rounds": 4.2
  },
  "per_engineer": [
    { "engineer": "alice", "loops": 20, "cost": 4.80, "converge_rate": 0.90 }
  ],
  "per_spec": [
    { "spec_path": "specs/foo.md", "runs": 5, "cost": 2.10, "converge_rate": 0.80, "avg_rounds": 3.5 }
  ],
  "time_series": [
    { "date": "2026-04-18", "started": 8, "converged": 5, "failed": 1 }
  ]
}
```

**FR-8c.** `/dashboard/state` is auth-protected same as other `/dashboard/*` routes.

### FR-9: CTO kit — fleet summary header

**FR-9a.** Top of the dashboard (above the card grid) renders a single-line **fleet summary**, always visible:

```
This week · 47 loops · $12.40 · 82% converged · avg 4.2 rounds · top: alice ($4.80)
```

Aggregates all non-terminal + terminal loops in the rolling 7-day window. Fields:
- `N loops` — total loops created in window
- `$X.XX` — total token cost (summed from per-round `token_usage` × `[pricing]`)
- `X% converged` — (CONVERGED + HARDENED + SHIPPED) / total_terminal. Excludes still-active loops from the denominator.
- `avg N.N rounds` — mean rounds-to-termination across terminal loops
- `top: <engineer> ($X.XX)` — highest-spender this window

**FR-9b.** A subtle trend indicator after each numeric field when a prior-period comparison is available: `82% ↑8% converged` means 8 percentage points higher than the prior 7-day window. First week of data has no trend; don't render the arrow.

**FR-9c.** Tapping any field of the summary navigates to `/dashboard/stats` (FR-14) with that field pre-focused. Pre-focused means: the URL includes an anchor fragment (e.g., `/dashboard/stats#converge-rate`), the page scrolls to the corresponding section, and a brief highlight animation (CSS `outline` pulse, 2s) draws attention to the relevant headline card or table.

### FR-10: Kill switch

**FR-10a.** A hidden-by-default `⋯` menu in the header has a destructive `Cancel all active loops` item. Tapping opens a modal: `Cancel N active loops? This cannot be undone.` Two buttons: `Cancel` (aborts) / `Confirm cancel all` (proceeds).

**FR-10b.** On confirm, the dashboard issues `DELETE /cancel/:id` for every active (non-terminal) loop in parallel. Reports `Cancelled N/M loops` via a dismissible toast notification at the top of the page (auto-dismisses after 10s); failures (rare — usually a race with a loop terminating naturally) are listed inline within the toast with their reasons.

**FR-10c.** Rationale: the dashboard exists precisely because the CTO is AFK. If cost is spiking, credentials are suspected compromised, or a bug is producing runaway loops, the kill switch needs to be one tap from the phone. No new server endpoint; the dashboard fans out to the existing per-loop cancel.

**FR-10d.** The kill switch is gated behind the `team` toggle (only visible when viewing team mode, NOT in `Mine`). Prevents muscle-memory mis-taps when browsing one's own loops.

### FR-11: Judge reasoning pane

**FR-11a.** Once the orchestrator-judge feature (#128) is implemented and `judge_decisions` rows exist, the loop detail view surfaces them inline:

- In the rounds table (FR-4a), any round where the judge fired gets a small gavel icon (`⚖`) in the Verdict column.
- Tapping the icon opens a detail drawer showing: `decision`, `confidence`, `reasoning` (human-readable), and (if set) the `hint` that was injected into the next round's feedback.
- A dedicated `/dashboard/loops/:id/judge` tab (or accordion) shows the full sequence of judge decisions for the loop end-to-end.

**FR-11b.** If `judge_decisions` is empty or the feature isn't yet shipped, the gavel icon + tab simply aren't rendered. No error, no placeholder — the UI degrades silently, so this spec can land before or after #128 without a sequencing constraint.

**FR-11c.** Critical for CTO trust: when the judge overrides a reviewer's "not clean" with `exit_clean`, the CTO should be able to tap through to the reasoning in two taps. Opaque ML decisions are the fastest way to lose stakeholder confidence in an autonomous loop.

### FR-12: Notification feed

**FR-12a.** New route `/dashboard/feed` renders a chronological, reverse-time list of terminal events across all loops:

```
15:47 · alice · health-json-body      CONVERGED  PR #147  2 rounds  $0.18
14:23 · bob   · schema-migration      FAILED     max rounds  15 rounds  $1.24
13:05 · alice · helm-tui-phase2       CONVERGED  PR #145  4 rounds  $0.42
12:40 · dev   · orchestrator-judge-v2 CONVERGED  PR #144  15 rounds  $3.10 [extended ×1]
```

Each row is tappable → navigates to the loop detail page.

**FR-12b.** Filters at the top: `All events` / `Converged only` / `Failed only` / per-engineer. Persists selection in localStorage across visits.

**FR-12c.** Pagination: loads the 50 most recent; `Load more` button at the bottom fetches the next 50. Backed by a new endpoint `GET /dashboard/feed?cursor=<timestamp>&limit=50` that returns terminal loops ordered by `updated_at DESC`.

**FR-12d.** This is the CTO's morning-coffee surface: skim, see what shipped overnight, note what failed, close the tab. Must load in under a second on a mobile connection.

### FR-13: Per-spec history

**FR-13a.** In the loop detail view, the spec filename is a link. Tapping navigates to `/dashboard/specs?path=<url-encoded-path>` (query-parameter form, consistent with the backing endpoint in FR-13b, avoids multi-segment path routing issues in axum). Shows all past loops that ran on that spec:

| Date | Engineer | Result | Rounds | Cost | Branch |
|---|---|---|---|---|---|
| 2026-04-18 15:47 | alice | CONVERGED | 2 | $0.18 | agent/alice/health-json-body-a1b2c3d4 |
| 2026-04-18 11:05 | dev | FAILED | 15 | $1.24 | agent/dev/health-json-body-e5f6g7h8 |
| 2026-04-17 22:21 | dev | CONVERGED | 15 | $3.10 | agent/dev/health-json-body-db6530fc |

Plus aggregates at the top: `3 runs · 67% converge rate · avg 10.7 rounds · total cost $4.52`.

**FR-13b.** Backed by a new endpoint `GET /dashboard/specs?path=<>&limit=50` returning loops filtered by `spec_path`. Existing schema already has `spec_path`, no migration.

**FR-13c.** Use case: a spec that fails 3x in a row or consistently takes 15 rounds is a spec-quality problem, not an implementor problem. Visibility surfaces this. Helps engineers tighten specs before submitting again.

### FR-14: Stats deep-dive page

**FR-14a.** `/dashboard/stats` — a single page with the expanded view of the FR-9 summary:
- **Headline cards**: total loops, total cost, converge rate, avg rounds — for the current window (7d default, toggleable: 24h / 7d / 30d).
- **Per-engineer table**: engineer, loops, cost, converge rate.
- **Per-spec table**: top 10 most-run specs with their aggregate metrics.
- **Time series**: daily count of loops started vs terminal outcomes, rendered as simple CSS-width bars (no chart library — consistent with FR-7b).

**FR-14b.** Backed by a single aggregation endpoint `GET /dashboard/stats?window=7d` returning a structured JSON that the template consumes. No new DB migrations; all data derivable from existing `loops` + `rounds` tables.

**FR-14c.** Cache-friendly: the aggregation is expensive relative to the rest of the dashboard, so the endpoint caches results for 60s server-side (the time-window granularity makes this trivially safe).

### FR-15: Pricing configuration

**FR-15a.** Token-to-cost conversion requires a `[pricing]` section in `nemo.toml`:

```toml
[pricing]
# Per-token costs in USD. Supports multiple models since model_implementor
# and model_reviewer may differ.
[pricing.models]
"claude-sonnet-4-20250514" = { input_per_million = 3.00, output_per_million = 15.00 }
"claude-opus-4-20250514"  = { input_per_million = 15.00, output_per_million = 75.00 }
```

**FR-15b.** A helper function `compute_cost(token_usage: &TokenUsage, model: &str, pricing: &PricingConfig) -> Option<f64>` lives in the dashboard aggregate module. Returns `None` if the model is not found in the pricing config (graceful degradation — no panic, no default guess).

**FR-15c.** When `[pricing]` is absent from `nemo.toml`, **all cost fields across the dashboard display `—` instead of a dollar amount**. Token counts (raw numbers) are always shown regardless of pricing config. This applies to: FR-3b metrics row, FR-4a token/cost breakdown, FR-9a fleet summary (cost and top-spender fields omitted), FR-12a feed rows, FR-13a history table, FR-14a headline cards and per-engineer/per-spec tables.

**FR-15d.** The pricing config is loaded once at startup and cached in the app state. No hot-reload required in v1; restart the control plane to pick up pricing changes.

### FR-16: JS feature tiers

**FR-16a.** Dashboard JS features are classified into two tiers to guide implementation priority:

| Tier | Features | Required for v1 |
|---|---|---|
| **Mandatory** | Card grid polling + diff re-render, action button fetch wiring, SSE log stream subscription, filter chip state, confirm modals | Yes |
| **Progressive enhancement** | Tab title badge (`(2) nautiloop`), web audio bell on convergence, 1s color fade animation on state transitions, localStorage persistence for feed filters | No — implement if budget allows, skip without blocking launch |

**FR-16b.** All progressive-enhancement features degrade silently: if the JS for them fails or is stripped, the mandatory features continue working. No feature in the progressive tier may block a mandatory feature.

### NFR-1: Security inherits from deployment

The dashboard is as private as the host. Documented as such. Tailscale-on-Hetzner deployments are private by default; other deployments can front with oauth2-proxy, Authelia, or similar. **Do NOT add sign-in-with-Google or a user database** — that's explicitly out of scope.

### NFR-2: Performance

Dashboard HTML generation: <50ms per request. `/dashboard/state` endpoint: <200ms for up to 50 active loops. Page weight (HTML + CSS + JS + assets): <100 KB uncompressed, <30 KB gzipped.

### NFR-3: Mobile-first testing

Every view MUST render correctly on iPhone SE viewport (375×667). Verified via the operator opening the URL on their phone during implementation; not a formal automated test requirement.

### NFR-4: Zero new crate dependencies at the HTTP layer

axum + tower already provide routing + middleware. Templating via `askama` or `maud` (one new dep, acceptable). No htmx/hyperscript/alpine.js — those are fine but every dependency has a maintenance cost. If reactivity pain motivates htmx later, it's a small follow-up; don't pre-empt it.

### NFR-5: No impact on CLI or existing API consumers

Dashboard routes live under `/dashboard/*`. All existing `/status`, `/inspect`, etc. endpoints keep exact behavior. No shared state mutation.

### NFR-6: Tests

- **Unit** (`control-plane/src/api/dashboard/handlers.rs`): each handler renders valid HTML given a mock state store; login validates API key correctly; rejected keys re-render with error.
- **Integration** (`control-plane/tests/dashboard_integration.rs`): end-to-end login → card grid → detail page → action button → verify API side effect.
- **Manual**: the operator opens the dashboard on their actual phone over Tailscale before the spec lands.

## Acceptance Criteria

A reviewer can verify by:

1. On a deployed nautiloop, browse to `https://<ts-ipv4>/dashboard` from a phone on the same tailnet. Login form prompts for API key and engineer name.
2. Enter the API key and engineer name. Land on the card grid. `Mine` filter is active, showing only the logged-in engineer's loops. At least one loop card is visible with live state.
3. Tap a card. Detail page loads with hero / actions / rounds table / log tail.
4. On an active loop: live log lines stream in. On a terminal loop: full log dump, no streaming.
5. Tap Approve on an AWAITING_APPROVAL loop. Response flashes success; card state updates.
6. Tap a FAILED-max-rounds loop's Extend button. Confirm modal → +10 rounds → loop resumes. Card updates.
7. Leave dashboard open, switch apps, come back. Polling resumed, state is current.
8. Toggle device to dark mode. Dashboard flips palette.
9. Curl `/dashboard` with no cookie → 302 to login. With a bad cookie → login with error. With a good cookie → 200 HTML.
10. Fleet summary (FR-9) at top shows a single-line this-week roll-up; numbers reconcile with manually-computed aggregates from `/inspect` on each loop in window.
11. `⋯` menu → `Cancel all active loops` (FR-10). Modal confirms; on proceed, N cancel requests fire and complete within 10s. Hidden in `Mine` mode.
12. For any loop where the judge fired (post-#128), rounds table shows ⚖ icon on those rows (FR-11). Tapping opens reasoning drawer.
13. `/dashboard/feed` (FR-12) shows chronological terminal events with engineer + outcome + cost. Filter chips work. `Load more` paginates.
14. Tap a spec filename on any loop detail page → `/dashboard/specs?path=<spec>` (FR-13). Shows all past runs + aggregate metrics.
15. `/dashboard/stats?window=30d` (FR-14) renders per-engineer + per-spec tables + daily time-series bars. Subsequent loads within 60s hit the cache (observe response time).
16. With `[pricing]` configured in `nemo.toml`, cost figures appear as dollar amounts throughout the dashboard. Without `[pricing]`, all cost fields show `—` and token counts are still displayed (FR-15).

## Out of Scope

- **Per-engineer API keys + role-based access.** v1 uses the single shared cluster API key; `Mine` vs `Team` is a view filter on top of full visibility. Strict RBAC (engineer A truly cannot see engineer B's loops, admin can see everyone) requires per-engineer keys with claims + handler-level enforcement; punted to a follow-up spec once a real team demands it.
- **User accounts with password / SSO / SAML.** Not in v1. Tailscale + shared API key is the model.
- **Push notifications** (web push, SMS, Slack). Future spec; dashboard v1 is a pull-to-refresh world.
- **Spec editor / upload via web.** Specs live in the repo; the dashboard is observation + light actions.
<!-- Historical analytics now IN scope via FR-14 stats deep-dive. -->
- **Anomaly detection / alerting** (auto-page on-call when cost spikes or convergence rate drops). Out of v1; FR-12 feed covers the manual-pull version.
- **WebSocket upgrade for general state** (beyond the existing SSE for logs). Polling is sufficient and simpler.
- **Fancy charts / D3 / visualization libraries.** One optional bar chart (token/cost breakdown), rendered as plain `<div>` bars with CSS widths. No chart library.
- **Offline support / PWA / service worker.** Maybe later; not required.
- **Cross-deployment federation** (one dashboard, many nautiloop servers). Each deploy has its own dashboard.
- **API documentation browser.** OpenAPI + a separate page is a follow-up if engineers ask for it.

## Files Likely Touched

- `control-plane/src/api/mod.rs` — route wiring under `/dashboard/*`.
- `control-plane/src/api/dashboard/mod.rs` — new module.
- `control-plane/src/api/dashboard/handlers.rs` — per-route handlers.
- `control-plane/src/api/dashboard/auth.rs` — cookie middleware.
- `control-plane/src/api/dashboard/aggregate.rs` — `/dashboard/state` roll-up + FR-9 fleet summary + FR-14 stats aggregator (with 60s cache) + FR-15 pricing helper (`compute_cost`).
- `control-plane/src/api/dashboard/feed.rs` — FR-12 notification feed endpoint.
- `control-plane/src/api/dashboard/specs.rs` — FR-13 per-spec history endpoint.
- `control-plane/src/api/dashboard/kill_switch.rs` — FR-10 fan-out cancel helper.
- `control-plane/src/api/dashboard/templates/` — new dir, Askama `.html` templates (or `maud!{}` macros in `.rs` files).
- `control-plane/assets/dashboard.css` — embedded via `include_str!`.
- `control-plane/assets/dashboard.js` — embedded via `include_str!`.
- `control-plane/Cargo.toml` — add `askama` (use latest compatible 0.12.x or newer) and `askama_axum` for response helpers. Or `maud` if that was chosen per FR-1c.
- Tests per NFR-6.
- `docs/dashboard-setup.md` — new doc covering the security model (Tailscale as default, explicit warning about public exposure).

## Baseline Branch

`main` at PR #144 merge.
