mod api_types;
mod claude_creds;
mod client;
mod commands;
mod config;
mod project_config;

use clap::{CommandFactory, Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(
    name = "nemo",
    about = "Nemo CLI - Convergent loop orchestrator",
    version,
    disable_help_subcommand = true
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,

    /// API server URL override
    #[arg(long, short = 's', global = true)]
    server: Option<String>,

    /// Select a named profile for this invocation (overrides NAUTILOOP_PROFILE and current_profile)
    #[arg(long, global = true)]
    profile: Option<String>,

    /// Disable TLS certificate verification (dev/self-signed certs only)
    #[arg(long, global = true)]
    insecure: bool,

    /// Suppress recovery hints on API errors (for scripting)
    #[arg(long, global = true)]
    no_hints: bool,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Harden spec, merge spec PR. Terminal: HARDENED
    #[command(long_about = "Harden spec, merge spec PR. Terminal: HARDENED\n\n\
        Runs the harden phase (audit + optional revise) on a spec without proceeding\n\
        to implementation. The loop terminates at HARDENED once the spec PR is ready.\n\
        Use this for spec refinement before implementation.\n\n\
        Lifecycle: PENDING \u{2192} HARDENING \u{2192} HARDENED\n\n\
        Example:\n  \
          $ nemo harden spec.md\n  \
          Loop ID: 8cb88352-5cf4-4dda-9cd0-6a0d6851ba92\n  \
          Branch: agent/alice/my-feature-a1b2c3d4\n  \
          Phase plan: HARDEN (terminal)\n\n  \
          # Review the hardened spec PR when HARDENED.\n\n\
        See also: nemo start (harden + implement), nemo ship (harden + implement + merge).")]
    Harden {
        /// Path to the spec file
        spec_path: String,

        /// Override implementor model
        #[arg(long)]
        model_impl: Option<String>,

        /// Override reviewer model
        #[arg(long)]
        model_review: Option<String>,

        /// Per-stage Job `activeDeadlineSeconds` override in seconds.
        /// Applies uniformly to every stage (audit/revise/implement/test/review).
        /// Floored to 300s server-side. Default: cluster config (audit/review: 900s,
        /// implement/test: 1800s). Use when large specs need longer audits than
        /// the cluster default, e.g. `--stage-timeout 2700` for 45-minute audits.
        #[arg(long, value_name = "SECONDS")]
        stage_timeout: Option<u32>,
    },

    /// Implement spec, create PR. Terminal: CONVERGED
    #[command(long_about = "Implement spec, create PR. Terminal: CONVERGED\n\n\
        Submits a spec for implementation. By default, the spec is hardened first\n\
        (audit + optional revise), then moves to AWAITING_APPROVAL for engineer\n\
        sign-off before implementation begins. Use --no-harden to skip hardening.\n\n\
        Lifecycle: PENDING \u{2192} HARDENING \u{2192} AWAITING_APPROVAL \u{2192} IMPLEMENTING \u{2192} \
        TESTING \u{2192} REVIEWING \u{2192} CONVERGED\n\n\
        Example:\n  \
          $ nemo start spec.md\n  \
          Loop ID: 8cb88352-5cf4-4dda-9cd0-6a0d6851ba92\n  \
          Branch: agent/alice/my-feature-a1b2c3d4\n  \
          Phase plan: HARDEN \u{2192} APPROVE \u{2192} IMPLEMENT\n\n  \
          $ nemo start spec.md --no-harden\n  \
          Phase plan: APPROVE \u{2192} IMPLEMENT\n\n  \
          $ nemo start spec.md --auto-approve --no-harden\n  \
          Phase plan: IMPLEMENT (no approval gate)\n\n\
        See also: nemo approve (approve after hardening), nemo logs (watch progress).")]
    Start {
        /// Path to the spec file
        spec_path: String,

        /// Skip the harden phase (audit + optional revise) before implement.
        /// Default: harden runs first. Use when you've already hardened the spec
        /// or when audit-in-the-loop is not wanted for this run.
        #[arg(long)]
        no_harden: bool,

        /// Deprecated: harden is now the default. Passing this flag only emits a deprecation warning.
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

        /// Per-stage Job `activeDeadlineSeconds` override in seconds.
        /// Applies uniformly to every stage. Floored to 300s server-side.
        #[arg(long, value_name = "SECONDS")]
        stage_timeout: Option<u32>,
    },

    /// Implement + auto-merge. Terminal: SHIPPED
    #[command(long_about = "Implement + auto-merge. Terminal: SHIPPED\n\n\
        Fully autonomous mode: submits a spec, skips the approval gate, and auto-merges\n\
        the PR when the loop converges. Skips hardening by default; use --harden to\n\
        harden first.\n\n\
        Lifecycle: PENDING \u{2192} IMPLEMENTING \u{2192} TESTING \u{2192} REVIEWING \u{2192} CONVERGED \u{2192} SHIPPED\n\n\
        Example:\n  \
          $ nemo ship spec.md\n  \
          Loop ID: 8cb88352-5cf4-4dda-9cd0-6a0d6851ba92\n  \
          Phase plan: IMPLEMENT \u{2192} SHIP\n\n  \
          $ nemo ship --harden spec.md\n  \
          Phase plan: HARDEN \u{2192} IMPLEMENT \u{2192} SHIP\n\n\
        See also: nemo start (with approval gate), nemo status (check progress).")]
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

        /// Per-stage Job `activeDeadlineSeconds` override in seconds.
        /// Applies uniformly to every stage. Floored to 300s server-side.
        #[arg(long, value_name = "SECONDS")]
        stage_timeout: Option<u32>,
    },

    /// Show your running loops
    #[command(long_about = "Show your running loops.\n\n\
        Displays a table of all active loops for the current engineer (or all\n\
        engineers with --team). Shows loop ID, state, stage, spec path, and round.\n\n\
        Example:\n  \
          $ nemo status\n  \
          LOOP_ID                              STATE               STAGE       ENGINEER  SPEC                    ROUND\n  \
          8cb88352-5cf4-4dda-9cd0-6a0d6851ba92 IMPLEMENTING        implement   alice     specs/invoice-cancel.md 3\n\n  \
          $ nemo status --json\n  \
          [{\"loop_id\": \"8cb88352-...\", \"state\": \"IMPLEMENTING\", ...}]\n\n  \
          $ nemo status --team\n  \
          # Shows all engineers' loops\n\n\
        See also: nemo logs (stream logs), nemo inspect (detailed state), nemo helm (TUI).")]
    Status {
        /// Show all engineers' loops
        #[arg(long)]
        team: bool,

        /// Output as JSON
        #[arg(long)]
        json: bool,
    },

    /// K9s-style loop overview with live logs
    #[command(long_about = "K9s-style loop overview with live logs.\n\n\
        Opens an interactive terminal UI showing all active loops with real-time\n\
        log streaming, round history, diffs, and cost tracking. Supports keyboard\n\
        navigation and loop actions (approve, cancel, resume).\n\n\
        Example:\n  \
          $ nemo helm\n  \
          # Opens TUI for your loops\n\n  \
          $ nemo helm --team\n  \
          # Shows all engineers' loops in the TUI\n\n\
        See also: nemo status (non-interactive), nemo logs (single loop).")]
    Helm {
        /// Show all engineers' loops
        #[arg(long)]
        team: bool,
    },

    /// Stream logs for a loop
    #[command(long_about = "Stream logs for a loop.\n\n\
        Streams real-time logs from a running loop via SSE, or fetches historical\n\
        logs from completed rounds. Filter by round and stage to narrow output.\n\
        Use --tail for raw pod container stdout (live only). Use --follow with\n\
        --tail to stream stdout until the pod exits — useful for audit/review\n\
        stages that run `opencode --format json`, which buffers its NDJSON so a\n\
        one-shot --tail may return an empty body.\n\n\
        Example:\n  \
          $ nemo logs 8cb88352-5cf4-4dda-9cd0-6a0d6851ba92\n  \
          [implement/r1] Setting up worktree...\n  \
          [implement/r1] Running agent...\n\n  \
          $ nemo logs 8cb88352-... --round 2 --stage review\n  \
          [review/r2] Reviewing implementation...\n\n  \
          $ nemo logs 8cb88352-... --tail --follow\n  \
          # Streams stdout in real time (use for --format json audits)\n\n\
        See also: nemo status (find loop IDs), nemo ps (pod-level introspection).")]
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

        /// Stream pod stdout until the pod exits (requires --tail). Use this
        /// for stages that buffer output (e.g. `opencode --format json`)
        /// where a one-shot `--tail` returns an empty body because nothing
        /// has been flushed to the kubelet yet.
        #[arg(long)]
        follow: bool,

        /// Max lines to return with --tail (default 500, max 10000)
        #[arg(long, default_value_t = 500)]
        tail_lines: u32,

        /// Container to read from with --tail ("agent" or "auth-sidecar")
        #[arg(long, default_value = "agent")]
        container: String,
    },

    /// Show live processes and runtime state of an active loop's pod
    #[command(
        long_about = "Show live processes and runtime state of an active loop's pod.\n\n\
        Displays a snapshot of the active pod's CPU/memory usage, running processes,\n\
        worktree state, and container stats. Use --watch for live updating display.\n\n\
        Example:\n  \
          $ nemo ps 8cb88352-5cf4-4dda-9cd0-6a0d6851ba92\n  \
          Pod: nemo-8cb88352-r3-implement   Phase: Running\n  \
          CPU: 250m   Memory: 512Mi\n  \
          ...\n\n  \
          $ nemo ps 8cb88352-... --watch\n  \
          # Live updating view (press q to quit)\n\n\
        See also: nemo logs (log output), nemo inspect (round history)."
    )]
    Ps {
        /// Loop ID
        loop_id: String,

        /// Poll every 2s and redraw (press q to quit)
        #[arg(long)]
        watch: bool,
    },

    /// Cancel a running loop
    #[command(long_about = "Cancel a running loop.\n\n\
        Requests cancellation of an active loop. The loop engine will transition it\n\
        to CANCELLED on the next reconciliation tick. Only works on non-terminal loops.\n\n\
        Example:\n  \
          $ nemo cancel 8cb88352-5cf4-4dda-9cd0-6a0d6851ba92\n  \
          Cancel requested for loop 8cb88352-5cf4-4dda-9cd0-6a0d6851ba92\n  \
            Current state: IMPLEMENTING\n  \
            The loop engine will cancel the loop on the next tick.\n\n\
        See also: nemo status (find loop IDs), nemo resume (un-pause instead).")]
    Cancel {
        /// Loop ID
        loop_id: String,

        /// Output as JSON
        #[arg(long)]
        json: bool,
    },

    /// Approve a loop awaiting approval
    #[command(long_about = "Approve a loop awaiting approval.\n\n\
        Moves a loop from AWAITING_APPROVAL to the next active stage. Required for:\n\
        - Loops started with `nemo start` (PENDING \u{2192} AWAITING_APPROVAL \u{2192} approve \u{2192} IMPLEMENTING)\n\
        - Loops that hardened first and are waiting for engineer review of the hardened spec\n\n\
        Does nothing useful on any other state; errors with 409 Conflict.\n\n\
        Example:\n  \
          $ nemo approve 8cb88352-5cf4-4dda-9cd0-6a0d6851ba92\n  \
          Approved loop 8cb88352-5cf4-4dda-9cd0-6a0d6851ba92\n  \
            State: AWAITING_APPROVAL\n  \
            Implementation will start on next reconciliation tick.\n\n\
        See also: nemo status (find loop IDs), nemo logs (watch after approve).")]
    Approve {
        /// Loop ID
        loop_id: String,

        /// Output as JSON
        #[arg(long)]
        json: bool,
    },

    /// Show detailed loop state, round history, and verdicts
    #[command(
        long_about = "Show detailed loop state, round history, and verdicts.\n\n\
        Fetches the full inspection payload for a loop identified by its branch path.\n\
        Output is always JSON. Includes round-by-round stage results, durations, and\n\
        judge decisions. The \"agent/\" prefix is auto-prepended if not present.\n\n\
        Example:\n  \
          $ nemo inspect alice/invoice-cancel-a1b2c3d4\n  \
          {\n  \
            \"loop_id\": \"8cb88352-...\",\n  \
            \"state\": \"IMPLEMENTING\",\n  \
            \"rounds\": [...],\n  \
            \"judge_decisions\": [...]\n  \
          }\n\n\
        See also: nemo status (list loops), nemo logs (stream logs)."
    )]
    Inspect {
        /// Branch path (e.g., "alice/invoice-cancel-a1b2c3d4" or "agent/alice/invoice-cancel-a1b2c3d4")
        path: String,

        /// Accept --json for consistency (output is always JSON)
        #[arg(long)]
        json: bool,
    },

    /// Resume a PAUSED, AWAITING_REAUTH, or transient-FAILED loop
    #[command(
        long_about = "Resume a PAUSED, AWAITING_REAUTH, or transient-FAILED loop.\n\n\
        Resumes a loop that has been paused or is waiting for re-authentication.\n\
        For AWAITING_REAUTH, push fresh credentials with `nemo auth` first.\n\n\
        Example:\n  \
          $ nemo resume 8cb88352-5cf4-4dda-9cd0-6a0d6851ba92\n  \
          Resumed loop 8cb88352-5cf4-4dda-9cd0-6a0d6851ba92\n  \
            State: PAUSED\n  \
            Loop will resume on next reconciliation tick.\n\n\
        See also: nemo auth (re-push credentials), nemo extend (for FAILED loops)."
    )]
    Resume {
        /// Loop ID
        loop_id: String,

        /// Output as JSON
        #[arg(long)]
        json: bool,

        /// Raise the per-stage Job `activeDeadlineSeconds` before resuming.
        /// Use this to recover from a `StageDeadlineExceeded` failure on a
        /// stage whose wall-clock budget was too small — e.g. a large spec
        /// whose audit exceeded the 900s default. Floored to 300s server-side.
        #[arg(long, value_name = "SECONDS")]
        stage_timeout: Option<u32>,
    },

    /// Extend a FAILED loop's max_rounds and resume it from the last stage
    #[command(
        long_about = "Extend a FAILED loop's max_rounds and resume it from the last stage.\n\n\
        Adds rounds to a FAILED loop and resumes it from the stage it was in when it\n\
        failed (failed_from_state). Use this to recover from max-rounds exhaustion\n\
        without starting over.\n\n\
        Example:\n  \
          $ nemo extend 8cb88352-5cf4-4dda-9cd0-6a0d6851ba92 --add 10\n  \
          Extended loop 8cb88352-5cf4-4dda-9cd0-6a0d6851ba92\n  \
            max_rounds: 10 -> 20 (+10)\n  \
            Resuming at: IMPLEMENTING\n  \
            Loop will continue on next reconciliation tick.\n\n\
        See also: nemo inspect (check what went wrong), nemo logs (review failures)."
    )]
    Extend {
        /// Loop ID
        loop_id: String,
        /// Number of rounds to add to max_rounds
        #[arg(long, default_value_t = 5)]
        add: u32,

        /// Output as JSON
        #[arg(long)]
        json: bool,
    },

    /// Scan monorepo, generate nemo.toml
    #[command(long_about = "Scan monorepo, generate nemo.toml.\n\n\
        Auto-detects the repository name, default branch, and project structure to\n\
        generate a nemo.toml configuration file. Use --force to overwrite an existing\n\
        config.\n\n\
        Example:\n  \
          $ nemo init\n  \
          Detected repo: my-project (branch: main)\n  \
          Generated nemo.toml\n\n  \
          $ nemo init --force\n  \
          Overwriting existing nemo.toml\n\n\
        See also: nemo config (edit engineer config), nemo auth (push credentials).")]
    Init {
        /// Overwrite existing nemo.toml
        #[arg(long)]
        force: bool,
    },

    /// Push local model credentials to cluster
    #[command(long_about = "Push local model credentials to cluster.\n\n\
        Reads local credential files for Claude, OpenAI, and SSH, validates them,\n\
        and registers them with the control plane. Required for loops to authenticate\n\
        with model providers. Use provider-specific flags to push only one provider.\n\n\
        Example:\n  \
          $ nemo auth\n  \
          Registered claude credentials with control plane\n  \
          Registered openai credentials with control plane\n  \
          Registered ssh credentials with control plane\n\n  \
          $ nemo auth --claude\n  \
          Registered claude credentials with control plane\n\n  \
          $ nemo auth --json\n  \
          {\"results\": [{\"provider\": \"claude\", \"status\": \"ok\", ...}]}\n\n\
        See also: nemo models (check provider status), nemo resume (resume after re-auth).")]
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

        /// Output as JSON
        #[arg(long)]
        json: bool,
    },

    /// Show authenticated providers and available models
    #[command(long_about = "Show authenticated providers and available models.\n\n\
        Displays which model providers (Claude, OpenAI, SSH) have valid credentials\n\
        registered with the control plane, plus the catalog of available models.\n\n\
        Example:\n  \
          $ nemo models\n  \
          Authenticated Providers\n  \
          ========================\n  \
            \u{2713} claude   ~/.claude/.credentials.json [control plane: valid]\n  \
            \u{2717} openai   not found\n  \
            \u{2713} ssh      ~/.ssh/id_ed25519 [control plane: valid]\n\n  \
          $ nemo models --json\n  \
          {\"providers\": [{\"provider\": \"claude\", \"models\": [...], \"valid\": true, ...}]}\n\n\
        See also: nemo auth (push credentials), nemo config (set model preferences).")]
    Models {
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },

    /// Show cache configuration and disk usage
    Cache {
        #[command(subcommand)]
        action: CacheAction,
    },

    /// Edit ~/.nemo/config.toml
    #[command(long_about = "Edit ~/.nemo/config.toml.\n\n\
        View or modify engineer-level configuration (server URL, API key, engineer name,\n\
        model preferences). Without arguments, displays current config. Use --get to\n\
        read a specific key, --set to write one.\n\n\
        Example:\n  \
          $ nemo config\n  \
          Active profile: work\n  \
          server_url: https://nemo.example.com:8080\n  \
          api_key: abc1...789z\n\n  \
          $ nemo config --get engineer\n  \
          alice\n\n  \
          $ nemo config --set engineer=bob\n  \
          Set engineer = bob\n\n\
        See also: nemo init (generate nemo.toml), nemo auth (push credentials).")]
    Config {
        /// Set a config value
        #[arg(long)]
        set: Option<String>,

        /// Get a config value
        #[arg(long)]
        get: Option<String>,

        /// Show full API key (disable redaction)
        #[arg(long)]
        unmask: bool,
    },

    /// Manage named profiles for multiple clusters
    #[command(long_about = "Manage named profiles for multiple clusters.\n\n\
        Profiles allow switching between nautiloop clusters without editing config files.\n\
        Each profile stores a server URL, API key, and engineer identity.\n\n\
        Example:\n  \
          $ nemo profile ls\n  \
          * work      https://nautiloop.work.internal  ggylfason\n  \
            personal  http://100.64.1.10:8080          gunnar\n\n  \
          $ nemo profile add staging --server https://staging.example.com --api-key xyz --engineer alice\n  \
          Added profile 'staging'.\n\n  \
          $ nemo profile show work\n  \
          Profile: work (active)\n  \
            server_url: https://nautiloop.work.internal\n  \
            api_key: abc1...789z\n\n\
        See also: nemo use-profile (switch active profile), nemo config (view/edit config).")]
    Profile {
        #[command(subcommand)]
        action: ProfileAction,
    },

    /// Switch the active profile
    #[command(
        name = "use-profile",
        long_about = "Switch the active profile.\n\n\
            Sets the active profile in ~/.nemo/config.toml. All subsequent commands\n\
            will use this profile's server URL, API key, and engineer identity.\n\n\
            Example:\n  \
              $ nemo use-profile work\n  \
              Active profile: work (https://nautiloop.work.internal).\n\n  \
              $ nemo use-profile dev\n  \
              Active profile: dev (http://localhost:18080).\n\n\
            See also: nemo profile ls (list profiles), nemo profile add (create profile)."
    )]
    UseProfile {
        /// Profile name to activate
        name: String,
    },

    /// Show CLI version and supported features
    #[command(long_about = "Show CLI version and supported features.\n\n\
        Outputs a JSON object describing which features this CLI version supports.\n\
        Agents can check this once at startup to know what commands and capabilities\n\
        are available without version-sniffing.\n\n\
        Example:\n  \
          $ nemo capabilities\n  \
          {\n  \
            \"version\": \"0.6.0\",\n  \
            \"commands\": [\"harden\", \"start\", ...],\n  \
            \"features\": {\n  \
              \"qa_stage\": false,\n  \
              \"harden_by_default\": true,\n  \
              ...\n  \
            }\n  \
          }\n\n\
        See also: nemo help ai (full LLM operator guide).")]
    Capabilities,

    /// Show help for nemo or a specific command
    #[command(
        name = "help",
        long_about = "Show help for nemo or a specific command.\n\n\
            Use `nemo help ai` for the full LLM operator guide.\n\
            Use `nemo help --all` to dump all command documentation.\n\
            Use `nemo help <command>` for help on a specific command."
    )]
    Help {
        /// Command or topic to show help for (e.g., "ai", "approve", "cache show")
        #[arg(num_args = 0..)]
        topic: Vec<String>,

        /// Dump all commands' help text
        #[arg(long)]
        all: bool,

        /// Output format (json)
        #[arg(long)]
        format: Option<String>,
    },
}

