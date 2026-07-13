use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::Instant,
};

use anyhow::{Context, Result, bail};
use image::Pixel;
use serde::{Deserialize, Serialize};

use crate::engine::{
    platform::{OcrFrame, OcrPixelFormat, PositionedOcrWord},
    types::{Rect, ScreenImage},
};

use super::{
    Condition, ConditionDetector, DetectorEvidence, Limit, MatchSelectionPolicy,
    ObservationRequest, PreprocessProfile, TextMatchMode, TextRule,
};

const DEFAULT_MAX_TEXT_STABILITY_STATES: usize = 256;
use crate::engine::{automation::CaptureSource, platform::WindowsTextRecognizer};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OcrWord {
    pub text: String,
    pub rect: Rect,
    pub line_index: u32,
    pub word_index: u32,
}

impl OcrWord {
    pub fn new(text: impl Into<String>, rect: Rect, line_index: u32, word_index: u32) -> Self {
        Self {
            text: text.into(),
            rect,
            line_index,
            word_index,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TextMatch {
    pub matched: bool,
    pub rect: Option<Rect>,
    pub score: Option<f64>,
    pub match_count: u32,
    pub source_word_indices: Vec<usize>,
}

impl TextRule {
    pub fn contains(expected: impl Into<String>) -> Self {
        test_rule(expected, TextMatchMode::Contains)
    }

    pub fn absent(expected: impl Into<String>) -> Self {
        test_rule(expected, TextMatchMode::Absent)
    }
}

fn test_rule(expected: impl Into<String>, match_mode: TextMatchMode) -> TextRule {
    TextRule {
        id: "text-rule".to_string(),
        revision: 1,
        region_id: "region".to_string(),
        language: "en-US".to_string(),
        preprocess: PreprocessProfile::Original,
        expected: expected.into(),
        match_mode,
        threshold: 1.0,
        case_sensitive: false,
        allow_cross_line: false,
        match_policy: MatchSelectionPolicy::FirstReadingOrder,
        poll_interval_ms: 100,
        timeout_ms: Limit::Finite(1_000),
        stable_frames: 1,
    }
}

pub fn match_text(words: &[OcrWord], rule: &TextRule) -> Result<TextMatch> {
    let expected = normalized_chars(&rule.expected, rule.case_sensitive);
    if expected.is_empty() {
        bail!("expected text must not normalize to empty");
    }
    if !(0.0..=1.0).contains(&rule.threshold) {
        bail!("text match threshold must be between 0 and 1");
    }

    let mode = if rule.match_mode == TextMatchMode::Absent {
        TextMatchMode::Contains
    } else {
        rule.match_mode
    };
    let mut candidates = candidate_matches(words, rule, mode, &expected)?;
    deduplicate_candidates_by_rect(&mut candidates);

    if rule.match_mode == TextMatchMode::Absent {
        return Ok(TextMatch {
            matched: candidates.is_empty(),
            rect: None,
            score: None,
            match_count: u32::try_from(candidates.len()).unwrap_or(u32::MAX),
            source_word_indices: Vec::new(),
        });
    }

    let match_count = u32::try_from(candidates.len()).unwrap_or(u32::MAX);
    let selected = select_candidate(&candidates, rule.match_policy);
    let qualifies = selected.is_some()
        && (rule.match_policy != MatchSelectionPolicy::ExactlyOne || candidates.len() == 1);
    let selected = qualifies.then_some(selected).flatten();
    Ok(match selected {
        Some(candidate) => TextMatch {
            matched: true,
            rect: Some(candidate.rect),
            score: Some(candidate.score),
            match_count,
            source_word_indices: candidate.source_word_indices.clone(),
        },
        None => TextMatch {
            matched: false,
            rect: None,
            score: None,
            match_count,
            source_word_indices: Vec::new(),
        },
    })
}

#[derive(Debug, Clone)]
struct Candidate {
    rect: Rect,
    score: f64,
    reading_order: (u32, u32, usize),
    source_word_indices: Vec<usize>,
}

#[derive(Debug, Default)]
struct NormalizedSequence {
    chars: Vec<char>,
    sources: Vec<Option<usize>>,
    word_indices: Vec<usize>,
}

fn candidate_matches(
    words: &[OcrWord],
    rule: &TextRule,
    mode: TextMatchMode,
    expected: &[char],
) -> Result<Vec<Candidate>> {
    let sequences = normalized_sequences(words, rule.case_sensitive, rule.allow_cross_line);
    let mut candidates = Vec::new();
    for sequence in sequences {
        match mode {
            TextMatchMode::Exact => {
                if sequence.chars == expected {
                    push_candidate(
                        words,
                        &sequence,
                        0,
                        sequence.chars.len(),
                        1.0,
                        &mut candidates,
                    )?;
                }
            }
            TextMatchMode::Contains => {
                if expected.len() <= sequence.chars.len() {
                    for start in 0..=sequence.chars.len() - expected.len() {
                        if sequence.chars[start..start + expected.len()] == *expected {
                            push_candidate(
                                words,
                                &sequence,
                                start,
                                start + expected.len(),
                                1.0,
                                &mut candidates,
                            )?;
                        }
                    }
                }
            }
            TextMatchMode::Fuzzy => {
                let expected_words = expected
                    .iter()
                    .filter(|character| **character == ' ')
                    .count()
                    + 1;
                if expected_words <= sequence.word_indices.len() {
                    for start_word in 0..=sequence.word_indices.len() - expected_words {
                        let selected_words =
                            &sequence.word_indices[start_word..start_word + expected_words];
                        let span = normalized_word_span(words, selected_words, rule.case_sensitive);
                        let score = strsim::jaro_winkler(
                            &span.iter().collect::<String>(),
                            &expected.iter().collect::<String>(),
                        );
                        if score >= rule.threshold {
                            let rect = text_match_rect(
                                selected_words.iter().map(|index| words[*index].rect),
                            )?
                            .expect("a fuzzy candidate has at least one word");
                            let first = selected_words[0];
                            candidates.push(Candidate {
                                rect,
                                score,
                                reading_order: (
                                    words[first].line_index,
                                    words[first].word_index,
                                    first,
                                ),
                                source_word_indices: selected_words.to_vec(),
                            });
                        }
                    }
                }
            }
            TextMatchMode::Absent => unreachable!("absent matching uses contains candidates"),
        }
    }
    Ok(candidates)
}

fn normalized_sequences(
    words: &[OcrWord],
    case_sensitive: bool,
    allow_cross_line: bool,
) -> Vec<NormalizedSequence> {
    let mut ordered: Vec<usize> = (0..words.len()).collect();
    ordered.sort_by_key(|index| (words[*index].line_index, words[*index].word_index, *index));
    let mut sequences = Vec::<NormalizedSequence>::new();
    for index in ordered {
        let normalized = normalized_chars(&words[index].text, case_sensitive);
        if normalized.is_empty() {
            continue;
        }
        let begins_line = sequences.last().is_none_or(|sequence| {
            sequence
                .word_indices
                .last()
                .is_some_and(|previous| words[*previous].line_index != words[index].line_index)
        });
        if sequences.is_empty() || (!allow_cross_line && begins_line) {
            sequences.push(NormalizedSequence::default());
        }
        let sequence = sequences.last_mut().expect("a sequence was inserted");
        if !sequence.chars.is_empty() {
            sequence.chars.push(' ');
            sequence.sources.push(None);
        }
        for character in normalized {
            sequence.chars.push(character);
            sequence.sources.push(Some(index));
        }
        sequence.word_indices.push(index);
    }
    sequences
}

fn normalized_word_span(words: &[OcrWord], indices: &[usize], case_sensitive: bool) -> Vec<char> {
    let mut normalized = Vec::new();
    for (position, index) in indices.iter().enumerate() {
        if position > 0 {
            normalized.push(' ');
        }
        normalized.extend(normalized_chars(&words[*index].text, case_sensitive));
    }
    normalized
}

fn push_candidate(
    words: &[OcrWord],
    sequence: &NormalizedSequence,
    start: usize,
    end: usize,
    score: f64,
    candidates: &mut Vec<Candidate>,
) -> Result<()> {
    let mut indices = Vec::new();
    for index in sequence.sources[start..end].iter().flatten().copied() {
        if indices.last() != Some(&index) {
            indices.push(index);
        }
    }
    let Some(first) = indices.first().copied() else {
        return Ok(());
    };
    let rect = text_match_rect(indices.iter().map(|index| words[*index].rect))?
        .expect("a text candidate has at least one source word");
    candidates.push(Candidate {
        rect,
        score,
        reading_order: (words[first].line_index, words[first].word_index, first),
        source_word_indices: indices,
    });
    Ok(())
}

fn deduplicate_candidates_by_rect(candidates: &mut Vec<Candidate>) {
    let mut unique = Vec::<Candidate>::new();
    for candidate in candidates.drain(..) {
        if let Some(existing) = unique.iter_mut().find(|item| item.rect == candidate.rect) {
            if candidate.score > existing.score {
                *existing = candidate;
            }
        } else {
            unique.push(candidate);
        }
    }
    *candidates = unique;
}

fn select_candidate(candidates: &[Candidate], policy: MatchSelectionPolicy) -> Option<&Candidate> {
    match policy {
        MatchSelectionPolicy::ExactlyOne | MatchSelectionPolicy::FirstReadingOrder => candidates
            .iter()
            .min_by_key(|candidate| candidate.reading_order),
        MatchSelectionPolicy::HighestScore => candidates.iter().max_by(|left, right| {
            left.score
                .total_cmp(&right.score)
                .then_with(|| right.reading_order.cmp(&left.reading_order))
        }),
        MatchSelectionPolicy::Topmost => candidates
            .iter()
            .min_by_key(|candidate| (candidate.rect.y, candidate.rect.x, candidate.reading_order)),
        MatchSelectionPolicy::Bottommost => candidates.iter().max_by_key(|candidate| {
            (
                i64::from(candidate.rect.y) + i64::from(candidate.rect.height),
                -i64::from(candidate.rect.x),
                std::cmp::Reverse(candidate.reading_order),
            )
        }),
    }
}

pub fn text_match_rect(rects: impl IntoIterator<Item = Rect>) -> Result<Option<Rect>> {
    let mut rects = rects.into_iter();
    let Some(first) = rects.next() else {
        return Ok(None);
    };
    let mut left = i64::from(first.x);
    let mut top = i64::from(first.y);
    let mut right = left + i64::from(first.width);
    let mut bottom = top + i64::from(first.height);
    for rect in rects {
        let rect_left = i64::from(rect.x);
        let rect_top = i64::from(rect.y);
        left = left.min(rect_left);
        top = top.min(rect_top);
        right = right.max(rect_left + i64::from(rect.width));
        bottom = bottom.max(rect_top + i64::from(rect.height));
    }
    let x = i32::try_from(left)?;
    let y = i32::try_from(top)?;
    let width = u32::try_from(right - left)?;
    let height = u32::try_from(bottom - top)?;
    Ok(Some(Rect::new(x, y, width, height)))
}

fn normalized_chars(text: &str, case_sensitive: bool) -> Vec<char> {
    let text = if case_sensitive {
        text.to_string()
    } else {
        text.to_lowercase()
    };
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .collect()
}

#[derive(Debug, Clone, Copy)]
struct PreparedOcrFrame<'a> {
    frame: &'a OcrFrame,
    coordinate_scale: u32,
}

#[derive(Debug)]
struct TextPreprocessWorker {
    frame: OcrFrame,
    gray_scratch: Vec<u8>,
    #[cfg(test)]
    frame_growths: usize,
    #[cfg(test)]
    scratch_growths: usize,
    #[cfg(test)]
    frame_resize_growths: usize,
    #[cfg(test)]
    scratch_resize_growths: usize,
}

impl Default for TextPreprocessWorker {
    fn default() -> Self {
        Self {
            frame: OcrFrame {
                pixels: Vec::new(),
                width: 0,
                height: 0,
                pixel_format: OcrPixelFormat::Gray8,
            },
            gray_scratch: Vec::new(),
            #[cfg(test)]
            frame_growths: 0,
            #[cfg(test)]
            scratch_growths: 0,
            #[cfg(test)]
            frame_resize_growths: 0,
            #[cfg(test)]
            scratch_resize_growths: 0,
        }
    }
}

impl TextPreprocessWorker {
    fn prepare(
        &mut self,
        image: &ScreenImage,
        profile: PreprocessProfile,
    ) -> Result<PreparedOcrFrame<'_>> {
        let width = image.rgba.width();
        let height = image.rgba.height();
        let pixel_count = checked_buffer_len(width, height, 1)?;
        let coordinate_scale = match profile {
            PreprocessProfile::Original => {
                self.ensure_frame_len(checked_buffer_len(width, height, 4)?);
                for (destination, pixel) in self
                    .frame
                    .pixels
                    .chunks_exact_mut(4)
                    .zip(image.rgba.pixels())
                {
                    destination.copy_from_slice(&[pixel[2], pixel[1], pixel[0], pixel[3]]);
                }
                self.frame.width = width;
                self.frame.height = height;
                self.frame.pixel_format = OcrPixelFormat::Bgra8;
                1
            }
            PreprocessProfile::Grayscale | PreprocessProfile::HighContrast => {
                self.ensure_frame_len(pixel_count);
                write_luminance(&mut self.frame.pixels, image);
                if profile == PreprocessProfile::HighContrast {
                    let threshold = otsu_threshold(&self.frame.pixels);
                    for pixel in &mut self.frame.pixels {
                        *pixel = if *pixel > threshold { 255 } else { 0 };
                    }
                }
                self.frame.width = width;
                self.frame.height = height;
                self.frame.pixel_format = OcrPixelFormat::Gray8;
                1
            }
            PreprocessProfile::SmallText => {
                self.ensure_scratch_len(pixel_count);
                write_luminance(&mut self.gray_scratch, image);
                let enlarged_width = width
                    .checked_mul(2)
                    .context("Small Text OCR width exceeds u32")?;
                let enlarged_height = height
                    .checked_mul(2)
                    .context("Small Text OCR height exceeds u32")?;
                self.ensure_frame_len(checked_buffer_len(enlarged_width, enlarged_height, 1)?);
                let source_width = width as usize;
                let destination_width = enlarged_width as usize;
                for y in 0..enlarged_height as usize {
                    for x in 0..destination_width {
                        self.frame.pixels[y * destination_width + x] =
                            self.gray_scratch[(y / 2) * source_width + x / 2];
                    }
                }
                self.frame.width = enlarged_width;
                self.frame.height = enlarged_height;
                self.frame.pixel_format = OcrPixelFormat::Gray8;
                2
            }
        };
        Ok(PreparedOcrFrame {
            frame: &self.frame,
            coordinate_scale,
        })
    }

    fn ensure_frame_len(&mut self, len: usize) {
        if self.frame.pixels.capacity() < len {
            #[cfg(test)]
            let capacity_before = self.frame.pixels.capacity();
            self.frame
                .pixels
                .reserve_exact(len - self.frame.pixels.len());
            #[cfg(test)]
            if self.frame.pixels.capacity() > capacity_before {
                self.frame_growths += 1;
            }
        }
        #[cfg(test)]
        let capacity_before_resize = self.frame.pixels.capacity();
        self.frame.pixels.resize(len, 0);
        #[cfg(test)]
        if self.frame.pixels.capacity() > capacity_before_resize {
            self.frame_resize_growths += 1;
        }
    }

    fn ensure_scratch_len(&mut self, len: usize) {
        if self.gray_scratch.capacity() < len {
            #[cfg(test)]
            let capacity_before = self.gray_scratch.capacity();
            self.gray_scratch
                .reserve_exact(len - self.gray_scratch.len());
            #[cfg(test)]
            if self.gray_scratch.capacity() > capacity_before {
                self.scratch_growths += 1;
            }
        }
        #[cfg(test)]
        let capacity_before_resize = self.gray_scratch.capacity();
        self.gray_scratch.resize(len, 0);
        #[cfg(test)]
        if self.gray_scratch.capacity() > capacity_before_resize {
            self.scratch_resize_growths += 1;
        }
    }

    #[cfg(test)]
    fn buffer_stats(&self) -> PreprocessBufferStats {
        PreprocessBufferStats {
            frame_ptr: self.frame.pixels.as_ptr() as usize,
            frame_len: self.frame.pixels.len(),
            frame_capacity: self.frame.pixels.capacity(),
            frame_growths: self.frame_growths,
            scratch_ptr: self.gray_scratch.as_ptr() as usize,
            scratch_capacity: self.gray_scratch.capacity(),
            scratch_growths: self.scratch_growths,
            frame_resize_growths: self.frame_resize_growths,
            scratch_resize_growths: self.scratch_resize_growths,
        }
    }
}

#[cfg(test)]
#[derive(Debug, Clone, Copy)]
struct PreprocessBufferStats {
    frame_ptr: usize,
    frame_len: usize,
    frame_capacity: usize,
    frame_growths: usize,
    scratch_ptr: usize,
    scratch_capacity: usize,
    scratch_growths: usize,
    frame_resize_growths: usize,
    scratch_resize_growths: usize,
}

fn checked_buffer_len(width: u32, height: u32, bytes_per_pixel: usize) -> Result<usize> {
    (width as usize)
        .checked_mul(height as usize)
        .and_then(|pixels| pixels.checked_mul(bytes_per_pixel))
        .context("OCR preprocessing buffer length overflows usize")
}

fn write_luminance(destination: &mut [u8], image: &ScreenImage) {
    for (destination, pixel) in destination.iter_mut().zip(image.rgba.pixels()) {
        *destination = pixel.to_luma()[0];
    }
}

fn otsu_threshold(pixels: &[u8]) -> u8 {
    let mut histogram = [0u64; 256];
    for pixel in pixels {
        histogram[*pixel as usize] += 1;
    }
    let total = pixels.len() as u64;
    if total == 0 {
        return 0;
    }
    let weighted_total: u64 = histogram
        .iter()
        .enumerate()
        .map(|(value, count)| value as u64 * count)
        .sum();
    let mut background_count = 0u64;
    let mut background_weight = 0u64;
    let mut best_variance = -1.0f64;
    let mut threshold = 0u8;
    for (value, count) in histogram.iter().enumerate() {
        background_count += count;
        if background_count == 0 {
            continue;
        }
        let foreground_count = total - background_count;
        if foreground_count == 0 {
            break;
        }
        background_weight += value as u64 * count;
        let background_mean = background_weight as f64 / background_count as f64;
        let foreground_mean = (weighted_total - background_weight) as f64 / foreground_count as f64;
        let difference = background_mean - foreground_mean;
        let variance = background_count as f64 * foreground_count as f64 * difference * difference;
        if variance > best_variance {
            best_variance = variance;
            threshold = value as u8;
        }
    }
    threshold
}

fn map_processed_rect_to_capture(
    rect: Rect,
    coordinate_scale: u32,
    capture_rect: Rect,
) -> Result<Rect> {
    if coordinate_scale == 0 {
        bail!("OCR coordinate scale must be non-zero");
    }
    let scale = i64::from(coordinate_scale);
    let left = i64::from(rect.x).div_euclid(scale);
    let top = i64::from(rect.y).div_euclid(scale);
    let right_scaled = i64::from(rect.x) + i64::from(rect.width);
    let bottom_scaled = i64::from(rect.y) + i64::from(rect.height);
    let right = right_scaled.div_euclid(scale) + i64::from(right_scaled.rem_euclid(scale) != 0);
    let bottom = bottom_scaled.div_euclid(scale) + i64::from(bottom_scaled.rem_euclid(scale) != 0);
    let capture_left = i64::from(capture_rect.x);
    let capture_top = i64::from(capture_rect.y);
    let capture_right = capture_left + i64::from(capture_rect.width);
    let capture_bottom = capture_top + i64::from(capture_rect.height);
    let intersect_left = (capture_left + left).max(capture_left);
    let intersect_top = (capture_top + top).max(capture_top);
    let intersect_right = (capture_left + right).min(capture_right);
    let intersect_bottom = (capture_top + bottom).min(capture_bottom);
    if intersect_right <= intersect_left || intersect_bottom <= intersect_top {
        bail!("OCR word bounds do not intersect the captured region");
    }
    Ok(Rect::new(
        i32::try_from(intersect_left)?,
        i32::try_from(intersect_top)?,
        u32::try_from(intersect_right - intersect_left)?,
        u32::try_from(intersect_bottom - intersect_top)?,
    ))
}

pub trait PositionedTextRecognizer: Send + Sync {
    fn recognize_words(&self, frame: &OcrFrame, language_tag: &str) -> Result<Vec<OcrWord>>;
}

impl PositionedTextRecognizer for WindowsTextRecognizer {
    fn recognize_words(&self, frame: &OcrFrame, language_tag: &str) -> Result<Vec<OcrWord>> {
        WindowsTextRecognizer::recognize_words(self, frame, language_tag).map(|words| {
            words
                .into_iter()
                .map(|word: PositionedOcrWord| OcrWord {
                    text: word.text,
                    rect: word.rect,
                    line_index: word.line_index,
                    word_index: word.word_index,
                })
                .collect()
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct StabilityKey {
    run_id: String,
    generation: u64,
    side_effect_epoch: u64,
    source_block_id: String,
    rule_id: String,
    rule_revision: u64,
    region_id: String,
    region_revision: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct StabilityState {
    rect: Option<Rect>,
    source_word_indices: Vec<usize>,
    count: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct StabilityWatermark {
    generation: u64,
    side_effect_epoch: u64,
}

#[derive(Debug, Default)]
struct TextDetectorState {
    stability: HashMap<StabilityKey, StabilityState>,
    watermark_by_run: HashMap<String, StabilityWatermark>,
}

pub struct TextDetector {
    recognizer: Arc<dyn PositionedTextRecognizer>,
    preprocess: Mutex<TextPreprocessWorker>,
    state: Mutex<TextDetectorState>,
    maximum_stability_states: usize,
}

impl Default for TextDetector {
    fn default() -> Self {
        Self::with_recognizer(Arc::new(WindowsTextRecognizer::default()))
    }
}

impl TextDetector {
    pub fn with_recognizer(recognizer: Arc<dyn PositionedTextRecognizer>) -> Self {
        Self {
            recognizer,
            preprocess: Mutex::new(TextPreprocessWorker::default()),
            state: Mutex::new(TextDetectorState::default()),
            maximum_stability_states: DEFAULT_MAX_TEXT_STABILITY_STATES,
        }
    }

    fn begin_observation(
        &self,
        run_id: &str,
        generation: u64,
        side_effect_epoch: u64,
    ) -> Result<bool> {
        let requested = StabilityWatermark {
            generation,
            side_effect_epoch,
        };
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("text detector stability lock is poisoned"))?;
        match state.watermark_by_run.get(run_id).copied() {
            Some(current) if current == requested => Ok(true),
            Some(current) if watermark_is_newer(requested, current) => {
                state.stability.retain(|key, _| key.run_id != run_id);
                state.watermark_by_run.insert(run_id.to_string(), requested);
                Ok(true)
            }
            Some(_) => Ok(false),
            None => {
                if state.watermark_by_run.len() >= self.maximum_stability_states {
                    bail!(
                        "text detector run watermark capacity {} is exhausted",
                        self.maximum_stability_states
                    );
                }
                state.watermark_by_run.insert(run_id.to_string(), requested);
                Ok(true)
            }
        }
    }

    fn observe_text(
        &self,
        request: &ObservationRequest<'_>,
        capture: &(dyn CaptureSource + Send + Sync),
        source_block_id: &str,
        rule_id: &str,
    ) -> Result<DetectorEvidence> {
        if !self.begin_observation(
            request.run_id,
            request.generation,
            request.side_effect_epoch,
        )? {
            return Ok(DetectorEvidence::unmatched(
                request.observed_at_ms,
                request.observed_at_ms,
            ));
        }
        let definition = request.compiled.definition();
        let rule = definition
            .text_rules
            .iter()
            .find(|rule| rule.id == rule_id)
            .with_context(|| format!("compiled text rule {rule_id:?} is missing"))?;
        let region = definition
            .regions
            .iter()
            .find(|region| region.id == rule.region_id)
            .with_context(|| format!("compiled text region {:?} is missing", rule.region_id))?;
        let client = Rect::new(
            0,
            0,
            definition.target.captured_client_width,
            definition.target.captured_client_height,
        );
        let capture_rect = client.rect_from_ratio(region.rect);
        let captured = capture.capture_frame(capture_rect)?;
        let frame = super::ImageFrameMetadata {
            frame_id: captured.metadata.frame_id,
            captured_at_ms: captured.metadata.captured_at_ms,
            window_id: captured.metadata.window_id,
            window_revision: captured.metadata.window_revision,
            process_id: captured.metadata.process_id,
            process_started_at_100ns: captured.metadata.process_started_at_100ns,
            client_x: captured.metadata.client_x,
            client_y: captured.metadata.client_y,
            client_width: captured.metadata.client_width,
            client_height: captured.metadata.client_height,
            geometry_revision: captured.metadata.geometry_revision,
            display_id: captured.metadata.display_id,
            display_profile_revision: captured.metadata.display_profile_revision,
            dpi: captured.metadata.dpi,
            is_visible: captured.metadata.is_visible,
            is_minimized: captured.metadata.is_minimized,
            is_foreground: captured.metadata.is_foreground,
            region_revision: region.revision,
            rule_revision: rule.revision,
        };
        if (frame.client_width, frame.client_height)
            != (
                definition.target.captured_client_width,
                definition.target.captured_client_height,
            )
        {
            bail!(
                "text rule client geometry {}x{} is stale for current client geometry {}x{}",
                definition.target.captured_client_width,
                definition.target.captured_client_height,
                frame.client_width,
                frame.client_height
            );
        }
        if frame.dpi != definition.target.captured_dpi {
            bail!(
                "text rule DPI {} is stale for current DPI {}",
                definition.target.captured_dpi,
                frame.dpi
            );
        }
        let mut preprocess = self
            .preprocess
            .lock()
            .map_err(|_| anyhow::anyhow!("text detector preprocessing lock is poisoned"))?;
        let prepared = preprocess.prepare(&captured.image, rule.preprocess)?;
        let relative_words = self
            .recognizer
            .recognize_words(prepared.frame, &rule.language)?;
        let words = relative_words
            .into_iter()
            .map(|word| {
                Ok(OcrWord {
                    rect: map_processed_rect_to_capture(
                        word.rect,
                        prepared.coordinate_scale,
                        capture_rect,
                    )?,
                    ..word
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let text_match = match_text(&words, rule)?;
        let key = StabilityKey {
            run_id: request.run_id.to_string(),
            generation: request.generation,
            side_effect_epoch: request.side_effect_epoch,
            source_block_id: source_block_id.to_string(),
            rule_id: rule.id.clone(),
            rule_revision: rule.revision,
            region_id: region.id.clone(),
            region_revision: region.revision,
        };
        let stable_frames = self.update_stability(&key, &text_match)?;
        let required_frames = rule.stable_frames.max(1);
        let qualified = text_match.matched && stable_frames >= required_frames;
        let details = serde_json::json!({
            "source_block_id": source_block_id,
            "rule_id": rule.id,
            "rule_revision": rule.revision,
            "region_id": region.id,
            "region_revision": region.revision,
            "preprocess": rule.preprocess,
            "raw_match": text_match,
            "words": words,
        });
        Ok(DetectorEvidence::captured_match(
            qualified,
            frame,
            qualified.then_some(text_match.rect).flatten(),
            text_match.score,
            text_match.match_count,
            stable_frames,
            details,
        ))
    }

    fn update_stability(&self, key: &StabilityKey, text_match: &TextMatch) -> Result<u8> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("text detector stability lock is poisoned"))?;
        if state.watermark_by_run.get(&key.run_id).copied()
            != Some(StabilityWatermark {
                generation: key.generation,
                side_effect_epoch: key.side_effect_epoch,
            })
        {
            return Ok(0);
        }
        if !text_match.matched {
            state.stability.remove(key);
            return Ok(0);
        }
        if !state.stability.contains_key(key)
            && state.stability.len() >= self.maximum_stability_states
        {
            bail!(
                "text stability state capacity {} is exhausted; clear completed runs",
                self.maximum_stability_states
            );
        }
        let stability = state
            .stability
            .entry(key.clone())
            .or_insert_with(|| StabilityState {
                rect: text_match.rect,
                source_word_indices: text_match.source_word_indices.clone(),
                count: 0,
            });
        if stability.rect == text_match.rect
            && stability.source_word_indices == text_match.source_word_indices
        {
            stability.count = stability.count.saturating_add(1);
        } else {
            stability.rect = text_match.rect;
            stability.source_word_indices = text_match.source_word_indices.clone();
            stability.count = 1;
        }
        Ok(stability.count)
    }
}

impl ConditionDetector for TextDetector {
    fn observe(
        &self,
        request: &ObservationRequest<'_>,
        capture: &(dyn CaptureSource + Send + Sync),
    ) -> Result<DetectorEvidence> {
        match request.condition {
            Condition::Text {
                source_block_id,
                rule_id,
                ..
            } => self.observe_text(request, capture, source_block_id, rule_id),
            Condition::Image { .. } => bail!("text detector cannot observe an image condition"),
        }
    }

    fn run_finished(&self, run_id: &str, generations: &[u64]) {
        if let Ok(mut state) = self.state.lock() {
            state
                .stability
                .retain(|key, _| key.run_id != run_id || !generations.contains(&key.generation));
            if state
                .watermark_by_run
                .get(run_id)
                .is_some_and(|watermark| generations.contains(&watermark.generation))
            {
                state.watermark_by_run.remove(run_id);
            }
        }
    }

    fn side_effect_boundary(&self, run_id: &str, generation: u64, next_epoch: u64) {
        let _ = self.begin_observation(run_id, generation, next_epoch);
    }
}

fn watermark_is_newer(candidate: StabilityWatermark, current: StabilityWatermark) -> bool {
    if candidate.generation != current.generation {
        serial_is_newer(candidate.generation, current.generation)
    } else {
        serial_is_newer(candidate.side_effect_epoch, current.side_effect_epoch)
    }
}

fn serial_is_newer(candidate: u64, current: u64) -> bool {
    let distance = candidate.wrapping_sub(current);
    distance != 0 && distance < (1_u64 << 63)
}

#[derive(Debug, Clone)]
pub struct SavedTextProfileSample {
    pub name: String,
    pub image: ScreenImage,
    pub rule: TextRule,
    pub expected_match: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TextProfileBenchmark {
    pub profile: PreprocessProfile,
    pub correct: usize,
    pub total: usize,
    pub median_elapsed_nanos: u128,
}

impl TextProfileBenchmark {
    pub fn accuracy(&self) -> f64 {
        if self.total == 0 {
            0.0
        } else {
            self.correct as f64 / self.total as f64
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TextProfileBenchmarkResult {
    pub profiles: Vec<TextProfileBenchmark>,
    pub recommended: Option<PreprocessProfile>,
}

pub fn benchmark_text_profiles(
    recognizer: &dyn PositionedTextRecognizer,
    samples: &[SavedTextProfileSample],
    accuracy_gate: f64,
) -> Result<TextProfileBenchmarkResult> {
    let clock = SystemTextBenchmarkClock::default();
    benchmark_text_profiles_with_clock(recognizer, samples, accuracy_gate, &clock, 5)
}

trait TextBenchmarkClock {
    fn now_nanos(&self) -> u128;
}

struct SystemTextBenchmarkClock {
    started: Instant,
}

impl Default for SystemTextBenchmarkClock {
    fn default() -> Self {
        Self {
            started: Instant::now(),
        }
    }
}

impl TextBenchmarkClock for SystemTextBenchmarkClock {
    fn now_nanos(&self) -> u128 {
        self.started.elapsed().as_nanos()
    }
}

const BENCHMARK_PROFILES: [PreprocessProfile; 4] = [
    PreprocessProfile::Original,
    PreprocessProfile::Grayscale,
    PreprocessProfile::HighContrast,
    PreprocessProfile::SmallText,
];

fn benchmark_text_profiles_with_clock(
    recognizer: &dyn PositionedTextRecognizer,
    samples: &[SavedTextProfileSample],
    accuracy_gate: f64,
    clock: &dyn TextBenchmarkClock,
    rounds: usize,
) -> Result<TextProfileBenchmarkResult> {
    if samples.is_empty() {
        bail!("text profile benchmark requires at least one saved sample");
    }
    if !(0.0..=1.0).contains(&accuracy_gate) {
        bail!("text profile benchmark accuracy gate must be between 0 and 1");
    }
    if rounds == 0 {
        bail!("text profile benchmark requires at least one measurement round");
    }

    let mut preprocess = TextPreprocessWorker::default();
    let warm = preprocess.prepare(&samples[0].image, PreprocessProfile::Original)?;
    recognizer
        .recognize_words(warm.frame, &samples[0].rule.language)
        .with_context(|| {
            format!(
                "OCR warm-up failed for saved text sample {:?}",
                samples[0].name
            )
        })?;

    let mut correct = [0usize; 4];
    let mut elapsed = std::array::from_fn::<Vec<u128>, 4, _>(|_| Vec::with_capacity(rounds));
    for round in 0..rounds {
        for offset in 0..BENCHMARK_PROFILES.len() {
            let profile_index = (round + offset) % BENCHMARK_PROFILES.len();
            let profile = BENCHMARK_PROFILES[profile_index];
            let started = clock.now_nanos();
            for sample in samples {
                let matched =
                    evaluate_profile_sample(recognizer, &mut preprocess, sample, profile)?;
                if matched == sample.expected_match {
                    correct[profile_index] += 1;
                }
            }
            elapsed[profile_index].push(clock.now_nanos().saturating_sub(started));
        }
    }

    let profiles = BENCHMARK_PROFILES
        .iter()
        .copied()
        .enumerate()
        .map(|(index, profile)| {
            elapsed[index].sort_unstable();
            TextProfileBenchmark {
                profile,
                correct: correct[index],
                total: samples.len() * rounds,
                median_elapsed_nanos: elapsed[index][elapsed[index].len() / 2],
            }
        })
        .collect::<Vec<_>>();
    let recommended = profiles
        .iter()
        .enumerate()
        .filter(|(_, profile)| profile.accuracy() >= accuracy_gate)
        .min_by_key(|(profile_index, profile)| (profile.median_elapsed_nanos, *profile_index))
        .map(|(_, profile)| profile.profile);
    Ok(TextProfileBenchmarkResult {
        profiles,
        recommended,
    })
}

fn evaluate_profile_sample(
    recognizer: &dyn PositionedTextRecognizer,
    preprocess: &mut TextPreprocessWorker,
    sample: &SavedTextProfileSample,
    profile: PreprocessProfile,
) -> Result<bool> {
    let prepared = preprocess.prepare(&sample.image, profile)?;
    let words = recognizer
        .recognize_words(prepared.frame, &sample.rule.language)
        .with_context(|| format!("OCR failed for saved text sample {:?}", sample.name))?;
    let words = words
        .into_iter()
        .map(|word| {
            Ok(OcrWord {
                rect: map_processed_rect_to_capture(
                    word.rect,
                    prepared.coordinate_scale,
                    Rect::new(0, 0, sample.image.rgba.width(), sample.image.rgba.height()),
                )?,
                ..word
            })
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(match_text(&words, &sample.rule)?.matched)
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{Arc, Condvar, Mutex, mpsc},
        thread,
    };

    use anyhow::Result;
    use image::{Rgba, RgbaImage};
    use sha2::{Digest, Sha256};

    use super::*;
    use crate::engine::automation::{CaptureFrameMetadata, CaptureSource, CapturedScreenFrame};
    use crate::engine::macro_engine::{
        Block, BlockKind, CompiledMacro, Condition, FocusLossPolicy, MACRO_SCHEMA_VERSION,
        MacroDefinition, ObservationRequest, ObserveMode, RegionDefinition, SafetyPolicy,
        SavedRevision, TargetProfile,
    };
    use crate::engine::platform::OcrPixelFormat;

    fn rule(expected: &str, mode: TextMatchMode) -> TextRule {
        let mut rule = TextRule::contains(expected);
        rule.match_mode = mode;
        rule
    }

    #[test]
    fn normalized_phrase_maps_to_union_of_word_boxes() {
        let words = vec![
            OcrWord::new("Max", Rect::new(10, 5, 30, 12), 0, 0),
            OcrWord::new("Health", Rect::new(44, 5, 48, 12), 0, 1),
        ];

        let matched = match_text(&words, &TextRule::contains("max health")).unwrap();

        assert_eq!(matched.rect, Some(Rect::new(10, 5, 82, 12)));
    }

    #[test]
    fn absent_condition_has_no_click_geometry() {
        let matched = match_text(&[], &TextRule::absent("Reconnect")).unwrap();

        assert!(matched.matched);
        assert!(matched.rect.is_none());
    }

    #[test]
    fn contains_maps_only_the_source_words_in_the_phrase() {
        let words = vec![
            OcrWord::new("Armor", Rect::new(0, 5, 30, 12), 0, 0),
            OcrWord::new("MAX", Rect::new(40, 5, 30, 12), 0, 1),
            OcrWord::new("Health", Rect::new(75, 5, 48, 12), 0, 2),
            OcrWord::new("Bonus", Rect::new(130, 5, 35, 12), 0, 3),
        ];

        let matched = match_text(&words, &TextRule::contains("max   health")).unwrap();

        assert_eq!(matched.rect, Some(Rect::new(40, 5, 83, 12)));
    }

    #[test]
    fn empty_ocr_words_do_not_create_extra_normalized_whitespace() {
        let words = vec![
            OcrWord::new("Max", Rect::new(10, 5, 30, 12), 0, 0),
            OcrWord::new("  ", Rect::new(42, 5, 1, 12), 0, 1),
            OcrWord::new("Health", Rect::new(44, 5, 48, 12), 0, 2),
        ];

        let matched = match_text(&words, &TextRule::contains("max health")).unwrap();

        assert!(matched.matched);
        assert_eq!(matched.rect, Some(Rect::new(10, 5, 82, 12)));
        assert_eq!(matched.source_word_indices, vec![0, 2]);
    }

    #[test]
    fn phrase_does_not_cross_lines_unless_rule_allows_it() {
        let words = vec![
            OcrWord::new("Max", Rect::new(10, 5, 30, 12), 0, 0),
            OcrWord::new("Health", Rect::new(10, 25, 48, 12), 1, 0),
        ];
        let mut rule = TextRule::contains("max health");

        assert!(!match_text(&words, &rule).unwrap().matched);

        rule.allow_cross_line = true;
        let matched = match_text(&words, &rule).unwrap();
        assert!(matched.matched);
        assert_eq!(matched.rect, Some(Rect::new(10, 5, 48, 32)));
    }

    #[test]
    fn exact_requires_the_complete_normalized_line() {
        let words = vec![
            OcrWord::new("Max", Rect::new(10, 5, 30, 12), 0, 0),
            OcrWord::new("Health", Rect::new(44, 5, 48, 12), 0, 1),
        ];

        assert!(
            match_text(&words, &rule("max health", TextMatchMode::Exact))
                .unwrap()
                .matched
        );
        assert!(
            !match_text(&words, &rule("health", TextMatchMode::Exact))
                .unwrap()
                .matched
        );
    }

    #[test]
    fn repeated_text_is_deduplicated_by_box_before_exactly_one_policy() {
        let words = vec![
            OcrWord::new("Ready", Rect::new(10, 5, 40, 12), 0, 0),
            OcrWord::new("Ready", Rect::new(10, 5, 40, 12), 1, 0),
        ];
        let mut rule = TextRule::contains("ready");
        rule.match_policy = MatchSelectionPolicy::ExactlyOne;

        let matched = match_text(&words, &rule).unwrap();

        assert!(matched.matched);
        assert_eq!(matched.match_count, 1);
        assert_eq!(matched.rect, Some(Rect::new(10, 5, 40, 12)));
    }

    #[test]
    fn exactly_one_rejects_distinct_repeated_boxes_without_geometry() {
        let words = vec![
            OcrWord::new("Ready", Rect::new(10, 5, 40, 12), 0, 0),
            OcrWord::new("Ready", Rect::new(10, 25, 40, 12), 1, 0),
        ];
        let mut rule = TextRule::contains("ready");
        rule.match_policy = MatchSelectionPolicy::ExactlyOne;

        let matched = match_text(&words, &rule).unwrap();

        assert!(!matched.matched);
        assert_eq!(matched.match_count, 2);
        assert!(matched.rect.is_none());
    }

    #[test]
    fn topmost_and_bottommost_policies_are_deterministic() {
        let words = vec![
            OcrWord::new("Ready", Rect::new(30, 25, 40, 12), 0, 0),
            OcrWord::new("Ready", Rect::new(10, 5, 40, 12), 1, 0),
        ];
        let mut rule = TextRule::contains("ready");
        rule.match_policy = MatchSelectionPolicy::Topmost;
        assert_eq!(
            match_text(&words, &rule).unwrap().rect,
            Some(Rect::new(10, 5, 40, 12))
        );

        rule.match_policy = MatchSelectionPolicy::Bottommost;
        assert_eq!(
            match_text(&words, &rule).unwrap().rect,
            Some(Rect::new(30, 25, 40, 12))
        );
    }

    #[test]
    fn fuzzy_match_retains_the_best_source_word_box() {
        let words = vec![
            OcrWord::new("Reedy", Rect::new(10, 5, 40, 12), 0, 0),
            OcrWord::new("Ready", Rect::new(10, 25, 40, 12), 1, 0),
        ];
        let mut rule = rule("ready", TextMatchMode::Fuzzy);
        rule.threshold = 0.8;
        rule.match_policy = MatchSelectionPolicy::HighestScore;

        let matched = match_text(&words, &rule).unwrap();

        assert_eq!(matched.rect, Some(Rect::new(10, 25, 40, 12)));
        assert_eq!(matched.score, Some(1.0));
        assert_eq!(matched.match_count, 2);
    }

    #[test]
    fn present_absent_condition_is_negative_and_never_carries_geometry() {
        let words = vec![OcrWord::new("Reconnect", Rect::new(10, 5, 70, 12), 0, 0)];

        let matched = match_text(&words, &TextRule::absent("Reconnect")).unwrap();

        assert!(!matched.matched);
        assert!(matched.rect.is_none());
    }

    #[test]
    fn saved_word_fixture_keeps_reading_order_and_integer_rectangles() {
        let words: Vec<OcrWord> = serde_json::from_str(include_str!(
            "../../../tests/fixtures/macro/text/words.json"
        ))
        .unwrap();

        assert_eq!(words.len(), 4);
        assert_eq!((words[2].line_index, words[2].word_index), (1, 0));
        assert_eq!(words[3].rect, Rect::new(54, 25, 58, 12));
    }

    #[test]
    fn original_profile_preserves_color_in_bgra_memory_order() {
        let image = ScreenImage::new(RgbaImage::from_pixel(1, 1, Rgba([10, 20, 30, 255])));
        let mut worker = TextPreprocessWorker::default();

        let prepared = worker.prepare(&image, PreprocessProfile::Original).unwrap();

        assert_eq!(prepared.frame.pixel_format, OcrPixelFormat::Bgra8);
        assert_eq!(prepared.frame.pixels, vec![30, 20, 10, 255]);
        assert_eq!(prepared.coordinate_scale, 1);
    }

    #[test]
    fn grayscale_profile_only_converts_luminance() {
        let image = ScreenImage::new(RgbaImage::from_pixel(2, 1, Rgba([255, 0, 0, 255])));
        let mut worker = TextPreprocessWorker::default();

        let prepared = worker
            .prepare(&image, PreprocessProfile::Grayscale)
            .unwrap();

        assert_eq!(prepared.frame.pixel_format, OcrPixelFormat::Gray8);
        assert_eq!((prepared.frame.width, prepared.frame.height), (2, 1));
        assert_eq!(prepared.frame.pixels[0], prepared.frame.pixels[1]);
        assert_eq!(prepared.coordinate_scale, 1);
    }

    #[test]
    fn high_contrast_profile_applies_deterministic_otsu_binary_threshold() {
        let image = ScreenImage::new(RgbaImage::from_fn(4, 1, |x, _| {
            let value = [10, 20, 220, 240][x as usize];
            Rgba([value, value, value, 255])
        }));
        let mut worker = TextPreprocessWorker::default();

        let prepared = worker
            .prepare(&image, PreprocessProfile::HighContrast)
            .unwrap();

        assert_eq!(prepared.frame.pixel_format, OcrPixelFormat::Gray8);
        assert_eq!(prepared.frame.pixels, vec![0, 0, 255, 255]);
        assert_eq!(prepared.coordinate_scale, 1);
    }

    #[test]
    fn small_text_profile_enlarges_grayscale_exactly_two_times() {
        let image = ScreenImage::new(RgbaImage::from_pixel(3, 2, Rgba([255, 255, 255, 255])));
        let mut worker = TextPreprocessWorker::default();

        let prepared = worker
            .prepare(&image, PreprocessProfile::SmallText)
            .unwrap();

        assert_eq!(prepared.frame.pixel_format, OcrPixelFormat::Gray8);
        assert_eq!((prepared.frame.width, prepared.frame.height), (6, 4));
        assert_eq!(prepared.coordinate_scale, 2);
    }

    #[test]
    fn small_text_boxes_map_back_to_capture_relative_integer_geometry() {
        let rect = map_processed_rect_to_capture(
            Rect::new(21, 11, 59, 25),
            2,
            Rect::new(100, 50, 100, 50),
        )
        .unwrap();

        assert_eq!(rect, Rect::new(110, 55, 30, 13));
    }

    #[test]
    fn inverse_scaled_geometry_is_intersected_with_capture_rect() {
        let capture = Rect::new(100, 50, 20, 10);

        assert_eq!(
            map_processed_rect_to_capture(Rect::new(30, 12, 20, 16), 2, capture).unwrap(),
            Rect::new(115, 56, 5, 4)
        );
        assert!(map_processed_rect_to_capture(Rect::new(50, 0, 4, 4), 2, capture).is_err());
    }

    #[test]
    fn small_text_inverse_bounds_never_escape_odd_sized_capture() {
        let capture = Rect::new(7, 9, 5, 3);

        let rect = map_processed_rect_to_capture(Rect::new(8, 4, 4, 4), 2, capture).unwrap();

        assert_eq!(rect, Rect::new(11, 11, 1, 1));
    }

    #[test]
    fn preprocess_worker_reuses_frame_allocation_for_same_size_polls() {
        let image = ScreenImage::new(RgbaImage::from_pixel(32, 16, Rgba([40, 50, 60, 255])));
        let mut worker = TextPreprocessWorker::default();

        worker
            .prepare(&image, PreprocessProfile::Grayscale)
            .unwrap();
        let first = worker.buffer_stats();
        worker
            .prepare(&image, PreprocessProfile::Grayscale)
            .unwrap();
        let second = worker.buffer_stats();

        assert_eq!(first.frame_ptr, second.frame_ptr);
        assert_eq!(first.frame_capacity, second.frame_capacity);
        assert_eq!(first.frame_growths, second.frame_growths);
        assert_eq!(second.frame_len, 32 * 16);
    }

    #[test]
    fn preprocess_worker_resizes_for_dimensions_and_profile_then_reuses_scratch() {
        let small = ScreenImage::new(RgbaImage::from_pixel(3, 2, Rgba([40, 50, 60, 255])));
        let color = ScreenImage::new(RgbaImage::from_pixel(4, 3, Rgba([10, 20, 30, 255])));
        let mut worker = TextPreprocessWorker::default();

        let grayscale = worker
            .prepare(&small, PreprocessProfile::Grayscale)
            .unwrap();
        assert_eq!((grayscale.frame.width, grayscale.frame.height), (3, 2));
        assert_eq!(grayscale.frame.pixel_format, OcrPixelFormat::Gray8);
        let enlarged = worker
            .prepare(&small, PreprocessProfile::SmallText)
            .unwrap();
        assert_eq!((enlarged.frame.width, enlarged.frame.height), (6, 4));
        assert_eq!(enlarged.frame.pixels.len(), 24);
        let small_text_stats = worker.buffer_stats();
        worker
            .prepare(&small, PreprocessProfile::SmallText)
            .unwrap();
        let repeated_small_text_stats = worker.buffer_stats();
        assert_eq!(
            small_text_stats.scratch_growths,
            repeated_small_text_stats.scratch_growths
        );
        assert_eq!(
            small_text_stats.scratch_ptr,
            repeated_small_text_stats.scratch_ptr
        );

        let original = worker.prepare(&color, PreprocessProfile::Original).unwrap();
        assert_eq!((original.frame.width, original.frame.height), (4, 3));
        assert_eq!(original.frame.pixel_format, OcrPixelFormat::Bgra8);
        assert_eq!(original.frame.pixels.len(), 4 * 3 * 4);
    }

    #[test]
    fn frame_reserve_accounts_for_shrink_then_growth_without_resize_allocation() {
        let image =
            |width| ScreenImage::new(RgbaImage::from_pixel(width, 1, Rgba([40, 50, 60, 255])));
        let mut worker = TextPreprocessWorker::default();
        worker
            .prepare(&image(100), PreprocessProfile::Grayscale)
            .unwrap();
        worker
            .prepare(&image(80), PreprocessProfile::Grayscale)
            .unwrap();
        let before_growth = worker.buffer_stats();
        let target_width = u32::try_from(before_growth.frame_capacity + 50).unwrap();

        worker
            .prepare(&image(target_width), PreprocessProfile::Grayscale)
            .unwrap();
        let grown = worker.buffer_stats();
        worker
            .prepare(&image(target_width), PreprocessProfile::Grayscale)
            .unwrap();
        let repeated = worker.buffer_stats();

        assert_eq!(grown.frame_growths, before_growth.frame_growths + 1);
        assert_eq!(
            grown.frame_resize_growths,
            before_growth.frame_resize_growths
        );
        assert!(grown.frame_capacity >= target_width as usize);
        assert_eq!(repeated.frame_ptr, grown.frame_ptr);
        assert_eq!(repeated.frame_capacity, grown.frame_capacity);
        assert_eq!(repeated.frame_growths, grown.frame_growths);
    }

    #[test]
    fn small_text_scratch_reserve_accounts_for_shrink_then_growth() {
        let image =
            |width| ScreenImage::new(RgbaImage::from_pixel(width, 1, Rgba([40, 50, 60, 255])));
        let mut worker = TextPreprocessWorker::default();
        worker
            .prepare(&image(100), PreprocessProfile::SmallText)
            .unwrap();
        worker
            .prepare(&image(80), PreprocessProfile::SmallText)
            .unwrap();
        let before_growth = worker.buffer_stats();
        let target_width = u32::try_from(before_growth.scratch_capacity + 50).unwrap();

        worker
            .prepare(&image(target_width), PreprocessProfile::SmallText)
            .unwrap();
        let grown = worker.buffer_stats();
        worker
            .prepare(&image(target_width), PreprocessProfile::SmallText)
            .unwrap();
        let repeated = worker.buffer_stats();

        assert_eq!(grown.scratch_growths, before_growth.scratch_growths + 1);
        assert_eq!(
            grown.scratch_resize_growths,
            before_growth.scratch_resize_growths
        );
        assert!(grown.scratch_capacity >= target_width as usize);
        assert_eq!(repeated.scratch_ptr, grown.scratch_ptr);
        assert_eq!(repeated.scratch_capacity, grown.scratch_capacity);
        assert_eq!(repeated.scratch_growths, grown.scratch_growths);
    }

    #[derive(Default)]
    struct FakeRecognizer {
        calls: Mutex<Vec<(OcrPixelFormat, u32, u32)>>,
        words: Vec<OcrWord>,
    }

    impl PositionedTextRecognizer for FakeRecognizer {
        fn recognize_words(&self, frame: &OcrFrame, _language_tag: &str) -> Result<Vec<OcrWord>> {
            self.calls
                .lock()
                .unwrap()
                .push((frame.pixel_format, frame.width, frame.height));
            Ok(self.words.clone())
        }
    }

    struct GatedRecognizer {
        started: mpsc::Sender<()>,
        release: Arc<(Mutex<bool>, Condvar)>,
    }

    impl PositionedTextRecognizer for GatedRecognizer {
        fn recognize_words(&self, _frame: &OcrFrame, _language_tag: &str) -> Result<Vec<OcrWord>> {
            let _ = self.started.send(());
            let (lock, wake) = &*self.release;
            let mut released = lock.lock().unwrap();
            while !*released {
                released = wake.wait(released).unwrap();
            }
            Ok(vec![OcrWord::new("ready", Rect::new(2, 3, 20, 5), 0, 0)])
        }
    }

    #[derive(Default)]
    struct FakeCapture {
        rects: Mutex<Vec<Rect>>,
        next_frame_id: AtomicU64,
    }

    impl CaptureSource for FakeCapture {
        fn capture(&self, _rect: Rect) -> Result<ScreenImage> {
            anyhow::bail!("executable text detection must not use raw capture")
        }

        fn capture_frame(&self, rect: Rect) -> Result<CapturedScreenFrame> {
            self.rects.lock().unwrap().push(rect);
            Ok(CapturedScreenFrame {
                image: ScreenImage::new(RgbaImage::from_fn(rect.width, rect.height, |x, _| {
                    let value = if x < rect.width / 2 { 10 } else { 240 };
                    Rgba([value, value, value, 255])
                })),
                metadata: CaptureFrameMetadata {
                    frame_id: self.next_frame_id.fetch_add(1, Ordering::Relaxed) + 1,
                    captured_at_ms: 42,
                    window_id: 1,
                    window_revision: 1,
                    process_id: 4,
                    process_started_at_100ns: 6,
                    client_x: 0,
                    client_y: 0,
                    client_width: 100,
                    client_height: 50,
                    geometry_revision: 1,
                    display_id: 1,
                    display_profile_revision: 1,
                    dpi: 96,
                    is_visible: true,
                    is_minimized: false,
                    is_foreground: true,
                },
            })
        }
    }

    #[derive(Default)]
    struct RawOnlyCapture;

    impl CaptureSource for RawOnlyCapture {
        fn capture(&self, rect: Rect) -> Result<ScreenImage> {
            Ok(ScreenImage::new(RgbaImage::from_pixel(
                rect.width,
                rect.height,
                Rgba([255, 255, 255, 255]),
            )))
        }
    }

    struct PairedFrameCapture {
        raw_calls: Mutex<Vec<Rect>>,
        frame_calls: Mutex<Vec<Rect>>,
        metadata: CaptureFrameMetadata,
    }

    impl CaptureSource for PairedFrameCapture {
        fn capture(&self, rect: Rect) -> Result<ScreenImage> {
            self.raw_calls.lock().unwrap().push(rect);
            anyhow::bail!("executable text detection must not use raw capture")
        }

        fn capture_frame(&self, rect: Rect) -> Result<CapturedScreenFrame> {
            self.frame_calls.lock().unwrap().push(rect);
            Ok(CapturedScreenFrame {
                image: ScreenImage::new(RgbaImage::from_fn(rect.width, rect.height, |x, _| {
                    let value = if x < rect.width / 2 { 10 } else { 240 };
                    Rgba([value, value, value, 255])
                })),
                metadata: self.metadata,
            })
        }
    }

    fn compiled_text_macro(profile: PreprocessProfile, stable_frames: u8) -> CompiledMacro {
        let condition = Condition::Text {
            source_block_id: "observe".to_string(),
            rule_id: "text".to_string(),
            mode: ObserveMode::CheckNow,
        };
        let definition = MacroDefinition {
            schema_version: MACRO_SCHEMA_VERSION,
            id: "macro".to_string(),
            name: "Text fixture".to_string(),
            revision: 4,
            target: TargetProfile {
                process_path: "game.exe".to_string(),
                window_class: "game".to_string(),
                title_contains: "Diablo".to_string(),
                captured_client_width: 100,
                captured_client_height: 50,
                captured_dpi: 96,
            },
            regions: vec![RegionDefinition {
                id: "region".to_string(),
                revision: 7,
                rect: crate::engine::types::RectRatio {
                    x: 0.1,
                    y: 0.2,
                    width: 0.5,
                    height: 0.4,
                },
            }],
            points: vec![],
            text_rules: vec![TextRule {
                id: "text".to_string(),
                revision: 9,
                region_id: "region".to_string(),
                language: "en-US".to_string(),
                preprocess: profile,
                expected: "ready".to_string(),
                match_mode: TextMatchMode::Contains,
                threshold: 0.9,
                case_sensitive: false,
                allow_cross_line: false,
                match_policy: MatchSelectionPolicy::FirstReadingOrder,
                poll_interval_ms: 10,
                timeout_ms: Limit::Finite(100),
                stable_frames,
            }],
            image_rules: vec![],
            blocks: vec![Block {
                id: "observe".to_string(),
                enabled: true,
                kind: BlockKind::Observe {
                    condition: condition.clone(),
                },
            }],
            safety: SafetyPolicy {
                max_runtime_ms: Limit::Finite(1_000),
                max_clicks: Limit::Finite(1),
                max_observation_retries: Limit::Finite(10),
                max_observations_per_second: 30,
                minimum_click_interval_ms: 10,
                focus_loss: FocusLossPolicy::Stop,
            },
        };
        let definition_hash = format!(
            "{:x}",
            Sha256::digest(serde_json::to_vec_pretty(&definition).unwrap())
        );
        CompiledMacro::compile(SavedRevision {
            definition,
            definition_hash,
            pinned_assets: vec![],
        })
        .unwrap()
    }

    fn text_condition() -> Condition {
        Condition::Text {
            source_block_id: "observe".to_string(),
            rule_id: "text".to_string(),
            mode: ObserveMode::CheckNow,
        }
    }

    #[test]
    fn condition_detector_runs_one_selected_profile_and_binds_revisioned_evidence() {
        let recognizer = Arc::new(FakeRecognizer {
            calls: Mutex::default(),
            words: vec![OcrWord::new("Ready", Rect::new(2, 3, 20, 5), 0, 0)],
        });
        let detector = TextDetector::with_recognizer(recognizer.clone());
        let capture = FakeCapture::default();
        let compiled = compiled_text_macro(PreprocessProfile::HighContrast, 1);
        let condition = text_condition();
        let request = ObservationRequest {
            run_id: "run-1",
            generation: 3,
            side_effect_epoch: 0,
            condition: &condition,
            compiled: &compiled,
            observed_at_ms: 42,
        };

        let evidence = detector.observe(&request, &capture).unwrap();

        assert!(evidence.matched);
        assert_eq!(
            capture.rects.lock().unwrap().as_slice(),
            &[Rect::new(10, 10, 50, 20)]
        );
        assert_eq!(
            recognizer.calls.lock().unwrap().as_slice(),
            &[(OcrPixelFormat::Gray8, 50, 20)]
        );
        assert_eq!(evidence.match_rect, Some(Rect::new(12, 13, 20, 5)));
        assert_eq!(evidence.frame_id, 1);
        assert_eq!(evidence.captured_at_ms, 42);
        assert_eq!(evidence.details["rule_id"], "text");
        assert_eq!(evidence.details["rule_revision"], 9);
        assert_eq!(evidence.details["region_id"], "region");
        assert_eq!(evidence.details["region_revision"], 7);
        assert_eq!(evidence.details["source_block_id"], "observe");
    }

    #[test]
    fn executable_text_detector_uses_one_paired_frame_and_preserves_canonical_metadata() {
        let recognizer = Arc::new(FakeRecognizer {
            calls: Mutex::default(),
            words: vec![OcrWord::new("Ready", Rect::new(2, 3, 20, 5), 0, 0)],
        });
        let detector = TextDetector::with_recognizer(recognizer.clone());
        let capture = PairedFrameCapture {
            raw_calls: Mutex::default(),
            frame_calls: Mutex::default(),
            metadata: CaptureFrameMetadata {
                frame_id: 44,
                captured_at_ms: 900,
                window_id: 77,
                window_revision: 5,
                process_id: 4,
                process_started_at_100ns: 6,
                client_x: -320,
                client_y: 180,
                client_width: 100,
                client_height: 50,
                geometry_revision: 6,
                display_id: 8,
                display_profile_revision: 7,
                dpi: 96,
                is_visible: true,
                is_minimized: false,
                is_foreground: true,
            },
        };
        let compiled = compiled_text_macro(PreprocessProfile::HighContrast, 1);
        let condition = text_condition();
        let request = ObservationRequest {
            run_id: "run-1",
            generation: 3,
            side_effect_epoch: 0,
            condition: &condition,
            compiled: &compiled,
            observed_at_ms: 42,
        };

        let evidence = detector.observe(&request, &capture).unwrap();

        assert!(evidence.matched);
        assert!(capture.raw_calls.lock().unwrap().is_empty());
        assert_eq!(
            capture.frame_calls.lock().unwrap().as_slice(),
            &[Rect::new(10, 10, 50, 20)]
        );
        assert_eq!(
            recognizer.calls.lock().unwrap().as_slice(),
            &[(OcrPixelFormat::Gray8, 50, 20)]
        );
        assert_eq!(evidence.frame_id, 44);
        assert_eq!(evidence.captured_at_ms, 900);
        let frame = evidence.frame_metadata.expect("canonical frame metadata");
        assert_eq!((frame.window_id, frame.window_revision), (77, 5));
        assert_eq!((frame.client_x, frame.client_y), (-320, 180));
        assert_eq!((frame.client_width, frame.client_height), (100, 50));
        assert_eq!((frame.region_revision, frame.rule_revision), (7, 9));
        assert_eq!(evidence.match_rect, Some(Rect::new(12, 13, 20, 5)));
    }

    #[test]
    fn executable_text_detector_fails_closed_when_capture_has_no_paired_metadata() {
        let recognizer = Arc::new(FakeRecognizer {
            calls: Mutex::default(),
            words: vec![OcrWord::new("Ready", Rect::new(2, 3, 20, 5), 0, 0)],
        });
        let detector = TextDetector::with_recognizer(recognizer.clone());
        let capture = RawOnlyCapture;
        let compiled = compiled_text_macro(PreprocessProfile::Original, 1);
        let condition = text_condition();
        let request = ObservationRequest {
            run_id: "run-1",
            generation: 3,
            side_effect_epoch: 0,
            condition: &condition,
            compiled: &compiled,
            observed_at_ms: 42,
        };

        let error = detector.observe(&request, &capture).unwrap_err();

        assert!(
            error
                .to_string()
                .contains("atomic pixels and frame metadata")
        );
        assert!(recognizer.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn text_stability_requires_distinct_consecutive_polls_before_geometry_is_clickable() {
        let recognizer = Arc::new(FakeRecognizer {
            calls: Mutex::default(),
            words: vec![OcrWord::new("Ready", Rect::new(2, 3, 20, 5), 0, 0)],
        });
        let detector = TextDetector::with_recognizer(recognizer);
        let capture = FakeCapture::default();
        let compiled = compiled_text_macro(PreprocessProfile::Original, 2);
        let condition = text_condition();
        let request = ObservationRequest {
            run_id: "run-1",
            generation: 3,
            side_effect_epoch: 0,
            condition: &condition,
            compiled: &compiled,
            observed_at_ms: 42,
        };

        let first = detector.observe(&request, &capture).unwrap();
        let second = detector.observe(&request, &capture).unwrap();

        assert!(!first.matched);
        assert_eq!(first.stable_frames, 1);
        assert!(first.match_rect.is_none());
        assert!(second.matched);
        assert_eq!(second.stable_frames, 2);
        assert_eq!(second.match_rect, Some(Rect::new(12, 13, 20, 5)));
        assert_ne!(first.frame_id, second.frame_id);
    }

    #[test]
    fn offline_profile_benchmark_evaluates_all_profiles_without_capture() {
        let recognizer = FakeRecognizer {
            calls: Mutex::default(),
            words: vec![OcrWord::new("Ready", Rect::new(2, 3, 20, 5), 0, 0)],
        };
        let compiled = compiled_text_macro(PreprocessProfile::Original, 1);
        let sample = SavedTextProfileSample {
            name: "positive-ready".to_string(),
            image: ScreenImage::new(RgbaImage::from_pixel(10, 5, Rgba([255, 255, 255, 255]))),
            rule: compiled.definition().text_rules[0].clone(),
            expected_match: true,
        };

        let result = benchmark_text_profiles(&recognizer, &[sample], 1.0).unwrap();

        assert_eq!(result.profiles.len(), 4);
        assert!(
            result
                .profiles
                .iter()
                .all(|profile| profile.correct == 5 && profile.total == 5)
        );
        assert!(result.recommended.is_some());
        assert_eq!(recognizer.calls.lock().unwrap().len(), 21);
    }

    #[derive(Default)]
    struct FakeBenchmarkClock {
        now: std::sync::atomic::AtomicU64,
    }

    impl FakeBenchmarkClock {
        fn advance(&self, nanos: u64) {
            self.now.fetch_add(nanos, Ordering::Relaxed);
        }
    }

    impl TextBenchmarkClock for FakeBenchmarkClock {
        fn now_nanos(&self) -> u128 {
            u128::from(self.now.load(Ordering::Relaxed))
        }
    }

    struct TimedFakeRecognizer {
        clock: Arc<FakeBenchmarkClock>,
        calls: std::sync::atomic::AtomicUsize,
    }

    impl PositionedTextRecognizer for TimedFakeRecognizer {
        fn recognize_words(&self, frame: &OcrFrame, _language_tag: &str) -> Result<Vec<OcrWord>> {
            let call = self.calls.fetch_add(1, Ordering::Relaxed);
            let steady_nanos = match (frame.pixel_format, frame.width) {
                (OcrPixelFormat::Bgra8, _) => 1,
                (OcrPixelFormat::Gray8, 20) => 8,
                (OcrPixelFormat::Gray8, _) => 4,
            };
            self.clock
                .advance(if call == 0 { 1_000 } else { steady_nanos });
            Ok(vec![OcrWord::new("Ready", Rect::new(1, 1, 5, 2), 0, 0)])
        }
    }

    #[test]
    fn benchmark_warms_ocr_and_uses_interleaved_medians_not_first_call_order() {
        let clock = Arc::new(FakeBenchmarkClock::default());
        let recognizer = TimedFakeRecognizer {
            clock: clock.clone(),
            calls: std::sync::atomic::AtomicUsize::new(0),
        };
        let compiled = compiled_text_macro(PreprocessProfile::Original, 1);
        let sample = SavedTextProfileSample {
            name: "positive-ready".to_string(),
            image: ScreenImage::new(RgbaImage::from_pixel(10, 5, Rgba([255, 255, 255, 255]))),
            rule: compiled.definition().text_rules[0].clone(),
            expected_match: true,
        };

        let result =
            benchmark_text_profiles_with_clock(&recognizer, &[sample], 1.0, &*clock, 3).unwrap();

        assert_eq!(result.recommended, Some(PreprocessProfile::Original));
        assert_eq!(recognizer.calls.load(Ordering::Relaxed), 13);
        let original = result
            .profiles
            .iter()
            .find(|profile| profile.profile == PreprocessProfile::Original)
            .unwrap();
        assert_eq!(original.median_elapsed_nanos, 1);
    }

    #[test]
    fn new_side_effect_epoch_cannot_continue_text_stability() {
        let detector = TextDetector::with_recognizer(Arc::new(FakeRecognizer {
            calls: Mutex::default(),
            words: vec![],
        }));
        let mut key = StabilityKey {
            run_id: "run".to_string(),
            generation: 2,
            side_effect_epoch: 0,
            source_block_id: "observe".to_string(),
            rule_id: "rule".to_string(),
            rule_revision: 1,
            region_id: "region".to_string(),
            region_revision: 1,
        };
        let text_match = TextMatch {
            matched: true,
            rect: Some(Rect::new(1, 2, 3, 4)),
            score: Some(1.0),
            match_count: 1,
            source_word_indices: vec![0],
        };
        detector.begin_observation("run", 2, 0).unwrap();
        assert_eq!(detector.update_stability(&key, &text_match).unwrap(), 1);
        assert_eq!(detector.update_stability(&key, &text_match).unwrap(), 2);

        key.side_effect_epoch = 1;
        detector.side_effect_boundary("run", 2, 1);

        assert_eq!(detector.update_stability(&key, &text_match).unwrap(), 1);
    }

    #[test]
    fn interleaved_text_runs_preserve_each_others_stability() {
        let detector = TextDetector::with_recognizer(Arc::new(FakeRecognizer {
            calls: Mutex::default(),
            words: vec![],
        }));
        let key = |run_id: &str| StabilityKey {
            run_id: run_id.to_string(),
            generation: 1,
            side_effect_epoch: 0,
            source_block_id: "observe".to_string(),
            rule_id: "rule".to_string(),
            rule_revision: 1,
            region_id: "region".to_string(),
            region_revision: 1,
        };
        let text_match = TextMatch {
            matched: true,
            rect: Some(Rect::new(1, 2, 3, 4)),
            score: Some(1.0),
            match_count: 1,
            source_word_indices: vec![0],
        };
        detector.begin_observation("run-a", 1, 0).unwrap();
        assert_eq!(
            detector
                .update_stability(&key("run-a"), &text_match)
                .unwrap(),
            1
        );
        detector.begin_observation("run-b", 1, 0).unwrap();
        assert_eq!(
            detector
                .update_stability(&key("run-b"), &text_match)
                .unwrap(),
            1
        );

        assert_eq!(
            detector
                .update_stability(&key("run-a"), &text_match)
                .unwrap(),
            2
        );
        assert_eq!(detector.state.lock().unwrap().watermark_by_run.len(), 2);
    }

    #[test]
    fn late_text_commit_cannot_repopulate_pre_action_epoch_and_boundaries_stay_bounded() {
        let detector = TextDetector::with_recognizer(Arc::new(FakeRecognizer {
            calls: Mutex::default(),
            words: vec![],
        }));
        let old_key = StabilityKey {
            run_id: "run".to_string(),
            generation: 2,
            side_effect_epoch: 0,
            source_block_id: "observe".to_string(),
            rule_id: "rule".to_string(),
            rule_revision: 1,
            region_id: "region".to_string(),
            region_revision: 1,
        };
        let text_match = TextMatch {
            matched: true,
            rect: Some(Rect::new(1, 2, 3, 4)),
            score: Some(1.0),
            match_count: 1,
            source_word_indices: vec![0],
        };
        detector.begin_observation("run", 2, 0).unwrap();
        for epoch in 1..=1_000 {
            detector.side_effect_boundary("run", 2, epoch);
        }

        assert_eq!(detector.update_stability(&old_key, &text_match).unwrap(), 0);
        let state = detector.state.lock().unwrap();
        assert!(state.stability.is_empty());
        assert_eq!(state.watermark_by_run.len(), 1);
        assert_eq!(state.watermark_by_run["run"].side_effect_epoch, 1_000);
    }

    #[test]
    fn in_flight_text_observation_cannot_commit_after_side_effect_boundary() {
        let (started_tx, started_rx) = mpsc::channel();
        let release = Arc::new((Mutex::new(false), Condvar::new()));
        let detector = Arc::new(TextDetector::with_recognizer(Arc::new(GatedRecognizer {
            started: started_tx,
            release: Arc::clone(&release),
        })));
        let worker_detector = Arc::clone(&detector);
        let handle = thread::spawn(move || {
            let compiled = compiled_text_macro(PreprocessProfile::Original, 1);
            let condition = text_condition();
            worker_detector.observe(
                &ObservationRequest {
                    run_id: "run",
                    generation: 2,
                    side_effect_epoch: 0,
                    condition: &condition,
                    compiled: &compiled,
                    observed_at_ms: 42,
                },
                &FakeCapture::default(),
            )
        });
        started_rx.recv().unwrap();

        detector.side_effect_boundary("run", 2, 1);
        let (lock, wake) = &*release;
        *lock.lock().unwrap() = true;
        wake.notify_all();
        let evidence = handle.join().unwrap().unwrap();

        assert!(!evidence.matched);
        assert_eq!(evidence.stable_frames, 0);
        let state = detector.state.lock().unwrap();
        assert!(state.stability.is_empty());
        assert_eq!(state.watermark_by_run["run"].side_effect_epoch, 1);
    }
}
