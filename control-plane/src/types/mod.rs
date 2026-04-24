pub mod api;
pub mod verdict;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::time::Duration;
use uuid::Uuid;

/// The top-level state of a convergent loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, sqlx::Type)]
#[sqlx(type_name = "loop_state", rename_all = "SCREAMING_SNAKE_CASE")]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum LoopState {
    Pending,
    Hardening,
    AwaitingApproval,
    Implementing,
    Testing,
    Reviewing,
    Converged,
    Failed,
    Cancelled,
    Paused,
    AwaitingReauth,
    Hardened,
    Shipped,
}

impl fmt::Display for LoopState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Pending => write!(f, "PENDING"),
            Self::Hardening => write!(f, "HARDENING"),
            Self::AwaitingApproval => write!(f, "AWAITING_APPROVAL"),
            Self::Implementing => write!(f, "IMPLEMENTING"),
            Self::Testing => write!(f, "TESTING"),
            Self::Reviewing => write!(f, "REVIEWING"),
            Self::Converged => write!(f, "CONVERGED"),
            Self::Failed => write!(f, "FAILED"),
            Self::Cancelled => write!(f, "CANCELLED"),
            Self::Paused => write!(f, "PAUSED"),
            Self::AwaitingReauth => write!(f, "AWAITING_REAUTH"),
            Self::Hardened => write!(f, "HARDENED"),
            Self::Shipped => write!(f, "SHIPPED"),
        }
    }
}

impl LoopState {
    /// Whether this state is terminal (no further transitions possible).
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Converged | Self::Failed | Self::Cancelled | Self::Hardened | Self::Shipped
        )
    }

    /// Whether this state is an active stage that has sub-states.
    pub fn is_active_stage(self) -> bool {
        matches!(
            self,
            Self::Hardening | Self::Implementing | Self::Testing | Self::Reviewing
        )
    }
}

/// Sub-state within an active stage (HARDENING, IMPLEMENTING, TESTING, REVIEWING).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type)]
#[sqlx(type_name = "sub_state", rename_all = "SCREAMING_SNAKE_CASE")]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum SubState {
    Dispatched,
    Running,
    Completed,
}

impl fmt::Display for SubState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Dispatched => write!(f, "DISPATCHED"),
            Self::Running => write!(f, "RUNNING"),
            Self::Completed => write!(f, "COMPLETED"),
        }
    }
}

/// The two kinds of convergent loops.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type)]
#[sqlx(type_name = "loop_kind", rename_all = "snake_case")]
#[serde(rename_all = "snake_case")]
pub enum LoopKind {
    Harden,
    Implement,
}

/// Stage name mapping between short names (used in jobs, API, logs, prompts)
/// and DB enum values (used in Postgres `loop_stage` column).
///
/// | Short name | DB enum value |
/// |------------|---------------|
/// | implement  | implementing  |
/// | test       | testing       |
/// | review     | reviewing     |
/// | audit      | spec_audit    |
/// | revise     | spec_revise   |
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Stage {
    Implement,
    Test,
    Review,
    Audit,
    Revise,
}

