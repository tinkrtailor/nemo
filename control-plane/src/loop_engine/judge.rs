//! Orchestrator Judge — lightweight LLM judge for loop transition decisions.
//!
//! Invoked at review and harden evaluation points to decide `continue | exit_clean
//! | exit_escalate | exit_fail`, replacing the brittle heuristics that treated all
//! non-clean verdicts identically regardless of severity, churn, or scope creep.
//!
//! The judge runs as an in-process call via the model-proxy sidecar (FR-4c),
//! NOT as a k8s Job. Every invocation is logged to `judge_decisions` for
//! Stage 2 fine-tuning.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use uuid::Uuid;

use crate::config::OrchestratorConfig;
use crate::error::Result;
use crate::state::StateStore;
use crate::types::verdict::{Issue, Severity};
use crate::types::{JudgeDecisionRecord, RoundRecord};

/// The four possible decisions the judge can return (FR-3).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JudgeDecision {
    Continue,
    ExitClean,
    ExitEscalate,
    ExitFail,
}

impl std::fmt::Display for JudgeDecision {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Continue => write!(f, "continue"),
            Self::ExitClean => write!(f, "exit_clean"),
            Self::ExitEscalate => write!(f, "exit_escalate"),
            Self::ExitFail => write!(f, "exit_fail"),
        }
    }
}

/// Structured output from the judge (FR-3).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JudgeOutput {
    pub decision: JudgeDecision,
    pub confidence: Option<f32>,
    pub reasoning: Option<String>,
    pub hint: Option<String>,
}

impl Default for JudgeOutput {
    fn default() -> Self {
        Self {
            decision: JudgeDecision::Continue,
            confidence: None,
            reasoning: None,
            hint: None,
        }
    }
}

/// The trigger reason for why the judge was invoked (FR-1b).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JudgeTrigger {
    NotClean,
    MaxRounds,
    RecurringFindings,
}

impl std::fmt::Display for JudgeTrigger {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotClean => write!(f, "not_clean"),
            Self::MaxRounds => write!(f, "max_rounds"),
            Self::RecurringFindings => write!(f, "recurring_findings"),
        }
    }
}

/// A recurring finding detected across rounds.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecurringFinding {
    pub category: Option<String>,
    pub file: Option<String>,
    pub line: Option<u32>,
    pub seen_in_rounds: Vec<i32>,
}

/// Context passed to the judge (FR-2).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JudgeContext {
    pub loop_id: Uuid,
    pub spec_path: String,
    pub spec_content: Option<String>,
    pub phase: String,
    pub round: i32,
    pub max_rounds: i32,
    pub rounds: Vec<RoundSummaryForJudge>,
    pub current_verdict: serde_json::Value,
    pub recurring_findings: Vec<RecurringFinding>,
    /// Prompt template loaded from .nautiloop/prompts/judge.md.
    /// When present, {{CONTEXT}} is substituted with the serialized context JSON.
    /// When absent, the hardcoded fallback prompt is used.
    #[serde(skip)]
    pub prompt_template: Option<String>,
}

/// Simplified round summary for the judge input.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoundSummaryForJudge {
    pub round: i32,
    pub stage: String,
    pub verdict: Option<serde_json::Value>,
    pub duration_secs: Option<i64>,
}

/// Lightweight summary of JudgeContext for storage in judge_decisions.input_json.
/// Excludes spec_content (can be tens of KB) and round verdicts (O(n²) cumulative)
/// to keep the table from growing excessively.
#[derive(Debug, Clone, Serialize)]
struct JudgeContextSummary {
    loop_id: Uuid,
    spec_path: String,
    spec_content_len: usize,
    phase: String,
    round: i32,
    max_rounds: i32,
    round_stages: Vec<RoundStageSummary>,
    current_verdict: serde_json::Value,
    recurring_findings: Vec<RecurringFinding>,
}

/// Minimal per-round info for storage (stage + duration only, no verdict blob).
#[derive(Debug, Clone, Serialize)]
struct RoundStageSummary {
    round: i32,
    stage: String,
    duration_secs: Option<i64>,
}

impl JudgeContextSummary {
    fn from_context(ctx: &JudgeContext) -> Self {
        Self {
            loop_id: ctx.loop_id,
            spec_path: ctx.spec_path.clone(),
            spec_content_len: ctx.spec_content.as_ref().map_or(0, |s| s.len()),
            phase: ctx.phase.clone(),
            round: ctx.round,
            max_rounds: ctx.max_rounds,
            round_stages: ctx
                .rounds
                .iter()
                .map(|r| RoundStageSummary {
                    round: r.round,
                    stage: r.stage.clone(),
                    duration_secs: r.duration_secs,
                })
                .collect(),
            current_verdict: ctx.current_verdict.clone(),
            recurring_findings: ctx.recurring_findings.clone(),
        }
    }
}

/// Trait for the judge model client, enabling mock testing (NFR-5).
#[async_trait]
pub trait JudgeModelClient: Send + Sync + 'static {
    /// Send a prompt to the judge model and return the raw response body.
    async fn invoke(&self, model: &str, prompt: &str) -> Result<String>;
}

/// Production judge model client that calls the Anthropic API via the sidecar proxy.
pub struct SidecarJudgeClient {
    client: reqwest::Client,
    base_url: String,
}

