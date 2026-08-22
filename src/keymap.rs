//! Declarative controller-to-keyboard mappings.
//!
//! Keep the mapping tables in this file and leave event handling in
//! [`crate::keyboard`]. A mapping starts with the base table, while an active
//! layer overrides only the controls it lists. The first matching active layer
//! wins when more than one layer is held.

use crate::input::{Button, Stick};
use evdev::KeyCode;
use std::collections::HashSet;

/// A direction produced by a stick after it crosses the deadzone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Direction {
    Left,
    Right,
    Up,
    Down,
}

/// A semantic controller control that can have a keyboard mapping.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ControllerInput {
    Button(Button),
    Stick { stick: Stick, direction: Direction },
}

/// One key press, expressed in logical QWERTY terms.
///
/// Colemak compensation is deliberately applied by the output side of the
/// mapper, not in these tables. That keeps mappings readable and makes the
/// same logical keys work for both controller mappings and voice typing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct KeyStroke {
    pub modifiers: &'static [KeyCode],
    pub key: KeyCode,
}

impl KeyStroke {
    pub const fn key(key: KeyCode) -> Self {
        Self {
            modifiers: &[],
            key,
        }
    }

    pub const fn with_modifiers(modifiers: &'static [KeyCode], key: KeyCode) -> Self {
        Self { modifiers, key }
    }
}

/// What a controller control does while it is active.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyAction {
    /// Hold one key stroke until the controller control is released.
    Stroke(KeyStroke),
    /// Emit each stroke once when the control is pressed.
    Sequence(&'static [KeyStroke]),
    /// Explicitly suppress a lower-priority/base mapping.
    #[allow(dead_code)]
    Disabled,
}

impl KeyAction {
    pub const fn key(key: KeyCode) -> Self {
        Self::Stroke(KeyStroke::key(key))
    }

    pub const fn combo(modifiers: &'static [KeyCode], key: KeyCode) -> Self {
        Self::Stroke(KeyStroke::with_modifiers(modifiers, key))
    }

    pub const fn sequence(strokes: &'static [KeyStroke]) -> Self {
        Self::Sequence(strokes)
    }

    #[allow(dead_code)]
    pub const fn disabled() -> Self {
        Self::Disabled
    }
}

/// One controller control and its action.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Binding {
    pub input: ControllerInput,
    pub action: KeyAction,
}

impl Binding {
    pub const fn button(button: Button, action: KeyAction) -> Self {
        Self {
            input: ControllerInput::Button(button),
            action,
        }
    }

    pub const fn stick(stick: Stick, direction: Direction, action: KeyAction) -> Self {
        Self {
            input: ControllerInput::Stick { stick, direction },
            action,
        }
    }
}

/// A set of overrides enabled while a controller button is held.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Layer {
    pub modifier: Button,
    pub bindings: &'static [Binding],
}

/// A complete mapping profile.
#[derive(Debug, Clone, Copy)]
pub struct Keymap {
    pub buttons: &'static [Binding],
    pub sticks: &'static [Binding],
    pub layers: &'static [Layer],
    pub keyboard_keys: &'static [KeyCode],
    pub stick_deadzone: f32,
    pub max_scroll_per_second: f32,
}

impl Keymap {
    /// Find the action for a control in the currently active layers.
    ///
    /// A layer only overrides controls that it explicitly lists. Missing layer
    /// entries fall back to the base mapping, while [`KeyAction::Disabled`]
    /// intentionally prevents that fallback.
    pub fn action_for(
        &self,
        input: ControllerInput,
        active_layers: &HashSet<Button>,
    ) -> Option<KeyAction> {
        for layer in self.layers {
            if !active_layers.contains(&layer.modifier) {
                continue;
            }
            if let Some(action) = find_binding(layer.bindings, input) {
                return Some(action);
            }
        }

        let bindings = match input {
            ControllerInput::Button(_) => self.buttons,
            ControllerInput::Stick { .. } => self.sticks,
        };
        find_binding(bindings, input)
    }

    pub fn is_layer_modifier(&self, button: Button) -> bool {
        self.layers.iter().any(|layer| layer.modifier == button)
    }
}

fn find_binding(bindings: &[Binding], input: ControllerInput) -> Option<KeyAction> {
    bindings
        .iter()
        .find(|binding| binding.input == input)
        .map(|binding| binding.action)
}

const CTRL: &[KeyCode] = &[KeyCode::KEY_LEFTCTRL];
const ALT: &[KeyCode] = &[KeyCode::KEY_LEFTALT];
const META: &[KeyCode] = &[KeyCode::KEY_LEFTMETA];

const R1_DPAD_UP_SEQUENCE: &[KeyStroke] = &[
    KeyStroke::key(KeyCode::KEY_P),
    KeyStroke::key(KeyCode::KEY_I),
    KeyStroke::key(KeyCode::KEY_ENTER),
];

