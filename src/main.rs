//! Read and map the Linux input events produced by a DualSense controller.

use anyhow::{Context, Result, anyhow};
use clap::Parser;
use evdev::Device;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread;

mod clipboard;
use clipboard::Clipboard;
mod dualsense;
mod focus;
use focus::FocusMonitor;
mod input;
use input::{ControllerEventKind, EventDecoder};
mod keyboard;
mod keymap;
use keyboard::KeyboardMapper;
mod light;
use light::{ControllerLight, parse_color};
mod voice;
use voice::{VoiceInput, VoiceOutput};

#[cfg(feature = "tui")]
mod tui;

#[derive(Debug, Parser)]
#[command(
    name = "dualsense",
    version,
    about = "Read and map DualSense controller input",
    long_about = "Read and map DualSense controller input.\n\nWith no argument, listen to the automatically discovered gamepad evdev device.\nPass an event device path to listen to that device explicitly.\nUse --light to change the controller RGB indicator.\nHold the right face button (○) to dictate through OPENAI_API_KEY.\nClipboard paste uses Ctrl+Shift+V by default; set DUALSENSE_VOICE_PASTE=ctrl-v when needed.\nUse --tui for an interactive status screen when the tui feature is enabled."
)]
struct Cli {
    /// Print all readable evdev devices and their DualSense classification.
    #[arg(long, conflicts_with_all = ["light", "tui", "device"])]
    list: bool,

    /// Set the controller light using RRGGBB, #RRGGBB, or off.
    #[arg(long, value_name = "COLOR", conflicts_with = "tui")]
    light: Option<String>,

    /// Run the interactive terminal UI.
    #[arg(long, conflicts_with = "light")]
    tui: bool,

    /// An evdev event device to open explicitly.
    #[arg(value_name = "/dev/input/eventN")]
    device: Option<PathBuf>,
}

fn handle_voice_output(
    path: &str,
    voice: &VoiceInput,
    clipboard: &Clipboard,
    output: &Arc<Mutex<()>>,
    message: VoiceOutput,
) {
    match message {
        VoiceOutput::Transcript(text) => {
            {
                let _guard = output.lock().expect("output lock poisoned");
                println!("[{path}] transcribed text: {text}");
            }
            if let Err(error) = voice.type_final(clipboard, &text) {
                let _guard = output.lock().expect("output lock poisoned");
                eprintln!("[{path}] clipboard paste failed: {error}");
            }
        }
        VoiceOutput::PartialTranscript(text) => {
            {
                let _guard = output.lock().expect("output lock poisoned");
                println!("[{path}] transcribed partial: {text}");
            }
            if let Err(error) = voice.type_partial(clipboard, &text) {
                let _guard = output.lock().expect("output lock poisoned");
                eprintln!("[{path}] clipboard paste failed: {error}");
            }
        }
        VoiceOutput::Status(status) => {
            let _guard = output.lock().expect("output lock poisoned");
            eprintln!("[{path}] {status}");
        }
        VoiceOutput::Light(_) | VoiceOutput::MicSample { .. } => {}
    }
}

fn drain_voice_outputs(
    path: &str,
    voice: &VoiceInput,
    clipboard: &Clipboard,
    output: &Arc<Mutex<()>>,
) {
    while let Some(message) = voice.try_recv() {
        handle_voice_output(path, voice, clipboard, output, message);
    }
}

fn spawn_voice_output_handler(
    path: String,
    voice: VoiceInput,
    clipboard: Clipboard,
    output: Arc<Mutex<()>>,
) -> Result<()> {
    thread::Builder::new()
        .name("dualsense-voice-output".to_owned())
        .spawn(move || {
            while let Some(message) = voice.recv() {
                handle_voice_output(&path, &voice, &clipboard, &output, message);
                drain_voice_outputs(&path, &voice, &clipboard, &output);
            }
        })
        .context("could not start voice output handler")?;
    Ok(())
}

