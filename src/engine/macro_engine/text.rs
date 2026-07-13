use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::Instant,
};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::engine::{
    platform::{OcrFrame, PositionedOcrWord},
    types::{Rect, ScreenImage},
};

use super::{
    Condition, ConditionDetector, DetectorEvidence, Limit, MatchSelectionPolicy,
    ObservationRequest, PreprocessProfile, TextMatchMode, TextRule,
};
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

#[derive(Debug, Clone)]
struct PreparedOcrFrame {
    frame: OcrFrame,
    coordinate_scale: u32,
}

fn preprocess_frame(image: &ScreenImage, profile: PreprocessProfile) -> PreparedOcrFrame {
    match profile {
        PreprocessProfile::Original => PreparedOcrFrame {
            frame: OcrFrame::from_bgra_screen_image(image),
            coordinate_scale: 1,
        },
        PreprocessProfile::Grayscale => {
            let gray = image::DynamicImage::ImageRgba8(image.rgba.clone()).into_luma8();
            PreparedOcrFrame {
                frame: OcrFrame::from_gray_image(&gray),
                coordinate_scale: 1,
            }
        }
        PreprocessProfile::HighContrast => {
            let mut gray = image::DynamicImage::ImageRgba8(image.rgba.clone()).into_luma8();
            let threshold = otsu_threshold(&gray);
            for pixel in gray.pixels_mut() {
                pixel[0] = if pixel[0] > threshold { 255 } else { 0 };
            }
            PreparedOcrFrame {
                frame: OcrFrame::from_gray_image(&gray),
                coordinate_scale: 1,
            }
        }
        PreprocessProfile::SmallText => {
            let gray = image::DynamicImage::ImageRgba8(image.rgba.clone()).into_luma8();
            let enlarged = image::imageops::resize(
                &gray,
                gray.width().saturating_mul(2),
                gray.height().saturating_mul(2),
                image::imageops::FilterType::Triangle,
            );
            PreparedOcrFrame {
                frame: OcrFrame::from_gray_image(&enlarged),
                coordinate_scale: 2,
            }
        }
    }
}

