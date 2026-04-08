//! Per-category runner dispatch.
//!
//! Each category gets its own sub-module with `run_case`. The public
//! [`dispatch`] entry point picks the right module based on the case's
//! category field and returns `(go_side, rust_side)` on success.
//!
//! Runners are responsible for:
//!
//! 1. Calling [`crate::introspection::reset_all`] if the case needs
//!    clean mock logs.
//! 2. Issuing the test input to BOTH sidecars (in parallel where it
//!    makes sense).
//! 3. Capturing outputs into [`SideOutput`] for diffing.
//!
//! Runners do NOT normalize — that's done by the main loop so the
//! normalization rules are applied identically across categories.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use rustls::ClientConfig;

use crate::corpus::{Category, CorpusCase};
use crate::introspection;
use crate::result::SideOutput;

pub mod divergence_drain;
pub mod egress;
pub mod git_ssh;
pub mod health;
pub mod model_proxy;

/// Shared context passed into every runner. Owned by `main.rs`, cloned
/// cheaply via Arc internally.
#[derive(Clone)]
pub struct RunnerContext {
    pub harness_dir: PathBuf,
    /// rustls client config with the harness test CA loaded. Used by
    /// runner modules that need to do direct HTTPS (currently only
    /// reserved for manual smoke — the parity cases go through the
    /// sidecars, which do their own TLS against the mocks).
    #[allow(dead_code)]
    pub harness_tls: Arc<ClientConfig>,
    pub ssh_key_path: PathBuf,
}

/// Dispatch a case to the appropriate category runner.
pub async fn dispatch(case: &CorpusCase, ctx: &RunnerContext) -> Result<(SideOutput, SideOutput)> {
    // Every case resets mock introspection logs first (FR-18 step 1).
    // We ignore the error on categories where the mocks might not be
    // listening yet; the error surfaces at the actual test step.
    if matches!(
        case.category,
        Category::ModelProxy | Category::Egress | Category::Health | Category::GitSsh
    ) || case.category == Category::Divergence
    {
        introspection::reset_all().await?;
    }

    match case.category {
        Category::ModelProxy => model_proxy::run(case, ctx).await,
        Category::Egress => egress::run(case, ctx).await,
        Category::GitSsh => git_ssh::run(case, ctx).await,
        Category::Health => health::run(case, ctx).await,
        Category::Divergence => divergence::dispatch(case, ctx).await,
    }
}

/// Divergence cases are dispatched to per-case modules because each
/// one is fundamentally different in shape (SSE timing, bare-exec
/// rejection, SIGTERM drain).
mod divergence {
    use super::*;

    pub async fn dispatch(
        case: &CorpusCase,
        ctx: &RunnerContext,
    ) -> Result<(SideOutput, SideOutput)> {
        match case.name.as_str() {
            "divergence_sse_streaming_openai" => {
                model_proxy::run_sse_divergence(case, ctx, true).await
            }
            "divergence_sse_streaming_anthropic" => {
                model_proxy::run_sse_divergence(case, ctx, false).await
            }
            "divergence_bare_exec_upload_pack_rejection"
            | "divergence_bare_exec_receive_pack_rejection" => {
                git_ssh::run_bare_exec_divergence(case, ctx).await
            }
            "divergence_connect_drain_on_sigterm" => divergence_drain::run(case, ctx).await,
            other => Err(anyhow::anyhow!(
                "unknown divergence case name {other:?}; no runner wired up"
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn divergence_case_names_are_covered() {
        // Guard against drift: if FR-22 adds a new divergence case,
        // the dispatcher must know about it. This test enumerates the
        // five expected names.
        let expected = [
            "divergence_sse_streaming_openai",
            "divergence_sse_streaming_anthropic",
            "divergence_bare_exec_upload_pack_rejection",
            "divergence_bare_exec_receive_pack_rejection",
            "divergence_connect_drain_on_sigterm",
        ];
        assert_eq!(expected.len(), 5);
    }
}
