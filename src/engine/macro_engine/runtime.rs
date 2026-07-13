use std::{
    collections::{HashMap, HashSet, VecDeque},
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::{self, Receiver, SyncSender, TrySendError},
    },
    time::Duration,
};

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::engine::automation::{CaptureSource, Clock};

use super::{
    Action, Block, BlockKind, Condition, ConditionDetector, DetectorEvidence, DetectorKind,
    JournalKind, JournalRecord, Limit, MacroDefinition, ObservationRequest, ObservationToken,
    ObserveMode, PinnedAsset, SavedRevision, TimeoutOutcome, if_once_decision,
    observation_satisfies_mode, repeat_n_decision, validate_macro,
};

static NEXT_RUN_ID: AtomicU64 = AtomicU64::new(1);
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

        Ok(Self {
            definition: Arc::new(saved.definition),
            definition_hash: saved.definition_hash,
            pinned_assets: saved.pinned_assets.into(),
        })
    }

    pub fn definition(&self) -> &MacroDefinition {
        &self.definition
    }
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
            action_count: 0,
            paused_event_emitted: false,
        };
        let reason = execution
            .execute_blocks(&blocks)
            .unwrap_or(StopReason::Completed);
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
    action_count: u64,
    paused_event_emitted: bool,
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
        if is_click_action(action) {
            self.action_count = self.action_count.saturating_add(1);
            if exceeds_limit(
                self.action_count,
                &self.compiled.definition().safety.max_clicks,
            ) {
                return Some(StopReason::SafetyLimit {
                    message: "maximum click count exceeded".to_string(),
                });
            }
        }
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

fn is_click_action(action: &Action) -> bool {
    matches!(
        action,
        Action::ClickTextMatch { .. }
            | Action::ClickImageMatch { .. }
            | Action::ClickPoint { .. }
            | Action::ClickRegion { .. }
    )
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
            AssetRef, FocusLossPolicy, ImageRule, Limit, MACRO_SCHEMA_VERSION,
            MatchSelectionPolicy, ObserveMode, PointDefinition, PreprocessProfile,
            RegionDefinition, SafetyPolicy, TargetProfile, TextMatchMode, TextRule, TimeoutOutcome,
        },
        types::{PointRatio, Rect, RectRatio, ScreenImage},
    };
    use std::{collections::VecDeque, thread};

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
                details: serde_json::json!({ "fixture": true }),
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

    fn fixture_runtime_with_detector(detector: FakeDetector) -> MacroRuntime {
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
        let bytes = b"immutable-template".to_vec();
        let asset = AssetRef {
            id: "template".to_string(),
            revision: 1,
            content_hash: sha256_hex(&bytes),
        };
        let mut definition = fixture_definition(vec![]);
        definition.image_rules.push(ImageRule {
            id: "image".to_string(),
            revision: 1,
            region_id: "region".to_string(),
            template: asset.clone(),
            transparent_mask: None,
            threshold: 0.95,
            scales_percent: vec![100],
            stable_frames: 1,
            maximum_center_drift_px: 2,
            minimum_runner_up_margin: 0.05,
            match_policy: MatchSelectionPolicy::ExactlyOne,
            poll_interval_ms: 1,
            timeout_ms: Limit::Finite(10),
        });
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
            definition.image_rules.push(ImageRule {
                id: id.to_string(),
                revision: 1,
                region_id: "region".to_string(),
                template,
                transparent_mask: None,
                threshold: 0.95,
                scales_percent: vec![100],
                stable_frames: 1,
                maximum_center_drift_px: 2,
                minimum_runner_up_margin: 0.05,
                match_policy: MatchSelectionPolicy::ExactlyOne,
                poll_interval_ms: 1,
                timeout_ms: Limit::Finite(10),
            });
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
