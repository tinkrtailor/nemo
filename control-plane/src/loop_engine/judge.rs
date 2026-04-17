use std::sync::Arc;
use std::time::Instant;

use uuid::Uuid;

use crate::config::OrchestratorConfig;
use crate::error::Result;
use crate::state::StateStore;
use crate::types::verdict::{
    Issue, JudgeDecision, JudgeInput, JudgeOutput, JudgeRoundSummary, JudgeTrigger,
    RecurringFinding,
};
use crate::types::{JudgeDecisionRecord, RoundRecord};

/// Model client trait for the orchestrator judge. Abstracted for testability.
#[async_trait::async_trait]
pub trait JudgeModelClient: Send + Sync + 'static {
    /// Send a prompt to the model and return the raw response text.
    async fn call(&self, model: &str, system: &str, user: &str) -> Result<String>;
}

/// HTTP-based model client that calls the sidecar proxy.
pub struct SidecarJudgeClient {
    client: reqwest::Client,
    base_url: String,
}

impl Default for SidecarJudgeClient {
    fn default() -> Self {
        Self::new()
    }
}

impl SidecarJudgeClient {
    pub fn new() -> Self {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .expect("Failed to build HTTP client");

        // The sidecar model proxy runs on localhost:9090. For Anthropic models,
        // requests go to /anthropic/v1/messages.
        let base_url = std::env::var("JUDGE_MODEL_PROXY_URL")
            .unwrap_or_else(|_| "http://127.0.0.1:9090".to_string());

        Self { client, base_url }
    }
}

#[async_trait::async_trait]
impl JudgeModelClient for SidecarJudgeClient {
    async fn call(&self, model: &str, system: &str, user: &str) -> Result<String> {
        let url = format!("{}/anthropic/v1/messages", self.base_url);

        let body = serde_json::json!({
            "model": model,
            "max_tokens": 512,
            "system": system,
            "messages": [
                {"role": "user", "content": user}
            ]
        });

        let resp = self
            .client
            .post(&url)
            .header("content-type", "application/json")
            .header("anthropic-version", "2023-06-01")
            .json(&body)
            .send()
            .await
            .map_err(|e| {
                crate::error::NautiloopError::Internal(format!("Judge model call failed: {e}"))
            })?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(crate::error::NautiloopError::Internal(format!(
                "Judge model returned {status}: {text}"
            )));
        }

        let resp_json: serde_json::Value = resp.json().await.map_err(|e| {
            crate::error::NautiloopError::Internal(format!(
                "Judge model response parse failed: {e}"
            ))
        })?;

        // Extract text from Anthropic Messages API response
        // Response shape: { "content": [{ "type": "text", "text": "..." }] }
        let text = resp_json
            .get("content")
            .and_then(|c| c.as_array())
            .and_then(|arr| arr.first())
            .and_then(|block| block.get("text"))
            .and_then(|t| t.as_str())
            .unwrap_or("")
            .to_string();

        Ok(text)
    }
}

/// The orchestrator judge. Invoked at transition points to make
/// continue/exit/escalate decisions using an LLM.
pub struct OrchestratorJudge {
    config: OrchestratorConfig,
    store: Arc<dyn StateStore>,
    model_client: Arc<dyn JudgeModelClient>,
    prompt_template: String,
}

impl OrchestratorJudge {
    pub async fn new(
        config: OrchestratorConfig,
        store: Arc<dyn StateStore>,
        model_client: Arc<dyn JudgeModelClient>,
    ) -> Self {
        let prompt_template = load_judge_prompt().await;
        Self {
            config,
            store,
            model_client,
            prompt_template,
        }
    }

