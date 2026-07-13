use super::{Block, BlockKind, Limit, ObserveMode, TimeoutOutcome};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoopDecision {
    EnterBody,
    ExitConditionMet,
    ExitCountMet,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BranchDecision {
    Then,
    Else,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MacroControl {
    Continue,
    StopSuccess,
    StopError(String),
}

#[derive(Debug, Clone, PartialEq)]
pub enum WaitTimeoutDecision {
    Continue,
    StopError(String),
    RunBody(Vec<Block>),
}

pub const fn if_once_decision(condition_met: bool) -> BranchDecision {
    if condition_met {
        BranchDecision::Then
    } else {
        BranchDecision::Else
    }
}

pub fn propagate_macro_stop(current: MacroControl, next: MacroControl) -> MacroControl {
    match current {
        MacroControl::Continue => next,
        stop => stop,
    }
}

pub fn macro_control_for_block(kind: &BlockKind) -> MacroControl {
    match kind {
        BlockKind::StopSuccess => MacroControl::StopSuccess,
        BlockKind::StopError { message } => MacroControl::StopError(message.clone()),
        _ => MacroControl::Continue,
    }
}

pub fn wait_timeout_decision(outcome: &TimeoutOutcome) -> WaitTimeoutDecision {
    match outcome {
        TimeoutOutcome::StopError { message } => WaitTimeoutDecision::StopError(message.clone()),
        TimeoutOutcome::Continue => WaitTimeoutDecision::Continue,
        TimeoutOutcome::RunBody { body } => WaitTimeoutDecision::RunBody(body.clone()),
    }
}

pub fn condition_wait_timeout_decision(mode: &ObserveMode) -> Option<WaitTimeoutDecision> {
    match mode {
        ObserveMode::CheckNow => None,
        ObserveMode::WaitForTrue {
            timeout_outcome, ..
        }
        | ObserveMode::WaitForFalse {
            timeout_outcome, ..
        } => Some(wait_timeout_decision(timeout_outcome)),
    }
}

pub fn observation_satisfies_mode(mode: &ObserveMode, condition_met: bool) -> bool {
    match mode {
        ObserveMode::CheckNow | ObserveMode::WaitForTrue { .. } => condition_met,
        ObserveMode::WaitForFalse { .. } => !condition_met,
    }
}

pub fn repeat_n_decision(count: u32, completed_iterations: u32) -> LoopDecision {
    if completed_iterations < count {
        LoopDecision::EnterBody
    } else {
        LoopDecision::ExitCountMet
    }
}

pub fn evaluate_repeat_until_before_body(
    condition_met: bool,
    completed_iterations: u64,
    max_iterations: Limit<u64>,
) -> LoopDecision {
    if condition_met {
        LoopDecision::ExitConditionMet
    } else if matches!(max_iterations, Limit::Finite(limit) if completed_iterations >= limit) {
        LoopDecision::ExitCountMet
    } else {
        LoopDecision::EnterBody
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::macro_engine::BlockKind;

    #[test]
    fn repeat_until_checks_before_first_body() {
        assert_eq!(
            evaluate_repeat_until_before_body(true, 0, Limit::Unlimited),
            LoopDecision::ExitConditionMet
        );
    }

    #[test]
    fn zero_repeat_count_skips_body() {
        assert_eq!(repeat_n_decision(0, 0), LoopDecision::ExitCountMet);
    }

    #[test]
    fn repeat_n_uses_count_before_completed_iterations() {
        assert_eq!(repeat_n_decision(3, 0), LoopDecision::EnterBody);
        assert_eq!(repeat_n_decision(3, 3), LoopDecision::ExitCountMet);
    }

    #[test]
    fn repeat_until_honors_finite_iteration_bound() {
        assert_eq!(
            evaluate_repeat_until_before_body(false, 3, Limit::Finite(3)),
            LoopDecision::ExitCountMet
        );
    }

    #[test]
    fn if_selects_exactly_one_branch_per_entry() {
        assert_eq!(if_once_decision(true), BranchDecision::Then);
        assert_eq!(if_once_decision(false), BranchDecision::Else);
    }

    #[test]
    fn macro_stop_remains_sticky_across_nested_propagation() {
        let stop = MacroControl::StopError("nested failure".to_string());
        assert_eq!(
            propagate_macro_stop(stop.clone(), MacroControl::Continue),
            stop
        );
    }

    #[test]
    fn stop_blocks_map_to_macro_wide_control() {
        assert_eq!(
            macro_control_for_block(&BlockKind::StopSuccess),
            MacroControl::StopSuccess
        );
        assert_eq!(
            macro_control_for_block(&BlockKind::StopError {
                message: "failed".to_string()
            }),
            MacroControl::StopError("failed".to_string())
        );
    }

    #[test]
    fn wait_timeout_outcomes_are_explicit() {
        let timeout_body = vec![Block {
            id: "timeout-comment".to_string(),
            enabled: true,
            kind: BlockKind::Comment {
                text: "timed out".to_string(),
            },
        }];

        assert_eq!(
            wait_timeout_decision(&TimeoutOutcome::StopError {
                message: "timeout".to_string()
            }),
            WaitTimeoutDecision::StopError("timeout".to_string())
        );
        assert_eq!(
            wait_timeout_decision(&TimeoutOutcome::Continue),
            WaitTimeoutDecision::Continue
        );
        assert_eq!(
            wait_timeout_decision(&TimeoutOutcome::RunBody {
                body: timeout_body.clone()
            }),
            WaitTimeoutDecision::RunBody(timeout_body)
        );
    }

    #[test]
    fn standalone_condition_waits_expose_their_timeout_decision() {
        let mode = ObserveMode::WaitForFalse {
            timeout_ms: Limit::Unlimited,
            timeout_outcome: TimeoutOutcome::Continue,
        };
        assert_eq!(
            condition_wait_timeout_decision(&mode),
            Some(WaitTimeoutDecision::Continue)
        );
        assert_eq!(
            condition_wait_timeout_decision(&ObserveMode::CheckNow),
            None
        );
    }

    #[test]
    fn wait_modes_have_explicit_success_targets() {
        assert!(observation_satisfies_mode(&ObserveMode::CheckNow, true));
        assert!(!observation_satisfies_mode(&ObserveMode::CheckNow, false));
        assert!(observation_satisfies_mode(
            &ObserveMode::WaitForTrue {
                timeout_ms: Limit::Finite(1),
                timeout_outcome: TimeoutOutcome::Continue,
            },
            true,
        ));
        assert!(observation_satisfies_mode(
            &ObserveMode::WaitForFalse {
                timeout_ms: Limit::Finite(1),
                timeout_outcome: TimeoutOutcome::Continue,
            },
            false,
        ));
    }
}
