//! Focused-application tracking for application-specific mappings.
//!
//! Wayland intentionally does not expose a compositor-independent global
//! focused-window query. The Hyprland backend therefore uses the compositor's
//! own IPC: the event socket tells us when focus changes, and the control
//! socket supplies the complete `activewindow` record for that change.

use anyhow::{Context, Result, anyhow};
use serde_json::Value;
use std::{
    env,
    io::{BufRead, BufReader, Read, Write},
    os::unix::net::UnixStream,
    path::PathBuf,
    sync::{Arc, RwLock},
    thread,
    time::Duration,
};

/// The focused window identity exposed by a compositor backend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FocusedApp {
    /// Hyprland's stable window address for the current compositor instance.
    pub address: String,
    /// The current Wayland app-id / X11 window class.
    pub app_id: String,
    /// The initial app-id / class, which is often more stable than `app_id`.
    pub initial_app_id: String,
    pub title: String,
    pub pid: Option<u32>,
}

/// A shared, continuously updated focused-window snapshot.
#[derive(Clone)]
pub struct FocusMonitor {
    current: Arc<RwLock<Option<FocusedApp>>>,
}

impl FocusMonitor {
    /// Start the best available compositor backend.
    ///
    /// `Ok(None)` means the current session is unsupported or is not Hyprland.
    /// The monitor is intentionally optional so keyboard mapping continues to
    /// work with the default profile on other desktops.
    pub fn start() -> Result<Option<Self>> {
        let Some(paths) = HyprlandPaths::from_environment() else {
            return Ok(None);
        };
        if !paths.control.exists() || !paths.events.exists() {
            return Ok(None);
        }

        let current = Arc::new(RwLock::new(None));
        let thread_current = Arc::clone(&current);
        thread::Builder::new()
            .name("focused-app".to_owned())
            .spawn(move || monitor_hyprland(paths, thread_current))
            .context("could not start focused-app monitor")?;

        Ok(Some(Self { current }))
    }

    pub fn current(&self) -> Option<FocusedApp> {
        self.current.read().ok().and_then(|current| current.clone())
    }
}

struct HyprlandPaths {
    control: PathBuf,
    events: PathBuf,
}

impl HyprlandPaths {
    fn from_environment() -> Option<Self> {
        let runtime_dir = env::var_os("XDG_RUNTIME_DIR")?;
        let instance = env::var_os("HYPRLAND_INSTANCE_SIGNATURE")?;
        if instance.is_empty() {
            return None;
        }

        let base = PathBuf::from(runtime_dir).join("hypr").join(instance);
        Some(Self {
            control: base.join(".socket.sock"),
            events: base.join(".socket2.sock"),
        })
    }
}

fn monitor_hyprland(paths: HyprlandPaths, current: Arc<RwLock<Option<FocusedApp>>>) {
    let mut warned_connection = false;

    loop {
        let stream = match UnixStream::connect(&paths.events) {
            Ok(stream) => {
                warned_connection = false;
                stream
            }
            Err(error) => {
                if !warned_connection {
                    eprintln!("Focused app detection unavailable: {error}");
                    warned_connection = true;
                }
                thread::sleep(Duration::from_millis(250));
                continue;
            }
        };

        // Subscribe before taking the initial snapshot. If focus changes while
        // the control query is in flight, the corresponding event remains in
        // the stream and the later query observes the newest focused window.
        refresh_hyprland_focus(&paths, &current);

        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) => break,
                Ok(_) if is_focus_event(&line) => {
                    refresh_hyprland_focus(&paths, &current);
                }
                Ok(_) => {}
                Err(error) => {
                    eprintln!("Focused app event stream stopped: {error}");
                    break;
                }
            }
        }

        thread::sleep(Duration::from_millis(250));
    }
}

fn is_focus_event(line: &str) -> bool {
    let Some((event, _payload)) = line.trim_end().split_once(">>") else {
        return false;
    };
    matches!(event, "activewindow" | "activewindowv2")
}

fn refresh_hyprland_focus(paths: &HyprlandPaths, current: &Arc<RwLock<Option<FocusedApp>>>) {
    match query_active_window(&paths.control) {
        Ok(focused) => {
            if let Ok(mut current) = current.write() {
                *current = focused;
            }
        }
        Err(error) => eprintln!("Could not query Hyprland focused app: {error}"),
    }
}

fn query_active_window(control_socket: &PathBuf) -> Result<Option<FocusedApp>> {
    let mut socket = UnixStream::connect(control_socket)?;
    socket.set_read_timeout(Some(Duration::from_secs(1)))?;
    socket.write_all(b"j/activewindow")?;
    socket.shutdown(std::net::Shutdown::Write)?;

    let mut response = String::new();
    socket.read_to_string(&mut response)?;
    parse_active_window(&response)
}

fn parse_active_window(response: &str) -> Result<Option<FocusedApp>> {
    let value: Value =
        serde_json::from_str(response).context("invalid Hyprland activewindow JSON")?;
    let object = value
        .as_object()
        .ok_or_else(|| anyhow!("Hyprland activewindow response is not an object"))?;

    let address = string_field(object, "address");
    if address.is_empty() || address == "0x0" {
        return Ok(None);
    }

    let mut app_id = string_field(object, "class");
    let initial_app_id = string_field(object, "initialClass");
    if app_id.is_empty() {
        app_id.clone_from(&initial_app_id);
    }

    Ok(Some(FocusedApp {
        address,
        app_id,
        initial_app_id,
        title: string_field(object, "title"),
        pid: object
            .get("pid")
            .and_then(Value::as_u64)
            .and_then(|pid| u32::try_from(pid).ok()),
    }))
}

fn string_field(object: &serde_json::Map<String, Value>, name: &str) -> String {
    object
        .get(name)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_hyprland_active_window_identity() {
        let window = parse_active_window(
            r#"{
                "address": "0x123",
                "class": "firefox",
                "title": "Example",
                "initialClass": "firefox",
                "pid": 42
            }"#,
        )
        .unwrap()
        .unwrap();

        assert_eq!(window.address, "0x123");
        assert_eq!(window.app_id, "firefox");
        assert_eq!(window.initial_app_id, "firefox");
        assert_eq!(window.title, "Example");
        assert_eq!(window.pid, Some(42));
    }

    #[test]
    fn empty_active_window_means_no_focused_app() {
        assert_eq!(parse_active_window("{}").unwrap(), None);
    }

    #[test]
    fn only_hyprland_focus_events_trigger_refreshes() {
        assert!(is_focus_event("activewindowv2>>0x123\n"));
        assert!(is_focus_event("activewindow>>firefox,Example\n"));
        assert!(!is_focus_event("workspace>>1\n"));
    }
}
