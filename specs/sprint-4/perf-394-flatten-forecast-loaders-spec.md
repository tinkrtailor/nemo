# perf-394: Flatten forecast loaders and bundle forecast load into one endpoint

Tracking: Reitun/virdismat-mono#394

## Overview

Loading a single forecast in the UI takes up to 45 seconds after the Hetzner migration. The host is idle, Postgres is fast. The primary bottleneck is deeply nested Sequelize `include` trees in `apps/api/src/api/data/forecast/index.ts` that produce cartesian-join result sets megabytes in size. With ~36 ms RTT between Hetzner (Finland) and DO Postgres (Frankfurt), wire time per large-payload query scales linearly with bytes.

A secondary factor is the frontend loader's call graph: `apps/frontend/src/Components/Forecast/services/fetchForecastData.ts` makes one sequential call to `getSingleForecast`, then a metadata `Promise.all`, then fires 4 more parallel `Promise.all` groups for forecast data (~23 forecast-specific endpoints plus ~10 reference-data endpoints). So it's **not** "20 sequential calls". The sequential edges are small, and most of the slowness is inside `getSingleForecast` and the per-group slowest-wins latency. Bundling still wins because it collapses the sequential edges and lets the server parallelise without the frontend's HTTP round-trip overhead, but the performance model is payload-dominated, not RTT-count-dominated.

This spec covers two changes that must land together because they touch the same hot path:

1. **Flatten the deeply nested forecast loaders** so children are fetched via small `WHERE parentId IN (...)` queries instead of cartesian joins.
2. **Add a single `POST /v1/forecasts/get-forecast-bundle` endpoint** that assembles everything the forecast screen needs in one response, and switch the frontend forecast loader to use it.

The correctness bar is zero regressions in the forecast screen or the calculation engine. The refactor is high risk because Sequelize nested `include` responses have specific shape, aliases, ordering, and are model instances (not plain objects); the calculation engine and UI both depend on these details. The spec therefore leans heavily on snapshot equivalence testing.

## Evidence and motivation

Measured on production 2026-04-09:

- Host metrics: CPU 99% idle, load 0.00, 0% iowait, 0% steal, api container 0.02% CPU, 372 MiB / 1 GiB mem. The box is bored.
- `SELECT 1` round trip from the api container: 36 ms stable.
- `count(*) forecasts`: 37 ms.
- `POST /v1/forecasts/get-single-forecast` response time in prod logs: 10–18 s, worst 18.8 s.
- `POST /v1/forecasts/publish-forecast` worst: 59.3 s.
- Per forecast screen load, the frontend runs the call graph described in the Overview (1 sequential + 4 parallel groups = ~23 forecast-specific endpoints + ~10 reference endpoints). The "20 distinct endpoints" counts in the #394 request log are aggregate across sessions and multiple pages, not a single screen load.

## Requirements

### Functional requirements

- **FR-1**: `_getSingleForecast` in `apps/api/src/api/data/forecast/index.ts` shall load its data using flat queries with at most one level of Sequelize `include`. Any child association beyond one level shall be loaded in a separate query using `where: { parentFk: { [Op.in]: parentIds } }`. Every flat query shall include an explicit `order: [...]` clause that reproduces the ordering the legacy nested-include query produced (or, where the legacy had no explicit order, an order that preserves the observed legacy ordering under the fixtures). Manual stitching shall preserve parent-array order and child-array order exactly. See OQ-2 for which arrays have implicit order contracts; the implementer shall grep frontend consumers (`apps/frontend/src/Components/Forecast/**`) for `.sort(`, `.find(`, and index access patterns on these arrays and document the findings in the PR body.
- **FR-2**: The loaders in scope for flattening (including `_getSingleForecast` from FR-1) are:
  - `_getSingleForecast`
  - `_getForecastedData`
  - `_getForecastedSubData`
  - `_getFPercOfKeyData`

  **Removed from scope after verification**: `_getForecastFormulas` is already a top-level-filtered flat query (`apps/api/src/api/data/forecast/index.ts:2101`) and was incorrectly listed in #394. No change needed there. Implementer shall re-verify this before coding. No other loaders in `forecast/index.ts` are in scope for this spec.
