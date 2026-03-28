use serde::{Deserialize, Serialize};

/// An issue found during review or audit.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Issue {
    pub severity: Severity,
    pub category: String,
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

/// Implementation output from the implement stage.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImplOutput {
    pub sha: String,
    pub affected_services: Vec<String>,
}

/// Output from the revise stage (harden loop).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviseOutput {
    pub updated_spec_path: String,
    pub sha: String,
}

/// A single test failure.
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

/// Output from the test stage.
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
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FeedbackSource {
    Review,
    Test,
    Audit,
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
                category: "completeness".to_string(),
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
                category: "correctness".to_string(),
                file: Some("api/src/invoice.rs".to_string()),
                line: Some(42),
                description: "Missing null check".to_string(),
                suggestion: "Add early return".to_string(),
            }]),
            failures: None,
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
}
