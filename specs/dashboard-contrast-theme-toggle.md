# Dashboard Contrast + Theme Toggle

## Overview

Two focused polish items on the mobile dashboard shipped in PR #166:
1. Increase foreground/background contrast — particularly for secondary text — to improve readability. The dashboard's `--text-secondary` value matches helm's `muted` exactly (`#8A8784`), but at ~4.2:1 against `--bg: #0F0F0E` it falls below WCAG AA 4.5:1 for normal text, making the fleet summary line and filter chips feel washed out compared to helm (where muted text appears against a terminal background with different rendering characteristics).
2. Add an explicit theme toggle (dark / light / system-auto) in the settings `⋯` menu so the user can override `prefers-color-scheme` without changing OS preferences.

Both are small, neither adds new endpoints, both inherit the existing CSS-custom-property structure the dashboard already uses.

## Baseline

Main at HEAD after PR #171 merge (the spec docs commit for this work). Current state:

- `control-plane/assets/dashboard.css` uses CSS custom properties for all colors. The actual variable names are:
  - Layout: `--bg`, `--surface`, `--surface-raised`, `--border`
  - Text: `--text-primary`, `--text-secondary`, `--text-tertiary`
  - Brand: `--primary`, `--primary-hover`, `--primary-muted`, `--accent`, `--accent-muted`
  - Semantic: `--success`, `--warning`, `--error`, `--info`
  - Spacing: `--sp-xs`, `--sp-sm`, `--sp-md`, `--sp-lg`, `--sp-xl`
  - Radii: `--r-sm`, `--r-md`, `--r-lg`
  - Fonts: `--font-body`, `--font-data`, `--font-code`, `--font-display`
- Dark mode is the `:root` default. Light mode is defined via `@media (prefers-color-scheme: light)`. No manual override.
- Settings `⋯` menu in the header contains three items:
  1. `Cancel all active loops` button (conditionally visible when team loops are active)
  2. `Bell on converge` checkbox toggle (persisted via `localStorage` key `nautiloop_bell`)
  3. `Logout` form (POST to `/dashboard/logout` with CSRF token)
- The dark-mode palette already matches helm's dark theme values exactly (see comparison below). The contrast issue is specifically that `--text-secondary: #8A8784` (helm's `muted`) is below WCAG AA 4.5:1 against `--bg: #0F0F0E`.
- The light-mode palette (`--bg: #F7F5F2`, `--surface: #EDEAE6`) does NOT match helm's light theme (`--bg: #FAFAF8`, `--surface: #F0EFED`).

### Current dark-mode values vs helm dark theme

| CSS Variable | Dashboard (current) | Helm dark | Match? |
|---|---|---|---|
| `--bg` | `#0F0F0E` | `#0F0F0E` | Yes |
| `--surface` | `#1A1918` | `#1A1918` | Yes |
| `--surface-raised` | `#242322` | (no equivalent) | N/A |
| `--border` | `#2E2D2B` | `#2E2D2B` | Yes |
| `--text-primary` | `#E8E6E3` | `#E8E6E3` (text) | Yes |
| `--text-secondary` | `#8A8784` | `#8A8784` (muted) | Yes |
| `--text-tertiary` | `#5C5A57` | (no equivalent) | N/A |
| `--primary` | `#1B6B5A` | `#1B6B5A` (teal) | Yes |
| `--accent` | `#E8A838` | `#E8A838` (amber) | Yes |
| `--success` | `#2D7A4F` | `#2D7A4F` (green) | Yes |
| `--error` | `#C4392D` | `#C4392D` (red) | Yes |
| `--info` | `#3B7BC0` | `#3B7BC0` (blue) | Yes |

### Current light-mode values vs helm light theme

