use crate::audio_toolkit::vad::SmoothedVad;
use crate::audio_toolkit::SileroVad;
use crate::managers::transcription::TranscriptionManager;
use log::{debug, error, info, warn};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use tauri::{AppHandle, Emitter};

const WHISPER_SAMPLE_RATE: usize = 16000;

/// Manages a VAD-segmented incremental transcription session for fast local models.
///
/// Audio flows: AudioRecorder (16kHz f32 30ms frames) → mpsc::Receiver → this module
/// → SmoothedVad detects segment boundaries → transcribe each segment on pause
/// → emits "streaming-transcription-update" events to the frontend overlay
///
/// On stop, the caller should still do a final full-buffer transcription for best quality.
/// The segment results shown during recording are just a live preview.
pub struct IncrementalTranscriptionSession {
    stop_signal: Arc<AtomicBool>,
    worker_handle: Option<std::thread::JoinHandle<()>>,
    accumulated_text: Arc<Mutex<String>>,
}

impl IncrementalTranscriptionSession {
    /// Start a new incremental transcription session.
    ///
    /// Spawns a worker thread that:
    /// 1. Receives audio frames from the recorder
    /// 2. Runs them through a SmoothedVad to detect speech segment boundaries
    /// 3. Transcribes each completed segment
    /// 4. Emits live preview text to the streaming overlay
    pub fn start(
        app_handle: AppHandle,
        transcription_manager: Arc<TranscriptionManager>,
        audio_rx: mpsc::Receiver<Vec<f32>>,
        vad_model_path: String,
    ) -> Self {
        let stop_signal = Arc::new(AtomicBool::new(false));
        let accumulated_text = Arc::new(Mutex::new(String::new()));

        let stop = stop_signal.clone();
        let text = accumulated_text.clone();

        // Mark transcription manager as having an active session
        transcription_manager.set_active_session(true);

        let tm = transcription_manager.clone();
        let worker_handle = std::thread::spawn(move || {
            Self::worker_loop(app_handle, tm, audio_rx, vad_model_path, stop, text);
        });

        Self {
            stop_signal,
            worker_handle: Some(worker_handle),
            accumulated_text,
        }
    }

    /// Stop the session: signal the worker and wait for it to finish.
    pub fn stop(&mut self, transcription_manager: &TranscriptionManager) {
        self.stop_signal.store(true, Ordering::Relaxed);

        if let Some(handle) = self.worker_handle.take() {
            if let Err(e) = handle.join() {
                error!(
                    "Failed to join incremental transcription worker thread: {:?}",
                    e
                );
            }
        }

        // Clear the active session flag so model unloading can proceed
        transcription_manager.set_active_session(false);
    }

    /// Get the accumulated preview text from all completed segments.
    pub fn get_accumulated_text(&self) -> String {
        self.accumulated_text.lock().unwrap().clone()
    }

    fn worker_loop(
        app: AppHandle,
        tm: Arc<TranscriptionManager>,
        audio_rx: mpsc::Receiver<Vec<f32>>,
        vad_model_path: String,
        stop_signal: Arc<AtomicBool>,
        accumulated_text: Arc<Mutex<String>>,
    ) {
        // Create a second SmoothedVad instance for segment boundary detection
        let silero = match SileroVad::new(&vad_model_path, 0.3) {
            Ok(v) => v,
            Err(e) => {
                error!("Failed to create SileroVad for incremental session: {}", e);
                return;
            }
        };
        let mut vad = SmoothedVad::new(Box::new(silero), 15, 15, 2);

        let mut segment_buffer: Vec<f32> = Vec::new();
        let mut was_speech = false;
        let is_transcribing = Arc::new(AtomicBool::new(false));

        info!("Incremental transcription worker started");

        loop {
            if stop_signal.load(Ordering::Relaxed) {
                debug!("Incremental transcription worker: stop signal received");
                break;
            }

            // Receive audio frames with a timeout to allow checking the stop signal
            let frame = match audio_rx.recv_timeout(std::time::Duration::from_millis(50)) {
                Ok(f) => f,
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    info!("Incremental transcription worker: audio channel disconnected");
                    break;
                }
            };

            // Push each frame through VAD
            use crate::audio_toolkit::vad::VadFrame;
            use crate::audio_toolkit::VoiceActivityDetector;

            match vad.push_frame(&frame) {
                Ok(VadFrame::Speech(samples)) => {
                    segment_buffer.extend_from_slice(samples);
                    was_speech = true;
                }
                Ok(VadFrame::Noise) => {
                    if was_speech && !segment_buffer.is_empty() {
                        // Segment boundary detected: speech → silence transition
                        was_speech = false;

                        // Check if a previous segment is still being transcribed
                        if is_transcribing.load(Ordering::Relaxed) {
                            // Keep accumulating — don't dispatch yet.
                            // The segment buffer keeps growing until transcription finishes.
                            debug!(
                                "Incremental: segment boundary detected but previous transcription still running, merging ({} samples)",
                                segment_buffer.len()
                            );
                            continue;
                        }

                        let mut segment = std::mem::take(&mut segment_buffer);

                        // Pad short segments with silence (same as audio.rs stop_recording)
                        if segment.len() < WHISPER_SAMPLE_RATE && !segment.is_empty() {
                            segment.resize(WHISPER_SAMPLE_RATE * 5 / 4, 0.0);
                        }

                        debug!(
                            "Incremental: dispatching segment for transcription ({} samples, {:.1}s)",
                            segment.len(),
                            segment.len() as f64 / WHISPER_SAMPLE_RATE as f64
                        );

                        // Spawn a thread to transcribe this segment
                        let tm_clone = Arc::clone(&tm);
                        let text_clone = accumulated_text.clone();
                        let app_clone = app.clone();
                        let transcribing_flag = is_transcribing.clone();

                        transcribing_flag.store(true, Ordering::Relaxed);

                        std::thread::spawn(move || {
                            match tm_clone.transcribe(segment) {
                                Ok(text) if !text.is_empty() => {
                                    let mut acc = text_clone.lock().unwrap();
                                    if !acc.is_empty() {
                                        acc.push(' ');
                                    }
                                    acc.push_str(&text);
                                    let current = acc.clone();
                                    drop(acc);

                                    // Emit to streaming overlay (same event as Mistral)
                                    let _ =
                                        app_clone.emit("streaming-transcription-update", current);
                                    debug!("Incremental: segment transcribed: \"{}\"", text);
                                }
                                Ok(_) => {
                                    debug!("Incremental: segment transcription returned empty");
                                }
                                Err(e) => {
                                    warn!("Incremental: segment transcription failed: {}", e);
                                }
                            }
                            transcribing_flag.store(false, Ordering::Relaxed);
                        });
                    }
                    // If was_speech is false and we get Noise, nothing to do
                }
                Err(e) => {
                    warn!("Incremental: VAD error: {}", e);
                }
            }
        }

        info!("Incremental transcription worker stopped");
    }
}
