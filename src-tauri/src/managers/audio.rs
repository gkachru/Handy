use crate::audio_toolkit::{list_input_devices, vad::SmoothedVad, AudioRecorder, SileroVad};
use crate::helpers::clamshell;
use crate::settings::{get_settings, AppSettings, AudioSource};
use crate::utils;
use log::{debug, error, info, warn};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};
use tauri::Manager;

#[cfg(target_os = "macos")]
use crate::audio_toolkit::system_audio::SystemAudioCapture;

const STREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(30);

fn set_mute(mute: bool) {
    // Expected behavior:
    // - Windows: works on most systems using standard audio drivers.
    // - Linux: works on many systems (PipeWire, PulseAudio, ALSA),
    //   but some distros may lack the tools used.
    // - macOS: works on most standard setups via AppleScript.
    // If unsupported, fails silently.

    #[cfg(target_os = "windows")]
    {
        unsafe {
            use windows::Win32::{
                Media::Audio::{
                    eMultimedia, eRender, Endpoints::IAudioEndpointVolume, IMMDeviceEnumerator,
                    MMDeviceEnumerator,
                },
                System::Com::{CoCreateInstance, CoInitializeEx, CLSCTX_ALL, COINIT_MULTITHREADED},
            };

            macro_rules! unwrap_or_return {
                ($expr:expr) => {
                    match $expr {
                        Ok(val) => val,
                        Err(_) => return,
                    }
                };
            }

            // Initialize the COM library for this thread.
            // If already initialized (e.g., by another library like Tauri), this does nothing.
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);

            let all_devices: IMMDeviceEnumerator =
                unwrap_or_return!(CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL));
            let default_device =
                unwrap_or_return!(all_devices.GetDefaultAudioEndpoint(eRender, eMultimedia));
            let volume_interface = unwrap_or_return!(
                default_device.Activate::<IAudioEndpointVolume>(CLSCTX_ALL, None)
            );

            let _ = volume_interface.SetMute(mute, std::ptr::null());
        }
    }

    #[cfg(target_os = "linux")]
    {
        use std::process::Command;

        let mute_val = if mute { "1" } else { "0" };
        let amixer_state = if mute { "mute" } else { "unmute" };

        // Try multiple backends to increase compatibility
        // 1. PipeWire (wpctl)
        if Command::new("wpctl")
            .args(["set-mute", "@DEFAULT_AUDIO_SINK@", mute_val])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
        {
            return;
        }

        // 2. PulseAudio (pactl)
        if Command::new("pactl")
            .args(["set-sink-mute", "@DEFAULT_SINK@", mute_val])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
        {
            return;
        }

        // 3. ALSA (amixer)
        let _ = Command::new("amixer")
            .args(["set", "Master", amixer_state])
            .output();
    }

    #[cfg(target_os = "macos")]
    {
        use std::process::Command;
        let script = format!(
            "set volume output muted {}",
            if mute { "true" } else { "false" }
        );
        let _ = Command::new("osascript").args(["-e", &script]).output();
    }
}

const WHISPER_SAMPLE_RATE: usize = 16000;

/* ──────────────────────────────────────────────────────────────── */

#[derive(Clone, Debug)]
pub enum RecordingState {
    Idle,
    Recording { binding_id: String },
}

#[derive(Clone, Debug)]
pub enum MicrophoneMode {
    AlwaysOn,
    OnDemand,
}

/* ──────────────────────────────────────────────────────────────── */

fn create_audio_recorder(
    vad_path: &str,
    app_handle: &tauri::AppHandle,
) -> Result<AudioRecorder, anyhow::Error> {
    let silero = SileroVad::new(vad_path, 0.3)
        .map_err(|e| anyhow::anyhow!("Failed to create SileroVad: {}", e))?;
    let smoothed_vad = SmoothedVad::new(Box::new(silero), 15, 15, 2);

    // Recorder with VAD plus a spectrum-level callback that forwards updates to
    // the frontend.
    let recorder = AudioRecorder::new()
        .map_err(|e| anyhow::anyhow!("Failed to create AudioRecorder: {}", e))?
        .with_vad(Box::new(smoothed_vad))
        .with_level_callback({
            let app_handle = app_handle.clone();
            move |levels| {
                utils::emit_levels(&app_handle, &levels);
            }
        });

    Ok(recorder)
}

/* ──────────────────────────────────────────────────────────────── */

