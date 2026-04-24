use anyhow::{Context, Result};
use std::path::Path;

use crate::client::NemoClient;

#[derive(serde::Deserialize)]
struct StartResponse {
    loop_id: uuid::Uuid,
    branch: String,
    state: String,
}

pub struct StartArgs<'a> {
    pub engineer: &'a str,
    pub spec_path: &'a str,
    pub harden: bool,
    pub harden_only: bool,
    pub auto_approve: bool,
    pub ship_mode: bool,
    pub model_impl: Option<String>,
    pub model_review: Option<String>,
    /// Optional per-stage Job `activeDeadlineSeconds` override. Uniform
    /// across all stages. Server-side floored to 300s.
    pub stage_timeout_secs: Option<u32>,
    /// Per-stage timeout overrides sourced from the repo-level
    /// `nemo.toml` `[timeouts]` block. Attached to the submit body so
    /// the server can stamp them on the loop record; per-stage beats
    /// uniform `stage_timeout_secs` at stage-dispatch time.
    pub project_timeouts: crate::project_config::TimeoutsSection,
}

pub async fn run(client: &NemoClient, args: StartArgs<'_>) -> Result<()> {
    // FR-1a: Read spec file from the engineer's local working directory.
    let local_path = Path::new(args.spec_path);
    let spec_content = std::fs::read_to_string(local_path).with_context(|| {
        format!(
            "Failed to read spec file '{}'. The file must exist locally.",
            args.spec_path
        )
    })?;

    let spec_bytes = spec_content.len();

    // FR-3b: client-side size check to avoid uploading oversized specs.
    validate_spec_size(args.spec_path, &spec_content)?;

    let body = build_start_body(&args, &spec_content);

    let resp: StartResponse = client.post("/start", &body).await?;

    println!("Started loop {}", resp.loop_id);
    println!("  Branch: {}", resp.branch);
    // FR-5b: show spec source receipt with thousands-separated byte count
    println!(
        "  Spec:   {} (local, {} bytes)",
        args.spec_path,
        format_thousands(spec_bytes)
    );

    // FR-4: show phase plan so engineers know what to expect
    println!("  Phase:  {}", phase_plan_label(&args));

    println!("  State:  {}", resp.state);

    if args.ship_mode {
        println!("\n  Ship mode: will auto-merge on convergence.");
    } else if args.harden_only {
        println!("\n  Hardening spec only. Will terminate at HARDENED.");
    } else if !args.auto_approve {
        println!(
            "\n  Run `nemo approve {}` to start implementation.",
            resp.loop_id
        );
    }

    Ok(())
}

/// Format a number with thousands separators (e.g., 1234 → "1,234").
fn format_thousands(n: usize) -> String {
    let s = n.to_string();
    let bytes = s.as_bytes();
    let mut result = String::with_capacity(s.len() + s.len() / 3);
    for (i, &b) in bytes.iter().enumerate() {
        #[allow(clippy::manual_is_multiple_of)]
        if i > 0 && (bytes.len() - i) % 3 == 0 {
            result.push(',');
        }
        result.push(b as char);
    }
    result
}

/// Maximum spec file size in bytes (1 MB).
const MAX_SPEC_SIZE: usize = 1_048_576;

/// Validate that a spec's content does not exceed the size limit.
/// Returns Ok(()) if within limits, or an error with a descriptive message.
fn validate_spec_size(spec_path: &str, content: &str) -> Result<()> {
    let spec_bytes = content.len();
    if spec_bytes > MAX_SPEC_SIZE {
        anyhow::bail!(
            "Spec file '{}' is {} bytes, which exceeds the 1 MB limit ({} bytes).",
            spec_path,
            format_thousands(spec_bytes),
            format_thousands(MAX_SPEC_SIZE),
        );
    }
    Ok(())
}

/// Return the phase-plan label for CLI output. Pure function for testability.
fn phase_plan_label(args: &StartArgs<'_>) -> &'static str {
    if args.ship_mode {
        if args.harden {
            "HARDEN \u{2192} IMPLEMENT \u{2192} SHIP"
        } else {
            "IMPLEMENT \u{2192} SHIP"
        }
    } else if args.harden_only {
        "HARDEN"
    } else if args.harden {
        if args.auto_approve {
            "HARDEN \u{2192} IMPLEMENT"
        } else {
            "HARDEN \u{2192} AWAITING_APPROVAL \u{2192} IMPLEMENT (add --no-harden to skip harden)"
        }
    } else {
        "IMPLEMENT (harden skipped)"
    }
}

/// Validate that `--harden` and `--no-harden` are not both provided.
/// Returns Ok(()) if valid, or an error with the spec-prescribed message.
pub fn validate_harden_flags(harden: bool, no_harden: bool) -> Result<()> {
    if harden && no_harden {
        anyhow::bail!(
            "Cannot use --harden and --no-harden together. --harden is deprecated; remove it."
        );
    }
    Ok(())
}

/// Return a deprecation warning if the `--harden` flag was explicitly passed.
pub fn deprecation_warning(harden_flag_set: bool) -> Option<&'static str> {
    if harden_flag_set {
        Some("Warning: --harden is now the default; this flag has no effect.")
    } else {
        None
    }
}

