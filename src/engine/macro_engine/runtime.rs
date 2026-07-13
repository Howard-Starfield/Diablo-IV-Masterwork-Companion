use std::{
    collections::{HashMap, HashSet, VecDeque},
    sync::{
        Arc, Condvar, Mutex, OnceLock,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::{self, Receiver, SyncSender, TrySendError},
    },
    thread,
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
    Action, Block, BlockKind, CandidateEvent, Condition, ConditionDetector, DetectorEvidence,
    DetectorKind, JournalKind, JournalRecord, LatchDecision, Limit, MacroDefinition,
    ObservationRequest, ObservationToken, ObserveMode, PassiveCondition, PinnedAsset, SafetyBypass,
    SavedRevision, TimeoutOutcome, WatchGroup, WatchGroupRunner, if_once_decision,
    observation_satisfies_mode, repeat_n_decision, validate_macro,
};

static NEXT_RUN_ID: AtomicU64 = AtomicU64::new(1);
static NEXT_LIVE_SESSION_ID: AtomicU64 = AtomicU64::new(1);
static NEXT_COMMITTER_OWNER_ID: AtomicU64 = AtomicU64::new(1);
static NEXT_WATCH_ENTRY_ID: AtomicU64 = AtomicU64::new(1);
static NEXT_WATCH_JOB_ID: AtomicU64 = AtomicU64::new(1);
const DEFAULT_SYNCHRONOUS_EVENT_CAPACITY: usize = 4_096;
const FINAL_EVENT_RESERVE: usize = 16;
const MAX_GLOBAL_WATCH_LANES: usize = 256;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct WatchJobKey {
    run_id: String,
    block_id: String,
    entry_id: u64,
    lane_id: String,
}

struct WatchDetectorJob {
    job_id: u64,
    key: WatchJobKey,
    lane_order: usize,
    family: super::DetectorFamily,
    generation: u64,
    side_effect_epoch: u64,
    condition: Condition,
    compiled: CompiledMacro,
    observed_at_ms: u64,
    capture: Arc<super::CapturedCycle>,
    detector: Arc<dyn ConditionDetector>,
    clock: Arc<dyn Clock + Send + Sync>,
    completion: SyncSender<WatchDetectorCompletion>,
}

struct WatchDetectorCompletion {
    job_id: u64,
    key: WatchJobKey,
    lane_order: usize,
    generation: u64,
    side_effect_epoch: u64,
    completed_at_ms: u64,
    capture: Arc<super::CapturedCycle>,
    result: std::result::Result<DetectorEvidence, WatchJobFailure>,
}

#[derive(Debug, thiserror::Error)]
enum WatchJobFailure {
    #[error("detector returned an error: {0}")]
    Detector(#[source] anyhow::Error),
    #[error("Watch detector panicked")]
    DetectorPanicked,
    #[error("Watch completion clock panicked")]
    CompletionClockPanicked,
    #[error("Watch worker iteration panicked")]
    WorkerIterationPanicked,
}

#[derive(Debug, thiserror::Error)]
#[error("Watch detector pool is unavailable: {message}")]
struct WatchPoolUnavailable {
    message: String,
}

struct PendingWatchJob {
    newest: WatchDetectorJob,
    enqueue_sequence: u64,
}

#[derive(Default)]
struct WatchPoolState {
    active: HashMap<WatchJobKey, (u64, super::DetectorFamily)>,
    pending: HashMap<WatchJobKey, PendingWatchJob>,
    cleanups: HashMap<String, WatchRunCleanup>,
    run_failures: HashMap<String, String>,
    global_failure: Option<String>,
    cleanup_failures: VecDeque<String>,
}

struct WatchRunCleanup {
    detector: Arc<dyn ConditionDetector>,
    generations: Vec<u64>,
}

struct WatchPoolInner {
    state: Mutex<WatchPoolState>,
    ready: Condvar,
    started_workers: AtomicU64,
    next_enqueue_sequence: AtomicU64,
    live_text_workers: AtomicU64,
    live_image_workers: AtomicU64,
    enforce_health: bool,
}

struct WatchDetectorPool {
    inner: Arc<WatchPoolInner>,
}

struct WatchScopeCleanup {
    pool: &'static WatchDetectorPool,
    run_id: String,
    entry_id: u64,
}

impl Drop for WatchScopeCleanup {
    fn drop(&mut self) {
        self.pool.cancel_scope(&self.run_id, self.entry_id);
    }
}

impl WatchDetectorPool {
    fn global() -> &'static Self {
        static POOL: OnceLock<WatchDetectorPool> = OnceLock::new();
        POOL.get_or_init(|| {
            let inner = Arc::new(WatchPoolInner {
                state: Mutex::new(WatchPoolState::default()),
                ready: Condvar::new(),
                started_workers: AtomicU64::new(0),
                next_enqueue_sequence: AtomicU64::new(1),
                live_text_workers: AtomicU64::new(0),
                live_image_workers: AtomicU64::new(0),
                enforce_health: true,
            });
            spawn_watch_worker(Arc::clone(&inner), super::DetectorFamily::Text);
            spawn_watch_worker(Arc::clone(&inner), super::DetectorFamily::Image);
            spawn_watch_worker(Arc::clone(&inner), super::DetectorFamily::Image);
            Self { inner }
        })
    }

    fn submit(
        &self,
        job: WatchDetectorJob,
    ) -> std::result::Result<super::SubmitOutcome, WatchPoolUnavailable> {
        let mut state = lock_watch_pool_state(&self.inner);
        if let Some(message) = self.health_failure_locked(&state, job.family, &job.key.run_id) {
            return Err(WatchPoolUnavailable { message });
        }
        if let Some(pending) = state.pending.get_mut(&job.key) {
            let dropped_frame_id = pending.newest.capture.frame_id();
            pending.newest = job;
            return Ok(super::SubmitOutcome::ReplacedPending { dropped_frame_id });
        }
        let active_same_lane = state.active.contains_key(&job.key);
        let known_lanes = state.active.len().saturating_add(state.pending.len());
        if !active_same_lane && known_lanes >= MAX_GLOBAL_WATCH_LANES {
            return Ok(super::SubmitOutcome::PollingDelayed);
        }
        let active_family = state
            .active
            .values()
            .filter(|(_, active_family)| *active_family == job.family)
            .count();
        let queued_family = state
            .pending
            .values()
            .filter(|pending| pending.newest.family == job.family)
            .count();
        let capacity = match job.family {
            super::DetectorFamily::Text => 1,
            super::DetectorFamily::Image => 2,
        };
        let outcome = if active_same_lane || active_family.saturating_add(queued_family) >= capacity
        {
            super::SubmitOutcome::Pending
        } else {
            super::SubmitOutcome::Started
        };
        let enqueue_sequence = self
            .inner
            .next_enqueue_sequence
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |sequence| {
                sequence.checked_add(1)
            })
            .map_err(|_| {
                let message = "global enqueue sequence exhausted".to_string();
                state.global_failure = Some(message.clone());
                WatchPoolUnavailable { message }
            })?;
        state.pending.insert(
            job.key.clone(),
            PendingWatchJob {
                newest: job,
                enqueue_sequence,
            },
        );
        drop(state);
        self.inner.ready.notify_all();
        Ok(outcome)
    }

    fn health_failure_locked(
        &self,
        state: &WatchPoolState,
        family: super::DetectorFamily,
        run_id: &str,
    ) -> Option<String> {
        state
            .global_failure
            .clone()
            .or_else(|| state.run_failures.get(run_id).cloned())
            .or_else(|| {
                if !self.inner.enforce_health {
                    return None;
                }
                let (live, expected) = match family {
                    super::DetectorFamily::Text => {
                        (self.inner.live_text_workers.load(Ordering::Acquire), 1)
                    }
                    super::DetectorFamily::Image => {
                        (self.inner.live_image_workers.load(Ordering::Acquire), 2)
                    }
                };
                (live < expected)
                    .then(|| format!("{family:?} worker topology degraded: {live}/{expected} live"))
            })
    }

    fn failure_for_run(&self, run_id: &str) -> Option<String> {
        let state = lock_watch_pool_state(&self.inner);
        state
            .global_failure
            .clone()
            .or_else(|| state.run_failures.get(run_id).cloned())
    }

    fn cancel_scope(&self, run_id: &str, entry_id: u64) {
        lock_watch_pool_state(&self.inner)
            .pending
            .retain(|key, _| key.run_id != run_id || key.entry_id != entry_id);
    }

    fn cancel_old_epoch(&self, run_id: &str, current_epoch: u64) {
        lock_watch_pool_state(&self.inner)
            .pending
            .retain(|key, pending| {
                key.run_id != run_id || pending.newest.side_effect_epoch == current_epoch
            });
    }

    fn cancel_run(&self, run_id: &str) {
        let mut state = lock_watch_pool_state(&self.inner);
        state.pending.retain(|key, _| key.run_id != run_id);
        state.run_failures.remove(run_id);
    }

    fn finish_run(
        &self,
        run_id: &str,
        detector: Arc<dyn ConditionDetector>,
        generations: Vec<u64>,
    ) {
        let cleanup = {
            let mut state = lock_watch_pool_state(&self.inner);
            let still_active = state.active.keys().any(|key| key.run_id == run_id)
                || state.pending.keys().any(|key| key.run_id == run_id);
            if still_active {
                state.cleanups.insert(
                    run_id.to_string(),
                    WatchRunCleanup {
                        detector,
                        generations,
                    },
                );
                None
            } else {
                Some(WatchRunCleanup {
                    detector,
                    generations,
                })
            }
        };
        if let Some(cleanup) = cleanup {
            run_cleanup_contained(&self.inner, run_id, cleanup);
        }
    }

    #[cfg(test)]
    fn worker_count(&self) -> usize {
        usize::try_from(self.inner.started_workers.load(Ordering::Acquire)).unwrap_or(usize::MAX)
    }
}

fn spawn_watch_worker(inner: Arc<WatchPoolInner>, family: super::DetectorFamily) {
    let worker_inner = Arc::clone(&inner);
    let (ready_tx, ready_rx) = mpsc::sync_channel(1);
    thread::Builder::new()
        .name(match family {
            super::DetectorFamily::Text => "macro-watch-ocr".to_string(),
            super::DetectorFamily::Image => "macro-watch-image".to_string(),
        })
        .spawn(move || {
            worker_live_counter(&worker_inner, family).fetch_add(1, Ordering::Release);
            let _liveness = WorkerLivenessGuard {
                inner: Arc::clone(&worker_inner),
                family,
            };
            let _ = ready_tx.send(());
            let escaped = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                watch_worker_loop(worker_inner, family)
            }));
            if escaped.is_err() {
                record_global_pool_failure(
                    &_liveness.inner,
                    format!("{family:?} Watch worker exited after an unexpected panic"),
                );
            }
        })
        .expect("failed to start fixed macro Watch worker");
    ready_rx
        .recv()
        .expect("fixed macro Watch worker exited before readiness");
    inner.started_workers.fetch_add(1, Ordering::Release);
}

fn worker_live_counter(inner: &WatchPoolInner, family: super::DetectorFamily) -> &AtomicU64 {
    match family {
        super::DetectorFamily::Text => &inner.live_text_workers,
        super::DetectorFamily::Image => &inner.live_image_workers,
    }
}

struct WorkerLivenessGuard {
    inner: Arc<WatchPoolInner>,
    family: super::DetectorFamily,
}

impl Drop for WorkerLivenessGuard {
    fn drop(&mut self) {
        worker_live_counter(&self.inner, self.family).fetch_sub(1, Ordering::AcqRel);
        self.inner.ready.notify_all();
    }
}

struct ActiveWatchJobLease {
    inner: Arc<WatchPoolInner>,
    key: WatchJobKey,
    job_id: u64,
}

impl Drop for ActiveWatchJobLease {
    fn drop(&mut self) {
        let cleanup = {
            let mut state = lock_watch_pool_state(&self.inner);
            if state.active.get(&self.key).map(|(job_id, _)| *job_id) == Some(self.job_id) {
                state.active.remove(&self.key);
            }
            let run_still_active = state.active.keys().any(|key| key.run_id == self.key.run_id)
                || state
                    .pending
                    .keys()
                    .any(|key| key.run_id == self.key.run_id);
            if run_still_active {
                None
            } else {
                state.cleanups.remove(&self.key.run_id)
            }
        };
        self.inner.ready.notify_all();
        if let Some(cleanup) = cleanup {
            run_cleanup_contained(&self.inner, &self.key.run_id, cleanup);
        }
    }
}

fn lock_watch_pool_state(inner: &WatchPoolInner) -> std::sync::MutexGuard<'_, WatchPoolState> {
    inner
        .state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn record_global_pool_failure(inner: &WatchPoolInner, message: String) {
    let mut state = lock_watch_pool_state(inner);
    state.global_failure.get_or_insert(message);
    drop(state);
    inner.ready.notify_all();
}

fn record_run_pool_failure(inner: &WatchPoolInner, run_id: &str, message: String) {
    let mut state = lock_watch_pool_state(inner);
    if state.run_failures.len() < MAX_GLOBAL_WATCH_LANES || state.run_failures.contains_key(run_id)
    {
        state
            .run_failures
            .entry(run_id.to_string())
            .or_insert(message);
    } else {
        state.global_failure = Some("Watch run failure capacity exhausted".to_string());
    }
    drop(state);
    inner.ready.notify_all();
}

fn run_cleanup_contained(inner: &WatchPoolInner, run_id: &str, cleanup: WatchRunCleanup) {
    if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        cleanup.detector.run_finished(run_id, &cleanup.generations)
    }))
    .is_err()
    {
        let mut state = lock_watch_pool_state(inner);
        if state.cleanup_failures.len() == 64 {
            state.cleanup_failures.pop_front();
        }
        state
            .cleanup_failures
            .push_back(format!("detector cleanup panicked for run {run_id}"));
    }
}

fn oldest_pending_key(
    state: &WatchPoolState,
    family: super::DetectorFamily,
) -> Option<WatchJobKey> {
    state
        .pending
        .iter()
        .filter(|(key, pending)| {
            pending.newest.family == family && !state.active.contains_key(*key)
        })
        .min_by(|(left_key, left), (right_key, right)| {
            left.enqueue_sequence
                .cmp(&right.enqueue_sequence)
                .then_with(|| left_key.run_id.cmp(&right_key.run_id))
                .then_with(|| left_key.entry_id.cmp(&right_key.entry_id))
                .then_with(|| left_key.lane_id.cmp(&right_key.lane_id))
        })
        .map(|(key, _)| key.clone())
}

fn watch_worker_loop(inner: Arc<WatchPoolInner>, family: super::DetectorFamily) {
    loop {
        let job = {
            let mut state = lock_watch_pool_state(&inner);
            loop {
                if state.global_failure.is_some() {
                    state = inner
                        .ready
                        .wait(state)
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    continue;
                }
                let next_key = oldest_pending_key(&state, family);
                if let Some(key) = next_key {
                    let pending = state
                        .pending
                        .remove(&key)
                        .expect("selected job disappeared");
                    state
                        .active
                        .insert(key, (pending.newest.job_id, pending.newest.family));
                    break pending.newest;
                }
                state = inner
                    .ready
                    .wait(state)
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
            }
        };

        let _lease = ActiveWatchJobLease {
            inner: Arc::clone(&inner),
            key: job.key.clone(),
            job_id: job.job_id,
        };

        let completion = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            build_watch_completion(&job)
        }))
        .unwrap_or_else(|_| {
            failed_watch_completion(&job, WatchJobFailure::WorkerIterationPanicked)
        });
        let run_id = job.key.run_id.clone();
        let send = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            match job.completion.try_send(completion) {
                Ok(()) => 0_u8,
                Err(TrySendError::Full(_)) => 1,
                Err(TrySendError::Disconnected(_)) => 2,
            }
        }));
        match send {
            Ok(1) => record_run_pool_failure(
                &inner,
                &run_id,
                "Watch completion channel remained full".to_string(),
            ),
            Err(_) => record_run_pool_failure(
                &inner,
                &run_id,
                "Watch completion dispatch panicked".to_string(),
            ),
            Ok(0 | 2) => {}
            Ok(_) => unreachable!("unknown Watch completion send status"),
        }
    }
}

