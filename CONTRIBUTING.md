# Contributing to Nautiloop

Nautiloop is open source under Apache 2.0. Contributions are welcome.

## Getting started

```bash
git clone https://github.com/tinkrtailor/nautiloop.git
cd nautiloop
cargo build --workspace
cargo test --workspace
```

### Prerequisites

- Rust (edition 2024, 1.85+)
- Docker with buildx (for image builds)
- Postgres (for integration tests, or use the in-memory store)

### Project layout

```
control-plane/     Rust library + binary (API server + loop engine)
cli/               Rust binary (nemo CLI)
images/            Dockerfiles (control-plane, agent-base, sidecar)
terraform/         Hetzner + k3s provisioning
.nautiloop/prompts/     Agent prompt templates
```

## Development workflow

1. Create a branch: `git checkout -b feat/your-feature`
2. Make changes
3. Run checks:
   ```bash
   cargo clippy --workspace -- -D warnings
   cargo test --workspace
   ```
4. Commit with [conventional commits](https://www.conventionalcommits.org/): `feat(api): add endpoint`
5. Push and open a PR against `main`

## Code standards

- **Clippy clean**: `cargo clippy --workspace -- -D warnings` must pass with zero warnings
- **Tests pass**: `cargo test --workspace` must pass
- **Conventional commits**: all commit messages follow the format `type(scope): description`
- **No secrets**: never commit credentials, API keys, or private keys
- **Complete implementations**: no TODOs, no stubs, no placeholders. If you can't finish it, don't start it.

## Architecture decisions

- **Rust** for control plane and CLI (performance, type safety, single binary)
- **axum** for the API server
- **sqlx** with Postgres for state (compile-time query checking where possible)
- **kube-rs** for Kubernetes API interaction
- **thiserror** for error types (no `unwrap()` in library code)
- **Trait-based mocking** for tests (no mocking frameworks)

## Testing

- Unit tests: inline `#[cfg(test)]` modules
- Integration tests: `tests/` directory in each crate
- State store tests use `MemoryStateStore` (no Postgres required)
- K8s tests use `k8s-openapi` fixtures

## What we look for in PRs

- Does it solve a real problem?
- Is the implementation complete (not partial)?
- Are edge cases handled?
- Do tests cover the new behavior?
- Is the commit history clean and conventional?

## Reporting issues

Open an issue on GitHub. Include:
- What you expected to happen
- What actually happened
- Steps to reproduce
- Nautiloop version (`nemo --version`)
- Relevant logs (`nemo logs <loop_id>`)

## License

By contributing, you agree that your contributions will be licensed under the Apache 2.0 License.
