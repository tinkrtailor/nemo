mod api_types;
mod claude_creds;
mod client;
mod commands;
mod config;
mod project_config;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "nemo",
    about = "Nemo CLI - Convergent loop orchestrator",
    version
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,

    /// API server URL override
    #[arg(long, global = true)]
    server: Option<String>,

    /// Disable TLS certificate verification (dev/self-signed certs only)
    #[arg(long, global = true)]
    insecure: bool,
}

#[derive(Subcommand)]
enum Commands {
    /// Harden spec, merge spec PR. Terminal: HARDENED
    Harden {
        /// Path to the spec file
        spec_path: String,

        /// Override implementor model
        #[arg(long)]
        model_impl: Option<String>,

        /// Override reviewer model
        #[arg(long)]
        model_review: Option<String>,
    },

    /// Implement spec, create PR. Terminal: CONVERGED
    Start {
        /// Path to the spec file
        spec_path: String,

        /// Harden spec first, then implement
        #[arg(long)]
        harden: bool,

        /// Skip AWAITING_APPROVAL gate
        #[arg(long)]
        auto_approve: bool,

        /// Override implementor model
        #[arg(long)]
        model_impl: Option<String>,

        /// Override reviewer model
        #[arg(long)]
        model_review: Option<String>,
    },

    /// Implement + auto-merge. Terminal: SHIPPED
    Ship {
        /// Path to the spec file
        spec_path: String,

        /// Harden first (skips approval gate), then implement + merge
        #[arg(long)]
        harden: bool,

        /// Override implementor model
        #[arg(long)]
        model_impl: Option<String>,

        /// Override reviewer model
        #[arg(long)]
        model_review: Option<String>,
    },

    /// Show your running loops
    Status {
        /// Show all engineers' loops
        #[arg(long)]
        team: bool,

        /// Output as JSON
        #[arg(long)]
        json: bool,
    },

    /// K9s-style loop overview with live logs
    Helm {
        /// Show all engineers' loops
        #[arg(long)]
        team: bool,
    },

    /// Stream logs for a loop
    Logs {
        /// Loop ID
        loop_id: String,

        /// Show only round N
        #[arg(long)]
        round: Option<i32>,

        /// Filter by stage (implement/test/review)
        #[arg(long)]
        stage: Option<String>,

        /// Dump live stdout from the active pod container instead of
        /// the Postgres log stream. Works mid-run without kubectl.
        #[arg(long)]
        tail: bool,

        /// Max lines to return with --tail (default 500, max 10000)
        #[arg(long, default_value_t = 500)]
        tail_lines: u32,

        /// Container to read from with --tail ("agent" or "auth-sidecar")
        #[arg(long, default_value = "agent")]
        container: String,
    },

    /// Show live processes and runtime state of an active loop's pod
    Ps {
        /// Loop ID
        loop_id: String,

        /// Poll every 2s and redraw (press q to quit)
        #[arg(long)]
        watch: bool,
    },

    /// Cancel a running loop
    Cancel {
        /// Loop ID
        loop_id: String,
    },

    /// Approve a loop awaiting approval
    Approve {
        /// Loop ID
        loop_id: String,
    },

    /// Show detailed loop state, round history, and verdicts
    Inspect {
        /// Branch path (e.g., "alice/invoice-cancel-a1b2c3d4" or "agent/alice/invoice-cancel-a1b2c3d4")
        path: String,
    },

    /// Resume a PAUSED, AWAITING_REAUTH, or transient-FAILED loop
    Resume {
        /// Loop ID
        loop_id: String,
    },

    /// Extend a FAILED loop's max_rounds and resume it from the last stage
    Extend {
        /// Loop ID
        loop_id: String,
        /// Number of rounds to add to max_rounds
        #[arg(long, default_value_t = 5)]
        add: u32,
    },

    /// Scan monorepo, generate nemo.toml
    Init {
        /// Overwrite existing nemo.toml
        #[arg(long)]
        force: bool,
    },

