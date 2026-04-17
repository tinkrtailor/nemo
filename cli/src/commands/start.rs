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
        if i > 0 && (bytes.len() - i).is_multiple_of(3) {
            result.push(',');
        }
        result.push(b as char);
    }
    result
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
    fn test_format_thousands() {
        assert_eq!(format_thousands(0), "0");
        assert_eq!(format_thousands(999), "999");
        assert_eq!(format_thousands(1_000), "1,000");
        assert_eq!(format_thousands(1_234), "1,234");
        assert_eq!(format_thousands(1_000_000), "1,000,000");
        assert_eq!(format_thousands(12_345_678), "12,345,678");
    }
}
