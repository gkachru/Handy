//! System audio capture via CoreAudio process taps (macOS 14.2+)
//!
//! Captures desktop/system audio (meetings, videos, podcasts, etc.) using
//! Apple's CoreAudio process tap API. This appears in the less-invasive
//! "System Audio Recording Only" permission section, unlike ScreenCaptureKit
//! which requires full Screen Recording permission.

#[cfg(target_os = "macos")]
use log::{info, warn};
#[cfg(target_os = "macos")]
use std::ffi::c_void;
#[cfg(target_os = "macos")]
use std::ptr::NonNull;
#[cfg(target_os = "macos")]
use std::sync::atomic::{AtomicU64, Ordering};
#[cfg(target_os = "macos")]
use std::sync::mpsc;

#[cfg(target_os = "macos")]
use objc2::rc::Retained;
#[cfg(target_os = "macos")]
use objc2::runtime::AnyObject;
#[cfg(target_os = "macos")]
use objc2::{msg_send, AnyThread, ClassType};
#[cfg(target_os = "macos")]
use objc2_core_audio::*;
#[cfg(target_os = "macos")]
use objc2_core_audio_types::*;
#[cfg(target_os = "macos")]
use objc2_core_foundation::CFDictionary;
#[cfg(target_os = "macos")]
use objc2_foundation::*;

/// Check if system audio capture is available (macOS 14.2+).
#[cfg(target_os = "macos")]
pub fn is_available() -> bool {
    let (major, minor, _) = macos_version();
    major > 14 || (major == 14 && minor >= 2)
}

#[cfg(not(target_os = "macos"))]
pub fn is_available() -> bool {
    false
}

/// No-op: CoreAudio taps don't need pre-registration.
/// Kept for backward compatibility with frontend commands.
pub fn prompt_screen_recording_registration() {}

// ---------------------------------------------------------------------------
// TCC (Transparency, Consent, and Control) private API
// ---------------------------------------------------------------------------
#[cfg(target_os = "macos")]
mod tcc {
    use std::ffi::{c_char, c_void};
    use std::sync::{mpsc, OnceLock};

    use objc2::runtime::Bool;
    use objc2_foundation::NSString;

    // Raw dlopen/dlsym FFI (avoid adding libc as a direct dependency)
    extern "C" {
        fn dlopen(filename: *const c_char, flags: i32) -> *mut c_void;
        fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
    }
    const RTLD_LAZY: i32 = 0x1;

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum TccStatus {
        Authorized,
        Denied,
        Undetermined,
    }

    // Function signatures from the private TCC.framework.
    // The service parameter is a CFStringRef (toll-free bridged to NSString*).
    // TCCAccessPreflight returns 0 = authorized, 1 = denied, other = undetermined.
    // TCCAccessRequest invokes the callback with an ObjC Bool (granted).
    type TCCAccessPreflightFn = unsafe extern "C" fn(*const c_void, *const c_void) -> i32;
    type TCCAccessRequestFn =
        unsafe extern "C" fn(*const c_void, *const c_void, *const block2::Block<dyn Fn(Bool)>);

    struct TccFns {
        preflight: TCCAccessPreflightFn,
        request: TCCAccessRequestFn,
    }

    // Safety: function pointers are Send+Sync (they point to code, not mutable data).
    unsafe impl Send for TccFns {}
    unsafe impl Sync for TccFns {}

    static TCC_FNS: OnceLock<Option<TccFns>> = OnceLock::new();

    /// Create the service NSString for audio capture TCC permission.
    fn service_string() -> objc2::rc::Retained<NSString> {
        NSString::from_str("kTCCServiceAudioCapture")
    }