    /// Push local model credentials to cluster
    Auth {
        /// Push Claude/Anthropic credentials only
        #[arg(long)]
        claude: bool,

        /// Push OpenAI credentials only (API key or local Codex/Opencode OAuth bundle)
        #[arg(long)]
        openai: bool,

        /// Push SSH key only
        #[arg(long)]
        ssh: bool,
    },

    /// Show authenticated providers and available models
    Models,

    /// Edit ~/.nemo/config.toml
    Config {
        /// Set a config value
        #[arg(long)]
        set: Option<String>,

        /// Get a config value
        #[arg(long)]
        get: Option<String>,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .init();

    let cli = Cli::parse();

    // Handle config command before loading config — a broken config file
    // must not prevent `nemo config --set` from working.
    if let Commands::Config { ref set, ref get } = cli.command {
        return commands::config::run(set.clone(), get.clone());
    }

    // Init is local-only — don't require config
    if let Commands::Init { force } = cli.command {
        return commands::init::run(force);
    }

    let eng_config = config::load_config()?;

    let server_url = cli.server.unwrap_or(eng_config.server_url.clone());

    let insecure =
        cli.insecure || matches!(std::env::var("NEMO_INSECURE").as_deref(), Ok("true" | "1"));
    // Warn early if api_key is missing — commands that hit the server will fail
    if eng_config.api_key.is_none() {
        // Init and Config don't need an API key
        if !matches!(cli.command, Commands::Init { .. }) {
            anyhow::bail!("API key not configured. Run: nemo config --set api_key=<your-key>");
        }
    }

    let http_client =
        client::NemoClient::new(&server_url, eng_config.api_key.as_deref(), insecure)?;

    // Validate engineer is configured for commands that need it
    // Status --team doesn't need engineer
    let needs_engineer = match &cli.command {
        Commands::Harden { .. }
        | Commands::Start { .. }
        | Commands::Ship { .. }
        | Commands::Auth { .. } => true,
        Commands::Helm { team } => !team,
        Commands::Status { team, .. } => !team,
        _ => false,
    };
    if needs_engineer && eng_config.engineer.is_empty() {
        anyhow::bail!("Engineer name not configured. Run: nemo config --set engineer=<your-name>");
    }

    match cli.command {
        Commands::Harden {
            spec_path,
            model_impl,
            model_review,
        } => {
            let (model_impl, model_review) =
                project_config::resolve_models(model_impl, model_review, &eng_config.models)?;
            claude_creds::ensure_fresh(
                &http_client,
                &eng_config.engineer,
                &eng_config.name,
                &eng_config.email,
            )
            .await?;
            // nemo harden: harden=true, harden_only=true, ship_mode=false
            commands::start::run(
                &http_client,
                commands::start::StartArgs {
                    engineer: &eng_config.engineer,
                    spec_path: &spec_path,
                    harden: true,
                    harden_only: true,
                    auto_approve: false,
                    ship_mode: false,
                    model_impl,
                    model_review,
                },
            )
            .await?;
        }
        Commands::Start {
            spec_path,
            harden,
            auto_approve,
            model_impl,
            model_review,
        } => {
            let (model_impl, model_review) =
                project_config::resolve_models(model_impl, model_review, &eng_config.models)?;
            claude_creds::ensure_fresh(
                &http_client,
                &eng_config.engineer,
                &eng_config.name,
                &eng_config.email,
            )
            .await?;
            // nemo start: ship_mode=false
            commands::start::run(
                &http_client,
                commands::start::StartArgs {
                    engineer: &eng_config.engineer,
                    spec_path: &spec_path,
                    harden,
                    harden_only: false,
                    auto_approve,
                    ship_mode: false,
                    model_impl,
                    model_review,
                },
            )
            .await?;
        }
        Commands::Ship {
            spec_path,
            harden,
            model_impl,
            model_review,
        } => {
            let (model_impl, model_review) =
                project_config::resolve_models(model_impl, model_review, &eng_config.models)?;
            claude_creds::ensure_fresh(
                &http_client,
                &eng_config.engineer,
                &eng_config.name,
                &eng_config.email,
            )
            .await?;
            // nemo ship: ship_mode=true, auto_approve implied
            commands::start::run(
                &http_client,
                commands::start::StartArgs {
                    engineer: &eng_config.engineer,
                    spec_path: &spec_path,
                    harden,
                    harden_only: false,
                    auto_approve: true,
                    ship_mode: true,
                    model_impl,
                    model_review,
                },
            )
            .await?;
        }
        Commands::Status { team, json } => {
            commands::status::run(&http_client, &eng_config.engineer, team, json).await?;
        }
        Commands::Helm { team } => {
            commands::helm::run(&http_client, &eng_config.engineer, team).await?;
        }
        Commands::Logs {
            loop_id,
            round,
            stage,
            tail,
            tail_lines,
            container,
        } => {
            if tail {
                // --tail reads raw pod container stdout, which has
                // no round/stage structure to filter on. Fail loud
                // instead of silently ignoring the flags.
                if round.is_some() || stage.is_some() {
                    anyhow::bail!(
                        "--round / --stage are not supported with --tail (pod stdout is unstructured); \
                         run without --tail for filtered historical logs"
                    );
                }
                // If --tail fails because the loop is terminal or has
                // no active pod, fall back to historical logs. Other
                // errors (bad container name, control plane down, etc.)
                // should surface directly to the operator.
                match commands::logs::run_tail(&http_client, &loop_id, tail_lines, &container).await
                {
                    Ok(commands::logs::TailResult::Ok) => {}
                    Ok(commands::logs::TailResult::NoPod) => {
                        // Don't fall back to the SSE stream here — that
                        // would block forever if the loop is paused or
                        // awaiting approval/auth. Just inform and exit.
                        eprintln!(
                            "No active pod. The loop may be between stages, paused, or awaiting auth. Try again shortly or use `nemo logs {loop_id}` (without --tail) for historical logs."
                        );
                    }
                    Err(e) => {
                        let msg = e.to_string();
                        if msg.contains("use `nemo logs") {
                            eprintln!("--tail: {msg}; falling back to historical logs");
                            commands::logs::run(&http_client, &loop_id, None, None).await?;
                        } else {
                            return Err(e);
                        }
                    }
                }
            } else {
                commands::logs::run(&http_client, &loop_id, round, stage).await?;
            }
        }
        Commands::Ps { loop_id, watch } => {
            if watch {
                commands::ps::run_watch(&http_client, &loop_id).await?;
            } else {
                let exit_code = commands::ps::run(&http_client, &loop_id).await?;
                std::process::exit(exit_code);
            }
        }
        Commands::Cancel { loop_id } => {
            commands::cancel::run(&http_client, &loop_id).await?;
        }
        Commands::Approve { loop_id } => {
            commands::approve::run(&http_client, &loop_id).await?;
        }
        Commands::Inspect { path } => {
            commands::inspect::run(&http_client, &path).await?;
        }
        Commands::Resume { loop_id } => {
            commands::resume::run(&http_client, &loop_id).await?;
        }
        Commands::Extend { loop_id, add } => {
            commands::extend::run(&http_client, &loop_id, add).await?;
        }
        Commands::Init { .. } => {
            // Handled above before config loading
            unreachable!("Init is dispatched before config loading");
        }
        Commands::Auth {
            claude,
            openai,
            ssh,
        } => {
            commands::auth::run(
                &http_client,
                &eng_config.engineer,
                &eng_config.name,
                &eng_config.email,
                claude,
                openai,
                ssh,
            )
            .await?;
        }
        Commands::Models => {
            commands::models::run(&http_client, &eng_config).await?;
        }
        Commands::Config { set, get } => {
            commands::config::run(set, get)?;
        }
    }

    Ok(())
}
