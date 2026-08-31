use std::{
    collections::{HashSet, VecDeque},
    ffi::{OsStr, c_void},
    mem::{size_of, zeroed},
    os::windows::{ffi::OsStrExt, io::AsRawHandle, process::CommandExt},
    process::{Child, Command, Stdio},
    ptr::{null, null_mut},
    sync::{Arc, Mutex},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, anyhow, bail};
use futures_util::{SinkExt, StreamExt};
use image::{DynamicImage, ImageBuffer, Rgb, imageops::FilterType};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::windows::named_pipe::{ClientOptions, NamedPipeClient, ServerOptions},
    sync::{mpsc, oneshot, watch},
};
use tokio_tungstenite::{
    connect_async_with_config,
    tungstenite::{
        Message, client::IntoClientRequest, http::HeaderValue, protocol::WebSocketConfig,
    },
};
use uuid::Uuid;
use windows::{
    Win32::{
        Foundation::{
            CloseHandle, FreeLibrary, GENERIC_WRITE, HANDLE, HMODULE, LocalFree, STILL_ACTIVE,
        },
        Graphics::{
            Direct3D::{D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL_11_0},
            Direct3D11::{
                D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_MAP_READ,
                D3D11_MAPPED_SUBRESOURCE, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC,
                D3D11_USAGE_STAGING, D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext,
                ID3D11Texture2D,
            },
            Dxgi::{
                Common::{
                    DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_B8G8R8A8_UNORM_SRGB,
                    DXGI_MODE_ROTATION, DXGI_MODE_ROTATION_ROTATE90, DXGI_MODE_ROTATION_ROTATE180,
                    DXGI_MODE_ROTATION_ROTATE270,
                },
                CreateDXGIFactory1, DXGI_ERROR_ACCESS_LOST, DXGI_ERROR_WAIT_TIMEOUT,
                DXGI_OUTDUPL_FRAME_INFO, IDXGIAdapter1, IDXGIFactory1, IDXGIOutput1,
                IDXGIOutputDuplication, IDXGIResource,
            },
            Gdi::{
                BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BitBlt, CreateCompatibleBitmap,
                CreateCompatibleDC, DIB_RGB_COLORS, DeleteDC, DeleteObject, GetDC, GetDIBits,
                ReleaseDC, SRCCOPY, SelectObject,
            },
        },
        Security::{
            Authorization::{
                ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
                SDDL_REVISION_1,
            },
            CreateRestrictedToken, DISABLE_MAX_PRIVILEGE, DuplicateTokenEx, GetTokenInformation,
            ImpersonateLoggedOnUser, PSECURITY_DESCRIPTOR, RevertToSelf, SECURITY_ATTRIBUTES,
            SecurityImpersonation, TOKEN_ALL_ACCESS, TOKEN_DUPLICATE, TOKEN_QUERY, TOKEN_USER,
            TokenPrimary, TokenUser,
        },
        System::{
            Environment::{CreateEnvironmentBlock, DestroyEnvironmentBlock},
            LibraryLoader::{GetProcAddress, LoadLibraryW},
            Pipes::GetNamedPipeClientProcessId,
            Registry::{HKEY_LOCAL_MACHINE, RRF_RT_REG_DWORD, RegGetValueW},
            RemoteDesktop::{
                ProcessIdToSessionId, WTS_CURRENT_SERVER_HANDLE, WTS_PROCESS_INFOW,
                WTS_SESSION_INFOW, WTSActive, WTSEnumerateProcessesW, WTSEnumerateSessionsW,
                WTSFreeMemory, WTSGetActiveConsoleSessionId,
            },
            StationsAndDesktops::{
                CloseDesktop, DESKTOP_ACCESS_FLAGS, DESKTOP_CREATEMENU, DESKTOP_CREATEWINDOW,
                DESKTOP_ENUMERATE, DESKTOP_HOOKCONTROL, DESKTOP_READOBJECTS, DESKTOP_SWITCHDESKTOP,
                DESKTOP_WRITEOBJECTS, GetProcessWindowStation, GetThreadDesktop,
                GetUserObjectInformationW, HDESK, OpenInputDesktop, OpenWindowStationW,
                SetProcessWindowStation, SetThreadDesktop, UOI_NAME,
            },
            Threading::{
                CREATE_NO_WINDOW, CREATE_UNICODE_ENVIRONMENT, CreateProcessAsUserW,
                GetCurrentProcess, GetCurrentProcessId, GetCurrentThreadId, GetExitCodeProcess,
                OpenProcess, OpenProcessToken, PROCESS_INFORMATION,
                PROCESS_QUERY_LIMITED_INFORMATION, STARTUPINFOW, TerminateProcess,
                WaitForSingleObject,
            },
        },
        UI::Input::KeyboardAndMouse::{
            INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYEVENTF_EXTENDEDKEY,
            KEYEVENTF_KEYUP, MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN,
            MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE,
            MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_WHEEL, MOUSEINPUT, SendInput,
            VIRTUAL_KEY,
        },
        UI::WindowsAndMessaging::{
            GetSystemMetrics, IDCANCEL, IDOK, IDYES, MB_DEFBUTTON2, MB_ICONWARNING, MB_OKCANCEL,
            MB_SETFOREGROUND, MB_SYSTEMMODAL, MB_TOPMOST, MB_YESNO, MessageBoxW, SM_CXSCREEN,
            SM_CYSCREEN,
        },
    },
    core::{Interface, PCWSTR, PWSTR},
};

use super::{
    AUDIO_CHANNEL_CAPACITY, AUDIO_CHANNELS, AUDIO_CODEC_NAME, AUDIO_FRAME_HEADER_LEN,
    AUDIO_SAMPLE_RATE, AUDIO_SAMPLES_PER_FRAME, AdaptiveSettings, AudioFrameHeader,
    CONTROL_RATE_DROP_LIMIT, ControlRateLimiter, DATA_CHANNEL_JOIN_TIMEOUT, DesktopControl,
    DesktopOpenRequest, DesktopOptions, FrameHeader, INPUT_RELEASE_ACK_TIMEOUT,
    MAX_AUDIO_FRAME_BYTES, MAX_CONTROL_BYTES, MAX_FRAME_BYTES, MAX_JPEG_QUALITY, MIN_JPEG_QUALITY,
    absolute_pointer_coordinate, dom_code_to_vk, dom_code_uses_extended_key, drop_oldest_channel,
    error_reason, scaled_dimensions, wait_for_input_release_ack, windows_audio,
    windows_command_line,
};
use crate::{
    config::{AgentConfig, RemoteDesktopConsent},
    models::AgentInbound,
    outbound::AgentEventSender,
};

const CREATE_NO_WINDOW_FLAG: u32 = 0x08000000;
const PIPE_FRAME: u8 = 1;
const PIPE_CONTROL: u8 = 2;
const PIPE_INTERNAL: u8 = 3;
const PIPE_AUDIO: u8 = 4;
const INTERNAL_STOP: &[u8] = b"stop";
const INTERNAL_STOPPED: &[u8] = b"stopped";
const INTERNAL_FATAL_PREFIX: &[u8] = b"fatal:";
const INTERNAL_AUDIO_CONTROL_ACK_PREFIX: &[u8] = b"audio_control_ack:";
const INTERNAL_INPUT_BARRIER_PREFIX: &[u8] = b"input_barrier:";
const INTERNAL_INPUT_READY_PREFIX: &[u8] = b"input_ready:";
const PIPE_MAX_PACKET: usize = MAX_FRAME_BYTES + 1024;
const SOCKET_SEND_TIMEOUT: Duration = Duration::from_secs(5);
const PIPE_WRITE_TIMEOUT: Duration = Duration::from_secs(5);
const INPUT_ERROR_LOG_INTERVAL: Duration = Duration::from_secs(5);
const DISPLAY_PREPARE_TIMEOUT: Duration = Duration::from_secs(10);
const DESKTOP_BINDING_LOST: &str = "input_desktop_binding_lost";
const DESKTOP_HANDLE_CLEANUP_FAILED: &str = "desktop_handle_cleanup_failed";
const LOCAL_SYSTEM_SID: &str = "S-1-5-18";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LocalSessionIndicatorAction {
    EndSession,
    Dismiss,
}