impl Stage {
    /// Short name used in jobs, API, logs, and prompt template filenames.
    pub fn short_name(self) -> &'static str {
        match self {
            Self::Implement => "implement",
            Self::Test => "test",
            Self::Review => "review",
            Self::Audit => "audit",
            Self::Revise => "revise",
        }
    }

    /// DB enum value used in Postgres `loop_stage` column.
    pub fn db_name(self) -> &'static str {
        match self {
            Self::Implement => "implementing",
            Self::Test => "testing",
            Self::Review => "reviewing",
            Self::Audit => "spec_audit",
            Self::Revise => "spec_revise",
        }
    }

    /// Prompt template filename (without directory).
    pub fn prompt_filename(self) -> &'static str {
        match self {
            Self::Implement => "implement.md",
            Self::Test => "test.md",
            Self::Review => "review.md",
            Self::Audit => "spec-audit.md",
            Self::Revise => "spec-revise.md",
        }
    }

    /// Parse a short name into a Stage.
    pub fn from_short_name(name: &str) -> Option<Self> {
        match name {
            "implement" => Some(Self::Implement),
            "test" => Some(Self::Test),
            "review" => Some(Self::Review),
            "audit" => Some(Self::Audit),
            "revise" => Some(Self::Revise),
            _ => None,
        }
    }

    /// Parse a DB enum value into a Stage.
    pub fn from_db_name(name: &str) -> Option<Self> {
        match name {
            "implementing" => Some(Self::Implement),
            "testing" => Some(Self::Test),
            "reviewing" => Some(Self::Review),
            "spec_audit" => Some(Self::Audit),
            "spec_revise" => Some(Self::Revise),
            _ => None,
        }
    }
}

impl fmt::Display for Stage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.short_name())
    }
}

/// Decision from evaluating a stage output.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "decision", rename_all = "snake_case")]
pub enum LoopDecision {
    Continue { feedback: serde_json::Value },
    Converged,
    Failed { reason: String },
}

/// Shared context for all stages in a loop.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoopContext {
    pub loop_id: Uuid,
    /// Engineer slug (lowercase, used for K8s secret names, branch names).
    pub engineer: String,
    /// Engineer display name for git attribution (GIT_AUTHOR_NAME).
    /// Falls back to engineer slug if not configured.
    pub engineer_name: String,
    /// Engineer email for git attribution (GIT_AUTHOR_EMAIL).
    pub engineer_email: String,
    pub spec_path: String,
    pub branch: String,
    pub current_sha: String,
    pub round: u32,
    pub max_rounds: u32,
    pub retry_count: u32,
    /// Stage-aware session ID: the resolved session ID for the current
    /// stage's tool. Set by build_context based on the stage and the
    /// typed per-tool session ID columns on the LoopRecord.
    pub session_id: Option<String>,
    pub feedback_path: Option<String>,
    /// Worktree sub-path relative to the bare-repo PVC root.
    /// e.g., "worktrees/agent-alice-invoice-cancel-a1b2c3d4" — mounted via subPath
    /// so the agent only sees its own worktree, not the shared bare repo.
    pub worktree_path: String,
    /// Credential references keyed by provider (e.g., "claude" -> credential JSON).
    /// Injected into job pods so agents can authenticate with model APIs.
    #[serde(default)]
    pub credentials: Vec<(String, String)>,
    /// Resolved default branch for the target repo (e.g., "main").
    /// Used by the entrypoint for git diff context in review/audit stages.
    pub base_branch: String,
}

/// Configuration for a single stage.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StageConfig {
    pub name: String,
    pub model: Option<String>,
    pub prompt_template: Option<String>,
    pub timeout: Duration,
    pub max_retries: u32,
}

impl Default for StageConfig {
    fn default() -> Self {
        Self {
            name: String::new(),
            model: None,
            prompt_template: None,
            timeout: Duration::from_secs(30 * 60),
            max_retries: 2,
        }
    }
}

