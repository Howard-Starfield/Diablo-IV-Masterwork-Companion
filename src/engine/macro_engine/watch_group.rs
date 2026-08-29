use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Mutex},
};

use anyhow::{Context, Result, ensure};
use image::imageops;

use crate::engine::{
    automation::{CaptureSource, CapturedScreenFrame},
    macro_engine::ObservationToken,
    types::{Rect, ScreenImage},
};

pub const ARBITRATION_WINDOW_MS: u64 = 25;
pub const MAX_WATCH_GROUP_LANES: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaneState {
    Enter,
    Observe,
    Qualify,
    Arbitrate,
    Commit,
    Execute,
    Settle,
    Exit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SafetyBypass {
    EmergencyStop,
    Cancelled,
    TargetInvalidated,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CandidateEvent {
    pub lane_id: String,
    pub lane_order: usize,
    pub ready_at_ms: u64,
    pub completed_at_ms: u64,
    pub token: ObservationToken,
}

impl CandidateEvent {
    pub fn new(
        run_id: impl Into<String>,
        generation: u64,
        lane_id: impl Into<String>,
        lane_order: usize,
        ready_at_ms: u64,
        frame_id: u64,
    ) -> Self {
        let lane_id = lane_id.into();
        Self {
            token: ObservationToken {
                run_id: run_id.into(),
                generation,
                side_effect_epoch: 0,
                source_block_id: lane_id.clone(),
                detector: super::DetectorKind::Text,
                region_id: "test-region".to_string(),
                region_revision: 0,
                rule_id: "test-rule".to_string(),
                rule_revision: 0,
                frame_id,
                captured_at_ms: ready_at_ms,
                match_rect: None,
                score: None,
                match_count: 1,
                stable_frames: 1,
                frame_metadata: None,
                evidence: serde_json::Value::Null,
            },
            lane_id,
            lane_order,
            ready_at_ms,
            completed_at_ms: ready_at_ms,
        }
    }

    pub fn from_observation(
        lane_id: impl Into<String>,
        lane_order: usize,
        ready_at_ms: u64,
        token: &ObservationToken,
    ) -> Self {
        Self {
            lane_id: lane_id.into(),
            lane_order,
            ready_at_ms,
            completed_at_ms: ready_at_ms,
            token: token.clone(),
        }
    }

    pub fn matches_observation(&self, token: &ObservationToken) -> bool {
        token == &self.token
    }

    #[cfg(test)]
    fn for_test(
        run_id: &str,
        generation: u64,
        lane_id: &str,
        lane_order: usize,
        ready_at_ms: u64,
        frame_id: u64,
    ) -> Self {
        Self::new(
            run_id,
            generation,
            lane_id,
            lane_order,
            ready_at_ms,
            frame_id,
        )
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ArbitrationResult {
    pub winner: Option<CandidateEvent>,
    pub discarded_lane_ids: Vec<String>,
    pub safety_bypassed: bool,
}

pub fn arbitrate_candidates(
    mut candidates: Vec<CandidateEvent>,
    safety: Option<SafetyBypass>,
) -> ArbitrationResult {
    candidates.sort_by(|left, right| {
        left.ready_at_ms
            .cmp(&right.ready_at_ms)
            .then_with(|| left.lane_order.cmp(&right.lane_order))
            .then_with(|| left.lane_id.cmp(&right.lane_id))
    });
    if safety.is_some() {
        return ArbitrationResult {
            winner: None,
            discarded_lane_ids: candidates
                .into_iter()
                .map(|candidate| candidate.lane_id)
                .collect(),
            safety_bypassed: true,
        };
    }
    let Some(first_ready_at) = candidates.first().map(|candidate| candidate.ready_at_ms) else {
        return ArbitrationResult {
            winner: None,
            discarded_lane_ids: Vec::new(),
            safety_bypassed: false,
        };
    };
    let deadline = first_ready_at.saturating_add(ARBITRATION_WINDOW_MS);
    let winner_index = candidates
        .iter()
        .enumerate()
        .filter(|(_, candidate)| candidate.ready_at_ms <= deadline)
        .min_by(|(_, left), (_, right)| {
            left.lane_order
                .cmp(&right.lane_order)
                .then_with(|| left.lane_id.cmp(&right.lane_id))
        })
        .map(|(index, _)| index)
        .expect("first candidate is inside its arbitration window");
    let winner = candidates.remove(winner_index);
    ArbitrationResult {
        winner: Some(winner),
        discarded_lane_ids: candidates
            .into_iter()
            .map(|candidate| candidate.lane_id)
            .collect(),
        safety_bypassed: false,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LatchDecision {
    Qualified,
    Latched,
    Rearmed,
    Unmatched,
    Stale,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LaneLatch {
    latched_at_frame_id: Option<u64>,
    latest_frame_id: Option<u64>,
}

impl LaneLatch {
    pub fn observe(&mut self, matched: bool, frame_id: u64) -> LatchDecision {
        if self
            .latest_frame_id
            .is_some_and(|latest| frame_id <= latest)
        {
            return LatchDecision::Stale;
        }
        self.latest_frame_id = Some(frame_id);
        if let Some(latched_at) = self.latched_at_frame_id {
            if matched {
                return LatchDecision::Latched;
            }
            if frame_id <= latched_at {
                return LatchDecision::Stale;
            }
            self.latched_at_frame_id = None;
            return LatchDecision::Rearmed;
        }
        if matched {
            self.latched_at_frame_id = Some(frame_id);
            LatchDecision::Qualified
        } else {
            LatchDecision::Unmatched
        }
    }

    fn qualified_at(&self, frame_id: u64) -> bool {
        self.latched_at_frame_id == Some(frame_id) && self.latest_frame_id == Some(frame_id)
    }
}

#[derive(Debug)]
pub struct WatchGroupRunner {
    run_id: String,
    generation: u64,
    candidate_capacity: usize,
    candidates: Vec<CandidateEvent>,
    latches: HashMap<String, LaneLatch>,
    state: LaneState,
    lifecycle: Vec<LaneState>,
    body_running: bool,
}

impl WatchGroupRunner {
    pub fn new(run_id: impl Into<String>, generation: u64, candidate_capacity: usize) -> Self {
        assert!(candidate_capacity > 0);
        Self {
            run_id: run_id.into(),
            generation,
            candidate_capacity,
            candidates: Vec::with_capacity(candidate_capacity),
            latches: HashMap::new(),
            state: LaneState::Enter,
            lifecycle: vec![LaneState::Enter],
            body_running: false,
        }
    }

    pub fn qualify(&mut self, candidate: CandidateEvent) -> Result<(), &'static str> {
        self.validate_candidate(&candidate)?;
        let decision = self
            .latches
            .entry(candidate.lane_id.clone())
            .or_default()
            .observe(true, candidate.token.frame_id);
        if !matches!(decision, LatchDecision::Qualified) {
            return Err("lane is latched or candidate frame is stale");
        }
        self.enqueue_candidate(candidate);
        Ok(())
    }

    pub(crate) fn qualify_preobserved(
        &mut self,
        candidate: CandidateEvent,
    ) -> Result<(), &'static str> {
        self.validate_candidate(&candidate)?;
        if !self
            .latches
            .get(&candidate.lane_id)
            .is_some_and(|latch| latch.qualified_at(candidate.token.frame_id))
        {
            return Err("candidate was not qualified by the current frame");
        }
        self.enqueue_candidate(candidate);
        Ok(())
    }

    fn validate_candidate(&self, candidate: &CandidateEvent) -> Result<(), &'static str> {
        if candidate.token.run_id != self.run_id || candidate.token.generation != self.generation {
            return Err("candidate is stale for the current run generation");
        }
        if self.body_running {
            return Err("ordinary matches cannot preempt a running body");
        }
        if self.candidates.len() >= self.candidate_capacity {
            return Err("candidate queue is full");
        }
        if self
            .candidates
            .iter()
            .any(|current| current.lane_id == candidate.lane_id)
        {
            return Err("lane already has a candidate");
        }
        Ok(())
    }

    fn enqueue_candidate(&mut self, candidate: CandidateEvent) {
        self.candidates.push(candidate);
        self.transition(LaneState::Qualify);
    }

    pub fn observe_latch(&mut self, lane_id: &str, matched: bool, frame_id: u64) -> LatchDecision {
        self.latches
            .entry(lane_id.to_string())
            .or_default()
            .observe(matched, frame_id)
    }

    pub fn revoke_candidate(&mut self, lane_id: &str) -> bool {
        let before = self.candidates.len();
        self.candidates
            .retain(|candidate| candidate.lane_id != lane_id);
        before != self.candidates.len()
    }

    pub fn has_candidates(&self) -> bool {
        !self.candidates.is_empty()
    }

    pub(crate) fn candidate_lane_ids(&self) -> Vec<String> {
        self.candidates
            .iter()
            .map(|candidate| candidate.lane_id.clone())
            .collect()
    }

    pub fn arbitrate(&mut self, safety: Option<SafetyBypass>) -> ArbitrationResult {
        self.transition(LaneState::Arbitrate);
        let result = arbitrate_candidates(std::mem::take(&mut self.candidates), safety);
        if result.winner.is_some() {
            self.transition(LaneState::Commit);
        } else {
            self.transition(LaneState::Exit);
        }
        result
    }

    pub fn queued_action_count(&self) -> usize {
        0
    }

    pub fn begin_execution(&mut self) {
        self.body_running = true;
        self.transition(LaneState::Execute);
    }

    pub fn settle_and_exit(&mut self) {
        self.body_running = false;
        self.candidates.clear();
        self.transition(LaneState::Settle);
        self.transition(LaneState::Exit);
    }

    pub fn invalidate(&mut self, generation: u64) {
        self.generation = generation;
        self.candidates.clear();
        self.body_running = false;
        self.transition(LaneState::Observe);
    }

    pub fn reset_for_run(&mut self, run_id: impl Into<String>, generation: u64) {
        self.run_id = run_id.into();
        self.generation = generation;
        self.candidates.clear();
        self.latches.clear();
        self.body_running = false;
        self.state = LaneState::Enter;
        self.lifecycle.clear();
        self.lifecycle.push(LaneState::Enter);
    }

    pub fn lifecycle(&self) -> &[LaneState] {
        &self.lifecycle
    }

    fn transition(&mut self, state: LaneState) {
        self.state = state;
        self.lifecycle.push(state);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DetectorFamily {
    Text,
    Image,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DetectorJob {
    pub lane_id: String,
    pub family: DetectorFamily,
    pub frame_id: u64,
    pub enqueued_at_ms: u64,
}

impl DetectorJob {
    #[cfg(test)]
    fn for_test(lane_id: &str, family: DetectorFamily, frame_id: u64, enqueued_at_ms: u64) -> Self {
        Self {
            lane_id: lane_id.to_string(),
            family,
            frame_id,
            enqueued_at_ms,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubmitOutcome {
    Started,
    Pending,
    ReplacedPending { dropped_frame_id: u64 },
    PollingDelayed,
}

#[derive(Debug)]
pub struct DetectorScheduler {
    text_capacity: usize,
    image_capacity: usize,
    lane_capacity: usize,
    active: HashMap<String, DetectorJob>,
    pending: HashMap<String, DetectorJob>,
    delayed: usize,
}

#[derive(Debug, Clone)]
struct SharedCapture {
    rect: Rect,
    frame: CapturedScreenFrame,
}

/// One immutable capture batch shared by every asynchronous detector job from a poll.
#[derive(Clone)]
pub struct CapturedCycle {
    source: Arc<dyn CaptureSource + Send + Sync>,
    shared: Arc<SharedCapture>,
}

impl CapturedCycle {
    pub fn capture(
        source: Arc<dyn CaptureSource + Send + Sync>,
        regions: &[Rect],
    ) -> Result<Arc<Self>> {
        ensure!(
            !regions.is_empty(),
            "capture cycle requires at least one region"
        );
        let rect = regions.iter().copied().try_fold(regions[0], union_rect)?;
        let frame = source.capture_frame(rect)?;
        ensure!(
            frame.image.rgba.width() == rect.width && frame.image.rgba.height() == rect.height,
            "capture source returned pixels with unexpected dimensions"
        );
        Ok(Arc::new(Self {
            source,
            shared: Arc::new(SharedCapture { rect, frame }),
        }))
    }

    pub fn frame_id(&self) -> u64 {
        self.shared.frame.metadata.frame_id
    }

    pub fn metadata(&self) -> crate::engine::automation::CaptureFrameMetadata {
        self.shared.frame.metadata
    }

    pub fn validate_fresh(&self) -> Result<()> {
        self.source
            .validate_frame(self.shared.rect, &self.shared.frame.metadata)
    }

    fn crop(&self, rect: Rect) -> Result<Option<CapturedScreenFrame>> {
        if !contains_rect(self.shared.rect, rect) {
            return Ok(None);
        }
        let offset_x = u32::try_from(i64::from(rect.x) - i64::from(self.shared.rect.x))
            .context("capture crop x offset is negative")?;
        let offset_y = u32::try_from(i64::from(rect.y) - i64::from(self.shared.rect.y))
            .context("capture crop y offset is negative")?;
        let rgba = imageops::crop_imm(
            &self.shared.frame.image.rgba,
            offset_x,
            offset_y,
            rect.width,
            rect.height,
        )
        .to_image();
        Ok(Some(CapturedScreenFrame {
            image: ScreenImage::new(rgba),
            metadata: self.shared.frame.metadata,
        }))
    }
}

impl CaptureSource for CapturedCycle {
    fn capture(&self, rect: Rect) -> Result<ScreenImage> {
        self.crop(rect)?
            .map(|frame| frame.image)
            .context("detector requested pixels outside the immutable capture cycle")
    }

    fn capture_frame(&self, rect: Rect) -> Result<CapturedScreenFrame> {
        self.crop(rect)?
            .context("detector requested a frame outside the immutable capture cycle")
    }

    fn validate_frame(
        &self,
        rect: Rect,
        metadata: &crate::engine::automation::CaptureFrameMetadata,
    ) -> Result<()> {
        ensure!(
            contains_rect(self.shared.rect, rect),
            "validation region is outside capture cycle"
        );
        ensure!(
            metadata == &self.shared.frame.metadata,
            "capture metadata changed"
        );
        self.validate_fresh()
    }
}

/// Run-owned capture cache. `begin_cycle` captures the union of compatible lane regions once;
/// every detector receives an immutable crop carrying the same frame identity.
pub struct CaptureCoordinator {
    source: Arc<dyn CaptureSource + Send + Sync>,
    shared: Mutex<Option<SharedCapture>>,
}

impl CaptureCoordinator {
    pub fn new(source: Arc<dyn CaptureSource + Send + Sync>) -> Self {
        Self {
            source,
            shared: Mutex::new(None),
        }
    }

    pub fn begin_cycle(&self, regions: &[Rect]) -> Result<()> {
        let cycle = CapturedCycle::capture(Arc::clone(&self.source), regions)?;
        *self.shared.lock().expect("capture coordinator poisoned") = Some((*cycle.shared).clone());
        Ok(())
    }

    pub fn captured_cycle(&self, regions: &[Rect]) -> Result<Arc<CapturedCycle>> {
        CapturedCycle::capture(Arc::clone(&self.source), regions)
    }

    pub fn invalidate(&self) {
        *self.shared.lock().expect("capture coordinator poisoned") = None;
    }

    fn shared_crop(&self, rect: Rect) -> Result<Option<CapturedScreenFrame>> {
        let shared = self.shared.lock().expect("capture coordinator poisoned");
        let Some(shared) = shared.as_ref() else {
            return Ok(None);
        };
        if !contains_rect(shared.rect, rect) {
            return Ok(None);
        }
        let offset_x = u32::try_from(i64::from(rect.x) - i64::from(shared.rect.x))
            .context("capture crop x offset is negative")?;
        let offset_y = u32::try_from(i64::from(rect.y) - i64::from(shared.rect.y))
            .context("capture crop y offset is negative")?;
        let rgba = imageops::crop_imm(
            &shared.frame.image.rgba,
            offset_x,
            offset_y,
            rect.width,
            rect.height,
        )
        .to_image();
        Ok(Some(CapturedScreenFrame {
            image: ScreenImage::new(rgba),
            metadata: shared.frame.metadata,
        }))
    }
}

impl CaptureSource for CaptureCoordinator {
    fn capture(&self, rect: Rect) -> Result<ScreenImage> {
        if let Some(frame) = self.shared_crop(rect)? {
            return Ok(frame.image);
        }
        self.source.capture(rect)
    }

    fn capture_frame(&self, rect: Rect) -> Result<CapturedScreenFrame> {
        if let Some(frame) = self.shared_crop(rect)? {
            return Ok(frame);
        }
        self.source.capture_frame(rect)
    }
}

fn contains_rect(outer: Rect, inner: Rect) -> bool {
    let outer_right = i64::from(outer.x) + i64::from(outer.width);
    let outer_bottom = i64::from(outer.y) + i64::from(outer.height);
    let inner_right = i64::from(inner.x) + i64::from(inner.width);
    let inner_bottom = i64::from(inner.y) + i64::from(inner.height);
    i64::from(inner.x) >= i64::from(outer.x)
        && i64::from(inner.y) >= i64::from(outer.y)
        && inner_right <= outer_right
        && inner_bottom <= outer_bottom
}

fn union_rect(left: Rect, right: Rect) -> Result<Rect> {
    let x = i64::from(left.x).min(i64::from(right.x));
    let y = i64::from(left.y).min(i64::from(right.y));
    let right_edge = (i64::from(left.x) + i64::from(left.width))
        .max(i64::from(right.x) + i64::from(right.width));
    let bottom_edge = (i64::from(left.y) + i64::from(left.height))
        .max(i64::from(right.y) + i64::from(right.height));
    Ok(Rect::new(
        i32::try_from(x).context("combined capture x is outside i32")?,
        i32::try_from(y).context("combined capture y is outside i32")?,
        u32::try_from(right_edge - x).context("combined capture width is outside u32")?,
        u32::try_from(bottom_edge - y).context("combined capture height is outside u32")?,
    ))
}

impl DetectorScheduler {
    pub fn new(text_capacity: usize, image_capacity: usize, lane_capacity: usize) -> Self {
        assert!(text_capacity > 0 && image_capacity > 0 && lane_capacity > 0);
        Self {
            text_capacity,
            image_capacity,
            lane_capacity,
            active: HashMap::new(),
            pending: HashMap::new(),
            delayed: 0,
        }
    }

    pub fn submit(&mut self, job: DetectorJob) -> SubmitOutcome {
        if let Some(pending) = self.pending.insert(job.lane_id.clone(), job.clone()) {
            return SubmitOutcome::ReplacedPending {
                dropped_frame_id: pending.frame_id,
            };
        }
        if self.active.contains_key(&job.lane_id) {
            return SubmitOutcome::Pending;
        }
        self.pending.remove(&job.lane_id);
        let active_family = self
            .active
            .values()
            .filter(|active| active.family == job.family)
            .count();
        let family_capacity = match job.family {
            DetectorFamily::Text => self.text_capacity,
            DetectorFamily::Image => self.image_capacity,
        };
        if active_family < family_capacity {
            self.active.insert(job.lane_id.clone(), job);
            return SubmitOutcome::Started;
        }
        let known: HashSet<_> = self.active.keys().chain(self.pending.keys()).collect();
        if known.len() >= self.lane_capacity {
            self.delayed = self.delayed.saturating_add(1);
            return SubmitOutcome::PollingDelayed;
        }
        self.pending.insert(job.lane_id.clone(), job);
        SubmitOutcome::Pending
    }

    pub fn complete(&mut self, lane_id: &str) -> Option<DetectorJob> {
        let completed = self.active.remove(lane_id)?;
        let next_id = self
            .pending
            .iter()
            .filter(|(_, job)| job.family == completed.family)
            .min_by(|(_, left), (_, right)| {
                left.enqueued_at_ms
                    .cmp(&right.enqueued_at_ms)
                    .then_with(|| left.lane_id.cmp(&right.lane_id))
            })
            .map(|(lane_id, _)| lane_id.clone());
        let next = next_id.and_then(|lane_id| self.pending.remove(&lane_id));
        if let Some(job) = next.clone() {
            self.active.insert(job.lane_id.clone(), job);
        }
        next
    }

    pub fn pending_count(&self) -> usize {
        self.pending.len()
    }
    pub fn delayed_count(&self) -> usize {
        self.delayed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::{
        automation::{CaptureFrameMetadata, CaptureSource, CapturedScreenFrame},
        types::{Rect, ScreenImage},
    };
    use anyhow::Result;
    use image::RgbaImage;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    struct CountingCapture(AtomicUsize);

    impl CaptureSource for CountingCapture {
        fn capture(&self, rect: Rect) -> Result<ScreenImage> {
            self.0.fetch_add(1, Ordering::Relaxed);
            Ok(ScreenImage::new(RgbaImage::new(rect.width, rect.height)))
        }

        fn capture_frame(&self, rect: Rect) -> Result<CapturedScreenFrame> {
            self.0.fetch_add(1, Ordering::Relaxed);
            Ok(CapturedScreenFrame {
                image: ScreenImage::new(RgbaImage::new(rect.width, rect.height)),
                metadata: CaptureFrameMetadata {
                    frame_id: 9,
                    captured_at_ms: 100,
                    window_id: 4,
                    window_revision: 2,
                    process_id: 4,
                    process_started_at_100ns: 6,
                    client_x: 0,
                    client_y: 0,
                    client_width: 100,
                    client_height: 100,
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
    }

    fn candidate(
        lane_id: &str,
        lane_order: usize,
        ready_at_ms: u64,
        frame_id: u64,
    ) -> CandidateEvent {
        CandidateEvent::for_test("run-1", 4, lane_id, lane_order, ready_at_ms, frame_id)
    }

    #[test]
    fn lowest_lane_order_ready_in_window_wins_and_losers_are_discarded() {
        let result = arbitrate_candidates(
            vec![
                candidate("lane-2", 1, 100, 7),
                candidate("lane-1", 0, 120, 7),
            ],
            None,
        );

        assert_eq!(
            result
                .winner
                .as_ref()
                .map(|candidate| candidate.lane_id.as_str()),
            Some("lane-1")
        );
        assert_eq!(result.discarded_lane_ids, vec!["lane-2"]);
        assert!(!result.safety_bypassed);
    }

    #[test]
    fn newer_false_can_revoke_qualified_candidate_before_arbitration() {
        let mut runner = WatchGroupRunner::new("run-1", 4, 1);
        assert_eq!(
            runner.observe_latch("lane", true, 1),
            LatchDecision::Qualified
        );
        runner
            .qualify_preobserved(candidate("lane", 0, 100, 1))
            .unwrap();
        assert!(runner.has_candidates());
        assert_eq!(
            runner.observe_latch("lane", false, 2),
            LatchDecision::Rearmed
        );

        assert!(runner.revoke_candidate("lane"));
        assert!(!runner.has_candidates());
    }

    #[test]
    fn candidate_outside_arbitration_window_cannot_delay_first_ready_lane() {
        let result = arbitrate_candidates(
            vec![
                candidate("fast", 1, 100, 7),
                candidate("slow-high-priority", 0, 126, 8),
            ],
            None,
        );

        assert_eq!(result.winner.unwrap().lane_id, "fast");
        assert_eq!(result.discarded_lane_ids, vec!["slow-high-priority"]);
    }

    #[test]
    fn safety_bypasses_arbitration_and_discards_every_candidate() {
        let result = arbitrate_candidates(
            vec![candidate("lane-1", 0, 100, 7)],
            Some(SafetyBypass::EmergencyStop),
        );

        assert!(result.winner.is_none());
        assert!(result.safety_bypassed);
        assert_eq!(result.discarded_lane_ids, vec!["lane-1"]);
    }

    #[test]
    fn losing_true_lane_latches_until_a_newer_false_frame_rearms_it() {
        let mut latch = LaneLatch::default();
        assert_eq!(latch.observe(true, 10), LatchDecision::Qualified);
        assert_eq!(latch.observe(true, 11), LatchDecision::Latched);
        assert_eq!(latch.observe(false, 10), LatchDecision::Stale);
        assert_eq!(latch.observe(false, 12), LatchDecision::Rearmed);
        assert_eq!(latch.observe(true, 13), LatchDecision::Qualified);
    }

    #[test]
    fn runner_rejects_stale_run_generation_and_does_not_queue_losing_actions() {
        let mut runner = WatchGroupRunner::new("run-1", 4, 4);
        assert!(runner.qualify(candidate("lane-1", 0, 100, 7)).is_ok());
        assert!(
            runner
                .qualify(CandidateEvent::for_test("old-run", 4, "lane-2", 1, 101, 7))
                .is_err()
        );
        assert!(
            runner
                .qualify(CandidateEvent::for_test("run-1", 3, "lane-2", 1, 101, 7))
                .is_err()
        );

        let result = runner.arbitrate(None);
        assert_eq!(result.winner.unwrap().lane_id, "lane-1");
        assert_eq!(runner.queued_action_count(), 0);
    }

    #[test]
    fn runner_rejects_same_or_older_frame_candidate_after_latching() {
        let mut runner = WatchGroupRunner::new("run-1", 4, 2);
        runner.qualify(candidate("lane-1", 0, 100, 10)).unwrap();
        runner.arbitrate(None);
        runner.invalidate(4);

        assert!(runner.qualify(candidate("lane-1", 0, 110, 9)).is_err());
        assert!(runner.qualify(candidate("lane-1", 0, 111, 10)).is_err());
    }

    #[test]
    fn newest_pending_frame_replaces_old_without_fifo_growth() {
        let mut scheduler = DetectorScheduler::new(1, 2, 8);
        assert_eq!(
            scheduler.submit(DetectorJob::for_test("lane", DetectorFamily::Text, 1, 0)),
            SubmitOutcome::Started
        );
        assert_eq!(
            scheduler.submit(DetectorJob::for_test("lane", DetectorFamily::Text, 2, 1)),
            SubmitOutcome::Pending
        );
        assert_eq!(
            scheduler.submit(DetectorJob::for_test("lane", DetectorFamily::Text, 3, 2)),
            SubmitOutcome::ReplacedPending {
                dropped_frame_id: 2
            }
        );
        assert_eq!(scheduler.pending_count(), 1);
        assert_eq!(scheduler.complete("lane").unwrap().frame_id, 3);
    }

    #[test]
    fn aging_schedules_starved_lane_without_using_action_priority() {
        let mut scheduler = DetectorScheduler::new(1, 2, 8);
        scheduler.submit(DetectorJob::for_test(
            "high-action-priority",
            DetectorFamily::Text,
            1,
            50,
        ));
        scheduler.submit(DetectorJob::for_test(
            "old-low-action-priority",
            DetectorFamily::Text,
            2,
            0,
        ));
        let next = scheduler.complete("high-action-priority").unwrap();

        assert_eq!(next.lane_id, "old-low-action-priority");
    }

    #[test]
    fn bounded_lane_slots_report_polling_delayed() {
        let mut scheduler = DetectorScheduler::new(1, 2, 1);
        assert_eq!(
            scheduler.submit(DetectorJob::for_test("lane-1", DetectorFamily::Text, 1, 0)),
            SubmitOutcome::Started
        );
        assert_eq!(
            scheduler.submit(DetectorJob::for_test("lane-2", DetectorFamily::Text, 2, 1)),
            SubmitOutcome::PollingDelayed
        );
        assert_eq!(scheduler.delayed_count(), 1);
        assert_eq!(scheduler.pending_count(), 0);
    }

    #[test]
    fn image_worker_pool_starts_at_most_two_serial_jobs() {
        let mut scheduler = DetectorScheduler::new(1, 2, 4);
        assert_eq!(
            scheduler.submit(DetectorJob::for_test(
                "image-1",
                DetectorFamily::Image,
                1,
                0
            )),
            SubmitOutcome::Started
        );
        assert_eq!(
            scheduler.submit(DetectorJob::for_test(
                "image-2",
                DetectorFamily::Image,
                1,
                0
            )),
            SubmitOutcome::Started
        );
        assert_eq!(
            scheduler.submit(DetectorJob::for_test(
                "image-3",
                DetectorFamily::Image,
                1,
                0
            )),
            SubmitOutcome::Pending
        );
    }

    #[test]
    fn capture_coordinator_shares_one_immutable_frame_for_overlapping_crops() {
        let source = Arc::new(CountingCapture(AtomicUsize::new(0)));
        let coordinator = CaptureCoordinator::new(source.clone());
        coordinator
            .begin_cycle(&[Rect::new(0, 0, 60, 60), Rect::new(40, 40, 60, 60)])
            .unwrap();

        let first = coordinator.capture_frame(Rect::new(0, 0, 60, 60)).unwrap();
        let second = coordinator
            .capture_frame(Rect::new(40, 40, 60, 60))
            .unwrap();

        assert_eq!(source.0.load(Ordering::Relaxed), 1);
        assert_eq!(first.metadata.frame_id, second.metadata.frame_id);
        assert_eq!(
            (first.image.rgba.width(), first.image.rgba.height()),
            (60, 60)
        );
        assert_eq!(
            (second.image.rgba.width(), second.image.rgba.height()),
            (60, 60)
        );
        coordinator.invalidate();
        coordinator.capture_frame(Rect::new(0, 0, 60, 60)).unwrap();
        assert_eq!(source.0.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn runner_records_full_one_shot_lifecycle_and_ordinary_matches_do_not_preempt_body() {
        let mut runner = WatchGroupRunner::new("run-1", 4, 2);
        runner.invalidate(4);
        runner.qualify(candidate("lane-1", 0, 100, 7)).unwrap();
        assert!(runner.arbitrate(None).winner.is_some());
        runner.begin_execution();
        assert!(runner.qualify(candidate("late", 1, 110, 8)).is_err());
        runner.settle_and_exit();

        assert_eq!(
            runner.lifecycle(),
            &[
                LaneState::Enter,
                LaneState::Observe,
                LaneState::Qualify,
                LaneState::Arbitrate,
                LaneState::Commit,
                LaneState::Execute,
                LaneState::Settle,
                LaneState::Exit
            ]
        );
    }

    #[test]
    fn generation_invalidation_discards_candidates_but_new_run_resets_latches() {
        let mut runner = WatchGroupRunner::new("run-1", 4, 2);
        runner.qualify(candidate("lane-1", 0, 100, 7)).unwrap();
        runner.invalidate(5);
        assert!(runner.arbitrate(None).winner.is_none());
        assert_eq!(
            runner.observe_latch("lane-1", true, 8),
            LatchDecision::Latched
        );

        runner.reset_for_run("run-2", 1);
        assert_eq!(
            runner.observe_latch("lane-1", true, 1),
            LatchDecision::Qualified
        );
    }

    #[test]
    fn candidate_binding_uses_the_complete_immutable_observation_token() {
        let token = ObservationToken {
            run_id: "run-1".to_string(),
            generation: 4,
            side_effect_epoch: 0,
            source_block_id: "lane-1".to_string(),
            detector: crate::engine::macro_engine::DetectorKind::Text,
            region_id: "region".to_string(),
            region_revision: 2,
            rule_id: "rule".to_string(),
            rule_revision: 3,
            frame_id: 7,
            captured_at_ms: 100,
            match_rect: None,
            score: Some(1.0),
            match_count: 1,
            stable_frames: 1,
            frame_metadata: Some(crate::engine::macro_engine::ImageFrameMetadata {
                frame_id: 7,
                captured_at_ms: 100,
                window_id: 9,
                window_revision: 10,
                process_id: 11,
                process_started_at_100ns: 12,
                client_x: 13,
                client_y: 14,
                client_width: 800,
                client_height: 600,
                geometry_revision: 15,
                display_id: 17,
                display_profile_revision: 16,
                dpi: 144,
                is_visible: true,
                is_minimized: false,
                is_foreground: true,
                region_revision: 2,
                rule_revision: 3,
            }),
            evidence: serde_json::json!({"word": "ready"}),
        };
        let candidate = CandidateEvent::from_observation("lane-1", 0, 105, &token);
        assert!(candidate.matches_observation(&token));
        let mut stale = token.clone();
        stale.frame_id = 8;
        assert!(!candidate.matches_observation(&stale));
        stale = token.clone();
        stale.rule_revision = 4;
        assert!(!candidate.matches_observation(&stale));
        stale = token.clone();
        stale.side_effect_epoch = 1;
        assert!(!candidate.matches_observation(&stale));
        stale = token.clone();
        stale.captured_at_ms = 101;
        assert!(!candidate.matches_observation(&stale));
        stale = token.clone();
        stale.score = Some(0.99);
        assert!(!candidate.matches_observation(&stale));
        stale = token.clone();
        stale
            .frame_metadata
            .as_mut()
            .unwrap()
            .process_started_at_100ns += 1;
        assert!(!candidate.matches_observation(&stale));
        stale = token.clone();
        stale.evidence = serde_json::json!({"word": "changed"});
        assert!(!candidate.matches_observation(&stale));
    }
}