#[derive(Debug, Subcommand)]
enum ProfileAction {
    /// List all profiles
    #[command(
        alias = "list",
        long_about = "List all profiles.\n\n\
        Shows all configured profiles with their server URL and engineer name.\n\
        The active profile is marked with *.\n\n\
        Example:\n  \
          $ nemo profile ls\n  \
            default   http://localhost:18080           dev\n  \
          * work      https://nautiloop.work.internal  ggylfason\n  \
            personal  http://100.64.1.10:8080          gunnar"
    )]
    Ls,

    /// Show profile details
    #[command(long_about = "Show profile details.\n\n\
        Prints the full configuration for a profile. Omit the name to show the\n\
        active profile. API key is redacted by default; use --unmask to reveal.\n\n\
        Example:\n  \
          $ nemo profile show work\n  \
          Profile: work (active)\n  \
            server_url: https://nautiloop.work.internal\n  \
            api_key: abc1...789z\n  \
            engineer: ggylfason")]
    Show {
        /// Profile name (default: active profile)
        name: Option<String>,

        /// Show full API key (disable redaction)
        #[arg(long)]
        unmask: bool,
    },

    /// Add a new profile
    #[command(long_about = "Add a new profile.\n\n\
        Creates a new named profile with connection details. --server, --api-key,\n\
        and --engineer are required. --name and --email default to the current\n\
        profile's values. Use --switch to activate the new profile immediately.\n\n\
        Example:\n  \
          $ nemo profile add work --server https://nautiloop.work.internal \\\n  \
              --api-key xyz789 --engineer ggylfason\n  \
          Added profile 'work'.")]
    Add {
        /// Profile name
        name: String,

        /// Server URL (required)
        #[arg(long)]
        server: String,

        /// API key (required)
        #[arg(long)]
        api_key: String,

        /// Engineer identifier (required)
        #[arg(long)]
        engineer: String,

        /// Display name (defaults to current profile's name)
        #[arg(long = "name")]
        name_field: Option<String>,

        /// Email (defaults to current profile's email)
        #[arg(long)]
        email: Option<String>,

        /// Switch to this profile after creating it
        #[arg(long)]
        switch: bool,
    },

    /// Remove a profile
    #[command(long_about = "Remove a profile.\n\n\
        Removes a named profile from the config. Cannot remove the active profile\n\
        or the last remaining profile.\n\n\
        Example:\n  \
          $ nemo profile rm staging\n  \
          Removed profile 'staging'.")]
    Rm {
        /// Profile name to remove
        name: String,
    },

    /// Rename a profile
    #[command(long_about = "Rename a profile.\n\n\
        Renames an existing profile. If the renamed profile is active, the active\n\
        profile reference is updated automatically.\n\n\
        Example:\n  \
          $ nemo profile rename work production\n  \
          Renamed profile 'work' to 'production'.")]
    Rename {
        /// Current profile name
        old: String,
        /// New profile name
        new: String,
    },

    /// Switch to a profile (alias for `nemo use-profile`)
    #[command(long_about = "Switch to a profile.\n\n\
        Sets the active profile. Equivalent to `nemo use-profile <name>`.\n\n\
        Example:\n  \
          $ nemo profile use work\n  \
          Active profile: work (https://nautiloop.work.internal).")]
    Use {
        /// Profile name to activate
        name: String,
    },
}