#[derive(Clone)]
pub struct AudioRecordingManager {
    state: Arc<Mutex<RecordingState>>,
    mode: Arc<Mutex<MicrophoneMode>>,
    app_handle: tauri::AppHandle,

    recorder: Arc<Mutex<Option<AudioRecorder>>>,
    is_open: Arc<Mutex<bool>>,
    is_recording: Arc<Mutex<bool>>,
    did_mute: Arc<Mutex<bool>>,

    audio_source: Arc<Mutex<AudioSource>>,
    buffered_samples: Arc<Mutex<Vec<f32>>>,
    /// Persistent sender for the audio streaming bridge channel.
    /// Survives recorder teardown/restart so external consumers (e.g.
    /// MistralRealtimeSession) never see a channel disconnect during
    /// mid-recording source/device switches.
    streaming_bridge_tx: Arc<Mutex<Option<mpsc::Sender<Vec<f32>>>>>,
    #[cfg(target_os = "macos")]
    system_capture: Arc<Mutex<Option<SystemAudioCapture>>>,
    close_generation: Arc<AtomicU64>,
}

impl AudioRecordingManager {
    /* ---------- construction ------------------------------------------------ */

    pub fn new(app: &tauri::AppHandle) -> Result<Self, anyhow::Error> {
        let settings = get_settings(app);
        let mode = if settings.always_on_microphone {
            MicrophoneMode::AlwaysOn
        } else {
            MicrophoneMode::OnDemand
        };

        let manager = Self {
            state: Arc::new(Mutex::new(RecordingState::Idle)),
            mode: Arc::new(Mutex::new(mode.clone())),
            app_handle: app.clone(),

            recorder: Arc::new(Mutex::new(None)),
            is_open: Arc::new(Mutex::new(false)),
            is_recording: Arc::new(Mutex::new(false)),
            did_mute: Arc::new(Mutex::new(false)),

            audio_source: Arc::new(Mutex::new(settings.audio_source)),
            buffered_samples: Arc::new(Mutex::new(Vec::new())),
            streaming_bridge_tx: Arc::new(Mutex::new(None)),
            #[cfg(target_os = "macos")]
            system_capture: Arc::new(Mutex::new(None)),
            close_generation: Arc::new(AtomicU64::new(0)),
        };

        // Always-on?  Open immediately.
        if matches!(mode, MicrophoneMode::AlwaysOn) {
            manager.start_microphone_stream()?;
        }

        Ok(manager)
    }

    /* ---------- helper methods --------------------------------------------- */

    fn get_effective_microphone_device(&self, settings: &AppSettings) -> Option<cpal::Device> {
        // Check if we're in clamshell mode and have a clamshell microphone configured
        let use_clamshell_mic = if let Ok(is_clamshell) = clamshell::is_clamshell() {
            is_clamshell && settings.clamshell_microphone.is_some()
        } else {
            false
        };

        let device_name = if use_clamshell_mic {
            settings.clamshell_microphone.as_ref().unwrap()
        } else {
            settings.selected_microphone.as_ref()?
        };

        // Find the device by name
        match list_input_devices() {
            Ok(devices) => devices
                .into_iter()
                .find(|d| d.name == *device_name)
                .map(|d| d.device),
            Err(e) => {
                debug!("Failed to list devices, using default: {}", e);
                None
            }
        }
    }

    fn schedule_lazy_close(&self) {
        let gen = self.close_generation.fetch_add(1, Ordering::SeqCst) + 1;
        let app = self.app_handle.clone();
        std::thread::spawn(move || {
            std::thread::sleep(STREAM_IDLE_TIMEOUT);
            let rm = app.state::<Arc<AudioRecordingManager>>();
            // Hold state lock across the check AND close to serialize against
            // try_start_recording, preventing a race where the stream is closed
            // under an active recording.
            let state = rm.state.lock().unwrap();
            if rm.close_generation.load(Ordering::SeqCst) == gen
                && matches!(*state, RecordingState::Idle)
            {
                // stop_microphone_stream does not acquire the state lock,
                // so holding it here is safe (no deadlock).
                info!(
                    "Closing idle microphone stream after {:?}",
                    STREAM_IDLE_TIMEOUT
                );
                rm.stop_microphone_stream();
            }
        });
    }

    /* ---------- microphone life-cycle -------------------------------------- */

