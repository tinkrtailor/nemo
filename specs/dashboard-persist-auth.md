# Dashboard Persistent Auth

## Overview

Dashboard users are getting prompted to log in far more often than the spec'd 7-day cookie lifetime should allow. Goal: keep a logged-in dashboard session stable across browser restarts, server restarts, and normal use. Operator types their API key once per week, not once per session.

## Baseline

Main at PR #174 merge. `control-plane/src/api/dashboard/handlers.rs` sets `nautiloop_api_key` (HttpOnly, SameSite=Strict, Max-Age=7 days) and `nautiloop_engineer` (same attributes) on successful login. Observed: operator is prompted to log in repeatedly across a few-hour window on the same browser without closing the tab.

Likely causes (from operator report + diagnostic run on 2026-04-20):
- Cookie rejected by browser due to `Secure` flag on non-TLS localhost (browser drops the cookie silently) — partially mitigated but not verified across all browsers.
- API key rotated by server restart (the login form validates against `NAUTILOOP_API_KEY` env var; if the cluster regenerates on restart, old cookies get stale-rejected).
- Login session tied to a CSRF token that expires early (`nautiloop_login_csrf` Max-Age=86400 is fine; could be another cookie with a short TTL).

## Problem Statement

### Problem 1: Auth prompt spam defeats the "phone dashboard" pitch

Core value of the dashboard is "open on phone, glance, close." If every glance requires re-entering an API key, the user stops opening it. Phone typing an API key through a Tailscale-reached HTTPS form is worst-case ergonomic.

### Problem 2: Server restarts invalidate all sessions

Deploying a new control-plane image (which we did 6+ times today) invalidates every active dashboard session because the API key is compared against an env var that survives restart... BUT any server-side state tied to the cookie (nothing today, actually) would also flush. Need to verify the current symptom isn't a restart-stale-cookie issue.

### Problem 3: Login flow doesn't explain WHY

When auth is required, the login page says nothing about why. Operator with a 10-min-old session sees the login form and assumes the system is broken.

## Functional Requirements

### FR-1: Cookie lifecycle verification + extension

**FR-1a.** Verify (via browser DevTools inspection on Chrome, Safari, Firefox on both macOS and iOS) that:
- `nautiloop_api_key` cookie is set on login
- Stays in the browser's cookie store for 7 days
- Is sent on every `/dashboard/*` request

If any browser rejects the cookie, fix by adjusting the flags (FR-2).

**FR-1b.** Bump Max-Age from 7 days (604800s) to 30 days (2592000s). The API key doesn't expire server-side; 7 days was a conservative default. 30 matches what the CLI's `api_key` persistence does (indefinite until changed).

**FR-1c.** Add a `Max-Age` check on every authenticated request: if the cookie is more than 20 days old, issue a refreshed Set-Cookie with a new Max-Age so long-active sessions sliding-window extend. Prevents the "logged in for a month, suddenly logged out in the middle of a loop" cliff.

### FR-2: Localhost cookie attributes

**FR-2a.** When `Host: localhost` or `127.0.0.1` or `[::1]`, OR when the request's scheme is `http` (not `https`), OR when the server's configured bind address is a loopback interface:
- `Secure` flag is OMITTED from Set-Cookie (so browsers accept it over plain HTTP).
- `SameSite=Strict` is kept (CSRF protection works over any scheme).
- `HttpOnly` kept (XSS protection independent of transport).

**FR-2b.** In production (non-localhost bind, HTTPS in front), the `Secure` flag IS set. Detection is via the request's `Host:` header and `X-Forwarded-Proto` if behind a TLS-terminating reverse proxy.

**FR-2c.** A startup log line reports which cookie attributes the server will use based on the bind config: `Dashboard cookies: HttpOnly=true SameSite=Strict Secure=<true|false>`.

### FR-3: Login-prompt context

**FR-3a.** When redirecting a request without cookies to `/dashboard/login`, include a `?reason=<code>` query param:
- `reason=new` — fresh visitor, no cookie at all
- `reason=expired` — cookie present but server rejected it (API key mismatch, stale from env var rotation)
- `reason=missing` — HAD a cookie earlier in the session but it's gone (cleared by browser or user)
- `reason=invalid` — cookie present but malformed

