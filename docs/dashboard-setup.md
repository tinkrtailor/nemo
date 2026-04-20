# Dashboard Setup

The nautiloop dashboard is a mobile-first web UI served by the control plane at `/dashboard`. It provides real-time visibility into loop status, actions (approve/cancel/extend), and fleet-wide metrics.

## Security Model

The dashboard is **as private as the host it runs on**. It does not implement its own authentication system (no user accounts, no SSO, no SAML). Security is inherited from the deployment topology.

### Tailscale (Default / Recommended)

Production Hetzner deployments bind the control plane to a Tailscale IPv4 address (see `terraform/examples/hetzner`). The dashboard is reachable only from devices joined to the tailnet.

To access the dashboard from your phone:

1. Install Tailscale on your phone and join the same tailnet as the nautiloop server.
2. Browse to `https://<nautiloop-ts-ipv4>/dashboard`.
3. Enter the shared API key (same as used by the `nemo` CLI).
4. Bookmark the URL for quick access.

### API Key (Defense in Depth)

The dashboard requires the same API key used by the `nemo` CLI (`NAUTILOOP_API_KEY`). This is set via a login form at `/dashboard/login` and stored as an HttpOnly, SameSite=Strict cookie with a 7-day expiry.

The API key is **defense in depth**, not the sole security boundary. It prevents casual access if someone happens to reach the host, but it is a shared secret — not a per-user credential.

### Do NOT Expose to the Public Internet

**The dashboard must not be exposed to the public internet without an external authentication proxy.**

If your deployment does not use Tailscale or a VPN, you must front the dashboard with an auth proxy such as:

- [oauth2-proxy](https://oauth2-proxy.github.io/oauth2-proxy/) — supports Google, GitHub, Azure AD, and other OAuth2 providers.
- [Authelia](https://www.authelia.com/) — self-hosted SSO with 2FA support.
- [Pomerium](https://www.pomerium.com/) — identity-aware access proxy.

Example nginx configuration with oauth2-proxy:

```nginx
location /dashboard/ {
    auth_request /oauth2/auth;
    error_page 401 = /oauth2/sign_in;
    proxy_pass http://127.0.0.1:3000;
}
```

### What the Dashboard Can Do

An authenticated dashboard user has the same permissions as a `nemo` CLI user with the shared API key:

- View all loops (own and team)
- Approve loops awaiting approval
- Cancel active loops
- Resume paused/failed loops
- Extend failed loops with additional rounds
- View pod introspection data
- Access fleet-wide stats and feed

### RBAC (Future)

Per-engineer API keys with role-based access control (e.g., `admin` vs `viewer`) are not implemented in v1. The current model is appropriate for small, trusted teams where all engineers share a single cluster API key. Strict RBAC is planned for a future release.

## Configuration

The dashboard is enabled by default when the control plane starts. No additional configuration is required beyond setting `NAUTILOOP_API_KEY`.

### Environment Variables

| Variable | Required | Description |
|---|---|---|
| `NAUTILOOP_API_KEY` | Yes | Shared API key for CLI and dashboard authentication |

### nemo.toml Options

The dashboard inherits theme settings from the `[helm]` section:

```toml
[helm]
theme = "dark"  # "dark", "light", or "high-contrast"
```

The dashboard defaults to the system color scheme (`prefers-color-scheme` media query) and falls back to the configured theme.

#### Cookie Secure Flag

By default, the dashboard auto-detects whether to set the `Secure` flag on authentication cookies based on the `bind_addr`: cookies are marked `Secure` unless the server binds to a loopback address (`127.0.0.1`, `localhost`, `::1`).

When running behind Tailscale without TLS termination, the auto-detection may incorrectly set the `Secure` flag (because the bind address is a non-loopback Tailscale IP), which prevents the browser from sending cookies over plain HTTP. Override this with:

```toml
[cluster]
dashboard_secure_cookie = false  # Disable Secure flag for plain-HTTP Tailscale setups
```

Set to `true` to force the `Secure` flag when TLS is terminated externally (e.g., by a reverse proxy) and the auto-detection would otherwise disable it.