/// Build the JSON request body for /start. Extracted for testability.
fn build_start_body(args: &StartArgs<'_>, spec_content: &str) -> serde_json::Value {
    let mut body = serde_json::json!({
        "spec_path": args.spec_path,
        "engineer": args.engineer,
        "spec_content": spec_content,
        "harden": args.harden,
        "harden_only": args.harden_only,
        "auto_approve": args.auto_approve,
        "ship_mode": args.ship_mode,
    });

    if args.model_impl.is_some() || args.model_review.is_some() {
        body["model_overrides"] = serde_json::json!({
            "implementor": args.model_impl,
            "reviewer": args.model_review,
        });
    }

    if let Some(secs) = args.stage_timeout_secs {
        body["stage_timeout_secs"] = serde_json::json!(secs);
    }

    if !args.project_timeouts.is_empty() {
        body["timeouts"] =
            serde_json::to_value(&args.project_timeouts).unwrap_or(serde_json::Value::Null);
    }

    body
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    #[test]
    fn test_read_local_spec_file_and_populate_content() {
        let mut tmp = NamedTempFile::new().unwrap();
        write!(tmp, "# My Spec\nSome content").unwrap();

        // Simulate the same read_to_string + build_start_body path from run()
        let content = std::fs::read_to_string(tmp.path()).unwrap();
        let args = StartArgs {
            engineer: "alice",
            spec_path: "specs/test.md",
            harden: false,
            harden_only: false,
            auto_approve: true,
            ship_mode: false,
            model_impl: None,
            model_review: None,
            stage_timeout_secs: None,
            project_timeouts: Default::default(),
        };
        let body = build_start_body(&args, &content);

        assert_eq!(body["spec_content"], "# My Spec\nSome content");
        assert_eq!(body["spec_path"], "specs/test.md");
        assert_eq!(body["engineer"], "alice");
    }

    #[test]
    fn test_read_nonexistent_spec_file_fails_with_context() {
        let spec_path = "nonexistent/spec.md";
        let local_path = Path::new(spec_path);
        let result: std::result::Result<String, anyhow::Error> =
            std::fs::read_to_string(local_path).with_context(|| {
                format!(
                    "Failed to read spec file '{}'. The file must exist locally.",
                    spec_path
                )
            });
        assert!(result.is_err());
        let err_msg = format!("{:#}", result.unwrap_err());
        assert!(
            err_msg.contains("The file must exist locally"),
            "Expected context message, got: {err_msg}"
        );
    }

    #[test]
    fn test_build_start_body_includes_model_overrides() {
        let args = StartArgs {
            engineer: "bob",
            spec_path: "specs/feat.md",
            harden: true,
            harden_only: false,
            auto_approve: false,
            ship_mode: false,
            model_impl: Some("claude-opus-4-6".to_string()),
            model_review: None,
            stage_timeout_secs: None,
            project_timeouts: Default::default(),
        };
        let body = build_start_body(&args, "# Spec");
        assert!(body["model_overrides"].is_object());
        assert_eq!(body["model_overrides"]["implementor"], "claude-opus-4-6");
    }

    #[test]
    fn test_build_start_body_includes_project_timeouts() {
        // v0.7.12 regression guard: [timeouts] in the repo-level
        // nemo.toml must flow through to the submit body so the server
        // can stamp per-stage overrides on the loop record. Prior to
        // this, `nemo init` generated [timeouts] that nothing read.
        let args = StartArgs {
            engineer: "alice",
            spec_path: "specs/big.md",
            harden: true,
            harden_only: true,
            auto_approve: false,
            ship_mode: false,
            model_impl: None,
            model_review: None,
            stage_timeout_secs: None,
            project_timeouts: crate::project_config::TimeoutsSection {
                implement_secs: Some(7200),
                review_secs: Some(3600),
                test_secs: Some(7200),
                audit_secs: Some(3600),
                revise_secs: Some(3600),
                watchdog_secs: None,
            },
        };
        let body = build_start_body(&args, "# Spec");
        let timeouts = &body["timeouts"];
        assert_eq!(timeouts["audit_secs"], 3600);
        assert_eq!(timeouts["implement_secs"], 7200);
        assert_eq!(timeouts["review_secs"], 3600);
        assert!(
            timeouts.get("watchdog_secs").is_none(),
            "unset stages must not appear; server treats absent as cluster default"
        );
    }

    #[test]
    fn test_build_start_body_omits_empty_project_timeouts() {
        let args = StartArgs {
            engineer: "alice",
            spec_path: "specs/ok.md",
            harden: true,
            harden_only: false,
            auto_approve: false,
            ship_mode: false,
            model_impl: None,
            model_review: None,
            stage_timeout_secs: None,
            project_timeouts: Default::default(),
        };
        let body = build_start_body(&args, "# Spec");
        assert!(
            body.get("timeouts").is_none(),
            "empty [timeouts] must not be serialized (keeps request bodies clean)"
        );
    }

    #[test]
    fn test_build_start_body_omits_model_overrides_when_none() {
        let args = StartArgs {
            engineer: "bob",
            spec_path: "specs/feat.md",
            harden: false,
            harden_only: false,
            auto_approve: false,
            ship_mode: false,
            model_impl: None,
            model_review: None,
            stage_timeout_secs: None,
            project_timeouts: Default::default(),
        };
        let body = build_start_body(&args, "# Spec");
        assert!(body.get("model_overrides").is_none());
    }

    #[test]
    fn test_oversized_spec_rejected_client_side() {
        // 1 MB + 1 byte must be rejected
        let oversized = "x".repeat(1_048_577);
        let result = validate_spec_size("specs/big.md", &oversized);
        assert!(result.is_err(), "Oversized spec should be rejected");
        let err_msg = format!("{:#}", result.unwrap_err());
        assert!(
            err_msg.contains("exceeds the 1 MB limit"),
            "Expected size limit message, got: {err_msg}"
        );
    }

    #[test]
    fn test_spec_at_limit_accepted_client_side() {
        // Exactly 1 MB should be accepted
        let content = "x".repeat(1_048_576);
        let result = validate_spec_size("specs/ok.md", &content);
        assert!(result.is_ok(), "Exactly 1 MB should not be rejected");
    }

    #[test]
    fn test_format_thousands() {
        assert_eq!(format_thousands(0), "0");
        assert_eq!(format_thousands(999), "999");
        assert_eq!(format_thousands(1_000), "1,000");
        assert_eq!(format_thousands(1_234), "1,234");
        assert_eq!(format_thousands(1_000_000), "1,000,000");
        assert_eq!(format_thousands(12_345_678), "12,345,678");
    }

    /// NFR-3: default invocation sends harden: true
    #[test]
    fn test_default_start_sends_harden_true() {
        let args = StartArgs {
            engineer: "alice",
            spec_path: "specs/test.md",
            harden: true, // default: !no_harden where no_harden=false
            harden_only: false,
            auto_approve: false,
            ship_mode: false,
            model_impl: None,
            model_review: None,
            stage_timeout_secs: None,
            project_timeouts: Default::default(),
        };
        let body = build_start_body(&args, "# Spec");
        assert_eq!(
            body["harden"], true,
            "default invocation must send harden: true"
        );
    }

    /// NFR-3: --no-harden sends harden: false
    #[test]
    fn test_no_harden_flag_sends_harden_false() {
        let args = StartArgs {
            engineer: "alice",
            spec_path: "specs/test.md",
            harden: false, // !no_harden where no_harden=true
            harden_only: false,
            auto_approve: false,
            ship_mode: false,
            model_impl: None,
            model_review: None,
            stage_timeout_secs: None,
            project_timeouts: Default::default(),
        };
        let body = build_start_body(&args, "# Spec");
        assert_eq!(body["harden"], false, "--no-harden must send harden: false");
    }

    /// NFR-3: deprecated --harden flag emits deprecation warning
    #[test]
    fn test_deprecated_harden_flag_emits_warning() {
        let warning = deprecation_warning(true);
        assert_eq!(
            warning,
            Some("Warning: --harden is now the default; this flag has no effect."),
            "--harden must emit deprecation warning"
        );
    }

    /// NFR-3: no deprecation warning when --harden is not passed
    #[test]
    fn test_no_deprecation_warning_without_harden_flag() {
        assert_eq!(deprecation_warning(false), None);
    }

    /// NFR-3: --harden --no-harden together parses at clap level but is caught
    /// by validate_harden_flags with the spec-prescribed error message.
    #[test]
    fn test_harden_and_no_harden_both_parse() {
        use clap::Parser;

        // Both flags parse successfully (conflict check is in validate_harden_flags, not clap)
        let cli = crate::Cli::try_parse_from([
            "nemo",
            "start",
            "specs/foo.md",
            "--harden",
            "--no-harden",
        ]);
        assert!(
            cli.is_ok(),
            "clap should parse both flags; conflict is checked manually"
        );
    }

    /// NFR-3: validate_harden_flags returns the spec-prescribed error message
    /// when both --harden and --no-harden are set.
    #[test]
    fn test_validate_harden_flags_both_set_returns_error() {
        let result = validate_harden_flags(true, true);
        assert!(result.is_err(), "both flags set must return error");
        let err_msg = format!("{:#}", result.unwrap_err());
        assert_eq!(
            err_msg,
            "Cannot use --harden and --no-harden together. --harden is deprecated; remove it."
        );
    }

    /// NFR-3: validate_harden_flags accepts valid flag combinations.
    #[test]
    fn test_validate_harden_flags_valid_combinations() {
        assert!(
            validate_harden_flags(false, false).is_ok(),
            "neither flag is valid"
        );
        assert!(
            validate_harden_flags(true, false).is_ok(),
            "--harden only is valid"
        );
        assert!(
            validate_harden_flags(false, true).is_ok(),
            "--no-harden only is valid"
        );
    }

    /// FR-4a: default output shows HARDEN → AWAITING_APPROVAL → IMPLEMENT phase plan
    #[test]
    fn test_default_start_phase_label() {
        let args = StartArgs {
            engineer: "alice",
            spec_path: "specs/test.md",
            harden: true,
            harden_only: false,
            auto_approve: false,
            ship_mode: false,
            model_impl: None,
            model_review: None,
            stage_timeout_secs: None,
            project_timeouts: Default::default(),
        };
        assert_eq!(
            phase_plan_label(&args),
            "HARDEN \u{2192} AWAITING_APPROVAL \u{2192} IMPLEMENT (add --no-harden to skip harden)"
        );
    }

    /// FR-4b: --no-harden output shows IMPLEMENT (harden skipped)
    #[test]
    fn test_no_harden_phase_label() {
        let args = StartArgs {
            engineer: "alice",
            spec_path: "specs/test.md",
            harden: false,
            harden_only: false,
            auto_approve: false,
            ship_mode: false,
            model_impl: None,
            model_review: None,
            stage_timeout_secs: None,
            project_timeouts: Default::default(),
        };
        assert_eq!(phase_plan_label(&args), "IMPLEMENT (harden skipped)");
    }

    /// FR-2b: --auto-approve omits AWAITING_APPROVAL from phase plan
    #[test]
    fn test_auto_approve_phase_label_omits_approval_gate() {
        let args = StartArgs {
            engineer: "alice",
            spec_path: "specs/test.md",
            harden: true,
            harden_only: false,
            auto_approve: true,
            ship_mode: false,
            model_impl: None,
            model_review: None,
            stage_timeout_secs: None,
            project_timeouts: Default::default(),
        };
        assert_eq!(phase_plan_label(&args), "HARDEN \u{2192} IMPLEMENT");
    }

    /// Phase plan for ship mode with harden
    #[test]
    fn test_ship_harden_phase_label() {
        let args = StartArgs {
            engineer: "alice",
            spec_path: "specs/test.md",
            harden: true,
            harden_only: false,
            auto_approve: true,
            ship_mode: true,
            model_impl: None,
            model_review: None,
            stage_timeout_secs: None,
            project_timeouts: Default::default(),
        };
        assert_eq!(
            phase_plan_label(&args),
            "HARDEN \u{2192} IMPLEMENT \u{2192} SHIP"
        );
    }

    /// Phase plan for harden-only mode
    #[test]
    fn test_harden_only_phase_label() {
        let args = StartArgs {
            engineer: "alice",
            spec_path: "specs/test.md",
            harden: true,
            harden_only: true,
            auto_approve: false,
            ship_mode: false,
            model_impl: None,
            model_review: None,
            stage_timeout_secs: None,
            project_timeouts: Default::default(),
        };
        assert_eq!(phase_plan_label(&args), "HARDEN");
    }
}