    /// Applies mute if mute_while_recording is enabled and stream is open.
    /// Only mutes system audio in microphone-only mode — when capturing
    /// system audio (alone or mixed), muting would silence the source.
    pub fn apply_mute(&self) {
        let settings = get_settings(&self.app_handle);
        let audio_source = *self.audio_source.lock().unwrap();
        let is_open = *self.is_open.lock().unwrap();

        if settings.mute_while_recording
            && audio_source == AudioSource::Microphone
            && is_open
        {
            set_mute(true);
            *self.did_mute.lock().unwrap() = true;
            debug!("Mute applied");
        }
    }

    /// Ensures system audio output is unmuted. Used in dual-stream mode where
    /// we need system audio to be audible for the process tap to capture it.
    pub fn ensure_unmuted(&self) {
        set_mute(false);
        *self.did_mute.lock().unwrap() = false;
        debug!("System audio explicitly unmuted for dual-stream mode");
    }

    /// Removes mute if it was applied
    pub fn remove_mute(&self) {
        let mut did_mute_guard = self.did_mute.lock().unwrap();
        if *did_mute_guard {
            set_mute(false);
            *did_mute_guard = false;
            debug!("Mute removed");
        }
    }

    pub fn preload_vad(&self) -> Result<(), anyhow::Error> {
        let mut recorder_opt = self.recorder.lock().unwrap();
        if recorder_opt.is_none() {
            let vad_path = self
                .app_handle
                .path()
                .resolve(
                    "resources/models/silero_vad_v4.onnx",
                    tauri::path::BaseDirectory::Resource,
                )
                .map_err(|e| anyhow::anyhow!("Failed to resolve VAD path: {}", e))?;
            *recorder_opt = Some(create_audio_recorder(
                vad_path.to_str().unwrap(),
                &self.app_handle,
            )?);
        }
        Ok(())
    }

    pub fn start_microphone_stream(&self) -> Result<(), anyhow::Error> {
        let audio_source = *self.audio_source.lock().unwrap();
        match audio_source {
            AudioSource::Microphone | AudioSource::Mixed => self.start_microphone_stream_inner(),
            AudioSource::SystemAudio => self.start_system_audio_stream(),
        }
    }

    fn start_microphone_stream_inner(&self) -> Result<(), anyhow::Error> {
        if *self.is_open.lock().unwrap() {
            debug!("Microphone stream already active");
            return Ok(());
        }

        let start_time = Instant::now();

        // Don't mute immediately - caller will handle muting after audio feedback
        *self.did_mute.lock().unwrap() = false;

        let vad_path = self
            .app_handle
            .path()
            .resolve(
                "resources/models/silero_vad_v4.onnx",
                tauri::path::BaseDirectory::Resource,
            )
            .map_err(|e| anyhow::anyhow!("Failed to resolve VAD path: {}", e))?;

        {
            let mut recorder_opt = self.recorder.lock().unwrap();
            if recorder_opt.is_none() {
                *recorder_opt = Some(create_audio_recorder(
                    vad_path.to_str().unwrap(),
                    &self.app_handle,
                )?);
            }

            // Get the selected device from settings, considering clamshell mode
            let settings = get_settings(&self.app_handle);
            let selected_device = self.get_effective_microphone_device(&settings);

            // Pre-flight check: if no device was selected/configured AND no devices
            // exist at all, fail early with a clear error instead of letting cpal
            // produce a cryptic backend-specific message.
            if selected_device.is_none() {
                let has_any_device = list_input_devices()
                    .map(|devices| !devices.is_empty())
                    .unwrap_or(false);
                if !has_any_device {
                    return Err(anyhow::anyhow!("No input device found"));
                }
            }

            if let Some(rec) = recorder_opt.as_mut() {
                rec.open(selected_device)
                    .map_err(|e| anyhow::anyhow!("Failed to open recorder: {}", e))?;
            }
        }

        *self.is_open.lock().unwrap() = true;
        info!(
            "Microphone stream initialized in {:?}",
            start_time.elapsed()
        );
        Ok(())
    }

