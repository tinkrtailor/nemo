//! Case execution result types.
//!
//! Each runner module builds a [`CaseOutcome`] per sidecar and the
//! diff engine (see [`crate::diff`]) compares them. The outcomes are
//! intentionally stringly-typed JSON-ish maps so every category can
//! share one set of fields — per-category fields that are not relevant
//! simply stay absent.

use std::collections::BTreeMap;
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// Observed output from a single sidecar for a single case. Fields
/// populated depend on the category. Only fields populated by BOTH
/// sides participate in the diff; absent fields on one side are
/// considered a diff against a present field on the other side.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SideOutput {
    /// HTTP status code for HTTP cases. `0` means "not applicable".
    #[serde(default)]
    pub http_status: u16,
    /// Response body for HTTP cases (stripped of dynamic fields by
    /// normalization).
    #[serde(default)]
    pub http_body: String,
    /// Response headers (lowercased name → value). Normalization
    /// strips dynamic entries per FR-19.
    #[serde(default)]
    pub http_headers: BTreeMap<String, String>,

    /// SSH exit status (if any).
    #[serde(default)]
    pub ssh_exit_status: Option<i32>,
    /// SSH stdout bytes (hex-encoded for display).
    #[serde(default)]
    pub ssh_stdout_hex: String,
    /// SSH stderr (trimmed).
    #[serde(default)]
    pub ssh_stderr: String,
    /// `true` if the SSH channel was failed before any exit status
    /// was sent (e.g. env request rejection).
    #[serde(default)]
    pub ssh_channel_failed: bool,

    /// Introspection records the mock services observed from THIS
    /// side (filtered by source IP). Already normalized.
    #[serde(default)]
    pub mock_observations: Vec<ObservedMockRequest>,

    /// Wall-clock timestamps (since the harness spawn) of each body
    /// chunk observed for streaming HTTP responses. Populated only
    /// for the SSE streaming divergence cases.
    #[serde(default)]
    pub chunk_timestamps_ms: Vec<u128>,

    /// Wall-clock milliseconds from request send to first chunk
    /// received — the primary assertion for the SSE divergence
    /// cases.
    #[serde(default)]
    pub time_to_first_chunk_ms: Option<u128>,

    /// Time (ms) from SIGTERM to last observed byte — populated only
    /// by the `divergence_connect_drain_on_sigterm` case.
    #[serde(default)]
    pub drain_stop_ms: Option<u128>,
}

impl SideOutput {
    /// Build a minimally-populated side output with just the HTTP
    /// fields filled in. Used by the HTTP runners.
    pub fn http(status: u16, headers: BTreeMap<String, String>, body: impl Into<String>) -> Self {
        Self {
            http_status: status,
            http_headers: headers,
            http_body: body.into(),
            ..Self::default()
        }
    }
}

/// A single request observed by a mock service, as returned by the
/// `/__harness/logs` endpoint. Normalized (id / timestamp stripped,
/// headers lowercased, body base64).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObservedMockRequest {
    pub mock: String,
    pub method: String,
    pub path: String,
    pub host_header: String,
    /// Headers lowercased, sensitive-only captured.
    pub headers: BTreeMap<String, String>,
    /// Base64-encoded body.
    pub body_b64: String,
    /// Source IP inside parity-net.
    pub source_ip: String,
}

/// Overall outcome of a case after both sides have been executed and
/// the diff engine has compared them.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CaseOutcome {
    /// Name of the case, for printing.
    pub name: String,
    /// Relative (from repo root) path of the source JSON file, so
    /// NFR-5 diffs can name the file.
    pub source_path: String,
    /// Passed or failed.
    pub passed: bool,
    /// Expected parity flag from the corpus.
    pub expected_parity: bool,
    /// Raw outcomes used by the diff engine.
    pub go_side: SideOutput,
    pub rust_side: SideOutput,
    /// Human-readable diff summary. Empty when `passed == true`.
    pub diff: String,
    /// Elapsed wall clock for the case.
    pub duration_ms: u128,
    /// Extra notes (e.g. "RUST first chunk at 34ms, GO at 295ms" for
    /// the SSE divergence cases).
    #[serde(default)]
    pub notes: String,
}

