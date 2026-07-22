use std::{
    collections::HashSet,
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
    connect_async,
    tungstenite::{Message, client::IntoClientRequest, http::HeaderValue},
};
use uuid::Uuid;
use windows::{
    Win32::{
        Foundation::{
            CloseHandle, FreeLibrary, GENERIC_WRITE, HANDLE, HMODULE, HWND, LocalFree, STILL_ACTIVE,
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
                    DXGI_MODE_ROTATION, DXGI_MODE_ROTATION_ROTATE90, DXGI_MODE_ROTATION_ROTATE180,
                    DXGI_MODE_ROTATION_ROTATE270,
                },
                CreateDXGIFactory1, DXGI_ERROR_ACCESS_LOST, DXGI_ERROR_WAIT_TIMEOUT,
                DXGI_OUTDUPL_FRAME_INFO, DXGI_OUTPUT_DESC, IDXGIAdapter1, IDXGIFactory1,
                IDXGIOutput1, IDXGIOutputDuplication, IDXGIResource,
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
            DuplicateTokenEx, GetTokenInformation, PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES,
            SecurityImpersonation, TOKEN_ALL_ACCESS, TOKEN_DUPLICATE, TOKEN_QUERY, TOKEN_USER,
            TokenPrimary, TokenUser,
        },
        System::{
            Environment::{CreateEnvironmentBlock, DestroyEnvironmentBlock},
            LibraryLoader::{GetProcAddress, LoadLibraryW},
            Pipes::GetNamedPipeClientProcessId,
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
        UI::WindowsAndMessaging::{GetSystemMetrics, SM_CXSCREEN, SM_CYSCREEN},
    },
    core::{ComInterface, PCWSTR, PWSTR},
};

use super::{
    AdaptiveSettings, DATA_CHANNEL_JOIN_TIMEOUT, DesktopControl, DesktopOpenRequest,
    DesktopOptions, FrameHeader, MAX_CONTROL_BYTES, MAX_FRAME_BYTES, absolute_pointer_coordinate,
    dom_code_to_vk, dom_code_uses_extended_key, error_reason, scaled_dimensions,
};
use crate::{config::AgentConfig, models::AgentInbound};

