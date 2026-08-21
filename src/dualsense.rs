//! DualSense device discovery and identification helpers.
//!
//! A DualSense exposes several evdev devices (the gamepad, touchpad, motion
//! sensors, and headset jack).  The gamepad device is the one consumers of
//! controller input should open by default.

use anyhow::{Context, Result, bail};
use evdev::Device;
#[cfg(feature = "tui")]
use evdev::KeyCode;
use std::path::{Path, PathBuf};

const SONY_VENDOR_ID: u16 = 0x054c;
const DUALSENSE_USB_PRODUCT_ID: u16 = 0x0ce6;
const DUALSENSE_BLUETOOTH_PRODUCT_ID: u16 = 0x0df2;

/// An evdev device together with the path it was opened from.
pub struct ControllerDevice {
    pub path: PathBuf,
    pub device: Device,
}

/// Return all readable input devices currently exposed by evdev.
pub fn enumerate_devices() -> Vec<(PathBuf, Device)> {
    evdev::enumerate().collect()
}

/// Find the first DualSense gamepad device.
pub fn discover_controller() -> Option<ControllerDevice> {
    enumerate_devices()
        .into_iter()
        .find(|(_, device)| is_dualsense_gamepad(device))
        .map(|(path, device)| ControllerDevice { path, device })
}

/// Open an explicitly selected evdev device.
pub fn open_device(path: &Path) -> Result<Device> {
    Device::open(path).with_context(|| format!("could not open {}", path.display()))
}

/// Open an explicitly selected DualSense gamepad device.
pub fn open_controller(path: &Path) -> Result<ControllerDevice> {
    let device = open_device(path)?;
    if !is_dualsense_gamepad(&device) {
        if is_dualsense(&device) {
            bail!(
                "{} is a DualSense auxiliary device; use the main gamepad event device instead",
                path.display()
            );
        }
        bail!("{} is not a DualSense gamepad device", path.display());
    }

    Ok(ControllerDevice {
        path: path.to_owned(),
        device,
    })
}

/// Whether an evdev device belongs to a DualSense controller.
pub fn is_dualsense(device: &Device) -> bool {
    let name = device.name().unwrap_or_default().to_ascii_lowercase();
    let id = device.input_id();

    // Do not identify devices by name alone: virtual mapper devices can also
    // contain "DualSense" in their name. The Sony vendor ID is authoritative.
    id.vendor() == SONY_VENDOR_ID
        && (matches!(
            id.product(),
            DUALSENSE_USB_PRODUCT_ID | DUALSENSE_BLUETOOTH_PRODUCT_ID
        ) || name.contains("dualsense"))
}

/// Whether an evdev device is the controller's main gamepad interface.
pub fn is_dualsense_gamepad(device: &Device) -> bool {
    if !is_dualsense(device) {
        return false;
    }

    // hid-playstation exposes separate evdev devices for the touchpad, motion
    // sensors, and headset jack.
    let name = device.name().unwrap_or_default().to_ascii_lowercase();
    !name.contains("touchpad") && !name.contains("motion sensor") && !name.contains("headset jack")
}

/// Return whether an input code represents a controller button.
#[cfg(feature = "tui")]
pub fn button_event(code: KeyCode) -> bool {
    matches!(
        code,
        KeyCode::BTN_SOUTH
            | KeyCode::BTN_EAST
            | KeyCode::BTN_NORTH
            | KeyCode::BTN_WEST
            | KeyCode::BTN_TL
            | KeyCode::BTN_TR
            | KeyCode::BTN_TL2
            | KeyCode::BTN_TR2
            | KeyCode::BTN_SELECT
            | KeyCode::BTN_START
            | KeyCode::BTN_MODE
            | KeyCode::BTN_THUMBL
            | KeyCode::BTN_THUMBR
            | KeyCode::BTN_DPAD_UP
            | KeyCode::BTN_DPAD_DOWN
            | KeyCode::BTN_DPAD_LEFT
            | KeyCode::BTN_DPAD_RIGHT
            // hid-playstation reports the DualSense D-pad as the standard
            // keyboard direction key codes on some kernel versions.
            | KeyCode::KEY_UP
            | KeyCode::KEY_DOWN
            | KeyCode::KEY_LEFT
            | KeyCode::KEY_RIGHT
    )
}

/// Print a compact description of an evdev device.
pub fn print_device_info(path: &Path, device: &Device) {
    println!(
        "{}: name={:?}, id={:?}",
        path.display(),
        device.name(),
        device.input_id()
    );
}