impl SidecarJudgeClient {
    pub fn new(sidecar_base_url: &str) -> Self {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .expect("Failed to build reqwest client with 30s timeout");
        Self {
            client,
            base_url: sidecar_base_url.to_string(),
        }
    }
}

#[async_trait]
impl JudgeModelClient for SidecarJudgeClient {
    async fn invoke(&self, model: &str, prompt: &str) -> Result<String> {
        let url = format!("{}/anthropic/v1/messages", self.base_url);
        let body = serde_json::json!({
            "model": model,
            "max_tokens": 512,
            "messages": [
                {
                    "role": "user",
                    "content": prompt
                }
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
                crate::error::NautiloopError::Internal(format!("Judge HTTP request failed: {e}"))
            })?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(crate::error::NautiloopError::Internal(format!(
                "Judge model returned {status}: {body}"
            )));
        }

        let response_json: serde_json::Value = resp.json().await.map_err(|e| {
            crate::error::NautiloopError::Internal(format!(
                "Judge model response not valid JSON: {e}"
            ))
        })?;

        // Extract text from Anthropic Messages API response
        let text = response_json
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

/// The orchestrator judge. Holds config, model client, and state store references.
pub struct OrchestratorJudge {
    config: OrchestratorConfig,
    model_client: Arc<dyn JudgeModelClient>,
    store: Arc<dyn StateStore>,
    /// In-memory counter tracking all judge call attempts per loop (including
    /// failures/timeouts) for cost ceiling enforcement (NFR-1). Using an
    /// in-memory counter ensures failed API calls also count against the ceiling.
    call_counts: Mutex<HashMap<Uuid, u32>>,
    /// Cached prompt template, loaded once on first use to avoid repeated git reads.
    cached_prompt_template: Mutex<Option<Option<String>>>,
}

impl OrchestratorJudge {
    pub fn new(
        config: OrchestratorConfig,
        model_client: Arc<dyn JudgeModelClient>,
        store: Arc<dyn StateStore>,
    ) -> Self {
        Self {
            config,
            model_client,
            store,
            call_counts: Mutex::new(HashMap::new()),
            cached_prompt_template: Mutex::new(None),
        }
    }

    /// Check whether the judge should be invoked for this transition (FR-1a through FR-1c).
    /// Returns the trigger reason if yes, None if the judge should be skipped.
    pub fn should_invoke(
        &self,
        verdict_clean: bool,
        round: i32,
        max_rounds: i32,
        recurring_findings: &[RecurringFinding],
    ) -> Option<JudgeTrigger> {
        if !self.config.judge_enabled {
            return None;
        }

        // FR-1c: skip on clean=true AND round==1
        if verdict_clean && round == 1 {
            return None;
        }

        // FR-1b: determine trigger.
        // Priority order (highest → lowest): RecurringFindings > MaxRounds > NotClean.
        // RecurringFindings takes precedence because it's the most specific signal —
        // it tells the judge exactly which findings are stuck, enabling churn detection.
        // The judge receives full context regardless of trigger, so priority only
        // affects the `trigger` label logged to judge_decisions for Stage 2 analysis.
        if !recurring_findings.is_empty()
            && recurring_findings
                .iter()
                .any(|f| f.seen_in_rounds.len() >= 2)
        {
            return Some(JudgeTrigger::RecurringFindings);
        }

        if round >= max_rounds {
            return Some(JudgeTrigger::MaxRounds);
        }

        if !verdict_clean {
            return Some(JudgeTrigger::NotClean);
        }

        // verdict_clean && round > 1: no trigger applies, skip judge
        None
    }

    /// Invoke the judge and return its decision. On error or timeout, returns
    /// None so the driver falls back to heuristic behavior (FR-1d).
    pub async fn invoke(
        &self,
        context: &JudgeContext,
        trigger: &JudgeTrigger,
    ) -> Option<JudgeOutput> {
        let start = Instant::now();

        // NFR-1: Cost ceiling — check and increment in-memory counter.
        // This counts ALL attempts (including failures/timeouts) to accurately
        // bound API cost, since even failed HTTP requests incur some cost.
        {
            let mut counts = self.call_counts.lock().await;
            let count = counts.entry(context.loop_id).or_insert(0);
            if *count >= self.config.max_judge_calls {
                tracing::warn!(
                    loop_id = %context.loop_id,
                    call_count = *count,
                    max = self.config.max_judge_calls,
                    "Judge call count exceeded cost ceiling, short-circuiting to heuristic"
                );
                return None;
            }
            *count += 1;
        }

        // Build prompt
        let prompt = self.build_prompt(context).await;

        // Invoke model with 30s timeout (FR-1d)
        let model_result = tokio::time::timeout(
            std::time::Duration::from_secs(30),
            self.model_client.invoke(&self.config.judge_model, &prompt),
        )
        .await;

        let duration_ms = start.elapsed().as_millis() as i32;

        // ship-judge FR-4d: read cumulative attempt count (includes this attempt)
        // for structured logging on both success and failure paths.
        let judge_decision_total = {
            let counts = self.call_counts.lock().await;
            counts.get(&context.loop_id).copied().unwrap_or(0)
        };

        let response_text = match model_result {
            Ok(Ok(text)) => text,
            Ok(Err(e)) => {
                tracing::warn!(
                    target: "judge",
                    loop_id = %context.loop_id,
                    round = context.round,
                    error = %e,
                    duration_ms,
                    judge_decision_total,
                    "Judge model invocation failed, falling back to heuristic"
                );
                return None;
            }
            Err(_) => {
                tracing::warn!(
                    target: "judge",
                    loop_id = %context.loop_id,
                    round = context.round,
                    duration_ms,
                    judge_decision_total,
                    "Judge model timed out after 30s, falling back to heuristic"
                );
                return None;
            }
        };

        // Parse structured output
        let output = match self.parse_response(&response_text) {
            Some(o) => o,
            None => {
                tracing::warn!(
                    target: "judge",
                    loop_id = %context.loop_id,
                    round = context.round,
                    response = %response_text,
                    judge_decision_total,
                    "Failed to parse judge response, falling back to heuristic"
                );
                return None;
            }
        };

        // Log the decision (FR-5a)
        let decision_record = JudgeDecisionRecord {
            id: Uuid::new_v4(),
            loop_id: context.loop_id,
            round: context.round,
            phase: context.phase.clone(),
            trigger: trigger.to_string(),
            input_json: serde_json::to_value(JudgeContextSummary::from_context(context))
                .unwrap_or_default(),
            decision: output.decision.to_string(),
            confidence: output.confidence,
            reasoning: output.reasoning.clone(),
            hint: output.hint.clone(),
            duration_ms,
            created_at: chrono::Utc::now(),
            loop_final_state: None,
            loop_terminated_at: None,
        };

        if let Err(e) = self.store.create_judge_decision(&decision_record).await {
            tracing::warn!(
                loop_id = %context.loop_id,
                error = %e,
                "Failed to persist judge decision (non-blocking)"
            );
        }

        // ship-judge FR-4a/FR-4d: Log at INFO level with all required fields.
        // judge_decision_total counts all attempts for this loop (including this one).
        tracing::info!(
            target: "judge",
            loop_id = %context.loop_id,
            round = context.round,
            phase = %context.phase,
            trigger = %trigger,
            decision = %output.decision,
            confidence = ?output.confidence,
            duration_ms,
            judge_decision_total,
            "Judge decision"
        );

        Some(output)
    }

    /// Cache the prompt template from the context on first invocation.
    /// Subsequent calls reuse the cached value, avoiding repeated git reads.
    async fn cache_prompt_template(&self, template: Option<String>) {
        let mut cached = self.cached_prompt_template.lock().await;
        if cached.is_none() {
            *cached = Some(template);
        }
    }

    /// Build the judge prompt from context.
    /// Uses the cached prompt template from .nautiloop/prompts/judge.md if available,
    /// falling back to a hardcoded prompt otherwise.
    async fn build_prompt(&self, context: &JudgeContext) -> String {
        // Cache the template from this invocation's context
        self.cache_prompt_template(context.prompt_template.clone())
            .await;

        let context_json = serde_json::to_string_pretty(context).unwrap_or_default();

        // Use cached template if available
        let cached = self.cached_prompt_template.lock().await;
        if let Some(Some(ref template)) = *cached {
            return template.replace("{{CONTEXT}}", &context_json);
        }
        drop(cached);

        // Fallback: hardcoded prompt (used when template file is missing)
        format!(
            r#"You are an orchestrator judge for a convergent software engineering loop. Your job is to decide whether the loop should continue iterating, accept the current state as clean, escalate to a human, or fail.

## Context

{context_json}

## Decision Criteria

1. **Severity distribution**: If all remaining findings are `low` severity cosmetic issues (style, naming, minor nits) and the spec's functional requirements are met, you should `exit_clean`.
2. **Churn detection**: If the same findings (same category, file, similar line numbers) keep recurring across rounds without being addressed, continuing is wasteful. Consider `exit_escalate` or `exit_fail`.
3. **Reviewer drift / scope creep**: If new findings in later rounds are unrelated to the spec's requirements (style preferences, unrelated refactoring suggestions), they should not block convergence. Consider `exit_clean`.
4. **Progress**: If findings are being resolved and meaningful progress is happening, `continue` is appropriate.
5. **Max rounds**: If we're at or near max_rounds and still have significant issues, `exit_fail` may be appropriate. If issues are minor, `exit_clean` or `exit_escalate`.

## Output Format

Respond with ONLY a JSON object (no markdown fencing, no explanation outside the JSON):

{{
  "decision": "continue" | "exit_clean" | "exit_escalate" | "exit_fail",
  "confidence": 0.0 to 1.0,
  "reasoning": "short human-readable summary of why this decision",
  "hint": "optional short instruction for the next agent round (null if not applicable)"
}}

Decisions:
- `continue`: Keep iterating. The agent should address the findings.
- `exit_clean`: Accept the current implementation despite remaining findings. Only use when remaining issues are trivial/cosmetic.
- `exit_escalate`: Stop and ask a human to review. Use when the loop is stuck or findings are ambiguous.
- `exit_fail`: The loop cannot converge. Use for fundamental issues or repeated failures."#
        )
    }

    /// Parse the judge's response text into a structured JudgeOutput.
    fn parse_response(&self, response: &str) -> Option<JudgeOutput> {
        // Try parsing the response directly as JSON
        let trimmed = response.trim();

        // Try to extract JSON from markdown code fences if present
        let json_str = if let Some(start) = trimmed.find('{') {
            let end = trimmed.rfind('}')?;
            &trimmed[start..=end]
        } else {
            trimmed
        };

        let parsed: serde_json::Value = serde_json::from_str(json_str).ok()?;

        let decision_str = parsed.get("decision")?.as_str()?;
        let decision = match decision_str {
            "continue" => JudgeDecision::Continue,
            "exit_clean" => JudgeDecision::ExitClean,
            "exit_escalate" => JudgeDecision::ExitEscalate,
            "exit_fail" => JudgeDecision::ExitFail,
            _ => return None,
        };

        let confidence = parsed
            .get("confidence")
            .and_then(|v| v.as_f64())
            .map(|v| v as f32);

        let reasoning = parsed
            .get("reasoning")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        let hint = parsed.get("hint").and_then(|v| {
            if v.is_null() {
                None
            } else {
                v.as_str().map(|s| s.to_string())
            }
        });

        Some(JudgeOutput {
            decision,
            confidence,
            reasoning,
            hint,
        })
    }
}

/// Detect recurring findings across rounds (FR-1b).
/// A finding recurs if its `(category, file, line±2)` matches a finding in a previous round.
pub fn detect_recurring_findings(
    rounds: &[RoundRecord],
    current_issues: &[Issue],
    current_round: i32,
) -> Vec<RecurringFinding> {
    // Collect issues from previous rounds
    let mut prior_issues: Vec<(i32, Vec<Issue>)> = Vec::new();
    for round in rounds {
        if round.round >= current_round {
            continue;
        }
        if let Some(output) = &round.output {
            // Try to extract issues from verdict
            if let Some(issues) = extract_issues_from_output(output) {
                prior_issues.push((round.round, issues));
            }
        }
    }

    let mut recurring = Vec::new();

    for current_issue in current_issues {
        let mut seen_in_rounds = Vec::new();

        for (prior_round, prior_round_issues) in &prior_issues {
            for prior_issue in prior_round_issues {
                if findings_match(current_issue, prior_issue) {
                    seen_in_rounds.push(*prior_round);
                    break; // Only count each round once
                }
            }
        }

        if !seen_in_rounds.is_empty() {
            seen_in_rounds.push(current_round);
            seen_in_rounds.sort();
            seen_in_rounds.dedup();

            // Only add if we haven't already added a matching finding
            let already_tracked = recurring.iter().any(|r: &RecurringFinding| {
                r.category == current_issue.category
                    && r.file == current_issue.file
                    && line_near(r.line, current_issue.line, 2)
            });

            if !already_tracked {
                recurring.push(RecurringFinding {
                    category: current_issue.category.clone(),
                    file: current_issue.file.clone(),
                    line: current_issue.line,
                    seen_in_rounds,
                });
            }
        }
    }

    recurring
}

/// Check if two findings match: same category, same file, line within ±2.
fn findings_match(a: &Issue, b: &Issue) -> bool {
    // Category must match (both None or both Some with same value)
    if a.category != b.category {
        return false;
    }

    // File must match
    if a.file != b.file {
        return false;
    }

    // Line within ±2
    line_near(a.line, b.line, 2)
}

fn line_near(a: Option<u32>, b: Option<u32>, tolerance: u32) -> bool {
    match (a, b) {
        (Some(la), Some(lb)) => la.abs_diff(lb) <= tolerance,
        (None, None) => true,
        _ => false,
    }
}

/// Extract issues from a round output (verdict JSON).
fn extract_issues_from_output(output: &serde_json::Value) -> Option<Vec<Issue>> {
    // Try ReviewResultData envelope first
    if let Some(verdict_val) = output.get("verdict")
        && let Ok(issues) = extract_issues_from_verdict(verdict_val)
    {
        return Some(issues);
    }

    // Try direct verdict
    extract_issues_from_verdict(output).ok()
}

fn extract_issues_from_verdict(verdict: &serde_json::Value) -> std::result::Result<Vec<Issue>, ()> {
    let issues_val = verdict.get("issues").ok_or(())?;
    let issues: Vec<Issue> = serde_json::from_value(issues_val.clone()).map_err(|_| ())?;
    Ok(issues)
}

/// Build the rounds summary for the judge context from round records.
pub fn build_rounds_summary(rounds: &[RoundRecord]) -> Vec<RoundSummaryForJudge> {
    rounds
        .iter()
        .map(|r| RoundSummaryForJudge {
            round: r.round,
            stage: r.stage.clone(),
            verdict: r.output.clone(),
            duration_secs: r.duration_secs,
        })
        .collect()
}

/// Check if any issue in the list has severity above the given threshold.
pub fn has_blocking_issues(issues: &[Issue]) -> bool {
    issues
        .iter()
        .any(|i| matches!(i.severity, Severity::Critical | Severity::High))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::memory::MemoryStateStore;
    use crate::types::verdict::{Issue, Severity};

    /// Mock judge model client for testing.
    struct MockJudgeClient {
        response: tokio::sync::Mutex<String>,
    }

    impl MockJudgeClient {
        fn new(response: &str) -> Self {
            Self {
                response: tokio::sync::Mutex::new(response.to_string()),
            }
        }
    }

    #[async_trait]
    impl JudgeModelClient for MockJudgeClient {
        async fn invoke(&self, _model: &str, _prompt: &str) -> Result<String> {
            Ok(self.response.lock().await.clone())
        }
    }

    /// Mock client that always errors.
    struct FailingJudgeClient;

    #[async_trait]
    impl JudgeModelClient for FailingJudgeClient {
        async fn invoke(&self, _model: &str, _prompt: &str) -> Result<String> {
            Err(crate::error::NautiloopError::Internal(
                "Mock model failure".to_string(),
            ))
        }
    }

    /// Mock client that sleeps longer than the timeout.
    struct SlowJudgeClient;

    #[async_trait]
    impl JudgeModelClient for SlowJudgeClient {
        async fn invoke(&self, _model: &str, _prompt: &str) -> Result<String> {
            tokio::time::sleep(std::time::Duration::from_secs(60)).await;
            Ok("should not reach".to_string())
        }
    }

    fn test_config() -> OrchestratorConfig {
        OrchestratorConfig {
            judge_model: "test-model".to_string(),
            judge_enabled: true,
            max_judge_calls: 10,
        }
    }

    fn test_context() -> JudgeContext {
        JudgeContext {
            loop_id: Uuid::new_v4(),
            spec_path: "specs/test.md".to_string(),
            spec_content: None,
            phase: "review".to_string(),
            round: 3,
            max_rounds: 15,
            rounds: vec![],
            current_verdict: serde_json::json!({"clean": false, "issues": []}),
            recurring_findings: vec![],
            prompt_template: None,
        }
    }

    fn make_issue(
        severity: Severity,
        category: Option<&str>,
        file: Option<&str>,
        line: Option<u32>,
    ) -> Issue {
        Issue {
            severity,
            category: category.map(|s| s.to_string()),
            file: file.map(|s| s.to_string()),
            line,
            description: "test issue".to_string(),
            suggestion: "fix it".to_string(),
        }
    }

    // --- should_invoke tests ---

    #[test]
    fn test_should_invoke_disabled() {
        let config = OrchestratorConfig {
            judge_enabled: false,
            ..test_config()
        };
        let judge = OrchestratorJudge::new(
            config,
            Arc::new(MockJudgeClient::new("")),
            Arc::new(MemoryStateStore::new()),
        );
        assert!(judge.should_invoke(false, 1, 15, &[]).is_none());
    }

    #[test]
    fn test_should_invoke_clean_round_1_skipped() {
        let judge = OrchestratorJudge::new(
            test_config(),
            Arc::new(MockJudgeClient::new("")),
            Arc::new(MemoryStateStore::new()),
        );
        // FR-1c: clean=true, round=1 → skip
        assert!(judge.should_invoke(true, 1, 15, &[]).is_none());
    }

    #[test]
    fn test_should_invoke_not_clean() {
        let judge = OrchestratorJudge::new(
            test_config(),
            Arc::new(MockJudgeClient::new("")),
            Arc::new(MemoryStateStore::new()),
        );
        assert_eq!(
            judge.should_invoke(false, 2, 15, &[]),
            Some(JudgeTrigger::NotClean)
        );
    }

    #[test]
    fn test_should_invoke_max_rounds() {
        let judge = OrchestratorJudge::new(
            test_config(),
            Arc::new(MockJudgeClient::new("")),
            Arc::new(MemoryStateStore::new()),
        );
        assert_eq!(
            judge.should_invoke(false, 15, 15, &[]),
            Some(JudgeTrigger::MaxRounds)
        );
    }

    #[test]
    fn test_should_invoke_recurring_findings() {
        let judge = OrchestratorJudge::new(
            test_config(),
            Arc::new(MockJudgeClient::new("")),
            Arc::new(MemoryStateStore::new()),
        );
        let recurring = vec![RecurringFinding {
            category: Some("correctness".to_string()),
            file: Some("src/main.rs".to_string()),
            line: Some(42),
            seen_in_rounds: vec![2, 3],
        }];
        assert_eq!(
            judge.should_invoke(false, 3, 15, &recurring),
            Some(JudgeTrigger::RecurringFindings)
        );
    }

    // --- parse_response tests ---

    #[test]
    fn test_parse_continue_response() {
        let judge = OrchestratorJudge::new(
            test_config(),
            Arc::new(MockJudgeClient::new("")),
            Arc::new(MemoryStateStore::new()),
        );
        let response = r#"{"decision": "continue", "confidence": 0.85, "reasoning": "Issues are being addressed", "hint": "Focus on the null check"}"#;
        let output = judge.parse_response(response).unwrap();
        assert_eq!(output.decision, JudgeDecision::Continue);
        assert_eq!(output.confidence, Some(0.85));
        assert!(output.hint.is_some());
    }

    #[test]
    fn test_parse_exit_clean_response() {
        let judge = OrchestratorJudge::new(
            test_config(),
            Arc::new(MockJudgeClient::new("")),
            Arc::new(MemoryStateStore::new()),
        );
        let response = r#"{"decision": "exit_clean", "confidence": 0.9, "reasoning": "Only cosmetic nits remain", "hint": null}"#;
        let output = judge.parse_response(response).unwrap();
        assert_eq!(output.decision, JudgeDecision::ExitClean);
        assert!(output.hint.is_none());
    }

    #[test]
    fn test_parse_response_with_markdown_fence() {
        let judge = OrchestratorJudge::new(
            test_config(),
            Arc::new(MockJudgeClient::new("")),
            Arc::new(MemoryStateStore::new()),
        );
        let response = "Here is my decision:\n```json\n{\"decision\": \"exit_escalate\", \"confidence\": 0.7, \"reasoning\": \"Stuck\", \"hint\": null}\n```";
        let output = judge.parse_response(response).unwrap();
        assert_eq!(output.decision, JudgeDecision::ExitEscalate);
    }

    #[test]
    fn test_parse_invalid_response() {
        let judge = OrchestratorJudge::new(
            test_config(),
            Arc::new(MockJudgeClient::new("")),
            Arc::new(MemoryStateStore::new()),
        );
        assert!(judge.parse_response("not json at all").is_none());
        assert!(judge.parse_response(r#"{"decision": "invalid"}"#).is_none());
    }

    // --- invoke tests ---

    #[tokio::test]
    async fn test_invoke_success() {
        let response = r#"{"decision": "continue", "confidence": 0.8, "reasoning": "keep going", "hint": "fix the bug"}"#;
        let store = Arc::new(MemoryStateStore::new());
        let judge = OrchestratorJudge::new(
            test_config(),
            Arc::new(MockJudgeClient::new(response)),
            store.clone(),
        );

        let ctx = test_context();
        let output = judge.invoke(&ctx, &JudgeTrigger::NotClean).await.unwrap();
        assert_eq!(output.decision, JudgeDecision::Continue);
        assert_eq!(output.hint, Some("fix the bug".to_string()));

        // Verify decision was persisted
        let decisions = store.get_judge_decisions(ctx.loop_id).await.unwrap();
        assert_eq!(decisions.len(), 1);
        assert_eq!(decisions[0].decision, "continue");
    }

    #[tokio::test]
    async fn test_invoke_model_error_returns_none() {
        let store = Arc::new(MemoryStateStore::new());
        let judge = OrchestratorJudge::new(test_config(), Arc::new(FailingJudgeClient), store);
        let output = judge.invoke(&test_context(), &JudgeTrigger::NotClean).await;
        assert!(output.is_none());
    }

    #[tokio::test]
    async fn test_invoke_timeout_returns_none() {
        // Pause time so the 30s timeout resolves instantly in virtual time
        tokio::time::pause();

        let store = Arc::new(MemoryStateStore::new());
        let judge = OrchestratorJudge::new(test_config(), Arc::new(SlowJudgeClient), store);

        let ctx = test_context();
        let output = judge.invoke(&ctx, &JudgeTrigger::NotClean).await;
        // The SlowJudgeClient sleeps 60s but the judge has a 30s timeout
        assert!(output.is_none());
    }

    #[tokio::test]
    async fn test_invoke_parse_failure_returns_none() {
        let store = Arc::new(MemoryStateStore::new());
        let judge = OrchestratorJudge::new(
            test_config(),
            Arc::new(MockJudgeClient::new("this is not json")),
            store,
        );
        let output = judge.invoke(&test_context(), &JudgeTrigger::NotClean).await;
        assert!(output.is_none());
    }

    #[tokio::test]
    async fn test_invoke_cost_ceiling() {
        let config = OrchestratorConfig {
            max_judge_calls: 2,
            ..test_config()
        };
        let store = Arc::new(MemoryStateStore::new());
        let response =
            r#"{"decision": "continue", "confidence": 0.8, "reasoning": "ok", "hint": null}"#;
        let judge = OrchestratorJudge::new(
            config,
            Arc::new(MockJudgeClient::new(response)),
            store.clone(),
        );

        let ctx = test_context();

        // First two calls should succeed
        assert!(judge.invoke(&ctx, &JudgeTrigger::NotClean).await.is_some());
        assert!(judge.invoke(&ctx, &JudgeTrigger::NotClean).await.is_some());

        // Third call should be short-circuited
        assert!(judge.invoke(&ctx, &JudgeTrigger::NotClean).await.is_none());
    }

    // --- recurring findings detection tests ---

    #[test]
    fn test_detect_no_recurring_findings() {
        let issues = vec![make_issue(
            Severity::High,
            Some("correctness"),
            Some("a.rs"),
            Some(10),
        )];
        let rounds = vec![];
        let result = detect_recurring_findings(&rounds, &issues, 1);
        assert!(result.is_empty());
    }

    #[test]
    fn test_detect_recurring_finding_same_category_file_line() {
        let current_issues = vec![make_issue(
            Severity::High,
            Some("correctness"),
            Some("src/main.rs"),
            Some(42),
        )];

        let prior_verdict = serde_json::json!({
            "clean": false,
            "issues": [{
                "severity": "high",
                "category": "correctness",
                "file": "src/main.rs",
                "line": 43,
                "description": "prior issue",
                "suggestion": "fix"
            }],
            "summary": "issues found",
            "token_usage": {"input": 100, "output": 50}
        });

        let rounds = vec![RoundRecord {
            id: Uuid::new_v4(),
            loop_id: Uuid::new_v4(),
            round: 1,
            stage: "review".to_string(),
            input: None,
            output: Some(prior_verdict),
            started_at: None,
            completed_at: None,
            duration_secs: None,
            job_name: None,
        }];

        let result = detect_recurring_findings(&rounds, &current_issues, 2);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].seen_in_rounds, vec![1, 2]);
    }

    #[test]
    fn test_detect_no_recurrence_different_file() {
        let current_issues = vec![make_issue(
            Severity::High,
            Some("correctness"),
            Some("src/other.rs"),
            Some(42),
        )];

        let prior_verdict = serde_json::json!({
            "clean": false,
            "issues": [{
                "severity": "high",
                "category": "correctness",
                "file": "src/main.rs",
                "line": 42,
                "description": "prior issue",
                "suggestion": "fix"
            }],
            "summary": "issues found",
            "token_usage": {"input": 100, "output": 50}
        });

        let rounds = vec![RoundRecord {
            id: Uuid::new_v4(),
            loop_id: Uuid::new_v4(),
            round: 1,
            stage: "review".to_string(),
            input: None,
            output: Some(prior_verdict),
            started_at: None,
            completed_at: None,
            duration_secs: None,
            job_name: None,
        }];

        let result = detect_recurring_findings(&rounds, &current_issues, 2);
        assert!(result.is_empty());
    }

    #[test]
    fn test_line_near() {
        assert!(line_near(Some(42), Some(44), 2));
        assert!(line_near(Some(42), Some(42), 2));
        assert!(!line_near(Some(42), Some(45), 2));
        assert!(line_near(None, None, 2));
        assert!(!line_near(Some(42), None, 2));
    }

    #[test]
    fn test_has_blocking_issues() {
        assert!(has_blocking_issues(&[make_issue(
            Severity::Critical,
            None,
            None,
            None
        )]));
        assert!(has_blocking_issues(&[make_issue(
            Severity::High,
            None,
            None,
            None
        )]));
        assert!(!has_blocking_issues(&[make_issue(
            Severity::Medium,
            None,
            None,
            None
        )]));
        assert!(!has_blocking_issues(&[make_issue(
            Severity::Low,
            None,
            None,
            None
        )]));
        assert!(!has_blocking_issues(&[]));
    }

    // --- FR-7a: exit_clean one-shot guard ---

    #[tokio::test]
    async fn test_exit_clean_guard_tracked_externally() {
        // The one-shot guard for exit_clean is enforced by the driver, not the
        // judge itself. This test verifies the judge can return exit_clean
        // multiple times — the driver is responsible for converting the second
        // one to continue.
        let response = r#"{"decision": "exit_clean", "confidence": 0.9, "reasoning": "trivial", "hint": null}"#;
        let store = Arc::new(MemoryStateStore::new());
        let judge = OrchestratorJudge::new(
            test_config(),
            Arc::new(MockJudgeClient::new(response)),
            store,
        );

        let ctx = test_context();
        let o1 = judge.invoke(&ctx, &JudgeTrigger::NotClean).await.unwrap();
        let o2 = judge.invoke(&ctx, &JudgeTrigger::NotClean).await.unwrap();
        assert_eq!(o1.decision, JudgeDecision::ExitClean);
        assert_eq!(o2.decision, JudgeDecision::ExitClean);
    }

    // --- prompt assembly test ---

    #[tokio::test]
    async fn test_prompt_assembly_includes_context() {
        let judge = OrchestratorJudge::new(
            test_config(),
            Arc::new(MockJudgeClient::new("")),
            Arc::new(MemoryStateStore::new()),
        );
        let ctx = test_context();
        let prompt = judge.build_prompt(&ctx).await;
        assert!(prompt.contains("orchestrator judge"));
        assert!(prompt.contains(&ctx.loop_id.to_string()));
        assert!(prompt.contains("specs/test.md"));
    }

    // --- SidecarJudgeClient construction test ---

    #[test]
    fn test_sidecar_judge_client_constructs() {
        let client = SidecarJudgeClient::new("http://localhost:9090");
        assert_eq!(client.base_url, "http://localhost:9090");
    }

    // --- SidecarJudgeClient HTTP integration test ---

    #[tokio::test]
    async fn test_sidecar_judge_client_http_request_format() {
        use wiremock::matchers::{header, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let mock_server = MockServer::start().await;

        let anthropic_response = serde_json::json!({
            "id": "msg_test123",
            "type": "message",
            "role": "assistant",
            "content": [
                {
                    "type": "text",
                    "text": "{\"decision\": \"continue\", \"confidence\": 0.8, \"reasoning\": \"ok\", \"hint\": null}"
                }
            ],
            "model": "claude-haiku-4-5-20251001",
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 100, "output_tokens": 50}
        });

        Mock::given(method("POST"))
            .and(path("/anthropic/v1/messages"))
            .and(header("anthropic-version", "2023-06-01"))
            .and(header("content-type", "application/json"))
            .respond_with(ResponseTemplate::new(200).set_body_json(&anthropic_response))
            .expect(1)
            .mount(&mock_server)
            .await;

        let client = SidecarJudgeClient::new(&mock_server.uri());

        let result = client.invoke("claude-haiku-4-5", "test prompt").await;
        assert!(result.is_ok());
        let text = result.unwrap();
        assert!(text.contains("continue"));
        assert!(text.contains("confidence"));
    }

    #[tokio::test]
    async fn test_sidecar_judge_client_non_success_status() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let mock_server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/anthropic/v1/messages"))
            .respond_with(ResponseTemplate::new(401).set_body_string("invalid x-api-key"))
            .expect(1)
            .mount(&mock_server)
            .await;

        let client = SidecarJudgeClient::new(&mock_server.uri());

        let result = client.invoke("claude-haiku-4-5", "test prompt").await;
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("401"));
    }