- **FR-3**: The response payload shape and field names of each refactored loader shall remain byte-for-byte identical to the pre-refactor output (after JSON serialization), with one exception: key order within an object is allowed to differ. Array order shall be preserved. See T-1 and T-2 for how this is enforced.
- **FR-4**: A new endpoint `POST /v1/forecasts/get-forecast-bundle` shall be added. It takes `{ forecastId: string (uuid) }` in the request body and requires the same auth and role gate as `/v1/forecasts/get-single-forecast` (`setUserAccess([ADMINS])`).
- **FR-5**: The bundle endpoint shall return a JSON object with one keyed field per forecast-specific call in `apps/frontend/src/Components/Forecast/services/fetchForecastData.ts` (the real source of truth). The verified list, drawn from `fetchForecastData.ts:293-398`:

  ```json
  {
    "success": true,
    "bundle": {
      "forecast":                <response of /v1/forecasts/get-single-forecast>,
      "dependencies":            <response of /v1/forecasts/get-dependencies>,

      "historicalData":          <response of /v1/forecasts/fetch-historical-data>,
      "historicalSubKeyData":    <response of /v1/forecasts/fetch-historical-sub-key-data>,
      "historicalPercOfData":    <response of /v1/forecasts/fetch-historical-perc-of-data>,
      "historicalNtmData":       <response of /v1/forecasts/fetch-historical-ntm-data>,
      "historicalTtmData":       <response of /v1/forecasts/fetch-historical-ttm-data>,
      "historicalMultiplesData": <response of /v1/forecasts/fetch-historical-multiples-data>,

      "forecastedData":          <response of /v1/forecasts/get-forecasted-data>,
      "subKeyData":              <response of /v1/forecasts/get-sub-key-data>,
      "ttmData":                 <response of /v1/forecasts/get-forecast-ttm-data>,
      "ntmData":                 <response of /v1/forecasts/get-forecast-ntm-data>,
      "multiplesData":           <response of /v1/forecasts/get-multiples-data>,
      "percOfKeyData":           <response of /v1/forecasts/get-forecast-perc-of-data>,

      "peergroupData":           <response of /v1/forecasts/get-peergroup-data-by-forecast>,
      "forecastKeyGroups":       <response of /v1/forecasts/get-forecast-key-groups>,
      "forecastKeySummations":   <response of /v1/forecasts/get-forecast-key-summations>,
      "forecastBeta":            <response of /v1/forecasts/get-forecast-beta>,
      "forecastGovBond":         <response of /v1/forecasts/get-forecast-gov-bond>,
      "forecastFormulas":        <response of /v1/forecasts/get-forecast-formulas>,

      "forecastYears":           <response of /v1/forecasts/get-forecast-years>,
      "forecastEIData":          <response of /v1/forecasts/get-forecast-ei-data>,
      "forecastIndexes":         <response of /v1/forecasts/get-forecast-indexes>
    }
  }
  ```

  That's 23 forecast-specific keys. Each keyed field shall contain exactly the same payload that the corresponding existing endpoint returns (minus its outer `success: true` envelope). `reportTables` was in the draft spec but is wrong; it lives under `routes/reports/`, not `forecasts/`, and the frontend forecast loader does not call it. Reference-data endpoints called by `fetchForecastData.ts` (`getAllReportTypes`, `getLanguages`, `getMultipleTypes`, `getQuarters`, `getBetas`, `getCountries`, `getMaturities`, `getIndexTypes`, `getYearSummationTypes`) are **not** in the bundle; they are global/cacheable reference data and the frontend can continue calling them in parallel or through an existing cache. See OQ-9 if we reconsider this.

  Implementer shall diff this list against `fetchForecastData.ts` at implementation time and add any forecast-specific call this list misses as a blocker.
