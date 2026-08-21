//! DualSense light control through the Linux hid-playstation LED interface.
//!
//! The kernel exposes the controller's RGB indicator as a multicolor LED under
//! `/sys/class/leds`. Writing `red green blue` values to `multi_intensity`
//! sends the corresponding light command to the controller.

use anyhow::{Context, Result};
use std::{
    fs,
    path::{Path, PathBuf},
};

#[derive(Clone)]
pub struct ControllerLight {
    rgb_path: PathBuf,
}

impl ControllerLight {
    /// Find the LED belonging to an evdev event device.
    ///
    /// Resolve it through sysfs instead of assuming that the event number and
    /// the kernel input number are equal. Both numbers can change when a
    /// controller is unplugged and reconnected.
    pub fn from_event_path(event_path: &Path) -> Option<Self> {
        let event_name = event_path.file_name()?.to_str()?;
        let leds_path = PathBuf::from("/sys/class/input")
            .join(event_name)
            .join("device/device/leds");
        if let Ok(entries) = fs::read_dir(&leds_path) {
            for entry in entries.flatten() {
                let name = entry.file_name();
                if name.to_string_lossy().ends_with(":rgb:indicator") {
                    let rgb_path = entry.path().join("multi_intensity");
                    if rgb_path.exists() {
                        return Some(Self { rgb_path });
                    }
                }
            }
        }

        // Compatibility fallback for kernels that do not expose the LED under
        // the event device's sysfs tree.
        let input_name = event_name.strip_prefix("event")?;
        let rgb_path = PathBuf::from("/sys/class/leds")
            .join(format!("input{input_name}:rgb:indicator"))
            .join("multi_intensity");
        rgb_path.exists().then_some(Self { rgb_path })
    }

    #[cfg(feature = "tui")]
    pub fn current_rgb(&self) -> Result<(u8, u8, u8)> {
        let value = fs::read_to_string(&self.rgb_path)
            .with_context(|| format!("could not read {}", self.rgb_path.display()))?;
        let values: Vec<u8> = value
            .split_whitespace()
            .map(|value| value.parse::<u8>())
            .collect::<std::result::Result<_, _>>()
            .context("invalid RGB LED intensity value")?;
        match values.as_slice() {
            [red, green, blue] => Ok((*red, *green, *blue)),
            _ => Err(anyhow::anyhow!("unexpected RGB LED intensity format")),
        }
    }

    pub fn set_rgb(&self, red: u8, green: u8, blue: u8) -> Result<()> {
        fs::write(&self.rgb_path, format!("{red} {green} {blue}\n"))
            .with_context(|| format!("could not write {}", self.rgb_path.display()))
    }

    pub fn off(&self) -> Result<()> {
        self.set_rgb(0, 0, 0)
    }
}

pub fn parse_color(value: &str) -> Option<(u8, u8, u8)> {
    let hex = value.strip_prefix('#').unwrap_or(value);
    if hex.len() != 6 {
        return None;
    }
    let red = u8::from_str_radix(&hex[0..2], 16).ok()?;
    let green = u8::from_str_radix(&hex[2..4], 16).ok()?;
    let blue = u8::from_str_radix(&hex[4..6], 16).ok()?;
    Some((red, green, blue))
}
