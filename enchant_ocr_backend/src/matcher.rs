use std::collections::HashSet;

use serde::{Deserialize, Serialize};
use strsim::jaro_winkler;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MatchResult {
    pub matched: bool,
    pub target: Option<String>,
    pub score: f64,
    pub raw_text: String,
    pub normalized_text: String,
}

pub fn normalize_ocr_text(text: &str) -> String {
    let mut normalized = String::with_capacity(text.len());
    let lower = text.to_lowercase().replace('|', "i");
    let chars: Vec<char> = lower.chars().collect();

    for (idx, ch) in chars.iter().copied().enumerate() {
        let prev_alpha = idx > 0 && chars[idx - 1].is_ascii_alphabetic();
        let next_alpha = idx + 1 < chars.len() && chars[idx + 1].is_ascii_alphabetic();
        let out = if ch == '0' && (prev_alpha || next_alpha) {
            'o'
        } else if ch.is_ascii_alphanumeric() || ch == '%' || ch == '+' {
            ch
        } else {
            ' '
        };
        normalized.push(out);
    }

    normalized.split_whitespace().collect::<Vec<_>>().join(" ")
}

pub fn match_affix(raw_text: &str, targets: &[String], threshold: f64) -> MatchResult {
    let normalized_text = normalize_ocr_text(raw_text);
    let mut best_target = None;
    let mut best_score = 0.0;

    for target in targets {
        let normalized_target = normalize_ocr_text(target);
        if normalized_target.is_empty() {
            continue;
        }

        if normalized_text.contains(&normalized_target) {
            return MatchResult {
                matched: true,
                target: Some(target.clone()),
                score: 1.0,
                raw_text: raw_text.to_string(),
                normalized_text,
            };
        }

        let token_score = token_match_score(&normalized_target, &normalized_text);
        if token_score > best_score {
            best_score = token_score;
            best_target = Some(target.clone());
        }
        if token_score >= threshold {
            return MatchResult {
                matched: true,
                target: Some(target.clone()),
                score: token_score,
                raw_text: raw_text.to_string(),
                normalized_text,
            };
        }

        let score = jaro_winkler(&normalized_target, &normalized_text);
        if score > best_score {
            best_score = score;
            best_target = Some(target.clone());
        }

        let target_words: Vec<_> = normalized_target.split_whitespace().collect();
        let text_words: Vec<_> = normalized_text.split_whitespace().collect();
        if target_words.len() > 1 && text_words.len() >= target_words.len() {
            for window in text_words.windows(target_words.len()) {
                let window_text = window.join(" ");
                let window_score = jaro_winkler(&normalized_target, &window_text);
                if window_score > best_score {
                    best_score = window_score;
                    best_target = Some(target.clone());
                }
            }
        }
    }

    MatchResult {
        matched: best_score >= threshold,
        target: best_target,
        score: best_score,
        raw_text: raw_text.to_string(),
        normalized_text,
    }
}

fn token_match_score(normalized_target: &str, normalized_text: &str) -> f64 {
    let target_tokens: Vec<_> = normalized_target.split_whitespace().collect();
    let text_tokens: Vec<_> = normalized_text.split_whitespace().collect();
    if target_tokens.is_empty() || text_tokens.is_empty() {
        return 0.0;
    }

    let mut matched = 0;
    let mut search_start = 0;
    for target in &target_tokens {
        let mut found_at = None;
        for (idx, text) in text_tokens.iter().enumerate().skip(search_start) {
            if token_matches(target, text) {
                found_at = Some(idx);
                break;
            }
        }
        if let Some(idx) = found_at {
            matched += 1;
            search_start = idx + 1;
        }
    }

    let coverage = matched as f64 / target_tokens.len() as f64;
    if coverage < 1.0 {
        coverage * 0.65
    } else if target_tokens.len() == 1 {
        0.90
    } else {
        0.94
    }
}

fn token_matches(target: &str, text: &str) -> bool {
    if target == text {
        return true;
    }
    if target.len() >= 3 && text.starts_with(target) {
        return true;
    }
    if text.len() >= 3 && target.starts_with(text) {
        return true;
    }

    let target_variants = variants(target);
    let text_variants = variants(text);
    if !target_variants.is_disjoint(&text_variants) {
        return true;
    }

    target_variants
        .iter()
        .any(|variant| variant.len() >= 4 && text.len() >= 4 && jaro_winkler(variant, text) >= 0.78)
}

fn variants(token: &str) -> HashSet<String> {
    let mut out = HashSet::from([token.to_string()]);
    match token {
        "max" => {
            out.insert("maximum".to_string());
        }
        "maximum" => {
            out.insert("max".to_string());
        }
        "health" => {
            out.extend(["life", "hitpoints", "hp"].iter().map(|s| s.to_string()));
        }
        "life" => {
            out.extend(["health", "hitpoints", "hp"].iter().map(|s| s.to_string()));
        }
        "resource" => {
            out.insert("resources".to_string());
        }
        "resources" => {
            out.insert("resource".to_string());
        }
        "skill" => {
            out.insert("skills".to_string());
        }
        "skills" => {
            out.insert("skill".to_string());
        }
        _ => {}
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn matches(raw: &str, target: &str) -> MatchResult {
        match_affix(raw, &[target.to_string()], 0.78)
    }

    #[test]
    fn exact_and_case_insensitive_match() {
        assert!(matches("CORE SKILLS", "Core Skills").matched);
    }

    #[test]
    fn noisy_punctuation_normalizes() {
        assert_eq!(
            normalize_ocr_text("+2 Ranks: to Core-Skills!"),
            "+2 ranks to core skills"
        );
    }

    #[test]
    fn max_health_matches_diablo_life_wording() {
        let result = matches("Maximum Life", "Max Health");
        assert!(result.matched);
        assert_eq!(result.target.as_deref(), Some("Max Health"));
    }

    #[test]
    fn typo_still_matches() {
        assert!(matches("Moximum Life", "Max Health").matched);
    }

    #[test]
    fn numeric_zero_is_preserved() {
        assert_eq!(normalize_ocr_text("+20% damage"), "+20% damage");
    }
}