/// Full loop record as stored in the database.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoopRecord {
    pub id: Uuid,
    pub engineer: String,
    pub spec_path: String,
    pub spec_content_hash: String,
    pub branch: String,
    pub kind: LoopKind,
    pub state: LoopState,
    pub sub_state: Option<SubState>,
    pub round: i32,
    pub max_rounds: i32,
    pub harden: bool,
    pub harden_only: bool,
    pub auto_approve: bool,
    pub cancel_requested: bool,
    pub approve_requested: bool,
    pub resume_requested: bool,
    pub paused_from_state: Option<LoopState>,
    pub reauth_from_state: Option<LoopState>,
    /// Stage the loop was running when it transitioned to Failed. Used by
    /// `nemo resume <loop-id>` on FAILED loops to redispatch the correct
    /// stage against the existing worktree (issue #96).
    pub failed_from_state: Option<LoopState>,
    pub failure_reason: Option<String>,
    pub current_sha: Option<String>,
    /// opencode session ID (`ses_<alphanum>`) from the last audit/review stage.
    /// Only forwarded to opencode-using stages; implement/revise stages ignore it.
    pub opencode_session_id: Option<String>,
    /// Claude Code session ID (UUID) from the last implement/revise stage.
    /// Only forwarded to claude-using stages; audit/review stages ignore it.
    pub claude_session_id: Option<String>,
    pub active_job_name: Option<String>,
    pub retry_count: i32,
    pub ship_mode: bool,
    pub model_implementor: Option<String>,
    pub model_reviewer: Option<String>,
    pub merge_sha: Option<String>,
    pub merged_at: Option<DateTime<Utc>>,
    pub hardened_spec_path: Option<String>,
    pub spec_pr_url: Option<String>,
    /// Resolved default branch name (e.g., "main"), frozen at loop creation.
    /// Used for PR --base and merge SHA resolution.
    pub resolved_default_branch: Option<String>,
    /// Optional per-loop uniform override for the `activeDeadlineSeconds`
    /// budget on every stage's K8s Job (CLI `--stage-timeout=<secs>`).
    /// When `None`, per-stage overrides (below) win, then the cluster
    /// default. Persisted so `nemo resume --stage-timeout=<larger>` can
    /// raise the budget without re-submitting the spec.
    pub stage_timeout_secs: Option<i32>,
    /// Per-stage `activeDeadlineSeconds` overrides plumbed from the
    /// repo-level `nemo.toml` `[timeouts]` block by the CLI at submit
    /// time. Each `Some(n)` pins that stage's deadline to `n` seconds
    /// (300s floor enforced server-side). `None` falls through to the
    /// uniform override, then the cluster default. Per-stage beats
    /// uniform because operators sometimes want a long audit budget
    /// without also extending every implement stage.
    pub implement_timeout_secs: Option<i32>,
    pub test_timeout_secs: Option<i32>,
    pub review_timeout_secs: Option<i32>,
    pub audit_timeout_secs: Option<i32>,
    pub revise_timeout_secs: Option<i32>,
    /// Per-loop `[cache.env]` overrides plumbed from the repo-level
    /// `nemo.toml` by the CLI at submit time. Merged with the
    /// cluster-default cache env at stage-dispatch; per-loop keys win
    /// on collisions. `None` means no override (use cluster default
    /// verbatim). Shape: `{"BUN_INSTALL_CACHE_DIR": "/cache/bun", ...}`.
    pub cache_env_overrides: Option<serde_json::Value>,
    /// Wall-clock heartbeat: the most recent moment the reconciler
    /// observed any signal of forward progress on this loop's pod
    /// (new log bytes, K8s status transition, fresh dispatch). `None`
    /// when the loop has never had an active pod. Surfaced in
    /// `nemo status` so operators can distinguish "still working"
    /// from "wedged on dead credentials" without kubectl-exec.
    pub last_activity_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// A round record tracking stage results within a loop.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoundRecord {
    pub id: Uuid,
    pub loop_id: Uuid,
    pub round: i32,
    pub stage: String,
    pub input: Option<serde_json::Value>,
    pub output: Option<serde_json::Value>,
    pub started_at: Option<DateTime<Utc>>,
    pub completed_at: Option<DateTime<Utc>>,
    pub duration_secs: Option<i64>,
    pub job_name: Option<String>,
}

/// A structured log event.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogEvent {
    pub id: Uuid,
    pub loop_id: Uuid,
    pub round: i32,
    pub stage: String,
    pub timestamp: DateTime<Utc>,
    pub line: String,
}

/// Engineer credentials record.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EngineerCredential {
    pub id: Uuid,
    pub engineer: String,
    pub provider: String,
    pub credential_ref: String,
    pub valid: bool,
    pub updated_at: DateTime<Utc>,
}