    fn load() -> &'static Option<TccFns> {
        TCC_FNS.get_or_init(|| {
            let path = c"/System/Library/PrivateFrameworks/TCC.framework/TCC";
            let handle = unsafe { dlopen(path.as_ptr(), RTLD_LAZY) };
            if handle.is_null() {
                log::warn!("TCC: could not dlopen TCC.framework");
                return None;
            }

            let preflight_sym =
                unsafe { dlsym(handle, c"TCCAccessPreflight".as_ptr()) };
            let request_sym =
                unsafe { dlsym(handle, c"TCCAccessRequest".as_ptr()) };

            if preflight_sym.is_null() || request_sym.is_null() {
                log::warn!("TCC: could not resolve TCCAccessPreflight/TCCAccessRequest");
                // Don't dlclose — keep handle alive for safety
                return None;
            }

            Some(TccFns {
                preflight: unsafe { std::mem::transmute(preflight_sym) },
                request: unsafe { std::mem::transmute(request_sym) },
            })
        })
    }

    /// Check the current TCC status for audio capture without prompting.
    pub fn preflight_audio_capture() -> Option<TccStatus> {
        let fns = load().as_ref()?;
        let service = service_string();
        let service_ptr: *const c_void = (service.as_ref() as *const NSString).cast();
        let result = unsafe { (fns.preflight)(service_ptr, std::ptr::null()) };
        Some(match result {
            0 => TccStatus::Authorized,
            1 => TccStatus::Denied,
            _ => TccStatus::Undetermined,
        })
    }

    /// Request audio capture permission. Shows the macOS TCC dialog if status
    /// is undetermined. Blocks the calling thread until the user responds or
    /// a 120-second timeout expires.
    pub fn request_audio_capture() -> Option<bool> {
        let fns = load().as_ref()?;
        let service = service_string();
        let service_ptr: *const c_void = (service.as_ref() as *const NSString).cast();
        let (tx, rx) = mpsc::channel();
        let block = block2::RcBlock::new(move |granted: Bool| {
            let _ = tx.send(granted.as_bool());
        });
        unsafe {
            (fns.request)(service_ptr, std::ptr::null(), &*block);
        }
        match rx.recv_timeout(std::time::Duration::from_secs(120)) {
            Ok(granted) => Some(granted),
            Err(_) => {
                log::warn!("TCC: request_audio_capture timed out after 120s");
                Some(false)
            }
        }
    }
}

/// Instantly check whether the "System Audio Recording" TCC permission is
/// granted, using the private TCC framework. Falls back to the legacy
/// silence-detection probe if the TCC API is unavailable.
#[cfg(target_os = "macos")]
pub fn check_permission() -> bool {
    if !is_available() {
        return false;
    }
    match tcc::preflight_audio_capture() {
        Some(tcc::TccStatus::Authorized) => {
            info!("check_permission: TCC says Authorized");
            true
        }
        Some(status) => {
            info!("check_permission: TCC says {status:?}");
            false
        }
        None => {
            info!("check_permission: TCC unavailable, using legacy probe");
            check_permission_legacy()
        }
    }
}

#[cfg(not(target_os = "macos"))]
pub fn check_permission() -> bool {
    false
}

/// Programmatically request the "System Audio Recording" TCC permission.
/// Shows the macOS permission dialog if status is undetermined. Blocks until
/// the user responds.
#[cfg(target_os = "macos")]
pub fn request_permission() -> bool {
    if !is_available() {
        return false;
    }
    if check_permission() {
        return true;
    }
    info!("request_permission: requesting TCC audio capture permission via TCC API");
    if tcc::request_audio_capture().unwrap_or(false) {
        return true;
    }
    // TCC dialog didn't show (unsigned dev build) or user denied.
    // Attempt a process tap to register the app in System Settings,
    // then open System Settings so the user can grant manually.
    info!("request_permission: TCC dialog failed, registering via process tap");
    register_for_permission();
    open_system_audio_settings();
    false
}

#[cfg(not(target_os = "macos"))]
pub fn request_permission() -> bool {
    false
}

/// Create and immediately destroy a process tap to force macOS to register
/// the app in the "System Audio Recording" section of System Settings.
#[cfg(target_os = "macos")]
fn register_for_permission() {
    let processes: Retained<NSArray<NSNumber>> = NSArray::new();
    let tap_description = unsafe {
        CATapDescription::initStereoGlobalTapButExcludeProcesses(
            CATapDescription::alloc(),
            &processes,
        )
    };
    let mut tap_id: AudioObjectID = 0;
    let status =
        unsafe { AudioHardwareCreateProcessTap(Some(&tap_description), &mut tap_id) };
    if status == 0 {
        info!("register_for_permission: created tap {tap_id}, destroying immediately");
        unsafe {
            AudioHardwareDestroyProcessTap(tap_id);
        }
    } else {
        warn!("register_for_permission: AudioHardwareCreateProcessTap failed ({status})");
    }
}

/// Open System Settings to the Screen & System Audio Recording pane.
#[cfg(target_os = "macos")]
fn open_system_audio_settings() {
    let _ = std::process::Command::new("open")
        .arg("x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture")
        .spawn();
}