// Base face-button and D-pad mappings.
const BASE_BUTTONS: &[Binding] = &[
    Binding::button(Button::North, KeyAction::combo(CTRL, KeyCode::KEY_U)),
    Binding::button(Button::South, KeyAction::combo(CTRL, KeyCode::KEY_E)),
    Binding::button(Button::West, KeyAction::key(KeyCode::KEY_ENTER)),
    Binding::button(Button::DpadUp, KeyAction::combo(ALT, KeyCode::KEY_UP)),
    Binding::button(Button::DpadDown, KeyAction::combo(ALT, KeyCode::KEY_DOWN)),
    Binding::button(Button::DpadLeft, KeyAction::combo(ALT, KeyCode::KEY_LEFT)),
    Binding::button(Button::DpadRight, KeyAction::combo(ALT, KeyCode::KEY_RIGHT)),
];

// Left stick directions are digitalized by the mapper after applying the
// deadzone. Direction order is left, right, up, down.
const BASE_STICKS: &[Binding] = &[
    Binding::stick(
        Stick::Left,
        Direction::Left,
        KeyAction::combo(META, KeyCode::KEY_N),
    ),
    Binding::stick(
        Stick::Left,
        Direction::Right,
        KeyAction::combo(META, KeyCode::KEY_I),
    ),
    Binding::stick(
        Stick::Left,
        Direction::Up,
        KeyAction::combo(META, KeyCode::KEY_U),
    ),
    Binding::stick(
        Stick::Left,
        Direction::Down,
        KeyAction::combo(META, KeyCode::KEY_E),
    ),
];

// L1 turns the face buttons into the movement keys used by the desktop app.
const L1_LAYER_BINDINGS: &[Binding] = &[
    Binding::button(Button::West, KeyAction::key(KeyCode::KEY_N)),
    Binding::button(Button::East, KeyAction::key(KeyCode::KEY_I)),
    Binding::button(Button::North, KeyAction::key(KeyCode::KEY_U)),
    Binding::button(Button::South, KeyAction::key(KeyCode::KEY_E)),
];

// R1 provides navigation shortcuts. It also turns D-pad up into a one-shot
// sequence rather than a held key.
const R1_LAYER_BINDINGS: &[Binding] = &[
    Binding::button(Button::DpadUp, KeyAction::sequence(R1_DPAD_UP_SEQUENCE)),
    Binding::button(Button::DpadDown, KeyAction::combo(CTRL, KeyCode::KEY_W)),
    Binding::button(Button::DpadLeft, KeyAction::combo(ALT, KeyCode::KEY_G)),
    Binding::button(Button::DpadRight, KeyAction::combo(CTRL, KeyCode::KEY_T)),
    Binding::stick(
        Stick::Left,
        Direction::Left,
        KeyAction::combo(META, KeyCode::KEY_LEFT),
    ),
    Binding::stick(
        Stick::Left,
        Direction::Right,
        KeyAction::combo(META, KeyCode::KEY_RIGHT),
    ),
];

const DEFAULT_LAYERS: &[Layer] = &[
    Layer {
        modifier: Button::L1,
        bindings: L1_LAYER_BINDINGS,
    },
    Layer {
        modifier: Button::R1,
        bindings: R1_LAYER_BINDINGS,
    },
];

/// The default profile used by direct mode and the TUI.
pub const DEFAULT_KEYMAP: Keymap = Keymap {
    buttons: BASE_BUTTONS,
    sticks: BASE_STICKS,
    layers: DEFAULT_LAYERS,
    keyboard_keys: VIRTUAL_KEYBOARD_KEYS,
    stick_deadzone: STICK_DEADZONE,
    max_scroll_per_second: MAX_SCROLL_PER_SECOND,
};

/// Analog stick deadzone used when converting a stick into directional key
/// presses. This is mapping behavior rather than controller decoding so it
/// belongs next to the profile.
pub const STICK_DEADZONE: f32 = 0.35;

/// Maximum scroll velocity generated by the right stick.
pub const MAX_SCROLL_PER_SECOND: f32 = 24.0;

