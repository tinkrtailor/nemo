Not clean. I read all Rust source files under `control-plane/src` and `cli/src`.

- `control-plane/src/k8s/client.rs:144` + `control-plane/src/loop_engine/driver.rs:266` + `control-plane/src/error.rs:77` + `control-plane/src/loop_engine/reconciler.rs:102` — a temporary pod log retrieval failure is treated as `Internal`, and `Internal` is classified as fatal, so the reconciler permanently marks the loop `FAILED`. In production, a brief apiserver/network hiccup after a job succeeds can irreversibly kill an otherwise successful loop.

- `control-plane/src/k8s/job_builder.rs:178` + `control-plane/src/loop_engine/driver.rs:194` — jobs are configured with `ttl_seconds_after_finished = 300`, but if reconciliation later observes `JobStatus::NotFound`, the driver treats that as a hard job failure. Any control-plane outage/restart or reconciliation delay beyond 5 minutes after completion can falsely fail already-finished work.