    /// Evaluate whether the judge should be invoked and, if so, invoke it.
    /// Returns `None` if the judge is disabled, not triggered, or fails (fallback).
    /// Returns `Some(JudgeOutput)` with the judge's decision.
    #[allow(clippy::too_many_arguments)]
    pub async fn evaluate(
        &self,
        loop_id: Uuid,
        spec_path: &str,
        spec_content: &str,
        phase: &str,
        round: i32,
        max_rounds: i32,
        current_verdict: &serde_json::Value,
        current_issues: &[Issue],
        rounds: &[RoundRecord],
    ) -> Option<JudgeOutput> {
        if !self.config.judge_enabled {
            return None;
        }

        // Determine trigger(s)
        let recurring = compute_recurring_findings(current_issues, rounds);
        let trigger = self.determine_trigger(round, max_rounds, &recurring);
        let trigger = match trigger {
            Some(t) => t,
            None => return None,
        };

        // Combined query: cost ceiling + one-shot exit_clean guard (FR-7a)
        let (call_count, prior_exit_clean) =
            match self.store.judge_decision_stats(loop_id).await {
                Ok(stats) => stats,
                Err(e) => {
                    tracing::warn!(
                        loop_id = %loop_id,
                        error = %e,
                        "Failed to query judge decision stats, falling back to heuristic"
                    );
                    return None;
                }
            };

        if call_count >= self.config.max_judge_calls_per_loop as i64 {
            tracing::warn!(
                loop_id = %loop_id,
                call_count,
                max = self.config.max_judge_calls_per_loop,
                "Judge call ceiling reached, falling back to heuristic"
            );
            return None;
        }

        // Build judge input
        let round_summaries: Vec<JudgeRoundSummary> = rounds
            .iter()
            .map(|r| JudgeRoundSummary {
                round: r.round,
                stage: r.stage.clone(),
                verdict: r.output.clone(),
                duration_secs: r.duration_secs,
            })
            .collect();

        let input = JudgeInput {
            loop_id,
            spec_path: spec_path.to_string(),
            spec_content: spec_content.to_string(),
            phase: phase.to_string(),
            round,
            max_rounds,
            rounds: round_summaries,
            current_verdict: current_verdict.clone(),
            recurring_findings: recurring,
        };

        let input_json = match serde_json::to_value(&input) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(
                    loop_id = %loop_id,
                    error = %e,
                    "Failed to serialize judge input, falling back to heuristic"
                );
                return None;
            }
        };

        // Invoke the judge with timeout
        let start = Instant::now();
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(30),
            self.invoke_judge(&input),
        )
        .await;

        let duration_ms = start.elapsed().as_millis() as i32;

        let mut output = match result {
            Ok(Ok(output)) => output,
            Ok(Err(e)) => {
                tracing::warn!(
                    loop_id = %loop_id,
                    round,
                    phase,
                    error = %e,
                    duration_ms,
                    "Judge invocation failed, falling back to heuristic"
                );
                return None;
            }
            Err(_) => {
                tracing::warn!(
                    loop_id = %loop_id,
                    round,
                    phase,
                    duration_ms,
                    "Judge invocation timed out (30s), falling back to heuristic"
                );
                return None;
            }
        };

        // FR-7a: If this would be a second exit_clean, downgrade to continue
        if output.decision == JudgeDecision::ExitClean && prior_exit_clean {
            tracing::warn!(
                loop_id = %loop_id,
                round,
                "Judge returned exit_clean but one was already issued; downgrading to continue"
            );
            output.decision = JudgeDecision::Continue;
            output.reasoning = format!(
                "[downgraded from exit_clean: already issued once] {}",
                output.reasoning
            );
        }

        // Log the decision
        tracing::info!(
            loop_id = %loop_id,
            round,
            phase,
            decision = output.decision.as_str(),
            confidence = output.confidence,
            duration_ms,
            "Judge decision"
        );

        // Persist the decision
        let record = JudgeDecisionRecord {
            id: Uuid::new_v4(),
            loop_id,
            round,
            phase: phase.to_string(),
            trigger: trigger.to_string(),
            input_json,
            decision: output.decision.as_str().to_string(),
            confidence: Some(output.confidence as f32),
            reasoning: Some(output.reasoning.clone()),
            hint: output.hint.clone(),
            duration_ms,
            created_at: chrono::Utc::now(),
            loop_final_state: None,
            loop_terminated_at: None,
        };

        if let Err(e) = self.store.create_judge_decision(&record).await {
            tracing::warn!(
                loop_id = %loop_id,
                error = %e,
                "Failed to persist judge decision (non-fatal)"
            );
        }

        Some(output)
    }

    /// Determine which trigger applies, if any.
    fn determine_trigger(
        &self,
        round: i32,
        max_rounds: i32,
        recurring: &[RecurringFinding],
    ) -> Option<JudgeTrigger> {
        // Priority: max_rounds > recurring_findings > not_clean
        if round >= max_rounds {
            Some(JudgeTrigger::MaxRounds)
        } else if recurring.iter().any(|f| !f.seen_in_rounds.is_empty()) {
            Some(JudgeTrigger::RecurringFindings)
        } else {
            // The caller already checked verdict.clean == false
            Some(JudgeTrigger::NotClean)
        }
    }

    /// Call the model with the assembled prompt and parse the JSON response.
    async fn invoke_judge(&self, input: &JudgeInput) -> Result<JudgeOutput> {
        let user_content = serde_json::to_string_pretty(input).map_err(|e| {
            crate::error::NautiloopError::Internal(format!(
                "Failed to serialize judge input: {e}"
            ))
        })?;

        let response_text = self
            .model_client
            .call(&self.config.judge_model, &self.prompt_template, &user_content)
            .await?;

        parse_judge_response(&response_text)
    }
}

