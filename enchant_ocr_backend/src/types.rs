use image::RgbaImage;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Point {
    pub x: i32,
    pub y: i32,
}

impl Point {
    pub const fn new(x: i32, y: i32) -> Self {
        Self { x, y }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

impl Rect {
    pub const fn new(x: i32, y: i32, width: u32, height: u32) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    pub fn contains_point(self, point: Point) -> bool {
        point.x >= self.x
            && point.y >= self.y
            && point.x < self.x + self.width as i32
            && point.y < self.y + self.height as i32
    }

    pub fn contains_rect(self, rect: Rect) -> bool {
        let self_right = self.x + self.width as i32;
        let self_bottom = self.y + self.height as i32;
        let rect_right = rect.x + rect.width as i32;
        let rect_bottom = rect.y + rect.height as i32;

        rect.x >= self.x
            && rect.y >= self.y
            && rect_right <= self_right
            && rect_bottom <= self_bottom
    }

    pub fn point_from_ratio(self, ratio: PointRatio) -> Point {
        Point {
            x: self.x + (ratio.x * self.width as f32).round() as i32,
            y: self.y + (ratio.y * self.height as f32).round() as i32,
        }
    }

    pub fn rect_from_ratio(self, ratio: RectRatio) -> Rect {
        Rect {
            x: self.x + (ratio.x * self.width as f32).round() as i32,
            y: self.y + (ratio.y * self.height as f32).round() as i32,
            width: ((ratio.width * self.width as f32).round() as u32).max(1),
            height: ((ratio.height * self.height as f32).round() as u32).max(1),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct PointRatio {
    pub x: f32,
    pub y: f32,
}

impl PointRatio {
    pub fn from_point(window: Rect, point: Point) -> Self {
        Self {
            x: ((point.x - window.x) as f32 / window.width as f32).clamp(0.0, 1.0),
            y: ((point.y - window.y) as f32 / window.height as f32).clamp(0.0, 1.0),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct RectRatio {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

impl RectRatio {
    pub fn from_rect(window: Rect, rect: Rect) -> Self {
        Self {
            x: ((rect.x - window.x) as f32 / window.width as f32).clamp(0.0, 1.0),
            y: ((rect.y - window.y) as f32 / window.height as f32).clamp(0.0, 1.0),
            width: (rect.width as f32 / window.width as f32).clamp(0.001, 1.0),
            height: (rect.height as f32 / window.height as f32).clamp(0.001, 1.0),
        }
    }

    pub fn from_rect_relative(window: Rect, rect: Rect) -> Self {
        Self {
            x: (rect.x - window.x) as f32 / window.width as f32,
            y: (rect.y - window.y) as f32 / window.height as f32,
            width: (rect.width as f32 / window.width as f32).max(0.001),
            height: (rect.height as f32 / window.height as f32).max(0.001),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ScreenImage {
    pub rect: Rect,
    pub rgba: RgbaImage,
}

impl ScreenImage {
    pub fn new(rect: Rect, rgba: RgbaImage) -> Self {
        Self { rect, rgba }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ratios_round_trip_inside_window() {
        let window = Rect::new(100, 200, 800, 600);
        let rect = Rect::new(300, 350, 200, 120);
        let ratio = RectRatio::from_rect(window, rect);

        assert_eq!(window.rect_from_ratio(ratio), rect);
    }

    #[test]
    fn ratios_round_trip_with_negative_monitor_origin() {
        let window = Rect::new(-1600, 120, 1280, 720);
        let rect = Rect::new(-1320, 390, 420, 86);
        let ratio = RectRatio::from_rect(window, rect);

        assert_eq!(window.rect_from_ratio(ratio), rect);
    }

    #[test]
    fn contains_rect_rejects_partial_outside_selection() {
        let window = Rect::new(100, 100, 500, 400);

        assert!(window.contains_rect(Rect::new(150, 180, 120, 60)));
        assert!(!window.contains_rect(Rect::new(90, 180, 120, 60)));
        assert!(!window.contains_rect(Rect::new(520, 180, 120, 60)));
    }

    #[test]
    fn relative_ratio_preserves_button_region_outside_window() {
        let window = Rect::new(100, 100, 500, 400);
        let rect = Rect::new(650, 460, 120, 40);
        let ratio = RectRatio::from_rect_relative(window, rect);

        assert_eq!(window.rect_from_ratio(ratio), rect);
    }
}
