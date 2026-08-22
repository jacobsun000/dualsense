//! Wayland clipboard-backed text input.
//!
//! A clipboard paste avoids emitting one virtual-keyboard event for every
//! character in a transcript. The clipboard contents are copied through
//! `wl-clipboard-rs`, while the paste shortcut is sent through a small uinput
//! keyboard so this works for native Wayland and XWayland applications alike.

#![cfg_attr(not(feature = "voice"), allow(dead_code))]

use anyhow::{Context, Result, anyhow};
use evdev::uinput::VirtualDevice;
use evdev::{AttributeSet, EventType, InputEvent, KeyCode};
use std::io::Read;
use std::sync::{Arc, Mutex};
use wl_clipboard_rs::{
    copy::{
        ClipboardType as CopyClipboardType, MimeSource, MimeType as CopyMimeType, Options,
        Seat as CopySeat, ServeRequests, Source, clear, copy, copy_multi,
    },
    paste::{
        ClipboardType as PasteClipboardType, Error as PasteError, MimeType as PasteMimeType,
        Seat as PasteSeat, get_contents, get_mime_types_ordered,
    },
};

const CTRL: KeyCode = KeyCode::KEY_LEFTCTRL;
const SHIFT: KeyCode = KeyCode::KEY_LEFTSHIFT;
const PASTE: KeyCode = KeyCode::KEY_V;

/// A saved set of clipboard MIME sources that can be restored later.
#[derive(Debug)]
pub struct ClipboardSnapshot {
    sources: Vec<MimeSource>,
}

/// Provides clipboard snapshot, restore, and paste operations.
///
/// The instance owns a virtual keyboard used only for the paste shortcut. The
/// keyboard is kept separate from controller mappings so text input remains
/// available in both direct and TUI modes.
#[derive(Clone)]
pub struct Clipboard {
    keyboard: Arc<Mutex<VirtualDevice>>,
    paste_with_shift: bool,
}

impl Clipboard {
    /// Create a clipboard text-input device.
    pub fn new() -> Result<Self> {
        let keyboard_keys: AttributeSet<KeyCode> = [CTRL, SHIFT, PASTE].into_iter().collect();
        let keyboard = VirtualDevice::builder()?
            .name(b"DualSense clipboard paste")
            .with_keys(&keyboard_keys)?
            .build()?;
        let paste_with_shift = !matches!(
            std::env::var("DUALSENSE_VOICE_PASTE").as_deref(),
            Ok("ctrl-v") | Ok("ctrl_v") | Ok("control-v") | Ok("control_v")
        );

        Ok(Self {
            keyboard: Arc::new(Mutex::new(keyboard)),
            paste_with_shift,
        })
    }

    /// Capture all currently advertised regular-clipboard MIME sources.
    pub fn snapshot(&self) -> Result<ClipboardSnapshot> {
        let mime_types =
            match get_mime_types_ordered(PasteClipboardType::Regular, PasteSeat::Unspecified) {
                Ok(mime_types) => mime_types,
                Err(PasteError::NoSeats)
                | Err(PasteError::ClipboardEmpty)
                | Err(PasteError::NoMimeType) => Vec::new(),
                Err(error) => return Err(anyhow!(error)),
            };

        let mut sources = Vec::with_capacity(mime_types.len());
        for mime_type in mime_types {
            // These are clipboard protocol targets, not data formats. Asking
            // their owners for contents can wait forever, which would stall
            // transcription output while a clipboard manager is active.
            if matches!(
                mime_type.as_str(),
                "TARGETS" | "MULTIPLE" | "SAVE_TARGETS" | "COMPOUND_TEXT"
            ) {
                continue;
            }

            let (mut pipe, actual_mime_type) = get_contents(
                PasteClipboardType::Regular,
                PasteSeat::Unspecified,
                PasteMimeType::Specific(&mime_type),
            )
            .map_err(|error| anyhow!(error))?;
            let mut data = Vec::new();
            pipe.read_to_end(&mut data)
                .context("could not read clipboard contents")?;
            sources.push(MimeSource {
                source: Source::Bytes(data.into_boxed_slice()),
                mime_type: CopyMimeType::Specific(actual_mime_type),
            });
        }

        Ok(ClipboardSnapshot { sources })
    }

    /// Restore a snapshot captured by [`Self::snapshot`].
    pub fn restore(&self, snapshot: ClipboardSnapshot) -> Result<()> {
        if snapshot.sources.is_empty() {
            clear(CopyClipboardType::Regular, CopySeat::All).map_err(|error| anyhow!(error))
        } else {
            copy_multi(Options::new(), snapshot.sources).map_err(|error| anyhow!(error))
        }
    }

    /// Put `text` on the clipboard and send the configured paste shortcut.
    pub fn paste(&self, text: String) -> Result<()> {
        let mut options = Options::new();
        // Keep the source alive while the target application requests the
        // clipboard data. Restoring the caller's clipboard happens after this
        // method returns.
        options.serve_requests(ServeRequests::Unlimited);
        copy(
            options,
            Source::Bytes(text.into_bytes().into_boxed_slice()),
            CopyMimeType::Text,
        )
        .map_err(|error| anyhow!(error))?;

        self.send_paste_shortcut()?;
        Ok(())
    }

    fn send_paste_shortcut(&self) -> Result<()> {
        let modifiers: &[KeyCode] = if self.paste_with_shift {
            &[CTRL, SHIFT]
        } else {
            &[CTRL]
        };
        let mut events = Vec::with_capacity(modifiers.len() * 2 + 2);
        for &modifier in modifiers {
            events.push(key_event(modifier, true));
        }
        events.push(key_event(PASTE, true));
        events.push(key_event(PASTE, false));
        for &modifier in modifiers.iter().rev() {
            events.push(key_event(modifier, false));
        }

        let mut keyboard = self
            .keyboard
            .lock()
            .map_err(|_| anyhow!("clipboard keyboard lock poisoned"))?;
        keyboard
            .emit(&events)
            .context("could not send clipboard paste shortcut")?;
        Ok(())
    }
}

fn key_event(code: KeyCode, down: bool) -> InputEvent {
    InputEvent::new(EventType::KEY.0, code.0, if down { 1 } else { 0 })
}