| CSS Variable | Dashboard (current) | Helm light | Match? |
|---|---|---|---|
| `--bg` | `#F7F5F2` | `#FAFAF8` | **No** |
| `--surface` | `#EDEAE6` | `#F0EFED` | **No** |
| `--surface-raised` | `#FFFFFF` | (no equivalent) | N/A |
| `--border` | `#D9D6D0` | `#D2D0CD` | **No** |
| `--text-primary` | `#1A1918` | `#1E1E1C` | **No** |
| `--text-secondary` | `#5C5A57` | `#6E6C69` (muted) | **No** |
| `--text-tertiary` | `#8A8784` | (no equivalent) | N/A |

Observed: the fleet summary line (`This week · 4 loops · $1.98 · 50% converged ...`) renders in `--text-secondary` against `--bg`. At `#8A8784` on `#0F0F0E` the contrast ratio is ~4.2:1 — below WCAG AA 4.5:1 for normal text — making it hard to scan quickly outdoors on mobile.

## Problem Statement

### Problem 1: Secondary text contrast is below WCAG AA

The dashboard's `--text-secondary` value (`#8A8784`) produces a contrast ratio of ~4.2:1 against `--bg` (`#0F0F0E`). While this matches helm's `muted` value exactly, the browser rendering context (subpixel antialiasing, mobile screens, outdoor viewing) makes it feel more washed out than the same value in a terminal emulator. The fleet summary line and filter chips — both important, always-visible data — are the most affected.

### Problem 2: Light palette diverges from helm

The dashboard's light-mode values were set independently from helm's light theme. Operators who use both see inconsistent colors. The light palette should be aligned with the helm light theme source of truth (`cli/src/commands/helm/themes.rs`).

### Problem 3: No manual theme control

`prefers-color-scheme` is fine for most users but not all:
- Mobile OS set to dark-at-night auto-switching — user wants dashboard consistent across the day, not flipping with sunset.
- Desktop user with OS light theme but prefers dark for long-running monitoring surfaces.
- Accessibility users who need high-contrast regardless of system setting.

Today they have no way to override without changing OS preferences.

## Functional Requirements

### FR-1: Contrast increase

**FR-1a.** Bump `--text-secondary` in dark mode from `#8A8784` to `#A29F9B` in `control-plane/assets/dashboard.css`. This raises the contrast ratio against `--bg` (`#0F0F0E`) from ~4.2:1 to ~5.2:1, comfortably above WCAG AA 4.5:1.

**FR-1b.** `--text-secondary` on dark backgrounds MUST meet WCAG AA 4.5:1 contrast minimum against `--bg`. If `#A29F9B` doesn't hit 4.5:1 (verify with a contrast checker), bump to `#B8B5B2` (~6.3:1). The implementor MUST verify the final value and document the measured ratio.

**FR-1c.** Align the light-mode palette with the helm light theme source of truth (`cli/src/commands/helm/themes.rs`). Replace the current light-mode values:

| CSS Variable | Current (dashboard) | New value (helm light) |
|---|---|---|
| `--bg` | `#F7F5F2` | `#FAFAF8` |
| `--surface` | `#EDEAE6` | `#F0EFED` |
| `--border` | `#D9D6D0` | `#D2D0CD` |
| `--text-primary` | `#1A1918` | `#1E1E1C` |
| `--text-secondary` | `#5C5A57` | `#6E6C69` |
| `--text-tertiary` | `#8A8784` | (keep as-is, no helm equivalent) |

The `--surface-raised` light value (`#FFFFFF`) has no helm equivalent; keep it unchanged.

Verify that the new `--text-secondary` light value (`#6E6C69`) meets WCAG AA 4.5:1 against `--bg` (`#FAFAF8`). `#6E6C69` on `#FAFAF8` is ~3.8:1 — this does NOT meet AA for normal text. **Bump light `--text-secondary` to `#5C5A57`** (the current value, ~5.0:1 against `#FAFAF8`) or `#4A4844` (~6.5:1). The implementor must verify and pick the value that passes 4.5:1 while remaining readable. Document the measured ratio.

