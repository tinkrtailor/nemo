# v0.6.0 Docs Refresh

## Overview

Update three existing documentation artifacts to reflect v0.6.0 features: the marketing landing page (`www/index.html`), the architecture reference (`docs/architecture.md`), and the deployment guide (`docs/deploy.md`). Tone stays founder/builder/technical — same voice the existing docs carry. No tone shift toward enterprise-cautious prose.

The README was already partially refreshed in PR #169; this spec covers the three heavier artifacts that need more editorial care than a CLI-command table update.

## Baseline

Main at PR #169 merge.

Current state per artifact:
- **`www/index.html`** — 342 lines. Sections: hero, how it works, "every engineer becomes a dev director", three verbs, deploy, security, "built with nautiloop" data table (81 rounds / 331 findings from original build), open source footer. Tone: founder-mode confident, bold typography, technical but punchy. Missing: dashboard, orchestrator judge, harden-by-default, pluggable cache, nemo helm phase 2.
- **`docs/architecture.md`** — 1141 lines. Comprehensive system design; covers pre-v0.6 features (loop engine, state machine, job builder, sidecar, git ops, state store). Missing: orchestrator judge, `/dashboard` subsystem, pluggable cache layer, pod introspection endpoint, `nemo extend` state transitions.
- **`docs/deploy.md`** — 201 lines. Terraform module reference, variable docs, deployment walkthrough. Missing: dashboard access over Tailscale, cache volume variable, pod-introspect RBAC, tomato new env vars.

## Problem Statement

### Problem 1: Landing page claims don't match the product

`www/index.html` advertises "Push a spec, get a clean PR" and walks through the core loop. It does NOT mention:
- The web dashboard (the biggest net-new capability in v0.6.0, and the primary reason a prospective user would open the landing page and go "oh, I want that")
- The orchestrator judge (the intelligence story: reviewer gets a second opinion)
- Harden-by-default (sharper-specs-automatically story)
- The "built with nautiloop" section is frozen at the original 81-round build; v0.6.0 itself was largely machine-produced but no one visiting the page would know

Net: the landing undersells. An engineer visiting today sees v0.3-era messaging.

### Problem 2: Architecture reference is incomplete

`docs/architecture.md` is the canonical reference for engineers going deep. Four major v0.6.0 subsystems (judge, dashboard, cache, introspection) are absent. Someone reading architecture.md forms an inaccurate mental model.

### Problem 3: Deploy guide under-documents new variables

`docs/deploy.md` guides terraform-module users. v0.6.0 added:
- `cache_volume_size` variable (was `cargo_cache_volume_size` in a superseded PR)
- Need for pod/exec RBAC on self-hosted clusters using pod-introspect
- Dashboard access pattern (Tailscale-native, `https://<ts-ipv4>/dashboard`)
- New env vars the deployment might pass

None of this is in the deploy guide.

## Functional Requirements

### FR-1: `www/index.html` updates

**FR-1a.** Keep the current hero: `Push a spec, get a clean PR.` Keep the hero-sub. Keep the terminal animation and CTA. No tone shift.

**FR-1b.** Update the page `<title>` to: `Nautiloop — Push a spec, get a clean PR.` (unchanged — it already says this).

**FR-1c.** Add a new section **between** "Three verbs" and "Deploy a nautiloop", with heading:

```html
<h2>Watch from anywhere</h2>
```

Content: a short paragraph and bullets explaining the dashboard:
- Web UI served by the control plane at `/dashboard`
- Mobile-first: cards on phone, tables on desktop
- Tailscale-native security model (the dashboard is as private as the server you run nautiloop on)
- At-a-glance: cost, convergence rate, engineer breakdown, recent terminal events
- One-tap actions: approve, cancel, extend from the phone

If a screenshot or live demo is referenced in the future, leave a placeholder: `<img>` tag commented out with `<!-- TODO: dashboard screenshot -->`. Do not fabricate an image path.

**FR-1d.** Update the "How it works" section to mention the orchestrator judge. The existing arch-flow is a 5-element CSS grid of `div.arch-node` elements (Your Machine → Nautiloop Server → Agent Pods) with `div.arch-arrow` separators — NOT ASCII art. Do not restructure the grid. Instead, add a short `<p>` paragraph **below** the `div.arch-flow` container (before the closing `</section>`) noting: "An orchestrator judge (LLM) decides transitions when the reviewer disagrees with itself or churns." This keeps the visual layout intact while surfacing the judge concept.