const DEVICE_PROBE_REQUEST: &[u8] = b"probe";
const DEVICE_PROBE_MAX_RESPONSE: usize = 1024;
const DEVICE_PROBE_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct AudioControlAck {
    enabled: bool,
    changed: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    generation: Option<u64>,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct HelperControlEnvelope {
    input_generation: u64,
    control: DesktopControl,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct InputBarrier {
    generation: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct InputReady {
    generation: u64,
    sequence: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct InputGenerationState {
    generation: u64,
    desktop: Option<String>,
    ready: bool,
}

#[derive(Debug, Default)]
struct InputGenerationFence {
    state: Mutex<InputGenerationState>,
}

impl Default for InputGenerationState {
    fn default() -> Self {
        Self {
            generation: 0,
            desktop: None,
            ready: false,
        }
    }
}

impl InputGenerationFence {
    fn begin_transition(&self, desktop: &str) -> u64 {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.generation = state.generation.wrapping_add(1).max(1);
        state.desktop = Some(desktop.to_string());
        state.ready = false;
        state.generation
    }

    fn mark_ready(&self, generation: u64) -> bool {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if state.generation != generation {
            return false;
        }
        state.ready = true;
        true
    }

    fn generation(&self) -> u64 {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .generation
    }

    fn ready_for_generation(&self, generation: u64) -> bool {
        let state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.ready && state.generation == generation
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
struct RelayInputGate {
    generation: u64,
    ready: bool,
    awaiting_feedback_sequence: Option<u64>,
}

impl RelayInputGate {
    fn begin_generation(&mut self, generation: u64) {
        if generation < self.generation {
            return;
        }
        self.generation = generation;
        self.ready = false;
        self.awaiting_feedback_sequence = None;
    }

    fn arm_after_first_frame(&mut self, sequence: u64) {
        self.ready = false;
        self.awaiting_feedback_sequence = Some(sequence);
    }

    fn observe_feedback(&mut self, sequence: u64) {
        if self
            .awaiting_feedback_sequence
            .is_some_and(|expected| sequence >= expected)
        {
            self.awaiting_feedback_sequence = None;
            self.ready = true;
        }
    }

    fn allows(&self, control: &DesktopControl) -> bool {
        self.ready
            || matches!(
                control,
                DesktopControl::AudioControl { .. } | DesktopControl::Feedback { .. }
            )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PendingAudioControl {
    order: u64,
    enabled: bool,
    generation: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AudioRelayPhase {
    Disabled,
    AwaitingControlAck,
    AwaitingDiscontinuity,
    Playing,
}

struct AudioRelayGate {
    next_order: u64,
    latest: Option<PendingAudioControl>,
    pending: VecDeque<PendingAudioControl>,
    phase: AudioRelayPhase,
}

impl AudioRelayGate {
    fn new() -> Self {
        Self {
            next_order: 0,
            latest: None,
            pending: VecDeque::new(),
            phase: AudioRelayPhase::Disabled,
        }
    }

    fn register_control(&mut self, generation: Option<u64>, enabled: bool) -> Result<()> {
        let previously_enabled = self.latest.is_some_and(|control| control.enabled);
        if let (Some(previous), Some(generation)) = (
            self.latest.and_then(|control| control.generation),
            generation,
        ) && generation <= previous
        {
            bail!("desktop audio control generation is not increasing")
        }
        self.next_order = self
            .next_order
            .checked_add(1)
            .ok_or_else(|| anyhow!("desktop audio control order overflow"))?;
        let control = PendingAudioControl {
            order: self.next_order,
            enabled,
            generation,
        };
        self.latest = Some(control);
        self.pending.push_back(control);
        self.phase = if !enabled {
            AudioRelayPhase::Disabled
        } else if !previously_enabled {
            AudioRelayPhase::AwaitingControlAck
        } else {
            self.phase
        };
        Ok(())
    }

    /// Returns the generation that must be acknowledged to the backend before audio resumes.
    fn acknowledge(&mut self, ack: AudioControlAck) -> Result<Option<u64>> {
        let Some(expected) = self.pending.front().copied() else {
            bail!("unexpected desktop audio control acknowledgement")
        };
        if expected.enabled != ack.enabled || expected.generation != ack.generation {
            bail!("out-of-order desktop audio control acknowledgement")
        }
        self.pending.pop_front();
        if self.latest != Some(expected) {
            return Ok(None);
        }
        if !ack.enabled {
            self.phase = AudioRelayPhase::Disabled;
            return Ok(None);
        }
        if ack.changed {
            self.phase = AudioRelayPhase::AwaitingDiscontinuity;
            return Ok(ack.generation);
        }
        Ok(None)
    }

    fn observe_status(&mut self, status: &str) {
        if !matches!(
            self.phase,
            AudioRelayPhase::Playing | AudioRelayPhase::AwaitingDiscontinuity
        ) || !audio_state_clears_queue(status)
        {
            return;
        }
        self.phase = AudioRelayPhase::AwaitingDiscontinuity;
    }

    fn accepts_audio(&mut self, frame: &[u8]) -> bool {
        match self.phase {
            AudioRelayPhase::Playing => true,
            AudioRelayPhase::AwaitingDiscontinuity
                if AudioFrameHeader::decode(frame)
                    .is_ok_and(|header| header.flags & super::AUDIO_FLAG_DISCONTINUITY != 0) =>
            {
                self.phase = AudioRelayPhase::Playing;
                true
            }
            _ => false,
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct SessionAuthorization {
    session_id: u32,
    unattended: bool,
    managed_service: bool,
}

struct SecureAttentionGate {
    authorization: SessionAuthorization,
    policy_confirmed: bool,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct SessionPolicyMessage {
    #[serde(rename = "type")]
    kind: String,
    access_mode: String,
    local_consent_required: bool,
    secure_desktop_control: bool,
    secure_attention_allowed: bool,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct DesktopStateMessage {
    #[serde(rename = "type")]
    kind: String,
    desktop: String,
    context: String,
    controllable: bool,
}

impl SecureAttentionGate {
    fn new(authorization: SessionAuthorization) -> Self {
        Self {
            authorization,
            policy_confirmed: false,
        }
    }

    fn observe_status(&mut self, status: &str) -> Result<()> {
        let value: serde_json::Value = serde_json::from_str(status)?;
        match value.get("type").and_then(serde_json::Value::as_str) {
            Some("session_policy") => {
                let policy: SessionPolicyMessage = serde_json::from_value(value)
                    .context("invalid desktop helper session_policy")?;
                let expected_mode = if self.authorization.unattended {
                    "unattended"
                } else {
                    "local_consent"
                };
                if policy.kind != "session_policy"
                    || policy.access_mode != expected_mode
                    || policy.local_consent_required == self.authorization.unattended
                    || policy.secure_desktop_control != self.authorization.unattended
                    || policy.secure_attention_allowed != self.authorization.unattended
                {
                    bail!("desktop helper session policy did not match service authorization")
                }
                self.policy_confirmed = true;
            }
            Some("desktop_state") => {
                let desktop: DesktopStateMessage = serde_json::from_value(value)
                    .context("invalid desktop helper desktop_state")?;
                let consistent = matches!(
                    (desktop.desktop.as_str(), desktop.context.as_str()),
                    ("default", "default") | ("secure", "winlogon") | ("other", "other")
                );
                if desktop.kind != "desktop_state"
                    || !matches!(desktop.desktop.as_str(), "default" | "secure" | "other")
                    || !matches!(desktop.context.as_str(), "default" | "winlogon" | "other")
                    || !consistent
                {
                    bail!("desktop helper reported an invalid desktop state")
                }
                let _ = desktop.controllable;
            }
            _ => {}
        }
        Ok(())
    }

    fn allowed(&self) -> bool {
        self.authorization.unattended && self.policy_confirmed
    }
}

fn secure_attention_notice(code: &str) -> String {
    serde_json::json!({
        "type":"notice",
        "code":code,
        "message":"Windows 未允许发送 Ctrl+Alt+Del"
    })
    .to_string()
}

fn secure_attention_result(succeeded: bool, code: Option<&str>) -> String {
    if succeeded {
        serde_json::json!({
            "type":"secure_attention_result",
            "status":"success"
        })
        .to_string()
    } else {
        serde_json::json!({
            "type":"secure_attention_result",
            "status":"failed",
            "code":code.unwrap_or("secure_attention_unavailable")
        })
        .to_string()
    }
}
// WINSTA_ALL_ACCESS from winuser.h; windows 0.52 does not expose the aggregate constant.
const WINSTA_ALL_ACCESS_MASK: u32 = 0x0000_037f;
// SetThreadDesktop constrains subsequent USER calls to the rights on this handle. Generic write
// supplies the journal playback rights used by software input injection.
const INPUT_DESKTOP_ACCESS: DESKTOP_ACCESS_FLAGS = DESKTOP_ACCESS_FLAGS(
    DESKTOP_CREATEMENU.0
        | DESKTOP_CREATEWINDOW.0
        | DESKTOP_ENUMERATE.0
        | DESKTOP_HOOKCONTROL.0
        | DESKTOP_READOBJECTS.0
        | DESKTOP_SWITCHDESKTOP.0
        | DESKTOP_WRITEOBJECTS.0
        | GENERIC_WRITE.0,
);

pub async fn run_session(
    config: AgentConfig,
    request: DesktopOpenRequest,
    outbound: AgentEventSender,
    close: oneshot::Receiver<String>,
) -> Result<String> {
    let established = tokio::time::timeout(
        DATA_CHANNEL_JOIN_TIMEOUT,
        establish_session(&config, &request),
    )
    .await
    .map_err(|_| anyhow!("data_channel_timeout"))??;
    let (socket, pipe, mut child, authorization) = established;
    let _ = outbound.send(AgentInbound::DesktopOpened {
        session_id: request.session_id.clone(),
    });

    let mut result = relay(socket, pipe, close, authorization).await;
    if result.is_err()
        && let Some(exit_code) = child.exit_code()
    {
        result = result.map_err(|error| {
            error.context(format!("desktop helper exited with code 0x{exit_code:08X}"))
        });
    }
    child.terminate();
    result
}

async fn establish_session(
    config: &AgentConfig,
    request: &DesktopOpenRequest,
) -> Result<(
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
    tokio::net::windows::named_pipe::NamedPipeServer,
    HelperProcess,
    SessionAuthorization,
)> {
    if !crate::remote_access::ensure_ready_for_remote_desktop().await {
        bail!("device_probe_failed")
    }
    if let Some(code) = crate::remote_access::display_readiness_error(config) {
        bail!(code)
    }
    let (target, user_sid) =
        helper_target(config.remote_desktop_consent == RemoteDesktopConsent::Unattended)?;
    let pipe_name = format!(r"\\.\pipe\omrd-{}", Uuid::new_v4());
    let pipe = create_private_pipe(&pipe_name, &user_sid)?;

    let min_fps = request.min_fps.clamp(1, 12);
    let audio_codec = request
        .audio_codec
        .as_deref()
        .filter(|codec| windows_audio::negotiated(Some(codec)))
        .map(|_| AUDIO_CODEC_NAME.to_string());
    let managed_unattended = matches!(target, HelperTarget::ServiceSession { .. })
        && config.remote_desktop_consent == RemoteDesktopConsent::Unattended
        && current_process_is_local_system()?;
    let options = DesktopOptions {
        pipe: pipe_name,
        max_width: request.max_width.clamp(320, 1920),
        max_height: request.max_height.clamp(240, 1080),
        min_fps,
        max_fps: request.max_fps.clamp(min_fps, 12),
        jpeg_quality: request
            .jpeg_quality
            .clamp(MIN_JPEG_QUALITY, MAX_JPEG_QUALITY),
        audio_codec,
        system_helper: matches!(target, HelperTarget::ServiceSession { .. }),
        unattended: managed_unattended,
        display_source: crate::remote_access::expected_display_source(config),
    };
    let mut child = spawn_helper(&options, target, config)?;

    let mut ws_request = desktop_websocket_url(config, &request.session_id)?
        .as_str()
        .into_client_request()
        .context("invalid desktop websocket URL")?;
    ws_request.headers_mut().insert(
        "authorization",
        HeaderValue::from_str(&format!("Bearer {}", request.stream_token))
            .context("invalid desktop stream token")?,
    );
    let connected = tokio::try_join!(
        async {
            connect_async_with_config(
                ws_request,
                Some(
                    WebSocketConfig::default()
                        .max_message_size(Some(MAX_CONTROL_BYTES))
                        .max_frame_size(Some(MAX_CONTROL_BYTES)),
                ),
                false,
            )
            .await
            .context("failed to connect desktop data websocket")
        },
        async {
            pipe.connect()
                .await
                .context("desktop helper did not connect to private pipe")
        }
    );
    let ((socket, _), ()) = match connected {
        Ok(value) => value,
        Err(error) => {
            child.terminate();
            return Err(error.into());
        }
    };
    if let Err(error) = validate_pipe_client(&pipe, child.pid(), target.session_id()) {
        child.terminate();
        return Err(error);
    }
    Ok((
        socket,
        pipe,
        child,
        SessionAuthorization {
            session_id: target.session_id(),
            unattended: managed_unattended,
            managed_service: matches!(target, HelperTarget::ServiceSession { .. }),
        },
    ))
}

fn create_private_pipe(
    name: &str,
    user_sid: &str,
) -> Result<tokio::net::windows::named_pipe::NamedPipeServer> {
    unsafe {
        // Service helpers run as LocalSystem. Foreground helpers additionally need the current
        // user's SID so development mode keeps working without weakening the service pipe.
        let descriptor = if user_sid == "SY" {
            "D:P(A;;GA;;;SY)".to_string()
        } else {
            format!("D:P(A;;GA;;;SY)(A;;GA;;;{user_sid})")
        };
        let sddl = wide(descriptor);
        let mut descriptor = PSECURITY_DESCRIPTOR::default();
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            PCWSTR(sddl.as_ptr()),
            SDDL_REVISION_1,
            &mut descriptor,
            None,
        )?;
        let mut attributes = SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: descriptor.0,
            bInheritHandle: false.into(),
        };
        let pipe = ServerOptions::new()
            .first_pipe_instance(true)
            .reject_remote_clients(true)
            .create_with_security_attributes_raw(
                name,
                (&mut attributes as *mut SECURITY_ATTRIBUTES).cast(),
            )
            .context("failed to create private desktop helper pipe");
        let _ = LocalFree(Some(windows::Win32::Foundation::HLOCAL(descriptor.0)));
        pipe
    }
}

pub(crate) fn probe_target_session_devices(
    unattended: bool,
) -> Result<crate::remote_access::DeviceProbeSnapshot> {
    static CLIENT: std::sync::OnceLock<Mutex<Option<DeviceProbeClient>>> =
        std::sync::OnceLock::new();

    let session_id =
        select_active_session(unattended).map_err(|_| anyhow!("device_probe_failed"))?;
    let clients = CLIENT.get_or_init(|| Mutex::new(None));
    let mut client = clients
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if client
        .as_ref()
        .is_some_and(|client| client.session_id != session_id)
    {
        *client = None;
    }
    if client.is_none() {
        *client = Some(
            DeviceProbeClient::connect(session_id).map_err(|_| anyhow!("device_probe_failed"))?,
        );
    }
    match client
        .as_mut()
        .expect("device probe client was initialized")
        .probe()
    {
        Ok(snapshot) => Ok(snapshot),
        Err(_) => {
            *client = None;
            Err(anyhow!("device_probe_failed"))
        }
    }
}

struct DeviceProbeClient {
    runtime: tokio::runtime::Runtime,
    pipe: tokio::net::windows::named_pipe::NamedPipeServer,
    process: HelperProcess,
    session_id: u32,
}

impl DeviceProbeClient {
    fn connect(session_id: u32) -> Result<Self> {
        let pipe_name = format!(r"\\.\pipe\om-device-probe-{}", Uuid::new_v4());
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_io()
            .enable_time()
            .build()
            .context("device_probe_failed")?;
        let pipe = create_device_probe_pipe(&runtime, &pipe_name)?;
        let mut process = spawn_device_probe_in_session(&pipe_name, session_id)?;
        let connected = runtime.block_on(async {
            tokio::time::timeout(DEVICE_PROBE_TIMEOUT, pipe.connect())
                .await
                .context("device_probe_failed")?
                .context("device_probe_failed")
        });
        if connected.is_err() {
            process.terminate();
            return Err(anyhow!("device_probe_failed"));
        }
        if validate_pipe_client(&pipe, process.pid(), session_id).is_err() {
            process.terminate();
            return Err(anyhow!("device_probe_failed"));
        }
        Ok(Self {
            runtime,
            pipe,
            process,
            session_id,
        })
    }

    fn probe(&mut self) -> Result<crate::remote_access::DeviceProbeSnapshot> {
        if self.process.exit_code().is_some() {
            bail!("device_probe_failed")
        }
        let bytes = self.runtime.block_on(async {
            tokio::time::timeout(DEVICE_PROBE_TIMEOUT, async {
                write_packet(&mut self.pipe, PIPE_INTERNAL, DEVICE_PROBE_REQUEST).await?;
                let (kind, bytes) = read_packet(&mut self.pipe).await?;
                if kind != PIPE_CONTROL || bytes.len() > DEVICE_PROBE_MAX_RESPONSE {
                    bail!("device_probe_failed")
                }
                Ok::<_, anyhow::Error>(bytes)
            })
            .await
            .context("device_probe_failed")?
        })?;
        let snapshot = serde_json::from_slice::<crate::remote_access::DeviceProbeSnapshot>(&bytes)
            .context("device_probe_failed")?;
        snapshot.validate(self.session_id)?;
        Ok(snapshot)
    }
}

fn create_device_probe_pipe(
    runtime: &tokio::runtime::Runtime,
    pipe_name: &str,
) -> Result<tokio::net::windows::named_pipe::NamedPipeServer> {
    // Named pipes register with the I/O runtime that is current at creation.
    // The service coordinator runs on a plain thread, so enter the client-owned runtime.
    let _runtime_guard = runtime.enter();
    create_private_pipe(pipe_name, LOCAL_SYSTEM_SID)
}

pub async fn run_device_probe(pipe_name: &str) -> Result<()> {
    let session_id = current_session_id().map_err(|_| anyhow!("device_probe_failed"))?;
    if session_id == 0 || !current_process_is_local_system().unwrap_or(false) {
        bail!("device_probe_failed")
    }
    let mut pipe = tokio::time::timeout(DEVICE_PROBE_TIMEOUT, connect_pipe(pipe_name))
        .await
        .context("device_probe_failed")??;
    loop {
        let (kind, request) = read_packet(&mut pipe)
            .await
            .map_err(|_| anyhow!("device_probe_failed"))?;
        if kind != PIPE_INTERNAL || request != DEVICE_PROBE_REQUEST {
            bail!("device_probe_failed")
        }
        let snapshot = crate::remote_access::probe_current_session_devices(session_id)?;
        let response = serde_json::to_vec(&snapshot).context("device_probe_failed")?;
        if response.len() > DEVICE_PROBE_MAX_RESPONSE {
            bail!("device_probe_failed")
        }
        write_packet(&mut pipe, PIPE_CONTROL, &response)
            .await
            .map_err(|_| anyhow!("device_probe_failed"))?;
    }
}

fn validate_pipe_client(
    pipe: &tokio::net::windows::named_pipe::NamedPipeServer,
    expected_pid: u32,
    expected_session_id: u32,
) -> Result<()> {
    unsafe {
        let handle = HANDLE(pipe.as_raw_handle());
        let mut client_pid = 0_u32;
        GetNamedPipeClientProcessId(handle, &mut client_pid)
            .context("failed to identify desktop helper pipe client")?;
        if client_pid != expected_pid {
            bail!("desktop helper pipe was claimed by an unexpected process")
        }
        let mut client_session_id = 0_u32;
        ProcessIdToSessionId(client_pid, &mut client_session_id)
            .context("failed to identify desktop helper client session")?;
        if client_session_id != expected_session_id {
            bail!("desktop helper connected from an unexpected Windows session")
        }
        Ok(())
    }
}

async fn relay(
    socket: tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    pipe: tokio::net::windows::named_pipe::NamedPipeServer,
    mut close: oneshot::Receiver<String>,
    authorization: SessionAuthorization,
) -> Result<String> {
    let (mut ws_write, mut ws_read) = socket.split();
    let (pipe_read, mut pipe_write) = tokio::io::split(pipe);
    let (frame_tx, mut frame_rx) = watch::channel::<Option<Vec<u8>>>(None);
    let (audio_tx, audio_rx) = drop_oldest_channel(AUDIO_CHANNEL_CAPACITY);
    let (status_tx, mut status_rx) = mpsc::channel::<String>(32);
    let (input_barrier_tx, mut input_barrier_rx) = mpsc::channel::<InputBarrier>(8);
    let (input_ready_tx, mut input_ready_rx) = mpsc::channel::<InputReady>(8);
    let (release_ack_tx, mut release_ack_rx) = mpsc::channel::<()>(1);
    let (audio_ack_tx, mut audio_ack_rx) = mpsc::channel::<AudioControlAck>(32);
    let (fatal_tx, mut fatal_rx) = mpsc::channel::<String>(1);
    let reader = tokio::spawn(pipe_reader(
        pipe_read,
        frame_tx,
        audio_tx,
        status_tx,
        input_barrier_tx,
        input_ready_tx,
        release_ack_tx,
        audio_ack_tx,
        fatal_tx,
    ));
    let mut heartbeat = tokio::time::interval(Duration::from_secs(10));
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut session_check = tokio::time::interval(Duration::from_secs(1));
    session_check.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut last_browser_message = tokio::time::Instant::now();
    let mut control_rate = ControlRateLimiter::new(Instant::now());
    let mut rate_limited_drops = 0u32;
    let mut input_gate = RelayInputGate::default();
    let mut audio_open = true;
    let mut audio_relay = AudioRelayGate::new();
    let mut secure_attention = SecureAttentionGate::new(authorization);

    let result: Result<String> = async {
        let reason = loop {
            tokio::select! {
            biased;
            fatal = fatal_rx.recv() => {
                let Some(fatal) = fatal else { break "helper_disconnected".to_string() };
                return Err(anyhow!("desktop helper fatal: {fatal}"));
            }
            reason = &mut close => {
                break reason.unwrap_or_else(|_| "agent_disconnected".to_string());
            }
            barrier = input_barrier_rx.recv() => {
                let Some(barrier) = barrier else { break "helper_disconnected".to_string() };
                input_gate.begin_generation(barrier.generation);
            }
            ready = input_ready_rx.recv() => {
                let Some(ready) = ready else { break "helper_disconnected".to_string() };
                if ready.generation == input_gate.generation {
                    input_gate.arm_after_first_frame(ready.sequence);
                }
            }
            incoming = ws_read.next() => {
                let Some(incoming) = incoming else { break "browser_disconnected".to_string() };
                last_browser_message = tokio::time::Instant::now();
                match incoming? {
                    Message::Text(text) => {
                        if text.len() > MAX_CONTROL_BYTES { bail!("desktop control message too large") }
                        let control = serde_json::from_str::<DesktopControl>(&text)
                            .context("invalid desktop control message")?;
                        if !control_rate.allow(Instant::now()) {
                            // Mirror the backend policy: drop unreliable messages over
                            // the limit and only end the session on a sustained flood
                            // or on reliable input that a real user cannot produce.
                            if !matches!(
                                &control,
                                DesktopControl::PointerMove { .. } | DesktopControl::Feedback { .. }
                            ) {
                                bail!("control_rate_limited: desktop control rate exceeded")
                            }
                            rate_limited_drops += 1;
                            if rate_limited_drops >= CONTROL_RATE_DROP_LIMIT {
                                bail!("control_rate_limited: desktop control rate exceeded")
                            }
                            continue;
                        }
                        rate_limited_drops = 0;
                        if let DesktopControl::Feedback { sequence, .. } = &control {
                            input_gate.observe_feedback(*sequence);
                        }
                        if !input_gate.allows(&control) {
                            continue;
                        }
                        if matches!(&control, DesktopControl::SecureAttention) {
                            if !secure_attention.allowed() {
                                let notice = secure_attention_notice("secure_attention_unavailable");
                                tokio::time::timeout(
                                    SOCKET_SEND_TIMEOUT,
                                    ws_write.send(Message::Text(notice.into())),
                                )
                                .await
                                .context("desktop secure attention notice timed out")??;
                                let result = secure_attention_result(
                                    false,
                                    Some("secure_attention_unavailable"),
                                );
                                tokio::time::timeout(
                                    SOCKET_SEND_TIMEOUT,
                                    ws_write.send(Message::Text(result.into())),
                                )
                                .await
                                .context("desktop secure attention result timed out")??;
                                continue;
                            }
                            let (succeeded, code) =
                                match send_service_secure_attention(authorization.session_id) {
                                    Ok(()) => (true, None),
                                    Err(error) => {
                                        crate::logging::error(format_args!(
                                            "Windows secure attention request was rejected: {error:#}"
                                        ));
                                        let notice = secure_attention_notice(
                                            "secure_attention_policy_denied",
                                        );
                                        tokio::time::timeout(
                                            SOCKET_SEND_TIMEOUT,
                                            ws_write.send(Message::Text(notice.into())),
                                        )
                                        .await
                                        .context(
                                            "desktop secure attention failure notice timed out",
                                        )??;
                                        (false, Some("secure_attention_policy_denied"))
                                    }
                                };
                            let result = secure_attention_result(succeeded, code);
                            tokio::time::timeout(
                                SOCKET_SEND_TIMEOUT,
                                ws_write.send(Message::Text(result.into())),
                            )
                            .await
                            .context("desktop secure attention result timed out")??;
                            continue;
                        }
                        if let DesktopControl::AudioControl {
                            enabled,
                            generation,
                        } = control
                        {
                            audio_relay.register_control(generation, enabled)?;
                        }
                        let command = serde_json::to_vec(&HelperControlEnvelope {
                            input_generation: input_gate.generation,
                            control,
                        })?;
                        tokio::time::timeout(
                            PIPE_WRITE_TIMEOUT,
                            write_packet(&mut pipe_write, PIPE_CONTROL, &command),
                        )
                        .await
                        .context("desktop helper pipe control write timed out")??;
                    }
                    Message::Ping(value) => {
                        tokio::time::timeout(
                            SOCKET_SEND_TIMEOUT,
                            ws_write.send(Message::Pong(value)),
                        )
                        .await
                        .context("desktop data websocket pong send timed out")??;
                    }
                    Message::Pong(_) => {}
                    Message::Close(_) => break "browser_closed".to_string(),
                    Message::Binary(_) => bail!("unexpected browser desktop binary message"),
                    _ => {}
                }
            }
            ack = audio_ack_rx.recv() => {
                let Some(ack) = ack else { break "helper_disconnected".to_string() };
                if let Some(generation) = audio_relay.acknowledge(ack)? {
                    let status = audio_control_ack_status(generation);
                    tokio::time::timeout(
                        SOCKET_SEND_TIMEOUT,
                        ws_write.send(Message::Text(status.into())),
                    )
                    .await
                    .context("desktop data websocket audio acknowledgement send timed out")??;
                }
            }
            status = status_rx.recv() => {
                let Some(status) = status else { break "helper_disconnected".to_string() };
                secure_attention.observe_status(&status)?;
                audio_relay.observe_status(&status);
                tokio::time::timeout(
                    SOCKET_SEND_TIMEOUT,
                    ws_write.send(Message::Text(status.into())),
                )
                .await
                .context("desktop data websocket status send timed out")??;
            }
            audio = audio_rx.recv(), if audio_open => {
                let Some(audio) = audio else {
                    audio_open = false;
                    continue;
                };
                validate_audio_frame(&audio)?;
                if !audio_relay.accepts_audio(&audio) {
                    continue;
                }
                tokio::time::timeout(
                    SOCKET_SEND_TIMEOUT,
                    ws_write.send(Message::Binary(audio.into())),
                )
                .await
                .context("desktop data websocket audio send timed out")??;
            }
            changed = frame_rx.changed() => {
                if changed.is_err() { break "helper_disconnected".to_string() }
                let Some(frame) = frame_rx.borrow_and_update().clone() else { continue };
                validate_frame(&frame)?;
                tokio::time::timeout(
                    SOCKET_SEND_TIMEOUT,
                    ws_write.send(Message::Binary(frame.into())),
                )
                .await
                .context("desktop data websocket frame send timed out")??;
            }
            _ = heartbeat.tick() => {
                if last_browser_message.elapsed() >= Duration::from_secs(30) {
                    break "browser_heartbeat_timeout".to_string();
                }
                tokio::time::timeout(
                    SOCKET_SEND_TIMEOUT,
                    ws_write.send(Message::Ping(Vec::new().into())),
                )
                .await
                .context("desktop data websocket ping send timed out")??;
            }
            _ = session_check.tick(), if authorization.managed_service => {
                if select_active_session(authorization.unattended)
                    .is_ok_and(|session_id| session_id != authorization.session_id)
                {
                    break "session_changed".to_string();
                }
            }
            }
        };
        Ok(reason)
    }
    .await;

    let close_reason = match &result {
        Ok(reason) => reason.clone(),
        Err(error) => error_reason(error),
    };

    // Keep the reader alive while the helper drains input state. This prevents a queued JPEG
    // packet from filling the pipe and blocking the helper before it can receive the stop packet.
    let stop_sent = match tokio::time::timeout(
        PIPE_WRITE_TIMEOUT,
        write_packet(&mut pipe_write, PIPE_INTERNAL, INTERNAL_STOP),
    )
    .await
    {
        Ok(Ok(())) => true,
        Ok(Err(error)) => {
            crate::logging::error(format_args!(
                "failed to send the desktop helper stop packet: {error:#}"
            ));
            false
        }
        Err(_) => {
            crate::logging::error(format_args!("desktop helper stop packet write timed out"));
            false
        }
    };

    let closed = serde_json::json!({"type":"closed", "reason":close_reason}).to_string();
    let _ = tokio::time::timeout(
        SOCKET_SEND_TIMEOUT,
        ws_write.send(Message::Text(closed.into())),
    )
    .await;

    if stop_sent
        && !wait_for_input_release_ack(&mut release_ack_rx, INPUT_RELEASE_ACK_TIMEOUT).await
    {
        crate::logging::error(format_args!(
            "desktop helper did not acknowledge input release before the cleanup deadline"
        ));
    }
    reader.abort();
    result
}

async fn pipe_reader<R: AsyncRead + Unpin>(
    mut reader: R,
    frame_tx: watch::Sender<Option<Vec<u8>>>,
    audio_tx: super::DropOldestSender<Vec<u8>>,
    status_tx: mpsc::Sender<String>,
    input_barrier_tx: mpsc::Sender<InputBarrier>,
    input_ready_tx: mpsc::Sender<InputReady>,
    release_ack_tx: mpsc::Sender<()>,
    audio_ack_tx: mpsc::Sender<AudioControlAck>,
    fatal_tx: mpsc::Sender<String>,
) -> Result<()> {
    let result: Result<()> = async {
        loop {
            let (kind, value) = read_packet(&mut reader)
                .await
                .context("failed to read desktop helper pipe")?;
            match kind {
                PIPE_FRAME => {
                    frame_tx.send_replace(Some(value));
                }
                PIPE_AUDIO => {
                    if let Err(error) = validate_audio_frame(&value) {
                        crate::logging::error(format_args!(
                            "discarded invalid desktop helper audio frame: {error:#}"
                        ));
                    } else {
                        let _ = audio_tx.send(value);
                    }
                }
                PIPE_CONTROL => {
                    let text = String::from_utf8(value).context("helper sent non-UTF8 control")?;
                    if status_is_display_preparing(&text) {
                        frame_tx.send_replace(None);
                    }
                    if audio_state_clears_queue(&text) {
                        audio_tx.clear();
                    }
                    status_tx.send(text).await?;
                }
                PIPE_INTERNAL if value == INTERNAL_STOPPED => {
                    let _ = release_ack_tx.try_send(());
                }
                PIPE_INTERNAL if value.starts_with(INTERNAL_INPUT_BARRIER_PREFIX) => {
                    let barrier =
                        serde_json::from_slice(&value[INTERNAL_INPUT_BARRIER_PREFIX.len()..])
                            .context("invalid desktop input generation barrier")?;
                    input_barrier_tx.send(barrier).await?;
                }
                PIPE_INTERNAL if value.starts_with(INTERNAL_INPUT_READY_PREFIX) => {
                    let ready = serde_json::from_slice(&value[INTERNAL_INPUT_READY_PREFIX.len()..])
                        .context("invalid desktop input generation readiness")?;
                    input_ready_tx.send(ready).await?;
                }
                PIPE_INTERNAL if value.starts_with(INTERNAL_FATAL_PREFIX) => {
                    let reason = String::from_utf8(value[INTERNAL_FATAL_PREFIX.len()..].to_vec())
                        .context("helper sent non-UTF8 fatal reason")?;
                    let _ = fatal_tx.try_send(reason);
                }
                PIPE_INTERNAL => {
                    let Some(ack) = parse_audio_control_ack(&value)? else {
                        bail!("unknown desktop helper internal packet")
                    };
                    if ack.changed {
                        audio_tx.clear();
                    }
                    audio_ack_tx.send(ack).await?;
                }
                _ => bail!("unknown desktop helper packet type"),
            }
        }
    }
    .await;
    if let Err(error) = &result {
        let _ = fatal_tx.send(format!("{error:#}")).await;
    }
    result
}

fn audio_state_clears_queue(status: &str) -> bool {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(status) else {
        return false;
    };
    value.get("type").and_then(serde_json::Value::as_str) == Some("audio_state")
        && value.get("state").and_then(serde_json::Value::as_str) != Some("playing")
}

fn status_is_display_preparing(status: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(status).is_ok_and(|value| {
        value.get("type").and_then(serde_json::Value::as_str) == Some("display_state")
            && value.get("state").and_then(serde_json::Value::as_str) == Some("preparing")
    })
}

fn current_display_source(fallback: &str) -> String {
    crate::remote_access::detected_display_source()
        .unwrap_or(fallback)
        .to_string()
}

fn encode_audio_control_ack(ack: AudioControlAck) -> Result<Vec<u8>> {
    let encoded = serde_json::to_vec(&ack)?;
    let mut packet = Vec::with_capacity(INTERNAL_AUDIO_CONTROL_ACK_PREFIX.len() + encoded.len());
    packet.extend_from_slice(INTERNAL_AUDIO_CONTROL_ACK_PREFIX);
    packet.extend_from_slice(&encoded);
    Ok(packet)
}

fn parse_audio_control_ack(value: &[u8]) -> Result<Option<AudioControlAck>> {
    let Some(encoded) = value.strip_prefix(INTERNAL_AUDIO_CONTROL_ACK_PREFIX) else {
        return Ok(None);
    };
    Ok(Some(serde_json::from_slice(encoded).context(
        "invalid desktop audio control acknowledgement",
    )?))
}

fn encode_internal_message<T: serde::Serialize>(prefix: &[u8], value: &T) -> Result<Vec<u8>> {
    let encoded = serde_json::to_vec(value)?;
    let mut packet = Vec::with_capacity(prefix.len() + encoded.len());
    packet.extend_from_slice(prefix);
    packet.extend_from_slice(&encoded);
    Ok(packet)
}

fn audio_control_ack_status(generation: u64) -> String {
    serde_json::json!({
        "type":"audio_state",
        "state":"starting",
        "reason":"control_ack",
        "generation":generation,
    })
    .to_string()
}

pub async fn run_helper(options: DesktopOptions) -> Result<()> {
    bind_interactive_window_station()?;
    log_helper_security_context(&options)?;
    let unattended = options.unattended
        && options.system_helper
        && current_process_is_local_system().unwrap_or(false);
    if options.unattended && !unattended {
        bail!("unattended_policy_rejected")
    }
    let audio_negotiated = windows_audio::negotiated(options.audio_codec.as_deref());
    let pipe = tokio::time::timeout(DATA_CHANNEL_JOIN_TIMEOUT, connect_pipe(&options.pipe))
        .await
        .map_err(|_| anyhow!("desktop helper pipe connection timeout"))??;
    let (read, mut write) = tokio::io::split(pipe);
    let session_policy = serde_json::json!({
        "type":"session_policy",
        "access_mode": if unattended { "unattended" } else { "local_consent" },
        "local_consent_required": !unattended,
        "secure_desktop_control": unattended,
        "secure_attention_allowed": unattended
    })
    .to_string();
    write_packet(&mut write, PIPE_CONTROL, session_policy.as_bytes()).await?;
    let preparing = serde_json::json!({
        "type":"display_state",
        "state":"preparing",
        "source":&options.display_source
    })
    .to_string();
    write_packet(&mut write, PIPE_CONTROL, preparing.as_bytes()).await?;
    if !unattended {
        let consent_required = serde_json::json!({"type":"consent_required"}).to_string();
        write_packet(&mut write, PIPE_CONTROL, consent_required.as_bytes()).await?;
        let consent_granted =
            tokio::task::spawn_blocking(move || request_local_control_consent(audio_negotiated))
                .await
                .context("local consent prompt task failed")?;
        if !consent_granted {
            write_fatal_packet(&mut write, "local_consent_denied").await?;
            bail!("local_consent_denied")
        }
    }
    let mut local_indicator = spawn_local_session_indicator(audio_negotiated)?;
    let mut local_indicator_open = true;

    // `read_packet` is not cancellation-safe: if another `select!` branch wins after the length
    // or kind byte has been consumed, dropping the future leaves the next read in the middle of a
    // packet. Browser feedback arrives while the timer and capture branches are also active, so
    // this used to desynchronize the pipe and terminate the helper a few seconds after joining.
    // Keep the packet read in its own task and only select on complete packets instead.
    let (packet_tx, mut packet_rx) = mpsc::channel::<Result<(u8, Vec<u8>)>>(1);
    tokio::spawn(helper_pipe_reader(read, packet_tx));
    let (capture_tx, mut capture_rx) = mpsc::channel::<HelperEvent>(1);
    let settings = Arc::new(Mutex::new(AdaptiveSettings::initial(
        options.min_fps,
        options.max_fps,
        options.jpeg_quality,
    )));
    let capture_settings = settings.clone();
    let capture_options = options.clone();
    let input_fence = Arc::new(InputGenerationFence::default());
    let capture_input_fence = input_fence.clone();
    std::thread::Builder::new()
        .name("om-desktop-capture".to_string())
        .spawn(move || {
            capture_loop(
                capture_options,
                capture_settings,
                capture_input_fence,
                capture_tx,
            )
        })?;

    let mut input = InputState::new(unattended);
    let mut input_desktop_available = controllable_input_desktop(unattended);
    let (audio_frame_tx, audio_frame_rx) = drop_oldest_channel(AUDIO_CHANNEL_CAPACITY);
    let (audio_status_tx, mut audio_status_rx) = mpsc::channel::<String>(8);
    let mut audio_frames_open = audio_negotiated;
    let mut audio_status_open = audio_negotiated;
    let audio_runtime = if audio_negotiated {
        let failure_tx = audio_status_tx.clone();
        let started = current_session_id().and_then(|session_id| {
            windows_audio::spawn(
                options.system_helper,
                session_id,
                default_input_desktop(),
                audio_frame_tx,
                audio_status_tx,
            )
        });
        match started {
            Ok(runtime) => Some(runtime),
            Err(error) => {
                crate::logging::error(format_args!(
                    "remote desktop audio thread failed to start: {error:#}"
                ));
                let unavailable = serde_json::json!({
                    "type":"audio_state",
                    "state":"unavailable",
                    "reason":"capture_failed"
                })
                .to_string();
                let _ = failure_tx.try_send(unavailable);
                None
            }
        }
    } else {
        drop(audio_frame_tx);
        drop(audio_status_tx);
        audio_frames_open = false;
        audio_status_open = false;
        None
    };
    let mut pending_release = false;
    let mut stopping = false;
    let mut last_input_error_log = None;
    let mut suppressed_input_errors = 0_u64;
    let mut desktop_check = tokio::time::interval(Duration::from_millis(100));
    desktop_check.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut frame_ready = false;
    loop {
        tokio::select! {
            biased;
            indicator_action = local_indicator.recv(), if local_indicator_open => {
                local_indicator_open = false;
                if !matches!(indicator_action, Some(LocalSessionIndicatorAction::Dismiss)) {
                    if let Some(audio) = &audio_runtime {
                        audio.stop();
                    }
                    let _ = release_current_input_generation(&input_fence, &mut input);
                    write_fatal_packet(&mut write, "local_consent_revoked").await?;
                    bail!("local_consent_revoked")
                }
            }
            packet = packet_rx.recv() => {
                let (kind, value) = packet
                    .context("desktop service pipe reader stopped unexpectedly")??;
                if kind == PIPE_INTERNAL && value == INTERNAL_STOP {
                    stopping = true;
                    if let Some(audio) = &audio_runtime {
                        audio.stop();
                    }
                    pending_release = !release_current_input_generation(&input_fence, &mut input)?;
                    if !pending_release {
                        write_packet(&mut write, PIPE_INTERNAL, INTERNAL_STOPPED).await?;
                        break;
                    }
                    continue;
                }
                if kind != PIPE_CONTROL { bail!("unexpected service packet type") }
                let command: HelperControlEnvelope = serde_json::from_slice(&value)?;
                let generation = command.input_generation;
                let control = command.control;
                match control {
                    DesktopControl::AudioControl {
                        enabled,
                        generation,
                    } if !stopping => {
                        let changed = audio_runtime
                            .as_ref()
                            .is_some_and(|audio| audio.set_enabled(enabled));
                        let ack = encode_audio_control_ack(AudioControlAck {
                            enabled,
                            changed,
                            generation,
                        })?;
                        write_packet(&mut write, PIPE_INTERNAL, &ack).await?;
                    }
                    DesktopControl::Feedback { sequence, fps, decode_ms } if !stopping => {
                        let mut settings = settings
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner());
                        settings.update(
                            options.min_fps,
                            options.max_fps,
                            sequence,
                            fps,
                            decode_ms,
                        );
                    }
                    DesktopControl::ReleaseAll => {
                        if input_fence.generation() == generation {
                            pending_release = !release_on_input_generation(
                                &input_fence,
                                &mut input,
                                generation,
                            )?;
                        }
                    }
                    control if !stopping => {
                        let available = controllable_input_desktop(unattended);
                        if input_desktop_available && !available {
                            input.discard_for_generation(input_fence.generation());
                            pending_release = false;
                        }
                        input_desktop_available = available;
                        if let Some(audio) = &audio_runtime {
                            audio.set_default_desktop(default_input_desktop());
                        }
                        if available && pending_release {
                            pending_release = !release_current_input_generation(
                                &input_fence,
                                &mut input,
                            )?;
                        }
                        if available && !pending_release {
                            let secure_attention = matches!(&control, DesktopControl::SecureAttention);
                            let releasing = matches!(
                                &control,
                                DesktopControl::Key { down: false, .. }
                                    | DesktopControl::PointerButton { down: false, .. }
                            );
                            if let Err(error) = apply_on_input_generation(
                                &input_fence,
                                &mut input,
                                generation,
                                control,
                            ) {
                                if error.to_string().contains(DESKTOP_BINDING_LOST)
                                    || error.to_string().contains(DESKTOP_HANDLE_CLEANUP_FAILED)
                                {
                                    write_fatal_packet(&mut write, DESKTOP_BINDING_LOST).await?;
                                    return Err(error);
                                }
                                log_input_injection_error(
                                    &error,
                                    &mut last_input_error_log,
                                    &mut suppressed_input_errors,
                                );
                                if releasing {
                                    pending_release = true;
                                }
                                if secure_attention {
                                    let notice = serde_json::json!({
                                        "type":"notice",
                                        "code":"secure_attention_unavailable",
                                        "message":"Windows 未允许发送 Ctrl+Alt+Del"
                                    })
                                    .to_string();
                                    write_packet(&mut write, PIPE_CONTROL, notice.as_bytes()).await?;
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
            _ = desktop_check.tick() => {
                let available = controllable_input_desktop(unattended);
                if input_desktop_available && !available {
                    input.discard_for_generation(input_fence.generation());
                    pending_release = false;
                }
                input_desktop_available = available;
                if let Some(audio) = &audio_runtime {
                    audio.set_default_desktop(default_input_desktop());
                }
                if available && pending_release {
                    pending_release = !release_current_input_generation(&input_fence, &mut input)?;
                }
                if stopping && !pending_release {
                    write_packet(&mut write, PIPE_INTERNAL, INTERNAL_STOPPED).await?;
                    break;
                }
            }
            audio = audio_frame_rx.recv(), if audio_frames_open && !stopping => {
                match audio {
                    Some(audio) if audio_runtime.as_ref().is_some_and(|runtime| runtime.accepts(&audio)) => {
                        write_packet(&mut write, PIPE_AUDIO, &audio.bytes).await?
                    }
                    Some(_) => {}
                    None => audio_frames_open = false,
                }
            }
            status = audio_status_rx.recv(), if audio_status_open && !stopping => {
                match status {
                    Some(status) => write_packet(&mut write, PIPE_CONTROL, status.as_bytes()).await?,
                    None => audio_status_open = false,
                }
            }
            event = capture_rx.recv() => {
                let Some(event) = event else {
                    let reason = "desktop capture thread stopped unexpectedly";
                    let mut fatal = INTERNAL_FATAL_PREFIX.to_vec();
                    fatal.extend_from_slice(reason.as_bytes());
                    write_packet(&mut write, PIPE_INTERNAL, &fatal).await?;
                    bail!(reason)
                };
                match event {
                    HelperEvent::Frame { generation, sequence, bytes } if !stopping => {
                        if generation != input_fence.generation() {
                            continue;
                        }
                        write_packet(&mut write, PIPE_FRAME, &bytes).await?;
                        if !frame_ready {
                            if !input_fence.mark_ready(generation) {
                                continue;
                            }
                            let ready = encode_internal_message(
                                INTERNAL_INPUT_READY_PREFIX,
                                &InputReady { generation, sequence },
                            )?;
                            write_packet(&mut write, PIPE_INTERNAL, &ready).await?;
                            let display_ready = serde_json::json!({
                                "type":"display_state",
                                "state":"ready",
                                "source":current_display_source(&options.display_source)
                            })
                            .to_string();
                            write_packet(&mut write, PIPE_CONTROL, display_ready.as_bytes()).await?;
                            let ready = serde_json::json!({"type":"ready"}).to_string();
                            write_packet(&mut write, PIPE_CONTROL, ready.as_bytes()).await?;
                            frame_ready = true;
                        }
                    }
                    HelperEvent::InputBarrier(barrier) if !stopping => {
                        input.discard_for_generation(barrier.generation);
                        pending_release = false;
                        frame_ready = false;
                        let barrier = encode_internal_message(
                            INTERNAL_INPUT_BARRIER_PREFIX,
                            &barrier,
                        )?;
                        write_packet(&mut write, PIPE_INTERNAL, &barrier).await?;
                    }
                    HelperEvent::Status(status) if !stopping => {
                        if status_is_display_preparing(&status) {
                            frame_ready = false;
                        }
                        write_packet(&mut write, PIPE_CONTROL, status.as_bytes()).await?
                    },
                    HelperEvent::Fatal(reason) => {
                        if let Some(audio) = &audio_runtime {
                            audio.stop();
                        }
                        let mut fatal = INTERNAL_FATAL_PREFIX.to_vec();
                        fatal.extend_from_slice(reason.as_bytes());
                        write_packet(&mut write, PIPE_INTERNAL, &fatal).await?;
                        stopping = true;
                        pending_release = !release_current_input_generation(&input_fence, &mut input)?;
                        if !pending_release {
                            write_packet(&mut write, PIPE_INTERNAL, INTERNAL_STOPPED).await?;
                            break;
                        }
                    }
                    _ => {}
                }
            }
        }
    }
    if let Some(audio) = &audio_runtime {
        audio.stop();
    }
    let _ = release_current_input_generation(&input_fence, &mut input)?;
    Ok(())
}

async fn helper_pipe_reader<R: AsyncRead + Unpin>(
    mut reader: R,
    tx: mpsc::Sender<Result<(u8, Vec<u8>)>>,
) {
    loop {
        let packet = read_packet(&mut reader)
            .await
            .context("failed to read desktop service pipe");
        let failed = packet.is_err();
        if tx.send(packet).await.is_err() || failed {
            break;
        }
    }
}

async fn connect_pipe(name: &str) -> Result<NamedPipeClient> {
    loop {
        match ClientOptions::new().open(name) {
            Ok(pipe) => return Ok(pipe),
            Err(error) if error.raw_os_error() == Some(2) => {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            Err(error) => return Err(error).context("failed to open desktop helper pipe"),
        }
    }
}

enum HelperEvent {
    Frame {
        generation: u64,
        sequence: u64,
        bytes: Vec<u8>,
    },
    InputBarrier(InputBarrier),
    Status(String),
    Fatal(String),
}

fn capture_loop(
    options: DesktopOptions,
    settings: Arc<Mutex<AdaptiveSettings>>,
    input_fence: Arc<InputGenerationFence>,
    tx: mpsc::Sender<HelperEvent>,
) {
    let mut capture: Option<DxgiCapture> = None;
    // Set when DXGI duplication delivers a pixel format we cannot decode
    // (e.g. HDR R10G10B10A2); the loop then stays on GDI capture instead of
    // re-initializing DXGI and failing again every frame.
    let mut dxgi_format_unsupported = false;
    let mut attached_desktop = match ThreadDesktopBinding::new() {
        Ok(binding) => binding,
        Err(error) => {
            let _ = tx.blocking_send(HelperEvent::Fatal(format!(
                "failed to inspect capture thread desktop: {error:#}"
            )));
            return;
        }
    };
    let mut sequence = 0_u64;
    let mut foreground_secure_paused = false;
    let mut awaiting_first_frame = true;
    let mut input_generation = 0_u64;
    let mut output_wait_started = Some(Instant::now());
    let mut next_capture = Instant::now();
    loop {
        let now = Instant::now();
        if now < next_capture {
            std::thread::sleep(next_capture - now);
        }
        let adaptive = *settings
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        next_capture =
            Instant::now() + Duration::from_millis(1000 / u64::from(adaptive.fps.max(1)));
        let desktop_name = match attach_input_desktop(&mut attached_desktop) {
            Ok((name, changed)) => {
                if changed {
                    input_generation = input_fence.begin_transition(&name);
                    if tx
                        .blocking_send(HelperEvent::InputBarrier(InputBarrier {
                            generation: input_generation,
                        }))
                        .is_err()
                    {
                        return;
                    }
                    capture = None;
                    dxgi_format_unsupported = false;
                    let kind = desktop_kind(&name);
                    let context = desktop_context(&name);
                    let controllable = desktop_controllable(&name, options.unattended);
                    if controllable {
                        let _ = tx.blocking_send(HelperEvent::Status(
                            serde_json::json!({
                                "type":"display_state",
                                "state":"preparing",
                                "source":current_display_source(&options.display_source)
                            })
                            .to_string(),
                        ));
                        awaiting_first_frame = true;
                        output_wait_started = Some(Instant::now());
                    }
                    let _ = tx.blocking_send(HelperEvent::Status(
                        serde_json::json!({
                            "type":"desktop_state",
                            "desktop":kind,
                            "context":context,
                            "controllable":controllable
                        })
                        .to_string(),
                    ));
                }
                name
            }
            Err(error) => {
                if error.to_string().contains(DESKTOP_HANDLE_CLEANUP_FAILED) {
                    let _ = tx.blocking_send(HelperEvent::Fatal(format!("{error:#}")));
                    return;
                }
                crate::logging::error(format_args!("failed to attach input desktop: {error:#}"));
                capture = None;
                std::thread::sleep(Duration::from_millis(100));
                continue;
            }
        };
        if !desktop_controllable(&desktop_name, options.unattended) {
            if !foreground_secure_paused {
                let _ = tx.blocking_send(HelperEvent::Status(
                    serde_json::json!({
                        "type":"paused",
                        "reason":"secure_desktop"
                    })
                    .to_string(),
                ));
                foreground_secure_paused = true;
            }
            capture = None;
            output_wait_started = None;
            continue;
        }
        if foreground_secure_paused {
            foreground_secure_paused = false;
        }
        let result = if desktop_name.eq_ignore_ascii_case("Default") {
            if capture.is_none() && !dxgi_format_unsupported {
                match DxgiCapture::new() {
                    Ok(value) => capture = Some(value),
                    Err(error) => {
                        crate::logging::error(format_args!(
                            "failed to initialize DXGI desktop duplication, using GDI: {error:#}"
                        ));
                    }
                }
            }
            if let Some(capture) = capture.as_mut() {
                capture.capture_jpeg(options.max_width, options.max_height, adaptive.jpeg_quality)
            } else {
                capture_gdi_jpeg(options.max_width, options.max_height, adaptive.jpeg_quality)
            }
        } else {
            capture = None;
            capture_gdi_jpeg(options.max_width, options.max_height, adaptive.jpeg_quality)
        };
        match result {
            Ok(Some((jpeg, width, height))) => {
                sequence += 1;
                let mut frame = Vec::with_capacity(32 + jpeg.len());
                frame.extend_from_slice(
                    &FrameHeader {
                        sequence,
                        captured_at_ms: now_ms(),
                        width,
                        height,
                    }
                    .encode(),
                );
                frame.extend_from_slice(&jpeg);
                if frame.len() <= MAX_FRAME_BYTES {
                    if awaiting_first_frame {
                        if tx
                            .blocking_send(HelperEvent::Frame {
                                generation: input_generation,
                                sequence,
                                bytes: frame,
                            })
                            .is_err()
                        {
                            return;
                        }
                        awaiting_first_frame = false;
                    } else {
                        let _ = tx.try_send(HelperEvent::Frame {
                            generation: input_generation,
                            sequence,
                            bytes: frame,
                        });
                    }
                    output_wait_started = None;
                }
            }
            Ok(None) => {}
            Err(error) if error.to_string().contains("DXGI_ERROR_ACCESS_LOST") => {
                // A display mode change (including toggling HDR) invalidates the
                // duplication; allow DXGI to be retried with the new mode.
                capture = None;
                dxgi_format_unsupported = false;
            }
            Err(error) if error.to_string().contains("unsupported_capture_format") => {
                crate::logging::error(format_args!(
                    "DXGI desktop duplication pixel format is unsupported, using GDI: {error:#}"
                ));
                capture = None;
                dxgi_format_unsupported = true;
            }
            Err(error) if error.to_string().contains("frame_too_large") => {
                let _ = tx.blocking_send(HelperEvent::Fatal("frame_too_large".to_string()));
                return;
            }
            Err(error) => {
                crate::logging::error(format_args!("desktop capture failed: {error:#}"));
                capture = None;
            }
        }
        if output_wait_started.is_some_and(|started| started.elapsed() >= DISPLAY_PREPARE_TIMEOUT) {
            let unavailable = serde_json::json!({
                "type":"display_state",
                "state":"unavailable",
                "source":"none",
                "code":"no_display_output"
            })
            .to_string();
            let _ = tx.blocking_send(HelperEvent::Status(unavailable));
            let _ = tx.blocking_send(HelperEvent::Fatal("no_display_output".to_string()));
            return;
        }
    }
}

fn desktop_kind(name: &str) -> &'static str {
    if name.eq_ignore_ascii_case("Default") {
        "default"
    } else if name.eq_ignore_ascii_case("Winlogon") {
        "secure"
    } else {
        "other"
    }
}

fn desktop_context(name: &str) -> &'static str {
    if name.eq_ignore_ascii_case("Default") {
        "default"
    } else if name.eq_ignore_ascii_case("Winlogon") {
        "winlogon"
    } else {
        "other"
    }
}

fn desktop_controllable(name: &str, unattended: bool) -> bool {
    name.eq_ignore_ascii_case("Default") || (unattended && name.eq_ignore_ascii_case("Winlogon"))
}

struct OwnedDesktop(Option<HDESK>);

impl OwnedDesktop {
    fn new(desktop: HDESK) -> Self {
        Self(Some(desktop))
    }

    fn handle(&self) -> HDESK {
        self.0.expect("owned desktop handle is present")
    }

    fn close(mut self) -> Result<()> {
        let desktop = self.0.take().expect("owned desktop handle is present");
        unsafe { CloseDesktop(desktop) }
            .with_context(|| format!("{DESKTOP_HANDLE_CLEANUP_FAILED}: failed to close desktop"))
    }
}

impl Drop for OwnedDesktop {
    fn drop(&mut self) {
        if let Some(desktop) = self.0.take() {
            unsafe {
                let _ = CloseDesktop(desktop);
            }
        }
    }
}

struct ThreadDesktopBinding {
    original: HDESK,
    current: Option<(OwnedDesktop, String)>,
}

impl ThreadDesktopBinding {
    fn new() -> Result<Self> {
        Ok(Self {
            original: unsafe { GetThreadDesktop(GetCurrentThreadId())? },
            current: None,
        })
    }
}

impl Drop for ThreadDesktopBinding {
    fn drop(&mut self) {
        if self.current.is_some() && unsafe { SetThreadDesktop(self.original) }.is_ok() {
            self.current.take();
        }
    }
}

fn attach_input_desktop(current: &mut ThreadDesktopBinding) -> Result<(String, bool)> {
    unsafe {
        let desktop = OwnedDesktop::new(
            OpenInputDesktop(Default::default(), false, DESKTOP_READOBJECTS)
                .context("input desktop is unavailable")?,
        );
        let name = desktop_name(desktop.handle())?;
        if current
            .current
            .as_ref()
            .is_some_and(|(_, value)| value == &name)
        {
            desktop.close()?;
            return Ok((name, false));
        }
        SetThreadDesktop(desktop.handle())
            .context("failed to bind capture thread to input desktop")?;
        if let Some((previous, _)) = current.current.replace((desktop, name.clone())) {
            previous.close()?;
        }
        Ok((name, true))
    }
}

fn desktop_name(desktop: HDESK) -> Result<String> {
    user_object_name(HANDLE(desktop.0))
}

fn user_object_name(handle: HANDLE) -> Result<String> {
    unsafe {
        let mut needed = 0_u32;
        let _ = GetUserObjectInformationW(handle, UOI_NAME, None, 0, Some(&mut needed));
        let mut value = vec![0_u16; (needed as usize / 2).max(1)];
        GetUserObjectInformationW(
            handle,
            UOI_NAME,
            Some(value.as_mut_ptr().cast()),
            needed,
            Some(&mut needed),
        )?;
        let len = value.iter().position(|v| *v == 0).unwrap_or(value.len());
        Ok(String::from_utf16_lossy(&value[..len]))
    }
}

fn bind_interactive_window_station() -> Result<()> {
    unsafe {
        let current = GetProcessWindowStation()
            .context("failed to inspect current process window station")?;
        if user_object_name(HANDLE(current.0))?.eq_ignore_ascii_case("WinSta0") {
            return Ok(());
        }

        let name = wide("WinSta0");
        let station = OpenWindowStationW(PCWSTR(name.as_ptr()), false, WINSTA_ALL_ACCESS_MASK)
            .context("failed to open interactive window station")?;
        if let Err(error) = SetProcessWindowStation(station) {
            let _ = windows::Win32::System::StationsAndDesktops::CloseWindowStation(station);
            return Err(error)
                .context("failed to bind desktop helper to interactive window station");
        }
        // The station must remain associated with the process until this short-lived helper exits.
        Ok(())
    }
}

fn request_local_control_consent(audio_negotiated: bool) -> bool {
    let title = wide("Operation Monitoring 远程桌面");
    let message = if audio_negotiated {
        wide(
            "管理员请求查看并控制此 Windows 桌面，并可能收听系统声音。\r\n\r\n是否允许本次远程桌面会话？",
        )
    } else {
        wide("管理员请求查看并控制此 Windows 桌面。\r\n\r\n是否允许本次远程桌面会话？")
    };
    unsafe {
        MessageBoxW(
            None,
            PCWSTR(message.as_ptr()),
            PCWSTR(title.as_ptr()),
            MB_YESNO
                | MB_ICONWARNING
                | MB_DEFBUTTON2
                | MB_SETFOREGROUND
                | MB_TOPMOST
                | MB_SYSTEMMODAL,
        ) == IDYES
    }
}

fn spawn_local_session_indicator(
    audio_negotiated: bool,
) -> Result<mpsc::Receiver<LocalSessionIndicatorAction>> {
    let (stop_tx, stop_rx) = mpsc::channel(1);
    std::thread::Builder::new()
        .name("om-desktop-indicator".to_string())
        .spawn(move || {
            let title = wide("Operation Monitoring 远程桌面正在进行");
            let message = if audio_negotiated {
                wide(
                    "此计算机正在被远程查看和控制，并可能被收听系统声音。\r\n\r\n点击“确定”结束远程桌面会话；点击“取消”关闭此提示并继续远程桌面。",
                )
            } else {
                wide("此计算机正在被远程查看和控制。\r\n\r\n点击“确定”结束远程桌面会话；点击“取消”关闭此提示并继续远程桌面。")
            };
            let action = unsafe {
                match MessageBoxW(
                    None,
                    PCWSTR(message.as_ptr()),
                    PCWSTR(title.as_ptr()),
                    MB_OKCANCEL
                        | MB_ICONWARNING
                        | MB_DEFBUTTON2
                        | MB_SETFOREGROUND
                        | MB_TOPMOST
                        | MB_SYSTEMMODAL,
                ) {
                    IDOK => LocalSessionIndicatorAction::EndSession,
                    IDCANCEL => LocalSessionIndicatorAction::Dismiss,
                    // A failed MessageBox call should fail closed and end the session.
                    _ => LocalSessionIndicatorAction::EndSession,
                }
            };
            let _ = stop_tx.blocking_send(action);
        })?;
    Ok(stop_rx)
}

fn log_helper_security_context(options: &DesktopOptions) -> Result<()> {
    unsafe {
        let mut token = HANDLE::default();
        OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token)
            .context("failed to inspect desktop helper token")?;
        let sid = token_user_sid(token);
        let _ = CloseHandle(token);
        let station =
            GetProcessWindowStation().context("failed to inspect desktop helper window station")?;
        let station_name = user_object_name(HANDLE(station.0))?;
        crate::logging::info(format_args!(
            "remote desktop helper security context: session_id={}, token_sid={}, window_station={}, system_helper={}",
            current_session_id()?,
            sid?,
            station_name,
            options.system_helper
        ));
        Ok(())
    }
}

fn apply_on_input_generation(
    fence: &InputGenerationFence,
    input: &mut InputState,
    generation: u64,
    control: DesktopControl,
) -> Result<()> {
    let fence_state = fence
        .state
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if !fence_state.ready || fence_state.generation != generation {
        return Ok(());
    }
    unsafe {
        let original = GetThreadDesktop(GetCurrentThreadId())?;
        let desktop = OwnedDesktop::new(
            OpenInputDesktop(Default::default(), false, INPUT_DESKTOP_ACCESS)
                .context("input desktop is unavailable for input injection")?,
        );
        let name = desktop_name(desktop.handle())?;
        if !name.eq_ignore_ascii_case("Default")
            && !(input.allow_winlogon && name.eq_ignore_ascii_case("Winlogon"))
        {
            bail!("secure_desktop: input injection is restricted to the default desktop")
        }
        if !fence_state
            .desktop
            .as_deref()
            .is_some_and(|desktop| desktop.eq_ignore_ascii_case(&name))
        {
            return Ok(());
        }
        input.bind_generation(generation, &name);
        SetThreadDesktop(desktop.handle())
            .context("failed to bind input thread to input desktop")?;
        let applied = input
            .apply(control)
            .with_context(|| format!("input desktop {name}"));
        if let Err(error) = SetThreadDesktop(original) {
            return Err(error).with_context(|| {
                format!("{DESKTOP_BINDING_LOST}: failed to restore input thread desktop")
            });
        }
        desktop.close()?;
        applied
    }
}

fn release_on_input_generation(
    fence: &InputGenerationFence,
    input: &mut InputState,
    generation: u64,
) -> Result<bool> {
    if !fence.ready_for_generation(generation) {
        input.discard_for_generation(fence.generation());
        return Ok(true);
    }
    match apply_on_input_generation(fence, input, generation, DesktopControl::ReleaseAll) {
        Ok(()) => Ok(true),
        Err(error)
            if error.to_string().contains(DESKTOP_BINDING_LOST)
                || error.to_string().contains(DESKTOP_HANDLE_CLEANUP_FAILED) =>
        {
            Err(error)
        }
        Err(_) => Ok(false),
    }
}

fn release_current_input_generation(
    fence: &InputGenerationFence,
    input: &mut InputState,
) -> Result<bool> {
    let generation = input.generation;
    if generation == 0 {
        return Ok(true);
    }
    if fence.generation() != generation {
        input.discard_for_generation(fence.generation());
        return Ok(true);
    }
    release_on_input_generation(fence, input, generation)
}

struct DxgiCapture {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    duplication: IDXGIOutputDuplication,
    rotation: DXGI_MODE_ROTATION,
}

unsafe fn primary_output() -> Result<(
    IDXGIAdapter1,
    windows::Win32::Graphics::Dxgi::IDXGIOutput,
    DXGI_MODE_ROTATION,
)> {
    let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1()? };
    let mut adapter_index = 0_u32;
    loop {
        let Ok(adapter) = (unsafe { factory.EnumAdapters1(adapter_index) }) else {
            break;
        };
        let mut output_index = 0_u32;
        loop {
            let Ok(output) = (unsafe { adapter.EnumOutputs(output_index) }) else {
                break;
            };
            let description = unsafe { output.GetDesc()? };
            let bounds = description.DesktopCoordinates;
            if description.AttachedToDesktop.as_bool()
                && bounds.left <= 0
                && bounds.top <= 0
                && bounds.right > 0
                && bounds.bottom > 0
            {
                return Ok((adapter, output, description.Rotation));
            }
            output_index += 1;
        }
        adapter_index += 1;
    }
    bail!("no_active_session: no primary desktop output is attached")
}

impl DxgiCapture {
    fn new() -> Result<Self> {
        unsafe {
            let (adapter, output, rotation) = primary_output()?;
            let output1: IDXGIOutput1 = output.cast()?;
            let mut device = None;
            let mut context = None;
            D3D11CreateDevice(
                &adapter,
                D3D_DRIVER_TYPE_UNKNOWN,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                Some(&[D3D_FEATURE_LEVEL_11_0]),
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )?;
            let device = device.context("D3D11 did not return a device")?;
            let context = context.context("D3D11 did not return an immediate context")?;
            let duplication = output1.DuplicateOutput(&device)?;
            Ok(Self {
                device,
                context,
                duplication,
                rotation,
            })
        }
    }

    fn capture_jpeg(
        &mut self,
        max_width: u32,
        max_height: u32,
        mut quality: u8,
    ) -> Result<Option<(Vec<u8>, u32, u32)>> {
        unsafe {
            let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
            let mut resource: Option<IDXGIResource> = None;
            if let Err(error) = self
                .duplication
                .AcquireNextFrame(0, &mut info, &mut resource)
            {
                if error.code() == DXGI_ERROR_WAIT_TIMEOUT {
                    return Ok(None);
                }
                if error.code() == DXGI_ERROR_ACCESS_LOST {
                    bail!("DXGI_ERROR_ACCESS_LOST")
                }
                return Err(error.into());
            }
            let captured = (|| -> Result<_> {
                if info.AccumulatedFrames == 0 {
                    return Ok(None);
                }
                let texture: ID3D11Texture2D = resource.context("missing DXGI frame")?.cast()?;
                let mut desc = D3D11_TEXTURE2D_DESC::default();
                texture.GetDesc(&mut desc);
                // copy_bgra_to_rgb assumes 8-bit BGRA. HDR/10-bit outputs deliver
                // R10G10B10A2 (or FP16) textures; decoding those as BGRA corrupts
                // colors, so report a distinct error and let the caller fall back
                // to GDI capture instead.
                if !matches!(
                    desc.Format,
                    DXGI_FORMAT_B8G8R8A8_UNORM | DXGI_FORMAT_B8G8R8A8_UNORM_SRGB
                ) {
                    bail!(
                        "unsupported_capture_format: DXGI duplication produced format {:?}",
                        desc.Format
                    )
                }
                let staging_desc = D3D11_TEXTURE2D_DESC {
                    Usage: D3D11_USAGE_STAGING,
                    BindFlags: 0,
                    CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                    MiscFlags: 0,
                    ..desc
                };
                let mut staging = None;
                self.device
                    .CreateTexture2D(&staging_desc, None, Some(&mut staging))?;
                let staging = staging.context("D3D11 did not create staging texture")?;
                self.context.CopyResource(&staging, &texture);
                let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
                self.context
                    .Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))?;
                let rgb = copy_bgra_to_rgb(
                    mapped.pData.cast(),
                    mapped.RowPitch,
                    desc.Width,
                    desc.Height,
                );
                self.context.Unmap(&staging, 0);
                let source = ImageBuffer::<Rgb<u8>, _>::from_raw(desc.Width, desc.Height, rgb)
                    .context("invalid desktop image buffer")?;
                let source = DynamicImage::ImageRgb8(source);
                let source = if self.rotation == DXGI_MODE_ROTATION_ROTATE90 {
                    source.rotate90()
                } else if self.rotation == DXGI_MODE_ROTATION_ROTATE180 {
                    source.rotate180()
                } else if self.rotation == DXGI_MODE_ROTATION_ROTATE270 {
                    source.rotate270()
                } else {
                    source
                };
                let source_width = source.width();
                let source_height = source.height();
                let (width, height) =
                    scaled_dimensions(source_width, source_height, max_width, max_height);
                let image = if width != source_width || height != source_height {
                    source.resize_exact(width, height, FilterType::Triangle)
                } else {
                    source
                };
                loop {
                    let mut jpeg = Vec::new();
                    image.write_with_encoder(
                        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, quality),
                    )?;
                    if jpeg.len() + 32 <= MAX_FRAME_BYTES || quality <= MIN_JPEG_QUALITY {
                        if jpeg.len() + 32 > MAX_FRAME_BYTES {
                            bail!("frame_too_large")
                        }
                        return Ok(Some((jpeg, width, height)));
                    }
                    quality = quality.saturating_sub(5).max(MIN_JPEG_QUALITY);
                }
            })();
            let _ = self.duplication.ReleaseFrame();
            captured
        }
    }
}

unsafe fn copy_bgra_to_rgb(data: *const u8, pitch: u32, width: u32, height: u32) -> Vec<u8> {
    let mut rgb = vec![0_u8; (width * height * 3) as usize];
    for y in 0..height as usize {
        let row =
            unsafe { std::slice::from_raw_parts(data.add(y * pitch as usize), width as usize * 4) };
        for x in 0..width as usize {
            let source = x * 4;
            let target = (y * width as usize + x) * 3;
            rgb[target] = row[source + 2];
            rgb[target + 1] = row[source + 1];
            rgb[target + 2] = row[source];
        }
    }
    rgb
}

fn capture_gdi_jpeg(
    max_width: u32,
    max_height: u32,
    quality: u8,
) -> Result<Option<(Vec<u8>, u32, u32)>> {
    unsafe {
        let width = GetSystemMetrics(SM_CXSCREEN);
        let height = GetSystemMetrics(SM_CYSCREEN);
        if width <= 0 || height <= 0 {
            bail!("no_active_session: invalid desktop dimensions")
        }
        let screen = GetDC(None);
        if screen.is_invalid() {
            bail!("failed to acquire desktop DC")
        }
        let memory = CreateCompatibleDC(Some(screen));
        let bitmap = CreateCompatibleBitmap(screen, width, height);
        if memory.is_invalid() || bitmap.is_invalid() {
            if !bitmap.is_invalid() {
                let _ = DeleteObject(bitmap.into());
            }
            if !memory.is_invalid() {
                let _ = DeleteDC(memory);
            }
            let _ = ReleaseDC(None, screen);
            bail!("failed to create GDI desktop capture objects")
        }
        let previous = SelectObject(memory, bitmap.into());
        let copied = BitBlt(memory, 0, 0, width, height, Some(screen), 0, 0, SRCCOPY).is_ok();
        let mut pixels = vec![0_u8; width as usize * height as usize * 4];
        let mut info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: width,
                biHeight: -height,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let rows = if copied {
            // GetDIBits requires the bitmap not to be selected into a device context.
            let _ = SelectObject(memory, previous);
            GetDIBits(
                memory,
                bitmap,
                0,
                height as u32,
                Some(pixels.as_mut_ptr().cast()),
                &mut info,
                DIB_RGB_COLORS,
            )
        } else {
            let _ = SelectObject(memory, previous);
            0
        };
        let _ = DeleteObject(bitmap.into());
        let _ = DeleteDC(memory);
        let _ = ReleaseDC(None, screen);
        if rows == 0 {
            bail!("failed to capture input desktop with GDI")
        }
        let rgb = copy_bgra_to_rgb(
            pixels.as_ptr(),
            width as u32 * 4,
            width as u32,
            height as u32,
        );
        let source = ImageBuffer::<Rgb<u8>, _>::from_raw(width as u32, height as u32, rgb)
            .context("invalid GDI desktop image buffer")?;
        let source = DynamicImage::ImageRgb8(source);
        let (target_width, target_height) =
            scaled_dimensions(width as u32, height as u32, max_width, max_height);
        let image = if target_width != width as u32 || target_height != height as u32 {
            source.resize_exact(target_width, target_height, FilterType::Triangle)
        } else {
            source
        };
        let mut quality = quality;
        loop {
            let mut jpeg = Vec::new();
            image.write_with_encoder(image::codecs::jpeg::JpegEncoder::new_with_quality(
                &mut jpeg, quality,
            ))?;
            if jpeg.len() + 32 <= MAX_FRAME_BYTES || quality <= MIN_JPEG_QUALITY {
                if jpeg.len() + 32 > MAX_FRAME_BYTES {
                    bail!("frame_too_large")
                }
                return Ok(Some((jpeg, target_width, target_height)));
            }
            quality = quality.saturating_sub(5).max(MIN_JPEG_QUALITY);
        }
    }
}

struct InputState {
    keys: HashSet<(u16, bool)>,
    buttons: HashSet<u8>,
    allow_winlogon: bool,
    generation: u64,
    generation_desktop: Option<String>,
}

impl Default for InputState {
    fn default() -> Self {
        Self::new(false)
    }
}

impl InputState {
    fn new(allow_winlogon: bool) -> Self {
        Self {
            keys: HashSet::new(),
            buttons: HashSet::new(),
            allow_winlogon,
            generation: 0,
            generation_desktop: None,
        }
    }

    fn bind_generation(&mut self, generation: u64, desktop: &str) {
        if self.generation == generation
            && self
                .generation_desktop
                .as_deref()
                .is_some_and(|current| current.eq_ignore_ascii_case(desktop))
        {
            return;
        }
        // Never release input remembered from one desktop into a newly attached desktop.
        self.keys.clear();
        self.buttons.clear();
        self.generation = generation;
        self.generation_desktop = Some(desktop.to_string());
    }

    fn discard_for_generation(&mut self, generation: u64) {
        self.keys.clear();
        self.buttons.clear();
        self.generation = generation;
        self.generation_desktop = None;
    }

    fn apply(&mut self, control: DesktopControl) -> Result<()> {
        match control {
            DesktopControl::PointerMove { x, y } => send_pointer_move(x, y),
            DesktopControl::PointerButton { x, y, button, down } => {
                send_pointer_move(x, y)?;
                send_pointer_button(button, down)?;
                if down {
                    self.buttons.insert(button);
                } else {
                    self.buttons.remove(&button);
                }
                Ok(())
            }
            DesktopControl::Wheel {
                x,
                y,
                delta_x,
                delta_y,
            } => {
                send_pointer_move(x, y)?;
                send_wheel(delta_x, delta_y)
            }
            DesktopControl::Key { code, down, .. } => {
                let Some(vk) = dom_code_to_vk(&code) else {
                    return Ok(());
                };
                let extended = dom_code_uses_extended_key(&code);
                send_key(vk, down, extended)?;
                if down {
                    self.keys.insert((vk, extended));
                } else {
                    self.keys.remove(&(vk, extended));
                }
                Ok(())
            }
            DesktopControl::ReleaseAll => {
                if !self.release_all() {
                    bail!("one or more remote desktop inputs are still pending release")
                }
                Ok(())
            }
            DesktopControl::SecureAttention => {
                bail!("secure_attention_unavailable")
            }
            DesktopControl::Feedback { .. } | DesktopControl::AudioControl { .. } => Ok(()),
        }
    }

    fn release_all(&mut self) -> bool {
        for key in self.keys.clone() {
            if send_key(key.0, false, key.1).is_ok() {
                self.keys.remove(&key);
            }
        }
        for button in self.buttons.clone() {
            if send_pointer_button(button, false).is_ok() {
                self.buttons.remove(&button);
            }
        }
        self.keys.is_empty() && self.buttons.is_empty()
    }
}

impl Drop for InputState {
    fn drop(&mut self) {
        self.keys.clear();
        self.buttons.clear();
    }
}

struct ImpersonationGuard;

impl Drop for ImpersonationGuard {
    fn drop(&mut self) {
        unsafe {
            let _ = RevertToSelf();
        }
    }
}

// SendSAS is void and never reports whether Winlogon acted on the request; the
// SoftwareSASGeneration policy decides that. Check it up front so a disallowed
// request fails with a real error instead of a fabricated success.
fn ensure_service_sas_generation_allowed() -> Result<()> {
    const SAS_GENERATED_BY_SERVICES: u32 = 1;
    const SAS_GENERATED_BY_SERVICES_AND_EASE_OF_ACCESS: u32 = 3;
    let subkey = wide(r"SOFTWARE\Microsoft\Windows\CurrentVersion\Policies\System");
    let value_name = wide("SoftwareSASGeneration");
    let mut value = 0u32;
    let mut size = std::mem::size_of::<u32>() as u32;
    unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            PCWSTR(subkey.as_ptr()),
            PCWSTR(value_name.as_ptr()),
            RRF_RT_REG_DWORD,
            None,
            Some((&mut value as *mut u32).cast()),
            Some(&mut size),
        )
        .ok()
        .context(
            "secure_attention_policy_denied: the SoftwareSASGeneration policy is not configured",
        )?;
    }
    if !matches!(
        value,
        SAS_GENERATED_BY_SERVICES | SAS_GENERATED_BY_SERVICES_AND_EASE_OF_ACCESS
    ) {
        bail!(
            "secure_attention_policy_denied: SoftwareSASGeneration={value} does not permit services to simulate Ctrl+Alt+Del"
        )
    }
    Ok(())
}

fn send_service_secure_attention(session_id: u32) -> Result<()> {
    if current_session_id()? != 0 || !current_process_is_local_system()? {
        bail!("secure_attention_unavailable: caller is not the managed LocalSystem service")
    }
    ensure_service_sas_generation_allowed()?;
    unsafe {
        let token = duplicate_session_system_token(session_id)?;
        let impersonated = ImpersonateLoggedOnUser(token);
        let _ = CloseHandle(token);
        impersonated.context("secure_attention_unavailable: Winlogon impersonation failed")?;
        let _impersonation = ImpersonationGuard;

        let library_name = wide("sas.dll");
        let library = LoadLibraryW(PCWSTR(library_name.as_ptr()))
            .context("secure_attention_unavailable: failed to load sas.dll")?;
        let address = GetProcAddress(library, windows::core::s!("SendSAS"));
        let Some(address) = address else {
            let _ = FreeLibrary(library);
            bail!("secure_attention_unavailable: SendSAS is unavailable")
        };
        let send_sas: unsafe extern "system" fn(i32) = std::mem::transmute(address);
        send_sas(0);
        let _ = FreeLibrary(library);
        Ok(())
    }
}

fn send_pointer_move(x: f64, y: f64) -> Result<()> {
    let x = absolute_pointer_coordinate(x);
    let y = absolute_pointer_coordinate(y);
    send_mouse(x, y, 0, MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE)
}

fn send_pointer_button(button: u8, down: bool) -> Result<()> {
    let flags = match (button, down) {
        (0, true) => MOUSEEVENTF_LEFTDOWN,
        (0, false) => MOUSEEVENTF_LEFTUP,
        (1, true) => MOUSEEVENTF_MIDDLEDOWN,
        (1, false) => MOUSEEVENTF_MIDDLEUP,
        (2, true) => MOUSEEVENTF_RIGHTDOWN,
        (2, false) => MOUSEEVENTF_RIGHTUP,
        _ => return Ok(()),
    };
    send_mouse(0, 0, 0, flags)
}

fn send_wheel(delta_x: i32, delta_y: i32) -> Result<()> {
    if delta_y != 0 {
        send_mouse(0, 0, delta_y.saturating_neg() as u32, MOUSEEVENTF_WHEEL)?;
    }
    if delta_x != 0 {
        send_mouse(0, 0, delta_x as u32, MOUSEEVENTF_HWHEEL)?;
    }
    Ok(())
}

fn send_mouse(
    dx: i32,
    dy: i32,
    data: u32,
    flags: windows::Win32::UI::Input::KeyboardAndMouse::MOUSE_EVENT_FLAGS,
) -> Result<()> {
    let input = INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx,
                dy,
                mouseData: data,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    };
    let sent = unsafe {
        windows_sys::Win32::Foundation::SetLastError(0);
        SendInput(&[input], size_of::<INPUT>() as i32)
    };
    if sent != 1 {
        return Err(send_input_error("mouse"));
    }
    Ok(())
}

fn send_key(vk: u16, down: bool, extended: bool) -> Result<()> {
    let mut flags = if down {
        Default::default()
    } else {
        KEYEVENTF_KEYUP
    };
    if extended {
        flags |= KEYEVENTF_EXTENDEDKEY;
    }
    let input = INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(vk),
                wScan: 0,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    };
    let sent = unsafe {
        windows_sys::Win32::Foundation::SetLastError(0);
        SendInput(&[input], size_of::<INPUT>() as i32)
    };
    if sent != 1 {
        return Err(send_input_error("keyboard"));
    }
    Ok(())
}

fn send_input_error(kind: &str) -> anyhow::Error {
    let code = unsafe { windows_sys::Win32::Foundation::GetLastError() };
    if code == 0 {
        anyhow!(
            "SendInput {kind} failed (Win32 error 0; Windows may have blocked input through UIPI or desktop/session isolation)"
        )
    } else {
        let description = std::io::Error::from_raw_os_error(code as i32);
        anyhow!("SendInput {kind} failed (Win32 error {code}: {description})")
    }
}

fn log_input_injection_error(
    error: &anyhow::Error,
    last_log: &mut Option<Instant>,
    suppressed: &mut u64,
) {
    if last_log
        .as_ref()
        .is_some_and(|last| last.elapsed() < INPUT_ERROR_LOG_INTERVAL)
    {
        *suppressed = suppressed.saturating_add(1);
        return;
    }
    if *suppressed == 0 {
        crate::logging::error(format_args!(
            "remote desktop input injection failed: {error:#}"
        ));
    } else {
        crate::logging::error(format_args!(
            "remote desktop input injection failed: {error:#} ({} similar errors suppressed)",
            *suppressed
        ));
    }
    *last_log = Some(Instant::now());
    *suppressed = 0;
}

fn default_input_desktop() -> bool {
    unsafe {
        let Ok(desktop) = OpenInputDesktop(Default::default(), false, DESKTOP_READOBJECTS) else {
            return false;
        };
        let desktop_handle = HANDLE(desktop.0);
        let mut needed = 0_u32;
        let _ = GetUserObjectInformationW(desktop_handle, UOI_NAME, None, 0, Some(&mut needed));
        let mut value = vec![0_u16; (needed as usize / 2).max(1)];
        let result = GetUserObjectInformationW(
            desktop_handle,
            UOI_NAME,
            Some(value.as_mut_ptr().cast()),
            needed,
            Some(&mut needed),
        )
        .is_ok();
        let _ = CloseDesktop(desktop);
        if !result {
            return false;
        }
        let len = value.iter().position(|v| *v == 0).unwrap_or(value.len());
        String::from_utf16_lossy(&value[..len]).eq_ignore_ascii_case("Default")
    }
}

fn controllable_input_desktop(unattended: bool) -> bool {
    unsafe {
        let Ok(desktop) = OpenInputDesktop(Default::default(), false, DESKTOP_READOBJECTS) else {
            return false;
        };
        let name = desktop_name(desktop).ok();
        let _ = CloseDesktop(desktop);
        name.is_some_and(|name| desktop_controllable(&name, unattended))
    }
}

#[derive(Clone, Copy)]
enum HelperTarget {
    Current { session_id: u32 },
    ServiceSession { session_id: u32 },
}

impl HelperTarget {
    fn session_id(self) -> u32 {
        match self {
            Self::Current { session_id } | Self::ServiceSession { session_id } => session_id,
        }
    }
}

enum HelperProcess {
    Child(Child),
    Handle { handle: HANDLE, pid: u32 },
}

unsafe impl Send for HelperProcess {}

impl HelperProcess {
    fn terminate(&mut self) {
        match self {
            Self::Child(child) => {
                let _ = child.kill();
                unsafe {
                    let handle = HANDLE(child.as_raw_handle());
                    let _ = WaitForSingleObject(handle, 5_000);
                }
                let _ = child.try_wait();
            }
            Self::Handle { handle, .. } if !handle.is_invalid() => unsafe {
                let _ = TerminateProcess(*handle, 1);
                let _ = WaitForSingleObject(*handle, 5_000);
            },
            Self::Handle { .. } => {}
        }
    }

    fn pid(&self) -> u32 {
        match self {
            Self::Child(child) => child.id(),
            Self::Handle { pid, .. } => *pid,
        }
    }

    fn exit_code(&mut self) -> Option<u32> {
        match self {
            Self::Child(child) => child
                .try_wait()
                .ok()
                .flatten()
                .and_then(|status| status.code())
                .map(|code| code as u32),
            Self::Handle { handle, .. } if !handle.is_invalid() => unsafe {
                let mut code = STILL_ACTIVE.0 as u32;
                GetExitCodeProcess(*handle, &mut code)
                    .is_ok()
                    .then_some(code)
                    .filter(|code| *code != STILL_ACTIVE.0 as u32)
            },
            Self::Handle { .. } => None,
        }
    }
}

impl Drop for HelperProcess {
    fn drop(&mut self) {
        self.terminate();
        if let Self::Handle { handle, .. } = self {
            if !handle.is_invalid() {
                unsafe {
                    let _ = CloseHandle(*handle);
                }
            }
        }
    }
}

fn spawn_helper(
    options: &DesktopOptions,
    target: HelperTarget,
    config: &AgentConfig,
) -> Result<HelperProcess> {
    match target {
        HelperTarget::ServiceSession { session_id } => {
            spawn_helper_in_active_session(options, session_id, config)
        }
        HelperTarget::Current { .. } => {
            let mut command = Command::new(std::env::current_exe()?);
            append_helper_args(&mut command, options);
            config.append_cli_args(&mut command);
            command
                .creation_flags(CREATE_NO_WINDOW_FLAG)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .context("failed to launch desktop helper")
                .map(HelperProcess::Child)
        }
    }
}

fn append_helper_args(command: &mut Command, options: &DesktopOptions) {
    command
        .arg("desktop-helper")
        .arg("--pipe")
        .arg(&options.pipe)
        .arg("--max-width")
        .arg(options.max_width.to_string())
        .arg("--max-height")
        .arg(options.max_height.to_string())
        .arg("--min-fps")
        .arg(options.min_fps.to_string())
        .arg("--max-fps")
        .arg(options.max_fps.to_string())
        .arg("--jpeg-quality")
        .arg(options.jpeg_quality.to_string())
        .arg("--display-source")
        .arg(&options.display_source);
    if let Some(codec) = &options.audio_codec {
        command.arg("--audio-codec").arg(codec);
    }
    if options.system_helper {
        command.arg("--system-helper");
    }
    if options.unattended {
        command.arg("--unattended");
    }
}

fn current_session_id() -> Result<u32> {
    let mut id = 0_u32;
    unsafe {
        ProcessIdToSessionId(GetCurrentProcessId(), &mut id)?;
    }
    Ok(id)
}

fn helper_target(unattended: bool) -> Result<(HelperTarget, String)> {
    let current_session_id = current_session_id()?;
    if current_session_id == 0 {
        let session_id = select_active_session(unattended)?;
        Ok((
            HelperTarget::ServiceSession { session_id },
            "SY".to_string(),
        ))
    } else {
        let mut token = HANDLE::default();
        unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token)? };
        let sid = token_user_sid(token);
        unsafe {
            let _ = CloseHandle(token);
        }
        Ok((
            HelperTarget::Current {
                session_id: current_session_id,
            },
            sid?,
        ))
    }
}

fn current_process_is_local_system() -> Result<bool> {
    unsafe {
        let mut token = HANDLE::default();
        OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token)
            .context("failed to inspect remote desktop service token")?;
        let sid = token_user_sid(token);
        let _ = CloseHandle(token);
        Ok(sid? == LOCAL_SYSTEM_SID)
    }
}

fn token_user_sid(token: HANDLE) -> Result<String> {
    unsafe {
        let mut needed = 0_u32;
        let _ = GetTokenInformation(token, TokenUser, None, 0, &mut needed);
        if needed < size_of::<TOKEN_USER>() as u32 {
            bail!("Windows user token did not contain a SID")
        }
        let words = (needed as usize).div_ceil(size_of::<usize>());
        let mut buffer = vec![0_usize; words];
        GetTokenInformation(
            token,
            TokenUser,
            Some(buffer.as_mut_ptr().cast()),
            needed,
            &mut needed,
        )?;
        let token_user = &*(buffer.as_ptr().cast::<TOKEN_USER>());
        let mut string_sid = PWSTR::null();
        ConvertSidToStringSidW(token_user.User.Sid, &mut string_sid)?;
        let sid = string_sid
            .to_string()
            .context("Windows user SID was not UTF-16")?;
        let _ = LocalFree(Some(windows::Win32::Foundation::HLOCAL(
            string_sid.0.cast(),
        )));
        Ok(sid)
    }
}

fn select_active_session(unattended: bool) -> Result<u32> {
    unsafe {
        let console = WTSGetActiveConsoleSessionId();
        let mut sessions: *mut WTS_SESSION_INFOW = null_mut();
        let mut count = 0_u32;
        WTSEnumerateSessionsW(
            Some(WTS_CURRENT_SERVER_HANDLE),
            0,
            1,
            &mut sessions,
            &mut count,
        )
        .context("failed to enumerate Windows sessions")?;
        let active = if sessions.is_null() {
            Vec::new()
        } else {
            std::slice::from_raw_parts(sessions, count as usize)
                .iter()
                .filter(|session| session.State == WTSActive)
                .map(|session| session.SessionId)
                .collect::<Vec<_>>()
        };
        if !sessions.is_null() {
            WTSFreeMemory(sessions.cast());
        }
        let console_has_winlogon = unattended
            && console != u32::MAX
            && duplicate_session_system_token(console).is_ok_and(|token| {
                let _ = CloseHandle(token);
                true
            });
        choose_active_session(console, &active, unattended, console_has_winlogon)
    }
}

fn choose_active_session(
    console: u32,
    active: &[u32],
    unattended: bool,
    console_has_winlogon: bool,
) -> Result<u32> {
    if active.contains(&console) {
        return Ok(console);
    }
    if unattended && console != u32::MAX && console_has_winlogon {
        return Ok(console);
    }
    match active {
        [] => bail!("no_active_session"),
        [only] => Ok(*only),
        _ => bail!("multiple_active_sessions"),
    }
}

fn duplicate_session_system_token(session_id: u32) -> Result<HANDLE> {
    unsafe {
        // Merely changing TokenSessionId on the Session 0 service token does not give the helper
        // the target interactive logon context. Winlogon already owns the correct LocalSystem
        // token for this session, so duplicate that token after validating its SID.
        let mut processes: *mut WTS_PROCESS_INFOW = null_mut();
        let mut count = 0_u32;
        WTSEnumerateProcessesW(
            Some(WTS_CURRENT_SERVER_HANDLE),
            0,
            1,
            &mut processes,
            &mut count,
        )
        .context("failed to enumerate target session processes")?;

        let result = (|| -> Result<HANDLE> {
            if processes.is_null() {
                bail!("target session process enumeration returned no data")
            }
            let entries = std::slice::from_raw_parts(processes, count as usize);
            let mut last_error = None;
            for process in entries {
                if process.SessionId != session_id || process.pProcessName.is_null() {
                    continue;
                }
                let Ok(name) = process.pProcessName.to_string() else {
                    continue;
                };
                if !name.eq_ignore_ascii_case("winlogon.exe") {
                    continue;
                }
                match duplicate_system_process_token(process.ProcessId) {
                    Ok(Some(token)) => return Ok(token),
                    Ok(None) => {}
                    Err(error) => last_error = Some(error),
                }
            }
            if let Some(error) = last_error {
                Err(error).context("failed to duplicate target session winlogon token")
            } else {
                bail!("no LocalSystem winlogon process found in target session {session_id}")
            }
        })();
        WTSFreeMemory(processes.cast());
        result
    }
}

fn duplicate_system_process_token(process_id: u32) -> Result<Option<HANDLE>> {
    unsafe {
        let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, process_id)
            .with_context(|| format!("failed to open winlogon process {process_id}"))?;
        let mut source_token = HANDLE::default();
        let opened = OpenProcessToken(process, TOKEN_DUPLICATE | TOKEN_QUERY, &mut source_token);
        let _ = CloseHandle(process);
        opened.with_context(|| format!("failed to open winlogon token {process_id}"))?;

        let sid = match token_user_sid(source_token) {
            Ok(sid) => sid,
            Err(error) => {
                let _ = CloseHandle(source_token);
                return Err(error).context("failed to identify winlogon token owner");
            }
        };
        if sid != LOCAL_SYSTEM_SID {
            let _ = CloseHandle(source_token);
            return Ok(None);
        }

        let mut primary_token = HANDLE::default();
        let duplicated = DuplicateTokenEx(
            source_token,
            TOKEN_ALL_ACCESS,
            None,
            SecurityImpersonation,
            TokenPrimary,
            &mut primary_token,
        );
        let _ = CloseHandle(source_token);
        duplicated.context("failed to duplicate winlogon primary token")?;
        Ok(Some(primary_token))
    }
}

fn spawn_helper_in_active_session(
    options: &DesktopOptions,
    session_id: u32,
    config: &AgentConfig,
) -> Result<HelperProcess> {
    unsafe {
        let primary_token = duplicate_session_system_token(session_id)?;
        let result = (|| -> Result<HelperProcess> {
            crate::logging::info(format_args!(
                "launching remote desktop helper with target session LocalSystem token: session_id={session_id}"
            ));

            let executable = std::env::current_exe()?;
            let mut command = Command::new(&executable);
            append_helper_args(&mut command, options);
            config.append_cli_args(&mut command);
            let mut command_line = windows_command_line(executable.as_os_str(), command.get_args());
            let application = wide(executable.as_os_str());
            let desktop = wide("winsta0\\default");
            let mut environment: *mut c_void = null_mut();
            CreateEnvironmentBlock(&mut environment, Some(primary_token), false)?;
            let startup = STARTUPINFOW {
                cb: size_of::<STARTUPINFOW>() as u32,
                lpDesktop: PWSTR(desktop.as_ptr() as *mut _),
                ..zeroed()
            };
            let mut process: PROCESS_INFORMATION = zeroed();
            let created = CreateProcessAsUserW(
                Some(primary_token),
                PCWSTR(application.as_ptr()),
                Some(PWSTR(command_line.as_mut_ptr())),
                None,
                None,
                false,
                CREATE_UNICODE_ENVIRONMENT | CREATE_NO_WINDOW,
                Some(environment),
                PCWSTR(null()),
                &startup,
                &mut process,
            );
            let _ = DestroyEnvironmentBlock(environment);
            created?;
            let _ = CloseHandle(process.hThread);
            Ok(HelperProcess::Handle {
                handle: process.hProcess,
                pid: process.dwProcessId,
            })
        })();
        let _ = CloseHandle(primary_token);
        result
    }
}

fn spawn_device_probe_in_session(pipe_name: &str, session_id: u32) -> Result<HelperProcess> {
    unsafe {
        let primary_token = duplicate_session_system_token(session_id)
            .map_err(|_| anyhow!("device_probe_failed"))?;
        let mut restricted_token = HANDLE::default();
        let restricted = CreateRestrictedToken(
            primary_token,
            DISABLE_MAX_PRIVILEGE,
            None,
            None,
            None,
            &mut restricted_token,
        );
        let _ = CloseHandle(primary_token);
        restricted.map_err(|_| anyhow!("device_probe_failed"))?;

        let result = (|| -> Result<HelperProcess> {
            let executable = std::env::current_exe().context("device_probe_failed")?;
            let arguments = [
                OsStr::new("device-probe"),
                OsStr::new("--pipe"),
                OsStr::new(pipe_name),
            ];
            let mut command_line = windows_command_line(executable.as_os_str(), arguments);
            let application = wide(executable.as_os_str());
            let desktop = wide("winsta0\\default");
            let mut environment: *mut c_void = null_mut();
            CreateEnvironmentBlock(&mut environment, Some(restricted_token), false)
                .map_err(|_| anyhow!("device_probe_failed"))?;
            let startup = STARTUPINFOW {
                cb: size_of::<STARTUPINFOW>() as u32,
                lpDesktop: PWSTR(desktop.as_ptr() as *mut _),
                ..zeroed()
            };
            let mut process: PROCESS_INFORMATION = zeroed();
            let created = CreateProcessAsUserW(
                Some(restricted_token),
                PCWSTR(application.as_ptr()),
                Some(PWSTR(command_line.as_mut_ptr())),
                None,
                None,
                false,
                CREATE_UNICODE_ENVIRONMENT | CREATE_NO_WINDOW,
                Some(environment),
                PCWSTR(null()),
                &startup,
                &mut process,
            );
            let _ = DestroyEnvironmentBlock(environment);
            created.map_err(|_| anyhow!("device_probe_failed"))?;
            let _ = CloseHandle(process.hThread);
            Ok(HelperProcess::Handle {
                handle: process.hProcess,
                pid: process.dwProcessId,
            })
        })();
        let _ = CloseHandle(restricted_token);
        result
    }
}

fn wide(value: impl AsRef<OsStr>) -> Vec<u16> {
    value.as_ref().encode_wide().chain(Some(0)).collect()
}

fn desktop_websocket_url(config: &AgentConfig, session_id: &str) -> Result<reqwest::Url> {
    let mut url = config
        .server_endpoint()?
        .websocket_url("api/agent/desktop/ws")?;
    url.query_pairs_mut().append_pair("session_id", session_id);
    Ok(url)
}

fn validate_frame(frame: &[u8]) -> Result<()> {
    if frame.len() > MAX_FRAME_BYTES {
        bail!("frame_too_large")
    }
    let header = FrameHeader::decode(frame)?;
    if header.width == 0 || header.height == 0 || header.width > 1920 || header.height > 1080 {
        bail!("invalid desktop frame dimensions")
    }
    Ok(())
}

fn validate_audio_frame(frame: &[u8]) -> Result<()> {
    if frame.len() <= AUDIO_FRAME_HEADER_LEN || frame.len() > MAX_AUDIO_FRAME_BYTES {
        bail!("invalid remote desktop audio frame size")
    }
    let header = AudioFrameHeader::decode(frame)?;
    if header.sample_rate != AUDIO_SAMPLE_RATE
        || header.samples_per_channel != AUDIO_SAMPLES_PER_FRAME
        || AUDIO_CHANNELS != 2
    {
        bail!("invalid remote desktop audio format")
    }
    Ok(())
}

async fn write_packet<W: AsyncWrite + Unpin>(writer: &mut W, kind: u8, value: &[u8]) -> Result<()> {
    if value.len() > PIPE_MAX_PACKET {
        bail!("desktop helper packet too large")
    }
    writer.write_u32((value.len() + 1) as u32).await?;
    writer.write_u8(kind).await?;
    writer.write_all(value).await?;
    writer.flush().await?;
    Ok(())
}

async fn write_fatal_packet<W: AsyncWrite + Unpin>(writer: &mut W, reason: &str) -> Result<()> {
    let mut fatal = Vec::with_capacity(INTERNAL_FATAL_PREFIX.len() + reason.len());
    fatal.extend_from_slice(INTERNAL_FATAL_PREFIX);
    fatal.extend_from_slice(reason.as_bytes());
    write_packet(writer, PIPE_INTERNAL, &fatal).await
}

async fn read_packet<R: AsyncRead + Unpin>(reader: &mut R) -> Result<(u8, Vec<u8>)> {
    let length = reader.read_u32().await? as usize;
    if length == 0 || length > PIPE_MAX_PACKET + 1 {
        bail!("invalid desktop helper packet length")
    }
    let kind = reader.read_u8().await?;
    let mut value = vec![0_u8; length - 1];
    reader.read_exact(&mut value).await?;
    Ok((kind, value))
}

#[cfg(test)]
async fn write_fragmented_packet<W: AsyncWrite + Unpin>(
    writer: &mut W,
    kind: u8,
    value: &[u8],
) -> Result<()> {
    let length = ((value.len() + 1) as u32).to_be_bytes();
    for byte in length
        .into_iter()
        .chain(std::iter::once(kind))
        .chain(value.iter().copied())
    {
        writer.write_all(&[byte]).await?;
        tokio::task::yield_now().await;
    }
    Ok(())
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_probe_pipe_creation_enters_client_runtime() {
        let result = std::thread::spawn(|| -> Result<()> {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_io()
                .enable_time()
                .build()?;
            let pipe_name = format!(r"\\.\pipe\om-device-probe-test-{}", Uuid::new_v4());
            drop(create_device_probe_pipe(&runtime, &pipe_name)?);
            Ok(())
        })
        .join()
        .expect("device probe pipe test thread panicked");

        assert!(
            result.is_ok(),
            "failed to create device probe pipe: {result:?}"
        );
    }

    fn test_audio_frame_with_flags(payload_len: usize, flags: u8) -> Vec<u8> {
        let mut frame = AudioFrameHeader {
            flags,
            sequence: 1,
            timestamp_us: 0,
            sample_rate: AUDIO_SAMPLE_RATE,
            samples_per_channel: AUDIO_SAMPLES_PER_FRAME,
        }
        .encode()
        .to_vec();
        frame.resize(AUDIO_FRAME_HEADER_LEN + payload_len, 0x55);
        frame
    }

    fn test_audio_frame(payload_len: usize) -> Vec<u8> {
        test_audio_frame_with_flags(payload_len, 0)
    }

    #[tokio::test]
    async fn helper_pipe_reader_preserves_fragmented_packet_boundaries() {
        let (mut writer, reader) = tokio::io::duplex(64);
        let (packet_tx, mut packet_rx) = mpsc::channel(1);
        let reader = tokio::spawn(helper_pipe_reader(reader, packet_tx));
        let packets = [
            (
                PIPE_CONTROL,
                br#"{"type":"feedback","sequence":0}"#.as_slice(),
            ),
            (PIPE_INTERNAL, INTERNAL_STOP),
        ];

        for (kind, value) in packets {
            write_fragmented_packet(&mut writer, kind, value)
                .await
                .unwrap();
            assert_eq!(
                packet_rx.recv().await.unwrap().unwrap(),
                (kind, value.to_vec())
            );
        }

        drop(writer);
        assert!(packet_rx.recv().await.unwrap().is_err());
        reader.await.unwrap();
    }

    #[test]
    fn audio_frame_validation_enforces_format_and_opus_payload_bounds() {
        assert!(validate_audio_frame(&test_audio_frame(1)).is_ok());
        assert!(validate_audio_frame(&test_audio_frame(1_275)).is_ok());
        assert!(validate_audio_frame(&test_audio_frame(0)).is_err());
        assert!(validate_audio_frame(&test_audio_frame(1_276)).is_err());

        let mut wrong_rate = test_audio_frame(1);
        wrong_rate[24..28].copy_from_slice(&44_100_u32.to_be_bytes());
        assert!(validate_audio_frame(&wrong_rate).is_err());
    }

    #[tokio::test]
    async fn pipe_reader_routes_audio_packets_without_blocking_video_statuses() {
        let (mut writer, reader) = tokio::io::duplex(256);
        let (frame_tx, _frame_rx) = watch::channel(None);
        let (audio_tx, audio_rx) = drop_oldest_channel(AUDIO_CHANNEL_CAPACITY);
        let (status_tx, mut status_rx) = mpsc::channel(1);
        let (input_barrier_tx, _input_barrier_rx) = mpsc::channel(1);
        let (input_ready_tx, _input_ready_rx) = mpsc::channel(1);
        let (release_ack_tx, _release_ack_rx) = mpsc::channel(1);
        let (audio_ack_tx, _audio_ack_rx) = mpsc::channel(1);
        let (fatal_tx, mut fatal_rx) = mpsc::channel(1);
        let task = tokio::spawn(pipe_reader(
            reader,
            frame_tx,
            audio_tx,
            status_tx,
            input_barrier_tx,
            input_ready_tx,
            release_ack_tx,
            audio_ack_tx,
            fatal_tx,
        ));

        write_packet(&mut writer, PIPE_AUDIO, b"invalid")
            .await
            .unwrap();
        let audio = test_audio_frame(1);
        write_packet(&mut writer, PIPE_AUDIO, &audio).await.unwrap();
        write_packet(
            &mut writer,
            PIPE_CONTROL,
            br#"{"type":"audio_state","state":"playing"}"#,
        )
        .await
        .unwrap();

        assert_eq!(audio_rx.recv().await, Some(audio));
        assert_eq!(
            status_rx.recv().await.as_deref(),
            Some(r#"{"type":"audio_state","state":"playing"}"#)
        );
        drop(writer);
        assert!(fatal_rx.recv().await.is_some());
        assert!(task.await.unwrap().is_err());
    }

    #[test]
    fn audio_control_ack_round_trips_and_rejects_malformed_payloads() {
        let ack = AudioControlAck {
            enabled: true,
            changed: true,
            generation: Some(42),
        };
        let encoded = encode_audio_control_ack(ack).unwrap();
        assert_eq!(parse_audio_control_ack(&encoded).unwrap(), Some(ack));
        assert_eq!(parse_audio_control_ack(INTERNAL_STOPPED).unwrap(), None);

        let mut unknown = INTERNAL_AUDIO_CONTROL_ACK_PREFIX.to_vec();
        unknown
            .extend_from_slice(br#"{"enabled":true,"changed":true,"generation":42,"error":"raw"}"#);
        assert!(parse_audio_control_ack(&unknown).is_err());
        assert!(parse_audio_control_ack(INTERNAL_AUDIO_CONTROL_ACK_PREFIX).is_err());
    }

    #[test]
    fn audio_relay_requires_ordered_latest_changed_ack_and_discontinuity() {
        let mut relay = AudioRelayGate::new();
        relay.register_control(Some(1), true).unwrap();
        relay.register_control(Some(2), false).unwrap();

        assert!(
            relay
                .acknowledge(AudioControlAck {
                    enabled: false,
                    changed: true,
                    generation: Some(2),
                })
                .is_err()
        );
        assert_eq!(relay.pending.len(), 2);
        assert_eq!(
            relay
                .acknowledge(AudioControlAck {
                    enabled: true,
                    changed: true,
                    generation: Some(1),
                })
                .unwrap(),
            None
        );
        assert_eq!(relay.phase, AudioRelayPhase::Disabled);
        assert_eq!(
            relay
                .acknowledge(AudioControlAck {
                    enabled: false,
                    changed: true,
                    generation: Some(2),
                })
                .unwrap(),
            None
        );

        relay.register_control(Some(3), true).unwrap();
        assert_eq!(
            relay
                .acknowledge(AudioControlAck {
                    enabled: true,
                    changed: true,
                    generation: Some(3),
                })
                .unwrap(),
            Some(3)
        );
        assert_eq!(relay.phase, AudioRelayPhase::AwaitingDiscontinuity);
        assert!(!relay.accepts_audio(&test_audio_frame(1)));
        assert!(relay.accepts_audio(&test_audio_frame_with_flags(
            1,
            super::super::AUDIO_FLAG_DISCONTINUITY,
        )));
        assert_eq!(relay.phase, AudioRelayPhase::Playing);
    }

    #[test]
    fn duplicate_enabled_ack_does_not_interrupt_playing_audio() {
        let mut relay = AudioRelayGate::new();
        relay.register_control(Some(1), true).unwrap();
        assert_eq!(
            relay
                .acknowledge(AudioControlAck {
                    enabled: true,
                    changed: true,
                    generation: Some(1),
                })
                .unwrap(),
            Some(1)
        );
        assert!(relay.accepts_audio(&test_audio_frame_with_flags(
            1,
            super::super::AUDIO_FLAG_DISCONTINUITY,
        )));

        relay.register_control(Some(2), true).unwrap();
        assert_eq!(relay.phase, AudioRelayPhase::Playing);
        assert_eq!(
            relay
                .acknowledge(AudioControlAck {
                    enabled: true,
                    changed: false,
                    generation: Some(2),
                })
                .unwrap(),
            None
        );
        assert_eq!(relay.phase, AudioRelayPhase::Playing);
        assert!(relay.accepts_audio(&test_audio_frame(1)));
    }

    #[test]
    fn duplicate_enabled_ack_preserves_discontinuity_wait() {
        let mut relay = AudioRelayGate::new();
        relay.register_control(Some(1), true).unwrap();
        assert_eq!(
            relay
                .acknowledge(AudioControlAck {
                    enabled: true,
                    changed: true,
                    generation: Some(1),
                })
                .unwrap(),
            Some(1)
        );
        assert_eq!(relay.phase, AudioRelayPhase::AwaitingDiscontinuity);

        relay.register_control(Some(2), true).unwrap();
        assert_eq!(relay.phase, AudioRelayPhase::AwaitingDiscontinuity);
        assert_eq!(
            relay
                .acknowledge(AudioControlAck {
                    enabled: true,
                    changed: false,
                    generation: Some(2),
                })
                .unwrap(),
            None
        );
        assert_eq!(relay.phase, AudioRelayPhase::AwaitingDiscontinuity);
        assert!(relay.accepts_audio(&test_audio_frame_with_flags(
            1,
            super::super::AUDIO_FLAG_DISCONTINUITY,
        )));
    }

    #[tokio::test]
    async fn changed_audio_ack_clears_queued_frames_at_the_pipe_boundary() {
        let (mut writer, reader) = tokio::io::duplex(512);
        let (frame_tx, _frame_rx) = watch::channel(None);
        let (audio_tx, audio_rx) = drop_oldest_channel(AUDIO_CHANNEL_CAPACITY);
        let (status_tx, _status_rx) = mpsc::channel(1);
        let (input_barrier_tx, _input_barrier_rx) = mpsc::channel(1);
        let (input_ready_tx, _input_ready_rx) = mpsc::channel(1);
        let (release_ack_tx, _release_ack_rx) = mpsc::channel(1);
        let (audio_ack_tx, mut audio_ack_rx) = mpsc::channel(1);
        let (fatal_tx, _fatal_rx) = mpsc::channel(1);
        let task = tokio::spawn(pipe_reader(
            reader,
            frame_tx,
            audio_tx,
            status_tx,
            input_barrier_tx,
            input_ready_tx,
            release_ack_tx,
            audio_ack_tx,
            fatal_tx,
        ));

        let old = test_audio_frame(1);
        write_packet(&mut writer, PIPE_AUDIO, &old).await.unwrap();
        let ack = AudioControlAck {
            enabled: true,
            changed: true,
            generation: Some(9),
        };
        write_packet(
            &mut writer,
            PIPE_INTERNAL,
            &encode_audio_control_ack(ack).unwrap(),
        )
        .await
        .unwrap();
        assert_eq!(audio_ack_rx.recv().await, Some(ack));

        let mut current = test_audio_frame_with_flags(1, super::super::AUDIO_FLAG_DISCONTINUITY);
        current[15] = 2;
        write_packet(&mut writer, PIPE_AUDIO, &current)
            .await
            .unwrap();
        assert_eq!(audio_rx.recv().await, Some(current));

        task.abort();
    }

    #[test]
    fn backend_audio_ack_status_is_generation_scoped() {
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&audio_control_ack_status(17)).unwrap(),
            serde_json::json!({
                "type":"audio_state",
                "state":"starting",
                "reason":"control_ack",
                "generation":17,
            })
        );
    }

    #[tokio::test]
    async fn non_playing_audio_state_discards_queued_audio() {
        let (audio_tx, audio_rx) = drop_oldest_channel(2);
        audio_tx.send(vec![1]).unwrap();
        assert!(audio_state_clears_queue(
            r#"{"type":"audio_state","state":"paused","reason":"secure_desktop"}"#
        ));
        audio_tx.clear();
        audio_tx.send(vec![2]).unwrap();
        assert_eq!(audio_rx.recv().await, Some(vec![2]));
    }

    #[test]
    fn input_desktop_access_includes_injection_rights() {
        for required in [
            DESKTOP_CREATEMENU.0,
            DESKTOP_CREATEWINDOW.0,
            DESKTOP_ENUMERATE.0,
            DESKTOP_HOOKCONTROL.0,
            DESKTOP_READOBJECTS.0,
            DESKTOP_SWITCHDESKTOP.0,
            DESKTOP_WRITEOBJECTS.0,
            GENERIC_WRITE.0,
        ] {
            assert_eq!(INPUT_DESKTOP_ACCESS.0 & required, required);
        }
    }

    #[test]
    fn interactive_window_station_access_matches_win32_all_access() {
        assert_eq!(WINSTA_ALL_ACCESS_MASK, 0x037f);
    }

    #[test]
    fn input_generation_gate_waits_for_current_first_frame_feedback() {
        let mut gate = RelayInputGate::default();
        gate.begin_generation(1);
        assert!(!gate.allows(&DesktopControl::PointerMove { x: 0.5, y: 0.5 }));
        gate.arm_after_first_frame(10);
        gate.observe_feedback(9);
        assert!(!gate.allows(&DesktopControl::PointerMove { x: 0.5, y: 0.5 }));
        gate.observe_feedback(10);
        assert!(gate.allows(&DesktopControl::PointerMove { x: 0.5, y: 0.5 }));

        gate.begin_generation(2);
        assert!(!gate.allows(&DesktopControl::PointerMove { x: 0.5, y: 0.5 }));
        gate.observe_feedback(10);
        assert!(!gate.ready);
        gate.arm_after_first_frame(20);
        gate.observe_feedback(19);
        assert!(!gate.ready);
        gate.observe_feedback(20);
        assert!(gate.ready);
    }

    #[test]
    fn audio_control_bypasses_the_remote_input_gate() {
        let gate = RelayInputGate::default();
        assert!(gate.allows(&DesktopControl::AudioControl {
            enabled: false,
            generation: Some(1),
        }));
        assert!(gate.allows(&DesktopControl::Feedback {
            sequence: 1,
            fps: 1.0,
            decode_ms: 1.0,
        }));
        assert!(!gate.allows(&DesktopControl::PointerMove { x: 0.5, y: 0.5 }));
    }

    #[test]
    fn helper_arguments_include_negotiated_audio_codec() {
        let mut command = Command::new("om-agent");
        append_helper_args(
            &mut command,
            &DesktopOptions {
                pipe: r"\\.\pipe\desktop".to_string(),
                max_width: 1280,
                max_height: 720,
                min_fps: 6,
                max_fps: 8,
                jpeg_quality: 50,
                audio_codec: Some("opus".to_string()),
                system_helper: true,
                unattended: true,
                display_source: "virtual".to_string(),
            },
        );
        let args = command
            .get_args()
            .map(|value| value.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert!(
            args.windows(2)
                .any(|pair| pair == ["--audio-codec", "opus"])
        );
        assert!(args.iter().any(|value| value == "--system-helper"));
        assert!(args.iter().any(|value| value == "--unattended"));
    }

    #[test]
    fn secure_attention_is_always_rejected() {
        let mut input = InputState::default();
        let error = input.apply(DesktopControl::SecureAttention).unwrap_err();
        assert!(error.to_string().contains("secure_attention_unavailable"));
    }

    #[test]
    fn audio_control_is_never_injected_as_input() {
        let mut input = InputState::default();
        input
            .apply(DesktopControl::AudioControl {
                enabled: true,
                generation: Some(1),
            })
            .unwrap();
    }

    #[test]
    fn active_console_session_is_preferred() {
        assert_eq!(choose_active_session(2, &[1, 2], false, false).unwrap(), 2);
    }

    #[test]
    fn unique_active_rdp_session_is_selected_without_active_console() {
        assert_eq!(
            choose_active_session(u32::MAX, &[3], false, false).unwrap(),
            3
        );
    }

    #[test]
    fn ambiguous_or_missing_active_sessions_are_rejected() {
        assert_eq!(
            choose_active_session(u32::MAX, &[], false, false)
                .unwrap_err()
                .to_string(),
            "no_active_session"
        );
        assert_eq!(
            choose_active_session(u32::MAX, &[1, 2], false, false)
                .unwrap_err()
                .to_string(),
            "multiple_active_sessions"
        );
    }

    #[test]
    fn unattended_can_select_a_logged_out_console_with_system_winlogon() {
        assert_eq!(choose_active_session(7, &[], true, true).unwrap(), 7);
        assert!(choose_active_session(7, &[], true, false).is_err());
    }

    #[test]
    fn secure_attention_requires_matching_managed_unattended_policy() {
        let authorization = SessionAuthorization {
            session_id: 7,
            unattended: true,
            managed_service: true,
        };
        let mut gate = SecureAttentionGate::new(authorization);
        gate.observe_status(r#"{"type":"session_policy","access_mode":"unattended","local_consent_required":false,"secure_desktop_control":true,"secure_attention_allowed":true}"#)
            .unwrap();
        assert!(gate.allowed());
        gate.observe_status(r#"{"type":"desktop_state","desktop":"secure","context":"winlogon","controllable":true}"#)
            .unwrap();
        assert!(gate.allowed());
        gate.observe_status(r#"{"type":"desktop_state","desktop":"default","context":"default","controllable":true}"#)
            .unwrap();
        assert!(gate.allowed());
    }

    #[test]
    fn stale_generation_release_is_discarded_without_becoming_current_input() {
        let fence = InputGenerationFence::default();
        let first = fence.begin_transition("Default");
        assert!(!fence.ready_for_generation(first));
        assert!(fence.mark_ready(first));
        let mut input = InputState::new(true);
        input.bind_generation(first, "Default");
        input.keys.insert((0x41, false));

        let second = fence.begin_transition("Winlogon");
        assert!(!fence.ready_for_generation(second));
        input.discard_for_generation(second);
        assert!(input.keys.is_empty());
        assert_eq!(input.generation, second);
        assert!(input.generation_desktop.is_none());
        assert!(!fence.mark_ready(first));
        assert!(fence.mark_ready(second));
    }

    #[test]
    fn stale_release_clears_bookkeeping_without_input_injection() {
        let fence = InputGenerationFence::default();
        let first = fence.begin_transition("Default");
        assert!(fence.mark_ready(first));
        let mut input = InputState::new(true);
        input.bind_generation(first, "Default");
        input.keys.insert((0x41, false));
        input.buttons.insert(0);

        let second = fence.begin_transition("Winlogon");
        assert!(release_on_input_generation(&fence, &mut input, first).unwrap());
        assert!(input.keys.is_empty());
        assert!(input.buttons.is_empty());
        assert_eq!(input.generation, second);
        assert!(input.generation_desktop.is_none());
    }
}
