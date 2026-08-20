//! Print the Linux input events produced by a DualSense controller.
//!
//! The kernel's `hid-playstation` driver exposes the controller through evdev.  Reading
//! evdev is preferable here to talking to the HID report directly: it gives us the same
//! button and analog-axis events that a future keyboard mapper will consume.

use evdev::{Device, KeyCode};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread;
use std::{env, io};

mod input;
use input::EventDecoder;
mod light;
use light::{ControllerLight, parse_color};
mod keyboard;
use keyboard::KeyboardMapper;
mod voice;
use voice::{VoiceInput, VoiceOutput};

#[cfg(feature = "tui")]
mod tui;

const SONY_VENDOR_ID: u16 = 0x054c;
const DUALSENSE_USB_PRODUCT_ID: u16 = 0x0ce6;
const DUALSENSE_BLUETOOTH_PRODUCT_ID: u16 = 0x0df2;

fn usage() {
    println!(
        "Usage: dualsense [--list | --light <RRGGBB|off> | /dev/input/eventN]\n\n\
         With no argument, listen to the main gamepad evdev device.\n\
         Pass an event device path to listen to that device explicitly.\n\
         Use --light to change the controller RGB indicator; optionally pass\n\
         an event device path after the color.\n\
         Hold the right face button (○) to dictate through OPENAI_API_KEY;\n\
         release to type the transcript.\n\
         Use --tui (with the tui feature) for an interactive status screen.\n\n\
         Examples:\n\
           dualsense\n\
           dualsense --list\n\
           dualsense --light '#ff6600'\n\
           dualsense --light off /dev/input/event24\n\
           dualsense /dev/input/event17\n\
           cargo run --features tui -- --tui"
    );
}

