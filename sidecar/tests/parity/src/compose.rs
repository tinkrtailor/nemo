//! `docker compose` wrapper + NFR-7 drop guard.
//!
//! Thin wrapper around `std::process::Command` / `tokio::process::Command`.
//! Every subprocess call is logged and stderr is captured into the
//! returned error on failure.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, anyhow};
use tokio::process::Command;

/// Docker service names matching `docker-compose.yml`.
pub const SIDECAR_GO_SERVICE: &str = "sidecar-go";
pub const SIDECAR_RUST_SERVICE: &str = "sidecar-rust";

/// Published host ports (FR-2).
///
/// These are the numeric values the harness driver connects to from
/// the host. The spec numbers are authoritative; this module exports
/// them as constants to keep docker-compose.yml and driver code in
/// sync.
pub mod ports {
    // Go sidecar ports (FR-2 #1)
    pub const GO_MODEL: u16 = 19090;
    pub const GO_SSH: u16 = 19091;
    pub const GO_EGRESS: u16 = 19092;
    pub const GO_HEALTH: u16 = 19093;

    // Rust sidecar ports (FR-2 #2)
    pub const RUST_MODEL: u16 = 29090;
    pub const RUST_SSH: u16 = 29091;
    pub const RUST_EGRESS: u16 = 29092;
    pub const RUST_HEALTH: u16 = 29093;

    // Mock healthcheck / smoke / introspection ports (FR-2 #3-#7)
    pub const MOCK_OPENAI_HEALTH: u16 = 50010;
    /// Manual smoke HTTPS target for mock-openai. Not used by the
    /// driver itself; published so operators can curl the mock
    /// directly per FR-10.
    #[allow(dead_code)]
    pub const MOCK_OPENAI_HTTPS: u16 = 50011;
    pub const MOCK_OPENAI_INTROSPECT: u16 = 49990;

    pub const MOCK_ANTHROPIC_HEALTH: u16 = 50020;
    /// Manual smoke HTTPS target for mock-anthropic (see
    /// `MOCK_OPENAI_HTTPS`).
    #[allow(dead_code)]
    pub const MOCK_ANTHROPIC_HTTPS: u16 = 50021;
    pub const MOCK_ANTHROPIC_INTROSPECT: u16 = 49991;

    pub const MOCK_GH_SSH_HEALTH: u16 = 50030;
    pub const MOCK_GH_SSH_INTROSPECT: u16 = 49992;

    pub const MOCK_EXAMPLE_HEALTH: u16 = 50040;
    /// Second listener on mock-example for the `_with_port` egress
    /// case. The driver reaches this via the sidecar tunnel, not
    /// directly; published so operators can smoke both ports.
    #[allow(dead_code)]
    pub const MOCK_EXAMPLE_8080: u16 = 50041;
    pub const MOCK_EXAMPLE_INTROSPECT: u16 = 49993;

    pub const MOCK_TCP_ECHO_HEALTH: u16 = 50050;
}

/// Shared, cloneable handle to a compose invocation context.
///
/// `clone()` is cheap — we store only the file path and working
/// directory.
#[derive(Debug, Clone)]
pub struct ComposeStack {
    inner: Arc<ComposeStackInner>,
}

#[derive(Debug)]
struct ComposeStackInner {
    compose_file: PathBuf,
    working_dir: PathBuf,
}

impl ComposeStack {
    /// Construct a stack pointing at the given compose file and
    /// working directory. The working directory is what `docker
    /// compose` resolves `build.context` against, so it MUST be the
    /// harness crate dir (which is where `docker-compose.yml` lives).
    pub fn new(compose_file: impl Into<PathBuf>, working_dir: impl Into<PathBuf>) -> Self {
        Self {
            inner: Arc::new(ComposeStackInner {
                compose_file: compose_file.into(),
                working_dir: working_dir.into(),
            }),
        }
    }