/// Parse the judge response text into a JudgeOutput.
/// Handles both raw JSON and JSON embedded in markdown code blocks.
pub fn parse_judge_response(text: &str) -> Result<JudgeOutput> {
    let trimmed = text.trim();

    // Try direct JSON parse first
    if let Ok(output) = serde_json::from_str::<JudgeOutput>(trimmed) {
        return Ok(output);
    }

    // Try extracting from markdown code block
    if let Some(start) = trimmed.find("```json") {
        let json_start = start + 7;
        if let Some(end) = trimmed[json_start..].find("```") {
            let json_str = trimmed[json_start..json_start + end].trim();
            if let Ok(output) = serde_json::from_str::<JudgeOutput>(json_str) {
                return Ok(output);
            }
        }
    }

    // Try extracting from generic code block
    if let Some(start) = trimmed.find("```") {
        let json_start = trimmed[start + 3..].find('\n').map(|n| start + 3 + n + 1);
        if let Some(json_start) = json_start
            && let Some(end) = trimmed[json_start..].find("```")
        {
            let json_str = trimmed[json_start..json_start + end].trim();
            if let Ok(output) = serde_json::from_str::<JudgeOutput>(json_str) {
                return Ok(output);
            }
        }
    }

    // Try finding JSON object boundaries
    if let (Some(start), Some(end)) = (trimmed.find('{'), trimmed.rfind('}')) {
        let json_str = &trimmed[start..=end];
        if let Ok(output) = serde_json::from_str::<JudgeOutput>(json_str) {
            return Ok(output);
        }
    }

    Err(crate::error::NautiloopError::Internal(format!(
        "Failed to parse judge response as JSON: {trimmed}"
    )))
}

/// Detect recurring findings across rounds.
/// A finding recurs if `(category, file, line±2)` matches across rounds.
pub fn compute_recurring_findings(
    current_issues: &[Issue],
    rounds: &[RoundRecord],
) -> Vec<RecurringFinding> {
    // Collect all prior issues from round outputs (owned data)
    let mut prior_issues: Vec<(i32, String, Option<String>, Option<u32>)> = Vec::new();

    for round_record in rounds {
        if let Some(ref output) = round_record.output {
            let issues = extract_issues_from_output(output);
            for issue in issues {
                prior_issues.push((
                    round_record.round,
                    issue.category.clone().unwrap_or_default(),
                    issue.file.clone(),
                    issue.line,
                ));
            }
        }
    }

    let mut recurring: Vec<RecurringFinding> = Vec::new();

    for issue in current_issues {
        let category = issue.category.as_deref().unwrap_or("");
        let file = issue.file.as_deref();
        let line = issue.line;

        let mut seen_rounds: Vec<i32> = Vec::new();
        for (round_num, prior_cat, prior_file, prior_line) in &prior_issues {
            if category == prior_cat.as_str()
                && file == prior_file.as_deref()
                && lines_within_tolerance(line, *prior_line, 2)
                && !seen_rounds.contains(round_num)
            {
                seen_rounds.push(*round_num);
            }
        }

        if !seen_rounds.is_empty() {
            // Check if we already have this finding tracked
            let existing = recurring.iter_mut().find(|f| {
                f.category.as_deref() == issue.category.as_deref()
                    && f.file.as_deref() == issue.file.as_deref()
                    && lines_within_tolerance(f.line, issue.line, 2)
            });

            if let Some(existing) = existing {
                for r in &seen_rounds {
                    if !existing.seen_in_rounds.contains(r) {
                        existing.seen_in_rounds.push(*r);
                    }
                }
            } else {
                recurring.push(RecurringFinding {
                    category: issue.category.clone(),
                    file: issue.file.clone(),
                    line: issue.line,
                    seen_in_rounds: seen_rounds,
                });
            }
        }
    }

    recurring
}