/// Legacy permission check: start a real capture and listen for non-zero
/// samples. Used as fallback when the private TCC API is unavailable.
#[cfg(target_os = "macos")]
fn check_permission_legacy() -> bool {
    let mut capture = SystemAudioCapture::new();
    let (rx, _sample_rate) = match capture.start() {
        Ok(pair) => pair,
        Err(e) => {
            info!("check_permission_legacy: capture start failed: {e}");
            return false;
        }
    };

    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(500);
    let mut got_audio = false;

    while std::time::Instant::now() < deadline {
        match rx.recv_timeout(std::time::Duration::from_millis(50)) {
            Ok(samples) => {
                if samples.iter().any(|&s| s != 0.0) {
                    got_audio = true;
                    break;
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
            Err(_) => break,
        }
    }

    capture.stop();

    if got_audio {
        info!("check_permission_legacy: real audio detected — permission granted");
    } else {
        info!(
            "check_permission_legacy: tap succeeded but only silence received — \
             TCC permission likely not granted"
        );
    }
    got_audio
}

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
// CoreAudio process tap constants
// ---------------------------------------------------------------------------

/// `kAudioTapPropertyFormat` ('tfmt') — property selector to read the tap's
/// `AudioStreamBasicDescription` (contains sample rate, channels, etc.).
#[cfg(target_os = "macos")]
const AUDIO_TAP_PROPERTY_FORMAT: u32 = 0x74666D74;

// ---------------------------------------------------------------------------
// CoreAudio process tap wrapper (macOS only)
// ---------------------------------------------------------------------------

#[cfg(target_os = "macos")]
pub struct SystemAudioCapture {
    tap_id: Option<AudioObjectID>,
    aggregate_device_id: Option<AudioObjectID>,
    io_proc_id: Option<AudioDeviceIOProcID>,
    running: bool,
    /// Prevent the sender from being dropped while the IO proc callback uses it.
    _sender: Option<Box<mpsc::Sender<Vec<f32>>>>,
}

#[cfg(target_os = "macos")]
impl SystemAudioCapture {
    pub fn new() -> Self {
        Self {
            tap_id: None,
            aggregate_device_id: None,
            io_proc_id: None,
            running: false,
            _sender: None,
        }
    }

    /// Start capturing system audio.
    ///
    /// Returns a `(Receiver<Vec<f32>>, u32)` tuple: the receiver yields mono
    /// f32 PCM chunks, and the `u32` is the native sample rate of the tap.
    /// The caller is responsible for resampling to 16 kHz before feeding into
    /// the transcription pipeline.
    pub fn start(&mut self) -> Result<(mpsc::Receiver<Vec<f32>>, u32), anyhow::Error> {
        if self.running {
            anyhow::bail!("System audio capture is already running");
        }

        info!("Starting system audio capture via CoreAudio process tap");

        // Reset diagnostic counters
        IO_CALLBACK_COUNT.store(0, Ordering::Relaxed);
        IO_NONZERO_COUNT.store(0, Ordering::Relaxed);

        // Step 1: Create CATapDescription — global stereo tap (empty exclude list
        // captures everything; our app doesn't play audio through system output).
        let processes: Retained<NSArray<NSNumber>> = NSArray::new();

        let tap_description = unsafe {
            CATapDescription::initStereoGlobalTapButExcludeProcesses(
                CATapDescription::alloc(),
                &processes,
            )
        };

        unsafe {
            tap_description.setMuteBehavior(CATapMuteBehavior::Unmuted);
            tap_description.setPrivate(true);
        }

        // Step 2: Create the process tap
        let mut tap_id: AudioObjectID = 0;
        let status = unsafe { AudioHardwareCreateProcessTap(Some(&tap_description), &mut tap_id) };
        if status != 0 {
            anyhow::bail!("AudioHardwareCreateProcessTap failed with status {status}");
        }
        info!("Created process tap with ID {tap_id}");

        // Step 3: Read the tap UUID for aggregate device configuration
        let tap_uuid_string = unsafe { tap_description.UUID().UUIDString() };

        // Step 4: Get the default output device UID (used as clock source)
        let output_uid = Self::get_default_output_device_uid().unwrap_or_default();
        info!("Default output device UID: {:?}", output_uid);
        let output_uid_ns = NSString::from_str(&output_uid);

        // Step 5: Build aggregate device dictionary
        let aggregate_dict =
            unsafe { Self::build_aggregate_device_dict(&tap_uuid_string, &output_uid_ns) };

        // Step 6: Create aggregate device
        let mut aggregate_device_id: AudioObjectID = 0;
        let cf_dict: &CFDictionary =
            unsafe { &*(&*aggregate_dict as *const _ as *const CFDictionary) };
        let status = unsafe {
            AudioHardwareCreateAggregateDevice(
                cf_dict,
                NonNull::new(&mut aggregate_device_id).unwrap(),
            )
        };
        if status != 0 {
            unsafe {
                AudioHardwareDestroyProcessTap(tap_id);
            }
            anyhow::bail!("AudioHardwareCreateAggregateDevice failed with status {status}");
        }
        info!("Created aggregate device with ID {aggregate_device_id}");

        // Step 7: Read the tap stream format to determine sample rate
        let sample_rate = Self::get_tap_sample_rate(tap_id);
        info!("Tap sample rate: {sample_rate} Hz");

        // Step 8: Set up IO proc callback and mpsc channel
        let (tx, rx) = mpsc::channel();
        let sender = Box::new(tx);
        let sender_ptr = &*sender as *const mpsc::Sender<Vec<f32>> as *mut c_void;

        let mut io_proc_id: AudioDeviceIOProcID = None;
        let status = unsafe {
            AudioDeviceCreateIOProcID(
                aggregate_device_id,
                Some(io_proc_callback),
                sender_ptr,
                NonNull::new(&mut io_proc_id).unwrap(),
            )
        };
        if status != 0 {
            unsafe {
                AudioHardwareDestroyAggregateDevice(aggregate_device_id);
                AudioHardwareDestroyProcessTap(tap_id);
            }
            anyhow::bail!("AudioDeviceCreateIOProcID failed with status {status}");
        }

        // Step 9: Start streaming audio
        let status = unsafe { AudioDeviceStart(aggregate_device_id, io_proc_id) };
        if status != 0 {
            unsafe {
                AudioDeviceDestroyIOProcID(aggregate_device_id, io_proc_id);
                AudioHardwareDestroyAggregateDevice(aggregate_device_id);
                AudioHardwareDestroyProcessTap(tap_id);
            }
            anyhow::bail!("AudioDeviceStart failed with status {status}");
        }

        self.tap_id = Some(tap_id);
        self.aggregate_device_id = Some(aggregate_device_id);
        self.io_proc_id = Some(io_proc_id);
        self.running = true;
        self._sender = Some(sender);

        info!("System audio capture started ({sample_rate} Hz stereo -> mono)");

        // Spawn a background check: if after ~2 seconds the IO callback has
        // fired but every buffer was silent, warn that TCC permission is likely
        // missing (macOS delivers zeros instead of real audio).
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_secs(2));
            let total = IO_CALLBACK_COUNT.load(Ordering::Relaxed);
            let nonzero = IO_NONZERO_COUNT.load(Ordering::Relaxed);
            if total > 50 && nonzero == 0 {
                warn!(
                    "System audio tap is delivering silence ({total} callbacks, 0 non-zero). \
                     The app likely lacks \"System Audio Recording\" permission. \
                     Open System Settings → Privacy & Security → Screen & System Audio Recording \
                     and grant access to Handy."
                );
            }
        });

        Ok((rx, sample_rate))
    }

    pub fn stop(&mut self) {
        if !self.running {
            return;
        }

        info!("Stopping system audio capture");

        // Tear down in reverse order: stop IO → destroy IO proc → destroy
        // aggregate device → destroy process tap.
        if let (Some(device_id), Some(proc_id)) = (self.aggregate_device_id, self.io_proc_id) {
            unsafe {
                let _ = AudioDeviceStop(device_id, proc_id);
                let _ = AudioDeviceDestroyIOProcID(device_id, proc_id);
            }
        }

        if let Some(device_id) = self.aggregate_device_id {
            unsafe {
                let _ = AudioHardwareDestroyAggregateDevice(device_id);
            }
        }

        if let Some(tap_id) = self.tap_id {
            unsafe {
                let _ = AudioHardwareDestroyProcessTap(tap_id);
            }
        }

        self.io_proc_id = None;
        self.aggregate_device_id = None;
        self.tap_id = None;
        self._sender = None;
        self.running = false;
    }

    pub fn is_running(&self) -> bool {
        self.running
    }

    // -- private helpers ----------------------------------------------------

    /// Build the `NSDictionary` that describes the aggregate device containing
    /// the process tap.
    unsafe fn build_aggregate_device_dict(
        tap_uuid: &NSString,
        output_device_uid: &NSString,
    ) -> Retained<NSMutableDictionary> {
        let dict: Retained<NSMutableDictionary> = NSMutableDictionary::new();

        let set_str =
            |d: &NSMutableDictionary, key: &str, val: &NSString| {
                let k = NSString::from_str(key);
                let _: () = msg_send![d, setObject: val, forKey: &*k];
            };
        let set_int =
            |d: &NSMutableDictionary, key: &str, val: i32| {
                let k = NSString::from_str(key);
                let v: Retained<NSNumber> = msg_send![NSNumber::class(), numberWithInt: val];
                let _: () = msg_send![d, setObject: &*v, forKey: &*k];
            };

        // Aggregate device UID and name
        set_str(&dict, "uid", &NSString::from_str("com.handy.system-audio-tap"));
        set_str(&dict, "name", &NSString::from_str("Handy System Audio"));

        // Private device (not visible in system audio UI)
        set_int(&dict, "private", 1);

        // Not stacked — required when using a tap
        set_int(&dict, "stacked", 0);

        // Auto-start the tap when the aggregate device starts
        set_int(&dict, "tapautostart", 1);

        // Main sub-device is the output device (clock source)
        set_str(&dict, "master", output_device_uid);
        set_str(&dict, "clock", output_device_uid);

        // Sub-device list: include the output device so the aggregate device
        // has a real audio device to drive the clock.
        let sub_device_dict: Retained<NSMutableDictionary> = NSMutableDictionary::new();
        set_str(&sub_device_dict, "uid", output_device_uid);
        let sub_device_list: Retained<NSArray<AnyObject>> =
            msg_send![NSArray::<AnyObject>::class(), arrayWithObject: &*sub_device_dict];
        let sub_key = NSString::from_str("sub");
        let _: () = msg_send![&dict, setObject: &*sub_device_list, forKey: &*sub_key];

        // Sub-tap dictionary (references the process tap by UUID)
        let tap_dict: Retained<NSMutableDictionary> = NSMutableDictionary::new();
        set_str(&tap_dict, "uid", tap_uuid);

        // Tap list: array of sub-tap dictionaries
        let tap_list: Retained<NSArray<AnyObject>> =
            msg_send![NSArray::<AnyObject>::class(), arrayWithObject: &*tap_dict];
        let taps_key = NSString::from_str("taps");
        let _: () = msg_send![&dict, setObject: &*tap_list, forKey: &*taps_key];

        dict
    }

    /// Read the sample rate from the tap's `AudioStreamBasicDescription`.
    /// Falls back to 48 000 Hz if the property read fails.
    fn get_tap_sample_rate(tap_id: AudioObjectID) -> u32 {
        let mut address = AudioObjectPropertyAddress {
            mSelector: AUDIO_TAP_PROPERTY_FORMAT,
            mScope: kAudioObjectPropertyScopeGlobal,
            mElement: kAudioObjectPropertyElementMain,
        };

        let mut format: AudioStreamBasicDescription = unsafe { std::mem::zeroed() };
        let mut size = std::mem::size_of::<AudioStreamBasicDescription>() as u32;

        let status = unsafe {
            AudioObjectGetPropertyData(
                tap_id,
                NonNull::from(&mut address),
                0,
                std::ptr::null(),
                NonNull::from(&mut size),
                NonNull::new(&mut format as *mut _ as *mut c_void).unwrap(),
            )
        };

        if status != 0 {
            info!("Failed to read tap format (status {status}), defaulting to 48000 Hz");
            return 48000;
        }

        format.mSampleRate as u32
    }

    /// Get the UID string of the default output audio device.
    fn get_default_output_device_uid() -> Option<String> {
        unsafe {
            // First, get the default output device AudioObjectID
            let mut address = AudioObjectPropertyAddress {
                mSelector: kAudioHardwarePropertyDefaultOutputDevice,
                mScope: kAudioObjectPropertyScopeGlobal,
                mElement: kAudioObjectPropertyElementMain,
            };
            let mut device_id: AudioObjectID = 0;
            let mut size = std::mem::size_of::<AudioObjectID>() as u32;
            let status = AudioObjectGetPropertyData(
                kAudioObjectSystemObject as AudioObjectID,
                NonNull::from(&mut address),
                0,
                std::ptr::null(),
                NonNull::from(&mut size),
                NonNull::new(&mut device_id as *mut _ as *mut c_void)?,
            );
            if status != 0 || device_id == 0 {
                return None;
            }

            // Then, get the device UID (a CFStringRef, toll-free bridged to NSString).
            // AudioObjectGetPropertyData returns an owned reference.
            address.mSelector = kAudioDevicePropertyDeviceUID;
            let mut uid_ptr: *mut NSString = std::ptr::null_mut();
            let mut size = std::mem::size_of::<*mut NSString>() as u32;
            let status = AudioObjectGetPropertyData(
                device_id,
                NonNull::from(&mut address),
                0,
                std::ptr::null(),
                NonNull::from(&mut size),
                NonNull::new(&mut uid_ptr as *mut _ as *mut c_void)?,
            );
            if status != 0 || uid_ptr.is_null() {
                return None;
            }

            // Wrap in Retained to take ownership (CFStringRef is toll-free bridged)
            let uid_nsstring = Retained::from_raw(uid_ptr)?;
            Some(uid_nsstring.to_string())
        }
    }
}