**FR-1d.** No changes to the dark-mode values for `--bg`, `--surface`, `--surface-raised`, `--border`, `--text-primary`, `--text-tertiary`, `--primary`, `--accent`, `--success`, `--warning`, `--error`, or `--info` — these already match helm's dark theme exactly.

### FR-2: Theme toggle in settings menu

**FR-2a.** The existing header settings `⋯` menu (rendered in `control-plane/src/api/dashboard/render.rs` via Maud macros) gains a theme radio group. The menu order becomes:

```
Theme
  · System (default)
  · Dark
  · Light
─────────
Bell on converge  [checkbox]
─────────
Cancel all active loops
Logout
```

Visual style: three radio-button-like items, the currently-active one has a checkmark or filled dot prefix. Tapping another item switches theme and updates the checkmark. The theme section is visually separated from other menu items with a `<hr>` or border. Menu closes automatically after theme selection.

**FR-2b.** The user's selection is persisted client-side in `localStorage` under key `nautiloop_theme` with values `"system"`, `"dark"`, or `"light"`. Default (first visit or cleared storage) is `"system"`. This follows the same `nautiloop_*` naming convention as the existing `nautiloop_bell` key.

**FR-2c.** Theme resolution at page load:
- Read `localStorage.nautiloop_theme`.
- If `"dark"` or `"light"`: set `<html data-theme="dark|light">` before CSS parses to prevent a flash.
- If `"system"` or missing: leave `data-theme` unset; let `prefers-color-scheme` media queries apply.

**FR-2d.** CSS structure: keep dark as the `:root` default (matching the current CSS structure). Add explicit `[data-theme]` selectors that override the media query:

```css
/* Default: dark */
:root { --bg: #0F0F0E; ... }

/* System-auto light (existing media query) */
@media (prefers-color-scheme: light) {
  :root { --bg: #FAFAF8; ... }
}

/* Explicit overrides — win over media queries */
[data-theme="dark"] { --bg: #0F0F0E; ... }
[data-theme="light"] { --bg: #FAFAF8; ... }
```

Dark values are specified once and reused across both `:root` and `[data-theme="dark"]` (CSS variables or a `:where()` grouping to avoid duplication). Light values are similarly shared between the media query and `[data-theme="light"]`.

**FR-2e.** The toggle's JavaScript lives in the embedded `control-plane/assets/dashboard.js`. Follow the existing `initBell()` pattern: define an `initTheme()` function that reads the current `localStorage.nautiloop_theme` value, sets the active indicator in the menu, and attaches click handlers to the three theme radio items. Each click handler sets the `data-theme` attribute on `<html>`, writes `localStorage`, and updates the menu's active-item indicator. ~30 lines. No theme-switching library. `initTheme()` is called from the existing `init()` function alongside `initBell()`.

**FR-2f.** The pre-parse flash prevention: a 5-line inline `<script>` in the HTML `<head>` (in `control-plane/src/api/dashboard/render.rs`) reads localStorage and sets `data-theme` before body renders. Must ship before any CSS or other JS runs. Since the template uses Maud macros, the inline script must be injected via `PreEscaped()` to emit raw JavaScript without HTML escaping.

### FR-3: Dashboard status-line + card contrast tune-up

**FR-3a.** The fleet summary line in the header currently uses `color: var(--text-secondary)`. Since FR-1a bumps `--text-secondary` from `#8A8784` to `#A29F9B`, the summary line's contrast improves automatically. If that is still not prominent enough, change the summary line to use `color: var(--text-primary)` with `opacity: 0.85` instead. The implementor should judge visually after the FR-1a change and only apply the opacity approach if the bumped `--text-secondary` still feels recessed.

**FR-3b.** Card metadata rows (engineer badge, elapsed time, token count) use `--text-secondary` today; leave unchanged — they're genuinely secondary and benefit from the FR-1a bump.

