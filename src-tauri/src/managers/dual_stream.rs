use log::{debug, info};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tauri::{AppHandle, Emitter};

#[derive(Clone)]
enum StreamSource {
    Mic,
    System,
}

#[derive(Clone)]
struct TextSegment {
    text: String,
    source: StreamSource,
    _timestamp: Instant,
}

/// Coordinates two parallel Mistral transcription sessions (mic + system audio),
/// merging their outputs with labels for the overlay and final paste.
pub struct DualStreamCoordinator {
    stop_signal: Arc<AtomicBool>,
    task_handle: Option<tauri::async_runtime::JoinHandle<()>>,
    merged_text: Arc<Mutex<String>>,
    segments: Arc<Mutex<Vec<TextSegment>>>,
}

impl DualStreamCoordinator {
    /// Start the coordinator polling loop.
    ///
    /// `mic_text_ref` and `sys_text_ref` are the `final_text` arcs from each
    /// `MistralRealtimeSession`. The coordinator polls them every 200ms, detects
    /// new deltas, and emits a merged view to the overlay.
    pub fn start(
        app: AppHandle,
        mic_text_ref: Arc<Mutex<String>>,
        sys_text_ref: Arc<Mutex<String>>,
    ) -> Self {
        let stop_signal = Arc::new(AtomicBool::new(false));
        let merged_text = Arc::new(Mutex::new(String::new()));
        let segments: Arc<Mutex<Vec<TextSegment>>> = Arc::new(Mutex::new(Vec::new()));

        let stop = stop_signal.clone();
        let merged = merged_text.clone();
        let segs = segments.clone();

        let handle = tauri::async_runtime::spawn(async move {
            Self::poll_loop(app, mic_text_ref, sys_text_ref, stop, merged, segs).await;
        });

        Self {
            stop_signal,
            task_handle: Some(handle),
            merged_text,
            segments,
        }
    }

    /// Get a shared reference to the merged text (for StreamingTranslator).
    pub fn get_text_ref(&self) -> Arc<Mutex<String>> {
        self.merged_text.clone()
    }

    /// Stop the polling loop and return the collected segments.
    pub fn stop(&mut self) -> Vec<(String, bool)> {
        self.stop_signal.store(true, Ordering::Relaxed);
        if let Some(handle) = self.task_handle.take() {
            handle.abort();
        }

        let segments = self.segments.lock().unwrap();
        segments
            .iter()
            .map(|s| {
                let is_mic = matches!(s.source, StreamSource::Mic);
                (s.text.clone(), is_mic)
            })
            .collect()
    }

    /// Build the final chronologically-interleaved output text from segments.
    pub fn build_final_text(
        segments: &[(String, bool)],
        mic_final: &str,
        sys_final: &str,
    ) -> String {
        if !segments.is_empty() {
            let text = Self::build_interleaved_text(segments);
            if !text.is_empty() {
                return text;
            }
        }

        // Fallback: just concatenate final texts with labels
        let mut result = String::new();
        if !mic_final.trim().is_empty() {
            result.push_str("[🎤 Mic] ");
            result.push_str(mic_final.trim());
        }
        if !sys_final.trim().is_empty() {
            if !result.is_empty() {
                result.push('\n');
            }
            result.push_str("[🔊 System Audio] ");
            result.push_str(sys_final.trim());
        }
        result
    }

    /// Build interleaved text from segments, merging consecutive same-source segments.
    fn build_interleaved_text(segments: &[(String, bool)]) -> String {
        let mut result = String::new();
        let mut last_source: Option<bool> = None;

        for (text, is_mic) in segments {
            let trimmed = text.trim();
            if trimmed.is_empty() {
                continue;
            }

            if last_source == Some(*is_mic) {
                // Same source as previous — append text to current segment
                result.push(' ');
                result.push_str(trimmed);
            } else {
                // New source — start a new labeled line
                let label = if *is_mic {
                    "[🎤 Mic]"
                } else {
                    "[🔊 System Audio]"
                };
                if !result.is_empty() {
                    result.push('\n');
                }
                result.push_str(label);
                result.push(' ');
                result.push_str(trimmed);
                last_source = Some(*is_mic);
            }
        }

        result
    }

    async fn poll_loop(
        app: AppHandle,
        mic_text_ref: Arc<Mutex<String>>,
        sys_text_ref: Arc<Mutex<String>>,
        stop_signal: Arc<AtomicBool>,
        merged_text: Arc<Mutex<String>>,
        segments: Arc<Mutex<Vec<TextSegment>>>,
    ) {
        let mut last_mic_len: usize = 0;
        let mut last_sys_len: usize = 0;

        loop {
            if stop_signal.load(Ordering::Relaxed) {
                break;
            }

            tokio::time::sleep(std::time::Duration::from_millis(200)).await;

            if stop_signal.load(Ordering::Relaxed) {
                break;
            }

            let mic_text = mic_text_ref.lock().unwrap().clone();
            let sys_text = sys_text_ref.lock().unwrap().clone();

            // Detect new deltas
            let mic_changed = mic_text.len() > last_mic_len;
            let sys_changed = sys_text.len() > last_sys_len;

            if mic_changed {
                let delta = mic_text[last_mic_len..].to_string();
                if !delta.trim().is_empty() {
                    segments.lock().unwrap().push(TextSegment {
                        text: delta,
                        source: StreamSource::Mic,
                        _timestamp: Instant::now(),
                    });
                }
                last_mic_len = mic_text.len();
            }

            if sys_changed {
                let delta = sys_text[last_sys_len..].to_string();
                if !delta.trim().is_empty() {
                    segments.lock().unwrap().push(TextSegment {
                        text: delta,
                        source: StreamSource::System,
                        _timestamp: Instant::now(),
                    });
                }
                last_sys_len = sys_text.len();
            }

            if mic_changed || sys_changed {
                // Build display text from segments (interleaved timeline)
                let segs = segments.lock().unwrap();
                let seg_pairs: Vec<(String, bool)> = segs
                    .iter()
                    .map(|s| (s.text.clone(), matches!(s.source, StreamSource::Mic)))
                    .collect();
                drop(segs);

                let display = Self::build_interleaved_text(&seg_pairs);
                *merged_text.lock().unwrap() = display.clone();
                let _ = app.emit("streaming-transcription-update", &display);
                debug!(
                    "DualStream: mic={}chars sys={}chars segments={}",
                    mic_text.len(),
                    sys_text.len(),
                    seg_pairs.len()
                );
            }
        }

        info!("DualStreamCoordinator poll loop stopped");
    }
}
