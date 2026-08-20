use crate::input::{ControllerEvent, EventDecoder};
use crate::keyboard::KeyboardMapper;
use crate::light::ControllerLight;
use crate::voice::{VoiceInput, VoiceOutput};
use crossterm::{
    event::{self, Event as TerminalEvent, KeyCode as TerminalKeyCode},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use evdev::{AbsoluteAxisCode, Device, EventType, InputEvent, KeyCode};
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Gauge, Paragraph},
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashSet, VecDeque},
    env,
    error::Error,
    fs, io,
    path::PathBuf,
    sync::mpsc::{self, Receiver, Sender},
    thread,
    time::{Duration, Instant},
};

const BUTTON_CODES: &[KeyCode] = &[
    KeyCode::BTN_SOUTH,
    KeyCode::BTN_EAST,
    KeyCode::BTN_NORTH,
    KeyCode::BTN_WEST,
    KeyCode::BTN_TL,
    KeyCode::BTN_TR,
    KeyCode::BTN_TL2,
    KeyCode::BTN_TR2,
    KeyCode::BTN_SELECT,
    KeyCode::BTN_START,
    KeyCode::BTN_MODE,
    KeyCode::BTN_THUMBL,
    KeyCode::BTN_THUMBR,
    KeyCode::BTN_DPAD_UP,
    KeyCode::BTN_DPAD_DOWN,
    KeyCode::BTN_DPAD_LEFT,
    KeyCode::BTN_DPAD_RIGHT,
    KeyCode::KEY_UP,
    KeyCode::KEY_DOWN,
    KeyCode::KEY_LEFT,
    KeyCode::KEY_RIGHT,
];

#[derive(Clone, Copy, Default)]
struct AxisState {
    value: i32,
    minimum: i32,
    maximum: i32,
}

impl AxisState {
    fn normalized(self) -> f64 {
        let range = (self.maximum - self.minimum) as f64;
        if range <= 0.0 {
            return 0.0;
        }
        (((self.value - self.minimum) as f64 / range) * 2.0 - 1.0).clamp(-1.0, 1.0)
    }

    fn percent(self) -> u16 {
        (((self.normalized() + 1.0) * 50.0).round() as u16).min(100)
    }

    fn normalized_with(self, calibration: AxisCalibration) -> f64 {
        let (low, high) = if self.value >= calibration.center {
            (calibration.center, calibration.maximum)
        } else {
            (calibration.minimum, calibration.center)
        };
        let range = (high - low).max(1) as f64;
        (((self.value - calibration.center) as f64) / range).clamp(-1.0, 1.0)
    }
}

#[derive(Clone, Copy, Serialize, Deserialize)]
struct AxisCalibration {
    center: i32,
    minimum: i32,
    maximum: i32,
}

impl AxisCalibration {
    fn from_axis(axis: AxisState) -> Self {
        Self {
            center: (axis.minimum + axis.maximum) / 2,
            minimum: axis.minimum,
            maximum: axis.maximum,
        }
    }

    fn valid(self) -> bool {
        self.minimum < self.center && self.center < self.maximum
    }
}

#[derive(Clone, Copy, Serialize, Deserialize)]
struct JoystickCalibration {
    left_x: AxisCalibration,
    left_y: AxisCalibration,
    right_x: AxisCalibration,
    right_y: AxisCalibration,
}

struct ControllerState {
    path: String,
    name: String,
    buttons: HashSet<KeyCode>,
    left_x: AxisState,
    left_y: AxisState,
    right_x: AxisState,
    right_y: AxisState,
    left_trigger: AxisState,
    right_trigger: AxisState,
    dpad_x: i32,
    dpad_y: i32,
    calibration: JoystickCalibration,
    calibration_message: Option<String>,
    reader_error: Option<String>,
    light: Option<ControllerLight>,
    light_rgb: [u8; 3],
    light_status: String,
    mic_status: String,
    mic_rms: f32,
    mic_peak: f32,
    mic_history: VecDeque<f32>,
}