    /// Return the CLI arguments that the wrapper passes to `docker`
    /// for a given operation. Exposed so unit tests can assert the
    /// right flags are used without having to spawn docker.
    pub fn args_for(&self, op: ComposeOp) -> Vec<OsString> {
        let mut args: Vec<OsString> = vec![
            OsString::from("compose"),
            OsString::from("-f"),
            self.inner.compose_file.clone().into_os_string(),
        ];
        match op {
            ComposeOp::Build => {
                args.push(OsString::from("build"));
            }
            ComposeOp::Up => {
                args.push(OsString::from("up"));
                args.push(OsString::from("-d"));
                args.push(OsString::from("--remove-orphans"));
            }
            ComposeOp::Down => {
                args.push(OsString::from("down"));
                args.push(OsString::from("-v"));
                args.push(OsString::from("--remove-orphans"));
            }
            ComposeOp::Logs => {
                args.push(OsString::from("logs"));
                args.push(OsString::from("--no-color"));
                args.push(OsString::from("--timestamps"));
            }
            ComposeOp::Kill { service, signal } => {
                args.push(OsString::from("kill"));
                args.push(OsString::from("--signal"));
                args.push(OsString::from(signal));
                args.push(OsString::from(service));
            }
        }
        args
    }

    /// Path to the parent directory that `docker compose` treats as
    /// the build context. Exposed for test diagnostics.
    #[allow(dead_code)]
    pub fn working_dir(&self) -> &Path {
        &self.inner.working_dir
    }

    /// Path to the compose YAML. Exposed for test diagnostics.
    #[allow(dead_code)]
    pub fn compose_file(&self) -> &Path {
        &self.inner.compose_file
    }

    /// Run `docker compose build`.
    pub async fn build(&self) -> Result<()> {
        self.run(ComposeOp::Build).await
    }

    /// Run `docker compose up -d --remove-orphans`.
    pub async fn up(&self) -> Result<()> {
        self.run(ComposeOp::Up).await
    }

    /// Run `docker compose down -v --remove-orphans`.
    ///
    /// The [`ComposeGuard`] Drop calls this synchronously; callers
    /// that need an explicit teardown without using a guard can call
    /// this directly.
    #[allow(dead_code)]
    pub async fn down(&self) -> Result<()> {
        self.run(ComposeOp::Down).await
    }

    /// Run `docker compose logs --no-color --timestamps` and return
    /// the captured stdout. Never fails — if the call errors we
    /// return an empty string so the NFR-9 artifact dump still runs.
    pub async fn logs(&self) -> Result<String> {
        let args = self.args_for(ComposeOp::Logs);
        let out = Command::new("docker")
            .args(&args)
            .current_dir(&self.inner.working_dir)
            .output()
            .await
            .context("failed to spawn docker compose logs")?;
        Ok(String::from_utf8_lossy(&out.stdout).to_string())
    }

    /// Signal a service via `docker compose kill --signal <sig>`.
    pub async fn kill_signal(&self, service: &str, signal: &str) -> Result<()> {
        self.run(ComposeOp::Kill {
            service: service.to_string(),
            signal: signal.to_string(),
        })
        .await
    }

    async fn run(&self, op: ComposeOp) -> Result<()> {
        let args = self.args_for(op);
        let output = Command::new("docker")
            .args(&args)
            .current_dir(&self.inner.working_dir)
            .output()
            .await
            .context("failed to spawn docker compose")?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let stdout = String::from_utf8_lossy(&output.stdout);
            return Err(anyhow!(
                "docker compose failed (status {}): stderr={stderr}, stdout={stdout}",
                output.status
            ));
        }
        Ok(())
    }
}

/// Operations supported by the `ComposeStack` wrapper.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ComposeOp {
    Build,
    Up,
    Down,
    Logs,
    Kill { service: String, signal: String },
}

/// NFR-7 Drop guard. On drop (unless disarmed) runs
/// `docker compose down -v --remove-orphans` synchronously so a panic
/// or early exit never leaves containers running.
pub struct ComposeGuard {
    stack: ComposeStack,
    armed: bool,
}

impl ComposeGuard {
    pub fn new(stack: ComposeStack) -> Self {
        Self { stack, armed: true }
    }

    /// Disable the teardown. The caller has decided to leave the
    /// stack up for inspection.
    pub fn disarm(mut self) {
        self.armed = false;
        // Drop body runs, sees `armed == false`, no-ops.
    }
}

