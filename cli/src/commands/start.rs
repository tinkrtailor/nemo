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

    let resp: StartResponse = client.post("/start", &body).await?;

    println!("Started loop {}", resp.loop_id);
    println!("  Branch: {}", resp.branch);
    // FR-5b: show spec source receipt
    println!("  Spec:   {} (local, {} bytes)", args.spec_path, spec_bytes);
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

#[cfg(test)]
mod tests {
    use std::io::Write;
    use tempfile::NamedTempFile;

    #[test]
    fn test_read_local_spec_file() {
        let mut tmp = NamedTempFile::new().unwrap();
        write!(tmp, "# My Spec\nSome content").unwrap();
        let content = std::fs::read_to_string(tmp.path()).unwrap();
        assert_eq!(content, "# My Spec\nSome content");
    }

    #[test]
    fn test_read_nonexistent_spec_file_fails() {
        let result = std::fs::read_to_string("nonexistent/spec.md");
        assert!(result.is_err());
    }
}