- **FR-6**: The bundle endpoint shall:
  1. Fetch the forecast first via `_getSingleCurrentForecast`. If it returns `{ success: false, status: 401, forecast: null }`, the bundle endpoint shall short-circuit and return that same shape at the top level **without** running the other 22 sub-loaders. This matches current behavior when the forecast does not exist or the user lacks access.
  2. Otherwise, run all 22 remaining sub-loaders in parallel via `Promise.allSettled`, wrapped so each result carries its bundle-key:
     ```ts
     const entries = await Promise.allSettled(
       loaders.map(({ key, run }) => run().then(value => ({ key, value }), error => ({ key, error })))
     );
     ```
  3. After all 22 settle, build the bundle object with one entry per key. For each rejected sub-loader, the bundle value shall be `{ success: false }` (matching the shape the legacy individual endpoint returns on failure, so frontend consumers that already handle `{ success: false }` keep working unchanged). For each fulfilled sub-loader, the bundle value shall be the sub-loader's payload minus its outer `success: true` envelope, exactly as specified in FR-5.
  4. The top-level response shall always be `{ success: true, bundle: { ... } }` with HTTP 200, regardless of how many sub-loaders failed, **as long as the forecast-existence check in step 1 passed**. This preserves current frontend behavior where a failed sub-call degrades a single widget but does not break the screen.
  5. Every rejection shall be logged server-side with `{ forecastId, bundleKey, error }` and reported to Sentry as a breadcrumb on the bundle transaction so ops has visibility into how many sub-loaders trip in a single bad request. The bundle endpoint itself does not return 500 on sub-loader failures; Sentry is the monitoring surface.
  6. The one case that still returns a non-200 is the step-1 forecast-existence short-circuit (returns the `{ success: false, status: 401, forecast: null }` shape, same as legacy `/get-single-forecast`). All other failures are tolerated per rule 4.

  Sequelize connection pool note: pool is already configured with `max: 20` in prod (`apps/api/src/api/sequelize.ts:13`), so 22-way parallelism fits. The test DB pool is `max: 5`, which is smaller than the bundle fan-out; T-5 shall either run with a raised test pool or explicitly document that test timing is not representative of prod.

  Sequelize connection pool note: pool is already configured with `max: 20` in prod (`apps/api/src/api/sequelize.ts:13`), so 22-way parallelism fits. The test DB pool is `max: 5`, which is smaller than the bundle fan-out; T-5 shall either run with a raised test pool or explicitly document that test timing is not representative of prod.
- **FR-7**: The existing individual endpoints (`/get-single-forecast`, `/get-forecast-ttm-data`, etc.) shall remain functional and unchanged in behavior. They may be reimplemented internally by calling the same flattened loaders. They shall not be deleted in this PR, to avoid breaking non-forecast-screen callers (see Open Question 1).
- **FR-8**: The frontend forecast loader shall be rewired to make a single call to `/get-forecast-bundle` instead of the ~20 sequential calls. The consumer code that currently handles each individual response (components, hooks, state setters) shall receive the same shapes from the bundle and require no changes beyond the network-call site.
- **FR-9**: Sentry tracing shall be enabled on the bundle endpoint with manual spans. The handler shall create a root transaction via existing Sentry Express auto-instrumentation, and wrap each of the 20 sub-loader calls in `Sentry.startSpan({ name: '<bundle-key>', op: 'db.forecast-loader' }, async () => ...)` so the Sentry flamegraph shows per-sub-loader duration in prod. No auto-instrumentation on Sequelize itself is required.

### Non-functional requirements

- **NFR-1 (correctness)**: Zero regressions in calculation engine outputs, forecast screen rendering, or any existing forecast route test.
- **NFR-2 (performance)**: Forecast screen load time in staging shall be measurably better after this PR. There is no numeric p95 target; the merge bar is "significantly faster" confirmed by before/after timing in the PR description. Implementer shall record a before/after number from staging in the PR body for at least one fixture forecast.
- **NFR-3 (rollback)**: All of FR-1 through FR-9 ship in a single PR, but gated behind two env flags so a wrong-numbers incident can be rolled back with a one-line env change and an api/frontend restart:
  - `FORECAST_FLAT_LOADERS_ENABLED` (api): when `false`, `_getSingleForecast`, `_getForecastedData`, `_getForecastedSubData`, `_getFPercOfKeyData` delegate to the frozen legacy implementations in `apps/api/src/api/data/forecast/__legacy__/perf-394.ts` (see T-1 / OQ-7). Default `true`.
  - `FORECAST_BUNDLE_ENABLED` (frontend): when `false`, `fetchForecastData.ts` uses the individual per-endpoint call graph. Default `true`.

  Both flags default `true` on merge. Soak period: 7-14 days in prod. Once the soak passes with no incidents, a follow-up PR deletes the legacy loader code, removes both flags, and removes the `__legacy__` oracle files. The follow-up PR is out of scope for this spec but is blocked on this spec's merge.
- **NFR-4 (no new dependencies)**: No new runtime dependencies. Test fixtures may use existing dev dependencies (faker, factory helpers already present in `apps/api/src/__tests__/integration/`).