impl ControllerState {
    fn new(path: String, device: &Device) -> Self {
        let light = ControllerLight::from_event_path(std::path::Path::new(&path));
        let (light_rgb, light_status) = match light.as_ref() {
            Some(light) => match light.current_rgb() {
                Ok((red, green, blue)) => (
                    [red, green, blue],
                    "Use r/R, g/G, b/B to adjust; 0 turns it off".to_owned(),
                ),
                Err(error) => ([0, 0, 0], format!("RGB light read failed: {error}")),
            },
            None => (
                [0, 0, 0],
                "RGB LED not exposed by hid-playstation".to_owned(),
            ),
        };
        let mut state = Self {
            path,
            name: device.name().unwrap_or("DualSense").to_owned(),
            buttons: HashSet::new(),
            left_x: AxisState {
                minimum: -32768,
                maximum: 32767,
                ..AxisState::default()
            },
            left_y: AxisState {
                minimum: -32768,
                maximum: 32767,
                ..AxisState::default()
            },
            right_x: AxisState {
                minimum: -32768,
                maximum: 32767,
                ..AxisState::default()
            },
            right_y: AxisState {
                minimum: -32768,
                maximum: 32767,
                ..AxisState::default()
            },
            left_trigger: AxisState {
                maximum: 255,
                ..AxisState::default()
            },
            right_trigger: AxisState {
                maximum: 255,
                ..AxisState::default()
            },
            dpad_x: 0,
            dpad_y: 0,
            calibration: JoystickCalibration {
                left_x: AxisCalibration {
                    center: 0,
                    minimum: -32768,
                    maximum: 32767,
                },
                left_y: AxisCalibration {
                    center: 0,
                    minimum: -32768,
                    maximum: 32767,
                },
                right_x: AxisCalibration {
                    center: 0,
                    minimum: -32768,
                    maximum: 32767,
                },
                right_y: AxisCalibration {
                    center: 0,
                    minimum: -32768,
                    maximum: 32767,
                },
            },
            calibration_message: None,
            reader_error: None,
            light,
            light_rgb,
            light_status,
            mic_status: "Searching for the DualSense microphone...".to_owned(),
            mic_rms: 0.0,
            mic_peak: 0.0,
            mic_history: VecDeque::with_capacity(128),
        };

        if let Ok(absinfo) = device.get_absinfo() {
            for (axis, info) in absinfo {
                state.set_axis_range(axis, info.minimum(), info.maximum(), info.value());
            }
        }
        if let Ok(keys) = device.get_key_state() {
            for &code in BUTTON_CODES {
                if keys.contains(code) {
                    state.buttons.insert(code);
                }
            }
        }
        state.calibration = load_calibration().unwrap_or(JoystickCalibration {
            left_x: AxisCalibration::from_axis(state.left_x),
            left_y: AxisCalibration::from_axis(state.left_y),
            right_x: AxisCalibration::from_axis(state.right_x),
            right_y: AxisCalibration::from_axis(state.right_y),
        });
        state
    }

    fn set_axis_range(&mut self, axis: AbsoluteAxisCode, minimum: i32, maximum: i32, value: i32) {
        let target = match axis {
            AbsoluteAxisCode::ABS_X => &mut self.left_x,
            AbsoluteAxisCode::ABS_Y => &mut self.left_y,
            AbsoluteAxisCode::ABS_RX => &mut self.right_x,
            AbsoluteAxisCode::ABS_RY => &mut self.right_y,
            AbsoluteAxisCode::ABS_Z => &mut self.left_trigger,
            AbsoluteAxisCode::ABS_RZ => &mut self.right_trigger,
            _ => return,
        };
        target.minimum = minimum;
        target.maximum = maximum;
        target.value = value;
    }

    fn set_axis_value(&mut self, axis: AbsoluteAxisCode, value: i32) {
        match axis {
            AbsoluteAxisCode::ABS_X => self.left_x.value = value,
            AbsoluteAxisCode::ABS_Y => self.left_y.value = value,
            AbsoluteAxisCode::ABS_RX => self.right_x.value = value,
            AbsoluteAxisCode::ABS_RY => self.right_y.value = value,
            AbsoluteAxisCode::ABS_Z => self.left_trigger.value = value,
            AbsoluteAxisCode::ABS_RZ => self.right_trigger.value = value,
            AbsoluteAxisCode::ABS_HAT0X => self.dpad_x = value,
            AbsoluteAxisCode::ABS_HAT0Y => self.dpad_y = value,
            _ => {}
        }
    }

    fn apply(&mut self, event: InputEvent) {
        match event.event_type() {
            EventType::KEY => {
                let code = KeyCode::new(event.code());
                if super::button_event(code) {
                    if event.value() == 0 {
                        self.buttons.remove(&code);
                    } else {
                        self.buttons.insert(code);
                    }
                }
            }
            EventType::ABSOLUTE => {
                self.set_axis_value(AbsoluteAxisCode(event.code()), event.value());
            }
            _ => {}
        }
    }

    fn pressed(&self, code: KeyCode) -> bool {
        self.buttons.contains(&code)
    }

