//! Binary entry point for the Nautiloop auth-sidecar parity harness.
//!
//! Implements the flow described in `specs/sidecar-parity-harness.md`
//! "Driver program structure" section:
//!
//! 1. Parse CLI args (FR-20).
//! 2. Resolve + validate the CGNAT subnet against the FR-29 whitelist.
//! 3. Load the corpus and apply `--category` / `--case` filters.
//! 4. Bring up the docker compose stack (FR-16 / FR-17).
//! 5. Wait for mock + sidecar readiness (FR-17 points 3-4).
//! 6. Run each case, print progress (FR-18).
//! 7. Run `order_hint: "last"` cases after everything else.
//! 8. Summarize, dump artifact log (NFR-9), return non-zero on failure.
//! 9. Drop guard always tears down on panic (NFR-7).
//!
//! This binary never modifies sidecar source. It just talks to running
//! containers through the published host ports and the mock
//! introspection API.

mod args;
mod compose;
mod corpus;
mod diff;
mod health_probe;
mod introspection;
mod normalize;
mod report;
mod result;
mod runner;
mod subnet;
mod tls_client;

use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use clap::Parser;

use crate::args::Args;
use crate::compose::{ComposeGuard, ComposeStack};
use crate::corpus::{Category, CorpusCase, load_corpus, partition_by_order_hint};
use crate::normalize::normalize;
use crate::report::{dump_run_log, print_case_result, print_summary};
use crate::result::{CaseOutcome, RunSummary, SideOutput};
use crate::runner::RunnerContext;

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let args = Args::parse();

    // FR-29: resolve + validate subnet, then export to env for compose.
    let subnet = subnet::resolve_and_validate(args.subnet.as_deref())
        .context("subnet whitelist validation failed (FR-29)")?;
    // SAFETY: `set_var` is only called here, before any child process
    // spawn, and no other thread in this binary writes to environment.
    unsafe {
        std::env::set_var(subnet::SUBNET_ENV_VAR, &subnet);
    }
    tracing::info!(%subnet, "resolved PARITY_NET_SUBNET");

    // Resolve paths relative to the harness crate directory. This
    // binary is expected to be invoked from the repo root OR from the
    // parity harness dir; in either case the corpus + compose paths
    // should resolve to real files.
    let harness_dir = resolve_harness_dir();
    let corpus_path = harness_dir.join(&args.corpus_dir);
    let compose_path = harness_dir.join(&args.compose_file);

    let cases = load_corpus(&corpus_path)
        .with_context(|| format!("loading corpus from {}", corpus_path.display()))?;
    let filtered: Vec<CorpusCase> = cases
        .into_iter()
        .filter(|c| {
            if let Some(cat) = &args.category
                && c.category.as_str() != cat
            {
                return false;
            }
            if let Some(name) = &args.case
                && &c.name != name
            {
                return false;
            }
            true
        })
        .collect();
    if filtered.is_empty() {
        anyhow::bail!(
            "no corpus cases matched --category={:?} --case={:?}",
            args.category,
            args.case
        );
    }
    tracing::info!(count = filtered.len(), "loaded corpus cases");

    // FR-17 step 1-4: compose build/up + health gate.
    let compose = ComposeStack::new(&compose_path, &harness_dir);
    if !args.no_rebuild {
        tracing::info!("running docker compose build");
        compose
            .build()
            .await
            .context("docker compose build failed")?;
    } else {
        tracing::info!("--no-rebuild set; skipping docker compose build");
    }
    tracing::info!("running docker compose up -d");
    compose.up().await.context("docker compose up failed")?;
    let guard = ComposeGuard::new(compose.clone());

    health_probe::wait_mock_health(Duration::from_secs(60))
        .await
        .context("mock services did not become healthy in 60s (FR-17 step 3)")?;
    health_probe::wait_sidecar_ready(Duration::from_secs(30))
        .await
        .context("sidecars did not become ready in 30s (FR-17 step 4)")?;

    // Build the shared context for each runner.
    let test_ca_path = harness_dir.join("fixtures/test-ca/ca.pem");
    let harness_tls = tls_client::build_harness_client_config(&test_ca_path)
        .with_context(|| format!("loading harness test CA from {}", test_ca_path.display()))?;
    let ssh_key_path = harness_dir.join("fixtures/go-secrets/ssh-key/id_ed25519");
    let ctx = RunnerContext {
        harness_dir: harness_dir.clone(),
        harness_tls,
        ssh_key_path,
    };

    // Partition into (normal, last) per FR-22 order_hint.
    let (rest, last) = partition_by_order_hint(&filtered);

    let mut outcomes: Vec<CaseOutcome> = Vec::with_capacity(filtered.len());
    for case in rest {
        let outcome = run_case(case, &ctx).await;
        print_case_result(&outcome);
        outcomes.push(outcome);
    }
    for case in last {
        let outcome = run_case(case, &ctx).await;
        print_case_result(&outcome);
        outcomes.push(outcome);
    }

    let summary = RunSummary::from_outcomes(&outcomes);
    print_summary(&summary);

    // NFR-9: always dump the run log.
    let log_path = harness_dir.join("harness-run.log");
    let docker_logs = compose.logs().await.unwrap_or_default();
    dump_run_log(&log_path, &summary, &outcomes, &docker_logs)
        .context("writing harness-run.log")?;

    // FR-17 step 6: if --stop OR all tests passed, tear down.
    if args.stop || summary.all_passed() {
        tracing::info!("tearing down docker compose stack");
        drop(guard);
    } else {
        tracing::warn!("leaving docker compose stack up for inspection (use --stop to override)");
        guard.disarm();
    }

    if !summary.all_passed() {
        std::process::exit(1);
    }
    Ok(())
}

