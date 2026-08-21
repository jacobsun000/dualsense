//! Push-to-talk voice input for the DualSense microphone.
//!
//! Holding the right face button (○) captures the controller microphone, streams 24 kHz PCM audio to
//! OpenAI's realtime transcription session, and returns transcript updates to the
//! input reader. Text is pasted through Wayland with Unicode support, and the user's
//! clipboard is restored after each completed turn.

use crate::input::ControllerEvent;
#[cfg(feature = "voice")]
use crate::input::{Button, ButtonState, ControllerEventKind};
use crate::light::ControllerLight;
use std::io;

#[cfg(feature = "voice")]
mod enabled {
    use super::*;
    use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
    use futures_util::{SinkExt, StreamExt};
    use serde_json::{Value, json};
    use std::ffi::CString;
    use std::io::Read;
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    };
    use std::thread;
    use std::time::Duration;
    use tokio::sync::mpsc as tokio_mpsc;
    use tokio_tungstenite::{
        MaybeTlsStream, WebSocketStream, connect_async,
        tungstenite::{Message, client::IntoClientRequest},
    };
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
    use wrtype::{Modifier, WrtypeClient};

    const INPUT_RATE: u32 = 24_000;
    // Kitty and other terminal emulators can be busy processing a paste while
    // the next clipboard owner is installed.  Partial transcripts therefore
    // use the compositor's virtual keyboard instead of repeatedly replacing
    // the clipboard.  A small inter-character delay also keeps slower clients
    // from dropping events.
    const WAYLAND_TYPE_DELAY: Duration = Duration::from_millis(20);
    const CLIPBOARD_READY_DELAY: Duration = Duration::from_millis(10);
    const CLIPBOARD_PASTE_DELAY: Duration = Duration::from_millis(30);
    const SESSION_IDLE_TIMEOUT: Duration = Duration::from_secs(5);
    const VOICE_BUTTON: Button = Button::East;

    #[derive(Debug)]
    pub enum VoiceOutput {
        Transcript(String),
        PartialTranscript(String),
        Status(String),
        MicSample { rms: f32, peak: f32 },
        Light([u8; 3]),
    }

    struct ClipboardSnapshot {
        sources: Vec<MimeSource>,
    }

    impl ClipboardSnapshot {
        fn capture() -> Result<Self, String> {
            let mime_types =
                match get_mime_types_ordered(PasteClipboardType::Regular, PasteSeat::Unspecified) {
                    Ok(mime_types) => mime_types,
                    Err(PasteError::NoSeats)
                    | Err(PasteError::ClipboardEmpty)
                    | Err(PasteError::NoMimeType) => Vec::new(),
                    Err(error) => return Err(error.to_string()),
                };
            let mut sources = Vec::with_capacity(mime_types.len());
            for mime_type in mime_types {
                let (mut pipe, actual_mime_type) = get_contents(
                    PasteClipboardType::Regular,
                    PasteSeat::Unspecified,
                    PasteMimeType::Specific(&mime_type),
                )
                .map_err(|error| error.to_string())?;
                let mut data = Vec::new();
                pipe.read_to_end(&mut data)
                    .map_err(|error| error.to_string())?;
                sources.push(MimeSource {
                    source: Source::Bytes(data.into_boxed_slice()),
                    mime_type: CopyMimeType::Specific(actual_mime_type),
                });
            }
            Ok(Self { sources })
        }

        fn restore(self) -> Result<(), String> {
            if self.sources.is_empty() {
                clear(CopyClipboardType::Regular, CopySeat::All).map_err(|error| error.to_string())
            } else {
                copy_multi(Options::new(), self.sources).map_err(|error| error.to_string())
            }
        }
    }

    struct ClipboardSession {
        snapshot: ClipboardSnapshot,
    }

    impl ClipboardSession {
        fn new() -> Result<Self, String> {
            Ok(Self {
                snapshot: ClipboardSnapshot::capture()?,
            })
        }

        fn paste(
            &self,
            text: &str,
            client: &mut WrtypeClient,
            paste_with_shift: bool,
        ) -> io::Result<()> {
            // Keep the data source alive until another copy replaces it. A
            // clipboard manager may request the data before the target app
            // receives the paste shortcut; limiting this to one request can
            // therefore consume the source before the actual paste.
            let mut options = Options::new();
            options.serve_requests(ServeRequests::Unlimited);
            copy(
                options,
                Source::Bytes(text.as_bytes().to_vec().into_boxed_slice()),
                CopyMimeType::Text,
            )
            .map_err(io::Error::other)?;
            thread::sleep(CLIPBOARD_READY_DELAY);
            let modifiers = if paste_with_shift {
                [Modifier::Ctrl, Modifier::Shift].as_slice()
            } else {
                [Modifier::Ctrl].as_slice()
            };
            client
                .send_shortcut(modifiers, "v")
                .map_err(|error| io::Error::other(error.to_string()))?;
            thread::sleep(CLIPBOARD_PASTE_DELAY);
            Ok(())
        }

        fn restore(self) -> io::Result<()> {
            self.snapshot.restore().map_err(io::Error::other)
        }
    }

    struct TextInput {
        client: Option<WrtypeClient>,
        attempted: bool,
        partial_seen: bool,
        partial_typed: bool,
        clipboard: Option<ClipboardSession>,
        clipboard_disabled: bool,
        paste_with_shift: bool,
    }

    impl Default for TextInput {
        fn default() -> Self {
            let paste_with_shift = !matches!(
                std::env::var("DUALSENSE_VOICE_PASTE").as_deref(),
                Ok("ctrl-v") | Ok("ctrl_v") | Ok("control-v") | Ok("control_v")
            );
            Self {
                client: None,
                attempted: false,
                partial_seen: false,
                partial_typed: false,
                clipboard: None,
                clipboard_disabled: false,
                paste_with_shift,
            }
        }
    }

    impl TextInput {
        fn ensure_client(&mut self, output: &mpsc::Sender<VoiceOutput>) -> bool {
            if self.attempted {
                return self.client.is_some();
            }
            self.attempted = true;
            match WrtypeClient::new() {
                Ok(client) => {
                    self.client = Some(client);
                    let _ = output.send(VoiceOutput::Status(
                        "Using Wayland virtual keyboard for transcription".to_owned(),
                    ));
                    true
                }
                Err(error) => {
                    let _ = output.send(VoiceOutput::Status(format!(
                        "Wayland virtual keyboard unavailable; deferring partials until the final transcript: {error}"
                    )));
                    false
                }
            }
        }

        fn restore_clipboard(&mut self, output: &mpsc::Sender<VoiceOutput>) {
            let Some(session) = self.clipboard.take() else {
                return;
            };
            if let Err(error) = session.restore() {
                let _ = output.send(VoiceOutput::Status(format!(
                    "Could not restore clipboard: {error}"
                )));
            }
        }

        fn begin_turn(&mut self, output: &mpsc::Sender<VoiceOutput>) {
            self.restore_clipboard(output);
            self.partial_seen = false;
            self.partial_typed = false;
        }

        fn type_wayland(
            &mut self,
            text: &str,
            output: &mpsc::Sender<VoiceOutput>,
        ) -> io::Result<bool> {
            if !self.ensure_client(output) {
                return Ok(false);
            }
            let Some(client) = self.client.as_mut() else {
                return Ok(false);
            };
            client
                .type_text_with_delay(text, WAYLAND_TYPE_DELAY)
                .map(|()| true)
                .map_err(|error| io::Error::other(error.to_string()))
        }

        fn type_text(
            &mut self,
            text: &str,
            output: &mpsc::Sender<VoiceOutput>,
        ) -> io::Result<bool> {
            if !self.clipboard_disabled && self.clipboard.is_none() {
                match ClipboardSession::new() {
                    Ok(session) => {
                        self.clipboard = Some(session);
                        let shortcut = if self.paste_with_shift {
                            "Ctrl+Shift+V"
                        } else {
                            "Ctrl+V"
                        };
                        let _ = output.send(VoiceOutput::Status(format!(
                            "Using clipboard paste ({shortcut}) for transcription"
                        )));
                    }
                    Err(error) => {
                        self.clipboard_disabled = true;
                        let _ = output.send(VoiceOutput::Status(format!(
                            "Clipboard unavailable; using paced Wayland keyboard input: {error}"
                        )));
                    }
                }
            }

            if self.clipboard.is_some() {
                if !self.ensure_client(output) {
                    self.clipboard_disabled = true;
                    self.restore_clipboard(output);
                } else {
                    let result = {
                        let client = self.client.as_mut().expect("Wayland client exists");
                        let session = self.clipboard.as_ref().expect("clipboard session exists");
                        session.paste(text, client, self.paste_with_shift)
                    };
                    match result {
                        Ok(()) => return Ok(true),
                        Err(error) => {
                            self.clipboard_disabled = true;
                            self.restore_clipboard(output);
                            let _ = output.send(VoiceOutput::Status(format!(
                                "Clipboard paste failed; using paced Wayland keyboard input: {error}"
                            )));
                        }
                    }
                }
            }

            self.type_wayland(text, output)
        }
    }

    struct AudioChunk {
        samples: Vec<f32>,
        sample_rate: u32,
        channels: u16,
    }

    enum WorkerMessage {
        Start,
        Stop,
        Audio(AudioChunk),
    }

    #[derive(Clone)]
    pub struct VoiceInput {
        commands: tokio_mpsc::UnboundedSender<WorkerMessage>,
        recording: Arc<AtomicBool>,
        outputs: Arc<Mutex<mpsc::Receiver<VoiceOutput>>>,
        output_sender: mpsc::Sender<VoiceOutput>,
        microphone_started: Arc<AtomicBool>,
        text_input: Arc<Mutex<TextInput>>,
        light: Option<ControllerLight>,
    }

    impl VoiceInput {
        pub fn new(light: Option<ControllerLight>) -> io::Result<Self> {
            let (commands, command_receiver) = tokio_mpsc::unbounded_channel();
            let (output_sender, output_receiver) = mpsc::channel();
            let recording = Arc::new(AtomicBool::new(false));
            let worker_output = output_sender.clone();
            thread::Builder::new()
                .name("dualsense-voice-session".to_owned())
                .spawn(move || {
                    let _ = rustls::crypto::ring::default_provider().install_default();
                    let runtime = match tokio::runtime::Builder::new_current_thread()
                        .enable_io()
                        .enable_time()
                        .build()
                    {
                        Ok(runtime) => runtime,
                        Err(error) => {
                            let _ = worker_output.send(VoiceOutput::Status(format!(
                                "Voice runtime unavailable: {error}"
                            )));
                            return;
                        }
                    };
                    runtime.block_on(run_session(command_receiver, worker_output));
                })
                .map_err(io::Error::other)?;

            let input = Self {
                commands,
                recording,
                outputs: Arc::new(Mutex::new(output_receiver)),
                output_sender,
                microphone_started: Arc::new(AtomicBool::new(false)),
                text_input: Arc::new(Mutex::new(TextInput::default())),
                light,
            };
            input.set_light([0, 0, 255]);
            Ok(input)
        }

        /// React to the semantic controller event that controls push-to-talk.
        pub fn handle(&self, event: ControllerEvent) {
            let ControllerEventKind::Button { button, state } = event.kind else {
                return;
            };
            if button != VOICE_BUTTON {
                return;
            }

            match state {
                ButtonState::Down if !self.recording.swap(true, Ordering::AcqRel) => {
                    self.begin_text_turn();
                    self.set_light([0, 255, 0]);
                    let _ = self.commands.send(WorkerMessage::Start);
                }
                ButtonState::Up if self.recording.swap(false, Ordering::AcqRel) => {
                    self.set_light([0, 0, 255]);
                    let _ = self.commands.send(WorkerMessage::Stop);
                }
                _ => {}
            }
        }

        pub fn spawn_microphone(&self) {
            if self
                .microphone_started
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
            {
                return;
            }

            let recording = Arc::clone(&self.recording);
            let commands = self.commands.clone();
            let output_sender = self.output_sender.clone();
            thread::Builder::new()
                .name("dualsense-microphone".to_owned())
                .spawn(move || {
                    run_microphone(recording, commands, output_sender);
                })
                .ok();
        }

        pub fn try_recv(&self) -> Option<VoiceOutput> {
            let receiver = self.outputs.lock().expect("voice output lock poisoned");
            receiver.try_recv().ok()
        }

        fn begin_text_turn(&self) {
            let mut input = self.text_input.lock().expect("text input lock poisoned");
            input.begin_turn(&self.output_sender);
        }

        /// Deliver a transcript delta through the ordered virtual keyboard.
        ///
        /// If that protocol is unavailable, the delta is deferred and the
        /// final transcript is entered once through the fallback path.
        pub fn type_partial(&self, text: &str) -> io::Result<bool> {
            let mut input = self.text_input.lock().expect("text input lock poisoned");

            // Never replace the clipboard for every realtime delta.  Clipboard
            // ownership and the target application's paste request are
            // asynchronous; when the next delta arrives first, Kitty can read
            // the wrong source (or no source at all).  The virtual keyboard
            // protocol gives us ordered key events and wrtype round-trips each
            // event before continuing.
            input.partial_seen = true;
            match input.type_wayland(text, &self.output_sender)? {
                true => {
                    input.partial_typed = true;
                    Ok(true)
                }
                false => {
                    // Do not fall back to one clipboard paste per delta.  Let
                    // the final, authoritative transcript be pasted once
                    // instead.  Returning true tells the caller that this
                    // delta is intentionally deferred rather than asking the
                    // raw uinput mapper to type it and then duplicating it at
                    // completion.
                    Ok(true)
                }
            }
        }

        /// Complete a transcript turn without duplicating streamed deltas.
        ///
        /// When virtual-keyboard delivery was unavailable, partials were
        /// deferred and this final transcript is delivered once through the
        /// normal fallback path.  When all partials were typed, the final API
        /// event is only an acknowledgement and must not be entered again.
        pub fn type_final(&self, text: &str) -> io::Result<bool> {
            let mut input = self.text_input.lock().expect("text input lock poisoned");
            if input.partial_seen {
                let partial_typed = input.partial_typed;
                input.partial_seen = false;
                input.partial_typed = false;
                if partial_typed {
                    input.restore_clipboard(&self.output_sender);
                    return Ok(true);
                }

                let typed = input.type_text(text, &self.output_sender)?;
                if typed {
                    input.restore_clipboard(&self.output_sender);
                }
                return Ok(typed);
            }

            let typed = input.type_text(text, &self.output_sender)?;
            if typed {
                input.restore_clipboard(&self.output_sender);
            }
            Ok(typed)
        }

        fn set_light(&self, rgb: [u8; 3]) {
            let Some(light) = self.light.as_ref() else {
                return;
            };
            match light.set_rgb(rgb[0], rgb[1], rgb[2]) {
                Ok(()) => {
                    let _ = self.output_sender.send(VoiceOutput::Light(rgb));
                }
                Err(error) => {
                    let _ = self.output_sender.send(VoiceOutput::Status(format!(
                        "Controller light error: {error}"
                    )));
                }
            }
        }
    }

    type Socket = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

    async fn run_session(
        mut commands: tokio_mpsc::UnboundedReceiver<WorkerMessage>,
        output: mpsc::Sender<VoiceOutput>,
    ) {
        let mut socket: Option<Socket> = None;
        let mut recording = false;
        let mut idle_timer: Option<std::pin::Pin<Box<tokio::time::Sleep>>> = None;
        let mut resampler = Resampler::default();
        let api_key = std::env::var("OPENAI_API_KEY").ok();
        let mut warned_missing_key = false;

        loop {
            if socket.is_none() {
                idle_timer = None;
                let Some(command) = commands.recv().await else {
                    return;
                };
                match command {
                    WorkerMessage::Start => {
                        recording = true;
                        let Some(api_key) = api_key.as_deref() else {
                            if !warned_missing_key {
                                let _ = output.send(VoiceOutput::Status(
                                    "Voice input disabled: OPENAI_API_KEY is not set".to_owned(),
                                ));
                                warned_missing_key = true;
                            }
                            continue;
                        };
                        match connect_session(api_key).await {
                            Ok(new_socket) => {
                                socket = Some(new_socket);
                                resampler = Resampler::default();
                                let _ = output.send(VoiceOutput::Status(
                                    "Voice transcription connected".to_owned(),
                                ));
                            }
                            Err(error) => {
                                let _ = output.send(VoiceOutput::Status(format!(
                                    "Voice connection failed: {error}"
                                )));
                            }
                        }
                    }
                    WorkerMessage::Stop => recording = false,
                    WorkerMessage::Audio(_) => {}
                }
                continue;
            }

            let ready = if let Some(timer) = idle_timer.as_mut() {
                tokio::select! {
                    command = commands.recv() => Some((command, None, false)),
                    incoming = async {
                        socket.as_mut().expect("socket exists").next().await
                    } => Some((None, incoming, false)),
                    _ = timer.as_mut() => Some((None, None, true)),
                }
            } else {
                tokio::select! {
                    command = commands.recv() => Some((command, None, false)),
                    incoming = async {
                        socket.as_mut().expect("socket exists").next().await
                    } => Some((None, incoming, false)),
                }
            };
            let Some((command, incoming, idle_expired)) = ready else {
                return;
            };
            if idle_expired {
                if let Some(mut socket) = socket.take() {
                    let _ = socket.send(Message::Close(None)).await;
                }
                recording = false;
                idle_timer = None;
                let _ = output.send(VoiceOutput::Status(
                    "Voice transcription session closed after 5 seconds idle".to_owned(),
                ));
                continue;
            }
            if command.is_none() && incoming.is_none() {
                return;
            }

            if let Some(command) = command {
                match command {
                    WorkerMessage::Start => {
                        recording = true;
                        idle_timer = None;
                    }
                    WorkerMessage::Stop => {
                        recording = false;
                        if let Some(socket) = socket.as_mut()
                            && let Err(error) = socket
                                .send(Message::Text(
                                    json!({ "type": "input_audio_buffer.commit" })
                                        .to_string()
                                        .into(),
                                ))
                                .await
                        {
                            let _ = output
                                .send(VoiceOutput::Status(format!("Voice commit failed: {error}")));
                        }
                        idle_timer = Some(Box::pin(tokio::time::sleep(SESSION_IDLE_TIMEOUT)));
                    }
                    WorkerMessage::Audio(chunk) if recording => {
                        let pcm = resampler.convert(chunk);
                        if !pcm.is_empty() {
                            let mut bytes = Vec::with_capacity(pcm.len() * 2);
                            for sample in pcm {
                                bytes.extend_from_slice(&sample.to_le_bytes());
                            }
                            let event = json!({
                                "type": "input_audio_buffer.append",
                                "audio": BASE64.encode(bytes),
                            });
                            let send_result = match socket.as_mut() {
                                Some(socket) => {
                                    socket.send(Message::Text(event.to_string().into())).await
                                }
                                None => return,
                            };
                            if let Err(error) = send_result {
                                let _ = output.send(VoiceOutput::Status(format!(
                                    "Voice audio send failed: {error}"
                                )));
                                socket = None;
                                idle_timer = None;
                                recording = false;
                            }
                        }
                    }
                    WorkerMessage::Audio(_) => {}
                }
            }

            if let Some(incoming) = incoming {
                match incoming {
                    Ok(Message::Text(text)) => {
                        handle_server_event(text.as_ref(), &output);
                    }
                    Ok(Message::Close(_)) | Err(_) | Ok(Message::Binary(_)) => {
                        let _ = output.send(VoiceOutput::Status(
                            "Voice transcription disconnected".to_owned(),
                        ));
                        socket = None;
                        idle_timer = None;
                        recording = false;
                    }
                    Ok(_) => {}
                }
            }
        }
    }

    async fn connect_session(api_key: &str) -> Result<Socket, String> {
        let mut request = "wss://api.openai.com/v1/realtime?intent=transcription"
            .into_client_request()
            .map_err(|error| error.to_string())?;
        let authorization = format!("Bearer {api_key}")
            .parse()
            .map_err(|error| format!("invalid authorization header: {error}"))?;
        request.headers_mut().insert("Authorization", authorization);
        let (mut socket, _) = connect_async(request)
            .await
            .map_err(|error| error.to_string())?;
        let session_update = json!({
            "type": "session.update",
            "session": {
                "type": "transcription",
                "audio": {
                    "input": {
                        "format": { "type": "audio/pcm", "rate": INPUT_RATE },
                        "transcription": {
                            "model": "gpt-live-transcribe",
                            "delay": "low"
                        },
                        "turn_detection": null
                    }
                }
            }
        });
        socket
            .send(Message::Text(session_update.to_string().into()))
            .await
            .map_err(|error| error.to_string())?;
        Ok(socket)
    }

    fn handle_server_event(text: &str, output: &mpsc::Sender<VoiceOutput>) {
        let Ok(event) = serde_json::from_str::<Value>(text) else {
            return;
        };
        match event.get("type").and_then(Value::as_str) {
            Some("input_audio_buffer.committed") => {
                let _ = output.send(VoiceOutput::Status(
                    "Voice audio committed; waiting for transcript".to_owned(),
                ));
            }
            Some("conversation.item.input_audio_transcription.delta") => {
                if let Some(delta) = event.get("delta").and_then(Value::as_str)
                    && !delta.is_empty()
                {
                    let _ = output.send(VoiceOutput::PartialTranscript(delta.to_owned()));
                }
            }
            Some("conversation.item.input_audio_transcription.completed") => {
                if let Some(transcript) = event.get("transcript").and_then(Value::as_str) {
                    let transcript = transcript.trim();
                    if !transcript.is_empty() {
                        let _ = output.send(VoiceOutput::Transcript(transcript.to_owned()));
                    }
                }
            }
            Some("error") => {
                let message = event
                    .get("error")
                    .and_then(|error| error.get("message"))
                    .and_then(Value::as_str)
                    .unwrap_or("unknown realtime API error");
                let _ = output.send(VoiceOutput::Status(format!("Voice API error: {message}")));
            }
            _ => {}
        }
    }

    /// Audio samples are exposed by ALSA/CPAL. Keep the conversion here so
    /// every supported ALSA sample format uses the same normalized range.
    trait NormalizedSample {
        fn normalized(self) -> f32;
    }

    macro_rules! signed_sample {
        ($type:ty, $max:expr) => {
            impl NormalizedSample for $type {
                fn normalized(self) -> f32 {
                    self as f32 / $max
                }
            }
        };
    }

    macro_rules! unsigned_sample {
        ($type:ty, $max:expr, $center:expr) => {
            impl NormalizedSample for $type {
                fn normalized(self) -> f32 {
                    (self as f32 - $center) / $max
                }
            }
        };
    }

    signed_sample!(i8, 128.0);
    signed_sample!(i16, 32768.0);
    signed_sample!(i32, 2_147_483_648.0);
    signed_sample!(i64, 9_223_372_036_854_775_808.0);
    unsigned_sample!(u8, 128.0, 128.0);
    unsigned_sample!(u16, 32768.0, 32768.0);
    unsigned_sample!(u32, 2_147_483_648.0, 2_147_483_648.0);
    unsigned_sample!(
        u64,
        9_223_372_036_854_775_808.0,
        9_223_372_036_854_775_808.0
    );

    impl NormalizedSample for f32 {
        fn normalized(self) -> f32 {
            self.clamp(-1.0, 1.0)
        }
    }

    impl NormalizedSample for f64 {
        fn normalized(self) -> f32 {
            self.clamp(-1.0, 1.0) as f32
        }
    }

    fn report_audio<T: cpal::SizedSample + NormalizedSample + Copy>(
        samples: &[T],
        sample_rate: u32,
        channels: u16,
        recording: &Arc<AtomicBool>,
        commands: &tokio_mpsc::UnboundedSender<WorkerMessage>,
        output: &mpsc::Sender<VoiceOutput>,
    ) {
        if samples.is_empty() {
            return;
        }
        let mut sum = 0.0_f32;
        let mut peak = 0.0_f32;
        for sample in samples {
            let value = (*sample).normalized();
            sum += value * value;
            peak = peak.max(value.abs());
        }
        let rms = (sum / samples.len() as f32).sqrt().clamp(0.0, 1.0);
        let _ = output.send(VoiceOutput::MicSample { rms, peak });

        if recording.load(Ordering::Acquire) {
            let normalized = samples
                .iter()
                .map(|sample| (*sample).normalized())
                .collect();
            let _ = commands.send(WorkerMessage::Audio(AudioChunk {
                samples: normalized,
                sample_rate,
                channels,
            }));
        }
    }

    struct StderrSilencer {
        saved: libc::c_int,
    }

    impl StderrSilencer {
        fn new() -> io::Result<Self> {
            let null_path = CString::new("/dev/null").expect("static path has no NUL");
            let null = unsafe { libc::open(null_path.as_ptr(), libc::O_WRONLY) };
            if null < 0 {
                return Err(io::Error::last_os_error());
            }
            let saved = unsafe { libc::dup(libc::STDERR_FILENO) };
            if saved < 0 {
                unsafe { libc::close(null) };
                return Err(io::Error::last_os_error());
            }
            if unsafe { libc::dup2(null, libc::STDERR_FILENO) } < 0 {
                unsafe {
                    libc::close(null);
                    libc::close(saved);
                }
                return Err(io::Error::last_os_error());
            }
            unsafe { libc::close(null) };
            Ok(Self { saved })
        }
    }

    impl Drop for StderrSilencer {
        fn drop(&mut self) {
            unsafe {
                libc::dup2(self.saved, libc::STDERR_FILENO);
                libc::close(self.saved);
            }
        }
    }

    fn run_microphone(
        recording: Arc<AtomicBool>,
        commands: tokio_mpsc::UnboundedSender<WorkerMessage>,
        output: mpsc::Sender<VoiceOutput>,
    ) {
        let host = cpal::default_host();
        let devices = {
            let _stderr_silencer = StderrSilencer::new().ok();
            match host.devices() {
                Ok(devices) => devices.collect::<Vec<_>>(),
                Err(error) => {
                    let _ = output.send(VoiceOutput::Status(format!(
                        "Microphone unavailable: {error}"
                    )));
                    return;
                }
            }
        };
        let device = devices
            .into_iter()
            .filter_map(|device| {
                let name = device.name().ok()?;
                let lower = name.to_ascii_lowercase();
                let rank = if lower.contains("hw:card=controller") && lower.contains("dev=0") {
                    4
                } else if lower.contains("dualsense") || lower.contains("wireless controller") {
                    3
                } else if lower.contains("controller") {
                    1
                } else {
                    return None;
                };
                Some((rank, name, device))
            })
            .max_by_key(|(rank, _, _)| *rank);
        let Some((_, device_name, device)) = device else {
            let _ = output.send(VoiceOutput::Status(
                "DualSense microphone not found (is the headset input exposed?)".to_owned(),
            ));
            return;
        };
        let supported_config = match device.default_input_config() {
            Ok(config) => config,
            Err(error) => {
                let _ = output.send(VoiceOutput::Status(format!(
                    "Cannot open {device_name}: {error}"
                )));
                return;
            }
        };
        let sample_rate = supported_config.sample_rate().0;
        let channels = supported_config.channels();
        let config: cpal::StreamConfig = supported_config.clone().into();
        let output_for_build = output.clone();
        let build = move |format| {
            let output = output_for_build.clone();
            match format {
                cpal::SampleFormat::I8 => {
                    let output_sender = output.clone();
                    let commands_sender = commands.clone();
                    let recording_state = Arc::clone(&recording);
                    device.build_input_stream(
                        &config,
                        move |data: &[i8], _| {
                            report_audio(
                                data,
                                sample_rate,
                                channels,
                                &recording_state,
                                &commands_sender,
                                &output_sender,
                            )
                        },
                        move |error| {
                            let _ = output.send(VoiceOutput::Status(format!(
                                "Microphone stream error: {error}"
                            )));
                        },
                        None,
                    )
                }
                cpal::SampleFormat::I16 => {
                    let output_sender = output.clone();
                    let commands_sender = commands.clone();
                    let recording_state = Arc::clone(&recording);
                    device.build_input_stream(
                        &config,
                        move |data: &[i16], _| {
                            report_audio(
                                data,
                                sample_rate,
                                channels,
                                &recording_state,
                                &commands_sender,
                                &output_sender,
                            )
                        },
                        move |error| {
                            let _ = output.send(VoiceOutput::Status(format!(
                                "Microphone stream error: {error}"
                            )));
                        },
                        None,
                    )
                }
                cpal::SampleFormat::I32 => {
                    let output_sender = output.clone();
                    let commands_sender = commands.clone();
                    let recording_state = Arc::clone(&recording);
                    device.build_input_stream(
                        &config,
                        move |data: &[i32], _| {
                            report_audio(
                                data,
                                sample_rate,
                                channels,
                                &recording_state,
                                &commands_sender,
                                &output_sender,
                            )
                        },
                        move |error| {
                            let _ = output.send(VoiceOutput::Status(format!(
                                "Microphone stream error: {error}"
                            )));
                        },
                        None,
                    )
                }
                cpal::SampleFormat::I64 => {
                    let output_sender = output.clone();
                    let commands_sender = commands.clone();
                    let recording_state = Arc::clone(&recording);
                    device.build_input_stream(
                        &config,
                        move |data: &[i64], _| {
                            report_audio(
                                data,
                                sample_rate,
                                channels,
                                &recording_state,
                                &commands_sender,
                                &output_sender,
                            )
                        },
                        move |error| {
                            let _ = output.send(VoiceOutput::Status(format!(
                                "Microphone stream error: {error}"
                            )));
                        },
                        None,
                    )
                }
                cpal::SampleFormat::U8 => {
                    let output_sender = output.clone();
                    let commands_sender = commands.clone();
                    let recording_state = Arc::clone(&recording);
                    device.build_input_stream(
                        &config,
                        move |data: &[u8], _| {
                            report_audio(
                                data,
                                sample_rate,
                                channels,
                                &recording_state,
                                &commands_sender,
                                &output_sender,
                            )
                        },
                        move |error| {
                            let _ = output.send(VoiceOutput::Status(format!(
                                "Microphone stream error: {error}"
                            )));
                        },
                        None,
                    )
                }
                cpal::SampleFormat::U16 => {
                    let output_sender = output.clone();
                    let commands_sender = commands.clone();
                    let recording_state = Arc::clone(&recording);
                    device.build_input_stream(
                        &config,
                        move |data: &[u16], _| {
                            report_audio(
                                data,
                                sample_rate,
                                channels,
                                &recording_state,
                                &commands_sender,
                                &output_sender,
                            )
                        },
                        move |error| {
                            let _ = output.send(VoiceOutput::Status(format!(
                                "Microphone stream error: {error}"
                            )));
                        },
                        None,
                    )
                }
                cpal::SampleFormat::U32 => {
                    let output_sender = output.clone();
                    let commands_sender = commands.clone();
                    let recording_state = Arc::clone(&recording);
                    device.build_input_stream(
                        &config,
                        move |data: &[u32], _| {
                            report_audio(
                                data,
                                sample_rate,
                                channels,
                                &recording_state,
                                &commands_sender,
                                &output_sender,
                            )
                        },
                        move |error| {
                            let _ = output.send(VoiceOutput::Status(format!(
                                "Microphone stream error: {error}"
                            )));
                        },
                        None,
                    )
                }
                cpal::SampleFormat::U64 => {
                    let output_sender = output.clone();
                    let commands_sender = commands.clone();
                    let recording_state = Arc::clone(&recording);
                    device.build_input_stream(
                        &config,
                        move |data: &[u64], _| {
                            report_audio(
                                data,
                                sample_rate,
                                channels,
                                &recording_state,
                                &commands_sender,
                                &output_sender,
                            )
                        },
                        move |error| {
                            let _ = output.send(VoiceOutput::Status(format!(
                                "Microphone stream error: {error}"
                            )));
                        },
                        None,
                    )
                }
                cpal::SampleFormat::F32 => {
                    let output_sender = output.clone();
                    let commands_sender = commands.clone();
                    let recording_state = Arc::clone(&recording);
                    device.build_input_stream(
                        &config,
                        move |data: &[f32], _| {
                            report_audio(
                                data,
                                sample_rate,
                                channels,
                                &recording_state,
                                &commands_sender,
                                &output_sender,
                            )
                        },
                        move |error| {
                            let _ = output.send(VoiceOutput::Status(format!(
                                "Microphone stream error: {error}"
                            )));
                        },
                        None,
                    )
                }
                cpal::SampleFormat::F64 => {
                    let output_sender = output.clone();
                    let commands_sender = commands.clone();
                    let recording_state = Arc::clone(&recording);
                    device.build_input_stream(
                        &config,
                        move |data: &[f64], _| {
                            report_audio(
                                data,
                                sample_rate,
                                channels,
                                &recording_state,
                                &commands_sender,
                                &output_sender,
                            )
                        },
                        move |error| {
                            let _ = output.send(VoiceOutput::Status(format!(
                                "Microphone stream error: {error}"
                            )));
                        },
                        None,
                    )
                }
                format => {
                    let _ = output.send(VoiceOutput::Status(format!(
                        "Unsupported microphone sample format: {format}"
                    )));
                    unreachable!("unsupported microphone sample format")
                }
            }
        };

        let stream = match build(supported_config.sample_format()) {
            Ok(stream) => stream,
            Err(error) => {
                let _ = output.send(VoiceOutput::Status(format!(
                    "Cannot start {device_name}: {error}"
                )));
                return;
            }
        };
        if let Err(error) = stream.play() {
            let _ = output.send(VoiceOutput::Status(format!(
                "Cannot play {device_name}: {error}"
            )));
            return;
        }
        let _ = output.send(VoiceOutput::Status(format!("Listening: {device_name}")));
        loop {
            thread::sleep(Duration::from_secs(1));
        }
    }

    #[derive(Default)]
    struct Resampler {
        input_rate: u32,
        channels: u16,
        buffer: Vec<f32>,
        position: f64,
    }

    impl Resampler {
        fn convert(&mut self, chunk: AudioChunk) -> Vec<i16> {
            if chunk.sample_rate == 0 || chunk.channels == 0 {
                return Vec::new();
            }
            if self.input_rate != chunk.sample_rate || self.channels != chunk.channels {
                *self = Self {
                    input_rate: chunk.sample_rate,
                    channels: chunk.channels,
                    ..Self::default()
                };
            }
            let channels = usize::from(chunk.channels);
            for frame in chunk.samples.chunks_exact(channels) {
                let sum: f32 = frame.iter().copied().sum();
                self.buffer.push((sum / channels as f32).clamp(-1.0, 1.0));
            }

            if self.input_rate == INPUT_RATE {
                let pcm = self.buffer.drain(..).map(float_to_pcm16).collect();
                self.position = 0.0;
                return pcm;
            }

            let step = self.input_rate as f64 / INPUT_RATE as f64;
            let mut pcm = Vec::new();
            while self.position + 1.0 < self.buffer.len() as f64 {
                let index = self.position.floor() as usize;
                let fraction = (self.position - index as f64) as f32;
                let sample =
                    self.buffer[index] + (self.buffer[index + 1] - self.buffer[index]) * fraction;
                pcm.push(float_to_pcm16(sample));
                self.position += step;
            }
            let consumed = self.position.floor() as usize;
            if consumed > 0 {
                self.buffer.drain(..consumed);
                self.position -= consumed as f64;
            }
            pcm
        }
    }

    fn float_to_pcm16(value: f32) -> i16 {
        (value.clamp(-1.0, 1.0) * 32767.0).round() as i16
    }
}

#[cfg(feature = "voice")]
pub use enabled::{VoiceInput, VoiceOutput};

#[cfg(not(feature = "voice"))]
#[derive(Clone, Copy)]
pub struct VoiceInput;

#[cfg(not(feature = "voice"))]
#[allow(dead_code)]
#[derive(Debug)]
pub enum VoiceOutput {
    Transcript(String),
    PartialTranscript(String),
    Status(String),
    MicSample { rms: f32, peak: f32 },
    Light([u8; 3]),
}

#[cfg(not(feature = "voice"))]
impl VoiceInput {
    pub fn new(_: Option<ControllerLight>) -> io::Result<Self> {
        Ok(Self)
    }

    pub fn handle(&self, _: ControllerEvent) {}

    pub fn spawn_microphone(&self) {}

    pub fn try_recv(&self) -> Option<VoiceOutput> {
        None
    }

    pub fn type_partial(&self, _: &str) -> io::Result<bool> {
        Ok(false)
    }

    pub fn type_final(&self, _: &str) -> io::Result<bool> {
        Ok(false)
    }
}