    fn dpad_up(&self) -> bool {
        self.dpad_y < 0 || self.pressed(KeyCode::BTN_DPAD_UP) || self.pressed(KeyCode::KEY_UP)
    }

    fn dpad_down(&self) -> bool {
        self.dpad_y > 0 || self.pressed(KeyCode::BTN_DPAD_DOWN) || self.pressed(KeyCode::KEY_DOWN)
    }

    fn dpad_left(&self) -> bool {
        self.dpad_x < 0 || self.pressed(KeyCode::BTN_DPAD_LEFT) || self.pressed(KeyCode::KEY_LEFT)
    }

    fn dpad_right(&self) -> bool {
        self.dpad_x > 0 || self.pressed(KeyCode::BTN_DPAD_RIGHT) || self.pressed(KeyCode::KEY_RIGHT)
    }

    fn set_light(&mut self, rgb: [u8; 3]) {
        let Some(light) = self.light.as_ref() else {
            self.light_status = "RGB LED is unavailable".to_owned();
            return;
        };
        let result = if rgb == [0, 0, 0] {
            light.off()
        } else {
            light.set_rgb(rgb[0], rgb[1], rgb[2])
        };
        match result {
            Ok(()) => {
                self.light_rgb = rgb;
                self.light_status = format!("RGB #{:02X}{:02X}{:02X}", rgb[0], rgb[1], rgb[2]);
            }
            Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {
                self.light_status =
                    "RGB permission denied; install udev/99-dualsense-led.rules".to_owned();
            }
            Err(error) => self.light_status = format!("RGB light error: {error}"),
        }
    }

    fn adjust_light(&mut self, channel: usize, delta: i16) {
        let mut rgb = self.light_rgb;
        rgb[channel] = (i16::from(rgb[channel]) + delta).clamp(0, 255) as u8;
        self.set_light(rgb);
    }
}

fn default_calibration(state: &ControllerState) -> JoystickCalibration {
    JoystickCalibration {
        left_x: AxisCalibration::from_axis(state.left_x),
        left_y: AxisCalibration::from_axis(state.left_y),
        right_x: AxisCalibration::from_axis(state.right_x),
        right_y: AxisCalibration::from_axis(state.right_y),
    }
}

fn calibration_path() -> Option<PathBuf> {
    let base = env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))?;
    Some(base.join("dualsense").join("calibration.json"))
}

fn load_calibration() -> Option<JoystickCalibration> {
    let text = fs::read_to_string(calibration_path()?).ok()?;
    let calibration: JoystickCalibration = serde_json::from_str(&text).ok()?;
    let axes = [
        calibration.left_x,
        calibration.left_y,
        calibration.right_x,
        calibration.right_y,
    ];
    axes.into_iter()
        .all(AxisCalibration::valid)
        .then_some(calibration)
}

fn save_calibration(calibration: JoystickCalibration) -> io::Result<()> {
    let path = calibration_path()
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "HOME is not set"))?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let text = serde_json::to_string_pretty(&calibration).map_err(io::Error::other)?;
    fs::write(path, format!("{text}\n"))
}

#[derive(Clone, Copy)]
enum CalibrationStage {
    Center,
    Range,
}

struct CalibrationSession {
    stage: CalibrationStage,
    started: Instant,
    samples: u32,
    sums: [i64; 4],
    center: [i32; 4],
    minimum: [i32; 4],
    maximum: [i32; 4],
}

impl CalibrationSession {
    fn new(state: &ControllerState) -> Self {
        let values = axis_values(state);
        Self {
            stage: CalibrationStage::Center,
            started: Instant::now(),
            samples: 0,
            sums: [0; 4],
            center: values,
            minimum: values,
            maximum: values,
        }
    }

    fn observe_event(&mut self, event: InputEvent) {
        if !matches!(self.stage, CalibrationStage::Range)
            || event.event_type() != EventType::ABSOLUTE
        {
            return;
        }
        let axis = AbsoluteAxisCode(event.code());
        let index = match axis {
            AbsoluteAxisCode::ABS_X => 0,
            AbsoluteAxisCode::ABS_Y => 1,
            AbsoluteAxisCode::ABS_RX => 2,
            AbsoluteAxisCode::ABS_RY => 3,
            _ => return,
        };
        let value = event.value();
        self.minimum[index] = self.minimum[index].min(value);
        self.maximum[index] = self.maximum[index].max(value);
    }

