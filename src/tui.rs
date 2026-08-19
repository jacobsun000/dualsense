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
    widgets::{Block, Borders, Gauge, Paragraph},
};
use std::{
    collections::HashSet,
    error::Error,
    io,
    path::PathBuf,
    sync::mpsc::{self, Receiver, Sender},
    thread,
    time::Duration,
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
    reader_error: Option<String>,
}

impl ControllerState {
    fn new(path: String, device: &Device) -> Self {
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
            reader_error: None,
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
}

enum ReaderMessage {
    Input(InputEvent),
    Error(String),
}

fn spawn_reader(mut device: Device, sender: Sender<ReaderMessage>) {
    thread::spawn(move || {
        loop {
            match device.fetch_events() {
                Ok(events) => {
                    for event in events {
                        if super::event_kind(&event).is_some()
                            && sender.send(ReaderMessage::Input(event)).is_err()
                        {
                            return;
                        }
                    }
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

fn render_stick(frame: &mut Frame<'_>, area: Rect, title: &str, x: AxisState, y: AxisState) {
    // Fit the plot to small terminals too. The physical arrangement remains
    // useful even at the common 80x24 terminal size.
    let width = (area.width.saturating_sub(2) as usize).clamp(5, 13);
    let height = (area.height.saturating_sub(3) as usize).clamp(3, 9);
    let col = ((x.normalized() + 1.0) * 0.5 * (width - 1) as f64).round() as usize;
    // Linux ABS_Y increases downward, which already matches terminal row
    // coordinates. Do not invert it a second time for the visual plot.
    let row = (((y.normalized() + 1.0) * 0.5) * (height - 1) as f64).round() as usize;
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

fn render_center_info(frame: &mut Frame<'_>, area: Rect, state: &ControllerState) {
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
    ];
    frame.render_widget(
        Paragraph::new(lines)
            .alignment(ratatui::layout::Alignment::Center)
            .block(Block::default().borders(Borders::ALL).title("Status")),
        area,
    );
}

fn draw(frame: &mut Frame<'_>, state: &ControllerState) {
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
    );

    render_center_info(frame, columns[1], state);

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
    );

    let footer =
        Paragraph::new("Press q or Esc to exit").style(Style::default().fg(Color::DarkGray));
    frame.render_widget(footer, outer[3]);
}

fn run_app(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    receiver: Receiver<ReaderMessage>,
    mut state: ControllerState,
) -> io::Result<()> {
    loop {
        while let Ok(message) = receiver.try_recv() {
            match message {
                ReaderMessage::Input(event) => state.apply(event),
                ReaderMessage::Error(error) => state.reader_error = Some(error),
            }
        }

        terminal.draw(|frame| draw(frame, &state))?;

        if event::poll(Duration::from_millis(50))? {
            if let TerminalEvent::Key(key) = event::read()?
                && matches!(key.code, TerminalKeyCode::Esc | TerminalKeyCode::Char('q'))
            {
                return Ok(());
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
    spawn_reader(device, sender);

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