    // --- NFR-1: Graceful degradation test ---

    #[tokio::test]
    async fn test_failing_judge_falls_through_to_heuristic() {
        // Verify that a failing JudgeModelClient returns None (heuristic fallback)
        let store = Arc::new(MemoryStateStore::new());
        let judge = OrchestratorJudge::new(test_config(), Arc::new(FailingJudgeClient), store);
        let ctx = test_context();
        let output = judge.invoke(&ctx, &JudgeTrigger::NotClean).await;
        assert!(
            output.is_none(),
            "Failed judge must return None for heuristic fallback"
        );
    }

    // --- NFR-3: Cost ceiling log test ---

    #[tokio::test]
    async fn test_cost_ceiling_logs_on_cap_hit() {
        let config = OrchestratorConfig {
            max_judge_calls: 1,
            ..test_config()
        };
        let store = Arc::new(MemoryStateStore::new());
        let response =
            r#"{"decision": "continue", "confidence": 0.8, "reasoning": "ok", "hint": null}"#;
        let judge = OrchestratorJudge::new(config, Arc::new(MockJudgeClient::new(response)), store);

        let ctx = test_context();
        // First call succeeds
        assert!(judge.invoke(&ctx, &JudgeTrigger::NotClean).await.is_some());
        // Second call short-circuits (cap is 1)
        assert!(
            judge.invoke(&ctx, &JudgeTrigger::NotClean).await.is_none(),
            "Call exceeding cost ceiling must short-circuit to heuristic"
        );
    }