#[derive(Debug, Subcommand)]
enum CacheAction {
    /// Show active cache configuration and disk usage
    #[command(long_about = "Show active cache configuration and disk usage.\n\n\
        Displays the current cache backend configuration, volume details, and\n\
        per-subdirectory disk usage breakdown.\n\n\
        Example:\n  \
          $ nemo cache show\n  \
          Cache: enabled\n  \
          Volume: nemo-cache (10 Gi)\n  \
          Disk usage: 2.3 GB\n\n  \
          $ nemo cache show --json\n  \
          {\"disabled\": false, \"volume_name\": \"nemo-cache\", ...}\n\n\
        See also: nemo config (general configuration).")]
    Show {
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },
}

/// Build the help-all JSON output from clap's Command tree.
fn build_help_all_json(root: &clap::Command) -> serde_json::Value {
    let mut global_options = Vec::new();
    for arg in root.get_arguments() {
        if arg.get_id() == "help" || arg.get_id() == "version" {
            continue;
        }
        let long = arg.get_long().map(|l| format!("--{l}")).unwrap_or_default();
        if long.is_empty() {
            continue;
        }
        let short = arg.get_short().map(|s| format!("-{s}"));
        let arg_type = match arg.get_action() {
            clap::ArgAction::SetTrue | clap::ArgAction::SetFalse => "bool",
            _ => "string",
        };
        global_options.push(serde_json::json!({
            "name": long,
            "short": short,
            "type": arg_type,
            "required": arg.is_required_set(),
            "description": arg.get_help().map(|h| h.to_string()).unwrap_or_default(),
        }));
    }

    let mut commands_map = serde_json::Map::new();
    for sub in root.get_subcommands() {
        let name = sub.get_name().to_string();
        let short = sub.get_about().map(|a| a.to_string()).unwrap_or_default();
        let long = sub
            .get_long_about()
            .map(|a| a.to_string())
            .unwrap_or_else(|| short.clone());

        let mut options = Vec::new();
        let mut positional_args = Vec::new();

        for arg in sub.get_arguments() {
            if arg.get_id() == "help" || arg.get_id() == "version" {
                continue;
            }
            let is_positional = arg.get_long().is_none() && arg.get_short().is_none();
            let arg_type = match arg.get_action() {
                clap::ArgAction::SetTrue | clap::ArgAction::SetFalse => "bool",
                _ => "string",
            };

            if is_positional {
                positional_args.push(serde_json::json!({
                    "name": arg.get_id().to_string().to_uppercase(),
                    "required": arg.is_required_set(),
                    "description": arg.get_help().map(|h| h.to_string()).unwrap_or_default(),
                }));
            } else {
                let long_name = arg.get_long().map(|l| format!("--{l}")).unwrap_or_default();
                let short_name = arg.get_short().map(|s| format!("-{s}"));
                options.push(serde_json::json!({
                    "name": long_name,
                    "short": short_name,
                    "type": arg_type,
                    "required": arg.is_required_set(),
                    "description": arg.get_help().map(|h| h.to_string()).unwrap_or_default(),
                }));
            }
        }

        // Handle nested subcommands (e.g., cache show, profile ls)
        for nested_sub in sub.get_subcommands() {
            let nested_name = format!("{} {}", name, nested_sub.get_name());
            let nested_short = nested_sub
                .get_about()
                .map(|a| a.to_string())
                .unwrap_or_default();
            let nested_long = nested_sub
                .get_long_about()
                .map(|a| a.to_string())
                .unwrap_or_else(|| nested_short.clone());

            let mut nested_options = Vec::new();
            let mut nested_positional = Vec::new();

            for arg in nested_sub.get_arguments() {
                if arg.get_id() == "help" || arg.get_id() == "version" {
                    continue;
                }
                let is_positional = arg.get_long().is_none() && arg.get_short().is_none();
                let arg_type = match arg.get_action() {
                    clap::ArgAction::SetTrue | clap::ArgAction::SetFalse => "bool",
                    _ => "string",
                };
                if is_positional {
                    nested_positional.push(serde_json::json!({
                        "name": arg.get_id().to_string().to_uppercase(),
                        "required": arg.is_required_set(),
                        "description": arg.get_help().map(|h| h.to_string()).unwrap_or_default(),
                    }));
                } else {
                    let long_name = arg.get_long().map(|l| format!("--{l}")).unwrap_or_default();
                    let short_name = arg.get_short().map(|s| format!("-{s}"));
                    nested_options.push(serde_json::json!({
                        "name": long_name,
                        "short": short_name,
                        "type": arg_type,
                        "required": arg.is_required_set(),
                        "description": arg.get_help().map(|h| h.to_string()).unwrap_or_default(),
                    }));
                }
            }

            commands_map.insert(
                nested_name,
                serde_json::json!({
                    "short": nested_short,
                    "long": nested_long,
                    "options": nested_options,
                    "positional_args": nested_positional,
                }),
            );
        }

        commands_map.insert(
            name,
            serde_json::json!({
                "short": short,
                "long": long,
                "options": options,
                "positional_args": positional_args,
            }),
        );
    }

    serde_json::json!({
        "global_options": global_options,
        "commands": commands_map,
    })
}

