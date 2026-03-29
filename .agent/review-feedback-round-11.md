Not clean. I read all Rust source under `control-plane/src` and `cli/src`.

- High - `control-plane/src/k8s/job_builder.rs:240` sets `GIT_SSH_COMMAND` with `StrictHostKeyChecking=no`, which bypasses the mounted `known_hosts` protection and allows SSH MITM on git traffic.
- High - `control-plane/src/loop_engine/reconciler.rs:65` and `control-plane/src/state/postgres.rs:286` reconcile via `get_active_loops()` with no lease/CAS/lock, so multiple control-plane instances can tick the same loop concurrently and double-dispatch or race state updates.
- High - `control-plane/src/loop_engine/driver.rs:237` accepts unexpected fast-forwards because it only pauses on non-ancestor divergence; that can ingest logs from one job while advancing `current_sha` to a newer external commit.
- Medium - `control-plane/src/k8s/client.rs:125` only reads the last 1000 log lines; verbose jobs can push `NEMO_RESULT` out of the tail and be misclassified as missing output.
- Medium - `control-plane/src/api/handlers.rs:454` treats `/credentials` as success even when `kube_client` is `None`, so Postgres says creds are valid while no K8s Secret exists for jobs to mount.
- Medium - `control-plane/src/git/mod.rs:171` and `control-plane/src/git/mod.rs:181` collapse git failures into `false`/`None`, hiding real repo corruption or transient command failures as “missing”.
- Medium - `cli/src/main.rs:178` and `cli/src/client.rs:25` let API commands run without an `api_key`, producing opaque server auth errors instead of a clear local config failure.
- Medium - `cli/src/commands/init.rs:4` claims monorepo scanning/generation, but always writes a fixed template and never inspects the repo.
- Medium - `cli/src/commands/config.rs:8` falls back to defaults on malformed config for `--set`, then rewrites the file and can silently discard unrelated settings.
- Low - `cli/src/main.rs:174` and `cli/src/commands/config.rs:21` allow `--set` and `--get` together and silently prefer `--get` instead of rejecting conflicting flags.
- Low - `cli/src/commands/logs.rs:18` interpolates `stage` into the query string without URL-encoding, so reserved characters/spaces can corrupt the request.
- Low - `cli/src/config.rs:35` falls back to `./.nemo/config.toml` when `HOME` is unset, which can unexpectedly read/write config in the current directory.

If you want, I can do round 12 as a fix pass or tighten this into a prioritized patch list.
