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
| `/dashboard/stream/:id` | GET (SSE) | Existing `/logs/:id` SSE, re-exposed under dashboard namespace |
| `/dashboard/static/*` | GET | Embedded CSS + JS assets |

**FR-1b.** Static assets (CSS, single JS file, optional icon/font) are **embedded in the binary** via `include_str!`/`include_bytes!`. No filesystem dependencies at runtime. Total asset budget: <50 KB gzipped.

**FR-1c.** All dashboard HTML is server-rendered (axum handler returns `Html<String>`). Templating via `askama` (compile-time, already a family of the rust ecosystem) or `maud` — pick one, stick with it. No SPA framework. No React/Vue/Svelte.

### FR-2: Auth

**FR-2a.** The dashboard requires the same API key used by `nemo` CLI. Two acceptance paths:
- **Cookie** `nautiloop_api_key=<key>` (HttpOnly, Secure, SameSite=Strict). Set by `/dashboard/login` form POST. Expires in 7 days.
- **Bearer header** for API endpoints called from the dashboard JS (same as current CLI → API auth).

**FR-2b.** `/dashboard/login` renders a trivial form: one input (`api_key`), one submit. On POST, validates the key by making an internal `/status` call; if 200, sets the cookie and redirects to `/dashboard`. If invalid, re-renders with an error.

**FR-2c.** Unauthenticated requests to any `/dashboard/*` route (other than `/login` and `/static/*`) redirect to `/dashboard/login`.

**FR-2d.** Deployment-level security (Tailscale, VPN, or an external auth proxy like oauth2-proxy) is the primary defense. The API-key cookie is defense in depth, not the only barrier. Documented explicitly in the dashboard README: **do not expose `/dashboard` to the public internet without fronting it with auth.**

### FR-3: Card grid (mobile-first)

**FR-3a.** `/dashboard` renders a responsive card grid:
- **Mobile (< 640px)**: one column, cards full-width, tap for detail.
- **Tablet (640-1024px)**: two columns.
- **Desktop (> 1024px)**: three or four columns, adjusts on viewport.

**FR-3b.** Each card shows for one loop:
- **Header row**: state badge (colored pill: IMPLEMENTING/REVIEWING/CONVERGED/FAILED/etc.), loop_id short form (first 8 chars), elapsed time since `created_at` (`3m 22s` / `1h 14m`).
- **Title**: spec filename (`health-json-body.md`).
- **Sub-title**: branch name (muted).
- **Progress line**: `round N/M · stage: <current_stage>` for active loops; `round N` for terminal.
- **Metrics row**: tokens (`52K`), cost (`$0.18`), last-round verdict (one of `clean`/`not clean`/`—`).
- **Pulse indicator**: small animated dot if state is RUNNING, solid otherwise.

**FR-3c.** Clicking/tapping a card navigates (not modal — actual route change) to `/dashboard/loops/:id`.

**FR-3d.** Card grid auto-refreshes every 5s via a small vanilla-JS poll that fetches `/status` and re-renders card fields in place (no full page reload). State transitions animate with a 1s color fade on the badge.

**FR-3e.** Two rows of filter/segment chips at the top of the grid:
- **State row**: `Active (N)`, `Converged (N)`, `Failed (N)`, `All`. Tapping filters by state.
- **Engineer row**: one chip per engineer with at least one loop in the current view, plus `Mine` (default) and `Team`. `Mine` scopes to the viewer's own loops (derived from API-key → engineer mapping). `Team` flips the `/dashboard/state?team=true` query and shows all engineers. Individual-engineer chips scope to one engineer.

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
- **Rounds table** (mirrors FR-9 of the helm TUI phase 2 spec): one row per round, columns for stages/verdict/issues/confidence/tokens/cost/duration. Tapping a row expands full verdict details inline.
- **Live log pane**: last 200 lines, auto-scrolls. SSE stream via `/dashboard/stream/:id` for active loops; static dump for terminal loops.
- **Token/cost breakdown**: per-stage, per-round (bar chart optional; data table mandatory).

