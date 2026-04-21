use std::collections::HashMap;

use crate::api_types::RoundSummary;

/// Per-model token pricing (FR-7a).
#[derive(Debug, Clone)]
pub struct ModelPricing {
    pub input_per_1m: f64,
    pub output_per_1m: f64,
}

/// Pricing configuration (FR-7).
#[derive(Debug, Clone)]
pub struct PricingConfig {
    pub models: HashMap<String, ModelPricing>,
}

impl Default for PricingConfig {
    fn default() -> Self {
        let mut models = HashMap::new();
        // FR-7c: sane defaults for common models
        models.insert(
            "claude-opus-4-6".to_string(),
            ModelPricing {
                input_per_1m: 15.0,
                output_per_1m: 75.0,
            },
        );
        models.insert(
            "claude-sonnet-4-6".to_string(),
            ModelPricing {
                input_per_1m: 3.0,
                output_per_1m: 15.0,
            },
        );
        models.insert(
            "claude-haiku-4-5".to_string(),
            ModelPricing {
                input_per_1m: 1.0,
                output_per_1m: 5.0,
            },
        );
        models.insert(
            "gpt-4o".to_string(),
            ModelPricing {
                input_per_1m: 2.50,
                output_per_1m: 10.0,
            },
        );
        models.insert(
            "gpt-4o-mini".to_string(),
            ModelPricing {
                input_per_1m: 0.15,
                output_per_1m: 0.60,
            },
        );
        models.insert(
            "o3-mini".to_string(),
            ModelPricing {
                input_per_1m: 1.10,
                output_per_1m: 4.40,
            },
        );
        Self { models }
    }
}

impl PricingConfig {
    /// Calculate cost for given token usage with a specific model.
    /// Returns None if the model has no pricing entry (FR-7b).
    pub fn calculate_cost(
        &self,
        model: Option<&str>,
        input_tokens: u64,
        output_tokens: u64,
    ) -> Option<f64> {
        let model = model?;
        let pricing = self.models.get(model)?;
        let input_cost = (input_tokens as f64 / 1_000_000.0) * pricing.input_per_1m;
        let output_cost = (output_tokens as f64 / 1_000_000.0) * pricing.output_per_1m;
        Some(input_cost + output_cost)
    }

    /// Load pricing from a parsed TOML table. The table format is:
    /// ```toml
    /// [pricing]
    /// "claude-opus-4-6" = { input_per_1m = 15.00, output_per_1m = 75.00 }
    /// ```
    pub fn from_toml(table: &toml::Value) -> Self {
        let mut config = Self::default();
        if let Some(pricing) = table.get("pricing").and_then(|v| v.as_table()) {
            for (model, value) in pricing {
                if let (Some(input), Some(output)) = (
                    value
                        .get("input_per_1m")
                        .and_then(|v| v.as_float().or_else(|| v.as_integer().map(|i| i as f64))),
                    value
                        .get("output_per_1m")
                        .and_then(|v| v.as_float().or_else(|| v.as_integer().map(|i| i as f64))),
                ) {
                    config.models.insert(
                        model.clone(),
                        ModelPricing {
                            input_per_1m: input,
                            output_per_1m: output,
                        },
                    );
                }
            }
        }
        config
    }
}

/// Calculate cost for a round by splitting tokens between implementor and reviewer models.
///
/// Implement/test stages use the implementor model pricing.
/// Review/audit stages use the reviewer model pricing.
/// Revise stages use the implementor model pricing (spec revision is implementor work).
/// Returns None only if ALL token-producing stages lack pricing entries.
pub fn calculate_loop_round_cost(
    pricing: &PricingConfig,
    model_implementor: Option<&str>,
    model_reviewer: Option<&str>,
    round: &RoundSummary,
) -> Option<f64> {
    let mut total_cost = 0.0f64;
    let mut any_priced = false;

    // Implementor stages: implement, test, revise
    for data in [&round.implement, &round.test, &round.revise]
        .into_iter()
        .flatten()
    {
        let (inp, out) = extract_token_usage(data);
        if inp > 0 || out > 0 {
            let model = model_implementor.or(model_reviewer);
            if let Some(c) = pricing.calculate_cost(model, inp, out) {
                total_cost += c;
                any_priced = true;
            }
        }
    }

    // Reviewer stages: review, audit
    for data in [&round.review, &round.audit].into_iter().flatten() {
        let (inp, out) = extract_token_usage(data);
        if inp > 0 || out > 0 {
            let model = model_reviewer.or(model_implementor);
            if let Some(c) = pricing.calculate_cost(model, inp, out) {
                total_cost += c;
                any_priced = true;
            }
        }
    }

    if any_priced { Some(total_cost) } else { None }
}

