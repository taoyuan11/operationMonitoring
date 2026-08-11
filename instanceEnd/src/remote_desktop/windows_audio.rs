use std::{
    collections::VecDeque,
    mem::size_of,
    ptr::null_mut,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use rusty_opus::{Application, OpusEncoder};
use tokio::sync::mpsc;
use windows::{
    Win32::{
        Foundation::{CloseHandle, ERROR_NOT_FOUND, HANDLE},
        Media::{
            Audio::{
                AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY, AUDCLNT_BUFFERFLAGS_SILENT,
                AUDCLNT_E_DEVICE_INVALIDATED, AUDCLNT_E_RESOURCES_INVALIDATED,
                AUDCLNT_E_SERVICE_NOT_RUNNING, AUDCLNT_SHAREMODE_SHARED,
                AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM, AUDCLNT_STREAMFLAGS_LOOPBACK,
                AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY, IAudioCaptureClient, IAudioClient,
                IMMDeviceEnumerator, MMDeviceEnumerator, WAVEFORMATEX, eMultimedia, eRender,
            },
            Multimedia::WAVE_FORMAT_IEEE_FLOAT,
        },
        Security::{ImpersonateLoggedOnUser, RevertToSelf},
        System::{
            Com::{
                CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree,
                CoUninitialize,
            },
            RemoteDesktop::WTSQueryUserToken,
        },
    },
    core::{Error as WindowsError, HRESULT},
};

use super::{
    AUDIO_CHANNELS, AUDIO_CODEC_NAME, AUDIO_FLAG_DISCONTINUITY, AUDIO_FRAME_HEADER_LEN,
    AUDIO_SAMPLE_RATE, AUDIO_SAMPLES_PER_FRAME, AudioFrameHeader, DropOldestSender,
    MAX_AUDIO_FRAME_BYTES, MAX_AUDIO_OPUS_PAYLOAD_BYTES,
};

const AUDIO_BITRATE_BPS: i32 = 96_000;
const AUDIO_PACKET_DURATION: Duration = Duration::from_millis(20);
const AUDIO_RETRY_DELAY: Duration = Duration::from_secs(2);
const AUDIO_IDLE_POLL: Duration = Duration::from_millis(20);
const AUDIO_CAPTURE_POLL: Duration = Duration::from_millis(5);
const AUDIO_DEVICE_CHECK_INTERVAL: Duration = Duration::from_secs(2);
const AUDIO_BYTES_PER_SAMPLE: usize = size_of::<f32>();
const AUDIO_BYTES_PER_FRAME: usize = AUDIO_CHANNELS as usize * AUDIO_BYTES_PER_SAMPLE;
const AUDIO_PCM_PACKET_BYTES: usize = AUDIO_SAMPLES_PER_FRAME as usize * AUDIO_BYTES_PER_FRAME;
const AUDIO_FRAME_DURATION_US: u64 =
    AUDIO_SAMPLES_PER_FRAME as u64 * 1_000_000 / AUDIO_SAMPLE_RATE as u64;

const STATE_OFF: &str = "off";
const STATE_STARTING: &str = "starting";
const STATE_PLAYING: &str = "playing";
const STATE_PAUSED: &str = "paused";
const STATE_UNAVAILABLE: &str = "unavailable";

const REASON_SECURE_DESKTOP: &str = "secure_desktop";
const REASON_NO_OUTPUT_DEVICE: &str = "no_output_device";
const REASON_AUDIO_SERVICE_UNAVAILABLE: &str = "audio_service_unavailable";
const REASON_DEVICE_INVALIDATED: &str = "device_invalidated";
const REASON_USER_TOKEN_UNAVAILABLE: &str = "user_token_unavailable";
const REASON_CAPTURE_FAILED: &str = "capture_failed";
const REASON_ENCODER_FAILED: &str = "encoder_failed";

pub(super) struct AudioRuntime {
    shared: Arc<AudioRuntimeState>,
    frames: DropOldestSender<CapturedAudioFrame>,
}

pub(super) struct CapturedAudioFrame {
    pub(super) epoch: u64,
    pub(super) bytes: Vec<u8>,
}

struct AudioRuntimeState {
    enabled: AtomicBool,
    default_desktop: AtomicBool,
    discontinuity: AtomicBool,
    state_epoch: AtomicU64,
    stopped: AtomicBool,
}

impl AudioRuntime {
    pub(super) fn set_enabled(&self, enabled: bool) -> bool {
        let changed = self.shared.enabled.swap(enabled, Ordering::AcqRel) != enabled;
        if changed {
            self.shared.discontinuity.store(true, Ordering::Release);
            self.shared.state_epoch.fetch_add(1, Ordering::AcqRel);
        }
        if changed || !enabled {
            self.frames.clear();
        }
        changed
    }

    pub(super) fn set_default_desktop(&self, available: bool) {
        if self
            .shared
            .default_desktop
            .swap(available, Ordering::AcqRel)
            != available
        {
            self.shared.discontinuity.store(true, Ordering::Release);
            self.shared.state_epoch.fetch_add(1, Ordering::AcqRel);
        }
        if !available {
            self.frames.clear();
        }
    }

    pub(super) fn stop(&self) {
        if !self.shared.stopped.swap(true, Ordering::AcqRel) {
            self.shared.state_epoch.fetch_add(1, Ordering::AcqRel);
        }
        self.frames.clear();
    }

    pub(super) fn accepts(&self, frame: &CapturedAudioFrame) -> bool {
        !self.shared.stopped.load(Ordering::Acquire)
            && self.shared.enabled.load(Ordering::Acquire)
            && self.shared.default_desktop.load(Ordering::Acquire)
            && self.shared.state_epoch.load(Ordering::Acquire) == frame.epoch
    }
}

impl Drop for AudioRuntime {
    fn drop(&mut self) {
        self.stop();
    }
}

pub(super) fn negotiated(codec: Option<&str>) -> bool {
    codec.is_some_and(|codec| codec.eq_ignore_ascii_case(AUDIO_CODEC_NAME))
}

pub(super) fn spawn(
    system_helper: bool,
    session_id: u32,
    default_desktop: bool,
    frame_tx: DropOldestSender<CapturedAudioFrame>,
    status_tx: mpsc::Sender<String>,
) -> Result<AudioRuntime> {
    let shared = Arc::new(AudioRuntimeState {
        enabled: AtomicBool::new(false),
        default_desktop: AtomicBool::new(default_desktop),
        discontinuity: AtomicBool::new(true),
        state_epoch: AtomicU64::new(0),
        stopped: AtomicBool::new(false),
    });
    let worker_shared = shared.clone();
    let worker_frames = frame_tx.clone();
    std::thread::Builder::new()
        .name("om-desktop-audio".to_string())
        .spawn(move || {
            audio_supervisor(
                system_helper,
                session_id,
                worker_shared,
                worker_frames,
                status_tx,
            )
        })?;
    Ok(AudioRuntime {
        shared,
        frames: frame_tx,
    })
}

fn audio_supervisor(
    system_helper: bool,
    session_id: u32,
    shared: Arc<AudioRuntimeState>,
    frame_tx: DropOldestSender<CapturedAudioFrame>,
    status_tx: mpsc::Sender<String>,
) {
    let mut reporter = AudioStateReporter::new(status_tx);
    reporter.report(STATE_OFF, None);
    let mut impersonation = None;
    let mut com = None;
    let mut sequence = 0_u64;
    let mut stream_started = None;
    let mut last_timestamp_us = None;
    let mut pending_discontinuity = true;
    loop {
        if shared.stopped.load(Ordering::Acquire) {
            frame_tx.clear();
            return;
        }
        if !shared.enabled.load(Ordering::Acquire) {
            frame_tx.clear();
            com = None;
            impersonation = None;
            pending_discontinuity = true;
            reporter.report(STATE_OFF, None);
            std::thread::sleep(AUDIO_IDLE_POLL);
            continue;
        }
        if !shared.default_desktop.load(Ordering::Acquire) {
            frame_tx.clear();
            com = None;
            impersonation = None;
            pending_discontinuity = true;
            reporter.report(STATE_PAUSED, Some(REASON_SECURE_DESKTOP));
            std::thread::sleep(AUDIO_IDLE_POLL);
            continue;
        }

        if system_helper && impersonation.is_none() {
            match UserImpersonation::new(session_id) {
                Ok(value) => impersonation = Some(value),
                Err(error) => {
                    crate::logging::error(format_args!(
                        "remote desktop audio user impersonation failed: {error:#}"
                    ));
                    reporter.report(STATE_UNAVAILABLE, Some(REASON_USER_TOKEN_UNAVAILABLE));
                    sleep_while_active(&shared, AUDIO_RETRY_DELAY);
                    continue;
                }
            }
        }
        if com.is_none() {
            match ComApartment::new() {
                Ok(value) => com = Some(value),
                Err(error) => {
                    crate::logging::error(format_args!(
                        "remote desktop audio COM initialization failed: {error:#}"
                    ));
                    reporter.report(STATE_UNAVAILABLE, Some(REASON_CAPTURE_FAILED));
                    sleep_while_active(&shared, AUDIO_RETRY_DELAY);
                    continue;
                }
            }
        }

        reporter.report(STATE_STARTING, None);
        match capture_until_state_change(
            &shared,
            &frame_tx,
            &mut reporter,
            &mut sequence,
            &mut stream_started,
            &mut last_timestamp_us,
            &mut pending_discontinuity,
        ) {
            Ok(CaptureOutcome::StateChanged) => {
                frame_tx.clear();
                pending_discontinuity = true;
            }
            Ok(CaptureOutcome::Rebuild) => {
                frame_tx.clear();
                pending_discontinuity = true;
            }
            Err(failure) => {
                frame_tx.clear();
                pending_discontinuity = true;
                crate::logging::error(format_args!(
                    "remote desktop audio capture failed: {:#}",
                    failure.detail
                ));
                reporter.report(STATE_UNAVAILABLE, Some(failure.reason));
                sleep_while_active(&shared, AUDIO_RETRY_DELAY);
            }
        }
    }
}

enum CaptureOutcome {
    StateChanged,
    Rebuild,
}

fn capture_until_state_change(
    shared: &AudioRuntimeState,
    frame_tx: &DropOldestSender<CapturedAudioFrame>,
    reporter: &mut AudioStateReporter,
    sequence: &mut u64,
    stream_started: &mut Option<Instant>,
    last_timestamp_us: &mut Option<u64>,
    pending_discontinuity: &mut bool,
) -> std::result::Result<CaptureOutcome, AudioFailure> {
    let state_epoch = shared.state_epoch.load(Ordering::Acquire);
    let session = WasapiLoopback::new()?;
    let mut encoder = OpusEncoder::new(
        AUDIO_SAMPLE_RATE as i32,
        AUDIO_CHANNELS as usize,
        Application::Audio,
    )
    .map_err(|error| AudioFailure::message(REASON_ENCODER_FAILED, error))?;
    encoder.bitrate_bps = AUDIO_BITRATE_BPS;
    encoder.use_cbr = false;

    let mut samples = VecDeque::with_capacity(AUDIO_PCM_PACKET_BYTES * 2);
    let mut opus = vec![0_u8; MAX_AUDIO_OPUS_PAYLOAD_BYTES];
    let mut last_device_check = Instant::now();
    reporter.report(STATE_PLAYING, None);

    loop {
        if shared.stopped.load(Ordering::Acquire)
            || !shared.enabled.load(Ordering::Acquire)
            || !shared.default_desktop.load(Ordering::Acquire)
            || shared.state_epoch.load(Ordering::Acquire) != state_epoch
        {
            return Ok(CaptureOutcome::StateChanged);
        }
        if shared.discontinuity.swap(false, Ordering::AcqRel) {
            *pending_discontinuity = true;
        }

        if last_device_check.elapsed() >= AUDIO_DEVICE_CHECK_INTERVAL {
            last_device_check = Instant::now();
            if session.default_device_changed()? {
                return Ok(CaptureOutcome::Rebuild);
            }
        }

        let discontinuity = session.drain_available(&mut samples)?;
        if discontinuity {
            samples.clear();
            *pending_discontinuity = true;
        }
        while samples.len() >= AUDIO_PCM_PACKET_BYTES {
            if shared.state_epoch.load(Ordering::Acquire) != state_epoch {
                frame_tx.clear();
                return Ok(CaptureOutcome::StateChanged);
            }
            let bytes = samples.drain(..AUDIO_PCM_PACKET_BYTES).collect::<Vec<_>>();
            let pcm = bytes
                .chunks_exact(AUDIO_BYTES_PER_SAMPLE)
                .map(|value| f32::from_le_bytes(value.try_into().expect("four-byte float")))
                .collect::<Vec<_>>();
            let encoded = encoder
                .encode(&pcm, AUDIO_SAMPLES_PER_FRAME as usize, &mut opus)
                .map_err(|error| AudioFailure::message(REASON_ENCODER_FAILED, error))?;
            *sequence = sequence.saturating_add(1);
            let timestamp_us = relative_frame_timestamp(stream_started, last_timestamp_us);
            let mut frame = Vec::with_capacity(AUDIO_FRAME_HEADER_LEN + encoded);
            frame.extend_from_slice(
                &AudioFrameHeader {
                    flags: if std::mem::take(pending_discontinuity) {
                        AUDIO_FLAG_DISCONTINUITY
                    } else {
                        0
                    },
                    sequence: *sequence,
                    timestamp_us,
                    sample_rate: AUDIO_SAMPLE_RATE,
                    samples_per_channel: AUDIO_SAMPLES_PER_FRAME,
                }
                .encode(),
            );
            frame.extend_from_slice(&opus[..encoded]);
            if frame.len() > MAX_AUDIO_FRAME_BYTES {
                return Err(AudioFailure::message(
                    REASON_ENCODER_FAILED,
                    "encoded Opus packet exceeded the audio frame limit",
                ));
            }
            if frame_tx
                .send(CapturedAudioFrame {
                    epoch: state_epoch,
                    bytes: frame,
                })
                .is_err()
            {
                return Ok(CaptureOutcome::StateChanged);
            }
            if shared.state_epoch.load(Ordering::Acquire) != state_epoch {
                frame_tx.clear();
                return Ok(CaptureOutcome::StateChanged);
            }
        }
        std::thread::sleep(AUDIO_CAPTURE_POLL);
    }
}

fn relative_frame_timestamp(
    stream_started: &mut Option<Instant>,
    last_timestamp_us: &mut Option<u64>,
) -> u64 {
    let now = Instant::now();
    let elapsed_us = match *stream_started {
        Some(started) => now.saturating_duration_since(started).as_micros() as u64,
        None => {
            *stream_started = Some(now);
            0
        }
    };
    let timestamp_us = last_timestamp_us
        .map(|last| last.saturating_add(AUDIO_FRAME_DURATION_US))
        .unwrap_or(0)
        .max(elapsed_us);
    *last_timestamp_us = Some(timestamp_us);
    timestamp_us
}

struct WasapiLoopback {
    enumerator: IMMDeviceEnumerator,
    device_id: String,
    client: IAudioClient,
    capture: IAudioCaptureClient,
}

impl WasapiLoopback {
    fn new() -> std::result::Result<Self, AudioFailure> {
        unsafe {
            let enumerator: IMMDeviceEnumerator =
                CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
                    .map_err(|error| AudioFailure::windows("create device enumerator", error))?;
            let device = enumerator
                .GetDefaultAudioEndpoint(eRender, eMultimedia)
                .map_err(|error| AudioFailure::windows("get default render endpoint", error))?;
            let device_id = endpoint_id(&device)?;
            let client: IAudioClient = device
                .Activate(CLSCTX_ALL, None)
                .map_err(|error| AudioFailure::windows("activate audio client", error))?;
            let format = WAVEFORMATEX {
                wFormatTag: WAVE_FORMAT_IEEE_FLOAT as u16,
                nChannels: AUDIO_CHANNELS as u16,
                nSamplesPerSec: AUDIO_SAMPLE_RATE,
                nAvgBytesPerSec: AUDIO_SAMPLE_RATE * AUDIO_BYTES_PER_FRAME as u32,
                nBlockAlign: AUDIO_BYTES_PER_FRAME as u16,
                wBitsPerSample: (AUDIO_BYTES_PER_SAMPLE * 8) as u16,
                cbSize: 0,
            };
            client
                .Initialize(
                    AUDCLNT_SHAREMODE_SHARED,
                    AUDCLNT_STREAMFLAGS_LOOPBACK
                        | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM
                        | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
                    AUDIO_PACKET_DURATION.as_nanos() as i64 / 100,
                    0,
                    &format,
                    None,
                )
                .map_err(|error| AudioFailure::windows("initialize loopback stream", error))?;
            let capture = client
                .GetService::<IAudioCaptureClient>()
                .map_err(|error| AudioFailure::windows("get audio capture client", error))?;
            client
                .Start()
                .map_err(|error| AudioFailure::windows("start loopback stream", error))?;
            Ok(Self {
                enumerator,
                device_id,
                client,
                capture,
            })
        }
    }

    fn default_device_changed(&self) -> std::result::Result<bool, AudioFailure> {
        unsafe {
            let device = self
                .enumerator
                .GetDefaultAudioEndpoint(eRender, eMultimedia)
                .map_err(|error| AudioFailure::windows("get default render endpoint", error))?;
            Ok(endpoint_id(&device)? != self.device_id)
        }
    }

    fn drain_available(
        &self,
        output: &mut VecDeque<u8>,
    ) -> std::result::Result<bool, AudioFailure> {
        let mut discontinuity = false;
        unsafe {
            loop {
                let available = self
                    .capture
                    .GetNextPacketSize()
                    .map_err(|error| AudioFailure::windows("query audio packet", error))?;
                if available == 0 {
                    return Ok(discontinuity);
                }

                let mut data = null_mut();
                let mut frames = 0_u32;
                let mut flags = 0_u32;
                self.capture
                    .GetBuffer(&mut data, &mut frames, &mut flags, None, None)
                    .map_err(|error| AudioFailure::windows("read audio packet", error))?;
                let bytes = frames as usize * AUDIO_BYTES_PER_FRAME;
                if flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0 {
                    output.extend(std::iter::repeat_n(0, bytes));
                } else if bytes > 0 {
                    if data.is_null() {
                        let _ = self.capture.ReleaseBuffer(frames);
                        return Err(AudioFailure::message(
                            REASON_CAPTURE_FAILED,
                            "WASAPI returned a null non-silent capture buffer",
                        ));
                    }
                    output.extend(std::slice::from_raw_parts(data, bytes).iter().copied());
                }
                discontinuity |= flags & AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY.0 as u32 != 0;
                self.capture
                    .ReleaseBuffer(frames)
                    .map_err(|error| AudioFailure::windows("release audio packet", error))?;
            }
        }
    }
}

fn endpoint_id(
    device: &windows::Win32::Media::Audio::IMMDevice,
) -> std::result::Result<String, AudioFailure> {
    unsafe {
        let value = device
            .GetId()
            .map_err(|error| AudioFailure::windows("get render endpoint ID", error))?;
        let result = value.to_string().map_err(|error| {
            AudioFailure::message(
                REASON_CAPTURE_FAILED,
                format!("render endpoint ID is not valid UTF-16: {error}"),
            )
        });
        CoTaskMemFree(Some(value.0.cast()));
        result
    }
}

impl Drop for WasapiLoopback {
    fn drop(&mut self) {
        unsafe {
            let _ = self.client.Stop();
        }
    }
}

struct AudioStateReporter {
    tx: mpsc::Sender<String>,
    last: Option<(&'static str, Option<&'static str>)>,
}

impl AudioStateReporter {
    fn new(tx: mpsc::Sender<String>) -> Self {
        Self { tx, last: None }
    }

    fn report(&mut self, state: &'static str, reason: Option<&'static str>) {
        if self.last == Some((state, reason)) {
            return;
        }
        self.last = Some((state, reason));
        let mut message = serde_json::json!({"type":"audio_state", "state":state});
        if let Some(reason) = reason {
            message["reason"] = serde_json::Value::String(reason.to_string());
        }
        let _ = self.tx.blocking_send(message.to_string());
    }
}

struct AudioFailure {
    reason: &'static str,
    detail: anyhow::Error,
}

impl AudioFailure {
    fn windows(context: &'static str, error: WindowsError) -> Self {
        let code = error.code();
        let reason = if code == AUDCLNT_E_SERVICE_NOT_RUNNING {
            REASON_AUDIO_SERVICE_UNAVAILABLE
        } else if code == AUDCLNT_E_DEVICE_INVALIDATED || code == AUDCLNT_E_RESOURCES_INVALIDATED {
            REASON_DEVICE_INVALIDATED
        } else if code == HRESULT::from_win32(ERROR_NOT_FOUND.0) {
            REASON_NO_OUTPUT_DEVICE
        } else {
            REASON_CAPTURE_FAILED
        };
        Self {
            reason,
            detail: anyhow::Error::new(error).context(context),
        }
    }

    fn message(reason: &'static str, message: impl Into<String>) -> Self {
        Self {
            reason,
            detail: anyhow::Error::msg(message.into()),
        }
    }
}

struct ComApartment;

impl ComApartment {
    fn new() -> Result<Self> {
        unsafe { CoInitializeEx(None, COINIT_MULTITHREADED)? };
        Ok(Self)
    }
}

impl Drop for ComApartment {
    fn drop(&mut self) {
        unsafe { CoUninitialize() };
    }
}

struct UserImpersonation {
    token: HANDLE,
}

impl UserImpersonation {
    fn new(session_id: u32) -> Result<Self> {
        unsafe {
            let mut token = HANDLE::default();
            WTSQueryUserToken(session_id, &mut token)
                .context("failed to acquire interactive user token for audio")?;
            if let Err(error) = ImpersonateLoggedOnUser(token) {
                let _ = CloseHandle(token);
                return Err(error).context("failed to impersonate interactive user for audio");
            }
            Ok(Self { token })
        }
    }
}

impl Drop for UserImpersonation {
    fn drop(&mut self) {
        unsafe {
            let _ = RevertToSelf();
            let _ = CloseHandle(self.token);
        }
    }
}

fn sleep_while_active(shared: &AudioRuntimeState, duration: Duration) {
    let deadline = std::time::Instant::now() + duration;
    while std::time::Instant::now() < deadline
        && !shared.stopped.load(Ordering::Acquire)
        && shared.enabled.load(Ordering::Acquire)
        && shared.default_desktop.load(Ordering::Acquire)
    {
        std::thread::sleep(AUDIO_IDLE_POLL);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_opus_is_negotiated() {
        assert!(negotiated(Some("opus")));
        assert!(negotiated(Some("OPUS")));
        assert!(!negotiated(None));
        assert!(!negotiated(Some("pcm")));
    }

    #[test]
    fn audio_wire_settings_are_stable() {
        assert_eq!(AUDIO_SAMPLE_RATE, 48_000);
        assert_eq!(AUDIO_CHANNELS, 2);
        assert_eq!(AUDIO_SAMPLES_PER_FRAME, 960);
        assert_eq!(AUDIO_PACKET_DURATION, Duration::from_millis(20));
        assert_eq!(AUDIO_FRAME_DURATION_US, 20_000);
        assert_eq!(AUDIO_BITRATE_BPS, 96_000);
        assert_eq!(MAX_AUDIO_OPUS_PAYLOAD_BYTES, 1_275);
    }

    #[test]
    fn runtime_rejects_frames_from_a_stale_capture_epoch() {
        let (frames, _receiver) = super::super::drop_oldest_channel(1);
        let shared = Arc::new(AudioRuntimeState {
            enabled: AtomicBool::new(false),
            default_desktop: AtomicBool::new(true),
            discontinuity: AtomicBool::new(true),
            state_epoch: AtomicU64::new(0),
            stopped: AtomicBool::new(false),
        });
        let runtime = AudioRuntime { shared, frames };
        let stale = CapturedAudioFrame {
            epoch: 0,
            bytes: vec![1],
        };

        assert!(!runtime.accepts(&stale));
        assert!(runtime.set_enabled(true));
        assert!(!runtime.accepts(&stale));
        let current = CapturedAudioFrame {
            epoch: 1,
            bytes: vec![2],
        };
        assert!(runtime.accepts(&current));
        runtime.set_default_desktop(false);
        assert!(!runtime.accepts(&current));
    }

    #[test]
    fn frame_timestamps_are_relative_monotonic_and_preserve_real_gaps() {
        let mut started = None;
        let mut last = None;
        assert_eq!(relative_frame_timestamp(&mut started, &mut last), 0);
        assert_eq!(relative_frame_timestamp(&mut started, &mut last), 20_000);

        started = Some(Instant::now() - Duration::from_millis(100));
        let after_gap = relative_frame_timestamp(&mut started, &mut last);
        assert!(after_gap >= 100_000);
    }
}
