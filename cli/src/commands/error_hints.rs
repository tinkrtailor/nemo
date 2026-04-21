// Pattern-based recovery hints for common API errors.
//
// Each hint rule is a (status_code, body_substring, hint_text) tuple.
// Rules are checked in definition order; first match wins.
// Unrecognized errors pass through unchanged (no hint).

struct HintRule {
    status: Option<u16>,
    pattern: &'static str,
    hint: &'static str,
}

const HINT_RULES: &[HintRule] = &[
    HintRule {
        status: Some(409),
        pattern: "cannot approve: loop is in implementing",
        hint: "Loops in IMPLEMENTING are already running. Run `nemo logs <id>` to watch.",
    },
    HintRule {
        status: Some(409),
        pattern: "cannot approve: loop is in pending",
        hint: "Wait ~5s for the reconciler to advance PENDING \u{2192} AWAITING_APPROVAL, then retry.",
    },
    HintRule {
        status: Some(409),
        pattern: "cannot cancel: loop is in converged",
        hint: "This loop has already completed. Check the PR with `nemo inspect <branch>`.",
    },
    HintRule {
        status: Some(401),
        pattern: "unknown engineer",
        hint: "Run `nemo auth` to register your engineer identity with the cluster.",
    },
    HintRule {
        status: Some(401),
        pattern: "authentication failed",
        hint: "Check your API key with `nemo config`. If expired, regenerate and update ~/.nemo/config.toml.",
    },
    HintRule {
        status: Some(404),
        pattern: "spec not found",
        hint: "The spec path was not found in the git repository. Verify the branch and path are correct. Run `nemo start --help` for usage.",
    },
];

/// Look up a recovery hint for a given HTTP status code and error body.
///
/// Returns `Some(hint)` if a rule matches, `None` otherwise.
pub fn find_hint(status: u16, body: &str) -> Option<&'static str> {
    let body_lower = body.to_lowercase();
    for rule in HINT_RULES {
        let status_matches = rule.status.is_none_or(|s| s == status);
        if status_matches && body_lower.contains(rule.pattern) {
            return Some(rule.hint);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hint_approve_implementing() {
        let hint = find_hint(
            409,
            r#"{"error":"Cannot approve: loop is in IMPLEMENTING, not AWAITING_APPROVAL"}"#,
        );
        assert_eq!(
            hint,
            Some("Loops in IMPLEMENTING are already running. Run `nemo logs <id>` to watch.")
        );
    }

    #[test]
    fn hint_approve_pending() {
        let hint = find_hint(
            409,
            r#"{"error":"Cannot approve: loop is in PENDING, not AWAITING_APPROVAL"}"#,
        );
        assert!(hint.unwrap().contains("Wait ~5s"));
    }

    #[test]
    fn hint_cancel_converged() {
        let hint = find_hint(409, r#"{"error":"Cannot cancel: loop is in CONVERGED"}"#);
        assert!(hint.unwrap().contains("nemo inspect"));
    }

    #[test]
    fn hint_unknown_engineer() {
        let hint = find_hint(401, r#"{"error":"Unknown engineer"}"#);
        assert!(hint.unwrap().contains("nemo auth"));
    }

    #[test]
    fn hint_auth_failed() {
        let hint = find_hint(401, r#"{"error":"Authentication failed"}"#);
        assert!(hint.unwrap().contains("nemo config"));
    }

    #[test]
    fn hint_spec_not_found() {
        let hint = find_hint(404, r#"{"error":"Spec not found: path/to/spec.md"}"#);
        assert!(hint.unwrap().contains("spec path was not found"));
    }

    #[test]
    fn no_hint_generic_not_found() {
        // A generic 404 "not found" should NOT match the spec-specific hint
        let hint = find_hint(404, r#"{"error":"Loop not found"}"#);
        assert!(hint.is_none());
    }

    #[test]
    fn no_hint_for_unknown_error() {
        let hint = find_hint(500, r#"{"error":"Internal server error"}"#);
        assert!(hint.is_none());
    }

    #[test]
    fn no_hint_wrong_status() {
        // Pattern matches but status does not
        let hint = find_hint(
            500,
            r#"{"error":"Cannot approve: loop is in IMPLEMENTING"}"#,
        );
        assert!(hint.is_none());
    }
}