**FR-1e.** Replace the "Built with Nautiloop" data table. The old table shows 81 rounds / 331 findings from the original build. Replace with a newer table reflecting v0.6.0 self-convergence:

| Phase | What | PRs produced | Notes |
|---|---|---|---|
| Original build | Core loop + infrastructure | 3 lanes, 81 rounds, 331 findings | Hardened + implemented across three parallel lanes |
| v0.6.0 (self-hosted dogfood) | Judge, dashboard, helm phase 2, pluggable cache, CLI polish, mobile UX | 12+ machine-produced convergent PRs in one session | Nautiloop implementing its own features against its own codebase |

The existing 5-row per-lane breakdown (Core loop engine, Infrastructure, Agent runtime, Integration, Total) is fully replaced by the 2-row summary table above. The per-lane granularity is no longer needed; the summary row "3 lanes, 81 rounds, 331 findings" preserves the aggregate numbers.

Retain the bold "331 production bugs caught by cross-model review before first deploy" line as historical context; add a sibling line: "v0.6.0: nautiloop shipped 12+ of its own feature PRs in a single day of dogfooding." Use "12+" consistently in both the table and the callout line (12 is the total machine-produced PR count; do not use "10+" elsewhere). Keep the voice confident-but-honest; don't inflate numbers.

**FR-1f.** Update the harden-by-default messaging. The "Three verbs" section is a 3-column CSS grid of `div.verb-card` elements — NOT a table. Do not add a 4th card (that would break the grid and violate NFR-3). Instead, update the existing `<p class="verbs-note">` paragraph (currently reads "Add `--harden` to `start` or `ship` to harden the spec before implementing.") to read: "Add `--no-harden` to skip the harden phase (default: on)." This is the simplest change that conveys harden-by-default without touching the grid layout.

**FR-1g.** Security section: add a bullet for the dashboard's auth model: "Dashboard auth = same API key as the CLI. HttpOnly cookies. Behind Tailscale by default. See `docs/dashboard-setup.md`." Mentioning Tailscale once is enough; don't oversell. The "See `docs/dashboard-setup.md`" text must link using the same absolute GitHub URL pattern as the existing footer (e.g., `https://github.com/tinkrtailor/nautiloop/blob/main/docs/dashboard-setup.md`), NOT a relative path — relative paths would resolve incorrectly from the `www/` serving origin.

**FR-1h.** Documentation links footer: add links to `docs/local-dev-quickstart.md` and `docs/dashboard-setup.md`. Both files exist at HEAD. Links must use the same absolute GitHub URL pattern as existing footer links (e.g., `https://github.com/tinkrtailor/nautiloop/blob/main/docs/dashboard-setup.md`). Follow the existing `<a>` + `<span class="footer-sep">·</span>` structure. Ensure link labels read naturally; do not add any link that points at a non-existent doc.

### FR-2: `docs/architecture.md` updates

**FR-2a.** Add a new top-level section `## Orchestrator Judge` after the existing `## State Machine Diagram` section (currently the 4th `##` heading in architecture.md). Content:
- What it is: an LLM call at loop transition points that decides `continue | exit_clean | exit_escalate | exit_fail` when the reviewer's verdict is ambiguous or churning.
- Where it runs: in-process from the loop engine (NOT a separate pod), reusing the sidecar model proxy.
- When it fires: on review-clean-but-with-medium+-issues, on round >= max_rounds with recurring findings, on audit ambiguity.
- Data it reads: full spec, round history, current verdict, recurring-finding analysis.
- Storage: every decision is persisted to `judge_decisions` table (loop_id, round, phase, trigger, input_json, decision, confidence, reasoning, hint, duration_ms, created_at, loop_final_state, loop_terminated_at).
- Future: training signal for a resident fine-tuned judge in v2.