## Behavior

### Normal flow

1. Frontend forecast loader calls `POST /v1/forecasts/get-forecast-bundle` with `{ forecastId }`.
2. API authenticates the request via the existing Cognito middleware, checks the `ADMINS` role gate.
3. Handler validates the body via Zod (`{ forecastId: z.string().uuid() }`).
4. Handler calls all 20 sub-loader functions in parallel via `Promise.all`. Each sub-loader internally uses the flattened query pattern (FR-1, FR-2).
5. Handler assembles the keyed `bundle` object (FR-5) and returns `{ success: true, bundle }` with HTTP 200.
6. Frontend destructures the bundle and hands each field to the existing consumers.

### Alternative flows

- **Forecast not found / no access**: current behavior is that `_getSingleCurrentForecast` returns `{ success: false, status: 401, forecast: null }`; the legacy `/get-single-forecast` route echoes that through as HTTP 401. Several other individual loaders (e.g. historical data) dereference `forecast.baseYearId` internally and will throw → HTTP 500 if the forecast is missing. Current frontend (`fetchForecastData.ts:297`) short-circuits on `getSingleForecast` failure and renders a "not found" state, so those downstream 500s are never hit in the happy path. The bundle endpoint shall preserve this: fetch the forecast first, and if it is missing/unauthorised, return `{ success: false, status: 401, forecast: null }` at the top level without running the other sub-loaders. This matches `_getSingleCurrentForecast`'s return and is what the frontend short-circuit already handles.
- **Partial failure of a non-forecast sub-loader**: bundle tolerates the failure. The failed section's value in the bundle is `{ success: false }`, matching the shape the legacy individual endpoint returned on failure. Frontend consumers that already handle `{ success: false }` at their call site keep working unchanged. Every rejection is logged server-side and reported to Sentry as a breadcrumb on the bundle transaction. This matches current `fetchForecastData.ts` graceful-degradation behavior exactly and is not behavioral drift.
- **Unauthenticated / wrong role**: middleware rejects with 401/403 as today.
- **Invalid body**: Zod rejects with 400 as today.

### Edge cases

| Scenario | Expected behavior |
|---|---|
| Forecast has zero `ForecastSheets` | All sub-loaders return empty arrays; bundle returns a valid object with empty collections; frontend renders an empty forecast screen. |
| Forecast has `ForecastSheets` but no `ForecastKeys` | `forecast.ForecastSheets[n].ForecastKeys` is `[]` (not `undefined`). |
| `ForecastKeys` without `ForecastSubKeys` | `ForecastKeys[n].ForecastSubKeys` is `[]`. |
| Missing `priorForecast` | Field is `null`. |
| Forecast with thousands of `ForecastedData` rows (largest prod forecast shape) | Bundle completes within the same order of magnitude as the slowest existing individual endpoint before the refactor; no OOM, no timeout. |
| A flat `IN (...)` query with an empty parent-id list | Loader short-circuits and returns `[]` without issuing the query. |
| Two parents share a child row (not expected but defensive) | Child is associated with both parents in the stitched output; no duplicate rows. |

## Root cause recap (code pointers)

From #394, confirmed by reading the file during triage:

- `apps/api/src/api/data/forecast/index.ts:170` `_getSingleForecast`:
  - Query 1: `Forecasts.findOne` with nested `ForecastSheets → ForecastKeys → Keys`.
  - Queries 2..N+1: `ForecastSheets.map(sheet => ForecastKeys.findAll(...))` with 15+ nested includes (YearSummationTypes, FcastKeySum, Keys → KeySummation/KeyTranslations/Languages/Units, KeyTypes×2, ForecastHFormulas, ForecastHFormulaParameters → ForecastHFormulas → ForecastKeys, ForecastFFormulas, ForecastFFormulaParameters, ForecastSubKeys → SubKeys / ForecastEI, ForecastPercOfKeys → PercOfKeys / ForecastKeys → Keys, ForecastTTMKeys → TTMKeys, ForecastNTMKeys → NTMKeys, ForecastMultiples `aFK` + `bFK` → Multiples / ForecastKeys → Keys).
  - Queries N+2..2N+1: another `ForecastSheets.map(... ForecastMultiples.findAll)`.
  - Query 2N+2: `Forecasts.findAll` with another huge include tree (Companies, PeerGroupInsert, CompanyReportFactor, priorForecast, ReportTypes, Languages, owner, forecastPeriod, baseYear, Quarters, Years, ForecastSheets → Sheets / SheetTypes / ForecastKeysRefs / ForecastMultiplesRefs).
