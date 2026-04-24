use super::is_terminal_state;
use crate::api_types::LoopSummary;

/// Loop command types (FR-3a).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoopCommand {
    Approve,
    Cancel,
    Resume,
    Extend,
    OpenPr,
}

impl LoopCommand {
    pub fn verb(self) -> &'static str {
        match self {
            Self::Approve => "approve",
            Self::Cancel => "cancel",
            Self::Resume => "resume",
            Self::Extend => "extend",
            Self::OpenPr => "open PR",
        }
    }
}

/// Validate whether an action is valid for the current loop state (FR-3c).
/// Returns Ok(()) if the action can proceed, or Err with a reason string.
pub fn validate_action(command: LoopCommand, loop_item: &LoopSummary) -> Result<(), String> {
    let state = loop_item.state.as_str();

    match command {
        LoopCommand::Approve => {
            if state == "AWAITING_APPROVAL" {
                Ok(())
            } else {
                Err(format!("cannot approve in state {state}"))
            }
        }
        LoopCommand::Cancel => {
            if is_terminal_state(state) {
                Err(format!("cannot cancel in state {state}"))
            } else {
                Ok(())
            }
        }
        LoopCommand::Resume => match state {
            "PAUSED" | "AWAITING_REAUTH" => Ok(()),
            "FAILED" if loop_item.failed_from_state.is_some() => Ok(()),
            "FAILED" => Err("cannot resume FAILED loop without resumable stage".to_string()),
            _ => Err(format!("cannot resume in state {state}")),
        },
        LoopCommand::Extend => match state {
            "FAILED" if loop_item.failed_from_state.is_some() => Ok(()),
            "FAILED" => Err("cannot extend FAILED loop without resumable stage".to_string()),
            _ => Err(format!("cannot extend in state {state}")),
        },
        LoopCommand::OpenPr => {
            if loop_item.spec_pr_url.is_some() {
                Ok(())
            } else {
                Err("no PR URL available".to_string())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_loop(state: &str) -> LoopSummary {
        LoopSummary {
            loop_id: uuid::Uuid::new_v4(),
            engineer: "alice".to_string(),
            spec_path: "specs/test.md".to_string(),
            branch: "agent/alice/test".to_string(),
            state: state.to_string(),
            sub_state: None,
            round: 1,
            current_stage: None,
            active_job_name: None,
            spec_pr_url: None,
            failed_from_state: None,
            kind: "implement".to_string(),
            max_rounds: 15,
            model_implementor: None,
            model_reviewer: None,
            created_at: "2026-04-10T10:00:00Z".to_string(),
            updated_at: "2026-04-10T10:00:00Z".to_string(),
            last_activity_at: None,
        }
    }

    #[test]
    fn approve_valid_in_awaiting_approval() {
        let l = make_loop("AWAITING_APPROVAL");
        assert!(validate_action(LoopCommand::Approve, &l).is_ok());
    }

    #[test]
    fn approve_invalid_in_implementing() {
        let l = make_loop("IMPLEMENTING");
        assert!(validate_action(LoopCommand::Approve, &l).is_err());
    }

    #[test]
    fn cancel_valid_in_implementing() {
        let l = make_loop("IMPLEMENTING");
        assert!(validate_action(LoopCommand::Cancel, &l).is_ok());
    }

    #[test]
    fn cancel_invalid_in_converged() {
        let l = make_loop("CONVERGED");
        assert!(validate_action(LoopCommand::Cancel, &l).is_err());
    }

    #[test]
    fn resume_valid_in_paused() {
        let l = make_loop("PAUSED");
        assert!(validate_action(LoopCommand::Resume, &l).is_ok());
    }

    #[test]
    fn resume_valid_in_failed_with_from_state() {
        let mut l = make_loop("FAILED");
        l.failed_from_state = Some("IMPLEMENTING".to_string());
        assert!(validate_action(LoopCommand::Resume, &l).is_ok());
    }

    #[test]
    fn resume_invalid_in_failed_without_from_state() {
        let l = make_loop("FAILED");
        assert!(validate_action(LoopCommand::Resume, &l).is_err());
    }

    #[test]
    fn extend_valid_in_failed_with_from_state() {
        let mut l = make_loop("FAILED");
        l.failed_from_state = Some("REVIEWING".to_string());
        assert!(validate_action(LoopCommand::Extend, &l).is_ok());
    }

    #[test]
    fn extend_invalid_in_implementing() {
        let l = make_loop("IMPLEMENTING");
        assert!(validate_action(LoopCommand::Extend, &l).is_err());
    }

    #[test]
    fn open_pr_valid_with_url() {
        let mut l = make_loop("CONVERGED");
        l.spec_pr_url = Some("https://github.com/org/repo/pull/1".to_string());
        assert!(validate_action(LoopCommand::OpenPr, &l).is_ok());
    }

    #[test]
    fn open_pr_invalid_without_url() {
        let l = make_loop("CONVERGED");
        assert!(validate_action(LoopCommand::OpenPr, &l).is_err());
    }
}
