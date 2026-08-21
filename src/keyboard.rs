//! DualSense-to-keyboard and mouse mapping.
//!
//! The mapper consumes semantic events from [`crate::input`] and emits Linux
//! uinput events. It is intentionally independent of the TUI, so the same
//! mapping is active in both direct and TUI modes.

use crate::input::{Button, ButtonState, ControllerEvent, ControllerEventKind, Stick, StickAxis};
use crate::keymap::{ControllerInput, DEFAULT_KEYMAP, Direction, KeyAction, KeyStroke, Keymap};
use evdev::uinput::VirtualDevice;
use evdev::{AttributeSet, EventType, InputEvent, KeyCode, RelativeAxisCode};
use std::{
    collections::{HashMap, HashSet},
    io,
    time::Instant,
};

const NO_MODIFIERS: &[KeyCode] = &[];
const SHIFT: &[KeyCode] = &[KeyCode::KEY_LEFTSHIFT];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct KeyCombo {
    modifiers: &'static [KeyCode],
    key: KeyCode,
}

pub struct KeyboardMapper {
    keyboard: VirtualDevice,
    mouse: VirtualDevice,
    keymap: Keymap,
    active_layers: HashSet<Button>,
    left_x: f32,
    left_y: f32,
    active_sources: HashMap<ControllerInput, KeyCombo>,
    active_combos: HashMap<KeyCombo, usize>,
    sequence_sources: HashSet<ControllerInput>,
    right_x: f32,
    right_y: f32,
    scroll_x_remainder: f32,
    scroll_y_remainder: f32,
    last_scroll: Instant,
}

impl KeyboardMapper {
    pub fn new() -> io::Result<Self> {
        Self::with_keymap(DEFAULT_KEYMAP)
    }

    /// Create a mapper with a specific profile.
    ///
    /// Keeping profile selection outside the event engine makes it possible to
    /// choose an application-specific keymap later without duplicating input
    /// and uinput handling.
    pub fn with_keymap(keymap: Keymap) -> io::Result<Self> {
        let keyboard_keys: AttributeSet<KeyCode> = keymap.keyboard_keys.iter().copied().collect();
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
            keymap,
            active_layers: HashSet::new(),
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

    /// Type text through the virtual keyboard. Letter keycodes are translated
    /// for Colemak in the same way as the controller's logical mappings.
    pub fn type_text(&mut self, text: &str) -> io::Result<()> {
        for character in text.chars() {
            let Some((key, shift)) = text_key(character) else {
                continue;
            };
            let combo = physical_combo(KeyStroke::with_modifiers(
                if shift { SHIFT } else { NO_MODIFIERS },
                key,
            ));
            self.emit_combo(combo, true)?;
            self.emit_combo(combo, false)?;
        }
        Ok(())
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
        let active = state == ButtonState::Down;
        if self.keymap.is_layer_modifier(button) {
            if active {
                self.active_layers.insert(button);
            } else {
                self.active_layers.remove(&button);
            }
            return Ok(());
        }

        self.apply_action(ControllerInput::Button(button), active)
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
            (Direction::Left, self.left_x < -self.keymap.stick_deadzone),
            (Direction::Right, self.left_x > self.keymap.stick_deadzone),
            (Direction::Up, self.left_y < -self.keymap.stick_deadzone),
            (Direction::Down, self.left_y > self.keymap.stick_deadzone),
        ] {
            self.apply_action(ControllerInput::Stick { stick, direction }, active)?;
        }
        Ok(())
    }

    fn apply_action(&mut self, input: ControllerInput, active: bool) -> io::Result<()> {
        let action = self.keymap.action_for(input, &self.active_layers);
        match action {
            Some(KeyAction::Sequence(strokes)) => {
                // A layer can change while a control is held. Release any old
                // held stroke before treating a new press as a sequence.
                if active {
                    self.set_source(input, None, false)?;
                    if self.sequence_sources.insert(input) {
                        self.emit_sequence(strokes)?;
                    }
                } else {
                    self.sequence_sources.remove(&input);
                    self.set_source(input, None, false)?;
                }
            }
            Some(action) => {
                self.sequence_sources.remove(&input);
                self.set_source(input, self.combo_for(action), active)?;
            }
            None => {
                self.sequence_sources.remove(&input);
                self.set_source(input, None, active)?;
            }
        }
        Ok(())
    }

    fn combo_for(&self, action: KeyAction) -> Option<KeyCombo> {
        let KeyAction::Stroke(stroke) = action else {
            return None;
        };
        Some(KeyCombo {
            modifiers: stroke.modifiers,
            key: colemak_physical_key(stroke.key),
        })
    }