- Similar cartesian include patterns in `_getForecastedData`, `_getForecastedSubData`, `_getFPercOfKeyData`, `_getForecastFormulas`.

## Testing strategy

This is where regressions will be caught or missed. All of the following are acceptance criteria.

### Test data

- **TD-1**: Test fixtures shall be built via factory functions in the existing Vitest integration-test infrastructure (`apps/api/src/__tests__/integration/testDb.ts` and siblings). No prod data, no anonymised prod dumps. The product owner may provide read-only prod credentials later to smoke-test, but seeded mock data is the test-gating bar.
- **TD-2**: At least 3 seeded forecast shapes shall exist, created via factories:
  - **Small**: 1 sheet, 5 forecast keys, no sub-keys, no multiples, no historical data, no prior forecast.
  - **Medium**: 3 sheets, ~30 forecast keys, some sub-keys, some multiples, some percOfKeys, one prior forecast, typical historical data.
  - **Large**: 5+ sheets, 100+ forecast keys, comprehensive sub-keys, TTM/NTM keys, both formula families (`H` and `F`) with multi-parameter formulas, percOfKeys, multiples with both `aFK` and `bFK` associations, peer group data, prior forecast, all report tables populated.
- **TD-3**: Fixtures shall exercise every edge case listed in the edge-cases table above via at least one of the three shapes.

### Snapshot equivalence tests

- **T-1 (per-loader snapshot equivalence against the frozen legacy oracle)**: the 4 refactored loaders (FR-2) shall be proven equivalent to an **external frozen legacy copy**, not an in-file duplicate. Setup:
  1. Before any flattening work begins, copy the 4 legacy loader functions verbatim into a new file `apps/api/src/api/data/forecast/__legacy__/perf-394.ts`. Copy every helper, constant, and attribute-list referenced by those functions into the same file (e.g. `ForecastKeyDefaultAttributes`, `KeyDefaultAttributes`, any local helpers). The goal is zero shared code between `__legacy__/perf-394.ts` and the live `forecast/index.ts` after the rewrite. Model imports stay shared (models are not being rewritten).
  2. Add `/* eslint-disable */` at the top of `__legacy__/perf-394.ts` and a header comment explaining: "Frozen copy of pre-perf-394 loaders. Do not edit. Used by (a) the `FORECAST_FLAT_LOADERS_ENABLED=false` fallback path defined in NFR-3, and (b) the T-1 equivalence tests. Will be deleted in the cleanup PR after the prod soak completes."
  3. In the live `forecast/index.ts`, the 4 loader functions check `process.env.FORECAST_FLAT_LOADERS_ENABLED !== 'false'` at entry; if the flag is explicitly `'false'`, they delegate to the corresponding `__legacy__/perf-394.ts` function and return its result verbatim. Otherwise they run the new flat implementation. This gives NFR-3 its runtime rollback.
  4. In the T-1 tests, import both the new flat implementation and the `__legacy__` copy directly (not through the flag), call both against the same seeded forecast, and assert `JSON.parse(JSON.stringify(legacy))` deep-equals `JSON.parse(JSON.stringify(new))`. The JSON round-trip normalises Sequelize model instances to plain objects, which is the right equivalence boundary for the frontend HTTP serialization path. In-process calc-engine equivalence is covered by T-3.
  5. T-1 runs against all 3 fixture shapes (small, medium, large).
  6. **T-1 is not deleted after merge.** It stays green throughout the soak period as a continuous regression guard. It is only deleted in the follow-up cleanup PR when the `__legacy__/perf-394.ts` file itself is deleted. That cleanup PR is gated on T-6 (below) being green, which pins the legacy behavior to committed JSON and survives the legacy removal.

- **T-2 (bundle endpoint parity test)**: A test shall:
  1. Seed a fixture forecast.
  2. Call `POST /v1/forecasts/get-forecast-bundle` once.
  3. Call each of the 20 existing individual endpoints in turn, collecting their responses.
  4. Assert that for each of the 20 keys in `bundle`, the JSON value deep-equals the individual endpoint's payload (minus the outer `success` envelope of the individual response).
  5. Run against all 3 fixture shapes.