fn otsu_threshold(image: &image::GrayImage) -> u8 {
    let mut histogram = [0u64; 256];
    for pixel in image.pixels() {
        histogram[pixel[0] as usize] += 1;
    }
    let total = u64::from(image.width()) * u64::from(image.height());
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
    capture_x: i32,
    capture_y: i32,
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
    Ok(Rect::new(
        i32::try_from(i64::from(capture_x) + left)?,
        i32::try_from(i64::from(capture_y) + top)?,
        u32::try_from(right - left)?,
        u32::try_from(bottom - top)?,
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

pub struct TextDetector {
    recognizer: Arc<dyn PositionedTextRecognizer>,
    next_frame_id: AtomicU64,
    stability: Mutex<HashMap<StabilityKey, StabilityState>>,
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
            next_frame_id: AtomicU64::new(1),
            stability: Mutex::new(HashMap::new()),
        }
    }

    fn observe_text(
        &self,
        request: &ObservationRequest<'_>,
        capture: &(dyn CaptureSource + Send + Sync),
        source_block_id: &str,
        rule_id: &str,
    ) -> Result<DetectorEvidence> {
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
        let image = capture.capture(capture_rect)?;
        let prepared = preprocess_frame(&image, rule.preprocess);
        let relative_words = self
            .recognizer
            .recognize_words(&prepared.frame, &rule.language)?;
        let words = relative_words
            .into_iter()
            .map(|word| {
                Ok(OcrWord {
                    rect: map_processed_rect_to_capture(
                        word.rect,
                        prepared.coordinate_scale,
                        capture_rect.x,
                        capture_rect.y,
                    )?,
                    ..word
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let text_match = match_text(&words, rule)?;
        let key = StabilityKey {
            run_id: request.run_id.to_string(),
            generation: request.generation,
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
        Ok(DetectorEvidence::new(
            qualified,
            self.next_frame_id.fetch_add(1, Ordering::Relaxed),
            request.observed_at_ms,
            qualified.then_some(text_match.rect).flatten(),
            text_match.score,
            text_match.match_count,
            stable_frames,
            details,
        ))
    }

    fn update_stability(&self, key: &StabilityKey, text_match: &TextMatch) -> Result<u8> {
        let mut states = self
            .stability
            .lock()
            .map_err(|_| anyhow::anyhow!("text detector stability lock is poisoned"))?;
        states.retain(|existing, _| {
            existing.run_id == key.run_id && existing.generation == key.generation
        });
        if !text_match.matched {
            states.remove(key);
            return Ok(0);
        }
        let state = states.entry(key.clone()).or_insert_with(|| StabilityState {
            rect: text_match.rect,
            source_word_indices: text_match.source_word_indices.clone(),
            count: 0,
        });
        if state.rect == text_match.rect
            && state.source_word_indices == text_match.source_word_indices
        {
            state.count = state.count.saturating_add(1);
        } else {
            state.rect = text_match.rect;
            state.source_word_indices = text_match.source_word_indices.clone();
            state.count = 1;
        }
        Ok(state.count)
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
    pub elapsed_nanos: u128,
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
    if samples.is_empty() {
        bail!("text profile benchmark requires at least one saved sample");
    }
    if !(0.0..=1.0).contains(&accuracy_gate) {
        bail!("text profile benchmark accuracy gate must be between 0 and 1");
    }
    let mut profiles = Vec::new();
    for profile in [
        PreprocessProfile::Original,
        PreprocessProfile::Grayscale,
        PreprocessProfile::HighContrast,
        PreprocessProfile::SmallText,
    ] {
        let started = Instant::now();
        let mut correct = 0;
        for sample in samples {
            let prepared = preprocess_frame(&sample.image, profile);
            let words = recognizer
                .recognize_words(&prepared.frame, &sample.rule.language)
                .with_context(|| format!("OCR failed for saved text sample {:?}", sample.name))?;
            let words = words
                .into_iter()
                .map(|word| {
                    Ok(OcrWord {
                        rect: map_processed_rect_to_capture(
                            word.rect,
                            prepared.coordinate_scale,
                            0,
                            0,
                        )?,
                        ..word
                    })
                })
                .collect::<Result<Vec<_>>>()?;
            if match_text(&words, &sample.rule)?.matched == sample.expected_match {
                correct += 1;
            }
        }
        profiles.push(TextProfileBenchmark {
            profile,
            correct,
            total: samples.len(),
            elapsed_nanos: started.elapsed().as_nanos(),
        });
    }
    let recommended = profiles
        .iter()
        .filter(|profile| profile.accuracy() >= accuracy_gate)
        .min_by_key(|profile| profile.elapsed_nanos)
        .map(|profile| profile.profile);
    Ok(TextProfileBenchmarkResult {
        profiles,
        recommended,
    })
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use anyhow::Result;
    use image::{Rgba, RgbaImage};
    use sha2::{Digest, Sha256};

    use super::*;
    use crate::engine::automation::CaptureSource;
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

        let prepared = preprocess_frame(&image, PreprocessProfile::Original);

        assert_eq!(prepared.frame.pixel_format, OcrPixelFormat::Bgra8);
        assert_eq!(prepared.frame.pixels, vec![30, 20, 10, 255]);
        assert_eq!(prepared.coordinate_scale, 1);
    }

    #[test]
    fn grayscale_profile_only_converts_luminance() {
        let image = ScreenImage::new(RgbaImage::from_pixel(2, 1, Rgba([255, 0, 0, 255])));

        let prepared = preprocess_frame(&image, PreprocessProfile::Grayscale);

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

        let prepared = preprocess_frame(&image, PreprocessProfile::HighContrast);

        assert_eq!(prepared.frame.pixel_format, OcrPixelFormat::Gray8);
        assert_eq!(prepared.frame.pixels, vec![0, 0, 255, 255]);
        assert_eq!(prepared.coordinate_scale, 1);
    }

    #[test]
    fn small_text_profile_enlarges_grayscale_exactly_two_times() {
        let image = ScreenImage::new(RgbaImage::from_pixel(3, 2, Rgba([255, 255, 255, 255])));

        let prepared = preprocess_frame(&image, PreprocessProfile::SmallText);

        assert_eq!(prepared.frame.pixel_format, OcrPixelFormat::Gray8);
        assert_eq!((prepared.frame.width, prepared.frame.height), (6, 4));
        assert_eq!(prepared.coordinate_scale, 2);
    }

    #[test]
    fn small_text_boxes_map_back_to_capture_relative_integer_geometry() {
        let rect = map_processed_rect_to_capture(Rect::new(21, 11, 59, 25), 2, 100, 50).unwrap();

        assert_eq!(rect, Rect::new(110, 55, 30, 13));
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

    #[derive(Default)]
    struct FakeCapture {
        rects: Mutex<Vec<Rect>>,
    }

    impl CaptureSource for FakeCapture {
        fn capture(&self, rect: Rect) -> Result<ScreenImage> {
            self.rects.lock().unwrap().push(rect);
            Ok(ScreenImage::new(RgbaImage::from_fn(
                rect.width,
                rect.height,
                |x, _| {
                    let value = if x < rect.width / 2 { 10 } else { 240 };
                    Rgba([value, value, value, 255])
                },
            )))
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
        assert!(result.profiles.iter().all(|profile| profile.correct == 1));
        assert!(result.recommended.is_some());
        assert_eq!(recognizer.calls.lock().unwrap().len(), 4);
    }
}
