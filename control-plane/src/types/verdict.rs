use serde::{Deserialize, Serialize};

/// An issue found during review or audit.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Issue {
    pub severity: Severity,
    /// Category is optional per spec FR-40: not all reviewers produce categories.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,
    pub description: String,
    pub suggestion: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Critical,
    High,
    Medium,
    Low,
}

/// Token usage reported by agents.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenUsage {
    pub input: u64,
    pub output: u64,
}

/// Review verdict written by the review agent to `.agent/review-verdict.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewVerdict {
    pub clean: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub confidence: Option<f64>,
    pub issues: Vec<Issue>,
    pub summary: String,
    pub token_usage: TokenUsage,
}

/// Audit verdict written by the audit agent to `.agent/audit-verdict.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditVerdict {
    pub clean: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub confidence: Option<f64>,
    pub issues: Vec<Issue>,
    pub summary: String,
    pub token_usage: TokenUsage,
}

/// NAUTILOOP_RESULT envelope: the typed output contract between agent and control plane.
/// Written as a single JSON line prefixed with `NAUTILOOP_RESULT:` to stdout (FR-13).
/// The `stage` field uses short names: implement, test, review, audit, revise.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NautiloopResult {
    pub stage: String,
    pub data: serde_json::Value,
}

impl NautiloopResult {
    /// Parse the data field into a typed implement output.
    pub fn as_impl_output(&self) -> std::result::Result<ImplResultData, serde_json::Error> {
        serde_json::from_value(self.data.clone())
    }

    /// Parse the data field into a typed test output.
    pub fn as_test_output(&self) -> std::result::Result<TestResultData, serde_json::Error> {
        serde_json::from_value(self.data.clone())
    }

    /// Parse the data field into a typed review/audit output.
    pub fn as_review_output(&self) -> std::result::Result<ReviewResultData, serde_json::Error> {
        serde_json::from_value(self.data.clone())
    }

    /// Parse the data field into a typed revise output.
    pub fn as_revise_output(&self) -> std::result::Result<ReviseResultData, serde_json::Error> {
        serde_json::from_value(self.data.clone())
    }
}

/// IMPLEMENT stage result data (FR-13).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImplResultData {
    pub new_sha: String,
    pub token_usage: TokenUsage,
    pub exit_code: i32,
    pub session_id: String,
}

/// TEST stage result data (FR-42d).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TestResultData {
    pub services: Vec<TestServiceResult>,
    pub all_passed: bool,
    pub ci_status: CiStatus,
    pub token_usage: TokenUsage,
}

/// Per-service test result within the TEST stage output.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TestServiceResult {
    pub name: String,
    pub test_command: String,
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
}

/// Three-state CI status model (FR-42d).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CiStatus {
    Passed,
    Failed,
    Unknown,
}

/// REVIEW/AUDIT stage result data (FR-13).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewResultData {
    pub verdict: serde_json::Value,
    pub token_usage: TokenUsage,
    pub exit_code: i32,
    pub session_id: String,
}

/// REVISE stage result data (FR-13).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviseResultData {
    pub revised_spec_path: String,
    pub new_sha: String,
    pub token_usage: TokenUsage,
    pub exit_code: i32,
    pub session_id: String,
}

/// Legacy implementation output (kept for backward compatibility with Lane A).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImplOutput {
    pub sha: String,
}

/// Legacy output from the revise stage (kept for backward compatibility with Lane A).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviseOutput {
    pub updated_spec_path: String,
    pub sha: String,
}

/// A single test failure (used in feedback files).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TestFailure {
    pub service: String,
    pub test_command: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub test_name: Option<String>,
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
}

/// Legacy test output (kept for backward compatibility with Lane A).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TestOutput {
    pub passed: bool,
    pub failures: Vec<TestFailure>,
}

/// Feedback file written by the loop engine for the next implement round.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FeedbackFile {
    pub round: u32,
    pub source: FeedbackSource,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub issues: Option<Vec<Issue>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failures: Option<Vec<TestFailure>>,
    /// Orchestrator judge hint injected when the judge decides to continue
    /// with guidance. Agents are instructed to weight these heavily.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub orchestrator_hint: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FeedbackSource {
    Review,
    Test,
    Audit,
}

// --- Orchestrator Judge types (FR-2, FR-3) ---

