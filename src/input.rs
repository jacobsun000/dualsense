//! Stateful conversion from Linux evdev events into controller events.
//!
//! This is the boundary that a future keyboard mapper should consume.  Raw evdev
//! values stay attached to each event for diagnostics, while analog values are
//! also exposed as normalized `f32`s.

use evdev::{AbsoluteAxisCode, Device, EventType, InputEvent, KeyCode};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Button {
    South,
    East,
    North,
    West,
    L1,
    R1,
    L2,
    R2,
    Create,
    Options,
    Ps,
    L3,
    R3,
    DpadUp,
    DpadDown,
    DpadLeft,
    DpadRight,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ButtonState {
    Down,
    Up,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stick {
    Left,
    Right,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StickAxis {
    X,
    Y,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trigger {
    Left,
    Right,
}

/// A semantic controller event generated from one raw evdev event.
///
/// `Button` events are edge events (`Down`/`Up`). Stick and trigger events are
/// value updates: a mapper can apply its own deadzone or threshold without
/// losing the underlying raw value.
#[allow(dead_code)] // Public event fields are consumed by the future keyboard mapper.
#[derive(Debug, Clone, Copy)]
pub struct ControllerEvent {
    pub kind: ControllerEventKind,
    pub raw: InputEvent,
}

#[allow(dead_code)] // Variants form the stable mapping API for future consumers.
#[derive(Debug, Clone, Copy)]
pub enum ControllerEventKind {
    Button {
        button: Button,
        state: ButtonState,
    },
    Stick {
        stick: Stick,
        axis: StickAxis,
        value: f32,
        raw_value: i32,
    },
    Trigger {
        trigger: Trigger,
        value: f32,
        raw_value: i32,
    },
}

#[derive(Debug, Clone, Copy)]
struct AxisRange {
    minimum: i32,
    maximum: i32,
}

impl Default for AxisRange {
    fn default() -> Self {
        Self {
            minimum: -32768,
            maximum: 32767,
        }
    }
}

impl AxisRange {
    fn normalize_signed(self, value: i32) -> f32 {
        let center = (self.minimum + self.maximum) as f32 / 2.0;
        let half_range = ((self.maximum - self.minimum) as f32 / 2.0).max(1.0);
        ((value as f32 - center) / half_range).clamp(-1.0, 1.0)
    }

    fn normalize_trigger(self, value: i32) -> f32 {
        let range = (self.maximum - self.minimum).max(1) as f32;
        ((value - self.minimum) as f32 / range).clamp(0.0, 1.0)
    }
}

/// Converts raw evdev input into stable, controller-specific events.
///
/// The decoder is stateful for D-pad hat axes so a transition such as left to
/// right becomes `DpadLeft Up` followed by `DpadRight Down`.
pub struct EventDecoder {
    axes: [AxisRange; 6],
    dpad_x: i32,
    dpad_y: i32,
}

impl EventDecoder {
    pub fn new(device: &Device) -> Self {
        let mut decoder = Self {
            axes: [AxisRange::default(); 6],
            dpad_x: 0,
            dpad_y: 0,
        };

        if let Ok(absinfo) = device.get_absinfo() {
            for (axis, info) in absinfo {
                let index = match axis {
                    AbsoluteAxisCode::ABS_X => Some(0),
                    AbsoluteAxisCode::ABS_Y => Some(1),
                    AbsoluteAxisCode::ABS_Z => Some(2),
                    AbsoluteAxisCode::ABS_RX => Some(3),
                    AbsoluteAxisCode::ABS_RY => Some(4),
                    AbsoluteAxisCode::ABS_RZ => Some(5),
                    _ => None,
                };
                if let Some(index) = index {
                    decoder.axes[index] = AxisRange {
                        minimum: info.minimum(),
                        maximum: info.maximum(),
                    };
                }
            }
        }
        decoder
    }

    /// Decode one raw event. Most raw events produce zero or one event; a
    /// direct D-pad direction change can produce two edge events.
    pub fn decode(&mut self, event: InputEvent) -> Vec<ControllerEvent> {
        match event.event_type() {
            EventType::KEY => self
                .button_from_key(KeyCode::new(event.code()))
                .map(|button| {
                    vec![ControllerEvent {
                        kind: ControllerEventKind::Button {
                            button,
                            state: if event.value() == 0 {
                                ButtonState::Up
                            } else {
                                ButtonState::Down
                            },
                        },
                        raw: event,
                    }]
                })
                .unwrap_or_default(),
            EventType::ABSOLUTE => self.decode_absolute(event),
            _ => Vec::new(),
        }
    }

    fn decode_absolute(&mut self, event: InputEvent) -> Vec<ControllerEvent> {
        let axis = AbsoluteAxisCode(event.code());
        let value = event.value();
        match axis {
            AbsoluteAxisCode::ABS_X => vec![self.stick(event, Stick::Left, StickAxis::X, 0)],
            AbsoluteAxisCode::ABS_Y => vec![self.stick(event, Stick::Left, StickAxis::Y, 1)],
            AbsoluteAxisCode::ABS_RX => vec![self.stick(event, Stick::Right, StickAxis::X, 3)],
            AbsoluteAxisCode::ABS_RY => vec![self.stick(event, Stick::Right, StickAxis::Y, 4)],
            AbsoluteAxisCode::ABS_Z => vec![self.trigger(event, Trigger::Left, 2)],
            AbsoluteAxisCode::ABS_RZ => vec![self.trigger(event, Trigger::Right, 5)],
            AbsoluteAxisCode::ABS_HAT0X => self.decode_hat(event, true, value),
            AbsoluteAxisCode::ABS_HAT0Y => self.decode_hat(event, false, value),
            _ => Vec::new(),
        }
    }

    fn stick(
        &self,
        event: InputEvent,
        stick: Stick,
        axis: StickAxis,
        range: usize,
    ) -> ControllerEvent {
        ControllerEvent {
            kind: ControllerEventKind::Stick {
                stick,
                axis,
                value: self.axes[range].normalize_signed(event.value()),
                raw_value: event.value(),
            },
            raw: event,
        }
    }

    fn trigger(&self, event: InputEvent, trigger: Trigger, range: usize) -> ControllerEvent {
        ControllerEvent {
            kind: ControllerEventKind::Trigger {
                trigger,
                value: self.axes[range].normalize_trigger(event.value()),
                raw_value: event.value(),
            },
            raw: event,
        }
    }

    fn decode_hat(
        &mut self,
        event: InputEvent,
        horizontal: bool,
        value: i32,
    ) -> Vec<ControllerEvent> {
        let previous = if horizontal { self.dpad_x } else { self.dpad_y };
        if horizontal {
            self.dpad_x = value;
        } else {
            self.dpad_y = value;
        }
        if previous == value {
            return Vec::new();
        }

        let mut events = Vec::with_capacity(2);
        if let Some(button) = hat_button(horizontal, previous) {
            events.push(button_event(event, button, ButtonState::Up));
        }
        if let Some(button) = hat_button(horizontal, value) {
            events.push(button_event(event, button, ButtonState::Down));
        }
        events
    }

    fn button_from_key(&self, code: KeyCode) -> Option<Button> {
        Some(match code {
            KeyCode::BTN_SOUTH => Button::South,
            KeyCode::BTN_EAST => Button::East,
            KeyCode::BTN_NORTH => Button::North,
            KeyCode::BTN_WEST => Button::West,
            KeyCode::BTN_TL => Button::L1,
            KeyCode::BTN_TR => Button::R1,
            KeyCode::BTN_TL2 => Button::L2,
            KeyCode::BTN_TR2 => Button::R2,
            KeyCode::BTN_SELECT => Button::Create,
            KeyCode::BTN_START => Button::Options,
            KeyCode::BTN_MODE => Button::Ps,
            KeyCode::BTN_THUMBL => Button::L3,
            KeyCode::BTN_THUMBR => Button::R3,
            KeyCode::BTN_DPAD_UP | KeyCode::KEY_UP => Button::DpadUp,
            KeyCode::BTN_DPAD_DOWN | KeyCode::KEY_DOWN => Button::DpadDown,
            KeyCode::BTN_DPAD_LEFT | KeyCode::KEY_LEFT => Button::DpadLeft,
            KeyCode::BTN_DPAD_RIGHT | KeyCode::KEY_RIGHT => Button::DpadRight,
            _ => return None,
        })
    }
}

fn hat_button(horizontal: bool, value: i32) -> Option<Button> {
    match (horizontal, value) {
        (true, -1) => Some(Button::DpadLeft),
        (true, 1) => Some(Button::DpadRight),
        (false, -1) => Some(Button::DpadUp),
        (false, 1) => Some(Button::DpadDown),
        _ => None,
    }
}

fn button_event(event: InputEvent, button: Button, state: ButtonState) -> ControllerEvent {
    ControllerEvent {
        kind: ControllerEventKind::Button { button, state },
        raw: event,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hat_transition_emits_release_then_press() {
        let mut decoder = EventDecoder {
            axes: [AxisRange::default(); 6],
            dpad_x: -1,
            dpad_y: 0,
        };
        let raw = InputEvent::new(
            evdev::EventType::ABSOLUTE.0,
            AbsoluteAxisCode::ABS_HAT0X.0,
            1,
        );
        let events = decoder.decode(raw);
        assert!(matches!(
            events[0].kind,
            ControllerEventKind::Button {
                button: Button::DpadLeft,
                state: ButtonState::Up
            }
        ));
        assert!(matches!(
            events[1].kind,
            ControllerEventKind::Button {
                button: Button::DpadRight,
                state: ButtonState::Down
            }
        ));
    }

    #[test]
    fn stick_value_is_normalized() {
        let mut decoder = EventDecoder {
            axes: [AxisRange {
                minimum: -100,
                maximum: 100,
            }; 6],
            dpad_x: 0,
            dpad_y: 0,
        };
        let raw = InputEvent::new(evdev::EventType::ABSOLUTE.0, AbsoluteAxisCode::ABS_X.0, 50);
        let events = decoder.decode(raw);
        match events[0].kind {
            ControllerEventKind::Stick { value, .. } => assert!((value - 0.5).abs() < 0.01),
            _ => panic!("expected stick event"),
        }
    }
}
