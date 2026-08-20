//! DualSense-to-keyboard and mouse mapping.
//!
//! The mapper consumes semantic events from [`crate::input`] and emits Linux
//! uinput events. It is intentionally independent of the TUI, so the same
//! mapping is active in both direct and TUI modes.

use crate::input::{Button, ButtonState, ControllerEvent, ControllerEventKind, Stick, StickAxis};
use evdev::uinput::VirtualDevice;
use evdev::{AttributeSet, EventType, InputEvent, KeyCode, RelativeAxisCode};
use std::{
    collections::{HashMap, HashSet},
    io,
    time::Instant,
};

const STICK_DEADZONE: f32 = 0.35;
const MAX_SCROLL_PER_SECOND: f32 = 24.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct KeyCombo {
    modifier: Option<KeyCode>,
    key: KeyCode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Source {
    Button(Button),
    Stick { stick: Stick, direction: Direction },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Direction {
    Left,
    Right,
    Up,
    Down,
}

pub struct KeyboardMapper {
    keyboard: VirtualDevice,
    mouse: VirtualDevice,
    l1_down: bool,
    r1_down: bool,
    left_x: f32,
    left_y: f32,
    active_sources: HashMap<Source, KeyCombo>,
    active_combos: HashMap<KeyCombo, usize>,
    sequence_sources: HashSet<Source>,
    right_x: f32,
    right_y: f32,
    scroll_x_remainder: f32,
    scroll_y_remainder: f32,
    last_scroll: Instant,
}

impl KeyboardMapper {
    pub fn new() -> io::Result<Self> {
        let keyboard_keys: AttributeSet<KeyCode> = [
            // The mapper's logical targets are written in QWERTY terms.
            // These are the physical keycodes that produce them under Colemak.
            KeyCode::KEY_F,
            KeyCode::KEY_I,
            KeyCode::KEY_J,
            KeyCode::KEY_K,
            KeyCode::KEY_L,
            KeyCode::KEY_R,
            KeyCode::KEY_T,
            KeyCode::KEY_W,
            KeyCode::KEY_X,
            KeyCode::KEY_ENTER,
            KeyCode::KEY_P,
            KeyCode::KEY_SLASH,
            KeyCode::KEY_LEFT,
            KeyCode::KEY_RIGHT,
            KeyCode::KEY_UP,
            KeyCode::KEY_DOWN,
            KeyCode::KEY_LEFTCTRL,
            KeyCode::KEY_LEFTALT,
            KeyCode::KEY_LEFTMETA,
        ]
        .into_iter()
        .collect();
        let keyboard = VirtualDevice::builder()?
            .name(b"DualSense keyboard mapper")
            .with_keys(&keyboard_keys)?
            .build()?;

        let mouse_axes: AttributeSet<RelativeAxisCode> =
            [RelativeAxisCode::REL_HWHEEL, RelativeAxisCode::REL_WHEEL]
                .into_iter()
                .collect();
        let mouse = VirtualDevice::builder()?
            .name(b"DualSense mouse mapper")
            .with_relative_axes(&mouse_axes)?
            .build()?;

        Ok(Self {
            keyboard,
            mouse,
            l1_down: false,
            r1_down: false,
            left_x: 0.0,
            left_y: 0.0,
            active_sources: HashMap::new(),
            active_combos: HashMap::new(),
            sequence_sources: HashSet::new(),
            right_x: 0.0,
            right_y: 0.0,
            scroll_x_remainder: 0.0,
            scroll_y_remainder: 0.0,
            last_scroll: Instant::now(),
        })
    }

    pub fn tick(&mut self) -> io::Result<()> {
        self.emit_scroll()
    }

    pub fn handle(&mut self, event: ControllerEvent) -> io::Result<()> {
        match event.kind {
            ControllerEventKind::Button { button, state } => self.handle_button(button, state),
            ControllerEventKind::Stick {
                stick, axis, value, ..
            } => self.handle_stick(stick, axis, value),
            ControllerEventKind::Trigger { .. } => Ok(()),
        }
    }

    fn handle_button(&mut self, button: Button, state: ButtonState) -> io::Result<()> {
        let source = Source::Button(button);
        if button == Button::DpadUp {
            if state == ButtonState::Down && self.r1_down {
                if self.sequence_sources.insert(source) {
                    self.emit_sequence(&[KeyCode::KEY_P, KeyCode::KEY_I, KeyCode::KEY_ENTER])?;
                }
                return Ok(());
            }
            if state == ButtonState::Up && self.sequence_sources.remove(&source) {
                return Ok(());
            }
        }

        match button {
            Button::L1 => {
                self.l1_down = state == ButtonState::Down;
                return Ok(());
            }
            Button::R1 => {
                self.r1_down = state == ButtonState::Down;
                return Ok(());
            }
            _ => {}
        }

        let source = Source::Button(button);
        let combo = self.button_combo(button);
        self.set_source(source, combo, state == ButtonState::Down)
    }

    fn handle_stick(&mut self, stick: Stick, axis: StickAxis, value: f32) -> io::Result<()> {
        if stick == Stick::Right {
            match axis {
                StickAxis::X => self.right_x = value,
                StickAxis::Y => self.right_y = value,
            }
            self.emit_scroll()?;
            return Ok(());
        }

        match axis {
            StickAxis::X => self.left_x = value,
            StickAxis::Y => self.left_y = value,
        }
        for (direction, active) in [
            (Direction::Left, self.left_x < -STICK_DEADZONE),
            (Direction::Right, self.left_x > STICK_DEADZONE),
            (Direction::Up, self.left_y < -STICK_DEADZONE),
            (Direction::Down, self.left_y > STICK_DEADZONE),
        ] {
            let source = Source::Stick { stick, direction };
            let combo = self.stick_combo(direction);
            self.set_source(source, combo, active)?;
        }
        Ok(())
    }

    fn button_combo(&self, button: Button) -> Option<KeyCombo> {
        let (direction, combo) = match button {
            // Right-side face buttons, arranged as a D-pad:
            //     North
            // West       East
            //     South
            Button::North => (
                Direction::Up,
                Some(key_combo(Some(KeyCode::KEY_LEFTCTRL), KeyCode::KEY_U)),
            ),
            Button::South => (
                Direction::Down,
                Some(key_combo(Some(KeyCode::KEY_LEFTCTRL), KeyCode::KEY_E)),
            ),
            Button::West => (Direction::Left, None),
            Button::East => (Direction::Right, None),
            Button::DpadUp => (
                Direction::Up,
                Some(key_combo(Some(KeyCode::KEY_LEFTALT), KeyCode::KEY_UP)),
            ),
            Button::DpadDown => (
                Direction::Down,
                Some(key_combo(Some(KeyCode::KEY_LEFTALT), KeyCode::KEY_DOWN)),
            ),
            Button::DpadLeft => (
                Direction::Left,
                Some(key_combo(Some(KeyCode::KEY_LEFTALT), KeyCode::KEY_LEFT)),
            ),
            Button::DpadRight => (
                Direction::Right,
                Some(key_combo(Some(KeyCode::KEY_LEFTALT), KeyCode::KEY_RIGHT)),
            ),
            _ => return None,
        };

        if matches!(
            button,
            Button::North | Button::South | Button::West | Button::East
        ) && self.l1_down
        {
            return Some(match direction {
                Direction::Left => key_combo(None, KeyCode::KEY_N),
                Direction::Right => key_combo(None, KeyCode::KEY_I),
                Direction::Up => key_combo(None, KeyCode::KEY_U),
                Direction::Down => key_combo(None, KeyCode::KEY_E),
            });
        }

        if matches!(
            button,
            Button::DpadLeft | Button::DpadRight | Button::DpadDown
        ) && self.r1_down
        {
            return Some(match direction {
                Direction::Left => key_combo(Some(KeyCode::KEY_LEFTALT), KeyCode::KEY_G),
                Direction::Right => key_combo(Some(KeyCode::KEY_LEFTCTRL), KeyCode::KEY_T),
                Direction::Down => key_combo(Some(KeyCode::KEY_LEFTCTRL), KeyCode::KEY_W),
                Direction::Up => combo.expect("D-pad up has a default mapping"),
            });
        }
        combo
    }

    fn stick_combo(&self, direction: Direction) -> Option<KeyCombo> {
        match direction {
            Direction::Left if self.r1_down => {
                Some(key_combo(Some(KeyCode::KEY_LEFTMETA), KeyCode::KEY_LEFT))
            }
            Direction::Right if self.r1_down => {
                Some(key_combo(Some(KeyCode::KEY_LEFTMETA), KeyCode::KEY_RIGHT))
            }
            Direction::Left => Some(key_combo(Some(KeyCode::KEY_LEFTMETA), KeyCode::KEY_N)),
            Direction::Right => Some(key_combo(Some(KeyCode::KEY_LEFTMETA), KeyCode::KEY_I)),
            Direction::Up => Some(key_combo(Some(KeyCode::KEY_LEFTMETA), KeyCode::KEY_U)),
            Direction::Down => Some(key_combo(Some(KeyCode::KEY_LEFTMETA), KeyCode::KEY_E)),
        }
    }

    fn set_source(
        &mut self,
        source: Source,
        combo: Option<KeyCombo>,
        active: bool,
    ) -> io::Result<()> {
        if active {
            let Some(combo) = combo else { return Ok(()) };
            if self.active_sources.contains_key(&source) {
                return Ok(());
            }
            let first_source = self.active_combos.get(&combo).copied().unwrap_or(0) == 0;
            if first_source {
                self.emit_combo(combo, true)?;
            }
            *self.active_combos.entry(combo).or_insert(0) += 1;
            self.active_sources.insert(source, combo);
        } else if let Some(combo) = self.active_sources.remove(&source) {
            let count = self
                .active_combos
                .get_mut(&combo)
                .expect("active source must have an active combo");
            *count -= 1;
            if *count == 0 {
                self.active_combos.remove(&combo);
                self.emit_combo(combo, false)?;
            }
        }
        Ok(())
    }

    fn emit_sequence(&mut self, keys: &[KeyCode]) -> io::Result<()> {
        for &key in keys {
            let combo = key_combo(None, key);
            self.emit_combo(combo, true)?;
            self.emit_combo(combo, false)?;
        }
        Ok(())
    }

    fn emit_combo(&mut self, combo: KeyCombo, down: bool) -> io::Result<()> {
        let mut events = Vec::with_capacity(2);
        if down {
            if let Some(modifier) = combo.modifier {
                events.push(key_event(modifier, true));
            }
            events.push(key_event(combo.key, true));
        } else {
            events.push(key_event(combo.key, false));
            if let Some(modifier) = combo.modifier {
                events.push(key_event(modifier, false));
            }
        }
        self.keyboard.emit(&events)
    }

    fn emit_scroll(&mut self) -> io::Result<()> {
        let now = Instant::now();
        let delta = now.duration_since(self.last_scroll).as_secs_f32().min(0.1);
        self.last_scroll = now;
        self.scroll_x_remainder += scroll_speed(self.right_x) * delta;
        self.scroll_y_remainder += scroll_speed(self.right_y) * delta;

        let horizontal = self.scroll_x_remainder.trunc() as i32;
        let vertical = self.scroll_y_remainder.trunc() as i32;
        self.scroll_x_remainder -= horizontal as f32;
        self.scroll_y_remainder -= vertical as f32;
        if horizontal == 0 && vertical == 0 {
            return Ok(());
        }

        let mut events = Vec::with_capacity(2);
        if horizontal != 0 {
            events.push(InputEvent::new(
                EventType::RELATIVE.0,
                RelativeAxisCode::REL_HWHEEL.0,
                horizontal,
            ));
        }
        if vertical != 0 {
            // Linux uses positive REL_WHEEL for scrolling up.
            events.push(InputEvent::new(
                EventType::RELATIVE.0,
                RelativeAxisCode::REL_WHEEL.0,
                -vertical,
            ));
        }
        self.mouse.emit(&events)
    }
}

impl Drop for KeyboardMapper {
    fn drop(&mut self) {
        let combos: Vec<_> = self.active_combos.keys().copied().collect();
        for combo in combos {
            let _ = self.emit_combo(combo, false);
        }
        self.active_sources.clear();
        self.active_combos.clear();
    }
}

fn key_combo(modifier: Option<KeyCode>, key: KeyCode) -> KeyCombo {
    KeyCombo {
        modifier,
        key: colemak_physical_key(key),
    }
}

/// Translate a logical QWERTY key to the physical Linux keycode that produces
/// that same character with a Colemak layout. Modifiers, arrows, and slash are
/// unchanged; only the letter targets used by this mapper need translation.
fn colemak_physical_key(key: KeyCode) -> KeyCode {
    match key {
        KeyCode::KEY_E => KeyCode::KEY_K,
        KeyCode::KEY_N => KeyCode::KEY_J,
        KeyCode::KEY_I => KeyCode::KEY_L,
        KeyCode::KEY_U => KeyCode::KEY_I,
        KeyCode::KEY_G => KeyCode::KEY_T,
        KeyCode::KEY_T => KeyCode::KEY_F,
        KeyCode::KEY_P => KeyCode::KEY_R,
        _ => key,
    }
}

fn key_event(code: KeyCode, down: bool) -> InputEvent {
    InputEvent::new(EventType::KEY.0, code.0, if down { 1 } else { 0 })
}

fn scroll_speed(value: f32) -> f32 {
    let magnitude = value.abs();
    if magnitude <= STICK_DEADZONE {
        return 0.0;
    }
    let normalized = ((magnitude - STICK_DEADZONE) / (1.0 - STICK_DEADZONE)).clamp(0.0, 1.0);
    value.signum() * normalized.powi(2) * MAX_SCROLL_PER_SECOND
}

#[cfg(test)]
mod tests {
    use super::colemak_physical_key;
    use evdev::KeyCode;

    #[test]
    fn translates_logical_qwerty_letters_for_colemak() {
        assert_eq!(colemak_physical_key(KeyCode::KEY_N), KeyCode::KEY_J);
        assert_eq!(colemak_physical_key(KeyCode::KEY_I), KeyCode::KEY_L);
        assert_eq!(colemak_physical_key(KeyCode::KEY_U), KeyCode::KEY_I);
        assert_eq!(colemak_physical_key(KeyCode::KEY_E), KeyCode::KEY_K);
        assert_eq!(colemak_physical_key(KeyCode::KEY_G), KeyCode::KEY_T);
        assert_eq!(colemak_physical_key(KeyCode::KEY_T), KeyCode::KEY_F);
        assert_eq!(colemak_physical_key(KeyCode::KEY_P), KeyCode::KEY_R);
        assert_eq!(colemak_physical_key(KeyCode::KEY_LEFT), KeyCode::KEY_LEFT);
    }
}