/// Run a single case, catching any runner error and turning it into a
/// failed outcome so the run never aborts mid-corpus.
async fn run_case(case: &CorpusCase, ctx: &RunnerContext) -> CaseOutcome {
    let start = Instant::now();
    match runner::dispatch(case, ctx).await {
        Ok((mut go, mut rust)) => {
            normalize(&mut go, &case.normalize);
            normalize(&mut rust, &case.normalize);
            let diff = diff::diff_sides(&go, &rust);
            let expected_parity = case.expected_parity;
            let passed = if expected_parity {
                diff.is_empty()
            } else {
                // Divergence cases must disagree in the direction
                // described by the divergence descriptor. The runner
                // already produced `go` and `rust` outputs that
                // encode the comparison verdict; if those side
                // outputs are identical for a divergence case,
                // that's a failure (Go got fixed or Rust regressed).
                !diff.is_empty()
            };
            if passed {
                CaseOutcome::pass(
                    &case.name,
                    case.path.to_string_lossy(),
                    expected_parity,
                    go,
                    rust,
                    start.elapsed(),
                    divergence_note(case),
                )
            } else {
                CaseOutcome::fail(
                    &case.name,
                    case.path.to_string_lossy(),
                    expected_parity,
                    go,
                    rust,
                    start.elapsed(),
                    if expected_parity {
                        diff
                    } else {
                        format!(
                            "divergence case matched; expected Go and Rust to differ.\n{}",
                            diff
                        )
                    },
                )
            }
        }
        Err(e) => CaseOutcome::fail(
            &case.name,
            case.path.to_string_lossy(),
            case.expected_parity,
            SideOutput::default(),
            SideOutput::default(),
            start.elapsed(),
            format!("runner error: {e:?}"),
        ),
    }
}

fn divergence_note(case: &CorpusCase) -> String {
    match (&case.category, &case.divergence) {
        (Category::Divergence, Some(d)) => format!(
            "divergence: {} | go={} | rust={}",
            d.description, d.go_expected, d.rust_expected
        ),
        _ => String::new(),
    }
}

/// Resolve the harness directory — the folder containing
/// `Cargo.toml` for this crate. This is `env!("CARGO_MANIFEST_DIR")`
/// at compile time so the binary can be invoked from any working
/// directory and still find its fixtures.
fn resolve_harness_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}
