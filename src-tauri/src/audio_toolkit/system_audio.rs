//! System audio capture via ScreenCaptureKit (macOS 13+)
//!
//! Captures desktop/system audio (meetings, videos, podcasts, etc.) using
//! Apple's ScreenCaptureKit framework. Requires Screen Recording permission.

#[cfg(target_os = "macos")]
use log::info;
#[cfg(target_os = "macos")]
use screencapturekit::prelude::*;
#[cfg(target_os = "macos")]
use std::sync::mpsc;

/// Check if system audio capture is available (macOS 13.0+).
#[cfg(target_os = "macos")]
pub fn is_available() -> bool {
    let (major, _, _) = macos_version();
    major >= 13
}

#[cfg(not(target_os = "macos"))]
pub fn is_available() -> bool {
    false
}

/// Prompt macOS to register this app in the Screen Recording permission list.
///
/// Calling `SCShareableContent::get()` triggers ScreenCaptureKit which causes
/// macOS to add the app to System Settings > Privacy & Security > Screen Recording.
#[cfg(target_os = "macos")]
pub fn prompt_screen_recording_registration() {
    let _ = SCShareableContent::get();
}

#[cfg(not(target_os = "macos"))]
pub fn prompt_screen_recording_registration() {}

#[cfg(target_os = "macos")]
fn macos_version() -> (u32, u32, u32) {
    use std::process::Command;
    let output = Command::new("sw_vers")
        .arg("-productVersion")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .unwrap_or_default();

    let parts: Vec<u32> = output
        .trim()
        .split('.')
        .filter_map(|s| s.parse().ok())
        .collect();

    (
        parts.first().copied().unwrap_or(0),
        parts.get(1).copied().unwrap_or(0),
        parts.get(2).copied().unwrap_or(0),
    )
}

// ---------------------------------------------------------------------------
// ScreenCaptureKit wrapper (macOS only)
// ---------------------------------------------------------------------------

#[cfg(target_os = "macos")]
pub struct SystemAudioCapture {
    stream: Option<SCStream>,
}

#[cfg(target_os = "macos")]
struct AudioHandler {
    tx: mpsc::Sender<Vec<f32>>,
}

#[cfg(target_os = "macos")]
impl SCStreamOutputTrait for AudioHandler {
    fn did_output_sample_buffer(&self, sample: CMSampleBuffer, of_type: SCStreamOutputType) {
        if of_type != SCStreamOutputType::Audio {
            return;
        }

        if let Some(samples) = extract_audio_f32(&sample) {
            let _ = self.tx.send(samples);
        }
    }
}

/// Extract mono f32 PCM samples from a CMSampleBuffer using the crate's
/// `audio_buffer_list()` API.
///
/// ScreenCaptureKit delivers audio as float32 PCM at the configured sample
/// rate and channel count. We read the AudioBufferList and downmix to mono.
#[cfg(target_os = "macos")]
fn extract_audio_f32(sample: &CMSampleBuffer) -> Option<Vec<f32>> {
    let buffer_list = sample.audio_buffer_list()?;

    let mut all_samples: Vec<f32> = Vec::new();

    for audio_buffer in &buffer_list {
        let raw_bytes = audio_buffer.data();
        if raw_bytes.is_empty() {
            continue;
        }

        let channels = audio_buffer.number_channels as usize;

        // Interpret raw bytes as f32 PCM
        let float_count = raw_bytes.len() / std::mem::size_of::<f32>();
        let float_slice = unsafe {
            std::slice::from_raw_parts(raw_bytes.as_ptr() as *const f32, float_count)
        };

        if channels <= 1 {
            // Already mono
            all_samples.extend_from_slice(float_slice);
        } else {
            // Downmix interleaved stereo/multi-channel to mono
            for frame in float_slice.chunks_exact(channels) {
                let sum: f32 = frame.iter().sum();
                all_samples.push(sum / channels as f32);
            }
        }
    }

    if all_samples.is_empty() {
        None
    } else {
        Some(all_samples)
    }
}

#[cfg(target_os = "macos")]
impl SystemAudioCapture {
    pub fn new() -> Self {
        Self { stream: None }
    }

    /// Start capturing system audio.
    ///
    /// Returns a `Receiver<Vec<f32>>` that yields mono f32 PCM chunks at
    /// 48 kHz.  The caller is responsible for resampling to 16 kHz before
    /// feeding into the transcription pipeline (the existing `FrameResampler`
    /// handles this).
    pub fn start(&mut self) -> Result<mpsc::Receiver<Vec<f32>>, anyhow::Error> {
        if self.stream.is_some() {
            anyhow::bail!("System audio capture is already running");
        }

        info!("Starting system audio capture via ScreenCaptureKit");

        let content = SCShareableContent::get()
            .map_err(|e| anyhow::anyhow!("Failed to get shareable content: {:?}", e))?;

        let display = content
            .displays()
            .into_iter()
            .next()
            .ok_or_else(|| anyhow::anyhow!("No display found for audio capture"))?;

        let filter = SCContentFilter::create()
            .with_display(&display)
            .with_excluding_windows(&[])
            .build();

        // Minimal video (required by SCK even for audio-only), full audio config.
        let config = SCStreamConfiguration::new()
            .with_width(2)
            .with_height(2)
            .with_captures_audio(true)
            .with_sample_rate(48000)
            .with_channel_count(2);

        let (tx, rx) = mpsc::channel();
        let handler = AudioHandler { tx };

        let mut stream = SCStream::new(&filter, &config);
        stream.add_output_handler(handler, SCStreamOutputType::Audio);

        stream
            .start_capture()
            .map_err(|e| anyhow::anyhow!("Failed to start system audio capture: {:?}", e))?;

        info!("System audio capture started (48 kHz stereo -> mono)");
        self.stream = Some(stream);
        Ok(rx)
    }

    pub fn stop(&mut self) {
        if let Some(stream) = self.stream.take() {
            info!("Stopping system audio capture");
            let _ = stream.stop_capture();
        }
    }

    pub fn is_running(&self) -> bool {
        self.stream.is_some()
    }
}

#[cfg(target_os = "macos")]
impl Drop for SystemAudioCapture {
    fn drop(&mut self) {
        self.stop();
    }
}
