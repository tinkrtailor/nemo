use anyhow::Result;

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
    let mut body = serde_json::json!({
        "spec_path": args.spec_path,
        "engineer": args.engineer,
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