    fn set_source(
        &mut self,
        source: ControllerInput,
        combo: Option<KeyCombo>,
        active: bool,
    ) -> io::Result<()> {
        if active {
            if let Some(current) = self.active_sources.get(&source).copied() {
                if Some(current) == combo {
                    return Ok(());
                }
                self.release_source(source)?;
            }

            let Some(combo) = combo else { return Ok(()) };
            let first_source = self.active_combos.get(&combo).copied().unwrap_or(0) == 0;
            if first_source {
                self.emit_combo(combo, true)?;
            }
            *self.active_combos.entry(combo).or_insert(0) += 1;
            self.active_sources.insert(source, combo);
        } else {
            self.release_source(source)?;
        }
        Ok(())
    }

    fn release_source(&mut self, source: ControllerInput) -> io::Result<()> {
        let Some(combo) = self.active_sources.remove(&source) else {
            return Ok(());
        };
        let count = self
            .active_combos
            .get_mut(&combo)
            .expect("active source must have an active combo");
        *count -= 1;
        if *count == 0 {
            self.active_combos.remove(&combo);
            self.emit_combo(combo, false)?;
        }
        Ok(())
    }

    fn emit_sequence(&mut self, strokes: &[KeyStroke]) -> io::Result<()> {
        for &stroke in strokes {
            let combo = physical_combo(stroke);
            self.emit_combo(combo, true)?;
            self.emit_combo(combo, false)?;
        }
        Ok(())
    }

    fn emit_combo(&mut self, combo: KeyCombo, down: bool) -> io::Result<()> {
        let mut events = Vec::with_capacity(combo.modifiers.len() + 1);
        if down {
            for &modifier in combo.modifiers {
                events.push(key_event(modifier, true));
            }
            events.push(key_event(combo.key, true));
        } else {
            events.push(key_event(combo.key, false));
            for &modifier in combo.modifiers.iter().rev() {
                events.push(key_event(modifier, false));
            }
        }
        self.keyboard.emit(&events)
    }