**FR-3b.** The login page reads `reason` and displays a one-line banner:
- `new`: "Welcome. Enter your API key to view the dashboard."
- `expired`: "Your session expired. Log in again to continue." 
- `missing`: "Session cookies are missing. Check your browser's cookie settings, then log in."
- `invalid`: "Session cookie is malformed. Log in again."

**FR-3c.** No info-leak: the banner never tells the user whether the API key they just tried was wrong; it only confirms re-auth. (Existing behavior, preserved.)

### FR-4: Persistent CSRF token

**FR-4a.** The login-form CSRF (`nautiloop_login_csrf`) currently uses Max-Age=86400 (24h). Fine — but verify the POST flow doesn't accidentally clear it on failure in a way that forces a page reload.

**FR-4b.** On successful login, explicitly clear `nautiloop_login_csrf` (Max-Age=0) — no longer needed, saves cookie jar space.

### FR-5: Logout explicit + respects return-to

**FR-5a.** The existing `/dashboard/logout` POST clears the session cookies. Keep the logout behavior; this spec doesn't modify it.

**FR-5b.** Logout redirects to `/dashboard/login?reason=loggedout` and the login banner reads: "You've been logged out."

**FR-5c.** Out of scope for this spec: preserving a return-to URL on logout (see earlier dashboard spec). Still out of scope.

## Non-Functional Requirements

### NFR-1: No server-side session state

Cookies remain a stateless wrapper around the API key (not a reference to a server-side session). No Redis, no in-memory store, no database table. This keeps the dashboard scalable and matches the existing CLI auth model.

### NFR-2: Backward compatibility

Existing logged-in users with a valid `nautiloop_api_key` cookie continue to work without re-logging. The Max-Age bump from 7d to 30d takes effect on next login OR next sliding-window refresh, not immediately.

### NFR-3: Tests

- **Unit** (`control-plane/src/api/dashboard/handlers.rs`): localhost bind → Set-Cookie omits `Secure`; production bind → Set-Cookie includes `Secure`. Verify via response-header assertion.
- **Unit**: sliding-window refresh sets a new Max-Age on an active session's request (after 20+ days of cookie age, simulated).
- **Manual cross-browser**: verify cookie persists across browser restarts on Chrome/Safari/Firefox, on macOS and iOS (Tailscale sharing makes this reachable from a real phone).

## Acceptance Criteria

A reviewer can verify by:

1. **Single login lasts days**: log into the dashboard on Chrome. Close the tab. Open a new tab to `/dashboard`. Still logged in. Restart the browser. Still logged in. Repeat on day 3. Still logged in.
2. **Cross-browser**: repeat on Safari, Firefox, iOS Safari over Tailscale. Same result.
3. **Server restart preserves session**: log in. Restart the control-plane deployment. Reload. Still logged in (assuming API key env var is the same, which it is across restarts with the same secret).
4. **Expiry reason surfaces**: manually clear `nautiloop_api_key` cookie but keep `nautiloop_engineer`. Visit `/dashboard`. Redirected to `/dashboard/login?reason=missing` with the "Session cookies missing" banner.
5. **Secure flag correct**: `curl -sI http://localhost:18080/dashboard/login` response headers: Set-Cookie does NOT include `Secure`. Same on a production HTTPS deploy (via `X-Forwarded-Proto: https`): Set-Cookie DOES include `Secure`.
6. **Sliding window**: advance the system clock 25 days (manual in test env). Make a request. Check response Set-Cookie: new Max-Age=2592000 (30 days from now), not the original expiry minus 25 days.

## Out of Scope

- **Multi-factor auth** / WebAuthn / passkeys. Single API key model stays.
- **Remember-me toggle on login form**. Default-on is fine; operators who want ephemeral sessions can clear cookies manually.
- **Server-side session revocation** (logout on one device logs out all). Per-device cookie lifecycle.
- **Per-engineer API keys**. Shared single key model; noted as out-of-scope throughout prior specs.
- **OAuth / SSO integration**. Separate, much-larger spec.

## Files Likely Touched

- `control-plane/src/api/dashboard/handlers.rs` — login success + middleware, with Secure-flag detection, Max-Age bump, sliding window refresh, reason code on redirect.
- `control-plane/src/api/dashboard/auth.rs` — cookie-flag helper (returns the right flag string based on bind/proto).
- `control-plane/src/api/dashboard/templates.rs` — login page renders the reason banner.
- Tests per NFR-3.

## Baseline Branch

`main` at PR #174 merge.