fn is_dualsense(device: &Device) -> bool {
    let name = device.name().unwrap_or_default().to_ascii_lowercase();
    let id = device.input_id();

    // Do not identify devices by name alone: our virtual mapper devices are
    // intentionally named "DualSense keyboard/mouse mapper" too. The Sony
    // vendor ID is the important part of the identity check.
    id.vendor() == SONY_VENDOR_ID
        && (matches!(
            id.product(),
            DUALSENSE_USB_PRODUCT_ID | DUALSENSE_BLUETOOTH_PRODUCT_ID
        ) || name.contains("dualsense"))
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

fn drain_voice_outputs(
    path: &str,
    voice: &VoiceInput,
    mapper: &mut KeyboardMapper,
    output: &Arc<Mutex<()>>,
) -> io::Result<()> {
    while let Some(message) = voice.try_recv() {
        match message {
            VoiceOutput::Transcript(text) => {
                mapper.type_text(&text)?;
                let _guard = output.lock().expect("output lock poisoned");
                println!("[{path}] voice input: {text}");
            }
            VoiceOutput::Status(status) => {
                let _guard = output.lock().expect("output lock poisoned");
                eprintln!("[{path}] {status}");
            }
            VoiceOutput::Light(_) | VoiceOutput::MicSample { .. } => {}
        }
    }
    Ok(())
}

fn read_device(
    path: String,
    mut device: Device,
    output: Arc<Mutex<()>>,
    mut mapper: KeyboardMapper,
    voice: VoiceInput,
) {
    print_device_info(&path, &device);
    if let Err(error) = device.set_nonblocking(true) {
        eprintln!("[{path}] could not enable nonblocking input: {error}");
        return;
    }
    let mut decoder = EventDecoder::new(&device);

    loop {
        if let Err(error) = drain_voice_outputs(&path, &voice, &mut mapper, &output) {
            let _guard = output.lock().expect("output lock poisoned");
            eprintln!("[{path}] voice input stopped: {error}");
            return;
        }
        match device.fetch_events() {
            Ok(events) => {
                for raw_event in events {
                    for event in decoder.decode(raw_event) {
                        voice.handle(event);
                        if let Err(error) = mapper.handle(event) {
                            let _guard = output.lock().expect("output lock poisoned");
                            eprintln!("[{path}] keyboard mapping stopped: {error}");
                            return;
                        }
                        let _guard = output.lock().expect("output lock poisoned");
                        println!("[{path}] {event:?}");
                    }
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                if let Err(error) = mapper.tick() {
                    let _guard = output.lock().expect("output lock poisoned");
                    eprintln!("[{path}] mouse mapping stopped: {error}");
                    return;
                }
                thread::sleep(std::time::Duration::from_millis(5));
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

    if args.first().is_some_and(|arg| arg == "--light") {
        if !(2..=3).contains(&args.len()) {
            usage();
            std::process::exit(2);
        }
        let color_name = args[1].to_string_lossy();
        let turn_off = color_name.eq_ignore_ascii_case("off");
        let color = if turn_off {
            Some((0, 0, 0))
        } else {
            parse_color(&color_name)
        };
        let Some(color) = color else {
            eprintln!(
                "Invalid color '{}'; use RRGGBB, #RRGGBB, or off",
                color_name
            );
            std::process::exit(2);
        };

        let selected = if let Some(path) = args.get(2).map(PathBuf::from) {
            match Device::open(&path) {
                Ok(device) if is_dualsense_gamepad(&device) => Some((path, device)),
                Ok(_) => {
                    eprintln!("{} is not a DualSense gamepad device", path.display());
                    None
                }
                Err(error) => {
                    eprintln!("Could not open {}: {error}", path.display());
                    None
                }
            }
        } else {
            evdev::enumerate().find(|(_, device)| is_dualsense_gamepad(device))
        };
        let Some((path, _device)) = selected else {
            eprintln!("No DualSense gamepad evdev device found.");
            std::process::exit(1);
        };
        let Some(light) = ControllerLight::from_event_path(&path) else {
            eprintln!(
                "No RGB LED sysfs device found for {}. Check that hid-playstation is loaded.",
                path.display()
            );
            std::process::exit(1);
        };
        let result = if turn_off {
            light.off()
        } else {
            light.set_rgb(color.0, color.1, color.2)
        };
        if let Err(error) = result {
            eprintln!("Could not change controller light: {error}");
            std::process::exit(1);
        }
        println!("Controller light set to {color_name}");
        return;
    }

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

                let mapper = match KeyboardMapper::new() {
                    Ok(mapper) => mapper,
                    Err(error) => {
                        eprintln!("Could not create keyboard mapping devices: {error}");
                        std::process::exit(1);
                    }
                };
                let voice = match VoiceInput::new(ControllerLight::from_event_path(&path)) {
                    Ok(voice) => voice,
                    Err(error) => {
                        eprintln!("Could not initialize voice input: {error}");
                        std::process::exit(1);
                    }
                };
                voice.spawn_microphone();
                let path = path.display().to_string();
                workers.push(thread::spawn({
                    let output = Arc::clone(&output);
                    move || read_device(path, device, output, mapper, voice)
                }));
            }
            Err(error) => {
                eprintln!("Could not open {}: {error}", path.display());
                std::process::exit(1);
            }
        }
    } else {
        let devices: Vec<_> = evdev::enumerate()
            .filter(|(_, device)| is_dualsense_gamepad(device))
            .collect();
        let Some((voice_path, _)) = devices.first() else {
            eprintln!(
                "No DualSense evdev device found. Connect the controller and check that your user\n\
                 can read /dev/input/event* (for example, via the input group), then try again.\n\
                 Use `dualsense --list` to inspect the devices that are visible."
            );
            return;
        };
        let voice = match VoiceInput::new(ControllerLight::from_event_path(voice_path)) {
            Ok(voice) => voice,
            Err(error) => {
                eprintln!("Could not initialize voice input: {error}");
                return;
            }
        };
        voice.spawn_microphone();
        for (path, device) in devices {
            let mapper = match KeyboardMapper::new() {
                Ok(mapper) => mapper,
                Err(error) => {
                    eprintln!("Could not create keyboard mapping devices: {error}");
                    return;
                }
            };
            let path = path.display().to_string();
            let voice = voice.clone();
            workers.push(thread::spawn({
                let output = Arc::clone(&output);
                move || read_device(path, device, output, mapper, voice)
            }));
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
