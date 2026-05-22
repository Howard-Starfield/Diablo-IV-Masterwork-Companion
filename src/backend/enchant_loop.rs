use std::time::Instant;
use std::{thread, time::Duration};

use anyhow::Result;
use serde::{Deserialize, Serialize};

use super::{
    config::{EnchantConfig, MouseMovementProfile},
    matcher::{MatchResult, match_affix},
    types::{Point, Rect, ScreenImage},
};

pub trait RegionCapture {
    fn capture_region(&self, rect: Rect) -> Result<ScreenImage>;
}

pub trait OcrReader {
    fn read_text(&self, image: &ScreenImage) -> Result<String>;
}

pub trait StopSignal {
    fn should_stop(&self) -> bool;
}

pub trait InputController {
    fn click(&self, point: Point) -> Result<()>;

    fn click_with_movement(
        &self,
        point: Point,
        _movement: Option<&MouseMovementProfile>,
        _stop: Option<&dyn StopSignal>,
    ) -> Result<()> {
        self.click(point)
    }
}

#[derive(Debug, Default)]
#[cfg(test)]
pub struct NeverStop;

#[cfg(test)]
impl StopSignal for NeverStop {
    fn should_stop(&self) -> bool {
        false
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum EnchantEvent {
    AttemptStarted {
        attempt: u32,
    },
    ClickEnchant {
        point: Point,
    },
    OcrReadStarted {
        rect: Rect,
    },
    OcrReadFinished {
        result: MatchResult,
        ocr_time_ms: u64,
    },
    TargetFound {
        attempt: u32,
        result: MatchResult,
    },
    ClickReplace {
        point: Point,
    },
    ClickClose {
        point: Point,
    },
    AttemptFinished {
        attempt: u32,
    },
    MaxAttemptsReached {
        attempts: u32,
    },
    Stopped,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum EnchantOutcome {
    Found { attempts: u32, result: MatchResult },
    MaxAttempts { attempts: u32 },
    Stopped { attempts: u32 },
}

enum AttemptResult {
    Continue,
    Found(MatchResult),
    Stopped,
}

pub struct EnchantRunner<C, O, I, S> {
    config: EnchantConfig,
    capture: C,
    ocr: O,
    input: I,
    stop: S,
}

impl<C, O, I, S> EnchantRunner<C, O, I, S>
where
    C: RegionCapture,
    O: OcrReader,
    I: InputController,
    S: StopSignal,
{
    pub fn new(config: EnchantConfig, capture: C, ocr: O, input: I, stop: S) -> Self {
        Self {
            config,
            capture,
            ocr,
            input,
            stop,
        }
    }

    pub fn run<F>(&self, mut emit: F) -> Result<EnchantOutcome>
    where
        F: FnMut(EnchantEvent),
    {
        let mut attempts = 0;
        while self.can_start_attempt(attempts) {
            if self.stop.should_stop() {
                emit(EnchantEvent::Stopped);
                return Ok(EnchantOutcome::Stopped { attempts });
            }

            attempts += 1;
            emit(EnchantEvent::AttemptStarted { attempt: attempts });

            match self.run_attempt(attempts, &mut emit)? {
                AttemptResult::Continue => {}
                AttemptResult::Found(found) => {
                    return Ok(EnchantOutcome::Found {
                        attempts,
                        result: found,
                    });
                }
                AttemptResult::Stopped => {
                    emit(EnchantEvent::Stopped);
                    return Ok(EnchantOutcome::Stopped { attempts });
                }
            }
        }

        emit(EnchantEvent::MaxAttemptsReached { attempts });
        Ok(EnchantOutcome::MaxAttempts { attempts })
    }

    fn can_start_attempt(&self, attempts: u32) -> bool {
        self.config.max_attempts == 0 || attempts < self.config.max_attempts
    }

    fn run_attempt<F>(&self, attempt: u32, emit: &mut F) -> Result<AttemptResult>
    where
        F: FnMut(EnchantEvent),
    {
        let window = self.config.enchant_window;
        let enchant_point = window.point_from_ratio(self.config.enchant_button);
        let ocr_rect = window.rect_from_ratio(self.config.ocr_region);
        let replace_point = window.point_from_ratio(self.config.replace_button);
        let close_point = window.point_from_ratio(self.config.close_button);

        emit(EnchantEvent::ClickEnchant {
            point: enchant_point,
        });
        if self.click(enchant_point)? || self.wait_or_stop(self.config.wait_after_enchant_ms) {
            return Ok(AttemptResult::Stopped);
        }

        emit(EnchantEvent::OcrReadStarted { rect: ocr_rect });
        let ocr_started = Instant::now();
        let image = self.capture.capture_region(ocr_rect)?;
        let raw_text = self.ocr.read_text(&image)?;
        if self.stop.should_stop() {
            return Ok(AttemptResult::Stopped);
        }
        let match_result =
            match_affix(&raw_text, &self.config.targets, self.config.fuzzy_threshold);
        emit(EnchantEvent::OcrReadFinished {
            result: match_result.clone(),
            ocr_time_ms: ocr_started.elapsed().as_millis() as u64,
        });

        if match_result.matched {
            emit(EnchantEvent::TargetFound {
                attempt,
                result: match_result.clone(),
            });
            return Ok(AttemptResult::Found(match_result));
        }

        emit(EnchantEvent::ClickReplace {
            point: replace_point,
        });
        if self.click(replace_point)? || self.wait_or_stop(self.config.wait_after_replace_ms) {
            return Ok(AttemptResult::Stopped);
        }

        emit(EnchantEvent::ClickClose { point: close_point });
        if self.click(close_point)? || self.wait_or_stop(self.config.wait_after_close_ms) {
            return Ok(AttemptResult::Stopped);
        }

        emit(EnchantEvent::AttemptFinished { attempt });
        Ok(AttemptResult::Continue)
    }

    fn click(&self, point: Point) -> Result<bool> {
        if self.stop.should_stop() {
            return Ok(true);
        }
        self.input.click_with_movement(
            point,
            self.config.mouse_movement.as_ref(),
            Some(&self.stop),
        )?;
        Ok(self.stop.should_stop())
    }

    fn wait_or_stop(&self, millis: u64) -> bool {
        let mut remaining = millis;
        while remaining > 0 {
            if self.stop.should_stop() {
                return true;
            }
            let chunk = remaining.min(50);
            thread::sleep(Duration::from_millis(chunk));
            remaining -= chunk;
        }
        self.stop.should_stop()
    }
}

#[cfg(test)]
mod tests {
    use std::{
        cell::{Cell, RefCell},
        rc::Rc,
    };

    use anyhow::Result;
    use image::RgbaImage;

    use super::*;
    use super::super::{
        config::{MouseMovementSample, MouseMovementStep},
        types::{PointRatio, RectRatio},
    };

    struct FakeCapture;
    impl RegionCapture for FakeCapture {
        fn capture_region(&self, rect: Rect) -> Result<ScreenImage> {
            Ok(ScreenImage::new(RgbaImage::new(rect.width, rect.height)))
        }
    }

    struct FakeOcr(&'static str);
    impl OcrReader for FakeOcr {
        fn read_text(&self, _image: &ScreenImage) -> Result<String> {
            Ok(self.0.to_string())
        }
    }

    #[derive(Clone)]
    struct FakeInput(Rc<RefCell<Vec<Point>>>);
    impl InputController for FakeInput {
        fn click(&self, point: Point) -> Result<()> {
            self.0.borrow_mut().push(point);
            Ok(())
        }
    }

    #[derive(Clone)]
    struct MovementInput {
        clicked: Rc<RefCell<Vec<Point>>>,
        movement_clicks: Rc<RefCell<usize>>,
    }

    impl InputController for MovementInput {
        fn click(&self, point: Point) -> Result<()> {
            self.clicked.borrow_mut().push(point);
            Ok(())
        }

        fn click_with_movement(
            &self,
            point: Point,
            movement: Option<&MouseMovementProfile>,
            _stop: Option<&dyn StopSignal>,
        ) -> Result<()> {
            if movement.is_some() {
                *self.movement_clicks.borrow_mut() += 1;
            }
            self.click(point)
        }
    }

    #[derive(Clone)]
    struct StoppingInput {
        clicked: Rc<RefCell<Vec<Point>>>,
        stop_requested: Rc<Cell<bool>>,
    }

    impl InputController for StoppingInput {
        fn click(&self, point: Point) -> Result<()> {
            self.clicked.borrow_mut().push(point);
            self.stop_requested.set(true);
            Ok(())
        }
    }

    #[derive(Clone)]
    struct FakeStop(Rc<Cell<bool>>);

    impl StopSignal for FakeStop {
        fn should_stop(&self) -> bool {
            self.0.get()
        }
    }

    fn config(target: &str) -> EnchantConfig {
        EnchantConfig {
            targets: vec![target.to_string()],
            fuzzy_threshold: 0.78,
            max_attempts: 2,
            enchant_window: Rect::new(0, 0, 100, 100),
            ocr_region: RectRatio {
                x: 0.2,
                y: 0.2,
                width: 0.5,
                height: 0.2,
            },
            enchant_button: PointRatio { x: 0.1, y: 0.1 },
            replace_button: PointRatio { x: 0.7, y: 0.7 },
            close_button: PointRatio { x: 0.9, y: 0.1 },
            mouse_movement: None,
            wait_after_enchant_ms: 0,
            wait_after_replace_ms: 0,
            wait_after_close_ms: 0,
        }
    }

    #[test]
    fn found_result_does_not_replace_or_close() {
        let clicked = Rc::new(RefCell::new(Vec::new()));
        let runner = EnchantRunner::new(
            config("Max Health"),
            FakeCapture,
            FakeOcr("Maximum Life"),
            FakeInput(clicked.clone()),
            NeverStop,
        );

        let outcome = runner.run(|_| {}).unwrap();

        assert!(matches!(outcome, EnchantOutcome::Found { attempts: 1, .. }));
        assert_eq!(*clicked.borrow(), vec![Point::new(10, 10)]);
    }

    #[test]
    fn zero_max_attempts_allows_infinite_mode() {
        let clicked = Rc::new(RefCell::new(Vec::new()));
        let mut config = config("Max Health");
        config.max_attempts = 0;
        let runner = EnchantRunner::new(
            config,
            FakeCapture,
            FakeOcr("Maximum Life"),
            FakeInput(clicked.clone()),
            NeverStop,
        );

        let outcome = runner.run(|_| {}).unwrap();

        assert!(matches!(outcome, EnchantOutcome::Found { attempts: 1, .. }));
        assert_eq!(*clicked.borrow(), vec![Point::new(10, 10)]);
    }

    #[test]
    fn miss_clicks_replace_then_close_after_ocr() {
        let clicked = Rc::new(RefCell::new(Vec::new()));
        let runner = EnchantRunner::new(
            config("Max Health"),
            FakeCapture,
            FakeOcr("Thorns"),
            FakeInput(clicked.clone()),
            NeverStop,
        );

        let mut events = Vec::new();
        let outcome = runner.run(|event| events.push(event)).unwrap();

        assert!(matches!(
            outcome,
            EnchantOutcome::MaxAttempts { attempts: 2 }
        ));
        assert_eq!(
            *clicked.borrow(),
            vec![
                Point::new(10, 10),
                Point::new(70, 70),
                Point::new(90, 10),
                Point::new(10, 10),
                Point::new(70, 70),
                Point::new(90, 10),
            ]
        );
        assert!(matches!(
            events[0],
            EnchantEvent::AttemptStarted { attempt: 1 }
        ));
        assert!(matches!(events[1], EnchantEvent::ClickEnchant { .. }));
        assert!(matches!(events[2], EnchantEvent::OcrReadStarted { .. }));
        assert!(matches!(events[4], EnchantEvent::ClickReplace { .. }));
        assert!(matches!(events[5], EnchantEvent::ClickClose { .. }));
    }

    #[test]
    fn configured_mouse_movement_is_applied_to_each_click() {
        let clicked = Rc::new(RefCell::new(Vec::new()));
        let movement_clicks = Rc::new(RefCell::new(0));
        let mut config = config("Max Health");
        config.max_attempts = 1;
        config.mouse_movement = Some(MouseMovementProfile {
            duration_ms: 120,
            distance_px: 100.0,
            model: None,
            movement_steps: vec![MouseMovementStep {
                delay_ms: 120,
                progress_delta: 1.0,
                lateral_delta: 0.0,
            }],
            samples: vec![
                MouseMovementSample {
                    at_ms: 0,
                    progress: 0.0,
                    lateral: 0.0,
                },
                MouseMovementSample {
                    at_ms: 120,
                    progress: 1.0,
                    lateral: 0.0,
                },
            ],
        });
        let runner = EnchantRunner::new(
            config,
            FakeCapture,
            FakeOcr("Thorns"),
            MovementInput {
                clicked: clicked.clone(),
                movement_clicks: movement_clicks.clone(),
            },
            NeverStop,
        );

        let outcome = runner.run(|_| {}).unwrap();

        assert!(matches!(outcome, EnchantOutcome::MaxAttempts { attempts: 1 }));
        assert_eq!(
            *clicked.borrow(),
            vec![Point::new(10, 10), Point::new(70, 70), Point::new(90, 10)]
        );
        assert_eq!(*movement_clicks.borrow(), 3);
    }

    #[test]
    fn stop_signal_after_click_stops_before_replace_or_close() {
        let clicked = Rc::new(RefCell::new(Vec::new()));
        let stop_requested = Rc::new(Cell::new(false));
        let runner = EnchantRunner::new(
            config("Max Health"),
            FakeCapture,
            FakeOcr("Thorns"),
            StoppingInput {
                clicked: clicked.clone(),
                stop_requested: stop_requested.clone(),
            },
            FakeStop(stop_requested),
        );

        let outcome = runner.run(|_| {}).unwrap();

        assert!(matches!(outcome, EnchantOutcome::Stopped { attempts: 1 }));
        assert_eq!(*clicked.borrow(), vec![Point::new(10, 10)]);
    }
}