/// Input context for the orchestrator judge.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JudgeInput {
    pub loop_id: uuid::Uuid,
    pub spec_path: String,
    pub spec_content: String,
    pub phase: String,
    pub round: i32,
    pub max_rounds: i32,
    pub rounds: Vec<JudgeRoundSummary>,
    pub current_verdict: serde_json::Value,
    pub recurring_findings: Vec<RecurringFinding>,
}

/// Summary of a round for the judge context.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JudgeRoundSummary {
    pub round: i32,
    pub stage: String,
    pub verdict: Option<serde_json::Value>,
    pub duration_secs: Option<i64>,
}

/// A finding that recurs across multiple rounds.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecurringFinding {
    pub category: Option<String>,
    pub file: Option<String>,
    pub line: Option<u32>,
    pub seen_in_rounds: Vec<i32>,
}

/// Structured decision returned by the orchestrator judge.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JudgeOutput {
    pub decision: JudgeDecision,
    pub confidence: f64,
    pub reasoning: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
}

/// The four possible judge decisions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JudgeDecision {
    Continue,
    ExitClean,
    ExitEscalate,
    ExitFail,
}

impl JudgeDecision {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Continue => "continue",
            Self::ExitClean => "exit_clean",
            Self::ExitEscalate => "exit_escalate",
            Self::ExitFail => "exit_fail",
        }
    }

}

impl std::fmt::Display for JudgeDecision {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

/// Trigger reason for a judge invocation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JudgeTrigger {
    NotClean,
    MaxRounds,
    RecurringFindings,
}

impl JudgeTrigger {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::NotClean => "not_clean",
            Self::MaxRounds => "max_rounds",
            Self::RecurringFindings => "recurring_findings",
        }
    }
}

impl std::fmt::Display for JudgeTrigger {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_review_verdict_clean_roundtrip() {
        let verdict = ReviewVerdict {
            clean: true,
            confidence: Some(0.95),
            issues: vec![],
            summary: "All good.".to_string(),
            token_usage: TokenUsage {
                input: 45000,
                output: 3200,
            },
        };
        let json = serde_json::to_string(&verdict).unwrap();
        let parsed: ReviewVerdict = serde_json::from_str(&json).unwrap();
        assert!(parsed.clean);
        assert!(parsed.issues.is_empty());
    }

