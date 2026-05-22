pub mod config;
pub mod enchant_loop;
pub mod matcher;
pub mod platform;
pub mod types;

pub use config::EnchantConfig;
pub use enchant_loop::{
    EnchantEvent, EnchantOutcome, EnchantRunner, InputController, OcrReader, RegionCapture,
    StopSignal,
};
pub use matcher::{MatchResult, match_affix, normalize_ocr_text};
pub use types::{Point, Rect, ScreenImage};
