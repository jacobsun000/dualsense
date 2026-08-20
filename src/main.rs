//! Print the Linux input events produced by a DualSense controller.
//!
//! The kernel's `hid-playstation` driver exposes the controller through evdev.  Reading
//! evdev is preferable here to talking to the HID report directly: it gives us the same
//! button and analog-axis events that a future keyboard mapper will consume.

use evdev::{Device, KeyCode};
use std::env;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread;

mod input;
use input::EventDecoder;

#[cfg(feature = "tui")]
mod tui;

const SONY_VENDOR_ID: u16 = 0x054c;
const DUALSENSE_USB_PRODUCT_ID: u16 = 0x0ce6;
const DUALSENSE_BLUETOOTH_PRODUCT_ID: u16 = 0x0df2;

fn usage() {
    println!(
        "Usage: dualsense [--list | /dev/input/eventN]\n\n\
         With no argument, listen to the main gamepad evdev device.\n\
         Pass an event device path to listen to that device explicitly.\n\
         Use --tui (with the tui feature) for an interactive status screen.\n\n\
         Examples:\n\
           dualsense\n\
           dualsense --list\n\
           dualsense /dev/input/event17\n\
           cargo run --features tui -- --tui"
    );
}

fn is_dualsense(device: &Device) -> bool {
    let name = device.name().unwrap_or_default().to_ascii_lowercase();
    let id = device.input_id();

    name.contains("dualsense")
        || (id.vendor() == SONY_VENDOR_ID
            && matches!(
                id.product(),
                DUALSENSE_USB_PRODUCT_ID | DUALSENSE_BLUETOOTH_PRODUCT_ID
            ))
}

fn is_dualsense_gamepad(device: &Device) -> bool {
    if !is_dualsense(device) {
        return false;
    }

    // hid-playstation exposes separate evdev devices for the touchpad, motion
    // sensors, and headset jack. Their ABS axes are not joystick/trigger axes.
    let name = device.name().unwrap_or_default().to_ascii_lowercase();
    !name.contains("touchpad") && !name.contains("motion sensor") && !name.contains("headset jack")
}

fn print_device_info(path: &str, device: &Device) {
    println!(
        "Listening to {path}: name={:?}, id={:?}",
        device.name(),
        device.input_id()
    );
}

fn button_event(code: KeyCode) -> bool {
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

fn read_device(path: String, mut device: Device, output: Arc<Mutex<()>>) {
    print_device_info(&path, &device);
    let mut decoder = EventDecoder::new(&device);

    loop {
        match device.fetch_events() {
            Ok(events) => {
                for raw_event in events {
                    for event in decoder.decode(raw_event) {
                        let _guard = output.lock().expect("output lock poisoned");
                        println!("[{path}] {event:?}");
                    }
                }
            }
            Err(error) => {
                let _guard = output.lock().expect("output lock poisoned");
                eprintln!("[{path}] stopped reading controller: {error}");
                return;
            }
        }
    }
}

fn main() {
    let args: Vec<_> = env::args_os().skip(1).collect();

    if args.first().is_some_and(|arg| arg == "--tui") {
        if args.len() > 2 {
            usage();
            std::process::exit(2);
        }

        #[cfg(feature = "tui")]
        {
            let path = args.get(1).map(PathBuf::from);
            if let Err(error) = tui::run(path) {
                eprintln!("TUI stopped: {error}");
                std::process::exit(1);
            }
            return;
        }

        #[cfg(not(feature = "tui"))]
        {
            eprintln!("The TUI is optional. Run with: cargo run --features tui -- --tui");
            std::process::exit(2);
        }
    }

    if args.len() > 1 {
        usage();
        std::process::exit(2);
    }
    if args
        .first()
        .is_some_and(|arg| arg == "-h" || arg == "--help")
    {
        usage();
        return;
    }

    let output = Arc::new(Mutex::new(()));
    let mut workers = Vec::new();

    if let Some(arg) = args.first() {
        if arg == "--list" {
            let mut found = false;
            for (path, device) in evdev::enumerate() {
                found = true;
                println!(
                    "{}: name={:?}, id={:?}, dualsense={}, gamepad={}",
                    path.display(),
                    device.name(),
                    device.input_id(),
                    is_dualsense(&device),
                    is_dualsense_gamepad(&device)
                );
            }
            if !found {
                eprintln!("No readable evdev devices found under /dev/input.");
            }
            return;
        }

        let path = PathBuf::from(arg);
        match Device::open(&path) {
            Ok(device) => {
                if is_dualsense(&device) && !is_dualsense_gamepad(&device) {
                    eprintln!(
                        "{} is a DualSense auxiliary device; use the main gamepad event device instead.",
                        path.display()
                    );
                    return;
                }

                let path = path.display().to_string();
                workers.push(thread::spawn({
                    let output = Arc::clone(&output);
                    move || read_device(path, device, output)
                }));
            }
            Err(error) => {
                eprintln!("Could not open {}: {error}", path.display());
                std::process::exit(1);
            }
        }
    } else {
        for (path, device) in evdev::enumerate() {
            if is_dualsense_gamepad(&device) {
                let path = path.display().to_string();
                workers.push(thread::spawn({
                    let output = Arc::clone(&output);
                    move || read_device(path, device, output)
                }));
            }
        }

        if workers.is_empty() {
            eprintln!(
                "No DualSense evdev device found. Connect the controller and check that your user\n\
                 can read /dev/input/event* (for example, via the input group), then try again.\n\
                 Use `dualsense --list` to inspect the devices that are visible."
            );
            return;
        }
    }

    // The workers block in fetch_events(). Ctrl-C terminates the process naturally.
    for worker in workers {
        let _ = worker.join();
    }
}