    /// Start system audio capture via CoreAudio process tap (macOS 14.2+ only).
    /// On non-macOS or older macOS, falls back to microphone.
    fn start_system_audio_stream(&self) -> Result<(), anyhow::Error> {
        #[cfg(target_os = "macos")]
        {
            use crate::audio_toolkit::system_audio;

            if !system_audio::is_available() {
                warn!("System audio capture not available (macOS 14.2+ required), falling back to microphone");
                return self.start_microphone_stream_inner();
            }

            if *self.is_open.lock().unwrap() {
                debug!("Audio stream already active");
                return Ok(());
            }

            let start_time = Instant::now();

            // Ensure system audio output is unmuted — we need to capture it
            set_mute(false);
            *self.did_mute.lock().unwrap() = false;

            // Start CoreAudio process tap (lock released before opening recorder)
            let (sample_rx, sample_rate) = {
                let mut sys_capture = self.system_capture.lock().unwrap();
                if sys_capture.is_none() {
                    *sys_capture = Some(SystemAudioCapture::new());
                }
                sys_capture
                    .as_mut()
                    .unwrap()
                    .start()
                    .map_err(|e| anyhow::anyhow!("Failed to start system audio: {}", e))?
            };

            // Create recorder and feed it from the system audio source
            let vad_path = self
                .app_handle
                .path()
                .resolve(
                    "resources/models/silero_vad_v4.onnx",
                    tauri::path::BaseDirectory::Resource,
                )
                .map_err(|e| anyhow::anyhow!("Failed to resolve VAD path: {}", e))?;

            {
                let mut recorder_opt = self.recorder.lock().unwrap();
                if recorder_opt.is_none() {
                    *recorder_opt = Some(create_audio_recorder(
                        vad_path.to_str().unwrap(),
                        &self.app_handle,
                    )?);
                }

                if let Some(rec) = recorder_opt.as_mut() {
                    rec.open_with_external_source(sample_rx, sample_rate)
                        .map_err(|e| {
                            anyhow::anyhow!("Failed to open recorder with system audio: {}", e)
                        })?;
                }
            }

            *self.is_open.lock().unwrap() = true;
            info!(
                "System audio stream initialized in {:?}",
                start_time.elapsed()
            );
            Ok(())
        }

        #[cfg(not(target_os = "macos"))]
        {
            warn!("System audio capture is only available on macOS, falling back to microphone");
            self.start_microphone_stream_inner()
        }
    }

    pub fn stop_microphone_stream(&self) {
        if !*self.is_open.lock().unwrap() {
            return;
        }

        {
            let mut did_mute_guard = self.did_mute.lock().unwrap();
            if *did_mute_guard {
                set_mute(false);
            }
            *did_mute_guard = false;
        }

        // Stop system audio capture FIRST so the recorder's audio source
        // disconnects, allowing its worker thread to exit cleanly.
        #[cfg(target_os = "macos")]
        {
            if let Some(capture) = self.system_capture.lock().unwrap().as_mut() {
                capture.stop();
            }
        }

        // Close the recorder (may block on h.join — no other locks held)
        if let Some(rec) = self.recorder.lock().unwrap().as_mut() {
            if *self.is_recording.lock().unwrap() {
                let _ = rec.stop();
                *self.is_recording.lock().unwrap() = false;
            }
            let _ = rec.close();
        }

        *self.is_open.lock().unwrap() = false;
        debug!("Audio stream stopped");
    }

    /// Update the audio source setting and restart streams if needed.
    /// If a recording is in progress, buffers the accumulated samples so they
    /// are preserved across the source switch.
    pub fn update_audio_source(&self, source: AudioSource) -> Result<(), anyhow::Error> {
        let current = *self.audio_source.lock().unwrap();
        if current == source {
            return Ok(());
        }

        info!("Changing audio source from {:?} to {:?}", current, source);
        *self.audio_source.lock().unwrap() = source;

        if !*self.is_open.lock().unwrap() {
            return Ok(());
        }

        let was_recording = *self.is_recording.lock().unwrap();

        if was_recording {
            // Collect accumulated samples from the current consumer
            if let Some(rec) = self.recorder.lock().unwrap().as_ref() {
                match rec.stop() {
                    Ok(samples) => {
                        self.buffered_samples.lock().unwrap().extend(samples);
                        info!("Buffered samples during audio source switch");
                    }
                    Err(e) => {
                        warn!("Failed to collect samples during source switch: {e}");
                    }
                }
            }
            // Prevent stop_microphone_stream from calling rec.stop() again
            *self.is_recording.lock().unwrap() = false;
        }

        self.stop_microphone_stream();
        self.start_microphone_stream()?;
        self.reconnect_audio_streaming();

        if was_recording {
            // Resume recording on the new consumer
            if let Some(rec) = self.recorder.lock().unwrap().as_ref() {
                if let Err(e) = rec.start() {
                    error!("Failed to resume recording after source switch: {e}");
                    self.buffered_samples.lock().unwrap().clear();
                    return Err(anyhow::anyhow!("Failed to resume recording: {e}"));
                }
            }
            *self.is_recording.lock().unwrap() = true;
            self.apply_mute();
        }

        Ok(())
    }