**FR-2b.** Add a new top-level section `## Dashboard` after the judge section. Content:
- Routes under `/dashboard/*` on the existing axum server (no new process).
- Server-rendered HTML via `maud` (the workspace uses `maud = "0.26"`), single embedded JS file polling `/dashboard/state` every 5s.
- Auth model: existing API key, cookie-based for browser sessions, bearer for programmatic.
- Features covered: card grid, loop detail with rounds table + diff + live logs, feed of terminal events, per-spec history, stats deep-dive (`/dashboard/stats?window=7d`), kill switch, fleet summary header.
- Security: inherits deployment security (Tailscale on hetzner module). Dashboard on localhost in dev; NEVER expose to public internet without fronting auth.
- No new database: aggregates computed on-demand from existing `loops` + `rounds` tables with a 60s cache on the stats endpoint.

**FR-2c.** Add a new top-level section `## Pluggable cache` after dashboard. Content:
- One PVC `nautiloop-cache` mounted at `/cache` on implement/revise pods.
- Env-var passthrough: `[cache.env]` in `nemo.toml` becomes pod env. No control-plane code per backend.
- Covered tools: sccache (default for Rust), ccache, npm, pnpm, yarn, bun, pip, poetry, uv, turbo, go, gradle, anything that wants a writable dir.
- Operational: `nemo cache show` prints resolved env + disk usage + recent hit stats.
- Terraform: `cache_volume_size` variable (default 50 GiB).

**FR-2d.** Add a `QA` stage note. There is no dedicated "Stages" section in architecture.md — stages are described inline within `## Full Loop Lifecycle (Harden + Implement)` (the 3rd `##` heading). Add a new subsection `### QA Stage (deferred)` at the end of `## Full Loop Lifecycle (Harden + Implement)`, before the state machine section. Content: "Deferred v2 work: runs acceptance-criteria verification after review-clean, before CONVERGED. Gated by `[qa] enabled = true` in nemo.toml. See `specs/qa-stage.md`." Do NOT imply it is shipped.

**FR-2e.** Update state machine diagram/description to include the `AWAITING_REAUTH` → resume transition (via `nemo auth --claude` + `nemo resume`) and the new extend flow: `FAILED` with `failed_from_state` → `nemo extend --add N` → resumes at last stage.

**FR-2f.** Add `nemo ps` and the `/pod-introspect/:id` endpoint. There is no dedicated "Observability" section in architecture.md. Create a new top-level section `## Observability` after `## Pluggable cache` (the section added in FR-2c). Content: a 2-sentence description of `nemo ps` (lists running agent pods with status, resource usage, and current stage) and `/pod-introspect/:id` (returns live pod details including logs tail, resource metrics, and current round — used by the dashboard and CLI).

### FR-3: `docs/deploy.md` updates

**FR-3a.** Add a subsection `### Accessing the dashboard` under the main deploy walkthrough:
- Default URL: `https://<server-ip-or-hostname>/dashboard`
- Hetzner example default: `https://<tailscale-ipv4>/dashboard` — already bound to tailnet-only by the terraform module.
- How to log in: API key from the cluster (`kubectl get secret nautiloop-api-key -o jsonpath='{.data.NAUTILOOP_API_KEY}' | base64 -d`). Engineer name is self-declared on login.
- Security callout: the dashboard is as private as the server. Do NOT expose to public internet without fronting with oauth2-proxy or similar.

**FR-3b.** Add a new variable to the module variable reference table: `cache_volume_size` (number, default 50, "Size of the shared /cache compiler cache PVC in GiB; used by sccache, ccache, npm, pnpm, yarn, bun, pip, turbo, go, and any tool configured via [cache.env] in nemo.toml"). The deprecated `cargo_cache_volume_size` alias: note it exists for one release cycle then is removed.

**FR-3c.** Add a subsection `### Pod introspection RBAC` explaining that `nemo ps` and the `/pod-introspect/:id` endpoint require `pods/exec` permission on the `nautiloop-jobs` namespace. Note that the terraform module provisions this by default; operators installing manifests by hand need to grant it.

**FR-3d.** Add a subsection `### Cache configuration examples` with a short snippet showing:
- Default (Rust-only, sccache): `[cache.env]` with sccache vars only.
- Polyglot example (Rust + TypeScript): sccache + npm + pnpm.
- Disabled: `[cache] disabled = true`.

