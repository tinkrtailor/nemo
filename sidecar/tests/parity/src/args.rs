//! CLI arguments for the parity harness driver (FR-20).

use clap::Parser;

/// Nautiloop sidecar parity harness driver.
///
/// Runs the Go sidecar and the Rust sidecar in parallel under docker
/// compose, issues deterministic test inputs to both, and diffs the
/// results. Implements FR-1 through FR-29 of
/// `specs/sidecar-parity-harness.md`.
#[derive(Debug, Clone, Parser)]
#[command(author, version, about, long_about = None)]
pub struct Args {
    /// Run only the cases matching this category. One of:
    /// `model_proxy`, `egress`, `git_ssh`, `health`, `divergence`.
    #[arg(long)]
    pub category: Option<String>,

    /// Run only the single case with this name (matches `name` field
    /// in the corpus JSON).
    #[arg(long)]
    pub case: Option<String>,

    /// Tear down the docker compose stack unconditionally after the
    /// run (even on failure). Without this flag, the stack is left
    /// running on failure so the operator can inspect it.
    #[arg(long, default_value_t = false)]
    pub stop: bool,

    /// Skip `docker compose build`. Useful for fast iteration when
    /// neither the sidecars nor the mock services have changed.
    #[arg(long, default_value_t = false)]
    pub no_rebuild: bool,

    /// Override the CGNAT subnet the parity-net bridge uses. Must be a
    /// valid IPv4 CIDR inside one of the FR-29 whitelisted ranges.
    /// Wraps the `PARITY_NET_SUBNET` environment variable.
    #[arg(long)]
    pub subnet: Option<String>,

    /// Directory containing the corpus JSON files. Defaults to the
    /// `corpus` directory alongside this binary's crate.
    #[arg(long, default_value = "corpus")]
    pub corpus_dir: String,

    /// Path to the docker compose file. Defaults to `docker-compose.yml`
    /// in the harness crate's directory.
    #[arg(long, default_value = "docker-compose.yml")]
    pub compose_file: String,

    /// Emit verbose orchestration logs (one line per phase transition).
    #[arg(long, default_value_t = false)]
    pub verbose: bool,
}