/// Build the help-all Markdown output.
///
/// Uses clap's `render_long_help()` for each subcommand so the output matches
/// what `nemo help <cmd>` produces (including flags, positional args, and usage).
fn build_help_all_markdown(root: &clap::Command) -> String {
    let mut out = String::new();
    for sub in root.get_subcommands() {
        let name = sub.get_name();
        out.push_str(&format!("## {name}\n\n"));
        out.push_str(&sub.clone().render_long_help().to_string());
        out.push_str("\n\n");

        // Nested subcommands
        for nested in sub.get_subcommands() {
            let nested_name = nested.get_name();
            out.push_str(&format!("### {name} {nested_name}\n\n"));
            out.push_str(&nested.clone().render_long_help().to_string());
            out.push_str("\n\n");
        }
    }
    out
}

/// Handle the custom help subcommand.
fn handle_help(topic: &[String], all: bool, format: Option<&str>) -> anyhow::Result<()> {
    let is_json = format.map(|f| f == "json").unwrap_or(false);

    // Validate format value
    if let Some(f) = format
        && f != "json"
    {
        anyhow::bail!("Unknown format: {f}. Supported: json");
    }

    // Error: --all and a topic are mutually exclusive
    if all && !topic.is_empty() {
        let topic_str = topic.join(" ");
        anyhow::bail!(
            "--all and a specific {} are mutually exclusive",
            if topic_str == "ai" || topic_str == "llm" {
                "topic"
            } else {
                "command"
            }
        );
    }

    // Error: --format without --all or ai topic
    if format.is_some() && !all && topic.is_empty() {
        anyhow::bail!("--format requires --all or a topic (e.g., 'ai')");
    }

    // nemo help ai / nemo help llm
    if !topic.is_empty() && (topic[0] == "ai" || topic[0] == "llm") {
        if topic.len() > 1 {
            anyhow::bail!(
                "'nemo help {}' does not accept additional arguments",
                topic[0]
            );
        }
        if is_json {
            return commands::help_ai::render_json();
        }
        commands::help_ai::render_markdown();
        return Ok(());
    }

    // Error: --format with a specific command
    if format.is_some() && !topic.is_empty() {
        anyhow::bail!("--format is only supported with --all or 'ai'");
    }

    let root = Cli::command();

    // nemo help --all
    if all {
        if is_json {
            let json = build_help_all_json(&root);
            println!("{}", serde_json::to_string_pretty(&json)?);
        } else {
            print!("{}", build_help_all_markdown(&root));
        }
        return Ok(());
    }

    // nemo help (no args) — same as nemo --help
    if topic.is_empty() {
        let mut cmd = Cli::command();
        cmd.print_long_help()?;
        return Ok(());
    }

    // nemo help <cmd> [subcmd] — resolve nested subcommand chain
    let mut current = root;
    for token in topic {
        match current.find_subcommand(token) {
            Some(sub) => current = sub.clone(),
            None => {
                let topic_str = topic.join(" ");
                anyhow::bail!(
                    "Unknown command: {topic_str}. Run 'nemo help' for a list of commands."
                );
            }
        }
    }

    // Render the found subcommand's long help
    current.print_long_help()?;
    Ok(())
}

