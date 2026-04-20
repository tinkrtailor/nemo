# Dashboard Contrast + Theme Toggle

## Overview

Two focused polish items on the mobile dashboard shipped in PR #166:
1. Increase foreground/background contrast to match the helm TUI's post-phase-2 palette. The dashboard is usable but noticeably washed out relative to helm and to operator expectations.
2. Add an explicit theme toggle (dark / light / system-auto) in the settings `⋯` menu so the user can override `prefers-color-scheme` without changing OS preferences.

Both are small, neither adds new endpoints, both inherit the existing CSS-custom-property structure the dashboard already uses.

## Baseline

Main at PR #166 merge (mobile dashboard). Current state:

- `control-plane/assets/dashboard.css` uses CSS custom properties for all colors: `--bg`, `--surface`, `--border`, `--text`, `--muted`, `--teal`, `--amber`, `--green`, `--red`, `--blue`.
- Palette follows the helm TUI's phase-1 values (pre-phase-2).
- Theme is chosen by `@media (prefers-color-scheme: dark)` with a default light. No manual override.
- Settings `⋯` menu in the header exists (shown in the recent screenshot) but currently only holds the kill-switch item.

Observed: text-on-surface contrast ratio on dark mode is ~7:1 but muted-text-on-surface is ~3.2:1 — readable-but-washed compared to helm's tightened palette. The "Loops" heading and filter chips in the dashboard look dimmer than comparable helm UI elements, on the same monitor, at the same time.

## Problem Statement

### Problem 1: Dashboard feels washed out vs helm

Operators switching between `nemo helm` (TUI) and the dashboard web view see a contrast drop. Helm phase 2 tightened its palette (text `#E8E6E3`, muted `#8A8784`, etc.). The dashboard kept older values with lower effective contrast, so reading quickly on a phone in bright conditions is harder than reading helm on a desktop terminal with the same color names.

Concrete impact: fleet summary line (`This week · 4 loops · $1.98 · 50% converged ...`) renders in `--muted` against `--bg`, and the muted value is too close to bg to scan quickly outdoors.

### Problem 2: No manual theme control

`prefers-color-scheme` is fine for most users but not all:
- Mobile OS set to dark-at-night auto-switching — user wants dashboard consistent across the day, not flipping with sunset.
- Desktop user with OS light theme but prefers dark for long-running monitoring surfaces.
- Accessibility users who need high-contrast regardless of system setting.

Today they have no way to override without changing OS preferences.

## Functional Requirements

### FR-1: Contrast increase

**FR-1a.** Adopt the helm phase-2 palette values in `control-plane/assets/dashboard.css`. Replace each `--` custom property's value with the helm equivalent:

| Var | Current value | New value (helm-aligned) |
|---|---|---|
| `--bg` (dark) | (current) | `#0F0F0E` |
| `--surface` (dark) | (current) | `#1A1918` |
| `--border` (dark) | (current) | `#2E2D2B` |
| `--text` (dark) | (current) | `#E8E6E3` |
| `--muted` (dark) | (current) | `#A29F9B` |
| `--teal` | (current) | `#1B6B5A` |
| `--amber` | (current) | `#E8A838` |
| `--green` | (current) | `#2D7A4F` |
| `--red` | (current) | `#C4392D` |
| `--blue` | (current) | `#3B7BC0` |

The implementor SHOULD verify the helm source (`cli/src/commands/helm.rs`) for the exact hex values if the TUI's constants moved during phase 2; those are the source of truth.

**FR-1b.** `--muted` on dark backgrounds MUST meet WCAG AA 4.5:1 contrast minimum against `--bg`. If the helm value above doesn't hit that, bump `--muted` to `#B8B5B2` (a touch brighter). Verify with a contrast checker.

**FR-1c.** Light-mode palette (currently also defined via `prefers-color-scheme: light`) gets a corresponding pass: text ~13:1 against bg, muted ~4.5:1. Reuse helm's light theme if it exists; otherwise:

| Var | Light value |
|---|---|
| `--bg` | `#FBFAF8` |
| `--surface` | `#F2F0EC` |
| `--border` | `#D4D1CC` |
| `--text` | `#1C1A17` |
| `--muted` | `#4A4844` |

### FR-2: Theme toggle in settings menu

**FR-2a.** The existing header settings `⋯` menu gains three new items above the existing `Cancel all active loops` kill switch:

```
Theme
  · System (default)
  · Dark
  · Light
─────────
Cancel all active loops
```

Visual style: three radio-button-like items, the currently-active one has a checkmark or filled dot prefix. Tapping another item switches theme and updates the checkmark. Menu closes automatically.

**FR-2b.** The user's selection is persisted client-side in `localStorage` under key `nautiloop_theme` with values `"system"`, `"dark"`, or `"light"`. Default (first visit or cleared storage) is `"system"`.

**FR-2c.** Theme resolution at page load:
- Read `localStorage.nautiloop_theme`.
- If `"dark"` or `"light"`: set `<html data-theme="dark|light">` before CSS parses to prevent a flash.
- If `"system"` or missing: leave `data-theme` unset; let `prefers-color-scheme` media queries apply.