    fn sample(&mut self, state: &ControllerState) {
        let values = axis_values(state);
        match self.stage {
            CalibrationStage::Center => {
                for (sum, value) in self.sums.iter_mut().zip(values) {
                    *sum += i64::from(value);
                }
                self.samples += 1;
                if self.started.elapsed() >= Duration::from_secs(2) {
                    self.finish_center(state);
                }
            }
            CalibrationStage::Range => {
                for ((minimum, maximum), value) in self
                    .minimum
                    .iter_mut()
                    .zip(self.maximum.iter_mut())
                    .zip(values)
                {
                    *minimum = (*minimum).min(value);
                    *maximum = (*maximum).max(value);
                }
            }
        }
    }

    fn finish_center(&mut self, state: &ControllerState) {
        if self.samples > 0 {
            self.center = self.sums.map(|sum| (sum / i64::from(self.samples)) as i32);
        } else {
            self.center = axis_values(state);
        }
        let values = axis_values(state);
        self.minimum = values;
        self.maximum = values;
        self.stage = CalibrationStage::Range;
        self.started = Instant::now();
    }

    fn finish_range(&self, state: &ControllerState) -> JoystickCalibration {
        let fallback = default_calibration(state);
        let axes = [
            (
                self.center[0],
                self.minimum[0],
                self.maximum[0],
                fallback.left_x,
            ),
            (
                self.center[1],
                self.minimum[1],
                self.maximum[1],
                fallback.left_y,
            ),
            (
                self.center[2],
                self.minimum[2],
                self.maximum[2],
                fallback.right_x,
            ),
            (
                self.center[3],
                self.minimum[3],
                self.maximum[3],
                fallback.right_y,
            ),
        ];
        let calibrated = axes.map(|(center, minimum, maximum, fallback)| {
            let axis = AxisCalibration {
                center,
                minimum,
                maximum,
            };
            if axis.valid() { axis } else { fallback }
        });
        JoystickCalibration {
            left_x: calibrated[0],
            left_y: calibrated[1],
            right_x: calibrated[2],
            right_y: calibrated[3],
        }
    }

    fn status(&self) -> String {
        match self.stage {
            CalibrationStage::Center => format!(
                "Release sticks: sampling center ({}/40)",
                self.samples.min(40)
            ),
            CalibrationStage::Range => {
                "Rotate both sticks through their full range, then press Enter".to_owned()
            }
        }
    }
}

fn axis_values(state: &ControllerState) -> [i32; 4] {
    [
        state.left_x.value,
        state.left_y.value,
        state.right_x.value,
        state.right_y.value,
    ]
}

enum ReaderMessage {
    Input(ControllerEvent),
    Error(String),
    MicSample { rms: f32, peak: f32 },
    MicStatus(String),
    Light([u8; 3]),
}

fn drain_voice_outputs(
    voice: &VoiceInput,
    mapper: &mut KeyboardMapper,
    sender: &Sender<ReaderMessage>,
) -> io::Result<()> {
    while let Some(message) = voice.try_recv() {
        match message {
            VoiceOutput::Transcript(text) => {
                if !voice.type_final(&text)? {
                    mapper.type_text(&text)?;
                }
                let _ = sender.send(ReaderMessage::MicStatus(format!("Voice input: {text}")));
            }
            VoiceOutput::PartialTranscript(text) => {
                if !voice.type_partial(&text)? {
                    mapper.type_text(&text)?;
                }
                let _ = sender.send(ReaderMessage::MicStatus(format!(
                    "Voice input (partial): {text}"
                )));
            }
            VoiceOutput::Status(status) => {
                let _ = sender.send(ReaderMessage::MicStatus(status));
            }
            VoiceOutput::MicSample { rms, peak } => {
                let _ = sender.send(ReaderMessage::MicSample { rms, peak });
            }
            VoiceOutput::Light(rgb) => {
                let _ = sender.send(ReaderMessage::Light(rgb));
            }
        }
    }
    Ok(())
}

fn spawn_reader(
    mut device: Device,
    sender: Sender<ReaderMessage>,
    mut mapper: KeyboardMapper,
    voice: VoiceInput,
) {
    thread::spawn(move || {
        if let Err(error) = device.set_nonblocking(true) {
            let _ = sender.send(ReaderMessage::Error(error.to_string()));
            return;
        }
        let mut decoder = EventDecoder::new(&device);
        loop {
            if let Err(error) = drain_voice_outputs(&voice, &mut mapper, &sender) {
                let _ = sender.send(ReaderMessage::Error(format!(
                    "Voice input stopped: {error}"
                )));
                return;
            }
            match device.fetch_events() {
                Ok(events) => {
                    for raw_event in events {
                        for event in decoder.decode(raw_event) {
                            voice.handle(event);
                            if let Err(error) = mapper.handle(event) {
                                let _ = sender.send(ReaderMessage::Error(error.to_string()));
                                return;
                            }
                            if sender.send(ReaderMessage::Input(event)).is_err() {
                                return;
                            }
                        }
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    if let Err(error) = mapper.tick() {
                        let _ = sender.send(ReaderMessage::Error(error.to_string()));
                        return;
                    }
                    thread::sleep(Duration::from_millis(5));
                }
                Err(error) => {
                    let _ = sender.send(ReaderMessage::Error(error.to_string()));
                    return;
                }
            }
        }
    });
}

fn active_style() -> Style {
    Style::default()
        .fg(Color::Green)
        .add_modifier(Modifier::BOLD)
}

fn inactive_style() -> Style {
    Style::default().fg(Color::DarkGray)
}

fn control(label: &str, active: bool) -> Span<'static> {
    Span::styled(
        label.to_owned(),
        if active {
            active_style()
        } else {
            inactive_style()
        },
    )
}

