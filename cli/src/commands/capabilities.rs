use anyhow::Result;
use clap::Command;

// Feature flags — hardcoded booleans representing server-side/product-level
// capability presence. Updated manually when features ship. NOT Cargo feature
// gates.
const QA_STAGE: bool = false;
const ORCHESTRATOR_JUDGE: bool = true;
const PLUGGABLE_CACHE: bool = true;
const HARDEN_BY_DEFAULT: bool = true;
const NEMO_EXTEND: bool = true;
const POD_INTROSPECT: bool = true;
const DASHBOARD: bool = false;

/// Build the capabilities JSON from the live clap `Command` tree.
pub fn run(cli_command: &Command) -> Result<()> {
    let version = cli_command.get_version().unwrap_or("unknown").to_string();

    let commands: Vec<String> = cli_command
        .get_subcommands()
        .map(|c| c.get_name().to_string())
        .collect();

    let output = serde_json::json!({
        "version": version,
        "commands": commands,
        "features": {
            "qa_stage": QA_STAGE,
            "orchestrator_judge": ORCHESTRATOR_JUDGE,
            "pluggable_cache": PLUGGABLE_CACHE,
            "harden_by_default": HARDEN_BY_DEFAULT,
            "nemo_extend": NEMO_EXTEND,
            "pod_introspect": POD_INTROSPECT,
            "dashboard": DASHBOARD,
        }
    });

    println!("{}", serde_json::to_string_pretty(&output)?);
    Ok(())
}
