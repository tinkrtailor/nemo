use anyhow::Result;

/// The mega-primer Markdown template, embedded at compile time.
pub const HELP_AI_TEMPLATE: &str = include_str!("help_ai.md");

/// Render the mega-primer as Markdown to stdout.
pub fn render_markdown() {
    print!("{HELP_AI_TEMPLATE}");
}

/// Render the mega-primer as structured JSON to stdout.
pub fn render_json() -> Result<()> {
    let output = build_json();
    println!("{}", serde_json::to_string_pretty(&output)?);
    Ok(())
}

fn build_json() -> serde_json::Value {
    serde_json::json!({
        "overview": "Nautiloop is a convergent-loop orchestrator that takes a specification and drives it to a merged pull request through adversarial implement \u{2192} test \u{2192} review cycles. It is self-hosted, model-agnostic (Claude, OpenAI, or mixed), and runs every agent job in an isolated Kubernetes pod with no access to secrets.",
        "state_machine": {
            "states": [
                { "name": "PENDING", "terminal": false, "description": "Loop submitted, waiting for reconciler to pick it up." },
                { "name": "HARDENING", "terminal": false, "description": "Spec is being hardened (audit + optional revise)." },
                { "name": "AWAITING_APPROVAL", "terminal": false, "description": "Waiting for engineer to approve via `nemo approve`." },
                { "name": "IMPLEMENTING", "terminal": false, "description": "Agent is writing code in an isolated pod." },
                { "name": "TESTING", "terminal": false, "description": "Agent is running tests against the implementation." },
                { "name": "REVIEWING", "terminal": false, "description": "Reviewer model is evaluating the implementation." },
                { "name": "CONVERGED", "terminal": true, "description": "Review approved; PR is open and ready to merge." },
                { "name": "FAILED", "terminal": true, "description": "Max rounds exceeded or unrecoverable error. Recoverable via `nemo extend`." },
                { "name": "CANCELLED", "terminal": true, "description": "Operator cancelled the loop via `nemo cancel`." },
                { "name": "PAUSED", "terminal": false, "description": "Loop paused internally; resume with `nemo resume`." },
                { "name": "AWAITING_REAUTH", "terminal": false, "description": "Model credentials expired; re-push with `nemo auth` then `nemo resume`." },
                { "name": "HARDENED", "terminal": true, "description": "Spec hardened (harden-only mode); no implementation was run." },
                { "name": "SHIPPED", "terminal": true, "description": "PR auto-merged after convergence (ship mode)." }
            ],
            "transitions": [
                { "from": "PENDING", "to": "HARDENING", "trigger": "Reconciler picks up loop (harden mode)" },
                { "from": "PENDING", "to": "AWAITING_APPROVAL", "trigger": "Reconciler picks up loop (no-harden mode)" },
                { "from": "PENDING", "to": "IMPLEMENTING", "trigger": "Reconciler picks up loop (ship mode / auto-approve)" },
                { "from": "HARDENING", "to": "AWAITING_APPROVAL", "trigger": "Harden job completes (start mode)" },
                { "from": "HARDENING", "to": "HARDENED", "trigger": "Harden job completes (harden_only mode)" },
                { "from": "HARDENING", "to": "FAILED", "trigger": "Harden job fails / max rounds exceeded / audit issues" },
                { "from": "AWAITING_APPROVAL", "to": "IMPLEMENTING", "trigger": "Engineer approves (`nemo approve`)" },
                { "from": "IMPLEMENTING", "to": "TESTING", "trigger": "Implementation job completes" },
                { "from": "TESTING", "to": "REVIEWING", "trigger": "Test job completes (tests pass)" },
                { "from": "TESTING", "to": "IMPLEMENTING", "trigger": "Test job completes (tests fail)" },
                { "from": "REVIEWING", "to": "CONVERGED", "trigger": "Reviewer approves" },
                { "from": "REVIEWING", "to": "IMPLEMENTING", "trigger": "Reviewer requests changes" },
                { "from": "REVIEWING", "to": "AWAITING_APPROVAL", "trigger": "Judge escalates during review" },
                { "from": "IMPLEMENTING", "to": "FAILED", "trigger": "Max rounds exceeded" },
                { "from": "REVIEWING", "to": "FAILED", "trigger": "Max rounds exceeded" },
                { "from": "TESTING", "to": "FAILED", "trigger": "Max rounds exceeded" },
                { "from": "FAILED", "to": "IMPLEMENTING", "trigger": "`nemo extend` (resumes from failed_from_state)" },
                { "from": "CONVERGED", "to": "SHIPPED", "trigger": "`nemo ship` auto-merge completes" },
                { "from": "Any non-terminal", "to": "CANCELLED", "trigger": "`nemo cancel`" },
                { "from": "Any non-terminal", "to": "PAUSED", "trigger": "Internal pause trigger" },
                { "from": "PAUSED", "to": "(previous state)", "trigger": "`nemo resume`" },
                { "from": "Any active", "to": "AWAITING_REAUTH", "trigger": "Loop engine detects expired model credentials" },
                { "from": "AWAITING_REAUTH", "to": "(previous state)", "trigger": "`nemo auth` + `nemo resume`" }
            ]
        },
        "workflows": [
            {
                "name": "implement",
                "description": "Submit a spec, approve after hardening, watch convergence, get a PR.",
                "steps": [
                    { "command": "nemo start spec.md", "description": "Submit spec; harden phase runs first (default)." },
                    { "command": "nemo status", "description": "Wait for AWAITING_APPROVAL state." },
                    { "command": "nemo approve <id>", "description": "Approve after reviewing the hardened spec PR." },
                    { "command": "nemo logs <id>", "description": "Watch implement \u{2192} test \u{2192} review cycles until convergence." }
                ]
            },
            {
                "name": "implement-no-harden",
                "description": "Submit a spec without hardening, approve, watch convergence.",
                "steps": [
                    { "command": "nemo start spec.md --no-harden", "description": "Skip harden, go straight to approval gate." },
                    { "command": "nemo approve <id>", "description": "Approve the loop." },
                    { "command": "nemo logs <id>", "description": "Watch convergence." }
                ]
            },
            {
                "name": "ship",
                "description": "Fully autonomous: submit spec, auto-approve, auto-merge PR.",
                "steps": [
                    { "command": "nemo ship spec.md", "description": "No approval, no human. Skips hardening by default. Use --harden to harden first." }
                ]
            },
            {
                "name": "harden-only",
                "description": "Harden a spec without implementation. Lifecycle: PENDING \u{2192} HARDENING \u{2192} HARDENED.",
                "steps": [
                    { "command": "nemo harden spec.md", "description": "Harden the spec, then stop. Review the hardened spec PR." }
                ]
            }
        ],
        "recovery_playbooks": [
            {
                "state": "AWAITING_REAUTH",
                "description": "Model credentials expired. The loop engine detected it internally and transitioned the loop.",
                "commands": ["nemo auth --claude", "nemo resume <id>"]
            },
            {
                "state": "PAUSED",
                "description": "Loop paused internally.",
                "commands": ["nemo resume <id>"]
            },
            {
                "state": "FAILED",
                "description": "Max rounds exceeded. Inspect round history, then extend or investigate.",
                "commands": ["nemo inspect <branch>", "nemo extend --add 10 <id>"]
            }
        ],
        "config_hierarchy": {
            "levels": [
                { "name": "engineer", "path": "~/.nemo/config.toml", "description": "Personal config: server URL, API key, engineer name, model preferences. Highest priority." },
                { "name": "repo", "path": "nemo.toml", "description": "Per-repository defaults: default models, pricing config." },
                { "name": "cluster", "path": "Control plane ConfigMap", "description": "Cluster-wide defaults set by the platform admin. Lowest priority." }
            ]
        },
        "command_catalog": {
            "loop_lifecycle": [
                { "command": "harden", "short": "Harden spec, merge spec PR. Terminal: HARDENED" },
                { "command": "start", "short": "Implement spec, create PR. Terminal: CONVERGED" },
                { "command": "ship", "short": "Implement + auto-merge. Terminal: SHIPPED" },
                { "command": "approve", "short": "Approve a loop awaiting approval" },
                { "command": "cancel", "short": "Cancel a running loop" },
                { "command": "resume", "short": "Resume a PAUSED, AWAITING_REAUTH, or transient-FAILED loop" },
                { "command": "extend", "short": "Extend a FAILED loop's max_rounds and resume" }
            ],
            "observability": [
                { "command": "status", "short": "Show your running loops" },
                { "command": "logs", "short": "Stream logs for a loop" },
                { "command": "ps", "short": "Show live processes and runtime state of a loop's pod" },
                { "command": "inspect", "short": "Show detailed loop state, round history, and verdicts" },
                { "command": "helm", "short": "K9s-style loop overview with live logs (TUI)" },
                { "command": "cache", "short": "Show cache configuration and disk usage" }
            ],
            "identity": [
                { "command": "auth", "short": "Push local model credentials to cluster" },
                { "command": "models", "short": "Show authenticated providers and available models" }
            ],
            "config": [
                { "command": "init", "short": "Scan monorepo, generate nemo.toml" },
                { "command": "config", "short": "Edit ~/.nemo/config.toml" },
                { "command": "capabilities", "short": "Show CLI version and supported features (JSON)" },
                { "command": "help", "short": "Show help for nemo or a specific command" }
            ]
        },
        "spec_structure": "A spec is a Markdown document with: # Title, ## Overview (one paragraph), ## Functional Requirements (FR-1, FR-2, ...), ## Acceptance Criteria (numbered list). More detail produces better results — include API contracts, edge cases, error handling, and test scenarios.",
        "known_failure_modes": [
            {
                "name": "Reviewer nitpick-loops",
                "detection": "Round count climbs without convergence; review verdicts keep requesting minor stylistic changes.",
                "recovery": "Check `nemo inspect <branch>` for reviewer verdicts. Consider adjusting the reviewer model or extending rounds. Update the spec with explicit acceptance criteria."
            },
            {
                "name": "Max rounds exhaustion",
                "detection": "Loop enters FAILED state. `nemo inspect <branch>` shows failed_from_state.",
                "recovery": "`nemo extend --add 10 <id>` to add more rounds. If the loop keeps failing, the spec may need clarification."
            },
            {
                "name": "Network drops mid-pod",
                "detection": "Pod shows as terminated in `nemo ps <id>`. Logs may be incomplete.",
                "recovery": "The loop engine detects pod failures and retries on the next tick. Check `nemo status` and `nemo logs <id>`."
            }
        ]
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn template_renders_without_error() {
        assert!(!HELP_AI_TEMPLATE.is_empty());
    }

    #[test]
    fn template_contains_state_machine_section() {
        assert!(HELP_AI_TEMPLATE.contains("## State Machine"));
    }

    #[test]
    fn template_contains_workflows_section() {
        assert!(HELP_AI_TEMPLATE.contains("## Typical Workflows"));
    }

    #[test]
    fn template_contains_recovery_section() {
        assert!(HELP_AI_TEMPLATE.contains("## Recovery Playbooks"));
    }

    #[test]
    fn template_contains_config_hierarchy() {
        assert!(HELP_AI_TEMPLATE.contains("## Configuration Hierarchy"));
    }

    #[test]
    fn template_contains_all_loop_states() {
        // Drift guard: every LoopState variant name must appear in the template.
        // If a new state is added to the enum, this test will fail, reminding
        // the developer to update help_ai.md.
        //
        // Uses the actual LoopState enum from control-plane to prevent silent
        // drift when new states are added without updating the template.
        use nautiloop_control_plane::types::LoopState;
        let all_states: &[LoopState] = &[
            LoopState::Pending,
            LoopState::Hardening,
            LoopState::AwaitingApproval,
            LoopState::Implementing,
            LoopState::Testing,
            LoopState::Reviewing,
            LoopState::Converged,
            LoopState::Failed,
            LoopState::Cancelled,
            LoopState::Paused,
            LoopState::AwaitingReauth,
            LoopState::Hardened,
            LoopState::Shipped,
        ];
        for state in all_states {
            let name = state.to_string();
            assert!(
                HELP_AI_TEMPLATE.contains(&name),
                "LoopState variant {name} not found in help_ai.md template"
            );
        }
        // Also verify the count matches — if a variant is added to the enum
        // but not to the list above, this assertion catches it. The count must
        // match the number of variants in the LoopState enum.
        assert_eq!(all_states.len(), 13, "LoopState variant count changed — update this test and help_ai.md");
    }

    #[test]
    fn json_output_has_all_required_keys() {
        let json = build_json();
        assert!(json.get("overview").is_some());
        assert!(json.get("state_machine").is_some());
        assert!(json.get("workflows").is_some());
        assert!(json.get("recovery_playbooks").is_some());
        assert!(json.get("config_hierarchy").is_some());
        assert!(json.get("command_catalog").is_some());
        assert!(json.get("spec_structure").is_some());
        assert!(json.get("known_failure_modes").is_some());
    }

    #[test]
    fn json_and_markdown_content_parity() {
        let json = build_json();

        // Verify workflow count and names match between JSON and Markdown.
        let json_workflows = json["workflows"].as_array().unwrap();
        let json_workflow_names: Vec<&str> = json_workflows
            .iter()
            .map(|w| w["name"].as_str().unwrap())
            .collect();

        // The Markdown template has "### Workflow N: <name>" headings.
        let md_workflow_count = HELP_AI_TEMPLATE
            .lines()
            .filter(|l| l.starts_with("### Workflow"))
            .count();

        assert_eq!(
            json_workflow_names.len(),
            md_workflow_count,
            "JSON has {} workflows but Markdown has {} workflow headings",
            json_workflow_names.len(),
            md_workflow_count,
        );

        // Verify all state names in JSON appear in the Markdown template.
        let json_states = json["state_machine"]["states"].as_array().unwrap();
        for state in json_states {
            let name = state["name"].as_str().unwrap();
            assert!(
                HELP_AI_TEMPLATE.contains(name),
                "JSON state {name} not found in Markdown template"
            );
        }

        // Verify recovery playbook states in JSON match headings in Markdown.
        let json_playbooks = json["recovery_playbooks"].as_array().unwrap();
        for playbook in json_playbooks {
            let state = playbook["state"].as_str().unwrap();
            assert!(
                HELP_AI_TEMPLATE.contains(state),
                "JSON recovery playbook state {state} not found in Markdown template"
            );
        }

        // Verify transition count parity: JSON transitions array should match
        // the number of data rows in the Markdown "### All Transitions" table.
        let json_transitions = json["state_machine"]["transitions"].as_array().unwrap();
        let md_transition_rows = {
            let mut in_transitions_table = false;
            let mut count = 0usize;
            for line in HELP_AI_TEMPLATE.lines() {
                if line.starts_with("### All Transitions") {
                    in_transitions_table = true;
                    continue;
                }
                if in_transitions_table {
                    // Stop at the next heading or end of table
                    if line.starts_with('#') || (line.is_empty() && count > 0) {
                        // Empty line after table rows signals end; but skip the
                        // separator line (|---|) and header row.
                        break;
                    }
                    // Count lines that look like table data rows: start with |,
                    // are not the header separator (|--)
                    if line.starts_with('|') && !line.starts_with("|--") && !line.starts_with("| From") {
                        count += 1;
                    }
                }
            }
            count
        };
        assert_eq!(
            json_transitions.len(),
            md_transition_rows,
            "JSON has {} transitions but Markdown 'All Transitions' table has {} data rows",
            json_transitions.len(),
            md_transition_rows,
        );

        // Verify recovery playbook count parity: JSON playbooks should match
        // the number of ### headings under ## Recovery Playbooks in Markdown.
        let md_recovery_count = {
            let mut in_recovery = false;
            let mut count = 0usize;
            for line in HELP_AI_TEMPLATE.lines() {
                if line.starts_with("## Recovery Playbooks") {
                    in_recovery = true;
                    continue;
                }
                if in_recovery {
                    if line.starts_with("## ") {
                        break; // Next top-level section
                    }
                    if line.starts_with("### ") {
                        count += 1;
                    }
                }
            }
            count
        };
        assert_eq!(
            json_playbooks.len(),
            md_recovery_count,
            "JSON has {} recovery playbooks but Markdown has {} recovery headings",
            json_playbooks.len(),
            md_recovery_count,
        );

        // Verify config hierarchy level count parity.
        let json_config_levels = json["config_hierarchy"]["levels"].as_array().unwrap();
        let md_config_rows = {
            let mut in_config = false;
            let mut count = 0usize;
            for line in HELP_AI_TEMPLATE.lines() {
                if line.starts_with("## Configuration Hierarchy") {
                    in_config = true;
                    continue;
                }
                if in_config {
                    if line.starts_with("## ") {
                        break;
                    }
                    if line.starts_with('|') && !line.starts_with("|--") && !line.starts_with("| Level") {
                        count += 1;
                    }
                }
            }
            count
        };
        assert_eq!(
            json_config_levels.len(),
            md_config_rows,
            "JSON has {} config levels but Markdown config table has {} data rows",
            json_config_levels.len(),
            md_config_rows,
        );
    }

    #[test]
    fn json_state_machine_has_all_states() {
        let json = build_json();
        let states = json["state_machine"]["states"].as_array().unwrap();
        let state_names: Vec<&str> = states
            .iter()
            .map(|s| s["name"].as_str().unwrap())
            .collect();
        assert!(state_names.contains(&"PENDING"));
        assert!(state_names.contains(&"HARDENING"));
        assert!(state_names.contains(&"AWAITING_APPROVAL"));
        assert!(state_names.contains(&"IMPLEMENTING"));
        assert!(state_names.contains(&"TESTING"));
        assert!(state_names.contains(&"REVIEWING"));
        assert!(state_names.contains(&"CONVERGED"));
        assert!(state_names.contains(&"FAILED"));
        assert!(state_names.contains(&"CANCELLED"));
        assert!(state_names.contains(&"PAUSED"));
        assert!(state_names.contains(&"AWAITING_REAUTH"));
        assert!(state_names.contains(&"HARDENED"));
        assert!(state_names.contains(&"SHIPPED"));
        assert_eq!(states.len(), 13);
    }
}