fn render_dpad(frame: &mut Frame<'_>, area: Rect, state: &ControllerState) {
    // Keep the four arrows on a common cross: left/right sit on the same
    // row, with the right arrow in the right-hand arm of the D-pad.
    let lines = vec![
        Line::from(vec![Span::raw("    "), control("▲", state.dpad_up())]),
        Line::from(vec![
            Span::raw("  "),
            control("◀", state.dpad_left()),
            Span::raw("   "),
            control("▶", state.dpad_right()),
        ]),
        Line::from(vec![Span::raw("    "), control("▼", state.dpad_down())]),
    ];
    frame.render_widget(
        Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title("D-pad")),
        area,
    );
}

fn render_face_buttons(frame: &mut Frame<'_>, area: Rect, state: &ControllerState) {
    let lines = vec![
        Line::from(vec![
            Span::raw("        "),
            control("△", state.pressed(KeyCode::BTN_NORTH)),
        ]),
        Line::from(vec![
            Span::raw("    "),
            control("□", state.pressed(KeyCode::BTN_WEST)),
            Span::raw("       "),
            control("○", state.pressed(KeyCode::BTN_EAST)),
        ]),
        Line::from(vec![
            Span::raw("        "),
            control("×", state.pressed(KeyCode::BTN_SOUTH)),
        ]),
    ];
    frame.render_widget(
        Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title("Buttons")),
        area,
    );
}

fn render_stick(
    frame: &mut Frame<'_>,
    area: Rect,
    title: &str,
    x: AxisState,
    y: AxisState,
    x_calibration: AxisCalibration,
    y_calibration: AxisCalibration,
) {
    // Fit the plot to small terminals too. The physical arrangement remains
    // useful even at the common 80x24 terminal size.
    let width = (area.width.saturating_sub(2) as usize).clamp(5, 13);
    let height = (area.height.saturating_sub(3) as usize).clamp(3, 9);
    let col =
        ((x.normalized_with(x_calibration) + 1.0) * 0.5 * (width - 1) as f64).round() as usize;
    // Linux ABS_Y increases downward, which already matches terminal row
    // coordinates. Do not invert it a second time for the visual plot.
    let row =
        (((y.normalized_with(y_calibration) + 1.0) * 0.5) * (height - 1) as f64).round() as usize;
    let mut lines = Vec::new();

    for current_row in 0..height {
        let mut spans = Vec::new();
        for current_col in 0..width {
            let marker = if current_col == col && current_row == row {
                "●"
            } else if current_col == width / 2 && current_row == height / 2 {
                "+"
            } else {
                "·"
            };
            spans.push(Span::styled(
                marker,
                if current_col == col && current_row == row {
                    active_style()
                } else {
                    inactive_style()
                },
            ));
        }
        lines.push(Line::from(spans));
    }
    lines.push(Line::from(format!("X {:+6}  Y {:+6}", x.value, y.value)));

    frame.render_widget(
        Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title(title)),
        area,
    );
}

fn microphone_level(rms: f32) -> f32 {
    if rms <= 0.000_001 {
        0.0
    } else {
        // Display quiet speech usefully while retaining a 60 dB dynamic range.
        ((20.0 * rms.log10() + 60.0) / 60.0).clamp(0.0, 1.0)
    }
}

fn microphone_db(rms: f32) -> String {
    if rms <= 0.000_001 {
        "-inf dB".to_owned()
    } else {
        format!("{:.1} dB", 20.0 * rms.log10())
    }
}

