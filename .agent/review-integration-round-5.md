Starting with the branch diff and changed files, then I’ll inspect only the touched code for runtime regressions.
The branch is substantial and already went through several rounds, so I’m doing an independent pass on the live Rust diff rather than relying on earlier notes. Focus is only runtime-affecting bugs in the touched code, excluding the intentional `MergedConfig` V1 deferral.
I’ve verified one concrete regression in the live code path. I’m doing a short second sweep around config consumption and branch resolution to make sure there isn’t another runtime bug hiding in the same area before I call the round.
1. High — `cli/src/commands/init.rs:59`, `control-plane/src/config/mod.rs:91`, `control-plane/src/api/handlers.rs:50`, `control-plane/src/loop_engine/driver.rs:1298`
`nemo init` now writes the detected default branch to `[repo].default_branch`, but the live runtime never reads that field. The control plane still resolves the base branch from `NemoConfig.cluster.default_branch` and defaults to `main`. On repos whose real default branch is `master`, `trunk`, etc., `/start` reads specs from the wrong ref, new loop branches fork from the wrong base, PRs target the wrong branch, and diff/test/merge logic also compares against the wrong branch. That is a real runtime regression.

Fix: either wire runtime branch resolution to repo-level `default_branch`, or have `nemo init` write the branch into the config field the runtime actually consumes.

I did not flag the intentional `MergedConfig` V1 deferral. Aside from the branch-resolution bug above, I did not find another high-confidence real runtime bug in this round.