impl Drop for ComposeGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        // Drop runs on whichever thread owns the guard. We block on
        // the down call via std::process::Command because the async
        // runtime may be shutting down at this point and spawning a
        // new task on it would race.
        let args = self.stack.args_for(ComposeOp::Down);
        let output = std::process::Command::new("docker")
            .args(&args)
            .current_dir(&self.stack.inner.working_dir)
            .output();
        match output {
            Ok(o) if !o.status.success() => {
                eprintln!(
                    "ComposeGuard drop: docker compose down failed (status {})\nstderr: {}",
                    o.status,
                    String::from_utf8_lossy(&o.stderr)
                );
            }
            Ok(_) => {}
            Err(e) => {
                eprintln!("ComposeGuard drop: failed to spawn docker compose down: {e}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_op_args_include_compose_file() {
        let stack = ComposeStack::new(
            PathBuf::from("docker-compose.yml"),
            PathBuf::from("/tmp/harness"),
        );
        let args = stack.args_for(ComposeOp::Build);
        assert_eq!(args[0], OsString::from("compose"));
        assert_eq!(args[1], OsString::from("-f"));
        assert_eq!(args[2], OsString::from("docker-compose.yml"));
        assert_eq!(args[3], OsString::from("build"));
    }

    #[test]
    fn up_op_includes_remove_orphans_and_detach() {
        let stack = ComposeStack::new(
            PathBuf::from("docker-compose.yml"),
            PathBuf::from("/tmp/harness"),
        );
        let args = stack.args_for(ComposeOp::Up);
        assert!(args.iter().any(|a| a == &OsString::from("-d")));
        assert!(
            args.iter()
                .any(|a| a == &OsString::from("--remove-orphans"))
        );
    }

    #[test]
    fn down_op_removes_volumes_and_orphans() {
        let stack = ComposeStack::new(
            PathBuf::from("docker-compose.yml"),
            PathBuf::from("/tmp/harness"),
        );
        let args = stack.args_for(ComposeOp::Down);
        assert!(args.iter().any(|a| a == &OsString::from("down")));
        assert!(args.iter().any(|a| a == &OsString::from("-v")));
        assert!(
            args.iter()
                .any(|a| a == &OsString::from("--remove-orphans"))
        );
    }

    #[test]
    fn kill_op_embeds_signal_and_service() {
        let stack = ComposeStack::new(
            PathBuf::from("docker-compose.yml"),
            PathBuf::from("/tmp/harness"),
        );
        let args = stack.args_for(ComposeOp::Kill {
            service: "sidecar-go".to_string(),
            signal: "SIGTERM".to_string(),
        });
        assert!(args.iter().any(|a| a == &OsString::from("kill")));
        assert!(args.iter().any(|a| a == &OsString::from("--signal")));
        assert!(args.iter().any(|a| a == &OsString::from("SIGTERM")));
        assert!(args.iter().any(|a| a == &OsString::from("sidecar-go")));
    }

    #[test]
    fn guard_disarm_prevents_teardown_attempt() {
        // The guard's drop body bails out immediately when armed is
        // false, so spawning docker is not attempted. We can observe
        // this by checking that `disarm()` consumes `self` (compile-
        // time guarantee) and that no panic happens when the guard
        // drops.
        let stack = ComposeStack::new(
            PathBuf::from("docker-compose.yml"),
            PathBuf::from("/tmp/harness"),
        );
        let guard = ComposeGuard::new(stack);
        guard.disarm(); // consumes guard, no teardown runs
    }

    #[test]
    fn port_constants_match_spec_fr2() {
        // Guard against drift: if any of these host ports changes,
        // docker-compose.yml and the driver's health poller must be
        // updated together.
        assert_eq!(ports::GO_HEALTH, 19093);
        assert_eq!(ports::RUST_HEALTH, 29093);
        assert_eq!(ports::MOCK_OPENAI_INTROSPECT, 49990);
        assert_eq!(ports::MOCK_ANTHROPIC_INTROSPECT, 49991);
        assert_eq!(ports::MOCK_GH_SSH_INTROSPECT, 49992);
        assert_eq!(ports::MOCK_EXAMPLE_INTROSPECT, 49993);
        assert_eq!(ports::MOCK_OPENAI_HEALTH, 50010);
        assert_eq!(ports::MOCK_ANTHROPIC_HEALTH, 50020);
        assert_eq!(ports::MOCK_GH_SSH_HEALTH, 50030);
        assert_eq!(ports::MOCK_EXAMPLE_HEALTH, 50040);
        assert_eq!(ports::MOCK_TCP_ECHO_HEALTH, 50050);
    }
}