    fn emit_scroll(&mut self) -> io::Result<()> {
        let now = Instant::now();
        let delta = now.duration_since(self.last_scroll).as_secs_f32().min(0.1);
        self.last_scroll = now;
        self.scroll_x_remainder += scroll_speed(
            self.right_x,
            self.keymap.stick_deadzone,
            self.keymap.max_scroll_per_second,
        ) * delta;
        self.scroll_y_remainder += scroll_speed(
            self.right_y,
            self.keymap.stick_deadzone,
            self.keymap.max_scroll_per_second,
        ) * delta;

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

fn physical_combo(stroke: KeyStroke) -> KeyCombo {
    KeyCombo {
        modifiers: stroke.modifiers,
        key: colemak_physical_key(stroke.key),
    }
}

/// Translate a logical QWERTY key to the physical Linux keycode that produces
/// that same character with a Colemak layout. Modifiers, arrows, and slash are
/// unchanged; only the letter targets used by this mapper need translation.
fn colemak_physical_key(key: KeyCode) -> KeyCode {
    match key {
        KeyCode::KEY_D => KeyCode::KEY_G,
        KeyCode::KEY_E => KeyCode::KEY_K,
        KeyCode::KEY_F => KeyCode::KEY_E,
        KeyCode::KEY_G => KeyCode::KEY_T,
        KeyCode::KEY_I => KeyCode::KEY_L,
        KeyCode::KEY_J => KeyCode::KEY_Y,
        KeyCode::KEY_N => KeyCode::KEY_J,
        KeyCode::KEY_O => KeyCode::KEY_SEMICOLON,
        KeyCode::KEY_P => KeyCode::KEY_R,
        KeyCode::KEY_R => KeyCode::KEY_S,
        KeyCode::KEY_S => KeyCode::KEY_D,
        KeyCode::KEY_T => KeyCode::KEY_F,
        KeyCode::KEY_U => KeyCode::KEY_I,
        KeyCode::KEY_Y => KeyCode::KEY_U,
        _ => key,
    }
}

fn key_event(code: KeyCode, down: bool) -> InputEvent {
    InputEvent::new(EventType::KEY.0, code.0, if down { 1 } else { 0 })
}

fn text_key(character: char) -> Option<(KeyCode, bool)> {
    let (character, shift) = if character.is_ascii_uppercase() {
        (character.to_ascii_lowercase(), true)
    } else {
        (character, false)
    };
    let (key, punctuation_shift) = match character {
        'a' => (KeyCode::KEY_A, false),
        'b' => (KeyCode::KEY_B, false),
        'c' => (KeyCode::KEY_C, false),
        'd' => (KeyCode::KEY_D, false),
        'e' => (KeyCode::KEY_E, false),
        'f' => (KeyCode::KEY_F, false),
        'g' => (KeyCode::KEY_G, false),
        'h' => (KeyCode::KEY_H, false),
        'i' => (KeyCode::KEY_I, false),
        'j' => (KeyCode::KEY_J, false),
        'k' => (KeyCode::KEY_K, false),
        'l' => (KeyCode::KEY_L, false),
        'm' => (KeyCode::KEY_M, false),
        'n' => (KeyCode::KEY_N, false),
        'o' => (KeyCode::KEY_O, false),
        'p' => (KeyCode::KEY_P, false),
        'q' => (KeyCode::KEY_Q, false),
        'r' => (KeyCode::KEY_R, false),
        's' => (KeyCode::KEY_S, false),
        't' => (KeyCode::KEY_T, false),
        'u' => (KeyCode::KEY_U, false),
        'v' => (KeyCode::KEY_V, false),
        'w' => (KeyCode::KEY_W, false),
        'x' => (KeyCode::KEY_X, false),
        'y' => (KeyCode::KEY_Y, false),
        'z' => (KeyCode::KEY_Z, false),
        '0' => (KeyCode::KEY_0, false),
        '1' => (KeyCode::KEY_1, false),
        '2' => (KeyCode::KEY_2, false),
        '3' => (KeyCode::KEY_3, false),
        '4' => (KeyCode::KEY_4, false),
        '5' => (KeyCode::KEY_5, false),
        '6' => (KeyCode::KEY_6, false),
        '7' => (KeyCode::KEY_7, false),
        '8' => (KeyCode::KEY_8, false),
        '9' => (KeyCode::KEY_9, false),
        ')' => (KeyCode::KEY_0, true),
        '!' => (KeyCode::KEY_1, true),
        '@' => (KeyCode::KEY_2, true),
        '#' => (KeyCode::KEY_3, true),
        '$' => (KeyCode::KEY_4, true),
        '%' => (KeyCode::KEY_5, true),
        '^' => (KeyCode::KEY_6, true),
        '&' => (KeyCode::KEY_7, true),
        '*' => (KeyCode::KEY_8, true),
        '(' => (KeyCode::KEY_9, true),
        ' ' => (KeyCode::KEY_SPACE, false),
        '\t' => (KeyCode::KEY_TAB, false),
        '\n' | '\r' => (KeyCode::KEY_ENTER, false),
        '-' => (KeyCode::KEY_MINUS, false),
        '_' => (KeyCode::KEY_MINUS, true),
        '=' => (KeyCode::KEY_EQUAL, false),
        '+' => (KeyCode::KEY_EQUAL, true),
        '[' => (KeyCode::KEY_LEFTBRACE, false),
        '{' => (KeyCode::KEY_LEFTBRACE, true),
        ']' => (KeyCode::KEY_RIGHTBRACE, false),
        '}' => (KeyCode::KEY_RIGHTBRACE, true),
        '\\' => (KeyCode::KEY_BACKSLASH, false),
        '|' => (KeyCode::KEY_BACKSLASH, true),
        ';' => (KeyCode::KEY_SEMICOLON, false),
        ':' => (KeyCode::KEY_SEMICOLON, true),
        '\'' => (KeyCode::KEY_APOSTROPHE, false),
        '"' => (KeyCode::KEY_APOSTROPHE, true),
        '`' => (KeyCode::KEY_GRAVE, false),
        '~' => (KeyCode::KEY_GRAVE, true),
        ',' => (KeyCode::KEY_COMMA, false),
        '<' => (KeyCode::KEY_COMMA, true),
        '.' => (KeyCode::KEY_DOT, false),
        '>' => (KeyCode::KEY_DOT, true),
        '/' => (KeyCode::KEY_SLASH, false),
        '?' => (KeyCode::KEY_SLASH, true),
        _ => return None,
    };
    Some((key, shift || punctuation_shift))
}

fn scroll_speed(value: f32, deadzone: f32, max_scroll_per_second: f32) -> f32 {
    let magnitude = value.abs();
    if magnitude <= deadzone {
        return 0.0;
    }
    let normalized = ((magnitude - deadzone) / (1.0 - deadzone)).clamp(0.0, 1.0);
    value.signum() * normalized.powi(2) * max_scroll_per_second
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