    #[test]
    fn test_review_verdict_with_issues_roundtrip() {
        let json = r#"{
            "clean": false,
            "confidence": 0.85,
            "issues": [
                {
                    "severity": "high",
                    "category": "correctness",
                    "file": "api/src/invoice.rs",
                    "line": 42,
                    "description": "Missing null check",
                    "suggestion": "Add early return"
                }
            ],
            "summary": "One issue found.",
            "token_usage": { "input": 45000, "output": 3200 }
        }"#;
        let verdict: ReviewVerdict = serde_json::from_str(json).unwrap();
        assert!(!verdict.clean);
        assert_eq!(verdict.issues.len(), 1);
        assert_eq!(verdict.issues[0].severity, Severity::High);
    }

    #[test]
    fn test_audit_verdict_roundtrip() {
        let verdict = AuditVerdict {
            clean: false,
            confidence: Some(0.9),
            issues: vec![Issue {
                severity: Severity::High,
                category: Some("completeness".to_string()),
                file: Some("specs/feature/invoice-cancel.md".to_string()),
                line: None,
                description: "Missing error handling section".to_string(),
                suggestion: "Add error handling".to_string(),
            }],
            summary: "Spec needs work.".to_string(),
            token_usage: TokenUsage {
                input: 32000,
                output: 2100,
            },
        };
        let json = serde_json::to_string(&verdict).unwrap();
        let parsed: AuditVerdict = serde_json::from_str(&json).unwrap();
        assert!(!parsed.clean);
        assert_eq!(parsed.issues.len(), 1);
    }

    #[test]
    fn test_feedback_file_review_source() {
        let feedback = FeedbackFile {
            round: 2,
            source: FeedbackSource::Review,
            issues: Some(vec![Issue {
                severity: Severity::High,
                category: Some("correctness".to_string()),
                file: Some("api/src/invoice.rs".to_string()),
                line: Some(42),
                description: "Missing null check".to_string(),
                suggestion: "Add early return".to_string(),
            }]),
            failures: None,
            orchestrator_hint: None,
        };
        let json = serde_json::to_string(&feedback).unwrap();
        assert!(json.contains("\"source\":\"review\""));
    }

    #[test]
    fn test_feedback_file_test_source() {
        let feedback = FeedbackFile {
            round: 2,
            source: FeedbackSource::Test,
            issues: None,
            failures: Some(vec![TestFailure {
                service: "api".to_string(),
                test_command: "cargo test -p api".to_string(),
                test_name: Some("test_cancel".to_string()),
                exit_code: 101,
                stdout: "panicked".to_string(),
                stderr: "error".to_string(),
            }]),
            orchestrator_hint: None,
        };
        let json = serde_json::to_string(&feedback).unwrap();
        assert!(json.contains("\"source\":\"test\""));
    }

    #[test]
    fn test_malformed_verdict_fails_parse() {
        let bad_json = r#"{ "not_a_verdict": true }"#;
        let result = serde_json::from_str::<ReviewVerdict>(bad_json);
        assert!(result.is_err());
    }

    #[test]
    fn test_nautiloop_result_implement_roundtrip() {
        let data = ImplResultData {
            new_sha: "abc123def456".to_string(),
            token_usage: TokenUsage {
                input: 10000,
                output: 2000,
            },
            exit_code: 0,
            session_id: "550e8400-e29b-41d4-a716-446655440000".to_string(),
        };
        let result = NautiloopResult {
            stage: "implement".to_string(),
            data: serde_json::to_value(&data).unwrap(),
        };
        let json = serde_json::to_string(&result).unwrap();
        let parsed: NautiloopResult = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.stage, "implement");
        let impl_data = parsed.as_impl_output().unwrap();
        assert_eq!(impl_data.new_sha, "abc123def456");
        assert_eq!(impl_data.exit_code, 0);
    }

    #[test]
    fn test_nautiloop_result_test_roundtrip() {
        let data = TestResultData {
            services: vec![TestServiceResult {
                name: "api".to_string(),
                test_command: "cargo test -p api".to_string(),
                exit_code: 0,
                stdout: "ok".to_string(),
                stderr: String::new(),
            }],
            all_passed: true,
            ci_status: CiStatus::Passed,
            token_usage: TokenUsage {
                input: 0,
                output: 0,
            },
        };
        let result = NautiloopResult {
            stage: "test".to_string(),
            data: serde_json::to_value(&data).unwrap(),
        };
        let json = serde_json::to_string(&result).unwrap();
        let parsed: NautiloopResult = serde_json::from_str(&json).unwrap();
        let test_data = parsed.as_test_output().unwrap();
        assert!(test_data.all_passed);
        assert_eq!(test_data.ci_status, CiStatus::Passed);
        assert_eq!(test_data.services.len(), 1);
    }

    #[test]
    fn test_nautiloop_result_review_roundtrip() {
        let data = ReviewResultData {
            verdict: serde_json::json!({"clean": true, "summary": "looks good"}),
            token_usage: TokenUsage {
                input: 5000,
                output: 1000,
            },
            exit_code: 0,
            session_id: "ses_abc123XYZ".to_string(),
        };
        let result = NautiloopResult {
            stage: "review".to_string(),
            data: serde_json::to_value(&data).unwrap(),
        };
        let json = serde_json::to_string(&result).unwrap();
        let parsed: NautiloopResult = serde_json::from_str(&json).unwrap();
        let review_data = parsed.as_review_output().unwrap();
        assert_eq!(review_data.exit_code, 0);
    }

    #[test]
    fn test_nautiloop_result_revise_roundtrip() {
        let data = ReviseResultData {
            revised_spec_path: "specs/feature/invoice-cancel.md".to_string(),
            new_sha: "def789".to_string(),
            token_usage: TokenUsage {
                input: 8000,
                output: 1500,
            },
            exit_code: 0,
            session_id: "550e8400-e29b-41d4-a716-446655440001".to_string(),
        };
        let result = NautiloopResult {
            stage: "revise".to_string(),
            data: serde_json::to_value(&data).unwrap(),
        };
        let json = serde_json::to_string(&result).unwrap();
        let parsed: NautiloopResult = serde_json::from_str(&json).unwrap();
        let revise_data = parsed.as_revise_output().unwrap();
        assert_eq!(
            revise_data.revised_spec_path,
            "specs/feature/invoice-cancel.md"
        );
        assert_eq!(revise_data.new_sha, "def789");
    }

    #[test]
    fn test_ci_status_serialization() {
        assert_eq!(
            serde_json::to_string(&CiStatus::Passed).unwrap(),
            "\"passed\""
        );
        assert_eq!(
            serde_json::to_string(&CiStatus::Failed).unwrap(),
            "\"failed\""
        );
        assert_eq!(
            serde_json::to_string(&CiStatus::Unknown).unwrap(),
            "\"unknown\""
        );
    }
}