    /* ---------- mode switching --------------------------------------------- */

    pub fn update_mode(&self, new_mode: MicrophoneMode) -> Result<(), anyhow::Error> {
        let cur_mode = self.mode.lock().unwrap().clone();

        match (cur_mode, &new_mode) {
            (MicrophoneMode::AlwaysOn, MicrophoneMode::OnDemand) => {
                if matches!(*self.state.lock().unwrap(), RecordingState::Idle) {
                    self.close_generation.fetch_add(1, Ordering::SeqCst);
                    self.stop_microphone_stream();
                }
            }
            (MicrophoneMode::OnDemand, MicrophoneMode::AlwaysOn) => {
                self.close_generation.fetch_add(1, Ordering::SeqCst);
                self.start_microphone_stream()?;
            }
            _ => {}
        }

        *self.mode.lock().unwrap() = new_mode;
        Ok(())
    }

    /* ---------- recording --------------------------------------------------- */

    pub fn try_start_recording(&self, binding_id: &str) -> Result<(), String> {
        let mut state = self.state.lock().unwrap();

        if let RecordingState::Idle = *state {
            // Ensure microphone is open in on-demand mode
            if matches!(*self.mode.lock().unwrap(), MicrophoneMode::OnDemand) {
                // Cancel any pending lazy close
                self.close_generation.fetch_add(1, Ordering::SeqCst);
                if let Err(e) = self.start_microphone_stream() {
                    let msg = format!("{e}");
                    error!("Failed to open microphone stream: {msg}");
                    return Err(msg);
                }
            }

            if let Some(rec) = self.recorder.lock().unwrap().as_ref() {
                if rec.start().is_ok() {
                    *self.is_recording.lock().unwrap() = true;
                    *state = RecordingState::Recording {
                        binding_id: binding_id.to_string(),
                    };
                    debug!("Recording started for binding {binding_id}");
                    return Ok(());
                }
            }
            Err("Recorder not available".to_string())
        } else {
            Err("Already recording".to_string())
        }
    }

    /// Update the selected microphone device and restart streams if needed.
    /// If a recording is in progress, buffers the accumulated samples so they
    /// are preserved across the device switch.
    pub fn update_selected_device(&self) -> Result<(), anyhow::Error> {
        if !*self.is_open.lock().unwrap() {
            return Ok(());
        }

        let was_recording = *self.is_recording.lock().unwrap();

        if was_recording {
            if let Some(rec) = self.recorder.lock().unwrap().as_ref() {
                match rec.stop() {
                    Ok(samples) => {
                        self.buffered_samples.lock().unwrap().extend(samples);
                        info!("Buffered samples during device switch");
                    }
                    Err(e) => {
                        warn!("Failed to collect samples during device switch: {e}");
                    }
                }
            }
            *self.is_recording.lock().unwrap() = false;
        }

        self.stop_microphone_stream();
        self.start_microphone_stream()?;
        self.reconnect_audio_streaming();

        if was_recording {
            if let Some(rec) = self.recorder.lock().unwrap().as_ref() {
                if let Err(e) = rec.start() {
                    error!("Failed to resume recording after device switch: {e}");
                    self.buffered_samples.lock().unwrap().clear();
                    return Err(anyhow::anyhow!("Failed to resume recording: {e}"));
                }
            }
            *self.is_recording.lock().unwrap() = true;
            self.apply_mute();
        }

        Ok(())
    }