/// Check if two line numbers are within a tolerance.
fn lines_within_tolerance(a: Option<u32>, b: Option<u32>, tolerance: u32) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => a.abs_diff(b) <= tolerance,
        (None, None) => true,
        _ => false,
    }
}

/// Extract issues from a round output value.
/// Handles both ReviewResultData envelope and direct verdict shapes.
pub fn extract_issues_from_output(output: &serde_json::Value) -> Vec<Issue> {
    // Try ReviewResultData envelope: { verdict: { issues: [...] } }
    if let Some(verdict) = output.get("verdict")
        && let Some(issues) = verdict.get("issues").and_then(|i| i.as_array())
    {
        return issues
            .iter()
            .filter_map(|v| serde_json::from_value::<Issue>(v.clone()).ok())
            .collect();
    }

    // Try direct verdict shape: { issues: [...] }
    if let Some(issues) = output.get("issues").and_then(|i| i.as_array()) {
        return issues
            .iter()
            .filter_map(|v| serde_json::from_value::<Issue>(v.clone()).ok())
            .collect();
    }

    Vec::new()
}

/// Load the judge prompt template from the .nautiloop/prompts directory.
async fn load_judge_prompt() -> String {
    let paths = [
        ".nautiloop/prompts/judge.md",
        "/etc/nautiloop/prompts/judge.md",
    ];

    for path in &paths {
        if let Ok(content) = tokio::fs::read_to_string(path).await {
            return content;
        }
    }

    // Fallback: embedded default prompt
    DEFAULT_JUDGE_PROMPT.to_string()
}

const DEFAULT_JUDGE_PROMPT: &str = r#"You are the orchestrator judge for a convergent loop engine. Your job is to decide whether the current loop should continue iterating, accept the current state as clean, escalate to a human, or fail.

You will receive a JSON context with the loop history, current verdict, and any recurring findings. Analyze the situation and return a structured JSON decision.

## Decision criteria

- **continue**: The issues are substantive and the implementor is making progress. Keep iterating.
- **exit_clean**: The remaining findings are trivial (cosmetic, stylistic, low-severity nits) and the spec's functional requirements are met. Accept and move on.
- **exit_escalate**: The loop is stuck (recurring findings not being addressed, churn detected) or the situation needs human judgment. Escalate to the engineer.
- **exit_fail**: The spec cannot be satisfied, there is a fundamental contradiction, or the implementor has demonstrated inability to address the core issues after multiple attempts.

## Key signals to weigh

1. **Severity distribution**: If only low-severity findings remain and functional requirements are met, lean toward exit_clean.
2. **Churn detection**: If the same findings (same category, file, line±2) appear across 2+ rounds, the implementor is not addressing them. Lean toward exit_escalate.
3. **Reviewer drift**: If new unrelated findings appear each round (scope creep beyond the spec), lean toward exit_clean for the spec-related work.
4. **Progress trajectory**: Compare issue counts and severities across rounds. Improving = continue. Stagnant = escalate. Worsening = fail.
5. **Round budget**: If near max_rounds and still have critical issues, lean toward exit_escalate over exit_fail (give the human a chance).

## Response format

Return ONLY a JSON object (no markdown, no explanation outside the JSON):