    // --- Integration: judge writes decisions to store ---

    #[tokio::test]
    async fn test_judge_writes_decisions_to_store() {
        let response = r#"{"decision": "exit_clean", "confidence": 0.95, "reasoning": "all good", "hint": null}"#;
        let store = Arc::new(MemoryStateStore::new());
        let judge = OrchestratorJudge::new(
            test_config(),
            Arc::new(MockJudgeClient::new(response)),
            store.clone(),
        );

        let ctx = test_context();
        let output = judge
            .invoke(&ctx, &JudgeTrigger::RecurringFindings)
            .await
            .unwrap();
        assert_eq!(output.decision, JudgeDecision::ExitClean);

        // Verify decision row was written
        let decisions = store.get_judge_decisions(ctx.loop_id).await.unwrap();
        assert_eq!(decisions.len(), 1);
        assert_eq!(decisions[0].decision, "exit_clean");
        assert_eq!(decisions[0].trigger, "recurring_findings");
        assert_eq!(decisions[0].phase, "review");
        assert!(decisions[0].confidence.is_some());
    }

    // --- Full integration: SidecarJudgeClient → OrchestratorJudge → store write ---

    #[tokio::test]
    async fn test_full_integration_sidecar_client_judge_store() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let mock_server = MockServer::start().await;