/// Extract total token usage (input + output) from all stages of a round.
pub fn round_total_tokens(round: &RoundSummary) -> (u64, u64) {
    let mut total_input = 0u64;
    let mut total_output = 0u64;

    for data in [
        &round.implement,
        &round.test,
        &round.review,
        &round.audit,
        &round.revise,
    ]
    .into_iter()
    .flatten()
    {
        let (inp, out) = extract_token_usage(data);
        total_input += inp;
        total_output += out;
    }

    (total_input, total_output)
}

/// Extract token usage from a stage output value.
pub fn extract_token_usage(value: &serde_json::Value) -> (u64, u64) {
    // Direct token_usage field
    if let Some(usage) = value.get("token_usage") {
        let input = usage.get("input").and_then(|v| v.as_u64()).unwrap_or(0);
        let output = usage.get("output").and_then(|v| v.as_u64()).unwrap_or(0);
        return (input, output);
    }
    // Nested in verdict (review/audit stage)
    if let Some(usage) = value.get("verdict").and_then(|v| v.get("token_usage")) {
        let input = usage.get("input").and_then(|v| v.as_u64()).unwrap_or(0);
        let output = usage.get("output").and_then(|v| v.as_u64()).unwrap_or(0);
        return (input, output);
    }
    (0, 0)
}

/// Format token count in compact K/M form (FR-2a).
pub fn format_tokens(tokens: u64) -> String {
    if tokens >= 1_000_000 {
        format!("{:.1}M", tokens as f64 / 1_000_000.0)
    } else if tokens >= 1_000 {
        // Round to nearest K instead of truncating
        format!("{}K", (tokens + 500) / 1_000)
    } else {
        format!("{tokens}")
    }
}

/// Format cost as $X.XX, or $?.?? if unknown (FR-7b).
pub fn format_cost(cost: Option<f64>) -> String {
    match cost {
        Some(c) => format!("${:.2}", c),
        None => "$?.??".to_string(),
    }
}

/// Sum duration_secs fields across all stages in a round.
pub fn round_duration_secs(round: &RoundSummary) -> i64 {
    [
        round.implement_duration_secs,
        round.test_duration_secs,
        round.review_duration_secs,
        round.audit_duration_secs,
        round.revise_duration_secs,
    ]
    .iter()
    .filter_map(|d| *d)
    .sum()
}