fn render_microphone(frame: &mut Frame<'_>, area: Rect, state: &ControllerState) {
    let block = Block::default()
        .borders(Borders::ALL)
        .title("DualSense microphone");
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    let graph_height = inner.height.saturating_sub(1) as usize;
    let graph_width = inner.width as usize;
    let start = state.mic_history.len().saturating_sub(graph_width);
    let values: Vec<f32> = state
        .mic_history
        .iter()
        .skip(start)
        .map(|rms| microphone_level(*rms))
        .collect();
    let mut lines = Vec::with_capacity(graph_height);
    for row in 0..graph_height {
        let threshold = 1.0 - (row + 1) as f32 / graph_height.max(1) as f32;
        let mut spans = Vec::with_capacity(graph_width);
        let missing = graph_width.saturating_sub(values.len());
        for column in 0..graph_width {
            let value = if column < missing {
                0.0
            } else {
                values[column - missing]
            };
            spans.push(if value >= threshold {
                Span::styled("█", active_style())
            } else {
                Span::raw(" ")
            });
        }
        lines.push(Line::from(spans));
    }
    frame.render_widget(
        Paragraph::new(lines),
        Rect {
            x: inner.x,
            y: inner.y,
            width: inner.width,
            height: graph_height as u16,
        },
    );

    let status = format!(
        "{}  RMS {}  peak {:>3}%",
        state.mic_status,
        microphone_db(state.mic_rms),
        (state.mic_peak * 100.0).round() as u16
    );
    frame.render_widget(
        Paragraph::new(status).style(Style::default().fg(Color::DarkGray)),
        Rect {
            x: inner.x,
            y: inner.y + graph_height as u16,
            width: inner.width,
            height: 1,
        },
    );
}

fn render_mapping_overlay(frame: &mut Frame<'_>) {
    let screen = frame.area();
    let popup = Rect {
        x: screen.x.saturating_add(2),
        y: screen.y.saturating_add(2),
        width: screen.width.saturating_sub(4),
        height: screen.height.saturating_sub(4),
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .title("Keyboard mappings  (m: close)");
    let inner = block.inner(popup);
    frame.render_widget(Clear, popup);
    frame.render_widget(block, popup);

    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(inner);
    let left = vec![
        Line::from(Span::styled("BASE", active_style())),
        Line::from("△  Ctrl+U"),
        Line::from("×  Ctrl+E"),
        Line::from("□  no mapping"),
        Line::from("○  no mapping"),
        Line::from("D-pad ↑  Alt+↑"),
        Line::from("D-pad ↓  Alt+↓"),
        Line::from("D-pad ←  Alt+←"),
        Line::from("D-pad →  Alt+→"),
        Line::from("L1 / R1  layer modifiers"),
        Line::from("L2 / R2  no mapping"),
    ];
    let right = vec![
        Line::from(Span::styled("LAYERS", active_style())),
        Line::from("L1 + □  N    L1 + ○  I"),
        Line::from("L1 + △  U    L1 + ×  E"),
        Line::from("R1 + ↑  P I Enter (sequence)"),
        Line::from("R1 + ↓  Ctrl+W"),
        Line::from("R1 + ←  Alt+G"),
        Line::from("R1 + →  Ctrl+T"),
        Line::from("Hold ○  voice input (green); release types text"),
        Line::from(Span::styled("STICKS", active_style())),
        Line::from("Left stick  Meta+N/I/U/E"),
        Line::from("R1 + left stick  Meta+←/→/U/E"),
        Line::from("Right stick  mouse scroll"),
        Line::from("L3 / R3, triggers, Create/PS/Options: none"),
        Line::from("Targets use logical QWERTY; Colemak compensation applies."),
    ];
    frame.render_widget(Paragraph::new(left), columns[0]);
    frame.render_widget(
        Paragraph::new(right).wrap(ratatui::widgets::Wrap { trim: true }),
        columns[1],
    );
}

fn render_trigger(frame: &mut Frame<'_>, area: Rect, title: &str, axis: AxisState) {
    let label = format!("{:>3}% ({})", axis.percent(), axis.value);
    let gauge = Gauge::default()
        .block(Block::default().borders(Borders::ALL).title(title))
        .gauge_style(active_style())
        .label(label)
        .ratio(axis.percent() as f64 / 100.0);
    frame.render_widget(gauge, area);
}

fn render_shoulder_side(
    frame: &mut Frame<'_>,
    area: Rect,
    button_label: &str,
    button_code: KeyCode,
    trigger_label: &str,
    trigger: AxisState,
    state: &ControllerState,
) {
    let parts = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(0)])
        .split(area);
    frame.render_widget(
        Paragraph::new(Line::from(control(
            button_label,
            state.pressed(button_code),
        )))
        .alignment(ratatui::layout::Alignment::Center)
        .block(Block::default().borders(Borders::ALL)),
        parts[0],
    );
    render_trigger(frame, parts[1], trigger_label, trigger);
}