**FR-3c.** Convergence-badge colors on cards (`--success` for CONVERGED, `--error` for FAILED) should pass 4.5:1 against both `--surface` variants (dark: `#1A1918`, light: `#F0EFED`). Verify:
- `--success` (`#2D7A4F`) against dark `--surface` (`#1A1918`): check passes 3:1 minimum for UI components. If it fails 4.5:1 for the badge text, use `#3A9A65` instead.
- `--error` (`#C4392D`) against dark `--surface`: similarly verify.

Document measured ratios.

## Non-Functional Requirements

### NFR-1: No new external dependencies

No CSS framework, no theme library, no new JS runtime deps. Vanilla CSS custom properties + a tiny JS handler.

### NFR-2: Backward compatibility

- URLs unchanged
- Existing cookies unchanged
- Existing endpoints unchanged
- Existing `localStorage` keys (`nautiloop_bell`, `nf`) unchanged
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

1. **Contrast check**: open dashboard, compare header fleet-summary readability to `nemo helm` header. Side by side, they should feel the same weight. No more washed-out look. Verify `--text-secondary` against `--bg` is >= 4.5:1 in both themes.
2. **Manual theme toggle**: open `⋯` menu → see three theme options → pick `Light` → page immediately renders light → refresh page → still light (localStorage persistence).
3. **System fallback**: pick `System` → page tracks OS preference. Toggle OS dark mode → dashboard follows.
4. **No flash**: refresh page with `Dark` selected on an OS set to light mode. No light-to-dark flash at page-load; theme applied before first paint.
5. **Light-mode helm consistency**: open `nemo helm` in one window with light theme, dashboard in another with Light selected. Layout colors match.
6. **Lighthouse**: accessibility score >= 90 for `/dashboard` route.
7. **Existing menu items preserved**: Bell toggle and Logout still work. Cancel-all still conditionally appears.

## Out of Scope

- **High-contrast theme** (3rd option beyond dark/light). Helm has one; dashboard gets `dark` and `light` only for v0.6.1. A separate spec can add it.
- **Per-user theme persistence** on the server side. Local-only.
- **Custom theme authoring**. Built-in palettes only.
- **Color-blind-mode palettes**. Revisit if requested.
- **Transitions / fade animations on theme switch**. Instant switch is fine.
- **Keyboard shortcut for theme switch**. Menu item only.
- **Syncing helm theme with dashboard theme via nemo.toml**. They're intentionally independent; helm is terminal, dashboard is browser.

## Files Likely Touched

- `control-plane/assets/dashboard.css` — bump `--text-secondary` dark value, align light-mode palette with helm, add `[data-theme="dark"]` and `[data-theme="light"]` selector blocks (FR-1, FR-2d).
- `control-plane/assets/dashboard.js` — `initTheme()` function following the `initBell()` pattern, theme toggle click handlers + localStorage (FR-2e).
- `control-plane/src/api/dashboard/render.rs` — inline `<script>` for flash prevention via Maud's `PreEscaped()` (FR-2f), new theme radio group in header `⋯` menu (FR-2a).
- Tests / screenshots per NFR-5.

## Helm Theme Source of Truth

The authoritative color values are in `cli/src/commands/helm/themes.rs`. Key mappings between helm variable names and dashboard CSS variable names:

| Helm name | Dashboard CSS variable |
|---|---|
| `bg` | `--bg` |
| `surface` | `--surface` |
| `border` | `--border` |
| `text` | `--text-primary` |
| `muted` | `--text-secondary` |
| `teal` | `--primary` |
| `amber` | `--accent` |
| `green` | `--success` |
| `red` | `--error` |
| `blue` | `--info` |

Dashboard-only variables with no helm equivalent: `--surface-raised`, `--text-tertiary`, `--primary-hover`, `--primary-muted`, `--accent-muted`, `--warning`.

## Baseline Branch

`main` at HEAD after PR #171 merge (which added this spec's docs commit). PR #170 introduced the docs refresh; #171 added the spec document itself.