```json
{
  "decision": "continue" | "exit_clean" | "exit_escalate" | "exit_fail",
  "confidence": 0.0 to 1.0,
  "reasoning": "one-paragraph explanation of your decision",
  "hint": "optional short instruction for the next agent round (null if not applicable)"
}
```"#;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::verdict::Severity;

    #[test]
    fn test_parse_judge_response_raw_json() {
        let json = r#"{"decision": "continue", "confidence": 0.85, "reasoning": "Issues are being addressed", "hint": "Focus on the error handling"}"#;
        let output = parse_judge_response(json).unwrap();
        assert_eq!(output.decision, JudgeDecision::Continue);
        assert!((output.confidence - 0.85).abs() < f64::EPSILON);
        assert_eq!(output.hint.as_deref(), Some("Focus on the error handling"));
    }

    #[test]
    fn test_parse_judge_response_markdown_block() {
        let text = r#"Here is my analysis:

```json
{"decision": "exit_clean", "confidence": 0.92, "reasoning": "Only low-severity nits remain", "hint": null}
```

That's my decision."#;
        let output = parse_judge_response(text).unwrap();
        assert_eq!(output.decision, JudgeDecision::ExitClean);
    }

    #[test]
    fn test_parse_judge_response_embedded_json() {
        let text = r#"Based on analysis: {"decision": "exit_escalate", "confidence": 0.78, "reasoning": "Churn detected", "hint": null} end"#;
        let output = parse_judge_response(text).unwrap();
        assert_eq!(output.decision, JudgeDecision::ExitEscalate);
    }

    #[test]
    fn test_parse_judge_response_invalid() {
        let text = "This is not JSON at all";
        assert!(parse_judge_response(text).is_err());
    }

    #[test]
    fn test_parse_judge_response_exit_fail() {
        let json = r#"{"decision": "exit_fail", "confidence": 0.95, "reasoning": "Fundamental contradiction in spec"}"#;
        let output = parse_judge_response(json).unwrap();
        assert_eq!(output.decision, JudgeDecision::ExitFail);
        assert!(output.hint.is_none());
    }

    #[test]
    fn test_compute_recurring_findings_empty() {
        let issues = vec![Issue {
            severity: Severity::High,
            category: Some("correctness".to_string()),
            file: Some("src/main.rs".to_string()),
            line: Some(42),
            description: "Bug".to_string(),
            suggestion: "Fix it".to_string(),
        }];
        let rounds: Vec<RoundRecord> = vec![];
        let recurring = compute_recurring_findings(&issues, &rounds);
        assert!(recurring.is_empty());
    }

    #[test]
    fn test_compute_recurring_findings_match() {
        let current_issues = vec![Issue {
            severity: Severity::High,
            category: Some("correctness".to_string()),
            file: Some("src/main.rs".to_string()),
            line: Some(42),
            description: "Bug".to_string(),
            suggestion: "Fix it".to_string(),
        }];

        let prior_output = serde_json::json!({
            "issues": [{
                "severity": "high",
                "category": "correctness",
                "file": "src/main.rs",
                "line": 43,
                "description": "Same bug",
                "suggestion": "Fix it"
            }]
        });

        let rounds = vec![RoundRecord {
            id: Uuid::new_v4(),
            loop_id: Uuid::new_v4(),
            round: 2,
            stage: "review".to_string(),
            input: None,
            output: Some(prior_output),
            started_at: None,
            completed_at: None,
            duration_secs: Some(30),
            job_name: None,
        }];

        let recurring = compute_recurring_findings(&current_issues, &rounds);
        assert_eq!(recurring.len(), 1);
        assert_eq!(recurring[0].seen_in_rounds, vec![2]);
    }

    #[test]
    fn test_compute_recurring_findings_no_match_different_file() {
        let current_issues = vec![Issue {
            severity: Severity::High,
            category: Some("correctness".to_string()),
            file: Some("src/main.rs".to_string()),
            line: Some(42),
            description: "Bug".to_string(),
            suggestion: "Fix it".to_string(),
        }];

        let prior_output = serde_json::json!({
            "issues": [{
                "severity": "high",
                "category": "correctness",
                "file": "src/other.rs",
                "line": 42,
                "description": "Different bug",
                "suggestion": "Fix it"
            }]
        });

        let rounds = vec![RoundRecord {
            id: Uuid::new_v4(),
            loop_id: Uuid::new_v4(),
            round: 2,
            stage: "review".to_string(),
            input: None,
            output: Some(prior_output),
            started_at: None,
            completed_at: None,
            duration_secs: Some(30),
            job_name: None,
        }];

        let recurring = compute_recurring_findings(&current_issues, &rounds);
        assert!(recurring.is_empty());
    }

    #[test]
    fn test_lines_within_tolerance() {
        assert!(lines_within_tolerance(Some(42), Some(44), 2));
        assert!(lines_within_tolerance(Some(42), Some(42), 2));
        assert!(!lines_within_tolerance(Some(42), Some(45), 2));
        assert!(lines_within_tolerance(None, None, 2));
        assert!(!lines_within_tolerance(Some(42), None, 2));
    }

    #[test]
    fn test_judge_decision_as_str() {
        assert_eq!(JudgeDecision::Continue.as_str(), "continue");
        assert_eq!(JudgeDecision::ExitClean.as_str(), "exit_clean");
        assert_eq!(JudgeDecision::ExitEscalate.as_str(), "exit_escalate");
        assert_eq!(JudgeDecision::ExitFail.as_str(), "exit_fail");
    }

    #[test]
    fn test_judge_decision_from_str() {
        assert_eq!(
            JudgeDecision::parse_decision("continue"),
            Some(JudgeDecision::Continue)
        );
        assert_eq!(
            JudgeDecision::parse_decision("exit_clean"),
            Some(JudgeDecision::ExitClean)
        );
        assert_eq!(JudgeDecision::parse_decision("invalid"), None);
    }

    #[test]
    fn test_extract_issues_from_output_direct() {
        let output = serde_json::json!({
            "issues": [{
                "severity": "high",
                "category": "correctness",
                "file": "src/main.rs",
                "line": 42,
                "description": "Bug",
                "suggestion": "Fix"
            }]
        });
        let issues = extract_issues_from_output(&output);
        assert_eq!(issues.len(), 1);
    }

    #[test]
    fn test_extract_issues_from_output_envelope() {
        let output = serde_json::json!({
            "verdict": {
                "clean": false,
                "issues": [{
                    "severity": "low",
                    "description": "Nit",
                    "suggestion": "Maybe fix"
                }],
                "summary": "One nit"
            }
        });
        let issues = extract_issues_from_output(&output);
        assert_eq!(issues.len(), 1);
    }

    #[test]
    fn test_extract_issues_from_output_no_issues() {
        let output = serde_json::json!({"clean": true, "summary": "ok"});
        let issues = extract_issues_from_output(&output);
        assert!(issues.is_empty());
    }

    // Mock model client for testing the full evaluate flow
    struct MockJudgeClient {
        response: String,
    }

    #[async_trait::async_trait]
    impl JudgeModelClient for MockJudgeClient {
        async fn call(&self, _model: &str, _system: &str, _user: &str) -> Result<String> {
            Ok(self.response.clone())
        }
    }

    struct ErrorJudgeClient;

    #[async_trait::async_trait]
    impl JudgeModelClient for ErrorJudgeClient {
        async fn call(&self, _model: &str, _system: &str, _user: &str) -> Result<String> {
            Err(crate::error::NautiloopError::Internal(
                "Model unavailable".to_string(),
            ))
        }
    }

    #[tokio::test]
    async fn test_evaluate_disabled() {
        let config = OrchestratorConfig {
            judge_enabled: false,
            ..Default::default()
        };
        let store = Arc::new(crate::state::memory::MemoryStateStore::new());
        let client = Arc::new(MockJudgeClient {
            response: String::new(),
        });
        let judge = OrchestratorJudge::new(config, store, client).await;

        let result = judge
            .evaluate(
                Uuid::new_v4(),
                "specs/test.md",
                "test spec",
                "review",
                2,
                15,
                &serde_json::json!({}),
                &[],
                &[],
            )
            .await;
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_evaluate_success() {
        let config = OrchestratorConfig::default();
        let store = Arc::new(crate::state::memory::MemoryStateStore::new());
        let client = Arc::new(MockJudgeClient {
            response: r#"{"decision": "continue", "confidence": 0.8, "reasoning": "Keep going", "hint": "Focus on tests"}"#.to_string(),
        });
        let judge = OrchestratorJudge::new(config, store.clone(), client).await;

        let issues = vec![Issue {
            severity: Severity::High,
            category: Some("correctness".to_string()),
            file: Some("src/main.rs".to_string()),
            line: Some(42),
            description: "Bug".to_string(),
            suggestion: "Fix".to_string(),
        }];

        let result = judge
            .evaluate(
                Uuid::new_v4(),
                "specs/test.md",
                "test spec",
                "review",
                2,
                15,
                &serde_json::json!({"clean": false}),
                &issues,
                &[],
            )
            .await;

        assert!(result.is_some());
        let output = result.unwrap();
        assert_eq!(output.decision, JudgeDecision::Continue);
        assert_eq!(output.hint.as_deref(), Some("Focus on tests"));
    }

    #[tokio::test]
    async fn test_evaluate_error_falls_back() {
        let config = OrchestratorConfig::default();
        let store = Arc::new(crate::state::memory::MemoryStateStore::new());
        let client = Arc::new(ErrorJudgeClient);
        let judge = OrchestratorJudge::new(config, store, client).await;

        let result = judge
            .evaluate(
                Uuid::new_v4(),
                "specs/test.md",
                "test spec",
                "review",
                2,
                15,
                &serde_json::json!({}),
                &[Issue {
                    severity: Severity::High,
                    category: None,
                    file: None,
                    line: None,
                    description: "Bug".to_string(),
                    suggestion: "Fix".to_string(),
                }],
                &[],
            )
            .await;

        // Error -> falls back to None (heuristic)
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_evaluate_cost_ceiling() {
        let config = OrchestratorConfig {
            max_judge_calls_per_loop: 2,
            ..Default::default()
        };
        let store = Arc::new(crate::state::memory::MemoryStateStore::new());
        let loop_id = Uuid::new_v4();

        // Pre-populate 2 judge decisions
        for i in 0..2 {
            let record = JudgeDecisionRecord {
                id: Uuid::new_v4(),
                loop_id,
                round: i + 1,
                phase: "review".to_string(),
                trigger: "not_clean".to_string(),
                input_json: serde_json::json!({}),
                decision: "continue".to_string(),
                confidence: Some(0.8),
                reasoning: Some("test".to_string()),
                hint: None,
                duration_ms: 100,
                created_at: chrono::Utc::now(),
                loop_final_state: None,
                loop_terminated_at: None,
            };
            store.create_judge_decision(&record).await.unwrap();
        }

        let client = Arc::new(MockJudgeClient {
            response: r#"{"decision": "continue", "confidence": 0.8, "reasoning": "test"}"#
                .to_string(),
        });
        let judge = OrchestratorJudge::new(config, store, client).await;

        let result = judge
            .evaluate(
                loop_id,
                "specs/test.md",
                "test spec",
                "review",
                3,
                15,
                &serde_json::json!({}),
                &[Issue {
                    severity: Severity::High,
                    category: None,
                    file: None,
                    line: None,
                    description: "Bug".to_string(),
                    suggestion: "Fix".to_string(),
                }],
                &[],
            )
            .await;

        // Cost ceiling reached -> falls back to None
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_evaluate_exit_clean_one_shot_guard() {
        let config = OrchestratorConfig::default();
        let store = Arc::new(crate::state::memory::MemoryStateStore::new());
        let loop_id = Uuid::new_v4();

        // Pre-populate one exit_clean decision
        let record = JudgeDecisionRecord {
            id: Uuid::new_v4(),
            loop_id,
            round: 1,
            phase: "review".to_string(),
            trigger: "not_clean".to_string(),
            input_json: serde_json::json!({}),
            decision: "exit_clean".to_string(),
            confidence: Some(0.9),
            reasoning: Some("Looks good".to_string()),
            hint: None,
            duration_ms: 100,
            created_at: chrono::Utc::now(),
            loop_final_state: None,
            loop_terminated_at: None,
        };
        store.create_judge_decision(&record).await.unwrap();

        let client = Arc::new(MockJudgeClient {
            response: r#"{"decision": "exit_clean", "confidence": 0.9, "reasoning": "Still looks good"}"#
                .to_string(),
        });
        let judge = OrchestratorJudge::new(config, store, client).await;

        let result = judge
            .evaluate(
                loop_id,
                "specs/test.md",
                "test spec",
                "review",
                3,
                15,
                &serde_json::json!({}),
                &[Issue {
                    severity: Severity::Low,
                    category: None,
                    file: None,
                    line: None,
                    description: "Nit".to_string(),
                    suggestion: "Maybe".to_string(),
                }],
                &[],
            )
            .await;

        // Second exit_clean should be downgraded to continue
        assert!(result.is_some());
        let output = result.unwrap();
        assert_eq!(output.decision, JudgeDecision::Continue);
        assert!(output.reasoning.contains("downgraded from exit_clean"));
    }
}