/// A merge event logged when ship mode auto-merges (NFR-8).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MergeEvent {
    pub id: Uuid,
    pub loop_id: Uuid,
    pub merge_sha: String,
    pub merge_strategy: String,
    pub ci_status: String,
    pub created_at: DateTime<Utc>,
}

/// A judge decision record for the orchestrator judge (Stage 1 self-learning).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JudgeDecisionRecord {
    pub id: Uuid,
    pub loop_id: Uuid,
    pub round: i32,
    pub phase: String,
    pub trigger: String,
    pub input_json: serde_json::Value,
    pub decision: String,
    pub confidence: Option<f32>,
    pub reasoning: Option<String>,
    pub hint: Option<String>,
    pub duration_ms: i32,
    pub created_at: DateTime<Utc>,
    pub loop_final_state: Option<String>,
    pub loop_terminated_at: Option<DateTime<Utc>>,
}

/// Generate a branch name per FR-5: agent/{engineer}/{spec-slug}-{short-hash}
///
/// Slugifies engineer and spec path to produce valid git refs.
/// Includes path hash to avoid collisions between same-stem specs in different dirs.
pub fn generate_branch_name(engineer: &str, spec_path: &str, spec_content: &str) -> String {
    use sha2::{Digest, Sha256};

    let safe_engineer = slugify(engineer);

    // Extract slug from spec path: "specs/feature/invoice-cancel.md" -> "invoice-cancel"
    let raw_slug = std::path::Path::new(spec_path)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("unknown");
    let safe_slug = slugify(raw_slug);

    // Hash includes raw engineer name, full spec path, and content to avoid
    // collisions between same-name specs in different dirs and between
    // engineers whose names normalize to the same slug (e.g., "Alice" vs "alice")
    let mut hasher = Sha256::new();
    hasher.update(engineer.as_bytes());
    hasher.update(spec_path.as_bytes());
    hasher.update(spec_content.as_bytes());
    let hash = hasher.finalize();
    let short_hash = &hex::encode(hash)[..8];

    format!("agent/{safe_engineer}/{safe_slug}-{short_hash}")
}