/// Format seconds as "Xh Xm" or "Xm Xs".
pub fn format_duration_secs(secs: i64) -> String {
    if secs >= 3600 {
        let h = secs / 3600;
        let m = (secs % 3600) / 60;
        format!("{h}h {m:02}m")
    } else if secs >= 60 {
        let m = secs / 60;
        let s = secs % 60;
        format!("{m}m {s:02}s")
    } else {
        format!("{secs}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_tokens_k() {
        assert_eq!(format_tokens(52000), "52K");
        assert_eq!(format_tokens(52999), "53K"); // rounds up
        assert_eq!(format_tokens(999), "999");
    }

    #[test]
    fn format_tokens_m() {
        assert_eq!(format_tokens(1_200_000), "1.2M");
    }

    #[test]
    fn format_cost_known() {
        assert_eq!(format_cost(Some(0.84)), "$0.84");
        assert_eq!(format_cost(Some(1.5)), "$1.50");
    }

    #[test]
    fn format_cost_unknown() {
        assert_eq!(format_cost(None), "$?.??");
    }

    #[test]
    fn calculate_cost_known_model() {
        let config = PricingConfig::default();
        let cost = config.calculate_cost(Some("claude-haiku-4-5"), 100_000, 10_000);
        assert!(cost.is_some());
        let c = cost.unwrap();
        // input: 100K * 1.0/1M = 0.10, output: 10K * 5.0/1M = 0.05
        assert!((c - 0.15).abs() < 0.001);
    }

    #[test]
    fn calculate_cost_unknown_model() {
        let config = PricingConfig::default();
        assert!(
            config
                .calculate_cost(Some("unknown-model"), 100_000, 10_000)
                .is_none()
        );
    }

    #[test]
    fn calculate_cost_no_model() {
        let config = PricingConfig::default();
        assert!(config.calculate_cost(None, 100_000, 10_000).is_none());
    }

    #[test]
    fn extract_token_usage_direct() {
        let val = serde_json::json!({"token_usage": {"input": 45000, "output": 3200}});
        assert_eq!(extract_token_usage(&val), (45000, 3200));
    }

    #[test]
    fn extract_token_usage_nested_verdict() {
        let val = serde_json::json!({"verdict": {"clean": true, "token_usage": {"input": 5000, "output": 1000}}});
        assert_eq!(extract_token_usage(&val), (5000, 1000));
    }

    #[test]
    fn round_total_tokens_sums_stages() {
        let round = RoundSummary {
            round: 1,
            implement: Some(serde_json::json!({"token_usage": {"input": 10000, "output": 2000}})),
            test: Some(serde_json::json!({"token_usage": {"input": 0, "output": 0}})),
            review: Some(
                serde_json::json!({"verdict": {"clean": true, "token_usage": {"input": 5000, "output": 1000}}}),
            ),
            audit: None,
            revise: None,
            implement_duration_secs: Some(120),
            test_duration_secs: Some(30),
            review_duration_secs: Some(60),
            audit_duration_secs: None,
            revise_duration_secs: None,
        };
        let (inp, out) = round_total_tokens(&round);
        assert_eq!(inp, 15000);
        assert_eq!(out, 3000);
    }

    #[test]
    fn format_duration_hours() {
        assert_eq!(format_duration_secs(3600 + 22 * 60), "1h 22m");
    }

    #[test]
    fn format_duration_minutes() {
        assert_eq!(format_duration_secs(202), "3m 22s");
    }

    #[test]
    fn calculate_loop_round_cost_splits_by_stage() {
        let config = PricingConfig::default();
        // Implement tokens at Opus pricing, review tokens at Haiku pricing
        let round = RoundSummary {
            round: 1,
            implement: Some(serde_json::json!({"token_usage": {"input": 80000, "output": 8000}})),
            test: Some(serde_json::json!({"token_usage": {"input": 0, "output": 0}})),
            review: Some(
                serde_json::json!({"verdict": {"clean": true, "token_usage": {"input": 20000, "output": 2000}}}),
            ),
            audit: None,
            revise: None,
            implement_duration_secs: Some(120),
            test_duration_secs: Some(30),
            review_duration_secs: Some(60),
            audit_duration_secs: None,
            revise_duration_secs: None,
        };
        let cost = calculate_loop_round_cost(
            &config,
            Some("claude-opus-4-6"),
            Some("claude-haiku-4-5"),
            &round,
        );
        assert!(cost.is_some());
        let c = cost.unwrap();
        // impl: input 80K * 15/1M = 1.20, output 8K * 75/1M = 0.60 → 1.80
        // review: input 20K * 1/1M = 0.02, output 2K * 5/1M = 0.01 → 0.03
        // total = 1.83
        assert!((c - 1.83).abs() < 0.001);
    }

    #[test]
    fn calculate_loop_round_cost_falls_back_to_reviewer() {
        let config = PricingConfig::default();
        let round = RoundSummary {
            round: 1,
            implement: Some(serde_json::json!({"token_usage": {"input": 100000, "output": 10000}})),
            test: None,
            review: None,
            audit: None,
            revise: None,
            implement_duration_secs: None,
            test_duration_secs: None,
            review_duration_secs: None,
            audit_duration_secs: None,
            revise_duration_secs: None,
        };
        let cost = calculate_loop_round_cost(&config, None, Some("claude-haiku-4-5"), &round);
        assert!(cost.is_some());
        let c = cost.unwrap();
        // impl falls back to reviewer: input 100K * 1.0/1M = 0.10, output 10K * 5.0/1M = 0.05
        assert!((c - 0.15).abs() < 0.001);
    }

    #[test]
    fn calculate_loop_round_cost_none_when_no_model() {
        let config = PricingConfig::default();
        let round = RoundSummary {
            round: 1,
            implement: Some(serde_json::json!({"token_usage": {"input": 100000, "output": 10000}})),
            test: None,
            review: None,
            audit: None,
            revise: None,
            implement_duration_secs: None,
            test_duration_secs: None,
            review_duration_secs: None,
            audit_duration_secs: None,
            revise_duration_secs: None,
        };
        assert!(calculate_loop_round_cost(&config, None, None, &round).is_none());
    }

    #[test]
    fn pricing_from_toml_overrides_defaults() {
        let toml_str = r#"
        [pricing]
        "custom-model" = { input_per_1m = 2.0, output_per_1m = 8.0 }
        "#;
        let val: toml::Value = toml::from_str(toml_str).unwrap();
        let config = PricingConfig::from_toml(&val);
        assert!(config.models.contains_key("custom-model"));
        // Defaults still present
        assert!(config.models.contains_key("claude-opus-4-6"));
    }
}
