# Design System — Nautiloop

## Product Context
- **What this is:** Convergent loop engine. Claude implements, OpenAI reviews, repeat until clean.
- **Who it's for:** Senior developers and engineering teams who care about code quality
- **Space:** Developer infrastructure (peers: Linear, Railway, Vercel, Cursor)
- **Project type:** CLI tool + docs + future web dashboard + marketing site (nautiloop.dev)

## Aesthetic Direction
- **Direction:** Dark Industrial-Terminal
- **Decoration level:** Minimal. Content is the design. Code blocks, diffs, and terminal output are the visual language.
- **Mood:** Confident, technical, data-dense. Like htop meets GitHub PR review. Built by engineers, for engineers.
- **Dark-first:** Dark mode is the primary experience. Light mode is secondary, for docs/README where needed.
- **Anti-patterns:** No soft gradients, no cream backgrounds, no illustrations, no decorative elements, no purple gradients, no centered-everything layouts, no bubbly border-radius.

## Logo
- **Mark:** Nautilus spiral (convergent loop / golden ratio)
- **Direction:** Approved variant B from design-shotgun session (2026-03-31)
- **Wordmark:** "NAUTILOOP" in Satoshi Bold, letterspaced
- **Usage:** Mark alone at 32px (favicon), mark + wordmark at larger sizes

## Typography
- **Display/Hero:** Satoshi Bold — geometric with personality, pairs with the nautilus mark
- **Body:** DM Sans — clean, readable, good pairing with Satoshi
- **UI/Labels:** DM Sans Medium
- **Data/Tables:** Geist (tabular-nums) — tight, modern, built for numbers
- **Code:** JetBrains Mono — familiar, trusted, ligatures
- **Loading:** Google Fonts for Satoshi + DM Sans, self-hosted for Geist + JetBrains Mono
- **Scale:**
  - xs: 12px / 0.75rem
  - sm: 14px / 0.875rem
  - base: 16px / 1rem
  - lg: 18px / 1.125rem
  - xl: 20px / 1.25rem
  - 2xl: 24px / 1.5rem
  - 3xl: 30px / 1.875rem
  - 4xl: 36px / 2.25rem
  - hero: 48px / 3rem

## Color

### Dark Mode (primary)
- **Background:** #0F0F0E (near-black, warm)
- **Surface:** #1A1918 (cards, code blocks, sidebar)
- **Surface raised:** #242322 (dropdowns, modals)
- **Border:** #2E2D2B (subtle separation)
- **Text primary:** #E8E6E3 (warm off-white)
- **Text secondary:** #8A8784 (muted, for labels and descriptions)
- **Text tertiary:** #5C5A57 (disabled, placeholder)

### Light Mode (secondary, for docs/README)
- **Background:** #F7F5F2 (warm off-white)
- **Surface:** #EDEAE6 (cards, code blocks)
- **Surface raised:** #FFFFFF
- **Border:** #D9D6D0
- **Text primary:** #1A1918
- **Text secondary:** #5C5A57
- **Text tertiary:** #8A8784

### Brand Colors
- **Primary (teal):** #1B6B5A — ocean depth, active states, links, CTAs
- **Primary hover:** #237D6A
- **Primary muted:** #1B6B5A33 (20% opacity, for backgrounds)
- **Accent (amber):** #E8A838 — golden ratio, warnings, findings counts, highlights
- **Accent hover:** #F0B844
- **Accent muted:** #E8A83833 (20% opacity)

### Semantic Colors
- **Success:** #2D7A4F (green, converged/clean)
- **Warning:** #C4841D (amber-adjacent, findings remain)
- **Error:** #C4392D (red, failed/blocked)
- **Info:** #3B7BC0 (blue, neutral status)

### Diff Colors
- **Addition:** #2D7A4F on #2D7A4F1A background
- **Deletion:** #C4392D on #C4392D1A background

## Spacing
- **Base unit:** 4px
- **Density:** Compact-to-comfortable. Data-dense where information matters, breathing room for marketing.
- **Scale:** 2xs(2px) xs(4px) sm(8px) md(16px) lg(24px) xl(32px) 2xl(48px) 3xl(64px)

## Layout
- **Approach:** Grid-disciplined for dashboard/docs, editorial for marketing hero only
- **Dashboard:** Sidebar (200-240px) + main content, no max-width constraint
- **Docs:** Sidebar (200px) + content (max 720px) + optional right TOC
- **Marketing:** Full-width sections, content max 1100px
- **Border radius:** sm: 4px, md: 6px, lg: 8px (tight, not bubbly)

## Motion
- **Approach:** Minimal-functional only
- **Easing:** enter(ease-out) exit(ease-in) move(ease-in-out)
- **Duration:** micro(50ms) short(150ms) medium(250ms)
- **Rules:** No bounce, no spring, no decorative animation. Transitions serve comprehension only.
- **Exception:** Nautilus spiral may animate once on initial site load (single rotation, 800ms, subtle)

## CLI Output Style
- **Teal** for success states and active indicators
- **Amber** for warnings and findings counts
- **Red** for errors and failures
- **Muted gray** for secondary information
- **Bold white** for headings and important values
- Use Unicode box-drawing characters sparingly for structure

## Decisions Log
| Date | Decision | Rationale |
|------|----------|-----------|
| 2026-03-31 | Dark-first design system | Target audience lives in terminals. Dark mode is the primary experience, not an afterthought. |
| 2026-03-31 | Teal + amber palette | Ocean teal = nautilus home. Amber = golden ratio. Every color tells the brand story while functioning as syntax-highlighting-native colors. |
| 2026-03-31 | Warm neutrals, not cool | Differentiates from Linear/Vercel cool-gray aesthetic. Feels like craft, not corporate. |
| 2026-03-31 | Data-dense over spacious | This is infrastructure. Users want information density, not whitespace. Terminal output and diffs are first-class visual elements. |
| 2026-03-31 | Nautilus spiral logo | Approved in design-shotgun session. Golden ratio / convergent loop metaphor. |