/// Sanitize a string for use in a git ref name.
/// Replaces invalid characters with hyphens, collapses runs, strips leading/trailing.
fn slugify(s: &str) -> String {
    let slug: String = s
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect();
    // Collapse consecutive hyphens, strip leading/trailing
    let mut result = String::new();
    let mut prev_hyphen = true; // treat start as hyphen to strip leading
    for c in slug.chars() {
        if c == '-' {
            if !prev_hyphen {
                result.push('-');
            }
            prev_hyphen = true;
        } else {
            result.push(c.to_ascii_lowercase());
            prev_hyphen = false;
        }
    }
    // Strip trailing hyphen
    while result.ends_with('-') {
        result.pop();
    }
    if result.is_empty() {
        "unknown".to_string()
    } else {
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_generate_branch_name() {
        let branch = generate_branch_name(
            "alice",
            "specs/feature/invoice-cancel.md",
            "# Invoice Cancel Spec\n",
        );
        assert!(branch.starts_with("agent/alice/invoice-cancel-"));
        assert_eq!(branch.len(), "agent/alice/invoice-cancel-".len() + 8);
    }

    #[test]
    fn test_generate_branch_name_stable() {
        // Same inputs produce same branch name (stable across rounds)
        let branch1 = generate_branch_name("alice", "specs/foo.md", "content");
        let branch2 = generate_branch_name("alice", "specs/foo.md", "content");
        assert_eq!(branch1, branch2);
    }

    #[test]
    fn test_generate_branch_name_different_engineers() {
        let branch1 = generate_branch_name("alice", "specs/foo.md", "content");
        let branch2 = generate_branch_name("bob", "specs/foo.md", "content");
        assert_ne!(branch1, branch2);
        assert!(branch1.starts_with("agent/alice/"));
        assert!(branch2.starts_with("agent/bob/"));
    }

    #[test]
    fn test_loop_state_terminal() {
        assert!(LoopState::Converged.is_terminal());
        assert!(LoopState::Failed.is_terminal());
        assert!(LoopState::Cancelled.is_terminal());
        assert!(LoopState::Hardened.is_terminal());
        assert!(LoopState::Shipped.is_terminal());
        assert!(!LoopState::Pending.is_terminal());
        assert!(!LoopState::Implementing.is_terminal());
    }

    #[test]
    fn test_loop_state_active_stage() {
        assert!(LoopState::Hardening.is_active_stage());
        assert!(LoopState::Implementing.is_active_stage());
        assert!(LoopState::Testing.is_active_stage());
        assert!(LoopState::Reviewing.is_active_stage());
        assert!(!LoopState::Pending.is_active_stage());
        assert!(!LoopState::Converged.is_active_stage());
    }

    #[test]
    fn test_stage_short_names() {
        assert_eq!(Stage::Implement.short_name(), "implement");
        assert_eq!(Stage::Test.short_name(), "test");
        assert_eq!(Stage::Review.short_name(), "review");
        assert_eq!(Stage::Audit.short_name(), "audit");
        assert_eq!(Stage::Revise.short_name(), "revise");
    }

    #[test]
    fn test_stage_db_names() {
        assert_eq!(Stage::Implement.db_name(), "implementing");
        assert_eq!(Stage::Test.db_name(), "testing");
        assert_eq!(Stage::Review.db_name(), "reviewing");
        assert_eq!(Stage::Audit.db_name(), "spec_audit");
        assert_eq!(Stage::Revise.db_name(), "spec_revise");
    }

    #[test]
    fn test_stage_prompt_filenames() {
        assert_eq!(Stage::Implement.prompt_filename(), "implement.md");
        assert_eq!(Stage::Test.prompt_filename(), "test.md");
        assert_eq!(Stage::Review.prompt_filename(), "review.md");
        assert_eq!(Stage::Audit.prompt_filename(), "spec-audit.md");
        assert_eq!(Stage::Revise.prompt_filename(), "spec-revise.md");
    }

    #[test]
    fn test_stage_from_short_name() {
        assert_eq!(Stage::from_short_name("implement"), Some(Stage::Implement));
        assert_eq!(Stage::from_short_name("test"), Some(Stage::Test));
        assert_eq!(Stage::from_short_name("review"), Some(Stage::Review));
        assert_eq!(Stage::from_short_name("audit"), Some(Stage::Audit));
        assert_eq!(Stage::from_short_name("revise"), Some(Stage::Revise));
        assert_eq!(Stage::from_short_name("unknown"), None);
    }

    #[test]
    fn test_stage_from_db_name() {
        assert_eq!(Stage::from_db_name("implementing"), Some(Stage::Implement));
        assert_eq!(Stage::from_db_name("testing"), Some(Stage::Test));
        assert_eq!(Stage::from_db_name("reviewing"), Some(Stage::Review));
        assert_eq!(Stage::from_db_name("spec_audit"), Some(Stage::Audit));
        assert_eq!(Stage::from_db_name("spec_revise"), Some(Stage::Revise));
        assert_eq!(Stage::from_db_name("unknown"), None);
    }

    #[test]
    fn test_stage_display() {
        assert_eq!(format!("{}", Stage::Implement), "implement");
        assert_eq!(format!("{}", Stage::Audit), "audit");
    }

    #[test]
    fn test_stage_roundtrip_through_db_name() {
        for stage in [
            Stage::Implement,
            Stage::Test,
            Stage::Review,
            Stage::Audit,
            Stage::Revise,
        ] {
            let db = stage.db_name();
            let parsed = Stage::from_db_name(db).unwrap();
            assert_eq!(parsed, stage);
        }
    }
}
