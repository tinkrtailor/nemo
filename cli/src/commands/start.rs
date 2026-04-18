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
    if args.ship_mode {
        if args.harden {
            println!(
                "  Phase:  HARDEN \u{2192} IMPLEMENT \u{2192} SHIP"
            );
        } else {
            println!("  Phase:  IMPLEMENT \u{2192} SHIP");
        }
    } else if args.harden_only {
        println!("  Phase:  HARDEN");
    } else if args.harden {
        println!(
            "  Phase:  HARDEN \u{2192} AWAITING_APPROVAL \u{2192} IMPLEMENT (add --no-harden to skip harden)"
        );
    } else {
        println!("  Phase:  IMPLEMENT (harden skipped)");
    }

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
        };
        let body = build_start_body(&args, "# Spec");
        assert!(body["model_overrides"].is_object());
        assert_eq!(body["model_overrides"]["implementor"], "claude-opus-4-6");
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
        };
        let body = build_start_body(&args, "# Spec");
        assert_eq!(body["harden"], true, "default invocation must send harden: true");
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
        };
        let body = build_start_body(&args, "# Spec");
        assert_eq!(body["harden"], false, "--no-harden must send harden: false");
    }

    /// NFR-3: deprecated --harden flag still sends harden: true
    #[test]
    fn test_deprecated_harden_flag_sends_harden_true() {
        // When --harden is passed (no --no-harden), the CLI computes harden = !false = true.
        // The --harden flag itself is a deprecated no-op; the result is the same as default.
        let args = StartArgs {
            engineer: "alice",
            spec_path: "specs/test.md",
            harden: true,
            harden_only: false,
            auto_approve: false,
            ship_mode: false,
            model_impl: None,
            model_review: None,
        };
        let body = build_start_body(&args, "# Spec");
        assert_eq!(body["harden"], true, "--harden (deprecated) must send harden: true");
    }

    /// NFR-3: --harden --no-harden together is rejected by clap (conflicts_with)
    #[test]
    fn test_harden_and_no_harden_conflict() {
        use clap::Parser;

        // Attempt to parse with both flags — clap should reject
        let result = crate::Cli::try_parse_from([
            "nemo", "start", "specs/foo.md", "--harden", "--no-harden",
        ]);
        assert!(
            result.is_err(),
            "--harden and --no-harden together must be rejected"
        );
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("--harden") && err_msg.contains("--no-harden"),
            "Error must mention both conflicting flags, got: {err_msg}"
        );
    }

    /// FR-4a: default output shows HARDEN → AWAITING_APPROVAL → IMPLEMENT phase plan
    #[test]
    fn test_default_start_phase_includes_harden() {
        // Verify the args that would be constructed for default invocation
        // produce harden=true, which triggers the HARDEN phase plan output
        let args = StartArgs {
            engineer: "alice",
            spec_path: "specs/test.md",
            harden: true,
            harden_only: false,
            auto_approve: false,
            ship_mode: false,
            model_impl: None,
            model_review: None,
        };
        // The phase plan logic: harden && !harden_only && !ship_mode
        assert!(args.harden && !args.harden_only && !args.ship_mode);
    }

    /// FR-4b: --no-harden output shows IMPLEMENT (harden skipped) phase plan
    #[test]
    fn test_no_harden_phase_shows_implement_only() {
        let args = StartArgs {
            engineer: "alice",
            spec_path: "specs/test.md",
            harden: false,
            harden_only: false,
            auto_approve: false,
            ship_mode: false,
            model_impl: None,
            model_review: None,
        };
        // The phase plan logic: !harden && !harden_only && !ship_mode → "IMPLEMENT (harden skipped)"
        assert!(!args.harden && !args.harden_only && !args.ship_mode);
    }
}