// ---------------------------------------------------------------------------
// IO proc callback — called on the CoreAudio real-time thread
// ---------------------------------------------------------------------------

/// Diagnostic counters for the IO proc callback.
#[cfg(target_os = "macos")]
static IO_CALLBACK_COUNT: AtomicU64 = AtomicU64::new(0);
#[cfg(target_os = "macos")]
static IO_NONZERO_COUNT: AtomicU64 = AtomicU64::new(0);

/// Audio callback invoked by CoreAudio for each buffer of captured audio.
/// Reads the `AudioBufferList`, downmixes to mono f32, and sends via the
/// `mpsc::Sender` stored in `client_data`.
#[cfg(target_os = "macos")]
unsafe extern "C-unwind" fn io_proc_callback(
    _device: AudioObjectID,
    _now: NonNull<AudioTimeStamp>,
    input_data: NonNull<AudioBufferList>,
    _input_time: NonNull<AudioTimeStamp>,
    _output_data: NonNull<AudioBufferList>,
    _output_time: NonNull<AudioTimeStamp>,
    client_data: *mut c_void,
) -> i32 {
    if client_data.is_null() {
        return 0;
    }

    let sender = &*(client_data as *const mpsc::Sender<Vec<f32>>);
    let buffer_list = input_data.as_ref();

    let num_buffers = buffer_list.mNumberBuffers as usize;
    if num_buffers == 0 {
        return 0;
    }

    let mut all_samples: Vec<f32> = Vec::new();

    let buffers_ptr = buffer_list.mBuffers.as_ptr();
    for i in 0..num_buffers {
        let buffer = &*buffers_ptr.add(i);

        if buffer.mData.is_null() || buffer.mDataByteSize == 0 {
            continue;
        }

        let channels = buffer.mNumberChannels as usize;
        let float_count = buffer.mDataByteSize as usize / std::mem::size_of::<f32>();
        let float_slice = std::slice::from_raw_parts(buffer.mData as *const f32, float_count);

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

    // Diagnostic: log callback activity periodically
    let count = IO_CALLBACK_COUNT.fetch_add(1, Ordering::Relaxed) + 1;
    let has_nonzero = all_samples.iter().any(|&s| s != 0.0);
    if has_nonzero {
        IO_NONZERO_COUNT.fetch_add(1, Ordering::Relaxed);
    }
    // Log every 500 callbacks (~10s at 48kHz/1024 frame)
    if count % 500 == 1 {
        let nonzero = IO_NONZERO_COUNT.load(Ordering::Relaxed);
        let peak = all_samples.iter().map(|s| s.abs()).fold(0.0f32, f32::max);
        // Use eprintln since log macros may not be safe on realtime thread
        eprintln!(
            "[system_audio] IO callback #{count}: buffers={num_buffers}, samples={}, peak={peak:.6}, non-zero callbacks={nonzero}/{count}",
            all_samples.len()
        );
    }

    if !all_samples.is_empty() {
        let _ = sender.send(all_samples);
    }

    0
}

#[cfg(target_os = "macos")]
impl Drop for SystemAudioCapture {
    fn drop(&mut self) {
        self.stop();
    }
}