/// Keys advertised by the virtual keyboard device.
///
/// This includes the logical keys used by the default profile. Keeping the
/// list here makes adding a new configurable mapping less error-prone: add its
/// key once to this capability list if it is not already present.
pub const VIRTUAL_KEYBOARD_KEYS: &[KeyCode] = &[
    // Letters and numbers used by controller mappings.
    KeyCode::KEY_A,
    KeyCode::KEY_B,
    KeyCode::KEY_C,
    KeyCode::KEY_D,
    KeyCode::KEY_E,
    KeyCode::KEY_F,
    KeyCode::KEY_G,
    KeyCode::KEY_H,
    KeyCode::KEY_I,
    KeyCode::KEY_J,
    KeyCode::KEY_K,
    KeyCode::KEY_L,
    KeyCode::KEY_M,
    KeyCode::KEY_N,
    KeyCode::KEY_O,
    KeyCode::KEY_P,
    KeyCode::KEY_Q,
    KeyCode::KEY_R,
    KeyCode::KEY_S,
    KeyCode::KEY_T,
    KeyCode::KEY_U,
    KeyCode::KEY_V,
    KeyCode::KEY_W,
    KeyCode::KEY_X,
    KeyCode::KEY_Y,
    KeyCode::KEY_Z,
    KeyCode::KEY_0,
    KeyCode::KEY_1,
    KeyCode::KEY_2,
    KeyCode::KEY_3,
    KeyCode::KEY_4,
    KeyCode::KEY_5,
    KeyCode::KEY_6,
    KeyCode::KEY_7,
    KeyCode::KEY_8,
    KeyCode::KEY_9,
    // Punctuation and editing keys used by controller mappings.
    KeyCode::KEY_MINUS,
    KeyCode::KEY_EQUAL,
    KeyCode::KEY_LEFTBRACE,
    KeyCode::KEY_RIGHTBRACE,
    KeyCode::KEY_BACKSLASH,
    KeyCode::KEY_SEMICOLON,
    KeyCode::KEY_APOSTROPHE,
    KeyCode::KEY_GRAVE,
    KeyCode::KEY_COMMA,
    KeyCode::KEY_DOT,
    KeyCode::KEY_SLASH,
    KeyCode::KEY_ENTER,
    KeyCode::KEY_SPACE,
    KeyCode::KEY_TAB,
    // Modifiers and navigation used by the profile.
    KeyCode::KEY_LEFTCTRL,
    KeyCode::KEY_LEFTALT,
    KeyCode::KEY_LEFTMETA,
    KeyCode::KEY_LEFTSHIFT,
    KeyCode::KEY_LEFT,
    KeyCode::KEY_RIGHT,
    KeyCode::KEY_UP,
    KeyCode::KEY_DOWN,
];

/// A compact label for the mapping overlay and diagnostics.
#[allow(dead_code)]
pub fn action_label(action: Option<KeyAction>) -> String {
    match action {
        None => "no mapping".to_owned(),
        Some(KeyAction::Disabled) => "disabled".to_owned(),
        Some(KeyAction::Stroke(stroke)) => stroke_label(stroke),
        Some(KeyAction::Sequence(strokes)) => strokes
            .iter()
            .map(|stroke| stroke_label(*stroke))
            .collect::<Vec<_>>()
            .join(" "),
    }
}

fn stroke_label(stroke: KeyStroke) -> String {
    let mut keys = stroke
        .modifiers
        .iter()
        .copied()
        .map(key_label)
        .collect::<Vec<_>>();
    keys.push(key_label(stroke.key));
    keys.join("+")
}

fn key_label(key: KeyCode) -> String {
    match key {
        KeyCode::KEY_LEFTCTRL => "Ctrl".to_owned(),
        KeyCode::KEY_LEFTALT => "Alt".to_owned(),
        KeyCode::KEY_LEFTMETA => "Meta".to_owned(),
        KeyCode::KEY_LEFTSHIFT => "Shift".to_owned(),
        KeyCode::KEY_ENTER => "Enter".to_owned(),
        KeyCode::KEY_SPACE => "Space".to_owned(),
        KeyCode::KEY_TAB => "Tab".to_owned(),
        KeyCode::KEY_LEFT => "←".to_owned(),
        KeyCode::KEY_RIGHT => "→".to_owned(),
        KeyCode::KEY_UP => "↑".to_owned(),
        KeyCode::KEY_DOWN => "↓".to_owned(),
        _ => format!("{key:?}").trim_start_matches("KEY_").to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layers_override_only_their_declared_controls() {
        let no_layers = HashSet::new();
        let l1 = HashSet::from([Button::L1]);

        assert_eq!(
            DEFAULT_KEYMAP.action_for(ControllerInput::Button(Button::North), &no_layers),
            Some(KeyAction::combo(CTRL, KeyCode::KEY_U)),
        );
        assert_eq!(
            DEFAULT_KEYMAP.action_for(ControllerInput::Button(Button::North), &l1),
            Some(KeyAction::key(KeyCode::KEY_U)),
        );
        assert_eq!(
            DEFAULT_KEYMAP.action_for(ControllerInput::Button(Button::DpadUp), &l1),
            Some(KeyAction::combo(ALT, KeyCode::KEY_UP)),
        );
    }

    #[test]
    fn sequence_is_configured_as_a_layer_action() {
        let r1 = HashSet::from([Button::R1]);
        assert_eq!(
            DEFAULT_KEYMAP.action_for(ControllerInput::Button(Button::DpadUp), &r1),
            Some(KeyAction::sequence(R1_DPAD_UP_SEQUENCE)),
        );
    }
}