**FR-2d.** CSS structure: `:root { --bg: ...light... }` for the default/light base, `@media (prefers-color-scheme: dark) { :root { --bg: ...dark... } }` for system-auto-dark, and `[data-theme="dark"] { --bg: ...dark... }` / `[data-theme="light"] { --bg: ...light... }` overrides that win over media queries. Dark values are specified once and reused across both the media query and the `data-theme="dark"` selector (CSS variables or a `:where()` grouping).

**FR-2e.** The toggle's JavaScript lives in the embedded `dashboard.js`. ~30 lines: click handler sets `data-theme` attribute, writes localStorage, updates the menu's active-item indicator. No theme-switching library.

**FR-2f.** The pre-parse flash prevention: a 5-line inline `<script>` in the HTML `<head>` reads localStorage and sets `data-theme` before body renders. Must ship before any CSS or other JS runs.

### FR-3: Dashboard status-line + card contrast tune-up

**FR-3a.** The fleet summary line in the header currently uses `color: var(--muted)`. Change to `color: var(--text)` with `opacity: 0.85` OR to a new custom property `--text-secondary` defined as `#CFC9C3` in dark / `#2E2C28` in light. Rationale: the line is primary always-visible data, not decorative — muted is too recessed.

**FR-3b.** Card metadata rows (engineer badge, elapsed time, token count) use `--muted` today; leave unchanged — they're genuinely secondary.

**FR-3c.** Convergence-badge colors on cards (`--green` for CONVERGED, `--red` for FAILED) should pass 4.5:1 against both `--surface` variants. Verify the helm-aligned greens/reds meet this. If the new `--green` is too dark to pass contrast on dark surface, use `#3A9A65` instead.

## Non-Functional Requirements

### NFR-1: No new external dependencies

No CSS framework, no theme library, no new JS runtime deps. Vanilla CSS custom properties + a tiny JS handler.

### NFR-2: Backward compatibility

- URLs unchanged
- Existing cookies unchanged
- Existing endpoints unchanged
- A user with a pre-spec browser tab open will see the new colors on next refresh; no server-side coordination.

### NFR-3: Theme preference is per-browser, not per-user

localStorage is scoped to the origin on the specific device. Users with the dashboard open on a phone + laptop get independent theme choices. This is the correct scoping (matches how most dashboards handle it) and avoids needing a new server field.

### NFR-4: Accessibility

New palette passes WCAG AA (4.5:1 for normal text, 3:1 for large text + UI components). Verify with `axe` or a contrast-checker on the rendered page; document the verified ratios in the spec's sign-off PR description.

### NFR-5: Tests

- **Unit** (assets test, if present): snapshot the color values after change.
- **Manual**: screenshot dashboard in each of dark/light/system on a phone-sized viewport. Eyeball the contrast improvement.
- **Lighthouse accessibility audit**: score >= 90 for the dashboard route (currently ~85 per FR-1b rationale).

## Acceptance Criteria

A reviewer can verify by:

1. **Contrast check**: open dashboard, compare header fleet-summary readability to `nemo helm` header. Side by side, they should feel the same weight. No more washed-out look.
2. **Manual theme toggle**: open `⋯` menu → see three theme options → pick `Light` → page immediately renders light → refresh page → still light (localStorage persistence).
3. **System fallback**: pick `System` → page tracks OS preference. Toggle OS dark mode → dashboard follows.
4. **No flash**: refresh page with `Dark` selected on an OS set to light mode. No light-to-dark flash at page-load; theme applied before first paint.
5. **Helm consistency**: open `nemo helm` in one window, dashboard in another. Colors match.
6. **Lighthouse**: accessibility score ≥ 90 for `/dashboard` route.

## Out of Scope

- **High-contrast theme** (3rd option beyond dark/light). Helm has one; dashboard gets `dark` and `light` only for v0.6.1. A separate spec can add it.
- **Per-user theme persistence** on the server side. Local-only.
- **Custom theme authoring**. Built-in palettes only.
- **Color-blind-mode palettes**. Revisit if requested.
- **Transitions / fade animations on theme switch**. Instant switch is fine.
- **Keyboard shortcut for theme switch**. Menu item only.
- **Syncing helm theme with dashboard theme via nemo.toml**. They're intentionally independent; helm is terminal, dashboard is browser.

## Files Likely Touched

- `control-plane/assets/dashboard.css` — color values for dark/light + new `[data-theme]` selectors (FR-1, FR-2d).
- `control-plane/assets/dashboard.js` — theme toggle click handler + localStorage (FR-2e).
- `control-plane/src/api/dashboard/templates.rs` (or wherever the HTML is rendered) — inline `<script>` for flash prevention (FR-2f), new menu items in header `⋯` (FR-2a).
- Tests / screenshots per NFR-5.

## Baseline Branch

`main` at PR #170 merge.