### Correctness tests

- **T-3 (calculation engine correctness)**: the most robust option. Three stacked guards.

  **T-3a (multi-fixture top-level equivalence)**: run the calc engine against all 3 fixture shapes (small, medium, large), both via the frozen `__legacy__/perf-394.ts` loaders and via the new flat implementation, and assert the top-level computed outputs (DCF value, WACC, multiples-derived values, any other top-level computed fields) match. Uses the **canonical numeric comparator** defined in T-3b. Runs in the same PR as T-1 and is permanent (survives the cleanup PR, same as T-6, because it stops using `__legacy__` once T-3c's pinned snapshots take over).

  **T-3b (canonical numeric comparator)**: a single shared helper `assertDeepEqualWithTolerance(actual, expected, { tolerance: 1e-9 })` used by T-3a and T-3c. Rules:
  - For `number` values: if both finite, assert `Math.abs(actual - expected) < tolerance`. If either is `NaN`, `Infinity`, or `-Infinity`, require strict equality.
  - For `string`, `boolean`, `null`, `undefined`: strict `===` equality.
  - For arrays: length check, then element-wise recurse with index preserved.
  - For plain objects: key-set equality, then recurse per key.
  - For Sequelize model instances: normalise via `JSON.parse(JSON.stringify(x))` before comparison (same boundary as T-1).
  - On mismatch, the error message shall include the dotted path to the failing field (e.g. `bundle.forecast.ForecastSheets[0].ForecastKeys[5].fcastSum1.value`). This is cheap to implement and saves hours when a test fails.
  - Committed at `apps/api/src/__tests__/helpers/canonicalEqual.ts` so other tests can reuse it.

  **T-3c (full publish output pinning)**: run the existing publish/calculation path end-to-end against the **medium fixture** and pin the entire computed state (every DCF cell, every multiples cell, every valuation row, every derived value the calc engine produces, not just top-level summaries). The pinned snapshot is committed at `apps/api/src/api/data/forecast/__fixtures__/perf-394/publish__medium.json`. Generated once, before the refactor, from the legacy code path. Test asserts new flat implementation + calc engine + publish produces a result that matches the snapshot under T-3b's comparator. This is the end-to-end regression net for the calc path; any drift in any computed cell fails the test with a dotted path pointing at the exact field.

  T-3c only pins one fixture because full publish snapshots are large; the small and large fixtures are covered by T-3a at the top-level granularity which catches summary drift cheaply. If a future regression is subtle enough to slip past T-3a's top-level pins and only shows up in a non-medium shape, we add more T-3c snapshots in a follow-up.

  All three (T-3a, T-3b helper, T-3c) survive the cleanup PR. T-3a switches from comparing "legacy vs new" to comparing "new vs pinned snapshots from T-6" when `__legacy__/perf-394.ts` is deleted.

- **T-4 (existing route tests untouched)**: `apps/api/src/api/routes/__tests__/forecasts.test.ts` and any other forecast route test shall pass without modification. The PR may add new test files and add new cases to existing describe blocks, but shall not delete or change any existing assertion.

### Permanent canonical snapshots

- **T-6 (canonical JSON snapshots)**: Before any code is changed in `forecast/index.ts`, the implementer shall run each of the 4 loaders in FR-2 against all 3 fixture shapes (small, medium, large) and commit the `JSON.parse(JSON.stringify(result))` output as files under `apps/api/src/api/data/forecast/__fixtures__/perf-394/`:
  - `_getSingleForecast__small.json`, `_getSingleForecast__medium.json`, `_getSingleForecast__large.json`
  - `_getForecastedData__{small,medium,large}.json`
  - `_getForecastedSubData__{small,medium,large}.json`
  - `_getFPercOfKeyData__{small,medium,large}.json`

  Total: 12 pinned JSON files. These are produced before any rewrite, so they reflect the true pre-refactor output of the live code at the moment the PR started.

  T-6 runs the **new** flat implementation against each fixture and asserts `JSON.parse(JSON.stringify(result))` deep-equals the committed JSON. Uses a canonical key-sorted serializer (see Cheap safety nets in OQ-9's resolution / Hygiene AC) so key-order differences don't fail the test.

  T-6 survives the cleanup PR that deletes `__legacy__/perf-394.ts`. It is the permanent regression net for the 4 loader shapes going forward, even after the legacy code is gone. Any future change to the loaders has to either match these snapshots or intentionally update them with reviewer approval.

### Performance sanity test

- **T-5 (bundle endpoint perf sanity)**: A test shall time the bundle endpoint against the large fixture and assert it completes in under 10 seconds against the local in-process test DB. This is a crude regression tripwire, not a true perf target; the real perf validation is before/after on staging (see NFR-2). **Test pool caveat**: the test Sequelize pool is `max: 5`, while prod is `max: 20`. The 22-way bundle fan-out will serialize some queries in the test runner that run parallel in prod, so T-5 timing is a pessimistic upper bound relative to prod. Implementer shall either raise the test pool for this suite or document the gap in the test file's header comment.

### Manual verification before merge

- **M-1**: Implementer deploys to staging, opens the forecast screen on the largest real forecast available, and records a before/after wall-clock load time in the PR body. "Before" is measured by reverting staging to main for the sample; "after" is the PR branch deployed to staging.
- **M-2**: Implementer clicks through the forecast screen and verifies: all tabs render, numbers match, charts render, formulas edit and save correctly, no console errors.
- **M-3**: Implementer runs one full calculation (DCF publish or equivalent) on staging and spot-checks the resulting values against a known-good prior calculation of the same forecast.

## Error handling

| Error | HTTP | Response | Recovery |
|---|---|---|---|
| Missing / invalid `forecastId` in body | 400 | `{ error: "Invalid input", details: <zod flatten> }` | Caller fixes payload |
| Unauthenticated | 401 | `{ error: "Unauthorized" }` | User logs in |
| Wrong role | 403 | `{ error: "Insufficient permissions" }` | User requests access |
| Forecast not found / no access | 401 | `{ success: false, status: 401, forecast: null }` (matches legacy `/get-single-forecast`) | Frontend shows not-found state |
| Any non-forecast sub-loader throws | 200 | `{ success: true, bundle: { ..., <failedKey>: { success: false }, ... } }` + Sentry breadcrumb | Screen renders degraded, user sees empty widget for failed section (current behavior) |
| DB unreachable (all sub-loaders throw) | 200 | `{ success: true, bundle: { all 22 keys: { success: false } } }` | Screen renders empty; Sentry alerts ops |
| Handler itself throws (unexpected) | 500 | `{ error: "Internal server error" }` via generic handler | Ops issue |

## Out of scope

- Moving the VPS closer to Postgres (being tracked as a separate immediate mitigation).
- Caching layer for forecast data (follow-up, to be revisited once the data shapes are sane).
- Rewriting the calculation engine.
- Refactoring loaders in `forecast/index.ts` outside the 5 listed in FR-2.
- Changing the 20 individual endpoints' public contracts. They must continue to work unchanged for non-forecast-screen callers.
- Frontend refactoring of downstream consumers. Consumers of the 20 payloads should receive the same shapes via the bundle and require no logic changes.
- Sentry tracing on endpoints other than the new bundle endpoint.
- Test fixtures generated from prod data dumps.

## Open questions

Decisions the implementer or product owner must resolve before merge. The first three are small audits; OQ-4 through OQ-9 are architectural decisions surfaced by the adversarial review of this spec and left unresolved on purpose.

- **OQ-1**: Are there callers of any of the 23 individual forecast endpoints **other than** the forecast-screen loader? Candidates to audit: report generation jobs, scheduled tasks, the `apps/frontend` report page, external scripts. The PR shall include a short audit section (grep the frontend + any cron/job code) and list the other callers. If none, a follow-up PR can retire the individual endpoints; that retirement is out of scope for this PR regardless.
- **OQ-2**: Which sub-loaders have implicit ordering contracts? `_getSingleForecast` has no explicit `order` on its per-sheet `ForecastKeys.findAll` and `ForecastMultiples.findAll` calls, so the current ordering is "whatever Postgres returns," which the frontend then `.sort(...)`s in several places (see `apps/frontend/src/Components/Forecast/services/fetchForecastData.ts:461+`). Implementer shall enumerate every implicit contract in the PR body before merging.
- **OQ-3**: Are there N+1 issues lurking in the 23 sub-loaders beyond those called out in FR-2? If so, the bundle endpoint inherits their slowness. These are intentionally out of scope for this PR, but the implementer shall note any additional loaders that look expensive in the PR body as follow-up candidates.

### Architectural decisions (from adversarial review)

- **OQ-4: Rollback story.** **Resolved → (b)** two env flags. See NFR-3 for the full contract. Follow-up PR to remove the flags and legacy code after 7-14 day soak.

- **OQ-5: Parallelism primitive.** **Resolved → (b)** `Promise.allSettled` with per-key wrapping. See FR-6 step 2 for the exact pattern. Partial-failure policy still to be decided in OQ-6.

- **OQ-6: Bundle failure policy.** **Resolved → (a)** match current tolerance. Failed sub-loaders appear in the bundle as `{ success: false }`, top-level response stays `{ success: true }` with HTTP 200. Every rejection logged + Sentry breadcrumb. See FR-6 steps 3-6 for the exact contract. The only non-200 case is the step-1 forecast-existence short-circuit.

- **OQ-7: T-1 oracle strategy.** **Resolved → (b) + (c)**. External frozen legacy file at `apps/api/src/api/data/forecast/__legacy__/perf-394.ts` does double duty as the OQ-4 flag fallback and the T-1 oracle. See T-1 for details. Additionally, T-6 pins canonical JSON snapshots of the 4 loaders × 3 fixture shapes as a permanent regression net that survives the cleanup PR. Cleanup PR is gated on T-6 being green before `__legacy__/perf-394.ts` is deleted.

- **OQ-8: T-3 pinning strategy.** **Resolved → (a) + (b) + (c)**. Most robust option. Three stacked guards: multi-fixture top-level equivalence (T-3a), canonical numeric comparator with 1e-9 tolerance and dotted-path error messages (T-3b), and full publish-output pinning on the medium fixture (T-3c). All survive the cleanup PR.

- **OQ-9: Reference data in the bundle.** **Resolved → (a)** exclude. The bundle stays focused on the 23 forecast-scoped keys in FR-5. Reference data (`getAllReportTypes`, `getLanguages`, `getMultipleTypes`, `getQuarters`, `getBetas`, `getCountries`, `getMaturities`, `getIndexTypes`, `getYearSummationTypes`) continues to be fetched in parallel by the frontend via the existing per-endpoint calls. These are small and fast, already parallel, and not on the slow path. If future measurement shows they are a meaningful fraction of screen load, a follow-up PR can add a dedicated `/v1/reference/get-bundle` endpoint with aggressive HTTP caching. Not in scope for this spec.

## Acceptance criteria (merge gate)

Correctness:
- [ ] FR-1 through FR-9 implemented.
- [ ] `__legacy__/perf-394.ts` committed with the frozen pre-refactor copies of the 4 loaders + their helper/constant dependencies (T-1 / NFR-3).
- [ ] T-1 passes for all 4 loaders × 3 fixture shapes.
- [ ] T-2 passes for all 3 fixture shapes.
- [ ] T-3a passes (multi-fixture top-level calc-engine equivalence).
- [ ] T-3b canonical numeric comparator committed at `apps/api/src/__tests__/helpers/canonicalEqual.ts` and used by T-3a and T-3c.
- [ ] T-3c passes (full publish-output snapshot match on medium fixture).
- [ ] T-4: existing forecast route tests pass unchanged.
- [ ] T-5 passes (crude perf tripwire).
- [ ] T-6 passes: 12 canonical JSON snapshots committed and matched by the new flat implementation.
- [ ] M-1: before/after staging timing recorded in PR body.
- [ ] M-2: manual staging click-through with no visible regressions.
- [ ] M-3: one publish/calculation on staging produces the expected values.

Hygiene:
- [ ] `__legacy__/perf-394.ts` is reachable via `FORECAST_FLAT_LOADERS_ENABLED=false` on the api and the frontend falls back via `FORECAST_BUNDLE_ENABLED=false` (NFR-3).
- [ ] Sentry tracing active on `/v1/forecasts/get-forecast-bundle`.
- [ ] `turbo run build lint typecheck test` passes.
- [ ] PR body lists any other callers of the 23 individual endpoints found by the OQ-1 audit.
- [ ] PR body notes any ordering contracts found per OQ-2.
- [ ] PR body notes any follow-up N+1 candidates per OQ-3.
- [ ] PR body links a follow-up issue for the cleanup PR that deletes `__legacy__/perf-394.ts`, the two env flags, and T-1, gated on T-6 remaining green and the 7-14 day soak completing.