**FR-4b.** The action buttons call the existing API endpoints (`POST /approve/:id`, `DELETE /cancel/:id`, etc.) via fetch with the bearer header derived from the auth cookie. Responses update the card in-place.

**FR-4c.** Layout on mobile: hero → actions (horizontal scroll if >3 buttons) → rounds table (scrollable) → log tail. On desktop: two-column layout (rounds table left, log tail right).

### FR-5: Pod-live-introspection integration

**FR-5a.** For active loops (not terminal), the detail page has an "Inspect pod" disclosure. Expanded: shows the same data as `nemo ps` / helm introspect pane (process list, CPU/mem, worktree SHA). Polls `/pod-introspect/:id` every 5s while expanded; stops when collapsed.

**FR-5b.** Collapsed by default on mobile (takes space). Displayed inline on desktop when terminal width permits.

### FR-6: Theming + dark mode

**FR-6a.** Dashboard defaults to **system color scheme** (`prefers-color-scheme` media query). Dark by default on mobile operating systems that default dark at night.

**FR-6b.** CSS uses custom properties for all colors, same palette as helm themes (dark/light/high-contrast). Inherits from `[helm] theme` in nemo.toml if set.

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

**FR-8b.** One new JSON endpoint: `GET /dashboard/state` returns a single roll-up object combining all active loops plus aggregates (total tokens, total cost, counts per state). Lets the card grid refresh in one request instead of N+1 (one `/status` + N `/inspect`). Polled every 5s.

**FR-8c.** `/dashboard/state` is auth-protected same as other `/dashboard/*` routes.

## Non-Functional Requirements

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

1. On a deployed nautiloop, browse to `https://<ts-ipv4>/dashboard` from a phone on the same tailnet. Login form prompts for API key.
2. Enter the API key. Land on the card grid. At least one loop card is visible with live state.
3. Tap a card. Detail page loads with hero / actions / rounds table / log tail.
4. On an active loop: live log lines stream in. On a terminal loop: full log dump, no streaming.
5. Tap Approve on an AWAITING_APPROVAL loop. Response flashes success; card state updates.
6. Tap a FAILED-max-rounds loop's Extend button. Confirm modal → +10 rounds → loop resumes. Card updates.
7. Leave dashboard open, switch apps, come back. Polling resumed, state is current.
8. Toggle device to dark mode. Dashboard flips palette.
9. Curl `/dashboard` with no cookie → 302 to login. With a bad cookie → login with error. With a good cookie → 200 HTML.

## Out of Scope

- **Per-engineer API keys + role-based access.** v1 uses the single shared cluster API key; `Mine` vs `Team` is a view filter on top of full visibility. Strict RBAC (engineer A truly cannot see engineer B's loops, admin can see everyone) requires per-engineer keys with claims + handler-level enforcement; punted to a follow-up spec once a real team demands it.
- **User accounts with password / SSO / SAML.** Not in v1. Tailscale + shared API key is the model.
- **Push notifications** (web push, SMS, Slack). Future spec; dashboard v1 is a pull-to-refresh world.
- **Spec editor / upload via web.** Specs live in the repo; the dashboard is observation + light actions.
- **Historical analytics** (convergence over time, cost per engineer per week). Separate spec once the data is accumulated.
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
- `control-plane/src/api/dashboard/aggregate.rs` — `/dashboard/state` roll-up builder.
- `control-plane/src/api/dashboard/templates/` — new dir, Askama `.html` templates (or `maud!{}` macros in `.rs` files).
- `control-plane/assets/dashboard.css` — embedded via `include_str!`.
- `control-plane/assets/dashboard.js` — embedded via `include_str!`.
- `control-plane/Cargo.toml` — add `askama = "0.12"` (or `maud`) and `askama_axum` for response helpers.
- Tests per NFR-6.
- `docs/dashboard-setup.md` — new doc covering the security model (Tailscale as default, explicit warning about public exposure).

## Baseline Branch

`main` at PR #144 merge.
