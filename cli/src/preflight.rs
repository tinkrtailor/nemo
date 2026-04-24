//! Client-side preflight checks before submitting a loop.
//!
//! The control plane can submit a loop, dispatch a Job, and let the
//! agent run for the full stage budget before discovering that the
//! engineer's credentials don't actually work. Concrete failure modes
//! we've eaten in v0.7.x:
//!   - GitHub PAT missing: 60-min audit completes cleanly, then PR
//!     creation fails infinitely with `gh auth login` hint.
//!   - OAuth refresh token reused: opencode burns the full deadline
//!     on exponential backoff getting 502s from the sidecar.
//!
//! Preflight catches whatever we can on the engineer's laptop in
//! <500 ms before posting `/start`. A failure prints a precise hint
//! ("run `gh auth login --hostname github.com`", "run `nemo auth
//! --openai`") and returns a non-zero exit so the engineer doesn't
//! lose 60+ minutes of compute to a problem that's a one-line fix.
//!
//! v0.7.14 ships only the `gh auth status` check because that's the
//! one tied to the active production failure (#206 follow-up). The
//! Anthropic + OpenAI pings come in v0.7.15.

use anyhow::Result;
use std::process::Stdio;

/// Result of a single preflight check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckOutcome {
    /// The check passed.
    Ok,
    /// The check failed; render the message to the operator and
    /// abort the submit.
    Fail { hint: String },
    /// The check could not be performed (tool not installed,
    /// platform unsupported); proceed with a warning rather than
    /// blocking. Operators on a clean machine without `gh` installed
    /// shouldn't be locked out of `nemo harden`.
    Skip { reason: String },
}

/// Run all preflight checks and return aggregated outcomes. Caller
/// renders + decides whether to block. Sequential rather than
/// concurrent because the per-check cost is tens of milliseconds and
/// concurrent-with-Tokio adds more setup overhead than it saves.
pub async fn run_all() -> Result<Vec<(&'static str, CheckOutcome)>> {
    Ok(vec![("gh auth", check_gh_auth().await)])
}

/// Verify `gh auth status` succeeds for github.com. The agent pod's
/// PR-creation path shells out to `gh pr create`, which needs a token
/// in the engineer's `gh` config. Today the sidecar proxies git+ssh
/// for clone/push, but `gh` runs inside the agent container with its
/// own credential lookup — and we don't yet mount one (filed as a
/// separate v0.7.14 ask). Catching the absent-token case on the
/// engineer's laptop short-circuits the eventual 60-min-then-fail
/// failure mode by ~3600x.
async fn check_gh_auth() -> CheckOutcome {
    let output = match tokio::process::Command::new("gh")
        .arg("auth")
        .arg("status")
        .arg("--hostname")
        .arg("github.com")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
    {
        Ok(o) => o,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return CheckOutcome::Skip {
                reason: "gh CLI not installed on this machine; cannot preflight \
                         GitHub auth (the agent pod still needs a token at run time)"
                    .to_string(),
            };
        }
        Err(e) => {
            return CheckOutcome::Skip {
                reason: format!("gh auth status invocation failed: {e}"),
            };
        }
    };

    if output.status.success() {
        CheckOutcome::Ok
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        // gh prints its hint to stderr; if the binary returned a
        // recognisable "not logged in" pattern, surface its own
        // message verbatim — it's already actionable. Otherwise fall
        // back to a generic prompt.
        let hint = if stderr.contains("not logged into") || stderr.contains("gh auth login") {
            format!(
                "GitHub auth is not configured for github.com. Run:\n  gh auth login --hostname github.com\n\nFull `gh auth status` output:\n{stderr}"
            )
        } else {
            format!(
                "GitHub auth check failed (`gh auth status --hostname github.com` exited {}). Run `gh auth login --hostname github.com` and retry. Output:\n{stderr}",
                output.status
            )
        };
        CheckOutcome::Fail { hint }
    }
}

/// Render preflight outcomes and return Err if any check failed.
/// Skip outcomes print a one-line warning and don't block.
pub fn render_and_decide(outcomes: &[(&'static str, CheckOutcome)]) -> Result<()> {
    let mut blocking_failures: Vec<String> = Vec::new();
    for (name, outcome) in outcomes {
        match outcome {
            CheckOutcome::Ok => {}
            CheckOutcome::Skip { reason } => {
                eprintln!("preflight {name}: skipped — {reason}");
            }
            CheckOutcome::Fail { hint } => {
                blocking_failures.push(format!("preflight {name} failed:\n{hint}"));
            }
        }
    }
    if !blocking_failures.is_empty() {
        anyhow::bail!(
            "Aborting before submit. {} preflight check(s) failed:\n\n{}",
            blocking_failures.len(),
            blocking_failures.join("\n\n")
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_and_decide_passes_when_all_ok() {
        assert!(render_and_decide(&[("gh auth", CheckOutcome::Ok)]).is_ok());
    }

    #[test]
    fn render_and_decide_passes_on_skip_only() {
        assert!(
            render_and_decide(&[(
                "gh auth",
                CheckOutcome::Skip {
                    reason: "no gh".to_string()
                },
            )])
            .is_ok()
        );
    }

    #[test]
    fn render_and_decide_blocks_on_fail() {
        let r = render_and_decide(&[(
            "gh auth",
            CheckOutcome::Fail {
                hint: "run gh auth login".to_string(),
            },
        )]);
        let err = r.expect_err("must block on Fail");
        let msg = format!("{err:#}");
        assert!(msg.contains("preflight gh auth failed"));
        assert!(msg.contains("run gh auth login"));
    }
}