fn read_device(
    path: String,
    mut device: Device,
    output: Arc<Mutex<()>>,
    mut mapper: KeyboardMapper,
    voice: VoiceInput,
    focus: Option<FocusMonitor>,
) -> Result<()> {
    dualsense::print_device_info(Path::new(&path), &device);
    device
        .set_nonblocking(true)
        .with_context(|| format!("[{path}] could not enable nonblocking input"))?;
    let mut decoder = EventDecoder::new(&device);

    loop {
        if let Some(focus) = focus.as_ref() {
            mapper.set_focused_app(focus.current());
        }
        match device.fetch_events() {
            Ok(events) => {
                for raw_event in events {
                    for event in decoder.decode(raw_event) {
                        voice.handle(event);
                        mapper.handle(event).context("keyboard mapping stopped")?;
                        if let ControllerEventKind::Button { button, state } = event.kind {
                            let _guard = output.lock().expect("output lock poisoned");
                            println!("[{path}] button {button:?} {state:?}");
                        }
                    }
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                mapper.tick().context("mouse mapping stopped")?;
                thread::sleep(std::time::Duration::from_millis(5));
            }
            Err(error) => return Err(error.into()),
        }
    }
}

fn handle_list() -> Result<()> {
    let devices = dualsense::enumerate_devices();
    if devices.is_empty() {
        eprintln!("No readable evdev devices found under /dev/input.");
        return Ok(());
    }

    for (path, device) in devices {
        println!(
            "{}: name={:?}, id={:?}, dualsense={}, gamepad={}",
            path.display(),
            device.name(),
            device.input_id(),
            dualsense::is_dualsense(&device),
            dualsense::is_dualsense_gamepad(&device)
        );
    }
    Ok(())
}

fn handle_light(color_name: String, path: Option<PathBuf>) -> Result<()> {
    let turn_off = color_name.eq_ignore_ascii_case("off");
    let color = if turn_off {
        Some((0, 0, 0))
    } else {
        parse_color(&color_name)
    };
    let color = color.ok_or_else(|| {
        anyhow!(
            "invalid color '{}'; use RRGGBB, #RRGGBB, or off",
            color_name
        )
    })?;

    let controller = match path {
        Some(path) => dualsense::open_controller(&path)?,
        None => dualsense::discover_controller()
            .ok_or_else(|| anyhow!("no DualSense gamepad evdev device found"))?,
    };
    let light = ControllerLight::from_event_path(&controller.path).ok_or_else(|| {
        anyhow!(
            "no RGB LED sysfs device found for {}. Check that hid-playstation is loaded",
            controller.path.display()
        )
    })?;

    if turn_off {
        light.off()?;
    } else {
        light.set_rgb(color.0, color.1, color.2)?;
    }
    println!("Controller light set to {color_name}");
    Ok(())
}

fn handle_tui(path: Option<PathBuf>) -> Result<()> {
    #[cfg(feature = "tui")]
    {
        tui::run(path).context("TUI stopped")
    }

    #[cfg(not(feature = "tui"))]
    {
        let _ = path;
        Err(anyhow!(
            "the TUI is optional; run with: cargo run --features tui -- --tui"
        ))
    }
}

fn create_mapper() -> Result<KeyboardMapper> {
    KeyboardMapper::new().context("could not create keyboard mapping devices")
}

fn create_voice(path: &Path) -> Result<VoiceInput> {
    VoiceInput::new(ControllerLight::from_event_path(path))
        .context("could not initialize voice input")
}

fn start_focus_monitor() -> Option<FocusMonitor> {
    match FocusMonitor::start() {
        Ok(focus) => focus,
        Err(error) => {
            eprintln!("Focused app detection unavailable: {error}");
            None
        }
    }
}

fn handle_listen(path: Option<PathBuf>) -> Result<()> {
    let controller = match path {
        Some(path) => {
            let device = dualsense::open_device(&path)?;
            if dualsense::is_dualsense(&device) && !dualsense::is_dualsense_gamepad(&device) {
                return Err(anyhow!(
                    "{} is a DualSense auxiliary device; use the main gamepad event device instead",
                    path.display()
                ));
            }
            dualsense::ControllerDevice { path, device }
        }
        None => dualsense::discover_controller().ok_or_else(|| {
            anyhow!(
                "no DualSense evdev device found. Connect the controller and check that your user\n\
                 can read /dev/input/event* (for example, via the input group), then try again.\n\
                 Use `dualsense --list` to inspect the devices that are visible."
            )
        })?,
    };

    let mapper = create_mapper()?;
    let clipboard = Clipboard::new().context("could not initialize clipboard text input")?;
    let voice = create_voice(&controller.path)?;
    voice.spawn_microphone();
    let output = Arc::new(Mutex::new(()));
    let focus = start_focus_monitor();
    let controller_path = controller.path.display().to_string();
    spawn_voice_output_handler(
        controller_path.clone(),
        voice.clone(),
        clipboard.clone(),
        Arc::clone(&output),
    )?;

    // The reader blocks until the process is terminated. Keeping this path in
    // one handler makes explicit and automatic device selection share setup.
    read_device(
        controller_path,
        controller.device,
        output,
        mapper,
        voice,
        focus,
    )
}

fn run(cli: Cli) -> Result<()> {
    if cli.list {
        return handle_list();
    }
    if let Some(color) = cli.light {
        return handle_light(color, cli.device);
    }
    if cli.tui {
        return handle_tui(cli.device);
    }
    handle_listen(cli.device)
}

fn load_environment() {
    if let Err(error) = dotenvy::dotenv()
        && !error.not_found()
    {
        eprintln!("Could not load .env: {error}");
    }
}

fn main() {
    // Load local development settings before any voice worker reads them.
    // Existing process environment variables remain the source of truth.
    load_environment();

    if let Err(error) = run(Cli::parse()) {
        eprintln!("{error:#}");
        std::process::exit(1);
    }
}