const CREATE_NO_WINDOW_FLAG: u32 = 0x08000000;
const PIPE_FRAME: u8 = 1;
const PIPE_CONTROL: u8 = 2;
const PIPE_INTERNAL: u8 = 3;
const INTERNAL_STOP: &[u8] = b"stop";
const INTERNAL_STOPPED: &[u8] = b"stopped";
const INTERNAL_FATAL_PREFIX: &[u8] = b"fatal:";
const PIPE_MAX_PACKET: usize = MAX_FRAME_BYTES + 1024;
const SOCKET_SEND_TIMEOUT: Duration = Duration::from_secs(5);
const INPUT_ERROR_LOG_INTERVAL: Duration = Duration::from_secs(5);
const LOCAL_SYSTEM_SID: &str = "S-1-5-18";
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
    outbound: mpsc::UnboundedSender<AgentInbound>,
    close: oneshot::Receiver<String>,
) -> Result<String> {
    let established = tokio::time::timeout(
        DATA_CHANNEL_JOIN_TIMEOUT,
        establish_session(&config, &request),
    )
    .await
    .map_err(|_| anyhow!("data_channel_timeout"))??;
    let (socket, pipe, mut child) = established;
    let _ = outbound.send(AgentInbound::DesktopOpened {
        session_id: request.session_id.clone(),
    });

    let mut result = relay(socket, pipe, close).await;
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
)> {
    let (target, user_sid) = helper_target()?;
    let pipe_name = format!(r"\\.\pipe\omrd-{}", Uuid::new_v4());
    let pipe = create_private_pipe(&pipe_name, &user_sid)?;

    let min_fps = request.min_fps.clamp(1, 12);
    let options = DesktopOptions {
        pipe: pipe_name,
        max_width: request.max_width.clamp(320, 1920),
        max_height: request.max_height.clamp(240, 1080),
        min_fps,
        max_fps: request.max_fps.clamp(min_fps, 12),
        jpeg_quality: request.jpeg_quality.clamp(50, 75),
        system_helper: matches!(target, HelperTarget::ServiceSession { .. }),
    };
    let mut child = spawn_helper(&options, target, config)?;

    let mut ws_request = desktop_websocket_url(&config.server, &request.session_id)
        .into_client_request()
        .context("invalid desktop websocket URL")?;
    ws_request.headers_mut().insert(
        "authorization",
        HeaderValue::from_str(&format!("Bearer {}", request.stream_token))
            .context("invalid desktop stream token")?,
    );
    let connected = tokio::try_join!(
        async {
            connect_async(ws_request)
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
    Ok((socket, pipe, child))
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
        let _ = LocalFree(windows::Win32::Foundation::HLOCAL(descriptor.0));
        pipe
    }
}

fn validate_pipe_client(
    pipe: &tokio::net::windows::named_pipe::NamedPipeServer,
    expected_pid: u32,
    expected_session_id: u32,
) -> Result<()> {
    unsafe {
        let handle = HANDLE(pipe.as_raw_handle() as isize);
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
) -> Result<String> {
    let (mut ws_write, mut ws_read) = socket.split();
    let (pipe_read, mut pipe_write) = tokio::io::split(pipe);
    let (frame_tx, mut frame_rx) = watch::channel::<Option<Vec<u8>>>(None);
    let (status_tx, mut status_rx) = mpsc::channel::<String>(32);
    let (ack_tx, mut ack_rx) = mpsc::channel::<()>(1);
    let (fatal_tx, mut fatal_rx) = mpsc::channel::<String>(1);
    let reader = tokio::spawn(pipe_reader(
        pipe_read, frame_tx, status_tx, ack_tx, fatal_tx,
    ));
    let mut heartbeat = tokio::time::interval(Duration::from_secs(10));
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut last_browser_message = tokio::time::Instant::now();

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
            status = status_rx.recv() => {
                let Some(status) = status else { break "helper_disconnected".to_string() };
                tokio::time::timeout(
                    SOCKET_SEND_TIMEOUT,
                    ws_write.send(Message::Text(status.into())),
                )
                .await
                .context("desktop data websocket status send timed out")??;
            }
            incoming = ws_read.next() => {
                let Some(incoming) = incoming else { break "browser_disconnected".to_string() };
                last_browser_message = tokio::time::Instant::now();
                match incoming? {
                    Message::Text(text) => {
                        if text.len() > MAX_CONTROL_BYTES { bail!("desktop control message too large") }
                        serde_json::from_str::<DesktopControl>(&text)
                            .context("invalid desktop control message")?;
                        write_packet(&mut pipe_write, PIPE_CONTROL, text.as_bytes()).await?;
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
            }
        };
        Ok(reason)
    }
    .await;

    let close_reason = match &result {
        Ok(reason) => reason.clone(),
        Err(error) => error_reason(error),
    };
    let closed = serde_json::json!({"type":"closed", "reason":close_reason}).to_string();
    let _ = tokio::time::timeout(
        SOCKET_SEND_TIMEOUT,
        ws_write.send(Message::Text(closed.into())),
    )
    .await;

    // Keep the reader alive while the helper drains input state. This prevents a queued JPEG
    // packet from filling the pipe and blocking the helper before it can receive the stop packet.
    if write_packet(&mut pipe_write, PIPE_INTERNAL, INTERNAL_STOP)
        .await
        .is_ok()
    {
        match tokio::time::timeout(Duration::from_secs(2), ack_rx.recv()).await {
            Ok(_) => {}
            Err(_) => {
                // SendInput cannot release keys on Winlogon/UAC desktops. Keep this session's
                // ActivityGuard and helper alive until Default returns and the release ACK is
                // received, so an update cannot race a helper that still owns pressed input.
                crate::logging::info(format_args!(
                    "desktop helper input release is pending until the default desktop returns"
                ));
                let _ = ack_rx.recv().await;
            }
        }
    }
    reader.abort();
    result
}

async fn pipe_reader<R: AsyncRead + Unpin>(
    mut reader: R,
    frame_tx: watch::Sender<Option<Vec<u8>>>,
    status_tx: mpsc::Sender<String>,
    ack_tx: mpsc::Sender<()>,
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
                PIPE_CONTROL => {
                    let text = String::from_utf8(value).context("helper sent non-UTF8 control")?;
                    status_tx.send(text).await?;
                }
                PIPE_INTERNAL if value == INTERNAL_STOPPED => {
                    let _ = ack_tx.try_send(());
                }
                PIPE_INTERNAL if value.starts_with(INTERNAL_FATAL_PREFIX) => {
                    let reason = String::from_utf8(value[INTERNAL_FATAL_PREFIX.len()..].to_vec())
                        .context("helper sent non-UTF8 fatal reason")?;
                    let _ = fatal_tx.try_send(reason);
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

pub async fn run_helper(options: DesktopOptions) -> Result<()> {
    bind_interactive_window_station()?;
    log_helper_security_context(&options)?;
    let pipe = tokio::time::timeout(DATA_CHANNEL_JOIN_TIMEOUT, connect_pipe(&options.pipe))
        .await
        .map_err(|_| anyhow!("desktop helper pipe connection timeout"))??;
    let (mut read, mut write) = tokio::io::split(pipe);
    let (capture_tx, mut capture_rx) = mpsc::channel::<HelperEvent>(1);
    let settings = Arc::new(Mutex::new(AdaptiveSettings::initial(
        options.min_fps,
        options.max_fps,
        options.jpeg_quality,
    )));
    let capture_settings = settings.clone();
    let capture_options = options.clone();
    std::thread::Builder::new()
        .name("om-desktop-capture".to_string())
        .spawn(move || capture_loop(capture_options, capture_settings, capture_tx))?;

    let mut input = InputState {
        keys: HashSet::new(),
        buttons: HashSet::new(),
        allow_secure_attention: options.system_helper,
    };
    let mut input_desktop_available = options.system_helper || default_input_desktop();
    let mut pending_release = false;
    let mut stopping = false;
    let mut last_input_error_log = None;
    let mut suppressed_input_errors = 0_u64;
    let mut desktop_check = tokio::time::interval(Duration::from_millis(100));
    desktop_check.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let ready = serde_json::json!({"type":"ready"}).to_string();
    write_packet(&mut write, PIPE_CONTROL, ready.as_bytes()).await?;
    loop {
        tokio::select! {
            _ = desktop_check.tick() => {
                let available = options.system_helper || default_input_desktop();
                if input_desktop_available && !available {
                    pending_release = !release_on_input_desktop(&mut input);
                }
                input_desktop_available = available;
                if available && pending_release {
                    pending_release = !release_on_input_desktop(&mut input);
                }
                if stopping && !pending_release {
                    write_packet(&mut write, PIPE_INTERNAL, INTERNAL_STOPPED).await?;
                    break;
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
                    HelperEvent::Frame(frame) if !stopping => write_packet(&mut write, PIPE_FRAME, &frame).await?,
                    HelperEvent::Status(status) if !stopping => write_packet(&mut write, PIPE_CONTROL, status.as_bytes()).await?,
                    HelperEvent::Fatal(reason) => {
                        let mut fatal = INTERNAL_FATAL_PREFIX.to_vec();
                        fatal.extend_from_slice(reason.as_bytes());
                        write_packet(&mut write, PIPE_INTERNAL, &fatal).await?;
                        stopping = true;
                        pending_release = !release_on_input_desktop(&mut input);
                        if !pending_release {
                            write_packet(&mut write, PIPE_INTERNAL, INTERNAL_STOPPED).await?;
                            break;
                        }
                    }
                    _ => {}
                }
            }
            packet = read_packet(&mut read) => {
                let (kind, value) = packet?;
                if kind == PIPE_INTERNAL && value == INTERNAL_STOP {
                    stopping = true;
                    pending_release = !release_on_input_desktop(&mut input);
                    if !pending_release {
                        write_packet(&mut write, PIPE_INTERNAL, INTERNAL_STOPPED).await?;
                        break;
                    }
                    continue;
                }
                if kind != PIPE_CONTROL { bail!("unexpected service packet type") }
                let control: DesktopControl = serde_json::from_slice(&value)?;
                match control {
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
                        pending_release = !release_on_input_desktop(&mut input);
                    }
                    control if !stopping => {
                        let available = options.system_helper || default_input_desktop();
                        if input_desktop_available && !available {
                            pending_release = !release_on_input_desktop(&mut input);
                        }
                        input_desktop_available = available;
                        if available && pending_release {
                            pending_release = !release_on_input_desktop(&mut input);
                        }
                        if available && !pending_release {
                            let secure_attention = matches!(&control, DesktopControl::SecureAttention);
                            let releasing = matches!(
                                &control,
                                DesktopControl::Key { down: false, .. }
                                    | DesktopControl::PointerButton { down: false, .. }
                            );
                            if let Err(error) = apply_on_input_desktop(&mut input, control) {
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
        }
    }
    let _ = release_on_input_desktop(&mut input);
    Ok(())
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
    Frame(Vec<u8>),
    Status(String),
    Fatal(String),
}

fn capture_loop(
    options: DesktopOptions,
    settings: Arc<Mutex<AdaptiveSettings>>,
    tx: mpsc::Sender<HelperEvent>,
) {
    let mut capture: Option<DxgiCapture> = None;
    let mut attached_desktop: Option<(HDESK, String)> = None;
    let mut sequence = 0_u64;
    let mut foreground_secure_paused = false;
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
                    capture = None;
                    let kind = desktop_kind(&name);
                    let _ = tx.blocking_send(HelperEvent::Status(
                        serde_json::json!({"type":"desktop_state","desktop":kind}).to_string(),
                    ));
                }
                name
            }
            Err(error) => {
                crate::logging::error(format_args!("failed to attach input desktop: {error:#}"));
                capture = None;
                std::thread::sleep(Duration::from_millis(100));
                continue;
            }
        };
        if !options.system_helper && !desktop_name.eq_ignore_ascii_case("Default") {
            if !foreground_secure_paused {
                let _ = tx.blocking_send(HelperEvent::Status(
                    serde_json::json!({
                        "type":"paused",
                        "reason":"secure_desktop_requires_service"
                    })
                    .to_string(),
                ));
                foreground_secure_paused = true;
            }
            capture = None;
            continue;
        }
        if foreground_secure_paused {
            let _ = tx.blocking_send(HelperEvent::Status(
                serde_json::json!({"type":"ready"}).to_string(),
            ));
            foreground_secure_paused = false;
        }
        let result = if desktop_name.eq_ignore_ascii_case("Default") {
            if capture.is_none() {
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
                    let _ = tx.try_send(HelperEvent::Frame(frame));
                }
            }
            Ok(None) => {}
            Err(error) if error.to_string().contains("DXGI_ERROR_ACCESS_LOST") => capture = None,
            Err(error) if error.to_string().contains("frame_too_large") => {
                let _ = tx.blocking_send(HelperEvent::Fatal("frame_too_large".to_string()));
                return;
            }
            Err(error) => {
                crate::logging::error(format_args!("desktop capture failed: {error:#}"));
                capture = None;
            }
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

fn attach_input_desktop(current: &mut Option<(HDESK, String)>) -> Result<(String, bool)> {
    unsafe {
        let desktop = OpenInputDesktop(Default::default(), false, DESKTOP_READOBJECTS)
            .context("input desktop is unavailable")?;
        let name = desktop_name(desktop)?;
        if current.as_ref().is_some_and(|(_, value)| value == &name) {
            let _ = CloseDesktop(desktop);
            return Ok((name, false));
        }
        SetThreadDesktop(desktop).context("failed to bind capture thread to input desktop")?;
        if let Some((previous, _)) = current.replace((desktop, name.clone())) {
            let _ = CloseDesktop(previous);
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

fn apply_on_input_desktop(input: &mut InputState, control: DesktopControl) -> Result<()> {
    unsafe {
        let original = GetThreadDesktop(GetCurrentThreadId())?;
        let desktop = OpenInputDesktop(Default::default(), false, INPUT_DESKTOP_ACCESS)
            .context("input desktop is unavailable for input injection")?;
        let result = (|| -> Result<()> {
            let name = desktop_name(desktop)?;
            SetThreadDesktop(desktop).context("failed to bind input thread to input desktop")?;
            let applied = input
                .apply(control)
                .with_context(|| format!("input desktop {name}"));
            let restored = SetThreadDesktop(original)
                .context("failed to restore input thread desktop after injection");
            applied.and(restored)
        })();
        let _ = CloseDesktop(desktop);
        result
    }
}

fn release_on_input_desktop(input: &mut InputState) -> bool {
    apply_on_input_desktop(input, DesktopControl::ReleaseAll).is_ok()
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
            let mut description = DXGI_OUTPUT_DESC::default();
            unsafe { output.GetDesc(&mut description)? };
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
                HMODULE(0),
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
                    if jpeg.len() + 32 <= MAX_FRAME_BYTES || quality <= 50 {
                        if jpeg.len() + 32 > MAX_FRAME_BYTES {
                            bail!("frame_too_large")
                        }
                        return Ok(Some((jpeg, width, height)));
                    }
                    quality = quality.saturating_sub(5).max(50);
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
        let screen = GetDC(HWND(0));
        if screen.0 == 0 {
            bail!("failed to acquire desktop DC")
        }
        let memory = CreateCompatibleDC(screen);
        let bitmap = CreateCompatibleBitmap(screen, width, height);
        if memory.0 == 0 || bitmap.0 == 0 {
            if bitmap.0 != 0 {
                let _ = DeleteObject(bitmap);
            }
            if memory.0 != 0 {
                let _ = DeleteDC(memory);
            }
            let _ = ReleaseDC(HWND(0), screen);
            bail!("failed to create GDI desktop capture objects")
        }
        let previous = SelectObject(memory, bitmap);
        let copied = BitBlt(memory, 0, 0, width, height, screen, 0, 0, SRCCOPY).is_ok();
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
        let _ = DeleteObject(bitmap);
        let _ = DeleteDC(memory);
        let _ = ReleaseDC(HWND(0), screen);
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
            if jpeg.len() + 32 <= MAX_FRAME_BYTES || quality <= 50 {
                if jpeg.len() + 32 > MAX_FRAME_BYTES {
                    bail!("frame_too_large")
                }
                return Ok(Some((jpeg, target_width, target_height)));
            }
            quality = quality.saturating_sub(5).max(50);
        }
    }
}

#[derive(Default)]
struct InputState {
    keys: HashSet<(u16, bool)>,
    buttons: HashSet<u8>,
    allow_secure_attention: bool,
}

impl InputState {
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
                if !self.allow_secure_attention {
                    bail!("secure_attention_unavailable")
                }
                send_secure_attention()
            }
            DesktopControl::Feedback { .. } => Ok(()),
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

fn send_secure_attention() -> Result<()> {
    unsafe {
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

impl Drop for InputState {
    fn drop(&mut self) {
        let _ = self.release_all();
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
                    let handle = HANDLE(child.as_raw_handle() as isize);
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
        .arg(options.jpeg_quality.to_string());
    if options.system_helper {
        command.arg("--system-helper");
    }
}

fn current_session_id() -> Result<u32> {
    let mut id = 0_u32;
    unsafe {
        ProcessIdToSessionId(GetCurrentProcessId(), &mut id)?;
    }
    Ok(id)
}

fn helper_target() -> Result<(HelperTarget, String)> {
    let current_session_id = current_session_id()?;
    if current_session_id == 0 {
        let session_id = select_active_session()?;
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
        let _ = LocalFree(windows::Win32::Foundation::HLOCAL(string_sid.0.cast()));
        Ok(sid)
    }
}

fn select_active_session() -> Result<u32> {
    unsafe {
        let console = WTSGetActiveConsoleSessionId();
        let mut sessions: *mut WTS_SESSION_INFOW = null_mut();
        let mut count = 0_u32;
        WTSEnumerateSessionsW(WTS_CURRENT_SERVER_HANDLE, 0, 1, &mut sessions, &mut count)
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
        choose_active_session(console, &active)
    }
}

fn choose_active_session(console: u32, active: &[u32]) -> Result<u32> {
    if active.contains(&console) {
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
        WTSEnumerateProcessesW(WTS_CURRENT_SERVER_HANDLE, 0, 1, &mut processes, &mut count)
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
        crate::logging::info(format_args!(
            "launching remote desktop helper with target session LocalSystem token: session_id={session_id}"
        ));

        let executable = std::env::current_exe()?;
        let mut command = Command::new(&executable);
        append_helper_args(&mut command, options);
        config.append_cli_args(&mut command);
        let mut command_line = wide(&format!(
            "\"{}\" {}",
            executable.display(),
            command
                .get_args()
                .map(|v| quote_arg(v))
                .collect::<Vec<_>>()
                .join(" ")
        ));
        let application = wide(executable.as_os_str());
        let desktop = wide("winsta0\\default");
        let mut environment: *mut c_void = null_mut();
        CreateEnvironmentBlock(&mut environment, primary_token, false)?;
        let startup = STARTUPINFOW {
            cb: size_of::<STARTUPINFOW>() as u32,
            lpDesktop: PWSTR(desktop.as_ptr() as *mut _),
            ..zeroed()
        };
        let mut process: PROCESS_INFORMATION = zeroed();
        let created = CreateProcessAsUserW(
            primary_token,
            PCWSTR(application.as_ptr()),
            PWSTR(command_line.as_mut_ptr()),
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
        let _ = CloseHandle(primary_token);
        created?;
        let _ = CloseHandle(process.hThread);
        Ok(HelperProcess::Handle {
            handle: process.hProcess,
            pid: process.dwProcessId,
        })
    }
}

fn quote_arg(value: &OsStr) -> String {
    format!("\"{}\"", value.to_string_lossy().replace('"', "\\\""))
}
fn wide(value: impl AsRef<OsStr>) -> Vec<u16> {
    value.as_ref().encode_wide().chain(Some(0)).collect()
}

fn desktop_websocket_url(server: &str, session_id: &str) -> String {
    let trimmed = server.trim_end_matches('/');
    let base = if let Some(rest) = trimmed.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = trimmed.strip_prefix("http://") {
        format!("ws://{rest}")
    } else {
        format!("ws://{trimmed}")
    };
    format!("{base}/api/agent/desktop/ws?session_id={session_id}")
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
    fn active_console_session_is_preferred() {
        assert_eq!(choose_active_session(2, &[1, 2]).unwrap(), 2);
    }

    #[test]
    fn unique_active_rdp_session_is_selected_without_active_console() {
        assert_eq!(choose_active_session(u32::MAX, &[3]).unwrap(), 3);
    }

    #[test]
    fn ambiguous_or_missing_active_sessions_are_rejected() {
        assert_eq!(
            choose_active_session(u32::MAX, &[])
                .unwrap_err()
                .to_string(),
            "no_active_session"
        );
        assert_eq!(
            choose_active_session(u32::MAX, &[1, 2])
                .unwrap_err()
                .to_string(),
            "multiple_active_sessions"
        );
    }
}
