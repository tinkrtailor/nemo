mod client;
mod commands;
mod config;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "nemo", about = "Nemo CLI - Convergent loop orchestrator")]
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
        /// User/branch path (e.g., "alice/invoice-cancel-a1b2c3d4")
        path: String,
    },

    /// Resume a PAUSED or AWAITING_REAUTH loop
    Resume {
        /// Loop ID
        loop_id: String,
    },

    /// Scan monorepo, generate nemo.toml
    Init {
        /// Overwrite existing nemo.toml
        #[arg(long)]
        force: bool,
    },

    /// Push local model credentials to cluster
    Auth {
        /// Push Claude credentials only
        #[arg(long)]
        claude: bool,

        /// Push OpenAI credentials only
        #[arg(long)]
        openai: bool,
    },

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
    let eng_config = config::load_config()?;

    let server_url = cli.server.unwrap_or(eng_config.server_url.clone());

    let insecure = cli.insecure || std::env::var("NEMO_INSECURE").is_ok();
    let http_client = client::NemoClient::new(&server_url, eng_config.api_key.as_deref(), insecure);

    // Validate engineer is configured for commands that need it
    // Status --team doesn't need engineer
    let needs_engineer = match &cli.command {
        Commands::Harden { .. }
        | Commands::Start { .. }
        | Commands::Ship { .. }
        | Commands::Auth { .. } => true,
        Commands::Status { team, .. } => !team,
        _ => false,
    };
    if needs_engineer && eng_config.engineer.is_empty() {
        anyhow::bail!(
            "Engineer name not configured. Run: nemo config --set engineer=<your-name>"
        );
    }

    match cli.command {
        Commands::Harden {
            spec_path,
            model_impl,
            model_review,
        } => {
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
        Commands::Logs {
            loop_id,
            round,
            stage,
        } => {
            commands::logs::run(&http_client, &loop_id, round, stage).await?;
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
        Commands::Init { force } => {
            commands::init::run(force)?;
        }
        Commands::Auth { claude, openai } => {
            commands::auth::run(&http_client, &eng_config.engineer, claude, openai).await?;
        }
        Commands::Config { set, get } => {
            commands::config::run(set, get)?;
        }
    }

    Ok(())
}
