# Security Policy

## Reporting a Vulnerability

If you discover a security vulnerability in Nemo, please report it responsibly.

**Email:** security@tinkrtailor.com

**Do not** open a public GitHub issue for security vulnerabilities.

## What to include

- Description of the vulnerability
- Steps to reproduce
- Impact assessment
- Suggested fix (if any)

## Response timeline

- **Acknowledgment:** within 48 hours
- **Initial assessment:** within 1 week
- **Fix or mitigation:** depends on severity, but we aim for 30 days for critical issues

## Scope

This policy covers:

- The Nemo control plane (`control-plane/`)
- The CLI (`cli/`)
- The auth sidecar (`images/sidecar/`)
- The Terraform module (`terraform/modules/nautiloop/`)
- Agent base images (`images/base/`)

## Known limitations (V1)

- **Shared API key auth:** V1 uses a single shared API key. All authenticated users have full access. Designed for single-tenant / small-team deployments. Per-engineer RBAC is planned for V2.
- **Session data in agent pods:** Implement/revise jobs mount Claude session data into agent containers for session continuity.