impl CaseOutcome {
    pub fn pass(
        name: impl Into<String>,
        source_path: impl Into<String>,
        expected_parity: bool,
        go_side: SideOutput,
        rust_side: SideOutput,
        duration: Duration,
        notes: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            source_path: source_path.into(),
            passed: true,
            expected_parity,
            go_side,
            rust_side,
            diff: String::new(),
            duration_ms: duration.as_millis(),
            notes: notes.into(),
        }
    }

    pub fn fail(
        name: impl Into<String>,
        source_path: impl Into<String>,
        expected_parity: bool,
        go_side: SideOutput,
        rust_side: SideOutput,
        duration: Duration,
        diff: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            source_path: source_path.into(),
            passed: false,
            expected_parity,
            go_side,
            rust_side,
            diff: diff.into(),
            duration_ms: duration.as_millis(),
            notes: String::new(),
        }
    }
}

/// Summary of a full harness run.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RunSummary {
    pub total: usize,
    pub passed: usize,
    pub failed: usize,
    pub by_category: BTreeMap<String, CategorySummary>,
    pub failures: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CategorySummary {
    pub total: usize,
    pub passed: usize,
    pub failed: usize,
}

impl RunSummary {
    pub fn all_passed(&self) -> bool {
        self.failed == 0 && self.total > 0
    }

    pub fn from_outcomes(outcomes: &[CaseOutcome]) -> Self {
        let mut summary = RunSummary {
            total: outcomes.len(),
            ..Default::default()
        };
        for o in outcomes {
            if o.passed {
                summary.passed += 1;
            } else {
                summary.failed += 1;
                summary.failures.push(o.name.clone());
            }
            let key = category_of(&o.source_path);
            let entry = summary.by_category.entry(key).or_default();
            entry.total += 1;
            if o.passed {
                entry.passed += 1;
            } else {
                entry.failed += 1;
            }
        }
        summary
    }
}

/// Heuristic derivation of the category from a case's source path —
/// the runner already knows the category via the corpus, but the
/// summary printer uses this as a fallback when reading an older
/// serialized outcome file.
fn category_of(path: &str) -> String {
    let fname = std::path::Path::new(path)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("unknown");
    if fname.starts_with("divergence_") {
        "divergence".to_string()
    } else if fname.starts_with("healthz_") {
        "health".to_string()
    } else if fname.starts_with("egress_") {
        "egress".to_string()
    } else if fname.starts_with("ssh_") {
        "git_ssh".to_string()
    } else {
        "model_proxy".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summary_counts_pass_fail() {
        let outcomes = vec![
            CaseOutcome::pass(
                "a",
                "corpus/a.json",
                true,
                SideOutput::default(),
                SideOutput::default(),
                Duration::from_millis(10),
                "",
            ),
            CaseOutcome::fail(
                "b",
                "corpus/divergence_b.json",
                false,
                SideOutput::default(),
                SideOutput::default(),
                Duration::from_millis(5),
                "bad",
            ),
        ];
        let summary = RunSummary::from_outcomes(&outcomes);
        assert_eq!(summary.total, 2);
        assert_eq!(summary.passed, 1);
        assert_eq!(summary.failed, 1);
        assert_eq!(summary.failures, vec!["b"]);
        assert!(!summary.all_passed());
    }

    #[test]
    fn summary_all_passed_true_when_zero_failures() {
        let outcomes = vec![CaseOutcome::pass(
            "a",
            "corpus/a.json",
            true,
            SideOutput::default(),
            SideOutput::default(),
            Duration::from_millis(1),
            "",
        )];
        assert!(RunSummary::from_outcomes(&outcomes).all_passed());
    }

    #[test]
    fn category_of_recognizes_prefixes() {
        assert_eq!(category_of("corpus/divergence_a.json"), "divergence");
        assert_eq!(category_of("corpus/egress_a.json"), "egress");
        assert_eq!(category_of("corpus/ssh_a.json"), "git_ssh");
        assert_eq!(category_of("corpus/healthz_a.json"), "health");
        assert_eq!(category_of("corpus/openai_a.json"), "model_proxy");
    }
}