fn render_shoulders(frame: &mut Frame<'_>, area: Rect, state: &ControllerState) {
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(25),
            Constraint::Percentage(50),
            Constraint::Percentage(25),
        ])
        .split(area);

    render_shoulder_side(
        frame,
        columns[0],
        "L1",
        KeyCode::BTN_TL,
        "L2",
        state.left_trigger,
        state,
    );
    render_shoulder_side(
        frame,
        columns[2],
        "R1",
        KeyCode::BTN_TR,
        "R2",
        state.right_trigger,
        state,
    );

    let center = vec![Line::from(vec![
        control("Create", state.pressed(KeyCode::BTN_SELECT)),
        Span::raw("     "),
        control("PS", state.pressed(KeyCode::BTN_MODE)),
        Span::raw("     "),
        control("Options", state.pressed(KeyCode::BTN_START)),
    ])];
    frame.render_widget(
        Paragraph::new(center)
            .alignment(ratatui::layout::Alignment::Center)
            .block(Block::default().borders(Borders::ALL).title("Center")),
        columns[1],
    );
}

fn render_center_info(
    frame: &mut Frame<'_>,
    area: Rect,
    state: &ControllerState,
    calibration: Option<&CalibrationSession>,
) {
    let calibration_line = calibration
        .map(CalibrationSession::status)
        .or_else(|| state.calibration_message.clone())
        .unwrap_or_else(|| "Press c to calibrate sticks".to_owned());
    let lines = vec![
        Line::from(Span::styled(
            "LIVE INPUT",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(format!("Device: {}", state.path)),
        Line::from(
            state
                .reader_error
                .as_deref()
                .unwrap_or("Reading input events..."),
        ),
        Line::from(format!(
            "Light: #{:02X}{:02X}{:02X} ({})",
            state.light_rgb[0], state.light_rgb[1], state.light_rgb[2], state.light_status
        )),
        Line::from(calibration_line),
    ];
    frame.render_widget(
        Paragraph::new(lines)
            .alignment(ratatui::layout::Alignment::Center)
            .block(Block::default().borders(Borders::ALL).title("Status")),
        area,
    );
}

fn draw(
    frame: &mut Frame<'_>,
    state: &ControllerState,
    calibration: Option<&CalibrationSession>,
    show_mappings: bool,
) {
    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Length(6),
            Constraint::Min(0),
            Constraint::Length(2),
        ])
        .split(frame.area());

    let title = Paragraph::new(Line::from(vec![
        Span::styled(
            " DualSense calibration monitor ",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(state.name.clone()),
    ]))
    .block(Block::default().borders(Borders::ALL));
    frame.render_widget(title, outer[0]);

    // The shoulder buttons and triggers occupy the top edge, just as on the
    // controller. Create/PS/Options are centered between the two sides.
    render_shoulders(frame, outer[1], state);

    // Match the physical arrangement: D-pad/stick on the left, face
    // buttons/stick on the right, and the center controls between them.
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(30),
            Constraint::Percentage(40),
            Constraint::Percentage(30),
        ])
        .split(outer[2]);

    let left = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(42), Constraint::Percentage(58)])
        .split(columns[0]);
    render_dpad(frame, left[0], state);
    let left_stick_title = format!(
        "Left stick  [L3 {}]",
        if state.pressed(KeyCode::BTN_THUMBL) {
            "●"
        } else {
            "○"
        }
    );
    render_stick(
        frame,
        left[1],
        &left_stick_title,
        state.left_x,
        state.left_y,
        state.calibration.left_x,
        state.calibration.left_y,
    );

    let center = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(8)])
        .split(columns[1]);
    render_center_info(frame, center[0], state, calibration);
    render_microphone(frame, center[1], state);

    let right = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(42), Constraint::Percentage(58)])
        .split(columns[2]);
    render_face_buttons(frame, right[0], state);
    let right_stick_title = format!(
        "Right stick  [R3 {}]",
        if state.pressed(KeyCode::BTN_THUMBR) {
            "●"
        } else {
            "○"
        }
    );
    render_stick(
        frame,
        right[1],
        &right_stick_title,
        state.right_x,
        state.right_y,
        state.calibration.right_x,
        state.calibration.right_y,
    );

    let footer = Paragraph::new(
        "m: mappings  c: calibrate  Enter: advance/save  r/R g/G b/B: light -/+  0: off  q/Esc: exit",
    )
    .style(Style::default().fg(Color::DarkGray));
    frame.render_widget(footer, outer[3]);
    if show_mappings {
        render_mapping_overlay(frame);
    }
}