fn build_watch_completion(job: &WatchDetectorJob) -> WatchDetectorCompletion {
    let request = ObservationRequest {
        run_id: &job.key.run_id,
        generation: job.generation,
        side_effect_epoch: job.side_effect_epoch,
        condition: &job.condition,
        compiled: &job.compiled,
        observed_at_ms: job.observed_at_ms,
    };
    let observation = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        job.detector.observe(&request, job.capture.as_ref())
    }));
    let (result, completed_at_ms) = match observation {
        Ok(Ok(evidence)) => {
            match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| job.clock.now_ms())) {
                Ok(completed_at_ms) => (Ok(evidence), completed_at_ms),
                Err(_) => (
                    Err(WatchJobFailure::CompletionClockPanicked),
                    job.observed_at_ms,
                ),
            }
        }
        Ok(Err(error)) => (Err(WatchJobFailure::Detector(error)), job.observed_at_ms),
        Err(_) => (Err(WatchJobFailure::DetectorPanicked), job.observed_at_ms),
    };
    WatchDetectorCompletion {
        job_id: job.job_id,
        key: job.key.clone(),
        lane_order: job.lane_order,
        generation: job.generation,
        side_effect_epoch: job.side_effect_epoch,
        completed_at_ms,
        capture: Arc::clone(&job.capture),
        result,
    }
}

