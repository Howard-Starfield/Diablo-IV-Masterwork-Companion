use std::{
    collections::{HashMap, HashSet, VecDeque},
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::{self, Receiver, SyncSender, TrySendError},
    },
    time::Duration,
};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::engine::{
    automation::{CaptureSource, Clock, MouseButton, StopSource, TargetGuard, TargetSnapshot},
    config::MouseMovementProfile,
    types::Point,
};

use super::{
    Action, Block, BlockKind, Condition, ConditionDetector, DetectorEvidence, DetectorKind,
    JournalKind, JournalRecord, Limit, MacroDefinition, ObservationRequest, ObservationToken,
    ObserveMode, PinnedAsset, SavedRevision, TimeoutOutcome, if_once_decision,
    observation_satisfies_mode, repeat_n_decision, validate_macro,
};

static NEXT_RUN_ID: AtomicU64 = AtomicU64::new(1);
static NEXT_LIVE_SESSION_ID: AtomicU64 = AtomicU64::new(1);
static NEXT_COMMITTER_OWNER_ID: AtomicU64 = AtomicU64::new(1);
const DEFAULT_SYNCHRONOUS_EVENT_CAPACITY: usize = 4_096;
const FINAL_EVENT_RESERVE: usize = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunMode {
    ObservationOnly,
    DryRun,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Idle,
    Validating,
    Running,
    Paused,
    Stopping,
    Stopped,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StopReason {
    Completed,
    StopSuccess,
    StopError { message: String },
    UserStopped,
    EmergencyStopped,
    TechnicalFailure { message: String },
    SafetyLimit { message: String },
    UnsupportedBlock { block_id: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionState {
    Planned,
    Prepared,
    Committed,
    Dispatched,
    Blocked,
    UncertainDispatch,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TakeoverPolicy {
    Pause,
    Stop,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MovementOutcome {
    Reached,
    Cancelled,
    ManualTakeover,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SendInputFailure {
    ZeroInsertion {
        expected: u32,
        error_code: u32,
    },
    PartialInsertion {
        inserted: u32,
        expected: u32,
        error_code: u32,
    },
}

impl std::fmt::Display for SendInputFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ZeroInsertion {
                expected,
                error_code,
            } => write!(
                formatter,
                "SendInput inserted 0/{expected} events (Win32 error {error_code})"
            ),
            Self::PartialInsertion {
                inserted,
                expected,
                error_code,
            } => write!(
                formatter,
                "SendInput inserted {inserted}/{expected} events (Win32 error {error_code})"
            ),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PreCommitInputBlock {
    Stopped,
    ManualTakeover,
    InputFailure { message: String },
    Commit { reason: BlockReason },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputDispatchFailure {
    SendInput(SendInputFailure),
    Stopped,
    ManualTakeover,
    Validation { reason: BlockReason },
    InputFailure { message: String },
}

impl std::fmt::Display for InputDispatchFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SendInput(failure) => failure.fmt(formatter),
            Self::Stopped => formatter.write_str("stop observed after input commit"),
            Self::ManualTakeover => {
                formatter.write_str("manual mouse takeover observed after input commit")
            }
            Self::Validation { reason } => {
                write!(formatter, "post-movement validation failed: {reason:?}")
            }
            Self::InputFailure { message } => formatter.write_str(message),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommittedInputOutcome {
    Dispatched,
    UncertainDispatch { failure: InputDispatchFailure },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputDispatchOutcome {
    PreCommitBlocked(PreCommitInputBlock),
    Committed(CommittedInputOutcome),
}

/// Granular live-input boundary used only by `ActionCommitter`.
///
/// `MacroRuntime` deliberately does not own this trait in v1's observation-only modes, so merely
/// constructing the runtime cannot inject input.
pub trait LiveActionInput: Send + Sync {
    fn reset_manual_baseline(&self) -> Result<()>;
    fn manual_takeover_detected(&self) -> Result<bool>;
    /// Performs final stop/takeover checks, commits the attempt, and immediately issues the
    /// first movement `SendInput`. Once `commit` succeeds, only `Committed` outcomes are valid.
    fn dispatch_action(
        &self,
        point: Point,
        button: MouseButton,
        movement: Option<&MouseMovementProfile>,
        stop: &dyn StopSource,
        commit: &mut dyn FnMut() -> std::result::Result<(), BlockReason>,
        validate_after_movement: &mut dyn FnMut() -> std::result::Result<(), BlockReason>,
    ) -> InputDispatchOutcome;
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ActionAttemptId {
    run_id: String,
    action_instance_id: u64,
}

impl ActionAttemptId {
    #[cfg(test)]
    pub(crate) fn for_test(run_id: impl Into<String>, action_instance_id: u64) -> Self {
        Self {
            run_id: run_id.into(),
            action_instance_id,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ActionAuthorization {
    attempt_id: ActionAttemptId,
    block_id: String,
    action: Action,
    expected_target: TargetSnapshot,
    destination: Point,
    button: MouseButton,
    observation: Option<ObservationToken>,
    screen_authorized_rect: Option<crate::engine::types::Rect>,
    generation: u64,
    maximum_observation_age_ms: u64,
}

impl ActionAuthorization {
    #[cfg(test)]
    pub(crate) fn for_test(
        attempt_id: ActionAttemptId,
        expected_target: TargetSnapshot,
        destination: Point,
        button: MouseButton,
        observation: Option<ObservationToken>,
        generation: u64,
        maximum_observation_age_ms: u64,
    ) -> Self {
        let screen_authorized_rect = observation.as_ref().and_then(|token| {
            token
                .match_rect
                .and_then(|rect| local_rect_to_screen(expected_target.client_rect, rect).ok())
        });
        Self {
            attempt_id,
            block_id: "click".to_string(),
            action: Action::ClickImageMatch {
                source_block_id: "observe".to_string(),
                button,
            },
            expected_target,
            destination,
            button,
            observation,
            screen_authorized_rect,
            generation,
            maximum_observation_age_ms,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ActionPrepareRequest {
    authorization: ActionAuthorization,
    movement: Option<MouseMovementProfile>,
    minimum_click_interval_ms: u64,
    takeover_policy: TakeoverPolicy,
    resume: ResumeAuthorization,
}

impl ActionPrepareRequest {
    pub fn new(
        authorization: ActionAuthorization,
        movement: Option<MouseMovementProfile>,
        minimum_click_interval_ms: u64,
        takeover_policy: TakeoverPolicy,
        resume: ResumeAuthorization,
    ) -> Self {
        Self {
            authorization,
            movement,
            minimum_click_interval_ms,
            takeover_policy,
            resume,
        }
    }

    #[cfg(test)]
    pub(crate) fn set_destination_for_test(&mut self, destination: Point) {
        self.authorization.destination = destination;
    }

    #[cfg(test)]
    pub(crate) fn set_maximum_observation_age_for_test(&mut self, maximum: u64) {
        self.authorization.maximum_observation_age_ms = maximum;
    }

    #[cfg(test)]
    pub(crate) fn alter_observation_target_for_test(&mut self) {
        self.authorization.expected_target.window_id += 1;
    }

    #[cfg(test)]
    pub(crate) fn set_expected_foreground_for_test(&mut self, foreground: bool) {
        self.authorization.expected_target.is_foreground = foreground;
    }

    #[cfg(test)]
    pub(crate) fn set_takeover_policy_for_test(&mut self, policy: TakeoverPolicy) {
        self.takeover_policy = policy;
    }

    #[cfg(test)]
    pub(crate) fn set_minimum_click_interval_for_test(&mut self, interval_ms: u64) {
        self.minimum_click_interval_ms = interval_ms;
    }

    #[cfg(test)]
    pub(crate) fn set_resume_for_test(&mut self, resume: ResumeAuthorization) {
        self.resume = resume;
    }
}

#[derive(Debug, Clone)]
pub struct CommitContext {
    pub run_id: String,
    pub generation: u64,
    pub current_observation: Option<ObservationToken>,
}

impl CommitContext {
    pub fn new(
        run_id: impl Into<String>,
        generation: u64,
        current_observation: Option<ObservationToken>,
    ) -> Self {
        Self {
            run_id: run_id.into(),
            generation,
            current_observation,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BlockReason {
    ActionLockBusy,
    AttemptReplay,
    CrossCommitter,
    ResumeRequired,
    AttemptLedgerFull,
    ClickBudgetExceeded,
    RunFinished,
    Stopped,
    TargetChanged,
    StaleObservation,
    DestinationOutOfBounds,
    ClickPacing,
    ManualTakeover(TakeoverPolicy),
    MovementFailed { message: String },
    InputFailure { message: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActionOutcome {
    Blocked {
        reason: BlockReason,
        transitions: Vec<ActionState>,
    },
    Dispatched {
        transitions: Vec<ActionState>,
    },
    UncertainDispatch {
        message: String,
        transitions: Vec<ActionState>,
    },
}

impl ActionOutcome {
    pub fn transitions(&self) -> &[ActionState] {
        match self {
            Self::Blocked { transitions, .. }
            | Self::Dispatched { transitions }
            | Self::UncertainDispatch { transitions, .. } => transitions,
        }
    }
}

pub trait LiveControlSink: Send + Sync {
    fn pause_for_manual_takeover(&self);
    fn stop_for_manual_takeover(&self);
}

#[derive(Debug, Clone)]
pub struct ResumeAuthorization {
    session_id: u64,
    epoch: u64,
    target: TargetSnapshot,
}

impl ResumeAuthorization {
    #[cfg(test)]
    pub(crate) fn for_test(target: TargetSnapshot) -> Self {
        Self {
            session_id: 0,
            epoch: 1,
            target,
        }
    }
}

#[derive(Debug, Default)]
struct ResumeState {
    epoch: u64,
    current: Option<TargetSnapshot>,
}

pub struct LiveActionSession {
    id: u64,
    target: Arc<dyn TargetGuard + Send + Sync>,
    input: Arc<dyn LiveActionInput>,
    control: Arc<dyn LiveControlSink>,
    resume: Mutex<ResumeState>,
}

impl LiveActionSession {
    pub fn new(
        target: Arc<dyn TargetGuard + Send + Sync>,
        input: Arc<dyn LiveActionInput>,
        control: Arc<dyn LiveControlSink>,
    ) -> Arc<Self> {
        Arc::new(Self {
            id: NEXT_LIVE_SESSION_ID.fetch_add(1, Ordering::Relaxed),
            target,
            input,
            control,
            resume: Mutex::new(ResumeState::default()),
        })
    }

    pub fn resume(&self) -> Result<ResumeAuthorization> {
        let before = self.target.snapshot()?;
        anyhow::ensure!(
            target_is_actionable(&before),
            "target is not actionable during resume"
        );
        self.input.reset_manual_baseline()?;
        self.target.validate(&before)?;
        let mut state = self.resume.lock().expect("live resume gate poisoned");
        state.epoch = state.epoch.wrapping_add(1).max(1);
        state.current = Some(before.clone());
        Ok(ResumeAuthorization {
            session_id: self.id,
            epoch: state.epoch,
            target: before,
        })
    }

    fn validate_resume(&self, authorization: &ResumeAuthorization) -> bool {
        let state = self.resume.lock().expect("live resume gate poisoned");
        (authorization.session_id == self.id || cfg!(test) && authorization.session_id == 0)
            && authorization.epoch == state.epoch
            && state.current.as_ref() == Some(&authorization.target)
    }

    #[cfg(test)]
    pub(crate) fn activate_for_test(&self, target: TargetSnapshot) {
        let mut state = self.resume.lock().expect("live resume gate poisoned");
        state.epoch = 1;
        state.current = Some(target);
    }

    fn apply_takeover(&self, policy: TakeoverPolicy) {
        {
            let mut state = self.resume.lock().expect("live resume gate poisoned");
            state.current = None;
        }
        match policy {
            TakeoverPolicy::Pause => self.control.pause_for_manual_takeover(),
            TakeoverPolicy::Stop => self.control.stop_for_manual_takeover(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AttemptStatus {
    Prepared,
    Blocked,
    Committed,
    Dispatched,
    Uncertain,
}

#[derive(Debug)]
struct AttemptRecord {
    owner_id: u64,
    status: AttemptStatus,
}

#[derive(Debug)]
struct CommitLedger {
    run_id: String,
    maximum_clicks: Limit<u64>,
    maximum_attempts: usize,
    committed_clicks: u64,
    last_click_at_ms: Option<u64>,
    active: Option<ActionAttemptId>,
    attempts: HashMap<ActionAttemptId, AttemptRecord>,
    finished: bool,
}

impl CommitLedger {
    fn click_available(&self) -> bool {
        !matches!(
            self.maximum_clicks,
            Limit::Finite(maximum) if self.committed_clicks >= maximum
        )
    }
}

pub struct PreparedAction {
    request: ActionPrepareRequest,
    owner_id: u64,
    ledger: Arc<Mutex<CommitLedger>>,
}

#[derive(Debug, Clone)]
struct ReadyToCommit {
    request: ActionPrepareRequest,
    context: CommitContext,
}

impl std::fmt::Debug for PreparedAction {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PreparedAction")
            .field("request", &self.request)
            .finish_non_exhaustive()
    }
}

impl Drop for PreparedAction {
    fn drop(&mut self) {
        let attempt_id = &self.request.authorization.attempt_id;
        let mut ledger = self.ledger.lock().expect("action attempt ledger poisoned");
        if let Some(record) = ledger.attempts.get_mut(attempt_id)
            && record.owner_id == self.owner_id
            && record.status == AttemptStatus::Prepared
        {
            // Admission to the run ledger is permanent. Even a cancelled/abandoned prepared
            // action cannot reuse its once-only ID; bounded cleanup happens only at run finish.
            record.status = AttemptStatus::Blocked;
        }
        if ledger.active.as_ref() == Some(attempt_id) {
            ledger.active = None;
        }
    }
}

pub struct ActionCommitter {
    owner_id: u64,
    session: Arc<LiveActionSession>,
    clock: Arc<dyn Clock + Send + Sync>,
    ledger: Arc<Mutex<CommitLedger>>,
}

impl ActionCommitter {
    pub fn new(
        session: Arc<LiveActionSession>,
        clock: Arc<dyn Clock + Send + Sync>,
        run_id: impl Into<String>,
        maximum_clicks: Limit<u64>,
        maximum_attempts: usize,
    ) -> Self {
        assert!(
            maximum_attempts > 0,
            "action attempt ledger capacity must be positive"
        );
        Self {
            owner_id: NEXT_COMMITTER_OWNER_ID.fetch_add(1, Ordering::Relaxed),
            session,
            clock,
            ledger: Arc::new(Mutex::new(CommitLedger {
                run_id: run_id.into(),
                maximum_clicks,
                maximum_attempts,
                committed_clicks: 0,
                last_click_at_ms: None,
                active: None,
                attempts: HashMap::new(),
                finished: false,
            })),
        }
    }

    pub fn prepare(
        &self,
        request: ActionPrepareRequest,
    ) -> std::result::Result<PreparedAction, BlockReason> {
        if !self.session.validate_resume(&request.resume)
            || request.resume.target != request.authorization.expected_target
        {
            return Err(BlockReason::ResumeRequired);
        }
        if !target_is_actionable(&request.authorization.expected_target)
            || self
                .session
                .target
                .validate(&request.authorization.expected_target)
                .is_err()
        {
            return Err(BlockReason::TargetChanged);
        }
        let attempt_id = request.authorization.attempt_id.clone();
        let mut ledger = self.ledger.lock().expect("action attempt ledger poisoned");
        if ledger.finished {
            return Err(BlockReason::RunFinished);
        }
        if attempt_id.run_id != ledger.run_id {
            return Err(BlockReason::CrossCommitter);
        }
        if ledger.attempts.contains_key(&attempt_id) {
            return Err(BlockReason::AttemptReplay);
        }
        if ledger.active.is_some() {
            return Err(BlockReason::ActionLockBusy);
        }
        if ledger.attempts.len() >= ledger.maximum_attempts {
            return Err(BlockReason::AttemptLedgerFull);
        }
        ledger.active = Some(attempt_id.clone());
        ledger.attempts.insert(
            attempt_id,
            AttemptRecord {
                owner_id: self.owner_id,
                status: AttemptStatus::Prepared,
            },
        );
        drop(ledger);
        Ok(PreparedAction {
            request,
            owner_id: self.owner_id,
            ledger: Arc::clone(&self.ledger),
        })
    }

    pub fn commit(
        &self,
        prepared: PreparedAction,
        stop: &dyn StopSource,
        context: CommitContext,
    ) -> ActionOutcome {
        if prepared.owner_id != self.owner_id || !Arc::ptr_eq(&prepared.ledger, &self.ledger) {
            return ActionOutcome::Blocked {
                reason: BlockReason::CrossCommitter,
                transitions: vec![ActionState::Prepared, ActionState::Blocked],
            };
        }
        let blocked = |reason| ActionOutcome::Blocked {
            reason,
            transitions: vec![ActionState::Prepared, ActionState::Blocked],
        };

        let ready = match self.preflight(&prepared.request, stop, context) {
            Ok(ready) => ready,
            Err(reason) => {
                if matches!(reason, BlockReason::ManualTakeover(_)) {
                    self.session
                        .apply_takeover(prepared.request.takeover_policy);
                }
                return blocked(reason);
            }
        };
        let request = &ready.request;
        let authorization = &request.authorization;
        let attempt_id = authorization.attempt_id.clone();
        let committed = std::cell::Cell::new(false);
        let mut commit_boundary = || {
            if !self.session.validate_resume(&request.resume) {
                return Err(BlockReason::ResumeRequired);
            }
            let commit_time = self.clock.now_ms();
            let mut ledger = self.ledger.lock().expect("action attempt ledger poisoned");
            if ledger.finished {
                return Err(BlockReason::RunFinished);
            }
            if ledger.active.as_ref() != Some(&attempt_id) {
                return Err(BlockReason::CrossCommitter);
            }
            let valid_prepared = ledger.attempts.get(&attempt_id).is_some_and(|record| {
                record.owner_id == self.owner_id && record.status == AttemptStatus::Prepared
            });
            if !valid_prepared {
                return Err(BlockReason::AttemptReplay);
            }
            if !ledger.click_available() {
                return Err(BlockReason::ClickBudgetExceeded);
            }
            if ledger.last_click_at_ms.is_some_and(|previous| {
                commit_time < previous.saturating_add(request.minimum_click_interval_ms)
            }) {
                return Err(BlockReason::ClickPacing);
            }
            ledger.committed_clicks = ledger.committed_clicks.saturating_add(1);
            ledger.last_click_at_ms = Some(commit_time);
            ledger
                .attempts
                .get_mut(&attempt_id)
                .expect("prepared attempt disappeared")
                .status = AttemptStatus::Committed;
            committed.set(true);
            Ok(())
        };

        let mut validate_after_movement = || {
            if !self.session.validate_resume(&request.resume) {
                return Err(BlockReason::ResumeRequired);
            }
            if self
                .session
                .target
                .validate(&authorization.expected_target)
                .is_err()
            {
                return Err(BlockReason::TargetChanged);
            }
            if !observation_is_current(request, &ready.context, self.clock.now_ms()) {
                return Err(BlockReason::StaleObservation);
            }
            if !observation_authorizes_destination(request)
                || !point_inside_rect(
                    authorization.expected_target.client_rect,
                    authorization.destination,
                )
            {
                return Err(BlockReason::DestinationOutOfBounds);
            }
            Ok(())
        };

        let dispatch = self.session.input.dispatch_action(
            authorization.destination,
            authorization.button,
            request.movement.as_ref(),
            stop,
            &mut commit_boundary,
            &mut validate_after_movement,
        );
        match (committed.get(), dispatch) {
            (false, InputDispatchOutcome::PreCommitBlocked(block_reason)) => {
                self.map_precommit_block(block_reason, request.takeover_policy)
            }
            (false, InputDispatchOutcome::Committed(_)) => blocked(BlockReason::InputFailure {
                message: "input adapter reported a committed outcome before commit".to_string(),
            }),
            (true, InputDispatchOutcome::PreCommitBlocked(block_reason)) => {
                if matches!(block_reason, PreCommitInputBlock::ManualTakeover) {
                    self.session.apply_takeover(request.takeover_policy);
                }
                self.finish_attempt(&attempt_id, AttemptStatus::Uncertain);
                ActionOutcome::UncertainDispatch {
                    message: format!(
                        "input adapter returned a precommit block after commit: {block_reason:?}"
                    ),
                    transitions: vec![
                        ActionState::Prepared,
                        ActionState::Committed,
                        ActionState::UncertainDispatch,
                    ],
                }
            }
            (true, InputDispatchOutcome::Committed(CommittedInputOutcome::Dispatched)) => {
                self.finish_attempt(&attempt_id, AttemptStatus::Dispatched);
                ActionOutcome::Dispatched {
                    transitions: vec![
                        ActionState::Prepared,
                        ActionState::Committed,
                        ActionState::Dispatched,
                    ],
                }
            }
            (
                true,
                InputDispatchOutcome::Committed(CommittedInputOutcome::UncertainDispatch {
                    failure,
                }),
            ) => {
                if matches!(failure, InputDispatchFailure::ManualTakeover) {
                    self.session.apply_takeover(request.takeover_policy);
                }
                self.finish_attempt(&attempt_id, AttemptStatus::Uncertain);
                ActionOutcome::UncertainDispatch {
                    message: failure.to_string(),
                    transitions: vec![
                        ActionState::Prepared,
                        ActionState::Committed,
                        ActionState::UncertainDispatch,
                    ],
                }
            }
        }
    }

    fn preflight(
        &self,
        request: &ActionPrepareRequest,
        stop: &dyn StopSource,
        context: CommitContext,
    ) -> std::result::Result<ReadyToCommit, BlockReason> {
        let authorization = &request.authorization;
        if stop.is_stopped() {
            return Err(BlockReason::Stopped);
        }
        match self.session.input.manual_takeover_detected() {
            Ok(true) => return Err(BlockReason::ManualTakeover(request.takeover_policy)),
            Ok(false) => {}
            Err(error) => {
                return Err(BlockReason::InputFailure {
                    message: error.to_string(),
                });
            }
        }
        if !self.session.validate_resume(&request.resume)
            || request.resume.target != authorization.expected_target
        {
            return Err(BlockReason::ResumeRequired);
        }
        if !target_is_actionable(&authorization.expected_target)
            || self
                .session
                .target
                .validate(&authorization.expected_target)
                .is_err()
        {
            return Err(BlockReason::TargetChanged);
        }
        if !point_inside_rect(
            authorization.expected_target.client_rect,
            authorization.destination,
        ) || !observation_authorizes_destination(request)
        {
            return Err(BlockReason::DestinationOutOfBounds);
        }
        if !observation_is_current(request, &context, self.clock.now_ms()) {
            return Err(BlockReason::StaleObservation);
        }
        let now = self.clock.now_ms();
        let ledger = self.ledger.lock().expect("action attempt ledger poisoned");
        if ledger.finished {
            return Err(BlockReason::RunFinished);
        }
        if !ledger.click_available() {
            return Err(BlockReason::ClickBudgetExceeded);
        }
        if ledger
            .last_click_at_ms
            .is_some_and(|last| now < last.saturating_add(request.minimum_click_interval_ms))
        {
            return Err(BlockReason::ClickPacing);
        }
        drop(ledger);
        Ok(ReadyToCommit {
            request: request.clone(),
            context,
        })
    }

    fn map_precommit_block(
        &self,
        block: PreCommitInputBlock,
        takeover_policy: TakeoverPolicy,
    ) -> ActionOutcome {
        let reason = match block {
            PreCommitInputBlock::Stopped => BlockReason::Stopped,
            PreCommitInputBlock::ManualTakeover => {
                self.session.apply_takeover(takeover_policy);
                BlockReason::ManualTakeover(takeover_policy)
            }
            PreCommitInputBlock::InputFailure { message } => BlockReason::InputFailure { message },
            PreCommitInputBlock::Commit { reason } => reason,
        };
        ActionOutcome::Blocked {
            reason,
            transitions: vec![ActionState::Prepared, ActionState::Blocked],
        }
    }

    fn finish_attempt(&self, attempt_id: &ActionAttemptId, status: AttemptStatus) {
        let mut ledger = self.ledger.lock().expect("action attempt ledger poisoned");
        if let Some(record) = ledger.attempts.get_mut(attempt_id)
            && record.owner_id == self.owner_id
            && record.status == AttemptStatus::Committed
        {
            record.status = status;
        }
        if ledger.active.as_ref() == Some(attempt_id) {
            ledger.active = None;
        }
    }

    pub fn finish_run(&self) -> std::result::Result<(), BlockReason> {
        let mut ledger = self.ledger.lock().expect("action attempt ledger poisoned");
        if ledger.active.is_some() {
            return Err(BlockReason::ActionLockBusy);
        }
        ledger.attempts.clear();
        ledger.finished = true;
        Ok(())
    }

    pub fn committed_clicks(&self) -> u64 {
        self.ledger
            .lock()
            .expect("action attempt ledger poisoned")
            .committed_clicks
    }
}

fn target_is_actionable(target: &TargetSnapshot) -> bool {
    target.is_visible && !target.is_minimized && target.is_foreground
}

fn point_inside_rect(rect: crate::engine::types::Rect, point: Point) -> bool {
    let right = i64::from(rect.x) + i64::from(rect.width);
    let bottom = i64::from(rect.y) + i64::from(rect.height);
    i64::from(point.x) >= i64::from(rect.x)
        && i64::from(point.y) >= i64::from(rect.y)
        && i64::from(point.x) < right
        && i64::from(point.y) < bottom
}

fn observation_is_current(
    request: &ActionPrepareRequest,
    context: &CommitContext,
    now_ms: u64,
) -> bool {
    let authorization = &request.authorization;
    if context.run_id != authorization.attempt_id.run_id
        || context.generation != authorization.generation
    {
        return false;
    }
    let Some(expected) = authorization.observation.as_ref() else {
        return context.current_observation.is_none();
    };
    let Some(current) = context.current_observation.as_ref() else {
        return false;
    };
    if current != expected || !current.is_current(&context.run_id, context.generation) {
        return false;
    }
    if current.captured_at_ms > now_ms
        || now_ms.saturating_sub(current.captured_at_ms) > authorization.maximum_observation_age_ms
    {
        return false;
    }
    let Some(frame) = current.frame_metadata else {
        return true;
    };
    frame.frame_id == current.frame_id
        && frame.captured_at_ms == current.captured_at_ms
        && frame.window_id == authorization.expected_target.window_id
        && frame.window_revision == authorization.expected_target.window_revision
        && frame.client_x == authorization.expected_target.client_rect.x
        && frame.client_y == authorization.expected_target.client_rect.y
        && frame.client_width == authorization.expected_target.client_rect.width
        && frame.client_height == authorization.expected_target.client_rect.height
        && frame.geometry_revision == authorization.expected_target.geometry_revision
        && frame.display_profile_revision == authorization.expected_target.display_profile_revision
        && frame.dpi == authorization.expected_target.dpi
        && frame.region_revision == current.region_revision
        && frame.rule_revision == current.rule_revision
}

fn observation_authorizes_destination(request: &ActionPrepareRequest) -> bool {
    request
        .authorization
        .screen_authorized_rect
        .is_none_or(|rect| point_inside_rect(rect, request.authorization.destination))
}

#[derive(Debug, Clone, PartialEq)]
pub enum RuntimeCommand {
    Start {
        revision: SavedRevision,
    },
    Pause,
    Resume,
    Stop,
    EmergencyStop,
    Validate {
        revision: SavedRevision,
    },
    DryRun {
        revision: SavedRevision,
    },
    TestDetector {
        revision: SavedRevision,
        condition: Condition,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandDelivery {
    Sent,
    Full,
    Disconnected,
}

#[derive(Default)]
struct EmergencySignal {
    requested: AtomicBool,
    wake_lock: Mutex<()>,
    wake: Condvar,
}

impl EmergencySignal {
    fn request(&self) {
        self.requested.store(true, Ordering::Release);
        self.wake.notify_all();
    }

    fn reset(&self) {
        self.requested.store(false, Ordering::Release);
    }

    fn requested(&self) -> bool {
        self.requested.load(Ordering::Acquire)
    }

    fn notify(&self) {
        self.wake.notify_all();
    }

    fn wait(&self, duration: Duration) {
        let guard = self.wake_lock.lock().expect("runtime wake lock poisoned");
        if !self.requested() {
            let _ = self
                .wake
                .wait_timeout(guard, duration)
                .expect("runtime wake lock poisoned while waiting");
        }
    }
}

#[derive(Clone)]
pub struct RuntimeCommandSender {
    sender: SyncSender<RuntimeCommand>,
    emergency_stop: Arc<EmergencySignal>,
    emergency_notice: Arc<AtomicBool>,
}

impl RuntimeCommandSender {
    pub fn send(&self, command: RuntimeCommand) -> CommandDelivery {
        if matches!(command, RuntimeCommand::EmergencyStop) {
            self.emergency_stop.request();
            self.emergency_notice.store(true, Ordering::Release);
            return match self.sender.try_send(command) {
                Ok(()) => CommandDelivery::Sent,
                Err(TrySendError::Full(_)) => CommandDelivery::Full,
                Err(TrySendError::Disconnected(_)) => CommandDelivery::Disconnected,
            };
        }
        match self.sender.send(command) {
            Ok(()) => CommandDelivery::Sent,
            Err(_) => CommandDelivery::Disconnected,
        }
    }

    pub fn try_send(&self, command: RuntimeCommand) -> CommandDelivery {
        if matches!(command, RuntimeCommand::EmergencyStop) {
            self.emergency_stop.request();
            self.emergency_notice.store(true, Ordering::Release);
        }
        match self.sender.try_send(command) {
            Ok(()) => CommandDelivery::Sent,
            Err(TrySendError::Full(_)) => CommandDelivery::Full,
            Err(TrySendError::Disconnected(_)) => CommandDelivery::Disconnected,
        }
    }

    pub fn emergency_stop_requested(&self) -> bool {
        self.emergency_stop.requested()
    }
}

pub struct RuntimeCommandReceiver {
    receiver: Receiver<RuntimeCommand>,
    emergency_notice: Arc<AtomicBool>,
}

impl RuntimeCommandReceiver {
    pub fn recv(&self) -> Option<RuntimeCommand> {
        self.receiver.recv().ok()
    }

    pub fn try_recv(&self) -> Option<RuntimeCommand> {
        self.receiver.try_recv().ok()
    }

    pub fn take_emergency_stop(&self) -> bool {
        self.emergency_notice.swap(false, Ordering::AcqRel)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventDelivery {
    Sent,
    CoalescedProgress,
    DroppedProgress,
}

struct EventQueue {
    capacity: usize,
    queue: Mutex<VecDeque<RunEvent>>,
    available: Condvar,
    space: Condvar,
}

#[derive(Clone)]
pub struct RuntimeEventSender {
    queue: Arc<EventQueue>,
}

impl RuntimeEventSender {
    pub fn send(&self, event: RunEvent) -> EventDelivery {
        let mut queue = self
            .queue
            .queue
            .lock()
            .expect("runtime event queue poisoned");
        if event.is_progress() {
            if let Some(existing) = queue.back_mut().filter(|queued| {
                matches!(
                    (&**queued, &event),
                    (
                        RunEvent::ObservationProgress {
                            run_id: queued_run,
                            block_id: queued_block,
                            ..
                        },
                        RunEvent::ObservationProgress {
                            run_id: event_run,
                            block_id: event_block,
                            ..
                        }
                    ) if queued_run == event_run && queued_block == event_block
                )
            }) {
                *existing = event;
                return EventDelivery::CoalescedProgress;
            }
            if queue.len() >= self.queue.capacity {
                return EventDelivery::DroppedProgress;
            }
            queue.push_back(event);
            self.queue.available.notify_one();
            return EventDelivery::Sent;
        }

        while queue.len() >= self.queue.capacity {
            if let Some(progress) = queue.iter().position(RunEvent::is_progress) {
                queue.remove(progress);
                break;
            }
            queue = self
                .queue
                .space
                .wait(queue)
                .expect("runtime event queue poisoned while waiting");
        }
        queue.push_back(event);
        self.queue.available.notify_one();
        EventDelivery::Sent
    }
}

pub struct RuntimeEventReceiver {
    queue: Arc<EventQueue>,
}

impl RuntimeEventReceiver {
    pub fn recv(&self) -> RunEvent {
        let mut queue = self
            .queue
            .queue
            .lock()
            .expect("runtime event queue poisoned");
        loop {
            if let Some(event) = queue.pop_front() {
                self.queue.space.notify_one();
                return event;
            }
            queue = self
                .queue
                .available
                .wait(queue)
                .expect("runtime event queue poisoned while waiting");
        }
    }

    pub fn try_recv(&self) -> Option<RunEvent> {
        let event = self
            .queue
            .queue
            .lock()
            .expect("runtime event queue poisoned")
            .pop_front();
        if event.is_some() {
            self.queue.space.notify_one();
        }
        event
    }
}

pub struct RuntimeChannels {
    pub commands: RuntimeCommandSender,
    pub command_receiver: RuntimeCommandReceiver,
    pub events: RuntimeEventSender,
    pub event_receiver: RuntimeEventReceiver,
}

pub fn bounded_runtime_channels(command_capacity: usize, event_capacity: usize) -> RuntimeChannels {
    bounded_runtime_channels_with_signal(
        command_capacity,
        event_capacity,
        Arc::new(EmergencySignal::default()),
    )
}

fn bounded_runtime_channels_with_signal(
    command_capacity: usize,
    event_capacity: usize,
    emergency_stop: Arc<EmergencySignal>,
) -> RuntimeChannels {
    assert!(
        command_capacity > 0,
        "command channel capacity must be positive"
    );
    assert!(
        event_capacity > 0,
        "event channel capacity must be positive"
    );
    let (command_sender, command_receiver) = mpsc::sync_channel(command_capacity);
    let emergency_notice = Arc::new(AtomicBool::new(false));
    let event_queue = Arc::new(EventQueue {
        capacity: event_capacity,
        queue: Mutex::new(VecDeque::with_capacity(event_capacity)),
        available: Condvar::new(),
        space: Condvar::new(),
    });
    RuntimeChannels {
        commands: RuntimeCommandSender {
            sender: command_sender,
            emergency_stop: Arc::clone(&emergency_stop),
            emergency_notice: Arc::clone(&emergency_notice),
        },
        command_receiver: RuntimeCommandReceiver {
            receiver: command_receiver,
            emergency_notice,
        },
        events: RuntimeEventSender {
            queue: Arc::clone(&event_queue),
        },
        event_receiver: RuntimeEventReceiver { queue: event_queue },
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RunEvent {
    RunStarted {
        sequence: u64,
        elapsed_ms: u64,
        run_id: String,
        macro_id: String,
        revision: u64,
        definition_hash: String,
        mode: RunMode,
    },
    StatusChanged {
        sequence: u64,
        elapsed_ms: u64,
        run_id: String,
        status: RunStatus,
    },
    BlockEntered {
        sequence: u64,
        elapsed_ms: u64,
        run_id: String,
        block_id: String,
    },
    ActionPlanned {
        sequence: u64,
        elapsed_ms: u64,
        run_id: String,
        block_id: String,
        action: Action,
        state: ActionState,
        token: Option<super::ObservationToken>,
    },
    ActionBlocked {
        sequence: u64,
        elapsed_ms: u64,
        run_id: String,
        block_id: String,
        action: Action,
        state: ActionState,
        reason: String,
    },
    ObservationCompleted {
        sequence: u64,
        elapsed_ms: u64,
        run_id: String,
        block_id: String,
        evidence: DetectorEvidence,
        token: Option<ObservationToken>,
    },
    ConditionEvaluated {
        sequence: u64,
        elapsed_ms: u64,
        run_id: String,
        block_id: String,
        matched: bool,
    },
    ObservationProgress {
        sequence: u64,
        elapsed_ms: u64,
        run_id: String,
        block_id: String,
        attempts: u64,
    },
    LoopYielded {
        sequence: u64,
        elapsed_ms: u64,
        run_id: String,
        block_id: String,
        completed_iterations: u64,
    },
    Error {
        sequence: u64,
        elapsed_ms: u64,
        run_id: String,
        block_id: Option<String>,
        message: String,
    },
    RunStopped {
        sequence: u64,
        elapsed_ms: u64,
        run_id: String,
        status: RunStatus,
        reason: StopReason,
    },
}

impl RunEvent {
    pub fn sequence(&self) -> u64 {
        match self {
            Self::RunStarted { sequence, .. }
            | Self::StatusChanged { sequence, .. }
            | Self::BlockEntered { sequence, .. }
            | Self::ActionPlanned { sequence, .. }
            | Self::ActionBlocked { sequence, .. }
            | Self::ObservationCompleted { sequence, .. }
            | Self::ConditionEvaluated { sequence, .. }
            | Self::ObservationProgress { sequence, .. }
            | Self::LoopYielded { sequence, .. }
            | Self::Error { sequence, .. }
            | Self::RunStopped { sequence, .. } => *sequence,
        }
    }

    pub fn elapsed_ms(&self) -> u64 {
        match self {
            Self::RunStarted { elapsed_ms, .. }
            | Self::StatusChanged { elapsed_ms, .. }
            | Self::BlockEntered { elapsed_ms, .. }
            | Self::ActionPlanned { elapsed_ms, .. }
            | Self::ActionBlocked { elapsed_ms, .. }
            | Self::ObservationCompleted { elapsed_ms, .. }
            | Self::ConditionEvaluated { elapsed_ms, .. }
            | Self::ObservationProgress { elapsed_ms, .. }
            | Self::LoopYielded { elapsed_ms, .. }
            | Self::Error { elapsed_ms, .. }
            | Self::RunStopped { elapsed_ms, .. } => *elapsed_ms,
        }
    }

    fn is_progress(&self) -> bool {
        matches!(self, Self::ObservationProgress { .. })
    }
}

impl From<RunEvent> for JournalRecord {
    fn from(event: RunEvent) -> Self {
        let sequence = event.sequence();
        let elapsed_ms = event.elapsed_ms();
        let (kind, message) = match &event {
            RunEvent::RunStarted { .. } => (JournalKind::StateChange, "run started"),
            RunEvent::StatusChanged { .. } => (JournalKind::StateChange, "status changed"),
            RunEvent::BlockEntered { .. } => (JournalKind::StateChange, "block entered"),
            RunEvent::ActionPlanned { .. } => (JournalKind::Action, "action planned"),
            RunEvent::ActionBlocked { .. } => (JournalKind::Action, "action blocked"),
            RunEvent::ObservationCompleted { .. } => {
                (JournalKind::Candidate, "observation completed")
            }
            RunEvent::ConditionEvaluated { .. } => (JournalKind::Candidate, "condition evaluated"),
            RunEvent::ObservationProgress { .. } => {
                (JournalKind::Aggregate, "observation progress")
            }
            RunEvent::LoopYielded { .. } => (JournalKind::Aggregate, "loop yielded"),
            RunEvent::Error { .. } => (JournalKind::Error, "runtime error"),
            RunEvent::RunStopped { .. } => (JournalKind::StateChange, "run stopped"),
        };
        let fields = serde_json::to_value(&event).unwrap_or_else(
            |error| serde_json::json!({ "serialization_error": error.to_string() }),
        );
        Self {
            sequence,
            elapsed_ms,
            kind,
            message: message.to_string(),
            fields,
        }
    }
}

#[derive(Debug, Clone)]
pub struct CompiledMacro {
    definition: Arc<MacroDefinition>,
    pub definition_hash: String,
    pub pinned_assets: Arc<[PinnedAsset]>,
}

impl CompiledMacro {
    pub fn compile(saved: SavedRevision) -> Result<Self> {
        let problems = validate_macro(&saved.definition);
        if !problems.is_empty() {
            let summary = problems
                .iter()
                .map(|problem| format!("{}: {}", problem.code, problem.message))
                .collect::<Vec<_>>()
                .join("; ");
            bail!("saved macro revision is invalid: {summary}");
        }

        let definition_bytes = serde_json::to_vec_pretty(&saved.definition)?;
        let actual_definition_hash = sha256_hex(&definition_bytes);
        if actual_definition_hash != saved.definition_hash {
            bail!("saved definition hash does not match immutable revision bytes");
        }

        let referenced_assets: Vec<_> = referenced_assets(&saved.definition).collect();
        validate_asset_identities(referenced_assets.iter())?;
        let referenced: HashSet<_> = referenced_assets.into_iter().collect();
        let mut pinned = HashSet::new();
        validate_asset_identities(saved.pinned_assets.iter().map(|asset| &asset.asset))?;
        for asset in &saved.pinned_assets {
            if sha256_hex(&asset.bytes) != asset.asset.content_hash {
                bail!("pinned asset hash mismatch: {}", asset.asset.content_hash);
            }
            if !pinned.insert(asset.asset.clone()) {
                bail!("duplicate pinned asset identity");
            }
        }
        if referenced != pinned {
            bail!("saved revision does not pin exactly its referenced assets");
        }

        for rule in &saved.definition.image_rules {
            let template =
                decode_pinned_image_asset(&saved.pinned_assets, &rule.template, "template")?;
            let mask = rule
                .transparent_mask
                .as_ref()
                .map(|asset| decode_pinned_image_asset(&saved.pinned_assets, asset, "mask"))
                .transpose()?;
            super::image_verification::validate_decoded_rule(
                &saved.definition,
                rule,
                &template,
                mask.as_ref(),
            )?;
        }

        Ok(Self {
            definition: Arc::new(saved.definition),
            definition_hash: saved.definition_hash,
            pinned_assets: saved.pinned_assets.into(),
        })
    }

    pub fn definition(&self) -> &MacroDefinition {
        &self.definition
    }

    #[allow(clippy::too_many_arguments)]
    pub fn authorize_action(
        &self,
        run_id: &str,
        generation: u64,
        action_instance_id: u64,
        block_id: &str,
        action: &Action,
        token: Option<&ObservationToken>,
        resume: &ResumeAuthorization,
        destination: Point,
        maximum_observation_age_ms: u64,
    ) -> Result<ActionAuthorization> {
        let guard_target = &resume.target;
        let compiled_block = find_block_by_id(&self.definition.blocks, block_id)
            .ok_or_else(|| anyhow::anyhow!("action block '{block_id}' is not compiled"))?;
        let BlockKind::Action {
            action: compiled_action,
        } = &compiled_block.kind
        else {
            bail!("compiled block '{block_id}' is not an action");
        };
        anyhow::ensure!(
            compiled_action == action,
            "action does not match compiled block"
        );
        let button = match action {
            Action::ClickTextMatch { button, .. }
            | Action::ClickImageMatch { button, .. }
            | Action::ClickPoint { button, .. }
            | Action::ClickRegion { button, .. } => *button,
            Action::MoveOnly { .. } => bail!("MoveOnly does not authorize input dispatch"),
        };

        let screen_authorized_rect = match action {
            Action::ClickTextMatch { .. } | Action::ClickImageMatch { .. } => {
                let token = token.context("matched click requires an observation token")?;
                anyhow::ensure!(
                    token.is_current(run_id, generation),
                    "observation token is stale"
                );
                validate_action_token(self, action, token)?;
                let frame = token
                    .frame_metadata
                    .context("matched click token has no canonical frame metadata")?;
                validate_token_frame_consistency(token, guard_target)?;
                let local = token
                    .match_rect
                    .context("matched click token has no geometry")?;
                let captured_client = crate::engine::types::Rect::new(
                    frame.client_x,
                    frame.client_y,
                    frame.client_width,
                    frame.client_height,
                );
                Some(local_rect_to_screen(captured_client, local)?)
            }
            Action::ClickPoint { point_id, .. } => {
                anyhow::ensure!(
                    token.is_none(),
                    "point click cannot consume detector evidence"
                );
                let point = self
                    .definition
                    .points
                    .iter()
                    .find(|point| point.id == *point_id)
                    .ok_or_else(|| anyhow::anyhow!("compiled point '{point_id}' is missing"))?;
                anyhow::ensure!(
                    guard_target.client_rect.point_from_ratio(point.point) == destination,
                    "destination does not match compiled point"
                );
                None
            }
            Action::ClickRegion { region_id, .. } => {
                anyhow::ensure!(
                    token.is_none(),
                    "region click cannot consume detector evidence"
                );
                let region = self
                    .definition
                    .regions
                    .iter()
                    .find(|region| region.id == *region_id)
                    .ok_or_else(|| anyhow::anyhow!("compiled region '{region_id}' is missing"))?;
                Some(guard_target.client_rect.rect_from_ratio(region.rect))
            }
            Action::MoveOnly { .. } => unreachable!(),
        };
        if let Some(rect) = screen_authorized_rect {
            anyhow::ensure!(
                point_inside_rect(rect, destination),
                "destination is outside authorized geometry"
            );
        }
        anyhow::ensure!(
            point_inside_rect(guard_target.client_rect, destination),
            "destination is outside observation-time client bounds"
        );

        Ok(ActionAuthorization {
            attempt_id: ActionAttemptId {
                run_id: run_id.to_string(),
                action_instance_id,
            },
            block_id: block_id.to_string(),
            action: action.clone(),
            expected_target: guard_target.clone(),
            destination,
            button,
            observation: token.cloned(),
            screen_authorized_rect,
            generation,
            maximum_observation_age_ms,
        })
    }
}

fn find_block_by_id<'a>(blocks: &'a [Block], block_id: &str) -> Option<&'a Block> {
    for block in blocks {
        if block.id == block_id {
            return Some(block);
        }
        let nested = match &block.kind {
            BlockKind::If {
                then_body,
                else_body,
                ..
            } => find_block_by_id(then_body, block_id)
                .or_else(|| find_block_by_id(else_body, block_id)),
            BlockKind::RepeatN { body, .. }
            | BlockKind::RepeatUntil { body, .. }
            | BlockKind::Continuous { body } => find_block_by_id(body, block_id),
            BlockKind::WatchGroup { group } => group
                .lanes
                .iter()
                .find_map(|lane| find_block_by_id(&lane.then_body, block_id)),
            _ => None,
        };
        if nested.is_some() {
            return nested;
        }
    }
    None
}

fn validate_token_frame_consistency(
    token: &ObservationToken,
    target: &TargetSnapshot,
) -> Result<()> {
    let Some(frame) = token.frame_metadata else {
        return Ok(());
    };
    anyhow::ensure!(
        frame.frame_id == token.frame_id,
        "frame id is internally inconsistent"
    );
    anyhow::ensure!(
        frame.captured_at_ms == token.captured_at_ms,
        "frame capture time is internally inconsistent"
    );
    anyhow::ensure!(
        frame.window_id == target.window_id,
        "frame HWND does not match target"
    );
    anyhow::ensure!(
        frame.window_revision == target.window_revision,
        "frame window revision changed"
    );
    anyhow::ensure!(
        (
            frame.client_x,
            frame.client_y,
            frame.client_width,
            frame.client_height
        ) == (
            target.client_rect.x,
            target.client_rect.y,
            target.client_rect.width,
            target.client_rect.height
        ),
        "frame client geometry does not match observation target"
    );
    anyhow::ensure!(
        frame.geometry_revision == target.geometry_revision,
        "frame geometry revision changed"
    );
    anyhow::ensure!(
        frame.display_profile_revision == target.display_profile_revision,
        "frame display profile changed"
    );
    anyhow::ensure!(frame.dpi == target.dpi, "frame DPI changed");
    anyhow::ensure!(
        frame.region_revision == token.region_revision,
        "frame region revision is inconsistent"
    );
    anyhow::ensure!(
        frame.rule_revision == token.rule_revision,
        "frame rule revision is inconsistent"
    );
    Ok(())
}

fn local_rect_to_screen(
    client: crate::engine::types::Rect,
    local: crate::engine::types::Rect,
) -> Result<crate::engine::types::Rect> {
    let local_bounds = crate::engine::types::Rect::new(0, 0, client.width, client.height);
    anyhow::ensure!(
        rect_contains_checked(local_bounds, local),
        "local match geometry is outside client bounds"
    );
    let x = i64::from(client.x)
        .checked_add(i64::from(local.x))
        .and_then(|value| i32::try_from(value).ok())
        .context("screen match x overflowed")?;
    let y = i64::from(client.y)
        .checked_add(i64::from(local.y))
        .and_then(|value| i32::try_from(value).ok())
        .context("screen match y overflowed")?;
    Ok(crate::engine::types::Rect::new(
        x,
        y,
        local.width,
        local.height,
    ))
}

fn rect_contains_checked(
    container: crate::engine::types::Rect,
    nested: crate::engine::types::Rect,
) -> bool {
    let right = i64::from(container.x) + i64::from(container.width);
    let bottom = i64::from(container.y) + i64::from(container.height);
    let nested_right = i64::from(nested.x) + i64::from(nested.width);
    let nested_bottom = i64::from(nested.y) + i64::from(nested.height);
    i64::from(nested.x) >= i64::from(container.x)
        && i64::from(nested.y) >= i64::from(container.y)
        && nested_right <= right
        && nested_bottom <= bottom
}

fn decode_pinned_image_asset(
    pinned_assets: &[PinnedAsset],
    asset: &super::AssetRef,
    kind: &str,
) -> Result<image::GrayImage> {
    let pinned = pinned_assets
        .iter()
        .find(|pinned| pinned.asset == *asset)
        .ok_or_else(|| anyhow::anyhow!("compiled image {kind} asset is missing"))?;
    match kind {
        "mask" => super::ImageRuleVerification::decode_mask_png(&pinned.bytes),
        _ => super::ImageRuleVerification::decode_template_png(&pinned.bytes),
    }
    .map_err(|error| anyhow::anyhow!("compiled image {kind} asset cannot be decoded: {error}"))
}

fn referenced_assets(definition: &MacroDefinition) -> impl Iterator<Item = super::AssetRef> + '_ {
    definition.image_rules.iter().flat_map(|rule| {
        std::iter::once(rule.template.clone()).chain(rule.transparent_mask.clone())
    })
}

fn validate_asset_identities<'a>(
    assets: impl IntoIterator<Item = &'a super::AssetRef>,
) -> Result<()> {
    let mut hashes = HashMap::<(&str, u64), &str>::new();
    for asset in assets {
        let identity = (asset.id.as_str(), asset.revision);
        if hashes
            .insert(identity, asset.content_hash.as_str())
            .is_some_and(|existing| existing != asset.content_hash)
        {
            bail!(
                "conflicting hashes for immutable asset identity '{}@{}'",
                asset.id,
                asset.revision
            );
        }
    }
    Ok(())
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

#[derive(Debug, Default)]
struct ControlState {
    generation: u64,
    paused: bool,
    stop: Option<StopReason>,
}

#[derive(Clone)]
pub struct RuntimeControlHandle {
    control: Arc<Mutex<ControlState>>,
    emergency_stop: Arc<EmergencySignal>,
}

impl RuntimeControlHandle {
    pub fn pause(&self) {
        let mut control = self.control.lock().expect("runtime control poisoned");
        if !control.paused && control.stop.is_none() {
            control.paused = true;
            control.generation = control.generation.wrapping_add(1);
        }
        drop(control);
        self.emergency_stop.notify();
    }

    pub fn resume(&self) {
        let mut control = self.control.lock().expect("runtime control poisoned");
        if control.paused && control.stop.is_none() {
            control.paused = false;
            control.generation = control.generation.wrapping_add(1);
        }
        drop(control);
        self.emergency_stop.notify();
    }

    pub fn stop(&self) {
        self.set_stop(StopReason::UserStopped);
    }

    pub fn emergency_stop(&self) {
        self.emergency_stop.request();
        self.set_stop(StopReason::EmergencyStopped);
    }

    fn set_stop(&self, reason: StopReason) {
        let mut control = self.control.lock().expect("runtime control poisoned");
        if control.stop.is_none() {
            control.stop = Some(reason);
            control.generation = control.generation.wrapping_add(1);
        }
        drop(control);
        self.emergency_stop.notify();
    }

    pub fn generation(&self) -> u64 {
        self.control
            .lock()
            .expect("runtime control poisoned")
            .generation
    }
}

pub struct MacroRuntime {
    capture: Arc<dyn CaptureSource + Send + Sync>,
    detector: Arc<dyn ConditionDetector>,
    clock: Arc<dyn Clock + Send + Sync>,
    control: Arc<Mutex<ControlState>>,
    emergency_stop: Arc<EmergencySignal>,
    active: Arc<Mutex<bool>>,
    event_capacity: usize,
}

impl Clone for MacroRuntime {
    fn clone(&self) -> Self {
        Self {
            capture: Arc::clone(&self.capture),
            detector: Arc::clone(&self.detector),
            clock: Arc::clone(&self.clock),
            control: Arc::clone(&self.control),
            emergency_stop: Arc::clone(&self.emergency_stop),
            active: Arc::clone(&self.active),
            event_capacity: self.event_capacity,
        }
    }
}

impl MacroRuntime {
    pub fn new(
        capture: Arc<dyn CaptureSource + Send + Sync>,
        detector: Arc<dyn ConditionDetector>,
        clock: Arc<dyn Clock + Send + Sync>,
    ) -> Self {
        Self::with_event_capacity(capture, detector, clock, DEFAULT_SYNCHRONOUS_EVENT_CAPACITY)
    }

    pub fn with_event_capacity(
        capture: Arc<dyn CaptureSource + Send + Sync>,
        detector: Arc<dyn ConditionDetector>,
        clock: Arc<dyn Clock + Send + Sync>,
        event_capacity: usize,
    ) -> Self {
        assert!(
            event_capacity > FINAL_EVENT_RESERVE,
            "event capacity must leave room for final events"
        );
        Self {
            capture,
            detector,
            clock,
            control: Arc::new(Mutex::new(ControlState::default())),
            emergency_stop: Arc::new(EmergencySignal::default()),
            active: Arc::new(Mutex::new(false)),
            event_capacity,
        }
    }

    pub fn run(&self, saved: SavedRevision, mode: RunMode) -> Result<Vec<RunEvent>> {
        let _active =
            ActiveRunGuard::acquire_and_reset(&self.active, &self.emergency_stop, &self.control)?;
        let compiled = CompiledMacro::compile(saved)?;
        let started_at = self.clock.now_ms();
        let run_number = NEXT_RUN_ID.fetch_add(1, Ordering::Relaxed);
        let run_id = format!("{}-{}-{run_number}", compiled.definition().id, started_at);
        let mut emitter = EventEmitter::new(&*self.clock, started_at, run_id, self.event_capacity);
        emitter.run_started(&compiled, mode);
        emitter.status(RunStatus::Running);
        let blocks = compiled.definition().blocks.clone();
        let mut execution = RunExecution {
            runtime: self,
            compiled: &compiled,
            emitter: &mut emitter,
            observations: HashMap::new(),
            last_observation_at_ms: None,
            paused_event_emitted: false,
            detector_generations: HashSet::new(),
        };
        let reason = execution
            .execute_blocks(&blocks)
            .unwrap_or(StopReason::Completed);
        let mut detector_generations = execution
            .detector_generations
            .iter()
            .copied()
            .collect::<Vec<_>>();
        detector_generations.sort_unstable();
        drop(execution);
        if matches!(
            reason,
            StopReason::TechnicalFailure { .. } | StopReason::SafetyLimit { .. }
        ) {
            let message = match &reason {
                StopReason::TechnicalFailure { message } | StopReason::SafetyLimit { message } => {
                    message.clone()
                }
                _ => unreachable!(),
            };
            emitter.error(None, message);
        }
        emitter.status(RunStatus::Stopping);
        emitter.run_stopped(reason);
        self.detector
            .run_finished(emitter.run_id(), &detector_generations);
        Ok(emitter.events)
    }

    pub fn control_handle(&self) -> RuntimeControlHandle {
        RuntimeControlHandle {
            control: Arc::clone(&self.control),
            emergency_stop: Arc::clone(&self.emergency_stop),
        }
    }

    pub fn bounded_channels(
        &self,
        command_capacity: usize,
        event_capacity: usize,
    ) -> RuntimeChannels {
        bounded_runtime_channels_with_signal(
            command_capacity,
            event_capacity,
            Arc::clone(&self.emergency_stop),
        )
    }

    pub fn pause(&self) {
        self.control_handle().pause();
    }

    pub fn resume(&self) {
        self.control_handle().resume();
    }

    pub fn stop(&self) {
        self.control_handle().stop();
    }

    pub fn emergency_stop(&self) {
        self.control_handle().emergency_stop();
    }
}

struct ActiveRunGuard {
    active: Arc<Mutex<bool>>,
}

impl ActiveRunGuard {
    fn acquire_and_reset(
        active: &Arc<Mutex<bool>>,
        emergency_stop: &EmergencySignal,
        control: &Mutex<ControlState>,
    ) -> Result<Self> {
        let mut is_active = active.lock().expect("runtime active-run state poisoned");
        if *is_active {
            bail!("macro runtime is already active");
        }
        emergency_stop.reset();
        {
            let mut control = control.lock().expect("runtime control poisoned");
            control.generation = control.generation.wrapping_add(1);
            control.paused = false;
            control.stop = None;
        }
        *is_active = true;
        drop(is_active);
        Ok(Self {
            active: Arc::clone(active),
        })
    }
}

impl Drop for ActiveRunGuard {
    fn drop(&mut self) {
        *self
            .active
            .lock()
            .expect("runtime active-run state poisoned") = false;
    }
}

struct RunExecution<'a, 'clock> {
    runtime: &'a MacroRuntime,
    compiled: &'a CompiledMacro,
    emitter: &'a mut EventEmitter<'clock>,
    observations: HashMap<String, ObservationToken>,
    last_observation_at_ms: Option<u64>,
    paused_event_emitted: bool,
    detector_generations: HashSet<u64>,
}

impl RunExecution<'_, '_> {
    fn execute_blocks(&mut self, blocks: &[Block]) -> Option<StopReason> {
        for block in blocks {
            if let Some(reason) = self.check_control() {
                return Some(reason);
            }
            if !block.enabled {
                continue;
            }
            self.emitter.block_entered(&block.id);
            if let Some(reason) = self.execute_block(block) {
                return Some(reason);
            }
            if let Some(reason) = self.check_control() {
                return Some(reason);
            }
        }
        None
    }

    fn execute_block(&mut self, block: &Block) -> Option<StopReason> {
        match &block.kind {
            BlockKind::Observe { condition } => {
                let outcome = self.evaluate_condition(&block.id, condition)?;
                self.execute_timeout_body(outcome.timeout_body.as_deref())
            }
            BlockKind::Action { action } => self.plan_action(&block.id, action),
            BlockKind::If {
                condition,
                then_body,
                else_body,
            } => {
                let outcome = self.evaluate_condition(&block.id, condition)?;
                if let Some(reason) = self.execute_timeout_body(outcome.timeout_body.as_deref()) {
                    return Some(reason);
                }
                let selected = match if_once_decision(outcome.matched) {
                    super::BranchDecision::Then => then_body,
                    super::BranchDecision::Else => else_body,
                };
                self.execute_blocks(selected)
            }
            BlockKind::Wait { duration_ms } => self.cooperative_wait(*duration_ms),
            BlockKind::RepeatN { count, body } => {
                let mut completed = 0_u32;
                while matches!(
                    repeat_n_decision(*count, completed),
                    super::LoopDecision::EnterBody
                ) {
                    if let Some(reason) = self.execute_blocks(body) {
                        return Some(reason);
                    }
                    completed += 1;
                    self.emitter.loop_yielded(&block.id, u64::from(completed));
                    if let Some(reason) = self.cooperative_wait(1) {
                        return Some(reason);
                    }
                }
                None
            }
            BlockKind::RepeatUntil {
                condition,
                max_iterations,
                body,
            } => {
                let mut completed = 0_u64;
                loop {
                    let outcome = self.evaluate_condition(&block.id, condition)?;
                    if let Some(reason) = self.execute_timeout_body(outcome.timeout_body.as_deref())
                    {
                        return Some(reason);
                    }
                    match super::evaluate_repeat_until_before_body(
                        outcome.matched,
                        completed,
                        max_iterations.clone(),
                    ) {
                        super::LoopDecision::EnterBody => {}
                        super::LoopDecision::ExitConditionMet
                        | super::LoopDecision::ExitCountMet => return None,
                    }
                    if let Some(reason) = self.execute_blocks(body) {
                        return Some(reason);
                    }
                    completed = completed.saturating_add(1);
                    self.emitter.loop_yielded(&block.id, completed);
                    if let Some(reason) = self.cooperative_wait(1) {
                        return Some(reason);
                    }
                }
            }
            BlockKind::Continuous { body } => {
                let mut completed = 0_u64;
                loop {
                    if let Some(reason) = self.execute_blocks(body) {
                        return Some(reason);
                    }
                    completed = completed.saturating_add(1);
                    self.emitter.loop_yielded(&block.id, completed);
                    if let Some(reason) = self.cooperative_wait(1) {
                        return Some(reason);
                    }
                }
            }
            BlockKind::WatchGroup { .. } => Some(StopReason::UnsupportedBlock {
                block_id: block.id.clone(),
            }),
            BlockKind::StopSuccess => Some(StopReason::StopSuccess),
            BlockKind::StopError { message } => Some(StopReason::StopError {
                message: message.clone(),
            }),
            BlockKind::Comment { .. } => None,
        }
    }

    fn execute_timeout_body(&mut self, body: Option<&[Block]>) -> Option<StopReason> {
        body.and_then(|body| self.execute_blocks(body))
    }

    fn evaluate_condition(
        &mut self,
        owner_block_id: &str,
        condition: &Condition,
    ) -> Option<ConditionOutcome> {
        let mode = condition_mode(condition);
        let started = self.runtime.clock.now_ms();
        let mut attempts = 0_u64;
        let mut last_matched = false;
        loop {
            if let Some(reason) = self.check_control() {
                return self.stop_condition(reason);
            }
            if attempts > 0 && timeout_reached(mode, started, self.runtime.clock.now_ms()) {
                return self.resolve_condition_timeout(mode, last_matched);
            }
            attempts = attempts.saturating_add(1);
            if exceeds_limit(
                attempts.saturating_sub(1),
                &self.compiled.definition().safety.max_observation_retries,
            ) {
                return self.stop_condition(StopReason::SafetyLimit {
                    message: "maximum observation retries exceeded".to_string(),
                });
            }

            if let Some(reason) = self.pace_observation() {
                return self.stop_condition(reason);
            }
            if attempts > 1 && timeout_reached(mode, started, self.runtime.clock.now_ms()) {
                return self.resolve_condition_timeout(mode, last_matched);
            }
            let generation = self.runtime.control_handle().generation();
            self.detector_generations.insert(generation);
            let observed_at_ms = self.runtime.clock.now_ms();
            self.last_observation_at_ms = Some(observed_at_ms);
            let request = ObservationRequest {
                run_id: self.emitter.run_id(),
                generation,
                condition,
                compiled: self.compiled,
                observed_at_ms,
            };
            let observation = self
                .runtime
                .detector
                .observe(&request, &*self.runtime.capture);
            if let Some(reason) = self.check_control() {
                return self.stop_condition(reason);
            }
            if self.runtime.control_handle().generation() != generation {
                self.emitter.observation_progress(owner_block_id, attempts);
                continue;
            }
            let evidence = match observation {
                Ok(evidence) => evidence,
                Err(error) => {
                    return self.stop_condition(StopReason::TechnicalFailure {
                        message: format!("detector failed for block '{owner_block_id}': {error}"),
                    });
                }
            };
            last_matched = evidence.matched;
            let token = if evidence.matched {
                match self.make_token(condition, &evidence, generation) {
                    Ok(token) => Some(token),
                    Err(message) => {
                        return self.stop_condition(StopReason::TechnicalFailure { message });
                    }
                }
            } else {
                None
            };
            let source_block_id = condition_source_id(condition);
            if let Some(token) = &token {
                self.observations
                    .insert(source_block_id.to_string(), token.clone());
            } else {
                self.observations.remove(source_block_id);
            }
            self.emitter
                .observation_completed(owner_block_id, evidence.clone(), token);
            self.emitter
                .condition_evaluated(owner_block_id, evidence.matched);

            if observation_satisfies_mode(mode, evidence.matched) {
                return Some(ConditionOutcome {
                    matched: evidence.matched,
                    timeout_body: None,
                });
            }
            if matches!(mode, ObserveMode::CheckNow) {
                return Some(ConditionOutcome {
                    matched: false,
                    timeout_body: None,
                });
            }
            if timeout_reached(mode, started, self.runtime.clock.now_ms()) {
                return self.resolve_condition_timeout(mode, last_matched);
            }
            self.emitter.observation_progress(owner_block_id, attempts);
            let wait_ms = remaining_condition_time_ms(mode, started, self.runtime.clock.now_ms())
                .map_or_else(
                    || self.poll_interval_ms(condition),
                    |remaining| self.poll_interval_ms(condition).min(remaining),
                );
            if let Some(reason) = self.cooperative_wait(wait_ms) {
                return self.stop_condition(reason);
            }
        }
    }

    fn resolve_condition_timeout(
        &mut self,
        mode: &ObserveMode,
        last_matched: bool,
    ) -> Option<ConditionOutcome> {
        match timeout_outcome(mode) {
            TimeoutOutcome::Continue => Some(ConditionOutcome {
                matched: last_matched,
                timeout_body: None,
            }),
            TimeoutOutcome::RunBody { body } => Some(ConditionOutcome {
                matched: last_matched,
                timeout_body: Some(body.clone()),
            }),
            TimeoutOutcome::StopError { message } => self.stop_condition(StopReason::StopError {
                message: message.clone(),
            }),
        }
    }

    fn stop_condition(&mut self, reason: StopReason) -> Option<ConditionOutcome> {
        self.runtime.control_handle().set_stop(reason);
        None
    }

    fn pace_observation(&mut self) -> Option<StopReason> {
        let last_observation = self.last_observation_at_ms?;
        let minimum_interval = minimum_observation_interval_ms(
            self.compiled
                .definition()
                .safety
                .max_observations_per_second,
        );
        let earliest = last_observation.saturating_add(minimum_interval);
        let now = self.runtime.clock.now_ms();
        if now < earliest {
            self.cooperative_wait(earliest - now)
        } else {
            None
        }
    }

    fn make_token(
        &self,
        condition: &Condition,
        evidence: &DetectorEvidence,
        generation: u64,
    ) -> std::result::Result<ObservationToken, String> {
        let (detector, source_block_id, rule_id, rule_revision, region_id) = match condition {
            Condition::Text {
                source_block_id,
                rule_id,
                ..
            } => {
                let rule = self
                    .compiled
                    .definition()
                    .text_rules
                    .iter()
                    .find(|rule| &rule.id == rule_id)
                    .ok_or_else(|| format!("compiled text rule '{rule_id}' is missing"))?;
                (
                    DetectorKind::Text,
                    source_block_id,
                    rule_id,
                    rule.revision,
                    &rule.region_id,
                )
            }
            Condition::Image {
                source_block_id,
                rule_id,
                ..
            } => {
                let rule = self
                    .compiled
                    .definition()
                    .image_rules
                    .iter()
                    .find(|rule| &rule.id == rule_id)
                    .ok_or_else(|| format!("compiled image rule '{rule_id}' is missing"))?;
                (
                    DetectorKind::Image,
                    source_block_id,
                    rule_id,
                    rule.revision,
                    &rule.region_id,
                )
            }
        };
        let region = self
            .compiled
            .definition()
            .regions
            .iter()
            .find(|region| &region.id == region_id)
            .ok_or_else(|| format!("compiled region '{region_id}' is missing"))?;
        Ok(ObservationToken {
            run_id: self.emitter.run_id().to_string(),
            generation,
            source_block_id: source_block_id.clone(),
            detector,
            region_id: region.id.clone(),
            region_revision: region.revision,
            rule_id: rule_id.clone(),
            rule_revision,
            frame_id: evidence.frame_id,
            captured_at_ms: evidence.captured_at_ms,
            match_rect: evidence.match_rect,
            score: evidence.score,
            match_count: evidence.match_count,
            stable_frames: evidence.stable_frames,
            frame_metadata: evidence.frame_metadata,
            evidence: evidence.details.clone(),
        })
    }

    fn poll_interval_ms(&self, condition: &Condition) -> u64 {
        match condition {
            Condition::Text { rule_id, .. } => self
                .compiled
                .definition()
                .text_rules
                .iter()
                .find(|rule| &rule.id == rule_id)
                .map_or(1, |rule| rule.poll_interval_ms),
            Condition::Image { rule_id, .. } => self
                .compiled
                .definition()
                .image_rules
                .iter()
                .find(|rule| &rule.id == rule_id)
                .map_or(1, |rule| rule.poll_interval_ms),
        }
        .max(1)
    }

    fn plan_action(&mut self, block_id: &str, action: &Action) -> Option<StopReason> {
        let token = match action_source(action) {
            Some(source_block_id) => {
                let generation = self.runtime.control_handle().generation();
                let Some(token) = self.observations.get(source_block_id).cloned() else {
                    let reason =
                        format!("action requires a fresh observation from '{source_block_id}'");
                    self.emitter
                        .action_blocked(block_id, action.clone(), reason.clone());
                    return Some(StopReason::TechnicalFailure { message: reason });
                };
                if !token.is_current(self.emitter.run_id(), generation) {
                    let reason = format!("observation from '{source_block_id}' is stale");
                    self.emitter
                        .action_blocked(block_id, action.clone(), reason.clone());
                    return Some(StopReason::TechnicalFailure { message: reason });
                }
                if let Err(error) = validate_action_token(self.compiled, action, &token) {
                    let reason = error.to_string();
                    self.emitter
                        .action_blocked(block_id, action.clone(), reason.clone());
                    return Some(StopReason::TechnicalFailure { message: reason });
                }
                Some(token)
            }
            None => None,
        };
        // Planning is observation-only. The live `ActionCommitter` owns and consumes the
        // authoritative click budget exactly at the pre-SendInput linearization boundary.
        self.emitter.action_planned(block_id, action.clone(), token);
        self.cooperative_wait(self.compiled.definition().safety.minimum_click_interval_ms)
    }

    fn cooperative_wait(&mut self, duration_ms: u64) -> Option<StopReason> {
        let deadline = self.runtime.clock.now_ms().saturating_add(duration_ms);
        loop {
            if let Some(reason) = self.check_control() {
                return Some(reason);
            }
            let now = self.runtime.clock.now_ms();
            if now >= deadline {
                return None;
            }
            let slice = deadline.saturating_sub(now).clamp(1, 10);
            self.runtime
                .emergency_stop
                .wait(Duration::from_millis(slice));
        }
    }

    fn check_control(&mut self) -> Option<StopReason> {
        if self.emitter.capacity_exhausted() {
            return Some(StopReason::SafetyLimit {
                message: "synchronous event capacity reached".to_string(),
            });
        }
        if self.runtime.emergency_stop.requested() {
            return Some(StopReason::EmergencyStopped);
        }
        loop {
            if self.runtime.emergency_stop.requested() {
                return Some(StopReason::EmergencyStopped);
            }
            let (paused, stop) = {
                let control = self
                    .runtime
                    .control
                    .lock()
                    .expect("runtime control poisoned");
                (control.paused, control.stop.clone())
            };
            if let Some(reason) = stop {
                return Some(reason);
            }
            if exceeds_limit(
                self.emitter.elapsed_now(),
                &self.compiled.definition().safety.max_runtime_ms,
            ) {
                return Some(StopReason::SafetyLimit {
                    message: "maximum runtime exceeded".to_string(),
                });
            }
            if !paused {
                if self.paused_event_emitted {
                    self.emitter.status(RunStatus::Running);
                    self.paused_event_emitted = false;
                }
                break;
            }
            if !self.paused_event_emitted {
                self.emitter.status(RunStatus::Paused);
                self.observations.clear();
                self.paused_event_emitted = true;
            }
            self.runtime.emergency_stop.wait(Duration::from_millis(10));
        }

        None
    }
}

struct ConditionOutcome {
    matched: bool,
    timeout_body: Option<Vec<Block>>,
}

fn condition_mode(condition: &Condition) -> &ObserveMode {
    match condition {
        Condition::Text { mode, .. } | Condition::Image { mode, .. } => mode,
    }
}

fn condition_source_id(condition: &Condition) -> &str {
    match condition {
        Condition::Text {
            source_block_id, ..
        }
        | Condition::Image {
            source_block_id, ..
        } => source_block_id,
    }
}

fn timeout_outcome(mode: &ObserveMode) -> &TimeoutOutcome {
    match mode {
        ObserveMode::WaitForTrue {
            timeout_outcome, ..
        }
        | ObserveMode::WaitForFalse {
            timeout_outcome, ..
        } => timeout_outcome,
        ObserveMode::CheckNow => unreachable!("CheckNow has no timeout outcome"),
    }
}

fn timeout_reached(mode: &ObserveMode, started_at: u64, now: u64) -> bool {
    let elapsed = now.saturating_sub(started_at);
    match mode {
        ObserveMode::CheckNow => false,
        ObserveMode::WaitForTrue { timeout_ms, .. }
        | ObserveMode::WaitForFalse { timeout_ms, .. } => {
            matches!(timeout_ms, Limit::Finite(limit) if elapsed >= *limit)
        }
    }
}

fn remaining_condition_time_ms(mode: &ObserveMode, started_at: u64, now: u64) -> Option<u64> {
    match mode {
        ObserveMode::WaitForTrue {
            timeout_ms: Limit::Finite(limit),
            ..
        }
        | ObserveMode::WaitForFalse {
            timeout_ms: Limit::Finite(limit),
            ..
        } => Some(limit.saturating_sub(now.saturating_sub(started_at))),
        ObserveMode::CheckNow
        | ObserveMode::WaitForTrue {
            timeout_ms: Limit::Unlimited,
            ..
        }
        | ObserveMode::WaitForFalse {
            timeout_ms: Limit::Unlimited,
            ..
        } => None,
    }
}

fn exceeds_limit(value: u64, limit: &Limit<u64>) -> bool {
    matches!(limit, Limit::Finite(maximum) if value > *maximum)
}

fn minimum_observation_interval_ms(max_observations_per_second: u32) -> u64 {
    1_000_u64.div_ceil(u64::from(max_observations_per_second.max(1)))
}

fn action_source(action: &Action) -> Option<&str> {
    match action {
        Action::ClickTextMatch {
            source_block_id, ..
        }
        | Action::ClickImageMatch {
            source_block_id, ..
        }
        | Action::MoveOnly {
            target: super::ActionTarget::TextMatch { source_block_id },
        }
        | Action::MoveOnly {
            target: super::ActionTarget::ImageMatch { source_block_id },
        } => Some(source_block_id),
        Action::ClickPoint { .. }
        | Action::ClickRegion { .. }
        | Action::MoveOnly {
            target: super::ActionTarget::Point { .. } | super::ActionTarget::Region { .. },
        } => None,
    }
}

fn validate_action_token(
    compiled: &CompiledMacro,
    action: &Action,
    token: &ObservationToken,
) -> Result<()> {
    let source_block_id = action_source(action)
        .ok_or_else(|| anyhow::anyhow!("action does not use observation evidence"))?;
    if token.source_block_id != source_block_id {
        bail!("observation token source does not match action source '{source_block_id}'");
    }

    let expected_detector = match action {
        Action::ClickTextMatch { .. }
        | Action::MoveOnly {
            target: super::ActionTarget::TextMatch { .. },
        } => DetectorKind::Text,
        Action::ClickImageMatch { .. }
        | Action::MoveOnly {
            target: super::ActionTarget::ImageMatch { .. },
        } => DetectorKind::Image,
        _ => bail!("action does not use observation evidence"),
    };
    if token.detector != expected_detector {
        bail!("observation token detector does not match the action detector");
    }

    let definition = compiled.definition();
    let rule_region_id = match expected_detector {
        DetectorKind::Text => {
            let rule = definition
                .text_rules
                .iter()
                .find(|rule| rule.id == token.rule_id && rule.revision == token.rule_revision)
                .ok_or_else(|| {
                    anyhow::anyhow!("observation token text rule identity is not compiled")
                })?;
            &rule.region_id
        }
        DetectorKind::Image => {
            let rule = definition
                .image_rules
                .iter()
                .find(|rule| rule.id == token.rule_id && rule.revision == token.rule_revision)
                .ok_or_else(|| {
                    anyhow::anyhow!("observation token image rule identity is not compiled")
                })?;
            &rule.region_id
        }
    };
    let region = definition
        .regions
        .iter()
        .find(|region| region.id == token.region_id && region.revision == token.region_revision)
        .ok_or_else(|| anyhow::anyhow!("observation token region identity is not compiled"))?;
    if &region.id != rule_region_id {
        bail!("observation token rule and region identities do not agree");
    }
    if !blocks_contain_token_source(
        &definition.blocks,
        source_block_id,
        expected_detector,
        &token.rule_id,
    ) {
        bail!("observation token rule is not bound to source '{source_block_id}'");
    }
    Ok(())
}

fn blocks_contain_token_source(
    blocks: &[Block],
    source_block_id: &str,
    detector: DetectorKind,
    rule_id: &str,
) -> bool {
    blocks.iter().any(|block| match &block.kind {
        BlockKind::Observe { condition } => {
            condition_or_timeout_matches_token_source(condition, source_block_id, detector, rule_id)
        }
        BlockKind::If {
            condition,
            then_body,
            else_body,
        } => {
            condition_or_timeout_matches_token_source(condition, source_block_id, detector, rule_id)
                || blocks_contain_token_source(then_body, source_block_id, detector, rule_id)
                || blocks_contain_token_source(else_body, source_block_id, detector, rule_id)
        }
        BlockKind::RepeatUntil {
            condition, body, ..
        } => {
            condition_or_timeout_matches_token_source(condition, source_block_id, detector, rule_id)
                || blocks_contain_token_source(body, source_block_id, detector, rule_id)
        }
        BlockKind::RepeatN { body, .. } | BlockKind::Continuous { body } => {
            blocks_contain_token_source(body, source_block_id, detector, rule_id)
        }
        BlockKind::WatchGroup { group } => group.lanes.iter().any(|lane| {
            passive_condition_matches_token_source(
                &lane.condition,
                source_block_id,
                detector,
                rule_id,
            ) || blocks_contain_token_source(&lane.then_body, source_block_id, detector, rule_id)
        }),
        _ => false,
    })
}

fn condition_or_timeout_matches_token_source(
    condition: &Condition,
    source_block_id: &str,
    detector: DetectorKind,
    rule_id: &str,
) -> bool {
    condition_matches_token_source(condition, source_block_id, detector, rule_id)
        || timeout_body(condition).is_some_and(|body| {
            blocks_contain_token_source(body, source_block_id, detector, rule_id)
        })
}

fn condition_matches_token_source(
    condition: &Condition,
    source_block_id: &str,
    detector: DetectorKind,
    rule_id: &str,
) -> bool {
    match condition {
        Condition::Text {
            source_block_id: source,
            rule_id: rule,
            ..
        } => source == source_block_id && rule == rule_id && detector == DetectorKind::Text,
        Condition::Image {
            source_block_id: source,
            rule_id: rule,
            ..
        } => source == source_block_id && rule == rule_id && detector == DetectorKind::Image,
    }
}

fn passive_condition_matches_token_source(
    condition: &super::PassiveCondition,
    source_block_id: &str,
    detector: DetectorKind,
    rule_id: &str,
) -> bool {
    match condition {
        super::PassiveCondition::Text {
            source_block_id: source,
            rule_id: rule,
        } => source == source_block_id && rule == rule_id && detector == DetectorKind::Text,
        super::PassiveCondition::Image {
            source_block_id: source,
            rule_id: rule,
        } => source == source_block_id && rule == rule_id && detector == DetectorKind::Image,
    }
}

fn timeout_body(condition: &Condition) -> Option<&[Block]> {
    match condition_mode(condition) {
        ObserveMode::WaitForTrue {
            timeout_outcome: TimeoutOutcome::RunBody { body },
            ..
        }
        | ObserveMode::WaitForFalse {
            timeout_outcome: TimeoutOutcome::RunBody { body },
            ..
        } => Some(body),
        _ => None,
    }
}

struct EventEmitter<'a> {
    clock: &'a dyn Clock,
    started_at: u64,
    last_elapsed_ms: u64,
    next_sequence: u64,
    run_id: String,
    events: Vec<RunEvent>,
    capacity: usize,
    capacity_exhausted: bool,
}

impl<'a> EventEmitter<'a> {
    fn new(clock: &'a dyn Clock, started_at: u64, run_id: String, capacity: usize) -> Self {
        Self {
            clock,
            started_at,
            last_elapsed_ms: 0,
            next_sequence: 1,
            run_id,
            events: Vec::with_capacity(capacity),
            capacity,
            capacity_exhausted: false,
        }
    }

    fn push_critical(&mut self, event: RunEvent) {
        assert!(
            self.events.len() < self.capacity,
            "critical event reserve invariant violated"
        );
        self.events.push(event);
        if self.events.len() >= self.capacity - FINAL_EVENT_RESERVE {
            self.capacity_exhausted = true;
        }
    }

    fn capacity_exhausted(&self) -> bool {
        self.capacity_exhausted
    }

    fn metadata(&mut self) -> (u64, u64, String) {
        let sequence = self.next_sequence;
        self.next_sequence += 1;
        let elapsed = self.clock.now_ms().saturating_sub(self.started_at);
        self.last_elapsed_ms = self.last_elapsed_ms.max(elapsed);
        (sequence, self.last_elapsed_ms, self.run_id.clone())
    }

    fn run_id(&self) -> &str {
        &self.run_id
    }

    fn elapsed_now(&mut self) -> u64 {
        let elapsed = self.clock.now_ms().saturating_sub(self.started_at);
        self.last_elapsed_ms = self.last_elapsed_ms.max(elapsed);
        self.last_elapsed_ms
    }

    fn run_started(&mut self, compiled: &CompiledMacro, mode: RunMode) {
        let (sequence, elapsed_ms, run_id) = self.metadata();
        self.push_critical(RunEvent::RunStarted {
            sequence,
            elapsed_ms,
            run_id,
            macro_id: compiled.definition().id.clone(),
            revision: compiled.definition().revision,
            definition_hash: compiled.definition_hash.clone(),
            mode,
        });
    }

    fn status(&mut self, status: RunStatus) {
        let (sequence, elapsed_ms, run_id) = self.metadata();
        self.push_critical(RunEvent::StatusChanged {
            sequence,
            elapsed_ms,
            run_id,
            status,
        });
    }

    fn block_entered(&mut self, block_id: &str) {
        let (sequence, elapsed_ms, run_id) = self.metadata();
        self.push_critical(RunEvent::BlockEntered {
            sequence,
            elapsed_ms,
            run_id,
            block_id: block_id.to_string(),
        });
    }

    fn action_planned(&mut self, block_id: &str, action: Action, token: Option<ObservationToken>) {
        let (sequence, elapsed_ms, run_id) = self.metadata();
        self.push_critical(RunEvent::ActionPlanned {
            sequence,
            elapsed_ms,
            run_id,
            block_id: block_id.to_string(),
            action,
            state: ActionState::Planned,
            token,
        });
    }

    fn action_blocked(&mut self, block_id: &str, action: Action, reason: String) {
        let (sequence, elapsed_ms, run_id) = self.metadata();
        self.push_critical(RunEvent::ActionBlocked {
            sequence,
            elapsed_ms,
            run_id,
            block_id: block_id.to_string(),
            action,
            state: ActionState::Blocked,
            reason,
        });
    }

    fn observation_completed(
        &mut self,
        block_id: &str,
        evidence: DetectorEvidence,
        token: Option<ObservationToken>,
    ) {
        let (sequence, elapsed_ms, run_id) = self.metadata();
        self.push_critical(RunEvent::ObservationCompleted {
            sequence,
            elapsed_ms,
            run_id,
            block_id: block_id.to_string(),
            evidence,
            token,
        });
    }

    fn condition_evaluated(&mut self, block_id: &str, matched: bool) {
        let (sequence, elapsed_ms, run_id) = self.metadata();
        self.push_critical(RunEvent::ConditionEvaluated {
            sequence,
            elapsed_ms,
            run_id,
            block_id: block_id.to_string(),
            matched,
        });
    }

    fn observation_progress(&mut self, block_id: &str, attempts: u64) {
        let (sequence, elapsed_ms, run_id) = self.metadata();
        let event = RunEvent::ObservationProgress {
            sequence,
            elapsed_ms,
            run_id,
            block_id: block_id.to_string(),
            attempts,
        };
        if let Some(last) = self.events.last_mut().filter(|last| {
            matches!(
                (&**last, &event),
                (
                    RunEvent::ObservationProgress {
                        run_id: previous_run,
                        block_id: previous_block,
                        ..
                    },
                    RunEvent::ObservationProgress {
                        run_id: current_run,
                        block_id: current_block,
                        ..
                    }
                ) if previous_run == current_run && previous_block == current_block
            )
        }) {
            *last = event;
        } else if self.events.len() + 1 < self.capacity - FINAL_EVENT_RESERVE {
            self.events.push(event);
        }
    }

    fn loop_yielded(&mut self, block_id: &str, completed_iterations: u64) {
        let (sequence, elapsed_ms, run_id) = self.metadata();
        self.push_critical(RunEvent::LoopYielded {
            sequence,
            elapsed_ms,
            run_id,
            block_id: block_id.to_string(),
            completed_iterations,
        });
    }

    fn error(&mut self, block_id: Option<String>, message: String) {
        let (sequence, elapsed_ms, run_id) = self.metadata();
        self.push_critical(RunEvent::Error {
            sequence,
            elapsed_ms,
            run_id,
            block_id,
            message,
        });
    }

    fn run_stopped(&mut self, reason: StopReason) {
        let (sequence, elapsed_ms, run_id) = self.metadata();
        self.push_critical(RunEvent::RunStopped {
            sequence,
            elapsed_ms,
            run_id,
            status: RunStatus::Stopped,
            reason,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::automation::SystemClock;
    use crate::engine::{
        macro_engine::{
            AssetRef, DEFAULT_MAX_SCORE_CELLS, FocusLossPolicy, IMAGE_RULE_VERIFICATION_VERSION,
            ImageRule, ImageRuleVerification, ImageRuleVerificationArtifact,
            ImageRuleVerificationInput, ImageVerificationPreprocess, Limit, MACRO_SCHEMA_VERSION,
            MatchSelectionPolicy, NegativeCorpusSample, NegativeSampleEvaluationInputs,
            ObserveMode, PointDefinition, PreprocessProfile, RegionDefinition, SafetyPolicy,
            TargetProfile, TextMatchMode, TextRule, TimeoutOutcome,
        },
        types::{PointRatio, Rect, RectRatio, ScreenImage},
    };
    use image::{DynamicImage, GrayImage, ImageFormat, Luma};
    use std::{collections::VecDeque, io::Cursor, thread};

    const NEGATIVE_CORPUS_SHA256: &str =
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    #[derive(Debug, Default)]
    struct FakeCapture;

    impl CaptureSource for FakeCapture {
        fn capture(&self, _rect: Rect) -> Result<ScreenImage> {
            bail!("capture should not be called")
        }
    }

    #[derive(Debug, Default)]
    struct FakeDetector(Mutex<VecDeque<bool>>);

    impl FakeDetector {
        fn returning(values: impl IntoIterator<Item = bool>) -> Self {
            Self(Mutex::new(values.into_iter().collect()))
        }
    }

    impl ConditionDetector for FakeDetector {
        fn observe(
            &self,
            request: &super::super::ObservationRequest<'_>,
            _capture: &(dyn CaptureSource + Send + Sync),
        ) -> Result<super::super::DetectorEvidence> {
            let matched = self.0.lock().unwrap().pop_front().unwrap_or(false);
            Ok(super::super::DetectorEvidence {
                matched,
                frame_id: request.observed_at_ms,
                captured_at_ms: request.observed_at_ms,
                match_rect: matched.then(|| Rect::new(10, 20, 30, 40)),
                score: matched.then_some(0.99),
                match_count: u32::from(matched),
                stable_frames: u8::from(matched),
                frame_metadata: None,
                details: serde_json::json!({ "fixture": true }),
            })
        }
    }

    #[derive(Debug, Default)]
    struct MetadataDetector;

    impl ConditionDetector for MetadataDetector {
        fn observe(
            &self,
            request: &super::super::ObservationRequest<'_>,
            _capture: &(dyn CaptureSource + Send + Sync),
        ) -> Result<super::super::DetectorEvidence> {
            Ok(super::super::DetectorEvidence {
                matched: true,
                frame_id: 7,
                captured_at_ms: request.observed_at_ms,
                match_rect: Some(Rect::new(10, 20, 30, 40)),
                score: Some(0.99),
                match_count: 1,
                stable_frames: 2,
                frame_metadata: Some(super::super::ImageFrameMetadata {
                    frame_id: 7,
                    captured_at_ms: request.observed_at_ms,
                    window_id: 9,
                    window_revision: 2,
                    client_x: 0,
                    client_y: 0,
                    client_width: 64,
                    client_height: 48,
                    geometry_revision: 3,
                    display_profile_revision: 4,
                    dpi: 96,
                    region_revision: 1,
                    rule_revision: 1,
                }),
                details: serde_json::Value::Null,
            })
        }
    }

    #[derive(Debug, Default)]
    struct FakeClock(AtomicU64);

    impl Clock for FakeClock {
        fn now_ms(&self) -> u64 {
            self.0.fetch_add(1, Ordering::Relaxed)
        }
    }

    #[derive(Debug, Default)]
    struct FrozenClock;

    impl Clock for FrozenClock {
        fn now_ms(&self) -> u64 {
            0
        }
    }

    #[derive(Debug, Default)]
    struct StepClock(AtomicU64);

    impl Clock for StepClock {
        fn now_ms(&self) -> u64 {
            self.0.fetch_add(100, Ordering::Relaxed)
        }
    }

    #[derive(Debug, Default)]
    struct ManualClock(AtomicU64);

    impl ManualClock {
        fn set(&self, now_ms: u64) {
            self.0.store(now_ms, Ordering::Relaxed);
        }
    }

    impl Clock for ManualClock {
        fn now_ms(&self) -> u64 {
            self.0.load(Ordering::Relaxed)
        }
    }

    #[derive(Debug, Default)]
    struct RecordingDetector(Mutex<Vec<u64>>);

    impl ConditionDetector for RecordingDetector {
        fn observe(
            &self,
            request: &super::super::ObservationRequest<'_>,
            _capture: &(dyn CaptureSource + Send + Sync),
        ) -> Result<super::super::DetectorEvidence> {
            self.0.lock().unwrap().push(request.observed_at_ms);
            Ok(super::super::DetectorEvidence {
                matched: true,
                frame_id: request.observed_at_ms,
                captured_at_ms: request.observed_at_ms,
                match_rect: Some(Rect::new(1, 2, 3, 4)),
                score: Some(0.99),
                match_count: 1,
                stable_frames: 1,
                frame_metadata: None,
                details: serde_json::Value::Null,
            })
        }
    }

    #[derive(Debug, Default)]
    struct FailingDetector;

    impl ConditionDetector for FailingDetector {
        fn observe(
            &self,
            _request: &super::super::ObservationRequest<'_>,
            _capture: &(dyn CaptureSource + Send + Sync),
        ) -> Result<super::super::DetectorEvidence> {
            bail!("capture device unavailable")
        }
    }

    #[derive(Debug)]
    struct LifecycleDetector {
        fail_observation: bool,
        finished: Mutex<Vec<(String, Vec<u64>)>>,
    }

    #[derive(Default)]
    struct GenerationChangingDetector {
        control: Mutex<Option<RuntimeControlHandle>>,
        changed: AtomicBool,
        observed: Mutex<Vec<u64>>,
        finished: Mutex<Vec<u64>>,
    }

    impl ConditionDetector for GenerationChangingDetector {
        fn observe(
            &self,
            request: &super::super::ObservationRequest<'_>,
            _capture: &(dyn CaptureSource + Send + Sync),
        ) -> Result<super::super::DetectorEvidence> {
            self.observed.lock().unwrap().push(request.generation);
            if !self.changed.swap(true, Ordering::SeqCst) {
                let control = self.control.lock().unwrap().clone().unwrap();
                control.pause();
                control.resume();
            }
            Ok(super::super::DetectorEvidence::unmatched(
                request.observed_at_ms,
                request.observed_at_ms,
            ))
        }

        fn run_finished(&self, _run_id: &str, generations: &[u64]) {
            self.finished.lock().unwrap().extend_from_slice(generations);
        }
    }

    impl LifecycleDetector {
        fn new(fail_observation: bool) -> Self {
            Self {
                fail_observation,
                finished: Mutex::new(Vec::new()),
            }
        }
    }

    impl ConditionDetector for LifecycleDetector {
        fn observe(
            &self,
            request: &super::super::ObservationRequest<'_>,
            _capture: &(dyn CaptureSource + Send + Sync),
        ) -> Result<super::super::DetectorEvidence> {
            if self.fail_observation {
                bail!("lifecycle fixture failure");
            }
            Ok(super::super::DetectorEvidence::unmatched(
                request.observed_at_ms,
                request.observed_at_ms,
            ))
        }

        fn run_finished(&self, run_id: &str, generations: &[u64]) {
            self.finished
                .lock()
                .unwrap()
                .push((run_id.to_string(), generations.to_vec()));
        }
    }

    #[derive(Debug, Default)]
    struct CountingUnmatchedDetector(AtomicU64);

    impl ConditionDetector for CountingUnmatchedDetector {
        fn observe(
            &self,
            request: &super::super::ObservationRequest<'_>,
            _capture: &(dyn CaptureSource + Send + Sync),
        ) -> Result<super::super::DetectorEvidence> {
            self.0.fetch_add(1, Ordering::Relaxed);
            Ok(super::super::DetectorEvidence::unmatched(
                request.observed_at_ms,
                request.observed_at_ms,
            ))
        }
    }

    #[derive(Default)]
    struct InvalidatingDetector {
        control: Mutex<Option<RuntimeControlHandle>>,
        invalidated_once: AtomicBool,
        calls: AtomicU64,
    }

    impl ConditionDetector for InvalidatingDetector {
        fn observe(
            &self,
            request: &super::super::ObservationRequest<'_>,
            _capture: &(dyn CaptureSource + Send + Sync),
        ) -> Result<super::super::DetectorEvidence> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            if !self.invalidated_once.swap(true, Ordering::AcqRel) {
                let control = self.control.lock().unwrap().clone().unwrap();
                control.pause();
                control.resume();
            }
            Ok(super::super::DetectorEvidence {
                matched: true,
                frame_id: 1,
                captured_at_ms: request.observed_at_ms,
                match_rect: Some(Rect::new(1, 2, 3, 4)),
                score: Some(0.99),
                match_count: 1,
                stable_frames: 1,
                frame_metadata: None,
                details: serde_json::Value::Null,
            })
        }
    }

    #[derive(Default)]
    struct InvalidatingErrorDetector {
        control: Mutex<Option<RuntimeControlHandle>>,
        calls: AtomicU64,
    }

    impl ConditionDetector for InvalidatingErrorDetector {
        fn observe(
            &self,
            request: &super::super::ObservationRequest<'_>,
            _capture: &(dyn CaptureSource + Send + Sync),
        ) -> Result<super::super::DetectorEvidence> {
            let call = self.calls.fetch_add(1, Ordering::Relaxed);
            if call == 0 {
                let control = self.control.lock().unwrap().clone().unwrap();
                control.pause();
                control.resume();
                bail!("stale capture failure")
            }
            Ok(super::super::DetectorEvidence {
                matched: true,
                frame_id: 2,
                captured_at_ms: request.observed_at_ms,
                match_rect: Some(Rect::new(1, 2, 3, 4)),
                score: Some(0.99),
                match_count: 1,
                stable_frames: 1,
                frame_metadata: None,
                details: serde_json::Value::Null,
            })
        }
    }

    fn fixture_definition(blocks: Vec<Block>) -> MacroDefinition {
        MacroDefinition {
            schema_version: MACRO_SCHEMA_VERSION,
            id: "macro".to_string(),
            name: "Fixture".to_string(),
            revision: 1,
            target: TargetProfile {
                process_path: "game.exe".to_string(),
                window_class: "game".to_string(),
                title_contains: "Diablo".to_string(),
                captured_client_width: 1920,
                captured_client_height: 1080,
                captured_dpi: 96,
            },
            regions: vec![RegionDefinition {
                id: "region".to_string(),
                revision: 1,
                rect: RectRatio {
                    x: 0.1,
                    y: 0.1,
                    width: 0.2,
                    height: 0.2,
                },
            }],
            points: vec![PointDefinition {
                id: "point".to_string(),
                revision: 1,
                point: PointRatio { x: 0.5, y: 0.5 },
            }],
            text_rules: vec![TextRule {
                id: "text".to_string(),
                revision: 1,
                region_id: "region".to_string(),
                language: "en-US".to_string(),
                preprocess: PreprocessProfile::Original,
                expected: "ready".to_string(),
                match_mode: TextMatchMode::Contains,
                threshold: 0.9,
                case_sensitive: false,
                allow_cross_line: false,
                match_policy: MatchSelectionPolicy::HighestScore,
                poll_interval_ms: 1,
                timeout_ms: Limit::Finite(10),
                stable_frames: 1,
            }],
            image_rules: vec![],
            blocks,
            safety: SafetyPolicy {
                max_runtime_ms: Limit::Finite(60_000),
                max_clicks: Limit::Finite(100),
                max_observation_retries: Limit::Finite(100),
                max_observations_per_second: 30,
                minimum_click_interval_ms: 1,
                focus_loss: FocusLossPolicy::Stop,
            },
        }
    }

    fn saved(definition: MacroDefinition) -> SavedRevision {
        let definition_hash = sha256_hex(&serde_json::to_vec_pretty(&definition).unwrap());
        SavedRevision {
            definition,
            definition_hash,
            pinned_assets: vec![],
        }
    }

    fn fixture_template(seed: u8) -> GrayImage {
        GrayImage::from_fn(7, 5, |x, y| {
            Luma([seed.wrapping_add((x * 31 + y * 47) as u8)])
        })
    }

    fn png_bytes(image: GrayImage) -> Vec<u8> {
        let mut bytes = Cursor::new(Vec::new());
        DynamicImage::ImageLuma8(image)
            .write_to(&mut bytes, ImageFormat::Png)
            .unwrap();
        bytes.into_inner()
    }

    fn fixture_image_rule(id: &str, asset: AssetRef, template: Option<&GrayImage>) -> ImageRule {
        let mut rule = ImageRule {
            id: id.to_string(),
            revision: 1,
            region_id: "region".to_string(),
            template: asset,
            transparent_mask: None,
            threshold: 0.95,
            scales_percent: vec![100],
            stable_frames: 1,
            maximum_center_drift_px: 2,
            minimum_runner_up_margin: 0.05,
            verification: None,
            match_policy: MatchSelectionPolicy::ExactlyOne,
            poll_interval_ms: 1,
            timeout_ms: Limit::Finite(10),
        };
        rule.verification = Some(if let Some(template) = template {
            let negative_samples = vec![NegativeCorpusSample {
                stable_id: "negative/a".to_string(),
                content_sha256: NEGATIVE_CORPUS_SHA256.to_string(),
                measured_score: 0.80,
                evaluation: NegativeSampleEvaluationInputs::for_rule(&rule, 96, 1, (384, 216)),
            }];
            ImageRuleVerification::verify(ImageRuleVerificationInput {
                rule: &rule,
                template,
                mask: None,
                captured_dpi: 96,
                current_dpi: 96,
                region_revision: 1,
                search_dimensions: (384, 216),
                negative_samples: &negative_samples,
                observed_clusters: &[],
                maximum_score_cells: DEFAULT_MAX_SCORE_CELLS,
            })
            .unwrap()
            .into_artifact()
        } else {
            let mut artifact = ImageRuleVerificationArtifact {
                version: IMAGE_RULE_VERIFICATION_VERSION,
                preprocess: ImageVerificationPreprocess::GrayscaleNormalizedCrossCorrelation,
                rule_id: rule.id.clone(),
                rule_revision: rule.revision,
                template: rule.template.clone(),
                transparent_mask: None,
                captured_dpi: 96,
                region_id: rule.region_id.clone(),
                region_revision: 1,
                search_width: 384,
                search_height: 216,
                scales_percent: rule.scales_percent.clone(),
                threshold: rule.threshold,
                minimum_runner_up_margin: rule.minimum_runner_up_margin,
                negative_corpus_sha256: NEGATIVE_CORPUS_SHA256.to_string(),
                negative_sample_count: 100_000,
                best_negative_score: 0.80,
                active_mask_variance: 42.0,
                verification_fingerprint_sha256: String::new(),
            };
            artifact.verification_fingerprint_sha256 =
                super::super::image_verification::fingerprint(&artifact);
            artifact
        });
        rule
    }

    fn fixture_click_macro() -> SavedRevision {
        saved(fixture_definition(vec![Block {
            id: "click".to_string(),
            enabled: true,
            kind: BlockKind::Action {
                action: Action::ClickPoint {
                    point_id: "point".to_string(),
                    button: super::super::MouseButton::Left,
                },
            },
        }]))
    }

    fn fixture_runtime() -> MacroRuntime {
        fixture_runtime_with_detector(FakeDetector::default())
    }

    fn fixture_runtime_with_detector(detector: impl ConditionDetector + 'static) -> MacroRuntime {
        MacroRuntime::new(
            Arc::new(FakeCapture),
            Arc::new(detector),
            Arc::new(FakeClock::default()),
        )
    }

    fn text_condition(source_block_id: &str, mode: ObserveMode) -> Condition {
        Condition::Text {
            source_block_id: source_block_id.to_string(),
            rule_id: "text".to_string(),
            mode,
        }
    }

    fn block(id: &str, kind: BlockKind) -> Block {
        Block {
            id: id.to_string(),
            enabled: true,
            kind,
        }
    }

    fn point_action(id: &str) -> Block {
        block(
            id,
            BlockKind::Action {
                action: Action::ClickPoint {
                    point_id: "point".to_string(),
                    button: super::super::MouseButton::Left,
                },
            },
        )
    }

    fn check_now_observation_macro() -> SavedRevision {
        saved(fixture_definition(vec![block(
            "observe",
            BlockKind::Observe {
                condition: text_condition("observe", ObserveMode::CheckNow),
            },
        )]))
    }

    #[test]
    fn detector_run_finished_hook_runs_exactly_once_on_success_and_failure() {
        for fail_observation in [false, true] {
            let detector = Arc::new(LifecycleDetector::new(fail_observation));
            let runtime = MacroRuntime::new(
                Arc::new(FakeCapture),
                detector.clone(),
                Arc::new(FakeClock::default()),
            );

            let events = runtime
                .run(check_now_observation_macro(), RunMode::DryRun)
                .unwrap();
            let stopped_run_id = match events.last().unwrap() {
                RunEvent::RunStopped { run_id, .. } => run_id,
                event => panic!("expected terminal run event, got {event:?}"),
            };
            let finished = detector.finished.lock().unwrap();
            assert_eq!(finished.len(), 1);
            assert_eq!(&finished[0].0, stopped_run_id);
            assert!(!finished[0].1.is_empty());
            assert!(finished[0].1.iter().all(|generation| *generation > 0));
        }
    }

    #[test]
    fn detector_completion_covers_generation_changes_during_the_run() {
        let detector = Arc::new(GenerationChangingDetector::default());
        let runtime = MacroRuntime::new(
            Arc::new(FakeCapture),
            detector.clone(),
            Arc::new(FakeClock::default()),
        );
        *detector.control.lock().unwrap() = Some(runtime.control_handle());

        runtime
            .run(check_now_observation_macro(), RunMode::DryRun)
            .unwrap();

        let observed = detector.observed.lock().unwrap();
        assert_eq!(observed.len(), 2);
        assert_eq!(
            detector.finished.lock().unwrap().as_slice(),
            observed.as_slice()
        );
    }

    #[test]
    fn dry_run_plans_actions_without_an_input_sink() {
        let runtime = fixture_runtime();
        let events = runtime.run(fixture_click_macro(), RunMode::DryRun).unwrap();

        assert!(
            events
                .iter()
                .any(|event| matches!(event, RunEvent::ActionPlanned { .. }))
        );
    }

    #[test]
    fn repeat_until_skips_already_satisfied_body() {
        let definition = fixture_definition(vec![block(
            "repeat",
            BlockKind::RepeatUntil {
                condition: text_condition("repeat", ObserveMode::CheckNow),
                max_iterations: Limit::Unlimited,
                body: vec![point_action("body-click")],
            },
        )]);

        let events = fixture_runtime_with_detector(FakeDetector::returning([true]))
            .run(saved(definition), RunMode::DryRun)
            .unwrap();

        assert!(!events.iter().any(|event| {
            matches!(event, RunEvent::BlockEntered { block_id, .. } if block_id == "body-click")
        }));
    }

    #[test]
    fn wait_timeout_runs_its_explicit_body() {
        let definition = fixture_definition(vec![block(
            "observe",
            BlockKind::Observe {
                condition: text_condition(
                    "observe",
                    ObserveMode::WaitForTrue {
                        timeout_ms: Limit::Finite(2),
                        timeout_outcome: TimeoutOutcome::RunBody {
                            body: vec![point_action("timeout-click")],
                        },
                    },
                ),
            },
        )]);

        let events = fixture_runtime_with_detector(FakeDetector::returning([false, false]))
            .run(saved(definition), RunMode::DryRun)
            .unwrap();

        assert!(events.iter().any(|event| {
            matches!(event, RunEvent::ActionPlanned { block_id, .. } if block_id == "timeout-click")
        }));
    }

    #[test]
    fn if_executes_exactly_one_branch() {
        let definition = fixture_definition(vec![block(
            "if",
            BlockKind::If {
                condition: text_condition("if", ObserveMode::CheckNow),
                then_body: vec![point_action("then")],
                else_body: vec![point_action("else")],
            },
        )]);

        let events = fixture_runtime_with_detector(FakeDetector::returning([false]))
            .run(saved(definition), RunMode::DryRun)
            .unwrap();

        assert!(events.iter().any(|event| {
            matches!(event, RunEvent::ActionPlanned { block_id, .. } if block_id == "else")
        }));
        assert!(!events.iter().any(|event| {
            matches!(event, RunEvent::ActionPlanned { block_id, .. } if block_id == "then")
        }));
    }

    #[test]
    fn repeat_n_enters_body_exactly_n_times() {
        let definition = fixture_definition(vec![block(
            "repeat",
            BlockKind::RepeatN {
                count: 3,
                body: vec![block(
                    "body",
                    BlockKind::Comment {
                        text: "iteration".to_string(),
                    },
                )],
            },
        )]);

        let events = fixture_runtime()
            .run(saved(definition), RunMode::DryRun)
            .unwrap();
        let entries = events
            .iter()
            .filter(|event| {
                matches!(event, RunEvent::BlockEntered { block_id, .. } if block_id == "body")
            })
            .count();

        assert_eq!(entries, 3);
    }

    #[test]
    fn nested_stop_is_macro_wide() {
        let definition = fixture_definition(vec![
            block(
                "repeat",
                BlockKind::RepeatN {
                    count: 3,
                    body: vec![block("stop", BlockKind::StopSuccess)],
                },
            ),
            point_action("never"),
        ]);

        let events = fixture_runtime()
            .run(saved(definition), RunMode::DryRun)
            .unwrap();

        assert!(!events.iter().any(|event| {
            matches!(event, RunEvent::BlockEntered { block_id, .. } if block_id == "never")
        }));
        assert!(matches!(
            events.last(),
            Some(RunEvent::RunStopped {
                reason: StopReason::StopSuccess,
                ..
            })
        ));
    }

    #[test]
    fn final_block_detector_failure_stops_by_default() {
        let definition = fixture_definition(vec![block(
            "observe",
            BlockKind::Observe {
                condition: text_condition("observe", ObserveMode::CheckNow),
            },
        )]);
        let runtime = MacroRuntime::new(
            Arc::new(FakeCapture),
            Arc::new(FailingDetector),
            Arc::new(FakeClock::default()),
        );

        let events = runtime.run(saved(definition), RunMode::DryRun).unwrap();

        assert!(matches!(
            events.last(),
            Some(RunEvent::RunStopped {
                reason: StopReason::TechnicalFailure { message },
                ..
            }) if message.contains("capture device unavailable")
        ));
    }

    #[test]
    fn ordered_events_use_monotonic_elapsed_time() {
        let events = fixture_runtime()
            .run(fixture_click_macro(), RunMode::DryRun)
            .unwrap();

        for (index, event) in events.iter().enumerate() {
            assert_eq!(event.sequence(), index as u64 + 1);
        }
        assert!(
            events
                .windows(2)
                .all(|pair| { pair[0].elapsed_ms() <= pair[1].elapsed_ms() })
        );
        let run_id = match &events[0] {
            RunEvent::RunStarted { run_id, .. } => run_id,
            other => panic!("unexpected first event: {other:?}"),
        };
        assert!(events.iter().all(|event| event_run_id(event) == run_id));
    }

    #[test]
    fn pause_and_resume_discard_in_flight_results_and_reobserve() {
        let detector = Arc::new(InvalidatingDetector::default());
        let runtime = MacroRuntime::new(
            Arc::new(FakeCapture),
            detector.clone(),
            Arc::new(FakeClock::default()),
        );
        *detector.control.lock().unwrap() = Some(runtime.control_handle());
        let definition = fixture_definition(vec![
            block(
                "observe",
                BlockKind::Observe {
                    condition: text_condition("observe", ObserveMode::CheckNow),
                },
            ),
            block(
                "click-match",
                BlockKind::Action {
                    action: Action::ClickTextMatch {
                        source_block_id: "observe".to_string(),
                        button: super::super::MouseButton::Left,
                    },
                },
            ),
        ]);

        let events = runtime.run(saved(definition), RunMode::DryRun).unwrap();

        assert!(detector.calls.load(Ordering::Relaxed) >= 2);
        assert!(events.iter().any(|event| {
            matches!(event, RunEvent::ActionPlanned { block_id, token: Some(token), .. }
                if block_id == "click-match" && token.generation == runtime.control_handle().generation())
        }));
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, RunEvent::ActionBlocked { .. }))
        );
    }

    #[test]
    fn pause_and_resume_discard_in_flight_detector_errors_before_interpreting_them() {
        let detector = Arc::new(InvalidatingErrorDetector::default());
        let runtime = MacroRuntime::new(
            Arc::new(FakeCapture),
            detector.clone(),
            Arc::new(FakeClock::default()),
        );
        *detector.control.lock().unwrap() = Some(runtime.control_handle());
        let definition = fixture_definition(vec![
            block(
                "observe",
                BlockKind::Observe {
                    condition: text_condition("observe", ObserveMode::CheckNow),
                },
            ),
            block(
                "click-match",
                BlockKind::Action {
                    action: Action::ClickTextMatch {
                        source_block_id: "observe".to_string(),
                        button: super::super::MouseButton::Left,
                    },
                },
            ),
        ]);

        let events = runtime.run(saved(definition), RunMode::DryRun).unwrap();

        assert_eq!(detector.calls.load(Ordering::Relaxed), 2);
        assert!(events.iter().any(|event| {
            matches!(event, RunEvent::ActionPlanned { block_id, .. } if block_id == "click-match")
        }));
        assert!(!events.iter().any(|event| {
            matches!(
                event,
                RunEvent::RunStopped {
                    reason: StopReason::TechnicalFailure { .. },
                    ..
                }
            )
        }));
    }

    #[test]
    fn later_false_observation_clears_the_declared_source_token() {
        let definition = fixture_definition(vec![
            block(
                "source-a",
                BlockKind::Observe {
                    condition: text_condition("source-a", ObserveMode::CheckNow),
                },
            ),
            block(
                "recheck-a",
                BlockKind::If {
                    condition: text_condition("source-a", ObserveMode::CheckNow),
                    then_body: vec![],
                    else_body: vec![block(
                        "must-not-click-stale-a",
                        BlockKind::Action {
                            action: Action::ClickTextMatch {
                                source_block_id: "source-a".to_string(),
                                button: super::super::MouseButton::Left,
                            },
                        },
                    )],
                },
            ),
        ]);

        let events = fixture_runtime_with_detector(FakeDetector::returning([true, false]))
            .run(saved(definition), RunMode::DryRun)
            .unwrap();

        assert!(!events.iter().any(|event| {
            matches!(event, RunEvent::ActionPlanned { block_id, .. }
                if block_id == "must-not-click-stale-a")
        }));
        assert!(events.iter().any(|event| {
            matches!(event, RunEvent::ActionBlocked { block_id, reason, .. }
                if block_id == "must-not-click-stale-a" && reason.contains("fresh observation"))
        }));
    }

    #[test]
    fn action_token_must_match_compiled_source_identity() {
        let action = Action::ClickTextMatch {
            source_block_id: "observe".to_string(),
            button: super::super::MouseButton::Left,
        };
        let revision = saved(fixture_definition(vec![
            block(
                "observe",
                BlockKind::Observe {
                    condition: text_condition("observe", ObserveMode::CheckNow),
                },
            ),
            block(
                "click-match",
                BlockKind::Action {
                    action: action.clone(),
                },
            ),
        ]));
        let events = fixture_runtime_with_detector(FakeDetector::returning([true]))
            .run(revision.clone(), RunMode::DryRun)
            .unwrap();
        let token = events
            .iter()
            .find_map(|event| match event {
                RunEvent::ActionPlanned {
                    token: Some(token), ..
                } => Some(token.clone()),
                _ => None,
            })
            .unwrap();
        let compiled = CompiledMacro::compile(revision).unwrap();

        validate_action_token(&compiled, &action, &token).unwrap();
        for mismatched in [
            {
                let mut token = token.clone();
                token.source_block_id = "other".to_string();
                token
            },
            {
                let mut token = token.clone();
                token.detector = DetectorKind::Image;
                token
            },
            {
                let mut token = token.clone();
                token.rule_id = "other".to_string();
                token
            },
            {
                let mut token = token.clone();
                token.rule_revision += 1;
                token
            },
            {
                let mut token = token.clone();
                token.region_id = "other".to_string();
                token
            },
            {
                let mut token = token.clone();
                token.region_revision += 1;
                token
            },
        ] {
            assert!(validate_action_token(&compiled, &action, &mismatched).is_err());
        }
    }

    #[test]
    fn action_authorization_binds_compiled_action_token_target_and_screen_geometry() {
        let action = Action::ClickTextMatch {
            source_block_id: "observe".to_string(),
            button: super::super::MouseButton::Left,
        };
        let revision = saved(fixture_definition(vec![
            block(
                "observe",
                BlockKind::Observe {
                    condition: text_condition("observe", ObserveMode::CheckNow),
                },
            ),
            block(
                "click-match",
                BlockKind::Action {
                    action: action.clone(),
                },
            ),
        ]));
        let compiled = CompiledMacro::compile(revision).unwrap();
        let target = TargetSnapshot {
            window_id: 91,
            process_id: 7,
            process_started_at_100ns: 100,
            process_path: "game.exe".to_string(),
            client_rect: Rect::new(100, 200, 800, 600),
            window_revision: 1,
            geometry_revision: 2,
            dpi: 144,
            display_profile: "display-a".to_string(),
            display_profile_revision: 3,
            is_visible: true,
            is_minimized: false,
            is_foreground: true,
        };
        let token = ObservationToken {
            run_id: "run-1".to_string(),
            generation: 4,
            source_block_id: "observe".to_string(),
            detector: DetectorKind::Text,
            region_id: "region".to_string(),
            region_revision: 1,
            rule_id: "text".to_string(),
            rule_revision: 1,
            frame_id: 8,
            captured_at_ms: 10,
            match_rect: Some(Rect::new(10, 20, 30, 40)),
            score: Some(0.99),
            match_count: 1,
            stable_frames: 2,
            frame_metadata: Some(super::super::ImageFrameMetadata {
                frame_id: 8,
                captured_at_ms: 10,
                window_id: 91,
                window_revision: 1,
                client_x: 100,
                client_y: 200,
                client_width: 800,
                client_height: 600,
                geometry_revision: 2,
                display_profile_revision: 3,
                dpi: 144,
                region_revision: 1,
                rule_revision: 1,
            }),
            evidence: serde_json::Value::Null,
        };
        let resume = ResumeAuthorization::for_test(target.clone());

        let authorization = compiled
            .authorize_action(
                "run-1",
                4,
                12,
                "click-match",
                &action,
                Some(&token),
                &resume,
                Point::new(115, 225),
                1_000,
            )
            .unwrap();
        assert_eq!(authorization.action, action);
        assert_eq!(authorization.block_id, "click-match");
        assert_eq!(authorization.expected_target, target);
        assert_eq!(
            authorization.screen_authorized_rect,
            Some(Rect::new(110, 220, 30, 40))
        );

        let mut wrong_source = token.clone();
        wrong_source.source_block_id = "other".to_string();
        assert!(
            compiled
                .authorize_action(
                    "run-1",
                    4,
                    13,
                    "click-match",
                    &action,
                    Some(&wrong_source),
                    &resume,
                    Point::new(115, 225),
                    1_000,
                )
                .is_err()
        );

        let mut wrong_frame = token.clone();
        wrong_frame.frame_metadata.as_mut().unwrap().frame_id += 1;
        assert!(
            compiled
                .authorize_action(
                    "run-1",
                    4,
                    14,
                    "click-match",
                    &action,
                    Some(&wrong_frame),
                    &resume,
                    Point::new(115, 225),
                    1_000,
                )
                .is_err()
        );

        let wrong_action = Action::ClickTextMatch {
            source_block_id: "observe".to_string(),
            button: super::super::MouseButton::Right,
        };
        assert!(
            compiled
                .authorize_action(
                    "run-1",
                    4,
                    15,
                    "click-match",
                    &wrong_action,
                    Some(&token),
                    &resume,
                    Point::new(115, 225),
                    1_000,
                )
                .is_err()
        );
    }

    #[test]
    fn window_a_observation_cannot_authorize_against_window_b_resume_guard() {
        let action = Action::ClickTextMatch {
            source_block_id: "observe".to_string(),
            button: super::super::MouseButton::Left,
        };
        let revision = saved(fixture_definition(vec![
            block(
                "observe",
                BlockKind::Observe {
                    condition: text_condition("observe", ObserveMode::CheckNow),
                },
            ),
            block(
                "click-match",
                BlockKind::Action {
                    action: action.clone(),
                },
            ),
        ]));
        let compiled = CompiledMacro::compile(revision).unwrap();
        let window_b = TargetSnapshot {
            window_id: 92,
            process_id: 8,
            process_started_at_100ns: 200,
            process_path: "game.exe".to_string(),
            client_rect: Rect::new(500, 300, 800, 600),
            window_revision: 1,
            geometry_revision: 2,
            dpi: 144,
            display_profile: "display-a".to_string(),
            display_profile_revision: 3,
            is_visible: true,
            is_minimized: false,
            is_foreground: true,
        };
        let resume = ResumeAuthorization::for_test(window_b);
        let window_a_token = ObservationToken {
            run_id: "run-1".to_string(),
            generation: 4,
            source_block_id: "observe".to_string(),
            detector: DetectorKind::Text,
            region_id: "region".to_string(),
            region_revision: 1,
            rule_id: "text".to_string(),
            rule_revision: 1,
            frame_id: 8,
            captured_at_ms: 10,
            match_rect: Some(Rect::new(10, 20, 30, 40)),
            score: Some(0.99),
            match_count: 1,
            stable_frames: 2,
            frame_metadata: Some(super::super::ImageFrameMetadata {
                frame_id: 8,
                captured_at_ms: 10,
                window_id: 91,
                window_revision: 1,
                client_x: 100,
                client_y: 200,
                client_width: 800,
                client_height: 600,
                geometry_revision: 2,
                display_profile_revision: 3,
                dpi: 144,
                region_revision: 1,
                rule_revision: 1,
            }),
            evidence: serde_json::Value::Null,
        };

        let error = compiled
            .authorize_action(
                "run-1",
                4,
                20,
                "click-match",
                &action,
                Some(&window_a_token),
                &resume,
                Point::new(115, 225),
                1_000,
            )
            .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("frame HWND does not match target")
        );
    }

    #[test]
    fn local_match_geometry_conversion_is_checked_and_cannot_overflow() {
        assert_eq!(
            local_rect_to_screen(Rect::new(-500, 300, 800, 600), Rect::new(10, 20, 30, 40))
                .unwrap(),
            Rect::new(-490, 320, 30, 40)
        );
        assert!(
            local_rect_to_screen(
                Rect::new(i32::MAX - 5, 0, 100, 100),
                Rect::new(10, 10, 20, 20),
            )
            .is_err()
        );
        assert!(
            local_rect_to_screen(Rect::new(100, 200, 800, 600), Rect::new(790, 10, 20, 20),)
                .is_err()
        );
    }

    #[test]
    fn observation_token_preserves_typed_frame_metadata() {
        let definition = fixture_definition(vec![
            block(
                "observe",
                BlockKind::Observe {
                    condition: text_condition("observe", ObserveMode::CheckNow),
                },
            ),
            block(
                "click-match",
                BlockKind::Action {
                    action: Action::ClickTextMatch {
                        source_block_id: "observe".to_string(),
                        button: super::super::MouseButton::Left,
                    },
                },
            ),
        ]);
        let events = fixture_runtime_with_detector(MetadataDetector)
            .run(saved(definition), RunMode::DryRun)
            .unwrap();
        let token = events.iter().find_map(|event| match event {
            RunEvent::ActionPlanned {
                token: Some(token), ..
            } => Some(token),
            _ => None,
        });

        assert_eq!(token.unwrap().frame_metadata.unwrap().window_id, 9);
    }

    #[test]
    fn stop_is_checked_during_wait_slices() {
        let runtime = MacroRuntime::new(
            Arc::new(FakeCapture),
            Arc::new(FakeDetector::default()),
            Arc::new(FrozenClock),
        );
        let runner = runtime.clone();
        let definition = fixture_definition(vec![block(
            "wait",
            BlockKind::Wait {
                duration_ms: 60_000,
            },
        )]);
        let handle = thread::spawn(move || runner.run(saved(definition), RunMode::DryRun).unwrap());
        thread::sleep(Duration::from_millis(20));
        runtime.stop();
        let events = handle.join().unwrap();

        assert!(matches!(
            events.last(),
            Some(RunEvent::RunStopped {
                reason: StopReason::UserStopped,
                ..
            })
        ));
    }

    #[test]
    fn maximum_runtime_is_enforced_while_the_runtime_is_paused() {
        let clock = Arc::new(ManualClock::default());
        let runtime = MacroRuntime::new(
            Arc::new(FakeCapture),
            Arc::new(FakeDetector::default()),
            clock.clone(),
        );
        let runner = runtime.clone();
        let mut definition = fixture_definition(vec![block(
            "wait",
            BlockKind::Wait {
                duration_ms: 60_000,
            },
        )]);
        definition.safety.max_runtime_ms = Limit::Finite(50);
        let handle = thread::spawn(move || runner.run(saved(definition), RunMode::DryRun).unwrap());
        thread::sleep(Duration::from_millis(20));
        runtime.pause();
        clock.set(100);
        thread::sleep(Duration::from_millis(40));
        if !handle.is_finished() {
            runtime.stop();
        }
        let events = handle.join().unwrap();

        assert!(matches!(
            events.last(),
            Some(RunEvent::RunStopped {
                reason: StopReason::SafetyLimit { message },
                ..
            }) if message == "maximum runtime exceeded"
        ));
    }

    #[test]
    fn continuous_execution_yields_until_stopped() {
        let runtime = MacroRuntime::new(
            Arc::new(FakeCapture),
            Arc::new(FakeDetector::default()),
            Arc::new(SystemClock::default()),
        );
        let runner = runtime.clone();
        let definition = fixture_definition(vec![block(
            "continuous",
            BlockKind::Continuous {
                body: vec![point_action("paced-action")],
            },
        )]);
        let handle = thread::spawn(move || runner.run(saved(definition), RunMode::DryRun).unwrap());
        thread::sleep(Duration::from_millis(20));
        runtime.stop();
        let events = handle.join().unwrap();

        assert!(events.iter().any(|event| {
            matches!(event, RunEvent::LoopYielded { block_id, .. } if block_id == "continuous")
        }));
        assert!(matches!(
            events.last(),
            Some(RunEvent::RunStopped {
                reason: StopReason::UserStopped,
                ..
            })
        ));
    }

    #[test]
    fn bounded_event_channel_coalesces_progress_before_critical_events() {
        let channels = bounded_runtime_channels(1, 1);
        let first = progress_event(1);
        let second = progress_event(2);
        assert_eq!(channels.events.send(first), EventDelivery::Sent);
        assert_eq!(
            channels.events.send(second),
            EventDelivery::CoalescedProgress
        );

        let critical = stopped_event();
        assert_eq!(channels.events.send(critical.clone()), EventDelivery::Sent);
        assert_eq!(channels.event_receiver.try_recv(), Some(critical));
    }

    #[test]
    fn progress_is_not_reordered_across_an_older_critical_event() {
        let channels = bounded_runtime_channels(2, 2);
        let first_progress = progress_event(1);
        let critical = stopped_event();
        let newer_progress = progress_event(4);
        assert_eq!(
            channels.events.send(first_progress.clone()),
            EventDelivery::Sent
        );
        assert_eq!(channels.events.send(critical.clone()), EventDelivery::Sent);

        assert_eq!(
            channels.events.send(newer_progress),
            EventDelivery::DroppedProgress
        );
        assert_eq!(channels.event_receiver.try_recv(), Some(first_progress));
        assert_eq!(channels.event_receiver.try_recv(), Some(critical));
    }

    #[test]
    fn bounded_command_channel_reports_full_without_dropping_existing_command() {
        let channels = bounded_runtime_channels(1, 1);
        assert_eq!(
            channels.commands.try_send(RuntimeCommand::Pause),
            CommandDelivery::Sent
        );
        assert_eq!(
            channels.commands.try_send(RuntimeCommand::Stop),
            CommandDelivery::Full
        );
        assert!(matches!(
            channels.command_receiver.try_recv(),
            Some(RuntimeCommand::Pause)
        ));
    }

    #[test]
    fn move_only_does_not_consume_click_budget() {
        let mut definition = fixture_definition(vec![block(
            "move",
            BlockKind::Action {
                action: Action::MoveOnly {
                    target: super::super::ActionTarget::Point {
                        point_id: "point".to_string(),
                    },
                },
            },
        )]);
        definition.safety.max_clicks = Limit::Finite(0);

        let events = fixture_runtime()
            .run(saved(definition), RunMode::DryRun)
            .unwrap();

        assert!(matches!(
            events.last(),
            Some(RunEvent::RunStopped {
                reason: StopReason::Completed,
                ..
            })
        ));
    }

    #[test]
    fn one_runtime_rejects_a_second_active_run() {
        let runtime = MacroRuntime::new(
            Arc::new(FakeCapture),
            Arc::new(FakeDetector::default()),
            Arc::new(FrozenClock),
        );
        let runner = runtime.clone();
        let waiting = fixture_definition(vec![block(
            "wait",
            BlockKind::Wait {
                duration_ms: 60_000,
            },
        )]);
        let handle = thread::spawn(move || runner.run(saved(waiting), RunMode::DryRun));
        thread::sleep(Duration::from_millis(20));

        let second = runtime.run(fixture_click_macro(), RunMode::DryRun);

        assert!(second.unwrap_err().to_string().contains("already active"));
        runtime.stop();
        handle.join().unwrap().unwrap();
    }

    #[test]
    fn compilation_rejects_hash_mismatch_and_owns_immutable_definition() {
        let saved = fixture_click_macro();
        let mut tampered = saved.clone();
        tampered.definition.name = "changed after save".to_string();
        assert!(CompiledMacro::compile(tampered).is_err());

        let compiled = CompiledMacro::compile(saved).unwrap();
        let original_name = compiled.definition().name.clone();
        let detached = compiled.definition().clone();
        assert_eq!(compiled.definition().name, original_name);
        assert_eq!(detached, *compiled.definition());
    }

    #[test]
    fn journal_mapping_preserves_event_order_and_typed_payload() {
        let event = stopped_event();
        let record = JournalRecord::from(event.clone());

        assert_eq!(record.sequence, event.sequence());
        assert_eq!(record.elapsed_ms, event.elapsed_ms());
        assert_eq!(record.kind, JournalKind::StateChange);
        assert_eq!(record.fields["type"], "run_stopped");
        assert_eq!(record.fields["reason"]["type"], "completed");
    }

    #[test]
    fn emergency_stop_bypasses_a_full_command_queue() {
        let channels = bounded_runtime_channels(1, 1);
        assert_eq!(
            channels.commands.try_send(RuntimeCommand::Pause),
            CommandDelivery::Sent
        );
        assert_eq!(
            channels.commands.try_send(RuntimeCommand::EmergencyStop),
            CommandDelivery::Full
        );
        assert!(channels.commands.emergency_stop_requested());
        assert!(channels.command_receiver.take_emergency_stop());
        assert!(!channels.command_receiver.take_emergency_stop());
    }

    #[test]
    fn emergency_stop_queue_bypass_stops_the_owning_runtime_and_wakes_waits() {
        let runtime = MacroRuntime::new(
            Arc::new(FakeCapture),
            Arc::new(FakeDetector::default()),
            Arc::new(SystemClock::default()),
        );
        let channels = runtime.bounded_channels(1, 1);
        assert_eq!(
            channels.commands.try_send(RuntimeCommand::Pause),
            CommandDelivery::Sent
        );
        let runner = runtime.clone();
        let definition = fixture_definition(vec![block(
            "wait",
            BlockKind::Wait {
                duration_ms: 60_000,
            },
        )]);
        let handle = thread::spawn(move || runner.run(saved(definition), RunMode::DryRun).unwrap());
        thread::sleep(Duration::from_millis(20));

        assert_eq!(
            channels.commands.try_send(RuntimeCommand::EmergencyStop),
            CommandDelivery::Full
        );
        let events = handle.join().unwrap();

        assert!(matches!(
            events.last(),
            Some(RunEvent::RunStopped {
                reason: StopReason::EmergencyStopped,
                ..
            })
        ));
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, RunEvent::ActionPlanned { .. }))
        );
    }

    #[test]
    fn receiving_the_queued_emergency_does_not_clear_the_runtime_bypass() {
        let runtime = MacroRuntime::new(
            Arc::new(FakeCapture),
            Arc::new(FakeDetector::default()),
            Arc::new(SystemClock::default()),
        );
        let channels = runtime.bounded_channels(1, 1);
        assert_eq!(
            channels.commands.try_send(RuntimeCommand::Pause),
            CommandDelivery::Sent
        );
        let runner = runtime.clone();
        let definition = fixture_definition(vec![block(
            "wait",
            BlockKind::Wait {
                duration_ms: 60_000,
            },
        )]);
        let handle = thread::spawn(move || runner.run(saved(definition), RunMode::DryRun).unwrap());
        thread::sleep(Duration::from_millis(20));

        assert_eq!(
            channels.commands.try_send(RuntimeCommand::EmergencyStop),
            CommandDelivery::Full
        );
        assert!(channels.command_receiver.take_emergency_stop());
        thread::sleep(Duration::from_millis(40));
        if !handle.is_finished() {
            runtime.stop();
        }
        let events = handle.join().unwrap();

        assert!(matches!(
            events.last(),
            Some(RunEvent::RunStopped {
                reason: StopReason::EmergencyStopped,
                ..
            })
        ));
    }

    #[test]
    fn critical_events_wait_for_capacity_and_are_never_dropped() {
        let channels = bounded_runtime_channels(1, 1);
        let first = stopped_event();
        let second = RunEvent::Error {
            sequence: 4,
            elapsed_ms: 4,
            run_id: "run".to_string(),
            block_id: None,
            message: "failure".to_string(),
        };
        assert_eq!(channels.events.send(first.clone()), EventDelivery::Sent);
        let sender = channels.events.clone();
        let expected = second.clone();
        let handle = thread::spawn(move || sender.send(expected));
        thread::sleep(Duration::from_millis(20));
        assert!(!handle.is_finished());

        assert_eq!(channels.event_receiver.try_recv(), Some(first));
        assert_eq!(handle.join().unwrap(), EventDelivery::Sent);
        assert_eq!(channels.event_receiver.try_recv(), Some(second));
    }

    #[test]
    fn condition_false_is_normal_and_timeout_stop_is_explicit() {
        let check_now = fixture_definition(vec![block(
            "observe",
            BlockKind::Observe {
                condition: text_condition("observe", ObserveMode::CheckNow),
            },
        )]);
        let events = fixture_runtime_with_detector(FakeDetector::returning([false]))
            .run(saved(check_now), RunMode::DryRun)
            .unwrap();
        assert!(matches!(
            events.last(),
            Some(RunEvent::RunStopped {
                reason: StopReason::Completed,
                ..
            })
        ));
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, RunEvent::Error { .. }))
        );

        let times_out = fixture_definition(vec![block(
            "observe",
            BlockKind::Observe {
                condition: text_condition(
                    "observe",
                    ObserveMode::WaitForTrue {
                        timeout_ms: Limit::Finite(1),
                        timeout_outcome: TimeoutOutcome::StopError {
                            message: "not ready".to_string(),
                        },
                    },
                ),
            },
        )]);
        let events = fixture_runtime_with_detector(FakeDetector::returning([false]))
            .run(saved(times_out), RunMode::DryRun)
            .unwrap();
        assert!(matches!(
            events.last(),
            Some(RunEvent::RunStopped {
                reason: StopReason::StopError { message },
                ..
            }) if message == "not ready"
        ));
    }

    #[test]
    fn timeout_continue_preserves_the_last_condition_value() {
        let definition = fixture_definition(vec![block(
            "if",
            BlockKind::If {
                condition: text_condition(
                    "if",
                    ObserveMode::WaitForFalse {
                        timeout_ms: Limit::Finite(1),
                        timeout_outcome: TimeoutOutcome::Continue,
                    },
                ),
                then_body: vec![point_action("then")],
                else_body: vec![point_action("else")],
            },
        )]);

        let events = fixture_runtime_with_detector(FakeDetector::returning([true]))
            .run(saved(definition), RunMode::DryRun)
            .unwrap();

        assert!(events.iter().any(|event| {
            matches!(event, RunEvent::ActionPlanned { block_id, .. } if block_id == "then")
        }));
        assert!(!events.iter().any(|event| {
            matches!(event, RunEvent::ActionPlanned { block_id, .. } if block_id == "else")
        }));
    }

    #[test]
    fn zero_retry_budget_still_allows_the_initial_observation() {
        let mut definition = fixture_definition(vec![block(
            "observe",
            BlockKind::Observe {
                condition: text_condition("observe", ObserveMode::CheckNow),
            },
        )]);
        definition.safety.max_observation_retries = Limit::Finite(0);

        let events = fixture_runtime_with_detector(FakeDetector::returning([true]))
            .run(saved(definition), RunMode::DryRun)
            .unwrap();

        assert!(matches!(
            events.last(),
            Some(RunEvent::RunStopped {
                reason: StopReason::Completed,
                ..
            })
        ));
    }

    fn run_finite_deadline_case(
        outcome: TimeoutOutcome,
        after: Vec<Block>,
    ) -> (Vec<RunEvent>, u64) {
        let detector = Arc::new(CountingUnmatchedDetector::default());
        let runtime = MacroRuntime::new(
            Arc::new(FakeCapture),
            detector.clone(),
            Arc::new(SystemClock::default()),
        );
        let mut blocks = vec![block(
            "observe",
            BlockKind::Observe {
                condition: text_condition(
                    "observe",
                    ObserveMode::WaitForTrue {
                        timeout_ms: Limit::Finite(100),
                        timeout_outcome: outcome,
                    },
                ),
            },
        )];
        blocks.extend(after);
        let mut definition = fixture_definition(blocks);
        definition.text_rules[0].poll_interval_ms = 10_000;
        definition.safety.max_observation_retries = Limit::Finite(0);
        definition.safety.max_observations_per_second = 1_000;
        let events = runtime.run(saved(definition), RunMode::DryRun).unwrap();
        (events, detector.0.load(Ordering::Relaxed))
    }

    #[test]
    fn finite_deadline_continue_does_not_poll_again_after_expiry() {
        let (events, calls) = run_finite_deadline_case(
            TimeoutOutcome::Continue,
            vec![point_action("after-continue")],
        );

        assert_eq!(calls, 1);
        assert!(events.iter().any(|event| {
            matches!(event, RunEvent::ActionPlanned { block_id, .. } if block_id == "after-continue")
        }));
        assert!(matches!(
            events.last(),
            Some(RunEvent::RunStopped {
                reason: StopReason::Completed,
                ..
            })
        ));
    }

    #[test]
    fn finite_deadline_run_body_does_not_poll_again_after_expiry() {
        let (events, calls) = run_finite_deadline_case(
            TimeoutOutcome::RunBody {
                body: vec![point_action("timeout-body")],
            },
            vec![],
        );

        assert_eq!(calls, 1);
        assert!(events.iter().any(|event| {
            matches!(event, RunEvent::ActionPlanned { block_id, .. } if block_id == "timeout-body")
        }));
        assert!(matches!(
            events.last(),
            Some(RunEvent::RunStopped {
                reason: StopReason::Completed,
                ..
            })
        ));
    }

    #[test]
    fn finite_deadline_stop_error_does_not_poll_again_after_expiry() {
        let (events, calls) = run_finite_deadline_case(
            TimeoutOutcome::StopError {
                message: "condition timed out".to_string(),
            },
            vec![],
        );

        assert_eq!(calls, 1);
        assert!(matches!(
            events.last(),
            Some(RunEvent::RunStopped {
                reason: StopReason::StopError { message },
                ..
            }) if message == "condition timed out"
        ));
    }

    #[test]
    fn retry_budget_is_scoped_to_each_condition_evaluation() {
        let mut definition = fixture_definition(vec![
            block(
                "first",
                BlockKind::Observe {
                    condition: text_condition("first", ObserveMode::CheckNow),
                },
            ),
            block(
                "second",
                BlockKind::Observe {
                    condition: text_condition("second", ObserveMode::CheckNow),
                },
            ),
        ]);
        definition.safety.max_observation_retries = Limit::Finite(0);

        let events = fixture_runtime_with_detector(FakeDetector::returning([true, true]))
            .run(saved(definition), RunMode::DryRun)
            .unwrap();

        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, RunEvent::ObservationCompleted { .. }))
                .count(),
            2
        );
        assert!(matches!(
            events.last(),
            Some(RunEvent::RunStopped {
                reason: StopReason::Completed,
                ..
            })
        ));
    }

    #[test]
    fn macro_wide_observation_rate_paces_independent_conditions() {
        let detector = Arc::new(RecordingDetector::default());
        let runtime = MacroRuntime::new(
            Arc::new(FakeCapture),
            detector.clone(),
            Arc::new(StepClock::default()),
        );
        let mut definition = fixture_definition(vec![
            block(
                "first",
                BlockKind::Observe {
                    condition: text_condition("first", ObserveMode::CheckNow),
                },
            ),
            block(
                "second",
                BlockKind::Observe {
                    condition: text_condition("second", ObserveMode::CheckNow),
                },
            ),
        ]);
        definition.safety.max_observations_per_second = 1;
        definition.safety.max_observation_retries = Limit::Finite(0);

        runtime.run(saved(definition), RunMode::DryRun).unwrap();
        let observed = detector.0.lock().unwrap();

        assert_eq!(observed.len(), 2);
        assert!(observed[1].saturating_sub(observed[0]) >= 1_000);
    }

    #[test]
    fn synchronous_event_collection_stops_at_its_bound() {
        let runtime = MacroRuntime::with_event_capacity(
            Arc::new(FakeCapture),
            Arc::new(FakeDetector::default()),
            Arc::new(FakeClock::default()),
            32,
        );
        let mut definition = fixture_definition(vec![block(
            "continuous",
            BlockKind::Continuous {
                body: vec![point_action("action")],
            },
        )]);
        definition.safety.max_runtime_ms = Limit::Unlimited;
        definition.safety.max_clicks = Limit::Unlimited;

        let events = runtime.run(saved(definition), RunMode::DryRun).unwrap();

        assert!(events.len() <= 32);
        assert!(matches!(
            events.last(),
            Some(RunEvent::RunStopped {
                reason: StopReason::SafetyLimit { message },
                ..
            }) if message.contains("event capacity")
        ));
    }

    #[test]
    fn compiled_snapshot_pins_asset_bytes_and_hashes() {
        let template = fixture_template(17);
        let bytes = png_bytes(template.clone());
        let asset = AssetRef {
            id: "template".to_string(),
            revision: 1,
            content_hash: sha256_hex(&bytes),
        };
        let mut definition = fixture_definition(vec![]);
        definition
            .image_rules
            .push(fixture_image_rule("image", asset.clone(), Some(&template)));
        let mut revision = saved(definition);
        revision.pinned_assets.push(PinnedAsset {
            asset: asset.clone(),
            bytes: bytes.clone(),
        });

        let compiled = CompiledMacro::compile(revision.clone()).unwrap();
        assert_eq!(compiled.pinned_assets[0].asset, asset);
        assert_eq!(compiled.pinned_assets[0].bytes, bytes);

        revision.pinned_assets[0].bytes.push(0);
        assert!(CompiledMacro::compile(revision).is_err());
    }

    #[test]
    fn compiled_snapshot_rejects_conflicting_hashes_for_one_asset_revision() {
        let first_bytes = b"first-template".to_vec();
        let second_bytes = b"second-template".to_vec();
        let first = AssetRef {
            id: "template".to_string(),
            revision: 1,
            content_hash: sha256_hex(&first_bytes),
        };
        let second = AssetRef {
            id: first.id.clone(),
            revision: first.revision,
            content_hash: sha256_hex(&second_bytes),
        };
        let mut definition = fixture_definition(vec![]);
        for (id, template) in [("image-a", first.clone()), ("image-b", second.clone())] {
            definition
                .image_rules
                .push(fixture_image_rule(id, template, None));
        }
        let mut revision = saved(definition);
        revision.pinned_assets = vec![
            PinnedAsset {
                asset: first,
                bytes: first_bytes,
            },
            PinnedAsset {
                asset: second,
                bytes: second_bytes,
            },
        ];
        let revision: SavedRevision =
            serde_json::from_value(serde_json::to_value(revision).unwrap()).unwrap();

        let error = CompiledMacro::compile(revision).unwrap_err().to_string();
        assert!(error.contains("conflicting hashes for immutable asset identity"));
    }

    fn progress_event(attempts: u64) -> RunEvent {
        RunEvent::ObservationProgress {
            sequence: attempts,
            elapsed_ms: attempts,
            run_id: "run".to_string(),
            block_id: "observe".to_string(),
            attempts,
        }
    }

    fn stopped_event() -> RunEvent {
        RunEvent::RunStopped {
            sequence: 3,
            elapsed_ms: 3,
            run_id: "run".to_string(),
            status: RunStatus::Stopped,
            reason: StopReason::Completed,
        }
    }

    fn event_run_id(event: &RunEvent) -> &str {
        match event {
            RunEvent::RunStarted { run_id, .. }
            | RunEvent::StatusChanged { run_id, .. }
            | RunEvent::BlockEntered { run_id, .. }
            | RunEvent::ActionPlanned { run_id, .. }
            | RunEvent::ActionBlocked { run_id, .. }
            | RunEvent::ObservationCompleted { run_id, .. }
            | RunEvent::ConditionEvaluated { run_id, .. }
            | RunEvent::ObservationProgress { run_id, .. }
            | RunEvent::LoopYielded { run_id, .. }
            | RunEvent::Error { run_id, .. }
            | RunEvent::RunStopped { run_id, .. } => run_id,
        }
    }
}
