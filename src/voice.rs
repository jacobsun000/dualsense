//! Push-to-talk voice input for the DualSense microphone.
//!
//! Holding the right face button (○) captures the controller microphone, streams 24 kHz PCM audio to
//! OpenAI's realtime transcription session, and returns transcript updates to the
//! input reader. Text is entered through Wayland with Unicode support.

use crate::input::ControllerEvent;
#[cfg(feature = "voice")]
use crate::input::{Button, ButtonState, ControllerEventKind};
use crate::light::ControllerLight;
use anyhow::Result;

#[cfg(feature = "voice")]
mod enabled {
    use super::*;
    use anyhow::anyhow;
    use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
    use futures_util::{SinkExt, StreamExt};
    use serde_json::{Value, json};
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
    use wrtype::WrtypeClient;

    const INPUT_RATE: u32 = 24_000;
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

    #[derive(Default)]
    struct TextInput {
        client: Option<WrtypeClient>,
        attempted: bool,
        partial_seen: bool,
        partial_typed: bool,
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
                    let display =
                        std::env::var("WAYLAND_DISPLAY").unwrap_or_else(|_| "<unset>".to_owned());
                    let session =
                        std::env::var("XDG_SESSION_TYPE").unwrap_or_else(|_| "<unset>".to_owned());
                    let _ = output.send(VoiceOutput::Status(format!(
                        "Wayland virtual keyboard: available (WAYLAND_DISPLAY={display}, XDG_SESSION_TYPE={session})"
                    )));
                    true
                }
                Err(error) => {
                    let display =
                        std::env::var("WAYLAND_DISPLAY").unwrap_or_else(|_| "<unset>".to_owned());
                    let session =
                        std::env::var("XDG_SESSION_TYPE").unwrap_or_else(|_| "<unset>".to_owned());
                    let _ = output.send(VoiceOutput::Status(format!(
                        "Wayland virtual keyboard: unavailable (WAYLAND_DISPLAY={display}, XDG_SESSION_TYPE={session}); deferring partials until the final transcript: {error}"
                    )));
                    false
                }
            }
        }

        fn begin_turn(&mut self) {
            self.partial_seen = false;
            self.partial_typed = false;
        }

        fn type_text(&mut self, text: &str, output: &mpsc::Sender<VoiceOutput>) -> Result<bool> {
            if !self.ensure_client(output) {
                return Ok(false);
            }
            let Some(client) = self.client.as_mut() else {
                return Ok(false);
            };
            // `text_input` is locked for the whole call by each public typing
            // method, so multiple partials and the final transcript are sent
            // synchronously and in order.
            client
                .type_text(text)
                .map(|()| true)
                .map_err(|error| anyhow!(error))
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
        pub fn new(light: Option<ControllerLight>) -> Result<Self> {
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
                .map_err(|error| anyhow!(error))?;

            let input = Self {
                commands,
                recording,
                outputs: Arc::new(Mutex::new(output_receiver)),
                output_sender,
                microphone_started: Arc::new(AtomicBool::new(false)),
                text_input: Arc::new(Mutex::new(TextInput::default())),
                light,
            };
            // Probe the compositor during startup so direct mode reports the
            // actual text-input transport before the first voice turn. The
            // successful client is retained for subsequent partials.
            input.probe_text_input();
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
            self.outputs
                .lock()
                .ok()
                .and_then(|receiver| receiver.try_recv().ok())
        }

        fn probe_text_input(&self) {
            let Ok(mut input) = self.text_input.lock() else {
                let _ = self.output_sender.send(VoiceOutput::Status(
                    "Voice text-input state is unavailable".to_owned(),
                ));
                return;
            };
            let _ = input.ensure_client(&self.output_sender);
        }

        fn begin_text_turn(&self) {
            let Ok(mut input) = self.text_input.lock() else {
                let _ = self.output_sender.send(VoiceOutput::Status(
                    "Voice text-input state is unavailable".to_owned(),
                ));
                return;
            };
            input.begin_turn();
        }

        /// Deliver a transcript delta through the ordered virtual keyboard.
        ///
        /// If wrtype is unavailable, the delta is deferred and the final
        /// transcript is attempted once through the normal fallback path.
        pub fn type_partial(&self, text: &str) -> Result<bool> {
            let mut input = self
                .text_input
                .lock()
                .map_err(|_| anyhow!("voice text-input state is unavailable"))?;

            // The text-input mutex is held while wrtype sends this complete
            // delta, preventing concurrent partials from overtaking one another.
            input.partial_seen = true;
            match input.type_text(text, &self.output_sender)? {
                true => {
                    input.partial_typed = true;
                    Ok(true)
                }
                false => {
                    // Defer the delta rather than asking the raw uinput
                    // mapper to type it and then duplicating it at completion.
                    Ok(true)
                }
            }
        }

        /// Complete a transcript turn without duplicating streamed deltas.
        ///
        /// When virtual-keyboard delivery was unavailable, partials were
        /// deferred and this final transcript is delivered once through wrtype
        /// (or the raw uinput fallback). When all partials were typed, the
        /// final API event is only an acknowledgement.
        pub fn type_final(&self, text: &str) -> Result<bool> {
            let mut input = self
                .text_input
                .lock()
                .map_err(|_| anyhow!("voice text-input state is unavailable"))?;
            if input.partial_seen {
                let partial_typed = input.partial_typed;
                input.partial_seen = false;
                input.partial_typed = false;
                if partial_typed {
                    return Ok(true);
                }

                return input.type_text(text, &self.output_sender);
            }

            input.type_text(text, &self.output_sender)
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

    async fn connect_session(api_key: &str) -> Result<Socket> {
        let mut request = "wss://api.openai.com/v1/realtime?intent=transcription"
            .into_client_request()
            .map_err(|error| anyhow!(error))?;
        let authorization = format!("Bearer {api_key}")
            .parse()
            .map_err(|error| anyhow!("invalid authorization header: {error}"))?;
        request.headers_mut().insert("Authorization", authorization);
        let (mut socket, _) = connect_async(request)
            .await
            .map_err(|error| anyhow!(error))?;
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
            .map_err(|error| anyhow!(error))?;
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

    fn run_microphone(
        recording: Arc<AtomicBool>,
        commands: tokio_mpsc::UnboundedSender<WorkerMessage>,
        output: mpsc::Sender<VoiceOutput>,
    ) {
        let host = cpal::default_host();
        // Do not redirect the process-wide stderr while enumerating ALSA
        // devices. The microphone runs on its own thread, so doing that can
        // hide direct-mode diagnostics printed by the input reader, including
        // the Wayland virtual-keyboard availability probe.
        let devices = match host.devices() {
            Ok(devices) => devices.collect::<Vec<_>>(),
            Err(error) => {
                let _ = output.send(VoiceOutput::Status(format!(
                    "Microphone unavailable: {error}"
                )));
                return;
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
    pub fn new(_: Option<ControllerLight>) -> Result<Self> {
        Ok(Self)
    }

    pub fn handle(&self, _: ControllerEvent) {}

    pub fn spawn_microphone(&self) {}

    pub fn try_recv(&self) -> Option<VoiceOutput> {
        None
    }

    pub fn type_partial(&self, _: &str) -> Result<bool> {
        Ok(false)
    }

    pub fn type_final(&self, _: &str) -> Result<bool> {
        Ok(false)
    }
}