    pub fn stop_recording(&self, binding_id: &str) -> Option<Vec<f32>> {
        let mut state = self.state.lock().unwrap();

        match *state {
            RecordingState::Recording {
                binding_id: ref active,
            } if active == binding_id => {
                *state = RecordingState::Idle;
                drop(state);

                // Optionally keep recording for a bit longer to capture trailing audio
                let settings = get_settings(&self.app_handle);
                if settings.extra_recording_buffer_ms > 0 {
                    debug!(
                        "Extra recording buffer: sleeping {}ms before stopping",
                        settings.extra_recording_buffer_ms
                    );
                    std::thread::sleep(Duration::from_millis(settings.extra_recording_buffer_ms));
                }

                let current_samples = if let Some(rec) = self.recorder.lock().unwrap().as_ref() {
                    match rec.stop() {
                        Ok(buf) => buf,
                        Err(e) => {
                            error!("stop() failed: {e}");
                            Vec::new()
                        }
                    }
                } else {
                    error!("Recorder not available");
                    Vec::new()
                };

                // Prepend any samples buffered from mid-recording source/device switches
                let mut buffered = std::mem::take(&mut *self.buffered_samples.lock().unwrap());
                let samples = if buffered.is_empty() {
                    current_samples
                } else {
                    buffered.extend(current_samples);
                    buffered
                };

                *self.is_recording.lock().unwrap() = false;

                // In on-demand mode, close the mic (lazily if the setting is enabled)
                if matches!(*self.mode.lock().unwrap(), MicrophoneMode::OnDemand) {
                    if get_settings(&self.app_handle).lazy_stream_close {
                        self.schedule_lazy_close();
                    } else {
                        self.stop_microphone_stream();
                    }
                }

                // Pad if very short
                let s_len = samples.len();
                // debug!("Got {} samples", s_len);
                if s_len < WHISPER_SAMPLE_RATE && s_len > 0 {
                    let mut padded = samples;
                    padded.resize(WHISPER_SAMPLE_RATE * 5 / 4, 0.0);
                    Some(padded)
                } else {
                    Some(samples)
                }
            }
            _ => None,
        }
    }
    pub fn enable_audio_streaming(&self) -> Option<mpsc::Receiver<Vec<f32>>> {
        if let Some(rec) = self.recorder.lock().unwrap().as_ref() {
            if let Ok(recorder_rx) = rec.enable_streaming() {
                let (bridge_tx, bridge_rx) = mpsc::channel();
                let tx = bridge_tx.clone();
                std::thread::spawn(move || {
                    while let Ok(samples) = recorder_rx.recv() {
                        if tx.send(samples).is_err() {
                            break;
                        }
                    }
                });
                *self.streaming_bridge_tx.lock().unwrap() = Some(bridge_tx);
                return Some(bridge_rx);
            }
        }
        None
    }

    pub fn disable_audio_streaming(&self) {
        *self.streaming_bridge_tx.lock().unwrap() = None;
        if let Some(rec) = self.recorder.lock().unwrap().as_ref() {
            let _ = rec.disable_streaming();
        }
    }

    /// Stop the system audio capture if active, disconnecting the recorder's
    /// audio source so its consumer thread can exit.
    pub fn stop_system_audio_capture(&self) {
        #[cfg(target_os = "macos")]
        {
            if let Some(capture) = self.system_capture.lock().unwrap().as_mut() {
                capture.stop();
            }
        }
    }

    /// Re-establish the forwarding thread from the current recorder's streaming
    /// output to the existing bridge channel. Called after a mid-recording
    /// source/device switch so external consumers keep receiving audio.
    fn reconnect_audio_streaming(&self) {
        let bridge_guard = self.streaming_bridge_tx.lock().unwrap();
        if let Some(ref tx) = *bridge_guard {
            let tx = tx.clone();
            drop(bridge_guard);
            if let Some(rec) = self.recorder.lock().unwrap().as_ref() {
                if let Ok(recorder_rx) = rec.enable_streaming() {
                    std::thread::spawn(move || {
                        while let Ok(samples) = recorder_rx.recv() {
                            if tx.send(samples).is_err() {
                                break;
                            }
                        }
                    });
                }
            }
        }
    }

    pub fn is_recording(&self) -> bool {
        matches!(
            *self.state.lock().unwrap(),
            RecordingState::Recording { .. }
        )
    }

    /// Cancel any ongoing recording without returning audio samples
    pub fn cancel_recording(&self) {
        let mut state = self.state.lock().unwrap();

        if let RecordingState::Recording { .. } = *state {
            *state = RecordingState::Idle;
            drop(state);

            if let Some(rec) = self.recorder.lock().unwrap().as_ref() {
                let _ = rec.stop(); // Discard the result
            }

            // Discard any samples buffered from mid-recording source/device switches
            self.buffered_samples.lock().unwrap().clear();

            *self.is_recording.lock().unwrap() = false;

            // In on-demand mode, close the mic (lazily if the setting is enabled)
            if matches!(*self.mode.lock().unwrap(), MicrophoneMode::OnDemand) {
                if get_settings(&self.app_handle).lazy_stream_close {
                    self.schedule_lazy_close();
                } else {
                    self.stop_microphone_stream();
                }
            }
        }
    }
}