**FR-3e.** There is no "What gets installed" section in deploy.md by that name. The deployment walkthrough text (lines 1-5 and ~150-157) describes what the module provisions. Add a new subsection `### What the module installs` near the top of the deploy guide (after the intro paragraph, before "Quick start") listing the components: k3s, Postgres, control plane (which includes the API server, loop engine, orchestrator judge, and dashboard — all in-process, not separate deployments), agent-base image, sidecar image. This makes it explicit that the judge and dashboard are first-class components shipped as part of the control plane binary.

## Non-Functional Requirements

### NFR-1: Tone consistency

Every added sentence matches the existing voice. No jargon shift toward "enterprise-grade", "turnkey", "mission-critical". Current voice is founder/builder: direct, technical, opinionated, honest about limitations.

### NFR-2: Accuracy

Every claim added must be true at v0.6.0 (PR #167 merge). No future-tense "will" without a "deferred" or "planned" qualifier. QA stage specifically is deferred; mobile-dashboard + judge + pluggable cache are shipped. Double-check against: merged PRs #155 (helm phase 2), #156 (harden-by-default), #157 (judge), #158 (pluggable cache), #162 (LLM-friendly CLI), #165 (auth keychain), #166 (mobile dashboard).

### NFR-3: No visual redesign

HTML markup changes in `www/index.html` use existing CSS classes. New section uses same `<section>` pattern as sibling sections. No new CSS file, no new fonts, no layout restructuring. If the new section doesn't fit cleanly into existing classes, add one minimal class in `www/style.css`, nothing more.

### NFR-4: No broken links

Every link added (in README, landing, docs/*) must point at a file that exists at the commit this spec is implemented against.

### NFR-5: Tests

- **Unit (manual)**: open `www/index.html` in a browser, verify the new "Watch from anywhere" section renders; verify the updated "Built with Nautiloop" table shows both historical and v0.6.0 rows.
- **Link check (manual or scripted)**: every internal link in README.md, www/index.html, docs/architecture.md, docs/deploy.md resolves to a real file or section.
- **No hot fix regressions**: existing tests don't break (this spec is documentation-only, so the only way to regress tests is to accidentally delete a doc another test asserts about — run `cargo test --workspace` just to confirm).

## Acceptance Criteria

A reviewer can verify by:

1. **Landing**: open `www/index.html` in a browser, see "Watch from anywhere" section between "Three verbs" and "Deploy a nautiloop". See updated "Built with Nautiloop" table with two rows. No placeholder images rendered.
2. **Architecture**: `docs/architecture.md` has new `## Orchestrator Judge`, `## Dashboard`, `## Pluggable cache`, `## Observability` sections. QA stage subsection under Full Loop Lifecycle notes it's deferred.
3. **Deploy**: `docs/deploy.md` has `### What the module installs`, `### Accessing the dashboard`, `### Pod introspection RBAC`, `### Cache configuration examples` subsections.
4. **Links resolve**: `grep -Eo '\]\([^)]+\)' docs/*.md www/index.html | sort -u` lists only existing targets (manual or scripted).
5. **Tone check**: no instance of "enterprise-grade", "mission-critical", "best-in-class", or similar buzzwords. Voice unchanged.
6. **No regression**: `cargo test --workspace` passes.

## Out of Scope

- **Redesigning the landing page visually**. Copy + structure only. Use existing CSS.
- **Adding screenshots or GIFs**. `<img>` placeholder with TODO comment is the contract; actual capture is a follow-up spec (needs browse-daemon integration).
- **Translating docs to other languages**. English only.
- **Auto-generating `docs/architecture.md` from source**. Hand-written prose, not generated.
- **Rewriting `docs/convergence-learnings.md` or `docs/design.md`**. Those are historical artifacts; leave them.
- **Updating CONTRIBUTING.md** unless a specific v0.6.0 workflow change needs to land there. (None identified.)
- **Auto-linking API types**. Prose references to `LoopState::QA` etc. are fine; no cross-linking to rustdoc.
- **SEO optimization of the landing page**. Out of scope.

## Files Likely Touched

- `www/index.html` — new section, updated table, updated security bullet, updated verbs note.
- `www/style.css` — at most one minimal class if the new section doesn't fit existing layout.
- `docs/architecture.md` — new sections for judge, dashboard, pluggable cache; updated state machine + observability.
- `docs/deploy.md` — new subsections for dashboard access, pod-exec RBAC, cache examples, new module variable.

## Baseline Branch

`main` at PR #169 merge.