        // Mock sidecar response (sidecar proxies to Anthropic, injects API key)
        let anthropic_response = serde_json::json!({
            "id": "msg_integration",
            "type": "message",
            "role": "assistant",
            "content": [
                {
                    "type": "text",
                    "text": "{\"decision\": \"exit_clean\", \"confidence\": 0.92, \"reasoning\": \"All issues resolved in latest round.\", \"hint\": null}"
                }
            ],
            "model": "claude-haiku-4-5-20251001",
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 200, "output_tokens": 60}
        });

        Mock::given(method("POST"))
            .and(path("/anthropic/v1/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_json(&anthropic_response))
            .expect(1)
            .mount(&mock_server)
            .await;

        // Wire: SidecarJudgeClient → OrchestratorJudge → MemoryStateStore
        let client = SidecarJudgeClient::new(&mock_server.uri());
        let store = Arc::new(MemoryStateStore::new());
        let judge = OrchestratorJudge::new(test_config(), Arc::new(client), store.clone());

        let ctx = test_context();
        let output = judge
            .invoke(&ctx, &JudgeTrigger::NotClean)
            .await
            .expect("Judge should return a decision");

        // Verify decision output
        assert_eq!(output.decision, JudgeDecision::ExitClean);
        assert_eq!(output.confidence, Some(0.92));
        assert_eq!(
            output.reasoning,
            Some("All issues resolved in latest round.".to_string())
        );

        // Verify decision was persisted to the store
        let decisions = store.get_judge_decisions(ctx.loop_id).await.unwrap();
        assert_eq!(decisions.len(), 1);
        assert_eq!(decisions[0].decision, "exit_clean");
        assert_eq!(decisions[0].trigger, "not_clean");
        assert_eq!(decisions[0].phase, "review");
        assert_eq!(decisions[0].confidence, Some(0.92));
        assert_eq!(
            decisions[0].reasoning,
            Some("All issues resolved in latest round.".to_string())
        );
    }
}