fn failed_watch_completion(
    job: &WatchDetectorJob,
    failure: WatchJobFailure,
) -> WatchDetectorCompletion {
    WatchDetectorCompletion {
        job_id: job.job_id,
        key: job.key.clone(),
        lane_order: job.lane_order,
        generation: job.generation,
        side_effect_epoch: job.side_effect_epoch,
        completed_at_ms: job.observed_at_ms,
        capture: Arc::clone(&job.capture),
        result: Err(failure),
    }
}

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
    registered_committer_run: Mutex<Option<String>>,
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
            registered_committer_run: Mutex::new(None),
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

    fn register_committer_run(
        &self,
        run_id: &str,
    ) -> std::result::Result<(), ActionCommitterCreateError> {
        let mut registered = self
            .registered_committer_run
            .lock()
            .expect("live committer registry poisoned");
        if let Some(registered_run_id) = registered.as_ref() {
            return Err(ActionCommitterCreateError::RunAlreadyRegistered {
                run_id: registered_run_id.clone(),
            });
        }
        *registered = Some(run_id.to_string());
        Ok(())
    }

    fn release_committer_run(&self, run_id: &str) {
        let mut registered = self
            .registered_committer_run
            .lock()
            .expect("live committer registry poisoned");
        if registered.as_deref() == Some(run_id) {
            *registered = None;
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

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ActionCommitterCreateError {
    #[error("action attempt ledger capacity must be positive")]
    ZeroAttemptCapacity,
    #[error("run {run_id:?} already has an action committer in this live session")]
    RunAlreadyRegistered { run_id: String },
}

impl ActionCommitter {
    pub fn new(
        session: Arc<LiveActionSession>,
        clock: Arc<dyn Clock + Send + Sync>,
        run_id: impl Into<String>,
        maximum_clicks: Limit<u64>,
        maximum_attempts: usize,
    ) -> std::result::Result<Self, ActionCommitterCreateError> {
        if maximum_attempts == 0 {
            return Err(ActionCommitterCreateError::ZeroAttemptCapacity);
        }
        let run_id = run_id.into();
        session.register_committer_run(&run_id)?;
        Ok(Self {
            owner_id: NEXT_COMMITTER_OWNER_ID.fetch_add(1, Ordering::Relaxed),
            session,
            clock,
            ledger: Arc::new(Mutex::new(CommitLedger {
                run_id,
                maximum_clicks,
                maximum_attempts,
                committed_clicks: 0,
                last_click_at_ms: None,
                active: None,
                attempts: HashMap::new(),
                finished: false,
            })),
        })
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
        let run_id = {
            let mut ledger = self.ledger.lock().expect("action attempt ledger poisoned");
            if ledger.active.is_some() {
                return Err(BlockReason::ActionLockBusy);
            }
            if ledger.finished {
                return Ok(());
            }
            ledger.attempts.clear();
            ledger.finished = true;
            ledger.run_id.clone()
        };
        self.session.release_committer_run(&run_id);
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
        && frame.process_id == authorization.expected_target.process_id
        && frame.process_started_at_100ns == authorization.expected_target.process_started_at_100ns
        && frame.client_x == authorization.expected_target.client_rect.x
        && frame.client_y == authorization.expected_target.client_rect.y
        && frame.client_width == authorization.expected_target.client_rect.width
        && frame.client_height == authorization.expected_target.client_rect.height
        && frame.geometry_revision == authorization.expected_target.geometry_revision
        && frame.display_profile_revision == authorization.expected_target.display_profile_revision
        && frame.dpi == authorization.expected_target.dpi
        && frame.is_visible == authorization.expected_target.is_visible
        && frame.is_minimized == authorization.expected_target.is_minimized
        && frame.is_foreground == authorization.expected_target.is_foreground
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
    ArbitrationCompleted {
        sequence: u64,
        elapsed_ms: u64,
        run_id: String,
        block_id: String,
        winner_lane_id: Option<String>,
        discarded_lane_ids: Vec<String>,
        safety_bypassed: bool,
    },
    PollingDelayed {
        sequence: u64,
        elapsed_ms: u64,
        run_id: String,
        block_id: String,
        lane_id: String,
        delayed_polls: u64,
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
            | Self::ArbitrationCompleted { sequence, .. }
            | Self::PollingDelayed { sequence, .. }
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
            | Self::ArbitrationCompleted { elapsed_ms, .. }
            | Self::PollingDelayed { elapsed_ms, .. }
            | Self::Error { elapsed_ms, .. }
            | Self::RunStopped { elapsed_ms, .. } => *elapsed_ms,
        }
    }

    fn is_progress(&self) -> bool {
        matches!(
            self,
            Self::ObservationProgress { .. } | Self::PollingDelayed { .. }
        )
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
            RunEvent::ArbitrationCompleted { .. } => {
                (JournalKind::Arbitration, "watch arbitration completed")
            }
            RunEvent::PollingDelayed { .. } => (JournalKind::Aggregate, "polling delayed"),
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
    anyhow::ensure!(frame.process_id == target.process_id, "frame PID changed");
    anyhow::ensure!(
        frame.process_started_at_100ns == target.process_started_at_100ns,
        "frame process identity changed"
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
        frame.is_visible == target.is_visible,
        "frame visibility changed"
    );
    anyhow::ensure!(
        frame.is_minimized == target.is_minimized,
        "frame minimized state changed"
    );
    anyhow::ensure!(
        frame.is_foreground == target.is_foreground,
        "frame foreground state changed"
    );
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
    watch_pool: &'static WatchDetectorPool,
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
            watch_pool: self.watch_pool,
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
            watch_pool: WatchDetectorPool::global(),
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
            side_effect_epoch: 0,
            last_observation_at_ms: None,
            non_authoritative_planned_clicks: 0,
            paused_event_emitted: false,
            detector_generations: HashSet::new(),
            watch_groups: HashMap::new(),
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
        self.watch_pool.cancel_run(emitter.run_id());
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
        self.watch_pool.finish_run(
            emitter.run_id(),
            Arc::clone(&self.detector),
            detector_generations,
        );
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
    side_effect_epoch: u64,
    last_observation_at_ms: Option<u64>,
    /// Simulation-only count of click actions emitted as planned during this run.
    /// It is deliberately separate from, and cannot mutate, `ActionCommitter`'s live ledger.
    non_authoritative_planned_clicks: u64,
    paused_event_emitted: bool,
    detector_generations: HashSet<u64>,
    watch_groups: HashMap<String, WatchGroupRunner>,
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
            BlockKind::WatchGroup { group } => self.execute_watch_group(&block.id, group),
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

    fn execute_watch_group(&mut self, block_id: &str, group: &WatchGroup) -> Option<StopReason> {
        let initial_generation = self.runtime.control_handle().generation();
        let mut runner = self.watch_groups.remove(block_id).unwrap_or_else(|| {
            WatchGroupRunner::new(
                self.emitter.run_id(),
                initial_generation,
                group.lanes.len().max(1),
            )
        });
        runner.invalidate(initial_generation);
        let started_at = self.runtime.clock.now_ms();
        let deadline = match group.timeout_ms {
            Limit::Finite(timeout_ms) => Some(started_at.saturating_add(timeout_ms)),
            Limit::Unlimited => None,
        };
        let entry_id = NEXT_WATCH_ENTRY_ID.fetch_add(1, Ordering::Relaxed);
        let _scope_cleanup = WatchScopeCleanup {
            pool: self.runtime.watch_pool,
            run_id: self.emitter.run_id().to_string(),
            entry_id,
        };
        let (completion_tx, completion_rx) = mpsc::sync_channel::<WatchDetectorCompletion>(3);
        let mut candidate_cycles: HashMap<String, Arc<super::CapturedCycle>> = HashMap::new();
        let mut completed_job_ids: HashMap<String, u64> = HashMap::new();
        let mut arbitration_deadline = None;
        let mut next_poll_at = started_at;
        let mut runner_generation = initial_generation;
        let mut lane_due_at: HashMap<String, u64> = group
            .enabled_lanes_in_priority_order()
            .map(|(_, lane)| (lane.id.clone(), started_at))
            .collect();
        let mut attempts = 0_u64;

        loop {
            if let Some(message) = self
                .runtime
                .watch_pool
                .failure_for_run(self.emitter.run_id())
            {
                self.watch_groups.insert(block_id.to_string(), runner);
                return Some(StopReason::TechnicalFailure { message });
            }
            if let Some(reason) = self.check_control() {
                self.runtime
                    .watch_pool
                    .cancel_scope(self.emitter.run_id(), entry_id);
                let arbitration = runner.arbitrate(Some(safety_bypass_for_stop(&reason)));
                self.emitter.arbitration_completed(
                    block_id,
                    None,
                    arbitration.discarded_lane_ids,
                    true,
                );
                self.watch_groups.insert(block_id.to_string(), runner);
                return Some(reason);
            }
            let now = self.runtime.clock.now_ms();
            let arbitration_is_due =
                arbitration_deadline.is_some_and(|arbitration_at| now >= arbitration_at);
            if !arbitration_is_due && deadline.is_some_and(|deadline| now >= deadline) {
                self.runtime
                    .watch_pool
                    .cancel_scope(self.emitter.run_id(), entry_id);
                return self.resolve_watch_timeout(block_id, group, runner);
            }
            let generation = self.runtime.control_handle().generation();
            if generation != runner_generation {
                runner.invalidate(generation);
                runner_generation = generation;
                arbitration_deadline = None;
                candidate_cycles.clear();
                completed_job_ids.clear();
                self.observations.clear();
                self.runtime
                    .watch_pool
                    .cancel_scope(self.emitter.run_id(), entry_id);
                next_poll_at = now;
                for due_at in lane_due_at.values_mut() {
                    *due_at = now;
                }
            }

            while let Ok(completion) = completion_rx.try_recv() {
                if completion.key.run_id != self.emitter.run_id()
                    || completion.key.block_id != block_id
                    || completion.key.entry_id != entry_id
                    || completion.generation != generation
                    || completion.side_effect_epoch != self.side_effect_epoch
                    || completed_job_ids.get(&completion.key.lane_id) == Some(&completion.job_id)
                {
                    continue;
                }
                completed_job_ids.insert(completion.key.lane_id.clone(), completion.job_id);
                if deadline.is_some_and(|deadline| completion.completed_at_ms >= deadline) {
                    continue;
                }
                if let Some(reason) = self.check_control() {
                    self.runtime
                        .watch_pool
                        .cancel_scope(self.emitter.run_id(), entry_id);
                    self.watch_groups.insert(block_id.to_string(), runner);
                    return Some(reason);
                }
                let lane = &group.lanes[completion.lane_order];
                let evidence = match completion.result {
                    Ok(evidence) => evidence,
                    Err(error) => {
                        if matches!(
                            &error,
                            WatchJobFailure::Detector(source)
                                if source.downcast_ref::<crate::engine::automation::StaleCapturedFrameError>().is_some()
                        ) {
                            let arbitration =
                                runner.arbitrate(Some(SafetyBypass::TargetInvalidated));
                            self.emitter.arbitration_completed(
                                block_id,
                                None,
                                arbitration.discarded_lane_ids,
                                true,
                            );
                        }
                        self.watch_groups.insert(block_id.to_string(), runner);
                        return Some(StopReason::TechnicalFailure {
                            message: format!(
                                "detector failed for Watch lane '{}': {error}",
                                completion.key.lane_id
                            ),
                        });
                    }
                };
                let condition = passive_condition(&lane.condition);
                let token = if evidence.matched {
                    match self.make_token(&condition, &evidence, generation) {
                        Ok(token) => {
                            let Some(frame) = token.frame_metadata else {
                                self.watch_groups.insert(block_id.to_string(), runner);
                                return Some(StopReason::TechnicalFailure {
                                    message: format!(
                                        "matched Watch lane '{}' has no atomic frame provenance",
                                        completion.key.lane_id
                                    ),
                                });
                            };
                            if !frame_matches_capture(frame, completion.capture.metadata())
                                || frame.frame_id != completion.capture.frame_id()
                                || frame.frame_id != token.frame_id
                                || frame.captured_at_ms != token.captured_at_ms
                                || frame.region_revision != token.region_revision
                                || frame.rule_revision != token.rule_revision
                            {
                                self.watch_groups.insert(block_id.to_string(), runner);
                                return Some(StopReason::TechnicalFailure {
                                    message: format!(
                                        "matched Watch lane '{}' returned inconsistent frame provenance",
                                        completion.key.lane_id
                                    ),
                                });
                            }
                            Some(token)
                        }
                        Err(message) => {
                            self.watch_groups.insert(block_id.to_string(), runner);
                            return Some(StopReason::TechnicalFailure { message });
                        }
                    }
                } else {
                    None
                };
                self.emitter.observation_completed(
                    &completion.key.lane_id,
                    evidence.clone(),
                    token.clone(),
                );
                self.emitter
                    .condition_evaluated(&completion.key.lane_id, evidence.matched);

                let latch = runner.observe_latch(
                    &completion.key.lane_id,
                    evidence.matched,
                    evidence.frame_id,
                );
                if matches!(latch, LatchDecision::Qualified) {
                    let candidate = token.as_ref().map(|token| {
                        CandidateEvent::from_observation(
                            &completion.key.lane_id,
                            completion.lane_order,
                            completion.completed_at_ms,
                            token,
                        )
                    });
                    if let Some(token) = token {
                        self.observations
                            .insert(completion.key.lane_id.clone(), token);
                    }
                    let Some(candidate) = candidate else {
                        self.watch_groups.insert(block_id.to_string(), runner);
                        return Some(StopReason::TechnicalFailure {
                            message: format!(
                                "qualified Watch lane '{}' has no evidence token",
                                completion.key.lane_id
                            ),
                        });
                    };
                    if let Err(message) = runner.qualify_preobserved(candidate) {
                        self.watch_groups.insert(block_id.to_string(), runner);
                        return Some(StopReason::TechnicalFailure {
                            message: format!(
                                "Watch lane '{}' candidate rejected: {message}",
                                completion.key.lane_id
                            ),
                        });
                    }
                    candidate_cycles.insert(completion.key.lane_id.clone(), completion.capture);
                    let candidate_deadline = completion
                        .completed_at_ms
                        .saturating_add(super::ARBITRATION_WINDOW_MS);
                    arbitration_deadline = establish_arbitration_deadline(
                        arbitration_deadline,
                        candidate_deadline,
                        deadline,
                    );
                } else if matches!(latch, LatchDecision::Rearmed | LatchDecision::Unmatched) {
                    self.observations.remove(&completion.key.lane_id);
                    candidate_cycles.remove(&completion.key.lane_id);
                    runner.revoke_candidate(&completion.key.lane_id);
                    if !runner.has_candidates() {
                        arbitration_deadline = None;
                    }
                }
            }

            let now = self.runtime.clock.now_ms();
            if arbitration_deadline.is_some_and(|deadline| now >= deadline) {
                self.runtime
                    .watch_pool
                    .cancel_scope(self.emitter.run_id(), entry_id);
                let arbitration = runner.arbitrate(None);
                let Some(winner) = arbitration.winner.clone() else {
                    self.watch_groups.insert(block_id.to_string(), runner);
                    return Some(StopReason::TechnicalFailure {
                        message: format!("Watch Group '{block_id}' arbitration lost its candidate"),
                    });
                };
                if let Some(reason) = self.check_control() {
                    self.observations.clear();
                    self.emitter.arbitration_completed(
                        block_id,
                        None,
                        std::iter::once(winner.lane_id)
                            .chain(arbitration.discarded_lane_ids)
                            .collect(),
                        true,
                    );
                    runner.invalidate(self.runtime.control_handle().generation());
                    self.watch_groups.insert(block_id.to_string(), runner);
                    return Some(reason);
                }
                let fresh_winner = self
                    .observations
                    .get(&winner.lane_id)
                    .is_some_and(|token| winner.matches_observation(token))
                    && winner.token.generation == self.runtime.control_handle().generation()
                    && winner.token.side_effect_epoch == self.side_effect_epoch;
                if !fresh_winner {
                    self.observations.clear();
                    runner.invalidate(self.runtime.control_handle().generation());
                    self.watch_groups.insert(block_id.to_string(), runner);
                    return Some(StopReason::TechnicalFailure {
                        message: format!(
                            "Watch lane '{}' candidate became stale before commit",
                            winner.lane_id
                        ),
                    });
                }
                let Some(winner_cycle) = candidate_cycles.remove(&winner.lane_id) else {
                    self.watch_groups.insert(block_id.to_string(), runner);
                    return Some(StopReason::TechnicalFailure {
                        message: format!(
                            "Watch lane '{}' lost its capture provenance",
                            winner.lane_id
                        ),
                    });
                };
                if let Err(error) = winner_cycle.validate_fresh() {
                    self.observations.clear();
                    runner.invalidate(self.runtime.control_handle().generation());
                    self.emitter.arbitration_completed(
                        block_id,
                        None,
                        std::iter::once(winner.lane_id)
                            .chain(arbitration.discarded_lane_ids)
                            .collect(),
                        true,
                    );
                    self.watch_groups.insert(block_id.to_string(), runner);
                    return Some(StopReason::TechnicalFailure {
                        message: format!("Watch target changed before winner body: {error}"),
                    });
                }
                if let Some(reason) = self.check_control() {
                    candidate_cycles.clear();
                    self.observations.clear();
                    self.emitter.arbitration_completed(
                        block_id,
                        None,
                        std::iter::once(winner.lane_id)
                            .chain(arbitration.discarded_lane_ids)
                            .collect(),
                        true,
                    );
                    runner.invalidate(self.runtime.control_handle().generation());
                    self.watch_groups.insert(block_id.to_string(), runner);
                    return Some(reason);
                }
                if deadline.is_some_and(|deadline| self.runtime.clock.now_ms() >= deadline) {
                    drop(winner_cycle);
                    candidate_cycles.clear();
                    self.observations.clear();
                    return self.resolve_watch_timeout(block_id, group, runner);
                }
                drop(winner_cycle);
                candidate_cycles.clear();
                self.emitter.arbitration_completed(
                    block_id,
                    Some(winner.lane_id.clone()),
                    arbitration.discarded_lane_ids.clone(),
                    false,
                );
                let losing: HashSet<_> = arbitration.discarded_lane_ids.iter().collect();
                self.observations
                    .retain(|source, _| !losing.contains(source));
                let winner_body = group
                    .lanes
                    .iter()
                    .find(|lane| lane.id == winner.lane_id)
                    .map(|lane| lane.then_body.clone())
                    .unwrap_or_default();
                runner.begin_execution();
                let body_reason = self.execute_blocks(&winner_body);
                runner.settle_and_exit();
                self.observations.clear();
                self.watch_groups.insert(block_id.to_string(), runner);
                if let Some(reason) = body_reason {
                    return Some(reason);
                }
                return self.cooperative_wait(group.cooldown_ms);
            }

            if arbitration_deadline.is_none() && now >= next_poll_at {
                attempts = attempts.saturating_add(1);
                if exceeds_limit(
                    attempts.saturating_sub(1),
                    &self.compiled.definition().safety.max_observation_retries,
                ) {
                    self.runtime
                        .watch_pool
                        .cancel_scope(self.emitter.run_id(), entry_id);
                    self.watch_groups.insert(block_id.to_string(), runner);
                    return Some(StopReason::SafetyLimit {
                        message: format!(
                            "observation retry limit exceeded in Watch Group '{block_id}'"
                        ),
                    });
                }
                if deadline.is_some_and(|deadline| self.runtime.clock.now_ms() >= deadline) {
                    self.runtime
                        .watch_pool
                        .cancel_scope(self.emitter.run_id(), entry_id);
                    return self.resolve_watch_timeout(block_id, group, runner);
                }
                let lane_schedule: Vec<_> = group
                    .enabled_lanes_in_priority_order()
                    .filter(|(_, lane)| {
                        lane_due_at
                            .get(&lane.id)
                            .is_some_and(|due_at| *due_at <= now)
                    })
                    .map(|(order, _)| order)
                    .collect();
                if lane_schedule.is_empty() {
                    next_poll_at = lane_due_at.values().copied().min().unwrap_or(now + 1);
                    self.runtime.emergency_stop.wait(Duration::from_millis(1));
                    continue;
                }
                let capture_regions = match lane_schedule
                    .iter()
                    .map(|lane_order| self.watch_capture_rect(&group.lanes[*lane_order].condition))
                    .collect::<std::result::Result<Vec<_>, _>>()
                {
                    Ok(regions) => regions,
                    Err(message) => {
                        self.watch_groups.insert(block_id.to_string(), runner);
                        return Some(StopReason::TechnicalFailure { message });
                    }
                };
                let cycle = match super::CapturedCycle::capture(
                    Arc::clone(&self.runtime.capture),
                    &capture_regions,
                ) {
                    Ok(cycle) => cycle,
                    Err(error) => {
                        let arbitration = runner.arbitrate(Some(SafetyBypass::TargetInvalidated));
                        self.emitter.arbitration_completed(
                            block_id,
                            None,
                            arbitration.discarded_lane_ids,
                            true,
                        );
                        self.watch_groups.insert(block_id.to_string(), runner);
                        return Some(StopReason::TechnicalFailure {
                            message: format!(
                                "capture failed for Watch Group '{block_id}': {error}"
                            ),
                        });
                    }
                };
                let after_capture = self.runtime.clock.now_ms();
                if deadline.is_some_and(|deadline| after_capture >= deadline) {
                    self.runtime
                        .watch_pool
                        .cancel_scope(self.emitter.run_id(), entry_id);
                    return self.resolve_watch_timeout(block_id, group, runner);
                }
                for lane_order in lane_schedule {
                    if let Some(reason) = self.check_control() {
                        self.runtime
                            .watch_pool
                            .cancel_scope(self.emitter.run_id(), entry_id);
                        self.watch_groups.insert(block_id.to_string(), runner);
                        return Some(reason);
                    }
                    let dispatch_at = self.runtime.clock.now_ms();
                    if deadline.is_some_and(|deadline| dispatch_at >= deadline) {
                        self.runtime
                            .watch_pool
                            .cancel_scope(self.emitter.run_id(), entry_id);
                        return self.resolve_watch_timeout(block_id, group, runner);
                    }
                    let lane = &group.lanes[lane_order];
                    let family = match lane.condition {
                        PassiveCondition::Text { .. } => super::DetectorFamily::Text,
                        PassiveCondition::Image { .. } => super::DetectorFamily::Image,
                    };
                    let job_id = NEXT_WATCH_JOB_ID.fetch_add(1, Ordering::Relaxed);
                    self.detector_generations.insert(generation);
                    self.last_observation_at_ms = Some(dispatch_at);
                    let outcome = match self.runtime.watch_pool.submit(WatchDetectorJob {
                        job_id,
                        key: WatchJobKey {
                            run_id: self.emitter.run_id().to_string(),
                            block_id: block_id.to_string(),
                            entry_id,
                            lane_id: lane.id.clone(),
                        },
                        lane_order,
                        family,
                        generation,
                        side_effect_epoch: self.side_effect_epoch,
                        condition: passive_condition(&lane.condition),
                        compiled: self.compiled.clone(),
                        observed_at_ms: dispatch_at,
                        capture: Arc::clone(&cycle),
                        detector: Arc::clone(&self.runtime.detector),
                        clock: Arc::clone(&self.runtime.clock),
                        completion: completion_tx.clone(),
                    }) {
                        Ok(outcome) => outcome,
                        Err(error) => {
                            self.watch_groups.insert(block_id.to_string(), runner);
                            return Some(StopReason::TechnicalFailure {
                                message: error.to_string(),
                            });
                        }
                    };
                    if !matches!(outcome, super::SubmitOutcome::Started) {
                        self.emitter.polling_delayed(block_id, &lane.id, attempts);
                    }
                    lane_due_at.insert(
                        lane.id.clone(),
                        dispatch_at.saturating_add(self.watch_poll_interval_ms(&lane.condition)),
                    );
                }
                self.emitter.observation_progress(block_id, attempts);
                next_poll_at = lane_due_at
                    .values()
                    .copied()
                    .min()
                    .unwrap_or_else(|| after_capture.saturating_add(1));
            }

            self.runtime.emergency_stop.wait(Duration::from_millis(1));
        }
    }

    fn resolve_watch_timeout(
        &mut self,
        block_id: &str,
        group: &WatchGroup,
        mut runner: WatchGroupRunner,
    ) -> Option<StopReason> {
        self.emitter
            .arbitration_completed(block_id, None, Vec::new(), false);
        runner.settle_and_exit();
        self.observations.clear();
        self.watch_groups.insert(block_id.to_string(), runner);
        match &group.timeout_outcome {
            TimeoutOutcome::Continue => None,
            TimeoutOutcome::RunBody { body } => self.execute_blocks(body),
            TimeoutOutcome::StopError { message } => Some(StopReason::StopError {
                message: message.clone(),
            }),
        }
    }

    fn watch_poll_interval_ms(&self, condition: &PassiveCondition) -> u64 {
        match condition {
            PassiveCondition::Text { rule_id, .. } => self
                .compiled
                .definition()
                .text_rules
                .iter()
                .find(|rule| &rule.id == rule_id)
                .map_or(1, |rule| rule.poll_interval_ms),
            PassiveCondition::Image { rule_id, .. } => self
                .compiled
                .definition()
                .image_rules
                .iter()
                .find(|rule| &rule.id == rule_id)
                .map_or(1, |rule| rule.poll_interval_ms),
        }
    }

    fn watch_capture_rect(
        &self,
        condition: &PassiveCondition,
    ) -> std::result::Result<crate::engine::types::Rect, String> {
        let region_id = match condition {
            PassiveCondition::Text { rule_id, .. } => {
                &self
                    .compiled
                    .definition()
                    .text_rules
                    .iter()
                    .find(|rule| &rule.id == rule_id)
                    .ok_or_else(|| format!("compiled text rule '{rule_id}' is missing"))?
                    .region_id
            }
            PassiveCondition::Image { rule_id, .. } => {
                &self
                    .compiled
                    .definition()
                    .image_rules
                    .iter()
                    .find(|rule| &rule.id == rule_id)
                    .ok_or_else(|| format!("compiled image rule '{rule_id}' is missing"))?
                    .region_id
            }
        };
        let region = self
            .compiled
            .definition()
            .regions
            .iter()
            .find(|region| &region.id == region_id)
            .ok_or_else(|| format!("compiled Watch region '{region_id}' is missing"))?;
        let target = &self.compiled.definition().target;
        Ok(crate::engine::types::Rect::new(
            0,
            0,
            target.captured_client_width,
            target.captured_client_height,
        )
        .rect_from_ratio(region.rect))
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
                side_effect_epoch: self.side_effect_epoch,
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
            side_effect_epoch: self.side_effect_epoch,
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
                if !token.is_current(self.emitter.run_id(), generation)
                    || token.side_effect_epoch != self.side_effect_epoch
                {
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
        if is_click_action(action) {
            if matches!(
                self.compiled.definition().safety.max_clicks,
                Limit::Finite(maximum)
                    if self.non_authoritative_planned_clicks >= maximum
            ) {
                return Some(StopReason::SafetyLimit {
                    message: "maximum click count exceeded".to_string(),
                });
            }
            self.non_authoritative_planned_clicks =
                self.non_authoritative_planned_clicks.saturating_add(1);
        }
        // This run-local simulation count gates planning only. The live `ActionCommitter`
        // remains the sole owner of actual click consumption at the first SendInput boundary.
        self.emitter.action_planned(block_id, action.clone(), token);
        if is_side_effect_action(action) {
            self.invalidate_after_side_effect();
        }
        self.cooperative_wait(self.compiled.definition().safety.minimum_click_interval_ms)
    }

    fn invalidate_after_side_effect(&mut self) {
        let generation = self.runtime.control_handle().generation();
        self.detector_generations.insert(generation);
        self.side_effect_epoch = self.side_effect_epoch.wrapping_add(1);
        self.observations.clear();
        for runner in self.watch_groups.values_mut() {
            runner.invalidate(generation);
        }
        self.runtime
            .watch_pool
            .cancel_old_epoch(self.emitter.run_id(), self.side_effect_epoch);
        self.runtime.detector.side_effect_boundary(
            self.emitter.run_id(),
            generation,
            self.side_effect_epoch,
        );
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

fn passive_condition(condition: &PassiveCondition) -> Condition {
    match condition {
        PassiveCondition::Text {
            source_block_id,
            rule_id,
        } => Condition::Text {
            source_block_id: source_block_id.clone(),
            rule_id: rule_id.clone(),
            mode: ObserveMode::CheckNow,
        },
        PassiveCondition::Image {
            source_block_id,
            rule_id,
        } => Condition::Image {
            source_block_id: source_block_id.clone(),
            rule_id: rule_id.clone(),
            mode: ObserveMode::CheckNow,
        },
    }
}

fn frame_matches_capture(
    frame: super::ImageFrameMetadata,
    capture: crate::engine::automation::CaptureFrameMetadata,
) -> bool {
    frame.frame_id == capture.frame_id
        && frame.captured_at_ms == capture.captured_at_ms
        && frame.window_id == capture.window_id
        && frame.window_revision == capture.window_revision
        && frame.process_id == capture.process_id
        && frame.process_started_at_100ns == capture.process_started_at_100ns
        && frame.client_x == capture.client_x
        && frame.client_y == capture.client_y
        && frame.client_width == capture.client_width
        && frame.client_height == capture.client_height
        && frame.geometry_revision == capture.geometry_revision
        && frame.display_id == capture.display_id
        && frame.display_profile_revision == capture.display_profile_revision
        && frame.dpi == capture.dpi
        && frame.is_visible == capture.is_visible
        && frame.is_minimized == capture.is_minimized
        && frame.is_foreground == capture.is_foreground
}

fn establish_arbitration_deadline(
    current: Option<u64>,
    candidate_deadline: u64,
    absolute_deadline: Option<u64>,
) -> Option<u64> {
    current.or_else(|| {
        Some(absolute_deadline.map_or(candidate_deadline, |deadline| {
            candidate_deadline.min(deadline)
        }))
    })
}

fn watch_timeout_reached(group: &WatchGroup, started_at: u64, now: u64) -> bool {
    matches!(group.timeout_ms, Limit::Finite(timeout_ms) if now >= started_at.saturating_add(timeout_ms))
}

fn safety_bypass_for_stop(reason: &StopReason) -> SafetyBypass {
    match reason {
        StopReason::EmergencyStopped => SafetyBypass::EmergencyStop,
        _ => SafetyBypass::Cancelled,
    }
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

fn is_click_action(action: &Action) -> bool {
    matches!(
        action,
        Action::ClickTextMatch { .. }
            | Action::ClickImageMatch { .. }
            | Action::ClickPoint { .. }
            | Action::ClickRegion { .. }
    )
}

fn is_side_effect_action(action: &Action) -> bool {
    is_click_action(action) || matches!(action, Action::MoveOnly { .. })
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

    fn arbitration_completed(
        &mut self,
        block_id: &str,
        winner_lane_id: Option<String>,
        discarded_lane_ids: Vec<String>,
        safety_bypassed: bool,
    ) {
        let (sequence, elapsed_ms, run_id) = self.metadata();
        self.push_critical(RunEvent::ArbitrationCompleted {
            sequence,
            elapsed_ms,
            run_id,
            block_id: block_id.to_string(),
            winner_lane_id,
            discarded_lane_ids,
            safety_bypassed,
        });
    }

    fn polling_delayed(&mut self, block_id: &str, lane_id: &str, delayed_polls: u64) {
        let (sequence, elapsed_ms, run_id) = self.metadata();
        let event = RunEvent::PollingDelayed {
            sequence,
            elapsed_ms,
            run_id,
            block_id: block_id.to_string(),
            lane_id: lane_id.to_string(),
            delayed_polls,
        };
        if let Some(last) = self.events.last_mut().filter(|last| {
            matches!(
                (&**last, &event),
                (
                    RunEvent::PollingDelayed { run_id: previous_run, block_id: previous_block, lane_id: previous_lane, .. },
                    RunEvent::PollingDelayed { run_id: current_run, block_id: current_block, lane_id: current_lane, .. }
                ) if previous_run == current_run && previous_block == current_block && previous_lane == current_lane
            )
        }) {
            *last = event;
        } else if self.events.len() + 1 < self.capacity - FINAL_EVENT_RESERVE {
            self.events.push(event);
        }
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
            ObserveMode, PassiveCondition, PointDefinition, PreprocessProfile, RegionDefinition,
            SafetyPolicy, TargetProfile, TextMatchMode, TextRule, TimeoutOutcome, WatchGroup,
            WatchLane,
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

        fn capture_frame(
            &self,
            rect: Rect,
        ) -> Result<crate::engine::automation::CapturedScreenFrame> {
            static NEXT_FAKE_FRAME_ID: AtomicU64 = AtomicU64::new(1);
            let frame_id = NEXT_FAKE_FRAME_ID.fetch_add(1, Ordering::Relaxed);
            Ok(crate::engine::automation::CapturedScreenFrame {
                image: ScreenImage::new(image::RgbaImage::new(rect.width, rect.height)),
                metadata: crate::engine::automation::CaptureFrameMetadata {
                    frame_id,
                    captured_at_ms: frame_id,
                    window_id: 9,
                    window_revision: 2,
                    process_id: 10,
                    process_started_at_100ns: 11,
                    client_x: 0,
                    client_y: 0,
                    client_width: 1920,
                    client_height: 1080,
                    geometry_revision: 3,
                    display_id: 5,
                    display_profile_revision: 4,
                    dpi: 96,
                    is_visible: true,
                    is_minimized: false,
                    is_foreground: true,
                },
            })
        }

        fn validate_frame(
            &self,
            _rect: Rect,
            metadata: &crate::engine::automation::CaptureFrameMetadata,
        ) -> Result<()> {
            anyhow::ensure!(metadata.window_id == 9 && metadata.process_id == 10);
            Ok(())
        }
    }

    #[derive(Debug, Clone)]
    struct StableLiveTarget(TargetSnapshot);

    impl TargetGuard for StableLiveTarget {
        fn snapshot(&self) -> Result<TargetSnapshot> {
            Ok(self.0.clone())
        }

        fn validate(&self, expected: &TargetSnapshot) -> Result<()> {
            anyhow::ensure!(&self.0 == expected, "target changed");
            Ok(())
        }
    }

    #[derive(Debug, Default)]
    struct SuccessfulLiveInput;

    impl LiveActionInput for SuccessfulLiveInput {
        fn reset_manual_baseline(&self) -> Result<()> {
            Ok(())
        }

        fn manual_takeover_detected(&self) -> Result<bool> {
            Ok(false)
        }

        fn dispatch_action(
            &self,
            _point: Point,
            _button: MouseButton,
            _movement: Option<&MouseMovementProfile>,
            _stop: &dyn StopSource,
            commit: &mut dyn FnMut() -> std::result::Result<(), BlockReason>,
            validate_after_movement: &mut dyn FnMut() -> std::result::Result<(), BlockReason>,
        ) -> InputDispatchOutcome {
            if let Err(reason) = commit() {
                return InputDispatchOutcome::PreCommitBlocked(PreCommitInputBlock::Commit {
                    reason,
                });
            }
            if let Err(reason) = validate_after_movement() {
                return InputDispatchOutcome::Committed(CommittedInputOutcome::UncertainDispatch {
                    failure: InputDispatchFailure::Validation { reason },
                });
            }
            InputDispatchOutcome::Committed(CommittedInputOutcome::Dispatched)
        }
    }

    #[derive(Debug, Default)]
    struct NoopLiveControl;

    impl LiveControlSink for NoopLiveControl {
        fn pause_for_manual_takeover(&self) {}

        fn stop_for_manual_takeover(&self) {}
    }

    #[derive(Debug, Default)]
    struct NeverStop;

    impl StopSource for NeverStop {
        fn is_stopped(&self) -> bool {
            false
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
                    process_id: 4,
                    process_started_at_100ns: 6,
                    client_x: 0,
                    client_y: 0,
                    client_width: 64,
                    client_height: 48,
                    geometry_revision: 3,
                    display_id: 5,
                    display_profile_revision: 4,
                    dpi: 96,
                    is_visible: true,
                    is_minimized: false,
                    is_foreground: true,
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
            _request: &super::super::ObservationRequest<'_>,
            capture: &(dyn CaptureSource + Send + Sync),
        ) -> Result<super::super::DetectorEvidence> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            if !self.invalidated_once.swap(true, Ordering::AcqRel) {
                let control = self.control.lock().unwrap().clone().unwrap();
                control.pause();
                control.resume();
            }
            let frame = capture.capture_frame(Rect::new(192, 108, 384, 216))?;
            let metadata = watch_frame_metadata(frame.metadata);
            Ok(super::super::DetectorEvidence {
                matched: true,
                frame_id: metadata.frame_id,
                captured_at_ms: metadata.captured_at_ms,
                match_rect: Some(Rect::new(1, 2, 3, 4)),
                score: Some(0.99),
                match_count: 1,
                stable_frames: 1,
                frame_metadata: Some(metadata),
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
            _request: &super::super::ObservationRequest<'_>,
            capture: &(dyn CaptureSource + Send + Sync),
        ) -> Result<super::super::DetectorEvidence> {
            let call = self.calls.fetch_add(1, Ordering::Relaxed);
            if call == 0 {
                let control = self.control.lock().unwrap().clone().unwrap();
                control.pause();
                control.resume();
                bail!("stale capture failure")
            }
            let frame = capture.capture_frame(Rect::new(192, 108, 384, 216))?;
            let metadata = watch_frame_metadata(frame.metadata);
            Ok(super::super::DetectorEvidence {
                matched: true,
                frame_id: metadata.frame_id,
                captured_at_ms: metadata.captured_at_ms,
                match_rect: Some(Rect::new(1, 2, 3, 4)),
                score: Some(0.99),
                match_count: 1,
                stable_frames: 1,
                frame_metadata: Some(metadata),
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
                poll_interval_ms: 50,
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
    fn dry_run_stops_before_planning_a_click_past_the_finite_limit() {
        let mut definition = fixture_definition(vec![
            point_action("allowed-click"),
            point_action("blocked-click"),
        ]);
        definition.safety.max_clicks = Limit::Finite(1);

        let events = fixture_runtime()
            .run(saved(definition), RunMode::DryRun)
            .unwrap();

        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, RunEvent::ActionPlanned { .. }))
                .count(),
            1
        );
        assert!(events.iter().any(|event| {
            matches!(event, RunEvent::ActionPlanned { block_id, .. } if block_id == "allowed-click")
        }));
        assert!(matches!(
            events.last(),
            Some(RunEvent::RunStopped {
                reason: StopReason::SafetyLimit { message },
                ..
            }) if message == "maximum click count exceeded"
        ));
    }

    #[test]
    fn observation_only_stops_before_planning_a_click_past_the_finite_limit() {
        let mut definition = fixture_definition(vec![
            point_action("allowed-click"),
            point_action("blocked-click"),
        ]);
        definition.safety.max_clicks = Limit::Finite(1);

        let events = fixture_runtime()
            .run(saved(definition), RunMode::ObservationOnly)
            .unwrap();

        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, RunEvent::ActionPlanned { .. }))
                .count(),
            1
        );
        assert!(matches!(
            events.last(),
            Some(RunEvent::RunStopped {
                reason: StopReason::SafetyLimit { message },
                ..
            }) if message == "maximum click count exceeded"
        ));
    }

    #[test]
    fn simulated_planning_does_not_consume_the_live_committer_click_budget() {
        let mut definition =
            fixture_definition(vec![point_action("simulated"), point_action("over-limit")]);
        definition.safety.max_clicks = Limit::Finite(1);
        let compiled = CompiledMacro::compile(saved(definition.clone())).unwrap();

        let simulated_events = fixture_runtime()
            .run(saved(definition), RunMode::DryRun)
            .unwrap();
        assert_eq!(
            simulated_events
                .iter()
                .filter(|event| matches!(event, RunEvent::ActionPlanned { .. }))
                .count(),
            1
        );
        assert!(matches!(
            simulated_events.last(),
            Some(RunEvent::RunStopped {
                reason: StopReason::SafetyLimit { .. },
                ..
            })
        ));

        let target = TargetSnapshot {
            window_id: 91,
            process_id: 7,
            process_started_at_100ns: 100,
            process_path: "game.exe".to_string(),
            client_rect: Rect::new(100, 100, 800, 600),
            window_revision: 1,
            geometry_revision: 2,
            dpi: 144,
            display_profile: "display-a".to_string(),
            display_profile_revision: 3,
            is_visible: true,
            is_minimized: false,
            is_foreground: true,
        };
        let session = LiveActionSession::new(
            Arc::new(StableLiveTarget(target.clone())),
            Arc::new(SuccessfulLiveInput),
            Arc::new(NoopLiveControl),
        );
        let resume = session.resume().unwrap();
        let action = Action::ClickPoint {
            point_id: "point".to_string(),
            button: MouseButton::Left,
        };
        let authorization = compiled
            .authorize_action(
                "live-run",
                1,
                1,
                "simulated",
                &action,
                None,
                &resume,
                target
                    .client_rect
                    .point_from_ratio(PointRatio { x: 0.5, y: 0.5 }),
                1_000,
            )
            .unwrap();
        let committer = ActionCommitter::new(
            session,
            Arc::new(FrozenClock),
            "live-run",
            Limit::Finite(1),
            1,
        )
        .unwrap();
        let prepared = committer
            .prepare(ActionPrepareRequest::new(
                authorization,
                None,
                0,
                TakeoverPolicy::Stop,
                resume,
            ))
            .unwrap();

        let outcome = committer.commit(
            prepared,
            &NeverStop,
            CommitContext::new("live-run", 1, None),
        );

        assert!(matches!(outcome, ActionOutcome::Dispatched { .. }));
        assert_eq!(committer.committed_clicks(), 1);
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
            side_effect_epoch: 0,
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
                process_id: 7,
                process_started_at_100ns: 100,
                client_x: 100,
                client_y: 200,
                client_width: 800,
                client_height: 600,
                geometry_revision: 2,
                display_id: 4,
                display_profile_revision: 3,
                dpi: 144,
                is_visible: true,
                is_minimized: false,
                is_foreground: true,
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
            side_effect_epoch: 0,
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
                process_id: 4,
                process_started_at_100ns: 6,
                client_x: 100,
                client_y: 200,
                client_width: 800,
                client_height: 600,
                geometry_revision: 2,
                display_id: 4,
                display_profile_revision: 3,
                dpi: 144,
                is_visible: true,
                is_minimized: false,
                is_foreground: true,
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

    #[derive(Debug, Default)]
    struct WatchSequenceDetector(Mutex<HashMap<String, VecDeque<bool>>>);

    #[derive(Debug, Default)]
    struct StaleWatchFrameDetector;

    #[derive(Debug, Default)]
    struct WatchCountingCapture(AtomicU64);

    #[derive(Debug, Default)]
    struct DriftBeforeWatchBodyCapture(WatchCountingCapture);

    struct DeadlineCrossingCapture {
        inner: WatchCountingCapture,
        clock: Arc<ManualClock>,
        return_at_ms: u64,
    }

    struct DeadlineDuringValidationCapture {
        inner: WatchCountingCapture,
        clock: Arc<FakeClock>,
        return_at_ms: u64,
    }

    #[derive(Default)]
    struct StopDuringValidationCapture {
        inner: WatchCountingCapture,
        control: Mutex<Option<RuntimeControlHandle>>,
    }

    impl CaptureSource for WatchCountingCapture {
        fn capture(&self, rect: Rect) -> Result<ScreenImage> {
            Ok(ScreenImage::new(image::RgbaImage::new(
                rect.width,
                rect.height,
            )))
        }

        fn capture_frame(
            &self,
            rect: Rect,
        ) -> Result<crate::engine::automation::CapturedScreenFrame> {
            let frame_id = self.0.fetch_add(1, Ordering::Relaxed) + 1;
            Ok(crate::engine::automation::CapturedScreenFrame {
                image: ScreenImage::new(image::RgbaImage::new(rect.width, rect.height)),
                metadata: crate::engine::automation::CaptureFrameMetadata {
                    frame_id,
                    captured_at_ms: frame_id,
                    window_id: 9,
                    window_revision: 2,
                    process_id: 4,
                    process_started_at_100ns: 6,
                    client_x: 0,
                    client_y: 0,
                    client_width: 1920,
                    client_height: 1080,
                    geometry_revision: 3,
                    display_id: 5,
                    display_profile_revision: 4,
                    dpi: 96,
                    is_visible: true,
                    is_minimized: false,
                    is_foreground: true,
                },
            })
        }

        fn validate_frame(
            &self,
            _rect: Rect,
            metadata: &crate::engine::automation::CaptureFrameMetadata,
        ) -> Result<()> {
            anyhow::ensure!(metadata.window_id == 9 && metadata.process_id == 4);
            Ok(())
        }
    }

    impl CaptureSource for DriftBeforeWatchBodyCapture {
        fn capture(&self, rect: Rect) -> Result<ScreenImage> {
            self.0.capture(rect)
        }

        fn capture_frame(
            &self,
            rect: Rect,
        ) -> Result<crate::engine::automation::CapturedScreenFrame> {
            self.0.capture_frame(rect)
        }

        fn validate_frame(
            &self,
            _rect: Rect,
            _metadata: &crate::engine::automation::CaptureFrameMetadata,
        ) -> Result<()> {
            Err(crate::engine::automation::StaleCapturedFrameError.into())
        }
    }

    impl CaptureSource for DeadlineCrossingCapture {
        fn capture(&self, rect: Rect) -> Result<ScreenImage> {
            self.inner.capture(rect)
        }

        fn capture_frame(
            &self,
            rect: Rect,
        ) -> Result<crate::engine::automation::CapturedScreenFrame> {
            let frame = self.inner.capture_frame(rect)?;
            self.clock.set(self.return_at_ms);
            Ok(frame)
        }

        fn validate_frame(
            &self,
            rect: Rect,
            metadata: &crate::engine::automation::CaptureFrameMetadata,
        ) -> Result<()> {
            self.inner.validate_frame(rect, metadata)
        }
    }

    impl CaptureSource for DeadlineDuringValidationCapture {
        fn capture(&self, rect: Rect) -> Result<ScreenImage> {
            self.inner.capture(rect)
        }

        fn capture_frame(
            &self,
            rect: Rect,
        ) -> Result<crate::engine::automation::CapturedScreenFrame> {
            self.inner.capture_frame(rect)
        }

        fn validate_frame(
            &self,
            rect: Rect,
            metadata: &crate::engine::automation::CaptureFrameMetadata,
        ) -> Result<()> {
            self.inner.validate_frame(rect, metadata)?;
            self.clock.0.store(self.return_at_ms, Ordering::Relaxed);
            Ok(())
        }
    }

    impl CaptureSource for StopDuringValidationCapture {
        fn capture(&self, rect: Rect) -> Result<ScreenImage> {
            self.inner.capture(rect)
        }

        fn capture_frame(
            &self,
            rect: Rect,
        ) -> Result<crate::engine::automation::CapturedScreenFrame> {
            self.inner.capture_frame(rect)
        }

        fn validate_frame(
            &self,
            rect: Rect,
            metadata: &crate::engine::automation::CaptureFrameMetadata,
        ) -> Result<()> {
            self.inner.validate_frame(rect, metadata)?;
            self.control.lock().unwrap().as_ref().unwrap().stop();
            Ok(())
        }
    }

    struct DeadlineCrossingDetector {
        clock: Arc<ManualClock>,
        return_at_ms: u64,
        calls: AtomicU64,
    }

    #[derive(Debug, Default)]
    struct MismatchedWatchProvenanceDetector;

    impl ConditionDetector for MismatchedWatchProvenanceDetector {
        fn observe(
            &self,
            _request: &super::super::ObservationRequest<'_>,
            capture: &(dyn CaptureSource + Send + Sync),
        ) -> Result<super::super::DetectorEvidence> {
            let frame = capture.capture_frame(Rect::new(192, 108, 384, 216))?;
            let mut metadata = watch_frame_metadata(frame.metadata);
            metadata.display_id = metadata.display_id.saturating_add(1);
            Ok(super::super::DetectorEvidence {
                matched: true,
                frame_id: metadata.frame_id,
                captured_at_ms: metadata.captured_at_ms,
                match_rect: Some(Rect::new(1, 2, 3, 4)),
                score: Some(0.99),
                match_count: 1,
                stable_frames: 1,
                frame_metadata: Some(metadata),
                details: serde_json::Value::Null,
            })
        }
    }

    impl ConditionDetector for DeadlineCrossingDetector {
        fn observe(
            &self,
            _request: &super::super::ObservationRequest<'_>,
            capture: &(dyn CaptureSource + Send + Sync),
        ) -> Result<super::super::DetectorEvidence> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            let frame = capture.capture_frame(Rect::new(192, 108, 384, 216))?;
            let metadata = watch_frame_metadata(frame.metadata);
            self.clock.set(self.return_at_ms);
            Ok(super::super::DetectorEvidence {
                matched: true,
                frame_id: metadata.frame_id,
                captured_at_ms: metadata.captured_at_ms,
                match_rect: Some(Rect::new(1, 2, 3, 4)),
                score: Some(0.99),
                match_count: 1,
                stable_frames: 1,
                frame_metadata: Some(metadata),
                details: serde_json::Value::Null,
            })
        }
    }

    fn watch_frame_metadata(
        metadata: crate::engine::automation::CaptureFrameMetadata,
    ) -> super::super::ImageFrameMetadata {
        super::super::ImageFrameMetadata {
            frame_id: metadata.frame_id,
            captured_at_ms: metadata.captured_at_ms,
            window_id: metadata.window_id,
            window_revision: metadata.window_revision,
            process_id: metadata.process_id,
            process_started_at_100ns: metadata.process_started_at_100ns,
            client_x: metadata.client_x,
            client_y: metadata.client_y,
            client_width: metadata.client_width,
            client_height: metadata.client_height,
            geometry_revision: metadata.geometry_revision,
            display_id: metadata.display_id,
            display_profile_revision: metadata.display_profile_revision,
            dpi: metadata.dpi,
            is_visible: metadata.is_visible,
            is_minimized: metadata.is_minimized,
            is_foreground: metadata.is_foreground,
            region_revision: 1,
            rule_revision: 1,
        }
    }

    #[derive(Debug, Default)]
    struct CapturingWatchDetector;

    impl ConditionDetector for CapturingWatchDetector {
        fn observe(
            &self,
            _request: &super::super::ObservationRequest<'_>,
            capture: &(dyn CaptureSource + Send + Sync),
        ) -> Result<super::super::DetectorEvidence> {
            let frame = capture.capture_frame(Rect::new(192, 108, 384, 216))?;
            let metadata = watch_frame_metadata(frame.metadata);
            Ok(super::super::DetectorEvidence {
                matched: true,
                frame_id: metadata.frame_id,
                captured_at_ms: metadata.captured_at_ms,
                match_rect: Some(Rect::new(1, 2, 3, 4)),
                score: Some(0.99),
                match_count: 1,
                stable_frames: 1,
                frame_metadata: Some(metadata),
                details: serde_json::Value::Null,
            })
        }
    }

    impl ConditionDetector for StaleWatchFrameDetector {
        fn observe(
            &self,
            _request: &super::super::ObservationRequest<'_>,
            _capture: &(dyn CaptureSource + Send + Sync),
        ) -> Result<super::super::DetectorEvidence> {
            Err(crate::engine::automation::StaleCapturedFrameError.into())
        }
    }

    impl WatchSequenceDetector {
        fn with(values: impl IntoIterator<Item = (&'static str, Vec<bool>)>) -> Self {
            Self(Mutex::new(
                values
                    .into_iter()
                    .map(|(lane, values)| (lane.to_string(), values.into_iter().collect()))
                    .collect(),
            ))
        }
    }

    impl ConditionDetector for WatchSequenceDetector {
        fn observe(
            &self,
            request: &super::super::ObservationRequest<'_>,
            capture: &(dyn CaptureSource + Send + Sync),
        ) -> Result<super::super::DetectorEvidence> {
            let source = condition_source_id(request.condition);
            let matched = self
                .0
                .lock()
                .unwrap()
                .get_mut(source)
                .and_then(VecDeque::pop_front)
                .unwrap_or(false);
            let frame = capture.capture_frame(Rect::new(192, 108, 384, 216))?;
            let metadata = watch_frame_metadata(frame.metadata);
            Ok(super::super::DetectorEvidence {
                matched,
                frame_id: metadata.frame_id,
                captured_at_ms: metadata.captured_at_ms,
                match_rect: matched.then(|| Rect::new(1, 2, 3, 4)),
                score: matched.then_some(0.99),
                match_count: u32::from(matched),
                stable_frames: u8::from(matched),
                frame_metadata: Some(metadata),
                details: serde_json::json!({"watch_fixture": source}),
            })
        }
    }

    fn watch_lane(id: &str, then_body: Vec<Block>) -> WatchLane {
        WatchLane {
            id: id.to_string(),
            enabled: true,
            condition: PassiveCondition::Text {
                source_block_id: id.to_string(),
                rule_id: "text".to_string(),
            },
            then_body,
        }
    }

    fn image_watch_lane(id: &str, then_body: Vec<Block>) -> WatchLane {
        WatchLane {
            id: id.to_string(),
            enabled: true,
            condition: PassiveCondition::Image {
                source_block_id: id.to_string(),
                rule_id: "image".to_string(),
            },
            then_body,
        }
    }

    fn saved_with_image_watch(blocks: Vec<Block>) -> SavedRevision {
        let template = fixture_template(23);
        let bytes = png_bytes(template.clone());
        let asset = AssetRef {
            id: "watch-template".to_string(),
            revision: 1,
            content_hash: sha256_hex(&bytes),
        };
        let mut definition = fixture_definition(blocks);
        definition
            .image_rules
            .push(fixture_image_rule("image", asset.clone(), Some(&template)));
        let mut revision = saved(definition);
        revision.pinned_assets.push(PinnedAsset { asset, bytes });
        revision
    }

    fn watch_group(
        lanes: Vec<WatchLane>,
        timeout_ms: u64,
        timeout_outcome: TimeoutOutcome,
    ) -> Block {
        block(
            "watch",
            BlockKind::WatchGroup {
                group: WatchGroup {
                    lanes,
                    timeout_ms: Limit::Finite(timeout_ms),
                    timeout_outcome,
                    cooldown_ms: 0,
                },
            },
        )
    }

    #[test]
    fn watch_group_executes_one_priority_winner_then_exits_without_queuing_loser() {
        let macro_revision = saved(fixture_definition(vec![watch_group(
            vec![
                watch_lane("lane-1", vec![point_action("lane-1-body")]),
                watch_lane("lane-2", vec![point_action("lane-2-body")]),
            ],
            2_000,
            TimeoutOutcome::Continue,
        )]));
        let events = fixture_runtime_with_detector(WatchSequenceDetector::with([
            ("lane-1", vec![true]),
            ("lane-2", vec![true]),
        ]))
        .run(macro_revision, RunMode::DryRun)
        .unwrap();

        assert!(events.iter().any(|event| matches!(event, RunEvent::ArbitrationCompleted { winner_lane_id: Some(id), .. } if id == "lane-1")), "{events:#?}");
        assert!(events.iter().any(|event| matches!(event, RunEvent::BlockEntered { block_id, .. } if block_id == "lane-1-body")));
        assert!(!events.iter().any(|event| matches!(event, RunEvent::BlockEntered { block_id, .. } if block_id == "lane-2-body")));
    }

    #[test]
    fn watch_group_timeout_runs_only_its_explicit_timeout_body() {
        let macro_revision = saved(fixture_definition(vec![watch_group(
            vec![watch_lane("lane-1", vec![point_action("winner-body")])],
            2,
            TimeoutOutcome::RunBody {
                body: vec![point_action("timeout-body")],
            },
        )]));
        let events = fixture_runtime_with_detector(WatchSequenceDetector::with([(
            "lane-1",
            vec![false, false, false],
        )]))
        .run(macro_revision, RunMode::DryRun)
        .unwrap();

        assert!(events.iter().any(|event| matches!(event, RunEvent::BlockEntered { block_id, .. } if block_id == "timeout-body")));
        assert!(!events.iter().any(|event| matches!(event, RunEvent::BlockEntered { block_id, .. } if block_id == "winner-body")));
    }

    #[test]
    fn watch_lane_latches_persist_across_repeat_entries_and_reset_after_newer_false() {
        let watch = watch_group(
            vec![
                watch_lane("lane-1", vec![point_action("lane-1-body")]),
                watch_lane("lane-2", vec![point_action("lane-2-body")]),
            ],
            1_000,
            TimeoutOutcome::Continue,
        );
        let macro_revision = saved(fixture_definition(vec![block(
            "repeat",
            BlockKind::RepeatN {
                count: 3,
                body: vec![watch],
            },
        )]));
        let events = fixture_runtime_with_detector(WatchSequenceDetector::with([
            ("lane-1", vec![true, false, false]),
            ("lane-2", vec![true, false, true]),
        ]))
        .run(macro_revision, RunMode::DryRun)
        .unwrap();

        let entered: Vec<_> = events
            .iter()
            .filter_map(|event| match event {
                RunEvent::BlockEntered { block_id, .. } if block_id.ends_with("-body") => {
                    Some(block_id.as_str())
                }
                _ => None,
            })
            .collect();
        assert_eq!(entered, vec!["lane-1-body", "lane-2-body"], "{events:#?}");
    }

    #[test]
    fn watch_group_reports_polling_delayed_when_text_worker_is_saturated() {
        let macro_revision = saved(fixture_definition(vec![watch_group(
            vec![watch_lane("lane-1", vec![]), watch_lane("lane-2", vec![])],
            100,
            TimeoutOutcome::Continue,
        )]));
        let events = fixture_runtime_with_detector(WatchSequenceDetector::with([
            ("lane-1", vec![false]),
            ("lane-2", vec![false]),
        ]))
        .run(macro_revision, RunMode::ObservationOnly)
        .unwrap();

        assert!(events.iter().any(|event| matches!(event, RunEvent::PollingDelayed { block_id, lane_id, .. } if block_id == "watch" && lane_id == "lane-2")));
    }

    #[test]
    fn target_frame_invalidation_bypasses_watch_arbitration() {
        let macro_revision = saved(fixture_definition(vec![watch_group(
            vec![watch_lane("lane-1", vec![point_action("body")])],
            20,
            TimeoutOutcome::Continue,
        )]));
        let events = fixture_runtime_with_detector(StaleWatchFrameDetector)
            .run(macro_revision, RunMode::ObservationOnly)
            .unwrap();

        assert!(events.iter().any(|event| matches!(
            event,
            RunEvent::ArbitrationCompleted {
                safety_bypassed: true,
                ..
            }
        )));
        assert!(!events.iter().any(
            |event| matches!(event, RunEvent::BlockEntered { block_id, .. } if block_id == "body")
        ));
    }

    #[test]
    fn text_watch_revalidates_current_target_immediately_before_body() {
        let runtime = MacroRuntime::new(
            Arc::new(DriftBeforeWatchBodyCapture::default()),
            Arc::new(CapturingWatchDetector),
            Arc::new(FakeClock::default()),
        );
        let revision = saved(fixture_definition(vec![watch_group(
            vec![watch_lane("lane-1", vec![point_action("body")])],
            2_000,
            TimeoutOutcome::Continue,
        )]));

        let events = runtime.run(revision, RunMode::DryRun).unwrap();

        assert!(events.iter().any(|event| matches!(
            event,
            RunEvent::ArbitrationCompleted {
                safety_bypassed: true,
                ..
            }
        )));
        assert!(!events.iter().any(
            |event| matches!(event, RunEvent::BlockEntered { block_id, .. } if block_id == "body")
        ));
    }

    #[test]
    fn image_watch_revalidates_current_target_immediately_before_body() {
        let runtime = MacroRuntime::new(
            Arc::new(DriftBeforeWatchBodyCapture::default()),
            Arc::new(CapturingWatchDetector),
            Arc::new(FakeClock::default()),
        );
        let revision = saved_with_image_watch(vec![watch_group(
            vec![image_watch_lane("lane-1", vec![point_action("body")])],
            2_000,
            TimeoutOutcome::Continue,
        )]);

        let events = runtime.run(revision, RunMode::DryRun).unwrap();

        assert!(events.iter().any(|event| matches!(
            event,
            RunEvent::ArbitrationCompleted {
                safety_bypassed: true,
                ..
            }
        )));
        assert!(!events.iter().any(
            |event| matches!(event, RunEvent::BlockEntered { block_id, .. } if block_id == "body")
        ));
    }

    #[test]
    fn pause_resume_generation_change_discards_in_flight_watch_candidate() {
        let detector = Arc::new(InvalidatingDetector::default());
        let runtime = MacroRuntime::new(
            Arc::new(FakeCapture),
            detector.clone(),
            Arc::new(FakeClock::default()),
        );
        *detector.control.lock().unwrap() = Some(runtime.control_handle());
        let macro_revision = saved(fixture_definition(vec![watch_group(
            vec![watch_lane("lane-1", vec![point_action("body")])],
            2_000,
            TimeoutOutcome::Continue,
        )]));

        let events = runtime.run(macro_revision, RunMode::DryRun).unwrap();

        assert!(detector.calls.load(Ordering::Relaxed) >= 2);
        let winner_generation = events.iter().find_map(|event| match event {
            RunEvent::ObservationCompleted {
                block_id,
                token: Some(token),
                ..
            } if block_id == "lane-1" => Some(token.generation),
            _ => None,
        });
        assert_eq!(
            winner_generation,
            Some(runtime.control_handle().generation()),
            "{events:#?}"
        );
        assert!(events.iter().any(
            |event| matches!(event, RunEvent::BlockEntered { block_id, .. } if block_id == "body")
        ), "{events:#?}");
    }

    #[test]
    fn winning_body_side_effect_invalidates_watch_observation_before_later_action() {
        let watch = watch_group(
            vec![watch_lane("lane-1", vec![point_action("body-click")])],
            2_000,
            TimeoutOutcome::Continue,
        );
        let later_match_click = block(
            "later-match-click",
            BlockKind::Action {
                action: Action::ClickTextMatch {
                    source_block_id: "lane-1".to_string(),
                    button: super::super::MouseButton::Left,
                },
            },
        );
        let macro_revision = saved(fixture_definition(vec![watch, later_match_click]));
        let events =
            fixture_runtime_with_detector(WatchSequenceDetector::with([("lane-1", vec![true])]))
                .run(macro_revision, RunMode::DryRun)
                .unwrap();

        assert!(events.iter().any(|event| matches!(event, RunEvent::ActionPlanned { block_id, .. } if block_id == "body-click")), "{events:#?}");
        assert!(events.iter().any(|event| matches!(event, RunEvent::ActionBlocked { block_id, reason, .. } if block_id == "later-match-click" && reason.contains("fresh observation"))));
    }

    fn matched_text_action(id: &str, source: &str) -> Block {
        block(
            id,
            BlockKind::Action {
                action: Action::ClickTextMatch {
                    source_block_id: source.to_string(),
                    button: super::super::MouseButton::Left,
                },
            },
        )
    }

    #[test]
    fn first_click_in_winner_body_invalidates_token_before_second_matched_action() {
        let winner_body = vec![
            point_action("first-click"),
            matched_text_action("second-match-click", "lane-1"),
        ];
        let revision = saved(fixture_definition(vec![watch_group(
            vec![watch_lane("lane-1", winner_body)],
            2_000,
            TimeoutOutcome::Continue,
        )]));

        let events =
            fixture_runtime_with_detector(WatchSequenceDetector::with([("lane-1", vec![true])]))
                .run(revision, RunMode::DryRun)
                .unwrap();

        assert!(events.iter().any(|event| matches!(event, RunEvent::ActionPlanned { block_id, .. } if block_id == "first-click")));
        assert!(events.iter().any(|event| {
            matches!(event, RunEvent::ActionBlocked { block_id, reason, .. } if block_id == "second-match-click" && reason.contains("fresh observation"))
        }));
        assert!(!events.iter().any(|event| matches!(event, RunEvent::ActionPlanned { block_id, .. } if block_id == "second-match-click")));
    }

    #[test]
    fn move_only_in_winner_body_invalidates_token_before_matched_action() {
        let winner_body = vec![
            block(
                "move-only",
                BlockKind::Action {
                    action: Action::MoveOnly {
                        target: super::super::ActionTarget::Point {
                            point_id: "point".to_string(),
                        },
                    },
                },
            ),
            matched_text_action("after-move-match", "lane-1"),
        ];
        let revision = saved(fixture_definition(vec![watch_group(
            vec![watch_lane("lane-1", winner_body)],
            2_000,
            TimeoutOutcome::Continue,
        )]));

        let events =
            fixture_runtime_with_detector(WatchSequenceDetector::with([("lane-1", vec![true])]))
                .run(revision, RunMode::DryRun)
                .unwrap();

        assert!(events.iter().any(|event| matches!(event, RunEvent::ActionPlanned { block_id, .. } if block_id == "move-only")));
        assert!(events.iter().any(|event| {
            matches!(event, RunEvent::ActionBlocked { block_id, reason, .. } if block_id == "after-move-match" && reason.contains("fresh observation"))
        }));
        assert!(!events.iter().any(|event| matches!(event, RunEvent::ActionPlanned { block_id, .. } if block_id == "after-move-match")));
    }

    #[test]
    fn watch_runtime_shares_one_capture_frame_across_compatible_lanes() {
        let capture = Arc::new(WatchCountingCapture::default());
        let runtime = MacroRuntime::new(
            capture.clone(),
            Arc::new(CapturingWatchDetector),
            Arc::new(FakeClock::default()),
        );
        let macro_revision = saved(fixture_definition(vec![watch_group(
            vec![watch_lane("lane-1", vec![]), watch_lane("lane-2", vec![])],
            2_000,
            TimeoutOutcome::Continue,
        )]));

        let events = runtime
            .run(macro_revision, RunMode::ObservationOnly)
            .unwrap();
        let frame_ids: Vec<_> = events
            .iter()
            .filter_map(|event| match event {
                RunEvent::ObservationCompleted {
                    block_id, evidence, ..
                } if block_id.starts_with("lane-") => Some(evidence.frame_id),
                _ => None,
            })
            .collect();

        assert!(capture.0.load(Ordering::Relaxed) >= 1);
        assert!(frame_ids.len() >= 2, "{events:#?}");
        assert_eq!(frame_ids[0], frame_ids[1]);
    }

    #[test]
    fn zero_timeout_watch_dispatches_no_capture_or_detector_job() {
        let capture = Arc::new(WatchCountingCapture::default());
        let detector = Arc::new(CountingUnmatchedDetector::default());
        let runtime = MacroRuntime::new(
            capture.clone(),
            detector.clone(),
            Arc::new(FakeClock::default()),
        );
        let macro_revision = saved(fixture_definition(vec![watch_group(
            vec![watch_lane("lane-1", vec![])],
            0,
            TimeoutOutcome::Continue,
        )]));

        let events = runtime
            .run(macro_revision, RunMode::ObservationOnly)
            .unwrap();

        assert_eq!(capture.0.load(Ordering::Relaxed), 0);
        assert_eq!(detector.0.load(Ordering::Relaxed), 0);
        assert!(!events.iter().any(|event| matches!(event, RunEvent::ObservationCompleted { block_id, .. } if block_id == "lane-1")));
    }

    #[test]
    fn capture_that_returns_after_absolute_watch_deadline_dispatches_no_job() {
        let clock = Arc::new(ManualClock::default());
        let capture = Arc::new(DeadlineCrossingCapture {
            inner: WatchCountingCapture::default(),
            clock: Arc::clone(&clock),
            return_at_ms: 10,
        });
        let detector = Arc::new(CountingUnmatchedDetector::default());
        let runtime = MacroRuntime::new(capture.clone(), detector.clone(), clock);
        let revision = saved(fixture_definition(vec![watch_group(
            vec![watch_lane("lane-1", vec![point_action("body")])],
            5,
            TimeoutOutcome::RunBody {
                body: vec![block(
                    "timeout-body",
                    BlockKind::Comment {
                        text: "deadline".to_string(),
                    },
                )],
            },
        )]));

        let events = runtime.run(revision, RunMode::DryRun).unwrap();

        assert_eq!(capture.inner.0.load(Ordering::Relaxed), 1);
        assert_eq!(detector.0.load(Ordering::Relaxed), 0);
        assert!(events.iter().any(|event| matches!(event, RunEvent::BlockEntered { block_id, .. } if block_id == "timeout-body")));
        assert!(!events.iter().any(
            |event| matches!(event, RunEvent::BlockEntered { block_id, .. } if block_id == "body")
        ));
    }

    #[test]
    fn detector_completion_after_absolute_watch_deadline_cannot_win() {
        let clock = Arc::new(ManualClock::default());
        let detector = Arc::new(DeadlineCrossingDetector {
            clock: Arc::clone(&clock),
            return_at_ms: 10,
            calls: AtomicU64::new(0),
        });
        let runtime = MacroRuntime::new(
            Arc::new(WatchCountingCapture::default()),
            detector.clone(),
            clock,
        );
        let revision = saved(fixture_definition(vec![watch_group(
            vec![watch_lane("lane-1", vec![point_action("body")])],
            5,
            TimeoutOutcome::RunBody {
                body: vec![block(
                    "timeout-body",
                    BlockKind::Comment {
                        text: "deadline".to_string(),
                    },
                )],
            },
        )]));

        let events = runtime.run(revision, RunMode::DryRun).unwrap();

        assert_eq!(detector.calls.load(Ordering::Relaxed), 1);
        assert!(events.iter().any(|event| matches!(event, RunEvent::BlockEntered { block_id, .. } if block_id == "timeout-body")));
        assert!(!events.iter().any(
            |event| matches!(event, RunEvent::BlockEntered { block_id, .. } if block_id == "body")
        ));
    }

    #[test]
    fn validation_that_crosses_absolute_watch_deadline_cannot_enter_winner_body() {
        let clock = Arc::new(FakeClock::default());
        let capture = Arc::new(DeadlineDuringValidationCapture {
            inner: WatchCountingCapture::default(),
            clock: Arc::clone(&clock),
            return_at_ms: 1_000,
        });
        let runtime = MacroRuntime::new(capture, Arc::new(CapturingWatchDetector), clock);
        let revision = saved(fixture_definition(vec![watch_group(
            vec![watch_lane("lane-1", vec![point_action("body")])],
            100,
            TimeoutOutcome::RunBody {
                body: vec![block(
                    "timeout-body",
                    BlockKind::Comment {
                        text: "deadline".to_string(),
                    },
                )],
            },
        )]));

        let events = runtime.run(revision, RunMode::DryRun).unwrap();

        assert!(events.iter().any(|event| matches!(event, RunEvent::BlockEntered { block_id, .. } if block_id == "timeout-body")));
        assert!(!events.iter().any(
            |event| matches!(event, RunEvent::BlockEntered { block_id, .. } if block_id == "body")
        ));
    }

    #[test]
    fn stop_during_winner_validation_is_rechecked_before_body() {
        let capture = Arc::new(StopDuringValidationCapture::default());
        let runtime = MacroRuntime::new(
            capture.clone(),
            Arc::new(CapturingWatchDetector),
            Arc::new(FakeClock::default()),
        );
        *capture.control.lock().unwrap() = Some(runtime.control_handle());
        let revision = saved(fixture_definition(vec![watch_group(
            vec![watch_lane("lane-1", vec![point_action("body")])],
            100,
            TimeoutOutcome::Continue,
        )]));

        let events = runtime.run(revision, RunMode::DryRun).unwrap();

        assert!(matches!(
            events.last(),
            Some(RunEvent::RunStopped {
                reason: StopReason::UserStopped,
                ..
            })
        ));
        assert!(!events.iter().any(
            |event| matches!(event, RunEvent::BlockEntered { block_id, .. } if block_id == "body")
        ));
    }

    #[test]
    fn watch_candidate_must_match_complete_capture_provenance() {
        let runtime = MacroRuntime::new(
            Arc::new(WatchCountingCapture::default()),
            Arc::new(MismatchedWatchProvenanceDetector),
            Arc::new(FakeClock::default()),
        );
        let revision = saved(fixture_definition(vec![watch_group(
            vec![watch_lane("lane-1", vec![point_action("body")])],
            100,
            TimeoutOutcome::Continue,
        )]));

        let events = runtime.run(revision, RunMode::DryRun).unwrap();

        assert!(matches!(
            events.last(),
            Some(RunEvent::RunStopped {
                reason: StopReason::TechnicalFailure { message },
                ..
            }) if message.contains("inconsistent frame provenance")
        ));
        assert!(!events.iter().any(
            |event| matches!(event, RunEvent::BlockEntered { block_id, .. } if block_id == "body")
        ));
    }

    struct GatedWatchDetector {
        blocked_source: String,
        started: mpsc::Sender<()>,
        release: Arc<(Mutex<bool>, Condvar)>,
    }

    struct FirstGatedSequenceDetector {
        started: mpsc::Sender<()>,
        release: Arc<(Mutex<bool>, Condvar)>,
        calls: AtomicU64,
        first_matched: bool,
    }

    impl ConditionDetector for FirstGatedSequenceDetector {
        fn observe(
            &self,
            _request: &super::super::ObservationRequest<'_>,
            capture: &(dyn CaptureSource + Send + Sync),
        ) -> Result<super::super::DetectorEvidence> {
            let call = self.calls.fetch_add(1, Ordering::AcqRel) + 1;
            if call == 1 {
                let _ = self.started.send(());
                let (lock, wake) = &*self.release;
                let mut released = lock.lock().unwrap();
                while !*released {
                    released = wake.wait(released).unwrap();
                }
            }
            let frame = capture.capture_frame(Rect::new(192, 108, 384, 216))?;
            let metadata = watch_frame_metadata(frame.metadata);
            let matched = self.first_matched || call > 1;
            Ok(super::super::DetectorEvidence {
                matched,
                frame_id: metadata.frame_id,
                captured_at_ms: metadata.captured_at_ms,
                match_rect: matched.then(|| Rect::new(1, 2, 3, 4)),
                score: matched.then_some(0.99),
                match_count: u32::from(matched),
                stable_frames: u8::from(matched),
                frame_metadata: Some(metadata),
                details: serde_json::Value::Null,
            })
        }
    }

    #[derive(Debug, Default)]
    struct PanickingWatchDetector;

    impl ConditionDetector for PanickingWatchDetector {
        fn observe(
            &self,
            _request: &super::super::ObservationRequest<'_>,
            _capture: &(dyn CaptureSource + Send + Sync),
        ) -> Result<super::super::DetectorEvidence> {
            panic!("injected detector panic")
        }
    }

    #[derive(Debug, Default)]
    struct PanickingCompletionClock;

    impl Clock for PanickingCompletionClock {
        fn now_ms(&self) -> u64 {
            panic!("injected completion clock panic")
        }
    }

    #[derive(Debug, Default)]
    struct PanickingCleanupDetector;

    impl ConditionDetector for PanickingCleanupDetector {
        fn observe(
            &self,
            request: &super::super::ObservationRequest<'_>,
            _capture: &(dyn CaptureSource + Send + Sync),
        ) -> Result<super::super::DetectorEvidence> {
            Ok(super::super::DetectorEvidence::unmatched(
                request.observed_at_ms,
                request.observed_at_ms,
            ))
        }

        fn run_finished(&self, _run_id: &str, _generations: &[u64]) {
            panic!("injected cleanup panic")
        }
    }

    struct GatedPanickingCleanupDetector {
        started: mpsc::Sender<()>,
        release: Arc<(Mutex<bool>, Condvar)>,
    }

    impl ConditionDetector for GatedPanickingCleanupDetector {
        fn observe(
            &self,
            request: &super::super::ObservationRequest<'_>,
            _capture: &(dyn CaptureSource + Send + Sync),
        ) -> Result<super::super::DetectorEvidence> {
            let _ = self.started.send(());
            let (lock, wake) = &*self.release;
            let mut released = lock.lock().unwrap();
            while !*released {
                released = wake.wait(released).unwrap();
            }
            Ok(super::super::DetectorEvidence::unmatched(
                request.observed_at_ms,
                request.observed_at_ms,
            ))
        }

        fn run_finished(&self, _run_id: &str, _generations: &[u64]) {
            panic!("injected deferred cleanup panic")
        }
    }

    impl ConditionDetector for GatedWatchDetector {
        fn observe(
            &self,
            request: &super::super::ObservationRequest<'_>,
            _capture: &(dyn CaptureSource + Send + Sync),
        ) -> Result<super::super::DetectorEvidence> {
            if condition_source_id(request.condition) == self.blocked_source {
                let _ = self.started.send(());
                let (lock, wake) = &*self.release;
                let mut released = lock.lock().unwrap();
                while !*released {
                    released = wake.wait(released).unwrap();
                }
            }
            Ok(super::super::DetectorEvidence::unmatched(
                request.observed_at_ms,
                request.observed_at_ms,
            ))
        }
    }

    fn direct_watch_job(
        run_id: &str,
        entry_id: u64,
        lane_id: &str,
        family: super::super::DetectorFamily,
        detector: Arc<dyn ConditionDetector>,
        compiled: &CompiledMacro,
        capture: &Arc<super::super::CapturedCycle>,
        completion: &SyncSender<WatchDetectorCompletion>,
    ) -> WatchDetectorJob {
        let job_id = NEXT_WATCH_JOB_ID.fetch_add(1, Ordering::Relaxed);
        WatchDetectorJob {
            job_id,
            key: WatchJobKey {
                run_id: run_id.to_string(),
                block_id: "direct-watch".to_string(),
                entry_id,
                lane_id: lane_id.to_string(),
            },
            lane_order: 0,
            family,
            generation: 1,
            side_effect_epoch: 0,
            condition: Condition::Text {
                source_block_id: lane_id.to_string(),
                rule_id: "text".to_string(),
                mode: ObserveMode::CheckNow,
            },
            compiled: compiled.clone(),
            observed_at_ms: job_id,
            capture: Arc::clone(capture),
            detector,
            clock: Arc::new(FakeClock::default()),
            completion: completion.clone(),
        }
    }

    fn isolated_text_watch_pool(enforce_health: bool) -> &'static WatchDetectorPool {
        let pool = Box::leak(Box::new(WatchDetectorPool {
            inner: Arc::new(WatchPoolInner {
                state: Mutex::new(WatchPoolState::default()),
                ready: Condvar::new(),
                started_workers: AtomicU64::new(0),
                next_enqueue_sequence: AtomicU64::new(1),
                live_text_workers: AtomicU64::new(0),
                live_image_workers: AtomicU64::new(0),
                enforce_health,
            }),
        }));
        spawn_watch_worker(Arc::clone(&pool.inner), super::super::DetectorFamily::Text);
        pool
    }

    fn isolated_full_watch_pool() -> &'static WatchDetectorPool {
        let pool = isolated_text_watch_pool(false);
        spawn_watch_worker(Arc::clone(&pool.inner), super::super::DetectorFamily::Image);
        spawn_watch_worker(Arc::clone(&pool.inner), super::super::DetectorFamily::Image);
        pool
    }

    fn wait_for_pool_idle(pool: &WatchDetectorPool) {
        let started = std::time::Instant::now();
        loop {
            let idle = {
                let state = lock_watch_pool_state(&pool.inner);
                state.active.is_empty() && state.pending.is_empty()
            };
            if idle {
                return;
            }
            assert!(
                started.elapsed() < Duration::from_secs(1),
                "Watch worker did not release its slot"
            );
            thread::sleep(Duration::from_millis(1));
        }
    }

    fn wait_for_counter(counter: &AtomicU64, minimum: u64) {
        let started = std::time::Instant::now();
        while counter.load(Ordering::Acquire) < minimum {
            assert!(
                started.elapsed() < Duration::from_secs(1),
                "counter did not reach {minimum}"
            );
            thread::sleep(Duration::from_millis(1));
        }
    }

    fn wait_for_cleanup_failures(pool: &WatchDetectorPool, minimum: usize) {
        let started = std::time::Instant::now();
        while lock_watch_pool_state(&pool.inner).cleanup_failures.len() < minimum {
            assert!(
                started.elapsed() < Duration::from_secs(1),
                "cleanup failure was not recorded"
            );
            thread::sleep(Duration::from_millis(1));
        }
    }

    #[test]
    fn macro_runtimes_share_exactly_three_process_global_watch_workers() {
        let first = fixture_runtime_with_detector(CountingUnmatchedDetector::default());
        let second = fixture_runtime_with_detector(CountingUnmatchedDetector::default());
        let cloned = first.clone();

        assert!(std::ptr::eq(first.watch_pool, second.watch_pool));
        assert!(std::ptr::eq(first.watch_pool, cloned.watch_pool));
        assert_eq!(first.watch_pool.worker_count(), 3);
        assert_eq!(
            first
                .watch_pool
                .inner
                .live_text_workers
                .load(Ordering::Acquire),
            1
        );
        assert_eq!(
            first
                .watch_pool
                .inner
                .live_image_workers
                .load(Ordering::Acquire),
            2
        );
    }

    #[test]
    fn detector_and_completion_clock_panics_are_typed_and_worker_slot_recovers() {
        let pool = isolated_text_watch_pool(false);
        let compiled = CompiledMacro::compile(saved(fixture_definition(vec![]))).unwrap();
        let capture = super::super::CapturedCycle::capture(
            Arc::new(FakeCapture),
            &[Rect::new(192, 108, 384, 216)],
        )
        .unwrap();

        let (panic_tx, panic_rx) = mpsc::sync_channel(1);
        let panic_job = direct_watch_job(
            "panic-detector",
            1,
            "lane",
            super::super::DetectorFamily::Text,
            Arc::new(PanickingWatchDetector),
            &compiled,
            &capture,
            &panic_tx,
        );
        pool.submit(panic_job).unwrap();
        assert!(matches!(
            panic_rx
                .recv_timeout(Duration::from_secs(1))
                .unwrap()
                .result,
            Err(WatchJobFailure::DetectorPanicked)
        ));
        wait_for_pool_idle(pool);

        let (clock_tx, clock_rx) = mpsc::sync_channel(1);
        let mut clock_job = direct_watch_job(
            "panic-clock",
            2,
            "lane",
            super::super::DetectorFamily::Text,
            Arc::new(CountingUnmatchedDetector::default()),
            &compiled,
            &capture,
            &clock_tx,
        );
        clock_job.clock = Arc::new(PanickingCompletionClock);
        pool.submit(clock_job).unwrap();
        assert!(matches!(
            clock_rx
                .recv_timeout(Duration::from_secs(1))
                .unwrap()
                .result,
            Err(WatchJobFailure::CompletionClockPanicked)
        ));
        wait_for_pool_idle(pool);

        let (normal_tx, normal_rx) = mpsc::sync_channel(1);
        pool.submit(direct_watch_job(
            "normal-after-panic",
            3,
            "lane",
            super::super::DetectorFamily::Text,
            Arc::new(CountingUnmatchedDetector::default()),
            &compiled,
            &capture,
            &normal_tx,
        ))
        .unwrap();
        assert!(
            normal_rx
                .recv_timeout(Duration::from_secs(1))
                .unwrap()
                .result
                .is_ok()
        );
        wait_for_pool_idle(pool);
        assert_eq!(pool.inner.started_workers.load(Ordering::Acquire), 1);
        assert_eq!(pool.inner.live_text_workers.load(Ordering::Acquire), 1);
    }

    #[test]
    fn detector_panic_fails_unlimited_watch_group_without_hanging() {
        let runtime = MacroRuntime::new(
            Arc::new(FakeCapture),
            Arc::new(PanickingWatchDetector),
            Arc::new(FrozenClock),
        );
        let mut watch = watch_group(
            vec![watch_lane("lane-1", vec![point_action("body")])],
            1,
            TimeoutOutcome::Continue,
        );
        if let BlockKind::WatchGroup { group } = &mut watch.kind {
            group.timeout_ms = Limit::Unlimited;
        }

        let events = runtime
            .run(saved(fixture_definition(vec![watch])), RunMode::DryRun)
            .unwrap();

        assert!(matches!(
            events.last(),
            Some(RunEvent::RunStopped {
                reason: StopReason::TechnicalFailure { message },
                ..
            }) if message.contains("panicked")
        ));
        assert!(!events.iter().any(
            |event| matches!(event, RunEvent::BlockEntered { block_id, .. } if block_id == "body")
        ));
    }

    #[test]
    fn cleanup_panic_is_contained_without_killing_worker() {
        let pool = isolated_text_watch_pool(false);
        pool.finish_run("cleanup-now", Arc::new(PanickingCleanupDetector), vec![1]);
        assert_eq!(lock_watch_pool_state(&pool.inner).cleanup_failures.len(), 1);

        let compiled = CompiledMacro::compile(saved(fixture_definition(vec![]))).unwrap();
        let capture = super::super::CapturedCycle::capture(
            Arc::new(FakeCapture),
            &[Rect::new(192, 108, 384, 216)],
        )
        .unwrap();
        let (started_tx, started_rx) = mpsc::channel();
        let release = Arc::new((Mutex::new(false), Condvar::new()));
        let deferred_detector: Arc<dyn ConditionDetector> =
            Arc::new(GatedPanickingCleanupDetector {
                started: started_tx,
                release: Arc::clone(&release),
            });
        let (deferred_tx, deferred_rx) = mpsc::sync_channel(1);
        pool.submit(direct_watch_job(
            "cleanup-later",
            1,
            "lane",
            super::super::DetectorFamily::Text,
            Arc::clone(&deferred_detector),
            &compiled,
            &capture,
            &deferred_tx,
        ))
        .unwrap();
        started_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        pool.finish_run("cleanup-later", deferred_detector, vec![1]);
        let (lock, wake) = &*release;
        *lock.lock().unwrap() = true;
        wake.notify_all();
        let _ = deferred_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        wait_for_pool_idle(pool);
        wait_for_cleanup_failures(pool, 2);

        let (completion_tx, completion_rx) = mpsc::sync_channel(1);
        pool.submit(direct_watch_job(
            "normal-after-cleanup-panic",
            1,
            "lane",
            super::super::DetectorFamily::Text,
            Arc::new(CountingUnmatchedDetector::default()),
            &compiled,
            &capture,
            &completion_tx,
        ))
        .unwrap();
        assert!(
            completion_rx
                .recv_timeout(Duration::from_secs(1))
                .unwrap()
                .result
                .is_ok()
        );
        wait_for_pool_idle(pool);
        assert_eq!(pool.inner.live_text_workers.load(Ordering::Acquire), 1);
    }

    #[test]
    fn disconnected_or_full_completion_channel_never_pins_worker() {
        let pool = isolated_text_watch_pool(false);
        let compiled = CompiledMacro::compile(saved(fixture_definition(vec![]))).unwrap();
        let capture = super::super::CapturedCycle::capture(
            Arc::new(FakeCapture),
            &[Rect::new(192, 108, 384, 216)],
        )
        .unwrap();
        let detector: Arc<dyn ConditionDetector> = Arc::new(CountingUnmatchedDetector::default());

        let (disconnected_tx, disconnected_rx) = mpsc::sync_channel(1);
        drop(disconnected_rx);
        pool.submit(direct_watch_job(
            "disconnected",
            1,
            "lane",
            super::super::DetectorFamily::Text,
            Arc::clone(&detector),
            &compiled,
            &capture,
            &disconnected_tx,
        ))
        .unwrap();
        wait_for_pool_idle(pool);

        let (full_tx, _full_rx) = mpsc::sync_channel(1);
        for lane in ["first", "second"] {
            pool.submit(direct_watch_job(
                "full-channel",
                2,
                lane,
                super::super::DetectorFamily::Text,
                Arc::clone(&detector),
                &compiled,
                &capture,
                &full_tx,
            ))
            .unwrap();
            wait_for_pool_idle(pool);
        }
        assert!(
            lock_watch_pool_state(&pool.inner)
                .run_failures
                .contains_key("full-channel")
        );

        let (normal_tx, normal_rx) = mpsc::sync_channel(1);
        pool.submit(direct_watch_job(
            "normal-after-channel-failure",
            3,
            "lane",
            super::super::DetectorFamily::Text,
            detector,
            &compiled,
            &capture,
            &normal_tx,
        ))
        .unwrap();
        assert!(
            normal_rx
                .recv_timeout(Duration::from_secs(1))
                .unwrap()
                .result
                .is_ok()
        );
        wait_for_pool_idle(pool);
    }

    #[test]
    fn pool_aging_uses_first_enqueue_sequence_not_runtime_clock_epoch() {
        let pool = Box::leak(Box::new(WatchDetectorPool {
            inner: Arc::new(WatchPoolInner {
                state: Mutex::new(WatchPoolState::default()),
                ready: Condvar::new(),
                started_workers: AtomicU64::new(0),
                next_enqueue_sequence: AtomicU64::new(1),
                live_text_workers: AtomicU64::new(0),
                live_image_workers: AtomicU64::new(0),
                enforce_health: false,
            }),
        }));
        let compiled = CompiledMacro::compile(saved(fixture_definition(vec![]))).unwrap();
        let capture = super::super::CapturedCycle::capture(
            Arc::new(FakeCapture),
            &[Rect::new(192, 108, 384, 216)],
        )
        .unwrap();
        let (completion_tx, _completion_rx) = mpsc::sync_channel(3);
        let detector: Arc<dyn ConditionDetector> = Arc::new(CountingUnmatchedDetector::default());
        let old_run = format!("old-{}", NEXT_RUN_ID.fetch_add(1, Ordering::Relaxed));
        let new_run = format!("new-{}", NEXT_RUN_ID.fetch_add(1, Ordering::Relaxed));

        let mut old = direct_watch_job(
            &old_run,
            1,
            "old-lane",
            super::super::DetectorFamily::Text,
            Arc::clone(&detector),
            &compiled,
            &capture,
            &completion_tx,
        );
        old.observed_at_ms = u64::MAX - 1;
        pool.submit(old).unwrap();
        let mut new = direct_watch_job(
            &new_run,
            1,
            "new-lane",
            super::super::DetectorFamily::Text,
            Arc::clone(&detector),
            &compiled,
            &capture,
            &completion_tx,
        );
        new.observed_at_ms = 0;
        pool.submit(new).unwrap();
        let replacement = direct_watch_job(
            &old_run,
            1,
            "old-lane",
            super::super::DetectorFamily::Text,
            detector,
            &compiled,
            &capture,
            &completion_tx,
        );
        pool.submit(replacement).unwrap();

        let state = pool.inner.state.lock().unwrap();
        let oldest = oldest_pending_key(&state, super::super::DetectorFamily::Text).unwrap();
        assert_eq!(oldest.run_id, old_run);
        assert!(
            state.pending[&oldest].enqueue_sequence
                < state
                    .pending
                    .iter()
                    .find(|(key, _)| key.run_id == new_run)
                    .unwrap()
                    .1
                    .enqueue_sequence
        );
    }

    #[test]
    fn degraded_worker_topology_and_enqueue_wrap_fail_closed() {
        let pool = Box::leak(Box::new(WatchDetectorPool {
            inner: Arc::new(WatchPoolInner {
                state: Mutex::new(WatchPoolState::default()),
                ready: Condvar::new(),
                started_workers: AtomicU64::new(0),
                next_enqueue_sequence: AtomicU64::new(1),
                live_text_workers: AtomicU64::new(0),
                live_image_workers: AtomicU64::new(0),
                enforce_health: true,
            }),
        }));
        let compiled = CompiledMacro::compile(saved(fixture_definition(vec![]))).unwrap();
        let capture = super::super::CapturedCycle::capture(
            Arc::new(FakeCapture),
            &[Rect::new(192, 108, 384, 216)],
        )
        .unwrap();
        let (completion_tx, _completion_rx) = mpsc::sync_channel(1);
        let job = direct_watch_job(
            "degraded",
            1,
            "lane",
            super::super::DetectorFamily::Text,
            Arc::new(CountingUnmatchedDetector::default()),
            &compiled,
            &capture,
            &completion_tx,
        );
        assert!(pool.submit(job).unwrap_err().message.contains("degraded"));

        pool.inner.live_text_workers.store(1, Ordering::Release);
        pool.inner
            .next_enqueue_sequence
            .store(u64::MAX, Ordering::Release);
        let wrap_job = direct_watch_job(
            "wrap",
            2,
            "lane",
            super::super::DetectorFamily::Text,
            Arc::new(CountingUnmatchedDetector::default()),
            &compiled,
            &capture,
            &completion_tx,
        );
        assert!(
            pool.submit(wrap_job)
                .unwrap_err()
                .message
                .contains("exhausted")
        );
    }

    #[test]
    fn first_candidate_fixes_arbitration_deadline_for_unlimited_and_finite_groups() {
        assert_eq!(establish_arbitration_deadline(None, 125, None), Some(125));
        assert_eq!(
            establish_arbitration_deadline(Some(125), 149, None),
            Some(125)
        );
        assert_eq!(
            establish_arbitration_deadline(None, 125, Some(120)),
            Some(120)
        );
        assert_eq!(
            establish_arbitration_deadline(Some(120), 119, Some(120)),
            Some(120)
        );
    }

    #[test]
    fn watch_scope_cleanup_removes_queued_work_on_every_exit_path() {
        let pool = Box::leak(Box::new(WatchDetectorPool {
            inner: Arc::new(WatchPoolInner {
                state: Mutex::new(WatchPoolState::default()),
                ready: Condvar::new(),
                started_workers: AtomicU64::new(0),
                next_enqueue_sequence: AtomicU64::new(1),
                live_text_workers: AtomicU64::new(0),
                live_image_workers: AtomicU64::new(0),
                enforce_health: false,
            }),
        }));
        let compiled = CompiledMacro::compile(saved(fixture_definition(vec![]))).unwrap();
        let capture = super::super::CapturedCycle::capture(
            Arc::new(FakeCapture),
            &[Rect::new(192, 108, 384, 216)],
        )
        .unwrap();
        let (completion_tx, _completion_rx) = mpsc::sync_channel(1);
        let run_id = format!(
            "direct-cleanup-{}",
            NEXT_RUN_ID.fetch_add(1, Ordering::Relaxed)
        );
        let entry_id = 77;
        let job = direct_watch_job(
            &run_id,
            entry_id,
            "queued",
            super::super::DetectorFamily::Text,
            Arc::new(CountingUnmatchedDetector::default()),
            &compiled,
            &capture,
            &completion_tx,
        );
        assert_eq!(
            pool.submit(job).unwrap(),
            super::super::SubmitOutcome::Started
        );
        assert_eq!(pool.inner.state.lock().unwrap().pending.len(), 1);

        drop(WatchScopeCleanup {
            pool,
            run_id,
            entry_id,
        });

        assert!(pool.inner.state.lock().unwrap().pending.is_empty());
    }

    #[test]
    fn slow_ocr_job_does_not_block_unrelated_image_worker_completion() {
        let pool = isolated_full_watch_pool();
        let compiled = CompiledMacro::compile(saved(fixture_definition(vec![]))).unwrap();
        let capture = super::super::CapturedCycle::capture(
            Arc::new(FakeCapture),
            &[Rect::new(192, 108, 384, 216)],
        )
        .unwrap();
        let (started_tx, started_rx) = mpsc::channel();
        let release = Arc::new((Mutex::new(false), Condvar::new()));
        let detector: Arc<dyn ConditionDetector> = Arc::new(GatedWatchDetector {
            blocked_source: "slow".to_string(),
            started: started_tx,
            release: Arc::clone(&release),
        });
        let (completion_tx, completion_rx) = mpsc::sync_channel(3);
        let run_id = format!(
            "direct-slow-{}",
            NEXT_RUN_ID.fetch_add(1, Ordering::Relaxed)
        );

        let slow = direct_watch_job(
            &run_id,
            1,
            "slow",
            super::super::DetectorFamily::Text,
            Arc::clone(&detector),
            &compiled,
            &capture,
            &completion_tx,
        );
        assert_eq!(
            pool.submit(slow).unwrap(),
            super::super::SubmitOutcome::Started
        );
        started_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        let fast = direct_watch_job(
            &run_id,
            1,
            "fast",
            super::super::DetectorFamily::Image,
            detector,
            &compiled,
            &capture,
            &completion_tx,
        );
        pool.submit(fast).unwrap();

        let fast_completion = completion_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        assert_eq!(fast_completion.key.lane_id, "fast");
        let (lock, wake) = &*release;
        *lock.lock().unwrap() = true;
        wake.notify_all();
        let slow_completion = completion_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        assert_eq!(slow_completion.key.lane_id, "slow");
    }

    #[test]
    fn stopped_run_returns_without_waiting_for_slow_detector_worker() {
        let (started_tx, started_rx) = mpsc::channel();
        let release = Arc::new((Mutex::new(false), Condvar::new()));
        let detector = Arc::new(GatedWatchDetector {
            blocked_source: "lane-1".to_string(),
            started: started_tx,
            release: Arc::clone(&release),
        });
        let mut runtime = MacroRuntime::new(Arc::new(FakeCapture), detector, Arc::new(FrozenClock));
        runtime.watch_pool = isolated_text_watch_pool(false);
        let mut watch = watch_group(
            vec![watch_lane("lane-1", vec![point_action("body")])],
            1,
            TimeoutOutcome::Continue,
        );
        if let BlockKind::WatchGroup { group } = &mut watch.kind {
            group.timeout_ms = Limit::Unlimited;
        }
        let revision = saved(fixture_definition(vec![watch]));
        let runner = runtime.clone();
        let (done_tx, done_rx) = mpsc::channel();
        let handle = thread::spawn(move || {
            let _ = done_tx.send(runner.run(revision, RunMode::DryRun));
        });
        started_rx.recv_timeout(Duration::from_secs(1)).unwrap();

        runtime.stop();
        let events = done_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("runtime waited for a slow detector")
            .unwrap();
        assert!(events.iter().any(|event| matches!(
            event,
            RunEvent::RunStopped {
                reason: StopReason::UserStopped,
                ..
            }
        )));
        assert!(!events.iter().any(
            |event| matches!(event, RunEvent::BlockEntered { block_id, .. } if block_id == "body")
        ));

        let (lock, wake) = &*release;
        *lock.lock().unwrap() = true;
        wake.notify_all();
        handle.join().unwrap();
    }

    #[test]
    fn replacing_pending_frames_never_invalidates_active_completion() {
        let clock = Arc::new(ManualClock::default());
        let capture = Arc::new(WatchCountingCapture::default());
        let (started_tx, started_rx) = mpsc::channel();
        let release = Arc::new((Mutex::new(false), Condvar::new()));
        let detector = Arc::new(FirstGatedSequenceDetector {
            started: started_tx,
            release: Arc::clone(&release),
            calls: AtomicU64::new(0),
            first_matched: false,
        });
        let mut runtime = MacroRuntime::new(capture.clone(), detector.clone(), clock.clone());
        runtime.watch_pool = isolated_text_watch_pool(false);
        let mut watch = watch_group(
            vec![watch_lane(
                "lane-1",
                vec![block(
                    "body",
                    BlockKind::Comment {
                        text: "winner".to_string(),
                    },
                )],
            )],
            1,
            TimeoutOutcome::Continue,
        );
        if let BlockKind::WatchGroup { group } = &mut watch.kind {
            group.timeout_ms = Limit::Unlimited;
        }
        let revision = saved(fixture_definition(vec![watch]));
        let (done_tx, done_rx) = mpsc::channel();
        let handle = thread::spawn(move || {
            let _ = done_tx.send(runtime.run(revision, RunMode::DryRun));
        });
        started_rx.recv_timeout(Duration::from_secs(1)).unwrap();

        clock.set(60);
        wait_for_counter(&capture.0, 2);
        clock.set(120);
        wait_for_counter(&capture.0, 3);
        clock.set(121);
        let (lock, wake) = &*release;
        *lock.lock().unwrap() = true;
        wake.notify_all();
        wait_for_counter(&detector.calls, 2);
        thread::sleep(Duration::from_millis(20));
        clock.set(146);

        let events = done_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("Watch Group hung after active/pending replacement")
            .unwrap();
        handle.join().unwrap();
        let observed_frames: Vec<_> = events
            .iter()
            .filter_map(|event| match event {
                RunEvent::ObservationCompleted {
                    block_id, evidence, ..
                } if block_id == "lane-1" => Some(evidence.frame_id),
                _ => None,
            })
            .collect();
        assert_eq!(observed_frames.first(), Some(&1), "{events:#?}");
        assert!(observed_frames.iter().any(|frame_id| *frame_id >= 3));
        assert!(events.iter().any(
            |event| matches!(event, RunEvent::BlockEntered { block_id, .. } if block_id == "body")
        ));
    }

    #[test]
    fn newer_matched_completion_preserves_qualified_candidate_until_arbitration() {
        let clock = Arc::new(ManualClock::default());
        let capture = Arc::new(WatchCountingCapture::default());
        let (started_tx, started_rx) = mpsc::channel();
        let release = Arc::new((Mutex::new(false), Condvar::new()));
        let detector = Arc::new(FirstGatedSequenceDetector {
            started: started_tx,
            release: Arc::clone(&release),
            calls: AtomicU64::new(0),
            first_matched: true,
        });
        let mut runtime = MacroRuntime::new(capture.clone(), detector.clone(), clock.clone());
        runtime.watch_pool = isolated_text_watch_pool(false);
        let mut watch = watch_group(
            vec![watch_lane(
                "lane-1",
                vec![block(
                    "body",
                    BlockKind::Comment {
                        text: "winner".to_string(),
                    },
                )],
            )],
            1,
            TimeoutOutcome::Continue,
        );
        if let BlockKind::WatchGroup { group } = &mut watch.kind {
            group.timeout_ms = Limit::Unlimited;
        }
        let revision = saved(fixture_definition(vec![watch]));
        let (done_tx, done_rx) = mpsc::channel();
        let handle = thread::spawn(move || {
            let _ = done_tx.send(runtime.run(revision, RunMode::DryRun));
        });
        started_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        clock.set(60);
        wait_for_counter(&capture.0, 2);
        clock.set(61);
        let (lock, wake) = &*release;
        *lock.lock().unwrap() = true;
        wake.notify_all();
        wait_for_counter(&detector.calls, 2);
        thread::sleep(Duration::from_millis(20));
        clock.set(86);

        let events = done_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("consecutive matched completions invalidated the candidate")
            .unwrap();
        handle.join().unwrap();
        assert!(events.iter().any(
            |event| matches!(event, RunEvent::BlockEntered { block_id, .. } if block_id == "body")
        ));
        assert!(!events.iter().any(|event| matches!(
            event,
            RunEvent::RunStopped {
                reason: StopReason::TechnicalFailure { .. },
                ..
            }
        )));
    }

    #[test]
    fn active_lane_keeps_only_newest_pending_detector_job() {
        let pool = isolated_text_watch_pool(false);
        let compiled = CompiledMacro::compile(saved(fixture_definition(vec![]))).unwrap();
        let capture = super::super::CapturedCycle::capture(
            Arc::new(FakeCapture),
            &[Rect::new(192, 108, 384, 216)],
        )
        .unwrap();
        let (started_tx, started_rx) = mpsc::channel();
        let release = Arc::new((Mutex::new(false), Condvar::new()));
        let detector: Arc<dyn ConditionDetector> = Arc::new(GatedWatchDetector {
            blocked_source: "lane".to_string(),
            started: started_tx,
            release: Arc::clone(&release),
        });
        let (completion_tx, completion_rx) = mpsc::sync_channel(3);
        let run_id = format!(
            "direct-newest-{}",
            NEXT_RUN_ID.fetch_add(1, Ordering::Relaxed)
        );
        let first = direct_watch_job(
            &run_id,
            2,
            "lane",
            super::super::DetectorFamily::Text,
            Arc::clone(&detector),
            &compiled,
            &capture,
            &completion_tx,
        );
        let first_id = first.job_id;
        pool.submit(first).unwrap();
        started_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        let second = direct_watch_job(
            &run_id,
            2,
            "lane",
            super::super::DetectorFamily::Text,
            Arc::clone(&detector),
            &compiled,
            &capture,
            &completion_tx,
        );
        let second_id = second.job_id;
        assert_eq!(
            pool.submit(second).unwrap(),
            super::super::SubmitOutcome::Pending
        );
        let third = direct_watch_job(
            &run_id,
            2,
            "lane",
            super::super::DetectorFamily::Text,
            detector,
            &compiled,
            &capture,
            &completion_tx,
        );
        let third_id = third.job_id;
        assert_eq!(
            pool.submit(third).unwrap(),
            super::super::SubmitOutcome::ReplacedPending {
                dropped_frame_id: capture.frame_id(),
            }
        );
        assert_ne!(second_id, third_id);
        let (lock, wake) = &*release;
        *lock.lock().unwrap() = true;
        wake.notify_all();

        let completed = [
            completion_rx
                .recv_timeout(Duration::from_secs(1))
                .unwrap()
                .job_id,
            completion_rx
                .recv_timeout(Duration::from_secs(1))
                .unwrap()
                .job_id,
        ];
        assert!(completed.contains(&first_id));
        assert!(completed.contains(&third_id));
        assert!(!completed.contains(&second_id));
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
            | RunEvent::ArbitrationCompleted { run_id, .. }
            | RunEvent::PollingDelayed { run_id, .. }
            | RunEvent::Error { run_id, .. }
            | RunEvent::RunStopped { run_id, .. } => run_id,
        }
    }
}