/// The inner run function that dispatches commands. Returns errors that main()
/// will handle (including ApiError for hint enrichment).
async fn run(cli: Cli) -> anyhow::Result<()> {
    // Fail fast on contradictory flags before any side effects (config loading,
    // credential checks, HTTP client construction).
    if let Commands::Start {
        harden, no_harden, ..
    } = cli.command
    {
        commands::start::validate_harden_flags(harden, no_harden)?;
    }

    // --- NFR-2: Help and Capabilities bypass config and auth ---

    // Handle help subcommand before config loading
    if let Commands::Help {
        ref topic,
        all,
        ref format,
    } = cli.command
    {
        return handle_help(topic, all, format.as_deref());
    }

    // Handle capabilities before config loading
    if let Commands::Capabilities = cli.command {
        let cmd = Cli::command();
        return commands::capabilities::run(&cmd);
    }

    // Handle config command before loading config — a broken config file
    // must not prevent `nemo config --set` from working. (FR-6e)
    if let Commands::Config {
        ref set,
        ref get,
        unmask,
    } = cli.command
    {
        return commands::config::run(set.clone(), get.clone(), cli.profile.as_deref(), unmask);
    }

    // Init is local-only — don't require config
    if let Commands::Init { force } = cli.command {
        return commands::init::run(force);
    }

    // --- Profile management commands: load config directly, no API client needed ---

    // Profile subcommands
    if let Commands::Profile { ref action } = cli.command {
        let mut nemo_config = config::load_config()?;
        match action {
            ProfileAction::Ls => {
                return commands::profile::run_list(&nemo_config);
            }
            ProfileAction::Show { name, unmask } => {
                return commands::profile::run_show(
                    &nemo_config,
                    name.as_deref(),
                    cli.profile.as_deref(),
                    *unmask,
                );
            }
            ProfileAction::Add {
                name,
                server,
                api_key,
                engineer,
                name_field,
                email,
                switch,
            } => {
                return commands::profile::run_add(
                    &mut nemo_config,
                    name,
                    server,
                    api_key,
                    engineer,
                    name_field.clone(),
                    email.clone(),
                    *switch,
                );
            }
            ProfileAction::Rm { name } => {
                return commands::profile::run_remove(&mut nemo_config, name);
            }
            ProfileAction::Rename { old, new } => {
                return commands::profile::run_rename(&mut nemo_config, old, new);
            }
            ProfileAction::Use { name } => {
                return commands::profile::run_use_profile(&mut nemo_config, name);
            }
        }
    }

    // use-profile top-level (FR-6f)
    if let Commands::UseProfile { ref name } = cli.command {
        let mut nemo_config = config::load_config()?;
        return commands::profile::run_use_profile(&mut nemo_config, name);
    }

    // --- Normal commands: load config + resolve profile ---

    let nemo_config = config::load_config()?;
    let profile_flag = cli.profile.as_deref();
    let (profile_name, active_profile) = nemo_config.active_profile(profile_flag)?;

    let server_url = cli
        .server
        .clone()
        .unwrap_or_else(|| active_profile.server_url.clone());

    let insecure =
        cli.insecure || matches!(std::env::var("NEMO_INSECURE").as_deref(), Ok("true" | "1"));

    // Warn early if api_key is missing
    if active_profile.api_key.is_none() {
        anyhow::bail!("API key not configured. Run: nemo config --set api_key=<your-key>");
    }

    let http_client =
        client::NemoClient::new(&server_url, active_profile.api_key.as_deref(), insecure)?;

    let engineer = &active_profile.engineer;
    let eng_name = active_profile.name.as_deref().unwrap_or("");
    let eng_email = active_profile.email.as_deref().unwrap_or("");

    // Validate engineer is configured for commands that need it
    let needs_engineer = match &cli.command {
        Commands::Harden { .. }
        | Commands::Start { .. }
        | Commands::Ship { .. }
        | Commands::Auth { .. } => true,
        Commands::Helm { team } => !team,
        Commands::Status { team, .. } => !team,
        _ => false,
    };
    if needs_engineer && engineer.is_empty() {
        anyhow::bail!("Engineer name not configured. Run: nemo config --set engineer=<your-name>");
    }

    match cli.command {
        Commands::Harden {
            spec_path,
            model_impl,
            model_review,
            stage_timeout,
        } => {
            let (model_impl, model_review) =
                project_config::resolve_models(model_impl, model_review, &nemo_config.models)?;
            let project_timeouts =
                project_config::load_project_timeouts(&std::env::current_dir()?)?;
            let project_cache_env =
                project_config::load_project_cache_env(&std::env::current_dir()?)?;
            claude_creds::ensure_fresh(&http_client, engineer, eng_name, eng_email).await?;
            commands::start::run(
                &http_client,
                commands::start::StartArgs {
                    engineer,
                    spec_path: &spec_path,
                    harden: true,
                    harden_only: true,
                    auto_approve: false,
                    ship_mode: false,
                    model_impl,
                    model_review,
                    stage_timeout_secs: stage_timeout,
                    project_timeouts,
                    project_cache_env,
                },
            )
            .await?;
        }
        Commands::Start {
            spec_path,
            no_harden,
            harden,
            auto_approve,
            model_impl,
            model_review,
            stage_timeout,
        } => {
            if let Some(warning) = commands::start::deprecation_warning(harden) {
                eprintln!("{warning}");
            }
            let (model_impl, model_review) =
                project_config::resolve_models(model_impl, model_review, &nemo_config.models)?;
            let project_timeouts =
                project_config::load_project_timeouts(&std::env::current_dir()?)?;
            let project_cache_env =
                project_config::load_project_cache_env(&std::env::current_dir()?)?;
            claude_creds::ensure_fresh(&http_client, engineer, eng_name, eng_email).await?;
            commands::start::run(
                &http_client,
                commands::start::StartArgs {
                    engineer,
                    spec_path: &spec_path,
                    harden: !no_harden,
                    harden_only: false,
                    auto_approve,
                    ship_mode: false,
                    model_impl,
                    model_review,
                    stage_timeout_secs: stage_timeout,
                    project_timeouts,
                    project_cache_env,
                },
            )
            .await?;
        }
        Commands::Ship {
            spec_path,
            harden,
            model_impl,
            model_review,
            stage_timeout,
        } => {
            let (model_impl, model_review) =
                project_config::resolve_models(model_impl, model_review, &nemo_config.models)?;
            let project_timeouts =
                project_config::load_project_timeouts(&std::env::current_dir()?)?;
            let project_cache_env =
                project_config::load_project_cache_env(&std::env::current_dir()?)?;
            claude_creds::ensure_fresh(&http_client, engineer, eng_name, eng_email).await?;
            commands::start::run(
                &http_client,
                commands::start::StartArgs {
                    engineer,
                    spec_path: &spec_path,
                    harden,
                    harden_only: false,
                    auto_approve: true,
                    ship_mode: true,
                    model_impl,
                    model_review,
                    stage_timeout_secs: stage_timeout,
                    project_timeouts,
                    project_cache_env,
                },
            )
            .await?;
        }
        Commands::Status { team, json } => {
            // FR-5a: profile header on stderr
            eprintln!("# Profile: {} \u{00b7} {}", profile_name, server_url);
            commands::status::run(&http_client, engineer, team, json).await?;
        }
        Commands::Helm { team } => {
            commands::helm::run(
                &http_client,
                engineer,
                team,
                &nemo_config.helm,
                profile_name,
            )
            .await?;
        }
        Commands::Logs {
            loop_id,
            round,
            stage,
            tail,
            follow,
            tail_lines,
            container,
        } => {
            if follow && !tail {
                anyhow::bail!(
                    "--follow requires --tail (it streams pod stdout); drop --follow for SSE streaming via the historical log path"
                );
            }
            if tail {
                if round.is_some() || stage.is_some() {
                    anyhow::bail!(
                        "--round / --stage are not supported with --tail (pod stdout is unstructured); \
                         run without --tail for filtered historical logs"
                    );
                }
                match commands::logs::run_tail(
                    &http_client,
                    &loop_id,
                    tail_lines,
                    &container,
                    follow,
                )
                .await
                {
                    Ok(commands::logs::TailResult::Ok) => {}
                    Ok(commands::logs::TailResult::NoPod) => {
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
        Commands::Cancel { loop_id, json } => {
            commands::cancel::run(&http_client, &loop_id, json).await?;
        }
        Commands::Approve { loop_id, json } => {
            commands::approve::run(&http_client, &loop_id, json).await?;
        }
        Commands::Inspect { path, json } => {
            commands::inspect::run(&http_client, &path, json).await?;
        }
        Commands::Resume {
            loop_id,
            json,
            stage_timeout,
        } => {
            commands::resume::run(&http_client, &loop_id, json, stage_timeout).await?;
        }
        Commands::Extend { loop_id, add, json } => {
            commands::extend::run(&http_client, &loop_id, add, json).await?;
        }
        Commands::Init { .. } => {
            unreachable!("Init is dispatched before config loading");
        }
        Commands::Auth {
            claude,
            openai,
            ssh,
            json,
        } => {
            commands::auth::run(
                &http_client,
                engineer,
                eng_name,
                eng_email,
                claude,
                openai,
                ssh,
                json,
            )
            .await?;
        }
        Commands::Cache { action } => match action {
            CacheAction::Show { json } => {
                commands::cache::run(&http_client, json).await?;
            }
        },
        Commands::Models { json } => {
            commands::models::run_with_models(&http_client, engineer, json).await?;
        }
        Commands::Config { .. } => {
            unreachable!("Config is dispatched before config loading");
        }
        Commands::Profile { .. } => {
            unreachable!("Profile is dispatched before config loading");
        }
        Commands::UseProfile { .. } => {
            unreachable!("UseProfile is dispatched before config loading");
        }
        Commands::Help { .. } => {
            unreachable!("Help is dispatched before config loading");
        }
        Commands::Capabilities => {
            unreachable!("Capabilities is dispatched before config loading");
        }
    }

    Ok(())
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .init();

    let cli = Cli::parse();
    let no_hints = cli.no_hints;

    if let Err(err) = run(cli).await {
        // Try to extract ApiError for hint enrichment
        if !no_hints
            && let Some(api_err) = err.downcast_ref::<client::ApiError>()
            && let Some(hint) = commands::error_hints::find_hint(api_err.status, &api_err.body)
        {
            eprintln!("Error: {api_err}");
            eprintln!("Hint: {hint}");
            std::process::exit(1);
        }
        eprintln!("Error: {err}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Verify that every subcommand has a long_about containing "Example:".
    #[test]
    fn all_commands_have_examples() {
        let cmd = Cli::command();
        for sub in cmd.get_subcommands() {
            let name = sub.get_name();
            // help doesn't need examples in long_about (it IS the help system)
            // cache and profile parents don't need examples (their subcommands do)
            if name == "help" || name == "cache" || name == "profile" {
                continue;
            }
            let long = sub
                .get_long_about()
                .map(|a| a.to_string())
                .unwrap_or_default();
            assert!(
                long.contains("Example:"),
                "Command '{name}' is missing 'Example:' in long_about"
            );
        }
    }

    /// Verify that nested subcommands (cache show, profile ls) have examples too.
    #[test]
    fn nested_commands_have_examples() {
        let cmd = Cli::command();
        for parent_name in &["cache", "profile"] {
            let parent = cmd.find_subcommand(parent_name).unwrap_or_else(|| {
                panic!("{parent_name} subcommand not found");
            });
            for nested in parent.get_subcommands() {
                let name = nested.get_name();
                let long = nested
                    .get_long_about()
                    .map(|a| a.to_string())
                    .unwrap_or_default();
                assert!(
                    long.contains("Example:") || long.contains("Example\n"),
                    "Nested command '{parent_name} {name}' is missing 'Example:' in long_about"
                );
            }
        }
    }

    /// Verify help --all --format=json produces valid JSON with expected keys.
    #[test]
    fn help_all_json_has_expected_structure() {
        let root = Cli::command();
        let json = build_help_all_json(&root);
        assert!(json.get("global_options").is_some());
        assert!(json.get("commands").is_some());

        let commands = json["commands"].as_object().unwrap();
        // Spot-check some commands exist
        assert!(commands.contains_key("approve"));
        assert!(commands.contains_key("status"));
        assert!(commands.contains_key("capabilities"));
        assert!(commands.contains_key("help"));
        assert!(commands.contains_key("profile"));
        assert!(commands.contains_key("use-profile"));

        // Check structure of one command
        let approve = &commands["approve"];
        assert!(approve.get("short").is_some());
        assert!(approve.get("long").is_some());
        assert!(approve.get("options").is_some());
        assert!(approve.get("positional_args").is_some());
    }

    /// Verify capabilities command's commands list matches actual subcommands.
    #[test]
    fn capabilities_commands_match_subcommands() {
        let root = Cli::command();
        let subcommand_names: Vec<String> = root
            .get_subcommands()
            .map(|c| c.get_name().to_string())
            .collect();
        assert!(subcommand_names.contains(&"capabilities".to_string()));
        assert!(subcommand_names.contains(&"help".to_string()));
        assert!(subcommand_names.contains(&"profile".to_string()));
        assert!(subcommand_names.contains(&"use-profile".to_string()));
    }
}
