# rust-secrets/ (TEST-ONLY)

**TEST-ONLY. NEVER COPY OUTSIDE THIS DIRECTORY. NEVER USE IN PRODUCTION.**

Mirror of `../go-secrets/`. See that README for the full rationale.

Mounted at `/secrets/` on the `sidecar-rust` container. Identical to
`go-secrets/` in content so both sidecars authenticate as the same
client against `mock-github-ssh` and present the same model-provider
credentials to the mock OpenAI / Anthropic upstream targets.
