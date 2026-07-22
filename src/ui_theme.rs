use eframe::egui::{self, Color32, Context, FontFamily, FontId, TextStyle};

pub mod text {
    pub const PAGE_TITLE: f32 = 22.0;
    pub const SECTION_TITLE: f32 = 16.0;
    pub const BODY: f32 = 16.0;
    pub const SUPPORTING: f32 = 14.0;
    pub const META: f32 = 12.0;
}

pub mod colors {
    use super::Color32;

    pub const CANVAS: Color32 = Color32::from_rgb(9, 11, 13);
    pub const SURFACE: Color32 = Color32::from_rgb(17, 20, 23);
    pub const ELEVATED_SURFACE: Color32 = Color32::from_rgb(24, 28, 32);
    pub const HOVERED_SURFACE: Color32 = Color32::from_rgb(34, 39, 44);
    pub const BORDER: Color32 = Color32::from_rgb(57, 64, 72);
    pub const PRIMARY_TEXT: Color32 = Color32::from_rgb(224, 228, 232);
    pub const SUPPORTING_TEXT: Color32 = Color32::from_rgb(181, 190, 198);
    pub const DIABLO_ORANGE: Color32 = Color32::from_rgb(244, 119, 32);
    pub const OBSERVE_BLUE: Color32 = Color32::from_rgb(102, 166, 214);
    pub const DECIDE_PURPLE: Color32 = Color32::from_rgb(166, 126, 210);
    pub const ACT_ORANGE: Color32 = Color32::from_rgb(225, 128, 64);
    pub const REPEAT_TEAL: Color32 = Color32::from_rgb(83, 175, 166);
    pub const SUCCESS: Color32 = Color32::from_rgb(76, 202, 118);
    pub const WARNING: Color32 = Color32::from_rgb(255, 158, 58);
    pub const DANGER: Color32 = Color32::from_rgb(239, 91, 76);
    pub const STATUS_IDLE: Color32 = Color32::from_rgb(130, 139, 148);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockCategory {
    Observe,
    Decide,
    Act,
    Repeat,
}

impl BlockCategory {
    #[cfg(test)]
    pub const ALL: [Self; 4] = [Self::Observe, Self::Decide, Self::Act, Self::Repeat];
}

pub struct CategoryStyle {
    pub label: &'static str,
    pub icon: &'static str,
    pub accent: Color32,
}

pub fn category_style(category: BlockCategory) -> CategoryStyle {
    match category {
        BlockCategory::Observe => CategoryStyle {
            label: "Observe",
            icon: "◉",
            accent: colors::OBSERVE_BLUE,
        },
        BlockCategory::Decide => CategoryStyle {
            label: "Decide",
            icon: "◇",
            accent: colors::DECIDE_PURPLE,
        },
        BlockCategory::Act => CategoryStyle {
            label: "Act",
            icon: "▶",
            accent: colors::ACT_ORANGE,
        },
        BlockCategory::Repeat => CategoryStyle {
            label: "Repeat",
            icon: "↻",
            accent: colors::REPEAT_TEAL,
        },
    }
}

pub fn build_style() -> egui::Style {
    let mut style = egui::Style::default();
    style.text_styles.insert(
        TextStyle::Heading,
        FontId::new(text::PAGE_TITLE, FontFamily::Proportional),
    );
    style.text_styles.insert(
        TextStyle::Name("SectionTitle".into()),
        FontId::new(text::SECTION_TITLE, FontFamily::Proportional),
    );
    style.text_styles.insert(
        TextStyle::Body,
        FontId::new(text::BODY, FontFamily::Proportional),
    );
    style.text_styles.insert(
        TextStyle::Button,
        FontId::new(text::BODY, FontFamily::Proportional),
    );
    style.text_styles.insert(
        TextStyle::Monospace,
        FontId::new(text::META, FontFamily::Monospace),
    );
    style.text_styles.insert(
        TextStyle::Small,
        FontId::new(text::META, FontFamily::Proportional),
    );
    style.spacing.button_padding = egui::vec2(12.0, 9.0);
    style.spacing.item_spacing = egui::vec2(10.0, 8.0);
    style.visuals = app_visuals();
    style
}

pub fn apply(ctx: &Context) {
    ctx.set_style(build_style());
}

fn app_visuals() -> egui::Visuals {
    let mut visuals = egui::Visuals::dark();
    visuals.panel_fill = colors::CANVAS;
    visuals.window_fill = colors::SURFACE;
    visuals.faint_bg_color = colors::SURFACE;
    visuals.extreme_bg_color = colors::CANVAS;
    visuals.code_bg_color = colors::ELEVATED_SURFACE;
    visuals.widgets.noninteractive.bg_fill = colors::SURFACE;
    visuals.widgets.noninteractive.fg_stroke.color = colors::SUPPORTING_TEXT;
    visuals.widgets.inactive.bg_fill = colors::ELEVATED_SURFACE;
    visuals.widgets.inactive.fg_stroke.color = colors::PRIMARY_TEXT;
    visuals.widgets.hovered.bg_fill = colors::HOVERED_SURFACE;
    visuals.widgets.hovered.fg_stroke.color = colors::PRIMARY_TEXT;
    visuals.widgets.active.bg_fill = colors::DIABLO_ORANGE;
    visuals.widgets.active.fg_stroke.color = Color32::BLACK;
    visuals.selection.bg_fill = colors::DIABLO_ORANGE;
    visuals.selection.stroke.color = Color32::WHITE;
    visuals.window_stroke.color = colors::BORDER;
    visuals
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shared_type_scale_never_drops_below_twelve_points() {
        let style = build_style();
        assert_eq!(style.text_styles[&egui::TextStyle::Heading].size, 22.0);
        assert_eq!(style.text_styles[&egui::TextStyle::Body].size, 16.0);
        assert_eq!(style.text_styles[&egui::TextStyle::Button].size, 16.0);
        assert_eq!(style.text_styles[&egui::TextStyle::Small].size, 12.0);
        assert!(style.text_styles.values().all(|font| font.size >= 12.0));
    }

    #[test]
    fn category_style_has_text_and_icon_not_just_color() {
        for category in BlockCategory::ALL {
            let style = category_style(category);
            assert!(!style.label.is_empty());
            assert!(!style.icon.is_empty());
        }
    }

    #[test]
    fn visuals_preserve_explicit_semantic_text_colors() {
        assert_eq!(app_visuals().override_text_color, None);
    }
}