fn run_app(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    receiver: Receiver<ReaderMessage>,
    mut state: ControllerState,
) -> io::Result<()> {
    let mut calibration: Option<CalibrationSession> = None;
    let mut show_mappings = false;

    loop {
        while let Ok(message) = receiver.try_recv() {
            match message {
                ReaderMessage::Input(event) => {
                    state.apply(event.raw);
                    if let Some(session) = calibration.as_mut() {
                        session.observe_event(event.raw);
                    }
                }
                ReaderMessage::Error(error) => state.reader_error = Some(error),
                ReaderMessage::MicSample { rms, peak } => {
                    state.mic_rms = rms;
                    state.mic_peak = peak;
                    state.mic_history.push_back(rms);
                    while state.mic_history.len() > 128 {
                        state.mic_history.pop_front();
                    }
                }
                ReaderMessage::MicStatus(status) => state.mic_status = status,
                ReaderMessage::Light(rgb) => {
                    state.light_rgb = rgb;
                    state.light_status = format!("RGB #{:02X}{:02X}{:02X}", rgb[0], rgb[1], rgb[2]);
                }
            }
        }

        if let Some(session) = calibration.as_mut() {
            session.sample(&state);
        }
        terminal.draw(|frame| draw(frame, &state, calibration.as_ref(), show_mappings))?;

        if event::poll(Duration::from_millis(50))?
            && let TerminalEvent::Key(key) = event::read()?
        {
            match key.code {
                TerminalKeyCode::Esc | TerminalKeyCode::Char('q') => return Ok(()),
                TerminalKeyCode::Char('m') => show_mappings = !show_mappings,
                TerminalKeyCode::Char('c') => {
                    calibration = Some(CalibrationSession::new(&state));
                    state.calibration_message = None;
                }
                TerminalKeyCode::Char('r') => state.adjust_light(0, -16),
                TerminalKeyCode::Char('R') => state.adjust_light(0, 16),
                TerminalKeyCode::Char('g') => state.adjust_light(1, -16),
                TerminalKeyCode::Char('G') => state.adjust_light(1, 16),
                TerminalKeyCode::Char('b') => state.adjust_light(2, -16),
                TerminalKeyCode::Char('B') => state.adjust_light(2, 16),
                TerminalKeyCode::Char('0') => state.set_light([0, 0, 0]),
                TerminalKeyCode::Enter => {
                    if let Some(session) = calibration.as_mut() {
                        match session.stage {
                            CalibrationStage::Center => session.finish_center(&state),
                            CalibrationStage::Range => {
                                let result = session.finish_range(&state);
                                match save_calibration(result) {
                                    Ok(()) => {
                                        state.calibration = result;
                                        state.calibration_message = Some(
                                            "Calibration saved; press c to recalibrate".to_owned(),
                                        );
                                    }
                                    Err(error) => {
                                        state.calibration_message = Some(format!(
                                            "Calibration measured, but save failed: {error}"
                                        ));
                                    }
                                }
                                calibration = None;
                            }
                        }
                    }
                }
                _ => {}
            }
        }
    }
}

pub fn run(path: Option<PathBuf>) -> Result<(), Box<dyn Error>> {
    let (path, device) = match path {
        Some(path) => {
            let device = Device::open(&path)?;
            if super::is_dualsense(&device) && !super::is_dualsense_gamepad(&device) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "the selected device is a DualSense auxiliary device, not the main gamepad",
                )
                .into());
            }
            (path, device)
        }
        None => evdev::enumerate()
            .find(|(_, device)| super::is_dualsense_gamepad(device))
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    "no DualSense gamepad evdev device found",
                )
            })?,
    };

    let (sender, receiver) = mpsc::channel();
    let state = ControllerState::new(path.display().to_string(), &device);
    let voice = VoiceInput::new(state.light.clone())?;
    let mapper = KeyboardMapper::new()?;
    spawn_reader(device, sender.clone(), mapper, voice.clone());
    voice.spawn_microphone();

    enable_raw_mode()?;
    let stdout = io::stdout();
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    execute!(terminal.backend_mut(), EnterAlternateScreen)?;

    let result = run_app(&mut terminal, receiver, state);

    // Always restore the terminal, including when drawing or input polling fails.
    let cleanup = (|| -> io::Result<()> {
        disable_raw_mode()?;
        execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
        terminal.show_cursor()?;
        Ok(())
    })();

    if let Err(error) = result {
        let _ = cleanup;
        return Err(error.into());
    }
    cleanup?;
    Ok(())
}
