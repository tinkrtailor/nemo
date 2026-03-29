Read all production Rust source files in `control-plane/src` and `cli/src` (35 files total). Not clean; I found 6 real production bugs.

- High — retry/resume can recreate the same K8s Job name and fail with `AlreadyExists`. `control-plane/src/k8s/job_builder.rs:43` uses `retry_count` directly, while first retry/resume bumps `retry_count` from `0` to `1` in `control-plane/src/loop_engine/driver.rs:795`, `control-plane/src/loop_engine/driver.rs:823`, and `control-plane/src/loop_engine/driver.rs:925`; both initial dispatch and first retry end up as `...-t1`. Deletion is background propagation in `control-plane/src/k8s/client.rs:43`, so immediate recreate can race and fail.
- High — `/credentials` accepts engineer values that become invalid Kubernetes Secret names. `/start` allows uppercase and `_` in `control-plane/src/api/handlers.rs:27`, but `/credentials` builds `nemo-creds-{engineer}` directly in `control-plane/src/api/handlers.rs:450`. Names like `Alice` or `alice_dev` are accepted by the API and then rejected by Kubernetes, breaking credential registration.
- Medium — bad client input is surfaced as HTTP 500 instead of 4xx. Validation failures in `control-plane/src/api/handlers.rs:34` and `control-plane/src/api/handlers.rs:411` return `NemoError::Internal`, which maps to 500 in `control-plane/src/error.rs:86`. That turns ordinary caller mistakes into false server incidents.
- Medium — broken CLI config cannot be repaired with `nemo config --set`. `cli/src/main.rs:172` explicitly tries to allow config repair without loading config first, but `cli/src/commands/config.rs:5` immediately calls `load_config()`, and malformed TOML hard-fails in `cli/src/config.rs:44`.
- High — `nemo auth --claude --openai` can report success even when one explicitly requested provider was skipped. Missing credential files only `continue` in `cli/src/commands/auth.rs:78` and never set `any_error`; if another provider succeeds, success is printed from `cli/src/commands/auth.rs:138` and the command exits `Ok(())` at `cli/src/commands/auth.rs:156`.
- Medium — `nemo config --set api_key=` stores an empty API key that breaks all authenticated requests. Empty string is persisted in `cli/src/commands/config.rs:43`, converted into `Authorization: Bearer ` in `cli/src/client.rs:26`, and rejected by server auth in `control-plane/src/api/auth.rs:25`.

If you want, I can do the next pass as:
1. fix-only patch set
2. fix + tests
3. fix + tests + commit on a branch
