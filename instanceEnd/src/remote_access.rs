use crate::{
    config::{AgentConfig, RemoteDesktopConsent, WindowsVirtualDevices},
    models::{RemoteAccessMode, RemoteAccessStatus},
};
#[cfg(windows)]
use crate::{
    models::{
        RemoteAccessAvailability, RemoteAccessComponent, RemoteAccessDriverState,
        RemoteAccessFallbackMode, RemoteAccessSource,
    },
    time::now_ts,
};

pub const STATUS_CAPABILITY: &str = "remote_access_status_v1";
pub const UNATTENDED_CAPABILITY: &str = "remote_desktop_unattended_v1";

pub fn initialize_service_devices(config: &AgentConfig) {
    #[cfg(windows)]
    {
        if managed_local_system_service() {
            let access_mode = if config.remote_desktop_consent == RemoteDesktopConsent::Unattended {
                RemoteAccessMode::Unattended
            } else {
                RemoteAccessMode::LocalConsent
            };
            let _ = start_windows_device_coordinator(access_mode, config.windows_virtual_devices);
        }
    }
    #[cfg(not(windows))]
    let _ = config;
}

#[cfg(windows)]
static WINDOWS_DEVICE_COORDINATOR_WAKE: std::sync::OnceLock<
    std::sync::mpsc::Sender<DeviceCoordinatorWake>,
> = std::sync::OnceLock::new();
#[cfg(windows)]
static WINDOWS_DEVICE_COORDINATOR_STATUS: std::sync::OnceLock<
    std::sync::Arc<std::sync::Mutex<Option<RemoteAccessStatus>>>,
> = std::sync::OnceLock::new();

#[cfg(windows)]
struct DeviceCoordinatorWake(std::sync::mpsc::Sender<()>);

#[cfg_attr(not(windows), allow(dead_code))]
pub struct RemoteAccessManager {
    access_mode: RemoteAccessMode,
    virtual_devices: WindowsVirtualDevices,
    #[cfg(windows)]
    coordinator_status: std::sync::Arc<std::sync::Mutex<Option<RemoteAccessStatus>>>,
}

impl RemoteAccessManager {
    pub fn new(config: &AgentConfig) -> Self {
        let unattended_requested =
            config.remote_desktop_consent == RemoteDesktopConsent::Unattended;
        #[cfg(windows)]
        let unattended_enabled = unattended_requested && managed_local_system_service();
        #[cfg(not(windows))]
        let unattended_enabled = {
            let _ = unattended_requested;
            false
        };
        #[cfg(windows)]
        let coordinator_status = if managed_local_system_service() {
            start_windows_device_coordinator(
                if unattended_enabled {
                    RemoteAccessMode::Unattended
                } else {
                    RemoteAccessMode::LocalConsent
                },
                config.windows_virtual_devices,
            )
        } else {
            std::sync::Arc::new(std::sync::Mutex::new(Some(current_session_windows_status(
                RemoteAccessMode::LocalConsent,
                config.windows_virtual_devices,
            ))))
        };
        Self {
            access_mode: if unattended_enabled {
                RemoteAccessMode::Unattended
            } else {
                RemoteAccessMode::LocalConsent
            },
            virtual_devices: config.windows_virtual_devices,
            #[cfg(windows)]
            coordinator_status,
        }
    }

    pub fn status(&self) -> Option<RemoteAccessStatus> {
        #[cfg(windows)]
        {
            self.coordinator_status
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clone()
                .or_else(|| {
                    Some(unknown_windows_status(
                        self.access_mode,
                        self.virtual_devices,
                    ))
                })
        }
        #[cfg(not(windows))]
        {
            None
        }
    }
}

#[cfg(windows)]
fn managed_local_system_service() -> bool {
    use std::mem::size_of;
    use windows::Win32::{
        Foundation::{CloseHandle, HANDLE},
        Security::{
            GetTokenInformation, IsWellKnownSid, TOKEN_QUERY, TOKEN_USER, TokenUser,
            WinLocalSystemSid,
        },
        System::{
            RemoteDesktop::ProcessIdToSessionId,
            Threading::{GetCurrentProcess, GetCurrentProcessId, OpenProcessToken},
        },
    };

    unsafe {
        let mut session_id = u32::MAX;
        if ProcessIdToSessionId(GetCurrentProcessId(), &mut session_id).is_err() || session_id != 0
        {
            return false;
        }
        let mut token = HANDLE::default();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).is_err() {
            return false;
        }
        let result = (|| {
            let mut needed = 0_u32;
            let _ = GetTokenInformation(token, TokenUser, None, 0, &mut needed);
            if needed < size_of::<TOKEN_USER>() as u32 {
                return false;
            }
            let mut buffer = vec![0_u8; needed as usize];
            if GetTokenInformation(
                token,
                TokenUser,
                Some(buffer.as_mut_ptr().cast()),
                needed,
                &mut needed,
            )
            .is_err()
            {
                return false;
            }
            let user = &*(buffer.as_ptr().cast::<TOKEN_USER>());
            IsWellKnownSid(user.User.Sid, WinLocalSystemSid).as_bool()
        })();
        let _ = CloseHandle(token);
        result
    }
}

#[cfg(any(windows, test))]
pub fn stable_code(value: &str) -> Option<String> {
    let value = value.trim();
    (!value.is_empty()
        && value.len() <= 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_'))
    .then(|| value.to_string())
}

#[cfg(windows)]
fn start_windows_device_coordinator(
    access_mode: RemoteAccessMode,
    virtual_devices: WindowsVirtualDevices,
) -> std::sync::Arc<std::sync::Mutex<Option<RemoteAccessStatus>>> {
    use std::sync::{Arc, Mutex};

    WINDOWS_DEVICE_COORDINATOR_STATUS
        .get_or_init(|| {
            let status = Arc::new(Mutex::new(Some(unknown_windows_status(
                access_mode,
                virtual_devices,
            ))));
            let worker_status = status.clone();
            let (wake_tx, wake_rx) = std::sync::mpsc::channel();
            let _ = WINDOWS_DEVICE_COORDINATOR_WAKE.set(wake_tx);
            if let Err(error) = std::thread::Builder::new()
                .name("om-windows-device-coordinator".to_string())
                .spawn(move || {
                    run_windows_device_coordinator(
                        access_mode,
                        virtual_devices,
                        worker_status,
                        wake_rx,
                    )
                })
            {
                crate::logging::error(format_args!(
                    "failed to start Windows virtual device coordinator: {error}"
                ));
            }
            status
        })
        .clone()
}

#[cfg(windows)]
pub async fn ensure_ready_for_remote_desktop() -> bool {
    if !managed_local_system_service() {
        return true;
    }
    let Some(wake) = WINDOWS_DEVICE_COORDINATOR_WAKE.get().cloned() else {
        return false;
    };
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    if wake.send(DeviceCoordinatorWake(ready_tx)).is_err() {
        return false;
    }
    tokio::task::spawn_blocking(move || {
        ready_rx
            .recv_timeout(std::time::Duration::from_secs(15))
            .is_ok()
    })
    .await
    .unwrap_or(false)
}

#[cfg(windows)]
pub fn display_readiness_error(config: &AgentConfig) -> Option<&'static str> {
    if !managed_local_system_service() {
        return match display_device_presence() {
            Ok(DevicePresence { any: true, .. }) => None,
            Err(_) => Some("device_probe_failed"),
            Ok(_) if config.windows_virtual_devices == WindowsVirtualDevices::Disabled => {
                Some("virtual_devices_disabled")
            }
            Ok(_) if !crate::windows_driver_assets::BUNDLED => Some("driver_bundle_missing"),
            Ok(_) if installed_driver_reboot_required() => Some("virtual_device_reboot_required"),
            Ok(_) => Some("virtual_device_not_ready"),
        };
    }
    let status = WINDOWS_DEVICE_COORDINATOR_STATUS.get().and_then(|status| {
        status
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    });
    if status.as_ref().is_some_and(|status| {
        status.display.availability == RemoteAccessAvailability::Ready
            || status.display.availability == RemoteAccessAvailability::Degraded
    }) {
        return None;
    }
    if status
        .as_ref()
        .is_none_or(|status| status.display.availability == RemoteAccessAvailability::Unknown)
    {
        return Some("device_probe_failed");
    }
    if let Some(code) = status
        .as_ref()
        .filter(|status| status.display.availability == RemoteAccessAvailability::Unavailable)
        .and_then(|status| status.display.code.as_deref())
    {
        return Some(match code {
            "virtual_device_create_failed" => "virtual_device_create_failed",
            "virtual_device_reboot_required" => "virtual_device_reboot_required",
            "virtual_devices_disabled" => "virtual_devices_disabled",
            "driver_bundle_missing" => "driver_bundle_missing",
            _ => "virtual_device_not_ready",
        });
    }
    if config.windows_virtual_devices == WindowsVirtualDevices::Disabled {
        return Some("virtual_devices_disabled");
    }
    if !crate::windows_driver_assets::BUNDLED {
        return Some("driver_bundle_missing");
    }
    Some(if installed_driver_reboot_required() {
        "virtual_device_reboot_required"
    } else {
        "virtual_device_not_ready"
    })
}

#[cfg(any(windows, test))]
#[derive(Debug, Clone, Copy, Default, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DevicePresence {
    pub(crate) physical: bool,
    pub(crate) any: bool,
}

#[cfg(any(windows, test))]
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DeviceProbeSnapshot {
    pub(crate) schema_version: u8,
    pub(crate) session_id: u32,
    pub(crate) display: Option<DevicePresence>,
    pub(crate) audio: Option<DevicePresence>,
}

#[cfg(any(windows, test))]
impl DeviceProbeSnapshot {
    pub(crate) fn validate(&self, expected_session_id: u32) -> anyhow::Result<()> {
        use anyhow::bail;

        if self.schema_version != 1 || self.session_id != expected_session_id {
            bail!("device_probe_failed")
        }
        if [self.display, self.audio]
            .into_iter()
            .flatten()
            .any(|presence| presence.physical && !presence.any)
        {
            bail!("device_probe_failed")
        }
        Ok(())
    }
}

#[cfg(windows)]
pub(crate) fn probe_current_session_devices(
    session_id: u32,
) -> anyhow::Result<DeviceProbeSnapshot> {
    use anyhow::bail;

    if session_id == 0 {
        bail!("device_probe_failed")
    }
    Ok(DeviceProbeSnapshot {
        schema_version: 1,
        session_id,
        display: display_device_presence().ok(),
        audio: audio_device_presence().ok(),
    })
}

#[cfg(any(windows, test))]
#[derive(Debug, Clone, PartialEq, Eq)]
struct ProductDriverDevice {
    instance_id: String,
    oem_inf: String,
    hardware_ids: Vec<String>,
    driver_version: String,
    present: Option<bool>,
    status: Option<String>,
    problem_code: Option<u32>,
}

#[cfg(any(windows, test))]
#[derive(Debug, Clone, PartialEq, Eq)]
struct ProductDriverPackage {
    kind: String,
    oem_inf: String,
    original_inf: String,
    provider: String,
    hardware_ids: Vec<String>,
    driver_version: String,
    devices: Vec<ProductDriverDevice>,
}

#[cfg(any(windows, test))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DriverRebootValidation {
    Healthy,
    Pending,
    Failed,
}

#[cfg(windows)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DriverRemovalOutcome {
    Complete,
    RebootRequired,
}

#[cfg(any(windows, test))]
struct PresenceHysteresis {
    initialized: bool,
    missing_since: Option<std::time::Instant>,
    present_since: Option<std::time::Instant>,
}

#[cfg(any(windows, test))]
impl PresenceHysteresis {
    fn new() -> Self {
        Self {
            initialized: false,
            missing_since: None,
            present_since: None,
        }
    }

    fn wants_lease(&mut self, now: std::time::Instant, physical: bool, leased: bool) -> bool {
        if physical {
            self.missing_since = None;
            let recovered = self.present_since.get_or_insert(now);
            self.initialized = true;
            leased && now.duration_since(*recovered) < std::time::Duration::from_secs(10)
        } else {
            self.present_since = None;
            if !self.initialized {
                self.initialized = true;
                return true;
            }
            let missing = self.missing_since.get_or_insert(now);
            leased || now.duration_since(*missing) >= std::time::Duration::from_secs(3)
        }
    }
}

#[cfg(windows)]
struct SoftwareDeviceLease(windows::Win32::Devices::Enumeration::Pnp::HSWDEVICE);

#[cfg(windows)]
impl Drop for SoftwareDeviceLease {
    fn drop(&mut self) {
        unsafe {
            windows::Win32::Devices::Enumeration::Pnp::SwDeviceClose(self.0);
        }
    }
}

#[cfg(windows)]
fn run_windows_device_coordinator(
    access_mode: RemoteAccessMode,
    virtual_devices: WindowsVirtualDevices,
    status: std::sync::Arc<std::sync::Mutex<Option<RemoteAccessStatus>>>,
    wake: std::sync::mpsc::Receiver<DeviceCoordinatorWake>,
) {
    use std::time::{Duration, Instant};

    let can_manage = managed_local_system_service()
        && virtual_devices == WindowsVirtualDevices::Auto
        && crate::windows_driver_assets::BUNDLED;
    let mut display_lease = None;
    let mut audio_lease = None;
    let mut display_hysteresis = PresenceHysteresis::new();
    let mut audio_hysteresis = PresenceHysteresis::new();
    let mut display_retry_at = Instant::now();
    let mut audio_retry_at = Instant::now();
    let mut display_code = None;
    let mut audio_code = None;
    let mut ready_waiters: Vec<std::sync::mpsc::Sender<()>> = Vec::new();

    loop {
        let now = Instant::now();
        let probe = crate::remote_desktop::windows::probe_target_session_devices(
            access_mode == RemoteAccessMode::Unattended,
        )
        .ok();
        let display = probe.as_ref().and_then(|probe| probe.display);
        let audio = probe.as_ref().and_then(|probe| probe.audio);

        if can_manage {
            if let Some(presence) = display {
                let wanted =
                    display_hysteresis.wants_lease(now, presence.physical, display_lease.is_some());
                if !wanted {
                    display_lease = None;
                    display_code = None;
                } else if display_lease.is_none() && now >= display_retry_at {
                    match create_software_device(
                        "OmVirtualDisplay",
                        "ROOT\\OMVIRTUALDISPLAY",
                        "Operation Monitoring Virtual Display",
                    ) {
                        Ok(lease) => {
                            display_lease = Some(lease);
                            display_code = Some("virtual_device_starting".to_string());
                        }
                        Err(_) => {
                            display_code = Some("virtual_device_create_failed".to_string());
                            display_retry_at = now + Duration::from_secs(30);
                        }
                    }
                }
            }
            if let Some(presence) = audio {
                let wanted =
                    audio_hysteresis.wants_lease(now, presence.physical, audio_lease.is_some());
                if !wanted {
                    audio_lease = None;
                    audio_code = None;
                } else if audio_lease.is_none() && now >= audio_retry_at {
                    match create_software_device(
                        "OmVirtualAudio",
                        "ROOT\\OMVIRTUALAUDIO",
                        "Operation Monitoring Virtual Audio",
                    ) {
                        Ok(lease) => {
                            audio_lease = Some(lease);
                            audio_code = Some("virtual_device_starting".to_string());
                        }
                        Err(_) => {
                            audio_code = Some("virtual_device_create_failed".to_string());
                            audio_retry_at = now + Duration::from_secs(30);
                        }
                    }
                }
            }
        }

        if display.is_some_and(|presence| presence.any) && display_lease.is_some() {
            display_code = None;
        }
        if audio.is_some_and(|presence| presence.any) && audio_lease.is_some() {
            audio_code = None;
        }
        let reboot_required = installed_driver_reboot_required();
        let fallback_mode = if virtual_devices == WindowsVirtualDevices::Disabled {
            RemoteAccessFallbackMode::Disabled
        } else if crate::windows_driver_assets::BUNDLED {
            RemoteAccessFallbackMode::Auto
        } else {
            RemoteAccessFallbackMode::PhysicalOnly
        };
        let next = RemoteAccessStatus {
            access_mode,
            fallback_mode,
            display: coordinated_component_status(
                "display",
                display,
                display_lease.is_some(),
                display_code.as_deref(),
                reboot_required,
                fallback_mode,
            ),
            audio: coordinated_component_status(
                "audio",
                audio,
                audio_lease.is_some(),
                audio_code.as_deref(),
                reboot_required,
                fallback_mode,
            ),
            reboot_required,
            checked_at: now_ts(),
        };
        *status
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(next);
        for waiter in ready_waiters.drain(..) {
            let _ = waiter.send(());
        }
        let check_interval = if display_code.as_deref() == Some("virtual_device_starting")
            || audio_code.as_deref() == Some("virtual_device_starting")
        {
            Duration::from_secs(1)
        } else {
            Duration::from_secs(30)
        };
        if let Ok(DeviceCoordinatorWake(waiter)) = wake.recv_timeout(check_interval) {
            ready_waiters.push(waiter);
            ready_waiters.extend(wake.try_iter().map(|wake| wake.0));
        }
    }
}

#[cfg(windows)]
fn coordinated_component_status(
    kind: &str,
    presence: Option<DevicePresence>,
    leased: bool,
    code: Option<&str>,
    reboot_required: bool,
    fallback_mode: RemoteAccessFallbackMode,
) -> RemoteAccessComponent {
    let driver_version = installed_driver_component_version(kind);
    match presence {
        Some(DevicePresence { physical: true, .. }) => RemoteAccessComponent {
            availability: RemoteAccessAvailability::Ready,
            source: RemoteAccessSource::Physical,
            driver_state: match fallback_mode {
                RemoteAccessFallbackMode::Auto => RemoteAccessDriverState::Standby,
                RemoteAccessFallbackMode::Disabled => RemoteAccessDriverState::Missing,
                RemoteAccessFallbackMode::PhysicalOnly => RemoteAccessDriverState::Unsupported,
            },
            driver_version,
            code: None,
        },
        Some(DevicePresence { any: true, .. }) if leased => RemoteAccessComponent {
            availability: RemoteAccessAvailability::Ready,
            source: RemoteAccessSource::Virtual,
            driver_state: RemoteAccessDriverState::Active,
            driver_version,
            code: None,
        },
        Some(_) if reboot_required => RemoteAccessComponent {
            availability: RemoteAccessAvailability::Unavailable,
            source: RemoteAccessSource::None,
            driver_state: RemoteAccessDriverState::RebootRequired,
            driver_version,
            code: stable_code("virtual_device_reboot_required"),
        },
        Some(_) if leased => RemoteAccessComponent {
            availability: RemoteAccessAvailability::Degraded,
            source: RemoteAccessSource::None,
            driver_state: RemoteAccessDriverState::Active,
            driver_version,
            code: code.and_then(stable_code),
        },
        Some(_) => RemoteAccessComponent {
            availability: RemoteAccessAvailability::Unavailable,
            source: RemoteAccessSource::None,
            driver_state: match fallback_mode {
                RemoteAccessFallbackMode::Auto => RemoteAccessDriverState::Standby,
                RemoteAccessFallbackMode::Disabled => RemoteAccessDriverState::Missing,
                RemoteAccessFallbackMode::PhysicalOnly => RemoteAccessDriverState::Unsupported,
            },
            driver_version,
            code: code.and_then(stable_code).or_else(|| {
                stable_code(match fallback_mode {
                    RemoteAccessFallbackMode::Auto => "virtual_device_not_ready",
                    RemoteAccessFallbackMode::Disabled => "virtual_devices_disabled",
                    RemoteAccessFallbackMode::PhysicalOnly => "driver_bundle_missing",
                })
            }),
        },
        None => RemoteAccessComponent {
            availability: RemoteAccessAvailability::Unknown,
            source: RemoteAccessSource::Unknown,
            driver_state: RemoteAccessDriverState::Unknown,
            driver_version,
            code: stable_code("device_probe_failed"),
        },
    }
}

#[cfg(windows)]
fn create_software_device(
    instance_id: &str,
    hardware_id: &str,
    description: &str,
) -> anyhow::Result<SoftwareDeviceLease> {
    use std::{ffi::c_void, mem::size_of, sync::mpsc, time::Duration};

    use anyhow::{Context, bail};
    use windows::{
        Win32::Devices::Enumeration::Pnp::{
            HSWDEVICE, SW_DEVICE_CREATE_INFO, SWDeviceCapabilitiesDriverRequired,
            SWDeviceCapabilitiesRemovable, SWDeviceCapabilitiesSilentInstall, SwDeviceClose,
            SwDeviceCreate,
        },
        core::{HRESULT, PCWSTR},
    };

    unsafe extern "system" fn created(
        _device: HSWDEVICE,
        result: HRESULT,
        context: *const c_void,
        _device_instance_id: PCWSTR,
    ) {
        let sender = unsafe { Box::from_raw(context as *mut mpsc::Sender<HRESULT>) };
        let _ = sender.send(result);
    }

    let enumerator = "OperationMonitoring\0".encode_utf16().collect::<Vec<_>>();
    let parent = "HTREE\\ROOT\\0\0".encode_utf16().collect::<Vec<_>>();
    let instance_id = instance_id
        .encode_utf16()
        .chain(Some(0))
        .collect::<Vec<_>>();
    let hardware_ids = hardware_id.encode_utf16().chain([0, 0]).collect::<Vec<_>>();
    let description = description
        .encode_utf16()
        .chain(Some(0))
        .collect::<Vec<_>>();
    let create = SW_DEVICE_CREATE_INFO {
        cbSize: size_of::<SW_DEVICE_CREATE_INFO>() as u32,
        pszInstanceId: PCWSTR(instance_id.as_ptr()),
        pszzHardwareIds: PCWSTR(hardware_ids.as_ptr()),
        CapabilityFlags: (SWDeviceCapabilitiesRemovable.0
            | SWDeviceCapabilitiesSilentInstall.0
            | SWDeviceCapabilitiesDriverRequired.0) as u32,
        pszDeviceDescription: PCWSTR(description.as_ptr()),
        ..Default::default()
    };
    let (result_tx, result_rx) = mpsc::channel::<HRESULT>();
    let callback_context = Box::into_raw(Box::new(result_tx)).cast::<c_void>();
    let device = match unsafe {
        SwDeviceCreate(
            PCWSTR(enumerator.as_ptr()),
            PCWSTR(parent.as_ptr()),
            &create,
            None,
            Some(created),
            Some(callback_context),
        )
    } {
        Ok(device) => device,
        Err(error) => {
            unsafe {
                drop(Box::from_raw(
                    callback_context as *mut mpsc::Sender<HRESULT>,
                ));
            }
            return Err(error).context("virtual_device_create_failed");
        }
    };
    let created = match result_rx.recv_timeout(Duration::from_secs(15)) {
        Ok(result) => result,
        Err(error) => {
            unsafe { SwDeviceClose(device) };
            return Err(error).context("virtual_device_create_timeout");
        }
    };
    if created.is_err() {
        unsafe { SwDeviceClose(device) };
        bail!("virtual_device_create_failed")
    }
    Ok(SoftwareDeviceLease(device))
}

#[cfg(windows)]
fn display_device_presence() -> anyhow::Result<DevicePresence> {
    use std::mem::size_of;

    use anyhow::{Context, bail};
    use windows::Win32::Devices::Display::{
        DISPLAYCONFIG_ADAPTER_NAME, DISPLAYCONFIG_DEVICE_INFO_GET_ADAPTER_NAME,
        DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME, DISPLAYCONFIG_DEVICE_INFO_HEADER,
        DISPLAYCONFIG_MODE_INFO, DISPLAYCONFIG_PATH_INFO, DISPLAYCONFIG_TARGET_DEVICE_NAME,
        DisplayConfigGetDeviceInfo, GetDisplayConfigBufferSizes, QDC_ONLY_ACTIVE_PATHS,
        QueryDisplayConfig,
    };

    for _ in 0..3 {
        let mut path_count = 0_u32;
        let mut mode_count = 0_u32;
        unsafe {
            GetDisplayConfigBufferSizes(QDC_ONLY_ACTIVE_PATHS, &mut path_count, &mut mode_count)
        }
        .ok()
        .context("display_probe_size_failed")?;
        if path_count == 0 {
            return Ok(DevicePresence::default());
        }
        let mut paths = vec![DISPLAYCONFIG_PATH_INFO::default(); path_count as usize];
        let mut modes = vec![DISPLAYCONFIG_MODE_INFO::default(); mode_count as usize];
        let result = unsafe {
            QueryDisplayConfig(
                QDC_ONLY_ACTIVE_PATHS,
                &mut path_count,
                paths.as_mut_ptr(),
                &mut mode_count,
                if modes.is_empty() {
                    std::ptr::null_mut()
                } else {
                    modes.as_mut_ptr()
                },
                None,
            )
        };
        if result.is_err() {
            continue;
        }
        paths.truncate(path_count as usize);
        let mut presence = DevicePresence::default();
        for path in paths {
            let mut target = DISPLAYCONFIG_TARGET_DEVICE_NAME {
                header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
                    r#type: DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME,
                    size: size_of::<DISPLAYCONFIG_TARGET_DEVICE_NAME>() as u32,
                    adapterId: path.targetInfo.adapterId,
                    id: path.targetInfo.id,
                },
                ..Default::default()
            };
            if unsafe { DisplayConfigGetDeviceInfo(&mut target.header) } != 0 {
                bail!("display_probe_target_failed")
            }
            let mut adapter = DISPLAYCONFIG_ADAPTER_NAME {
                header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
                    r#type: DISPLAYCONFIG_DEVICE_INFO_GET_ADAPTER_NAME,
                    size: size_of::<DISPLAYCONFIG_ADAPTER_NAME>() as u32,
                    adapterId: path.targetInfo.adapterId,
                    id: 0,
                },
                ..Default::default()
            };
            if unsafe { DisplayConfigGetDeviceInfo(&mut adapter.header) } != 0 {
                bail!("display_probe_adapter_failed")
            }
            presence.any = true;
            let path = utf16_array(&target.monitorDevicePath);
            let name = utf16_array(&target.monitorFriendlyDeviceName);
            let adapter_path = utf16_array(&adapter.adapterDevicePath);
            let product_owned = adapter_path
                .to_ascii_uppercase()
                .contains("OMVIRTUALDISPLAY")
                || path.to_ascii_uppercase().contains("OMVIRTUALDISPLAY")
                || name.eq_ignore_ascii_case("Operation Monitoring Virtual Display");
            presence.physical |= !product_owned;
        }
        return Ok(presence);
    }
    bail!("display_probe_changed")
}

#[cfg(windows)]
fn utf16_array(value: &[u16]) -> String {
    let length = value
        .iter()
        .position(|character| *character == 0)
        .unwrap_or(value.len());
    String::from_utf16_lossy(&value[..length])
}

#[cfg(any(windows, test))]
fn is_product_audio_endpoint(instance_id: &str, friendly_name: &str) -> bool {
    instance_id.eq_ignore_ascii_case("SWD\\OperationMonitoring\\OmVirtualAudio")
        || friendly_name
            .to_ascii_lowercase()
            .starts_with("operation monitoring virtual audio")
}

#[cfg(windows)]
struct ComUninitializeGuard(bool);

#[cfg(windows)]
impl Drop for ComUninitializeGuard {
    fn drop(&mut self) {
        if self.0 {
            unsafe {
                windows::Win32::System::Com::CoUninitialize();
            }
        }
    }
}

#[cfg(windows)]
fn audio_device_presence() -> anyhow::Result<DevicePresence> {
    use anyhow::Context;
    use windows::{
        Win32::{
            Foundation::PROPERTYKEY,
            Media::Audio::{DEVICE_STATE_ACTIVE, IMMDeviceEnumerator, MMDeviceEnumerator, eRender},
            System::Com::{
                CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, STGM_READ,
                StructuredStorage::{PropVariantClear, PropVariantToString},
            },
        },
        core::{GUID, HRESULT},
    };

    const RPC_E_CHANGED_MODE: HRESULT = HRESULT(0x80010106_u32 as i32);
    const FRIENDLY_NAME: PROPERTYKEY = PROPERTYKEY {
        fmtid: GUID::from_u128(0xa45c254e_df1c_4efd_8020_67d146a850e0),
        pid: 14,
    };
    const DEVICE_INSTANCE_ID: PROPERTYKEY = PROPERTYKEY {
        fmtid: GUID::from_u128(0x78c34fc8_104a_4aca_9ea4_524d52996e57),
        pid: 256,
    };
    unsafe {
        let initialized = match CoInitializeEx(None, COINIT_MULTITHREADED).ok() {
            Ok(()) => true,
            Err(error) if error.code() == RPC_E_CHANGED_MODE => false,
            Err(error) => return Err(error).context("audio_probe_com_failed"),
        };
        let _com = ComUninitializeGuard(initialized);
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
                .context("audio_probe_enumerator_failed")?;
        let collection = enumerator
            .EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE)
            .context("audio_probe_enumeration_failed")?;
        let mut presence = DevicePresence::default();
        for index in 0..collection.GetCount()? {
            let device = collection.Item(index)?;
            presence.any = true;
            let store = device.OpenPropertyStore(STGM_READ)?;
            let read_string = |key: &PROPERTYKEY| -> windows::core::Result<String> {
                let mut value = store.GetValue(key)?;
                let mut text = [0_u16; 512];
                let result = PropVariantToString(&value, &mut text);
                let _ = PropVariantClear(&mut value);
                result?;
                Ok(utf16_array(&text))
            };
            let instance_id = read_string(&DEVICE_INSTANCE_ID).unwrap_or_default();
            let friendly_name = read_string(&FRIENDLY_NAME).unwrap_or_default();
            presence.physical |= !is_product_audio_endpoint(&instance_id, &friendly_name);
        }
        Ok(presence)
    }
}

#[cfg(windows)]
fn installed_driver_reboot_required() -> bool {
    let Some(program_data) = std::env::var_os("ProgramData") else {
        return false;
    };
    let data = std::path::PathBuf::from(program_data).join("OperationMonitoring");
    let state_requires_reboot = std::fs::read(data.join("driver-state.json"))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
        .and_then(|state| {
            state
                .get("reboot_required")
                .and_then(serde_json::Value::as_bool)
        })
        .unwrap_or(false);
    state_requires_reboot
        || std::fs::read(data.join("driver-stage-journal.json"))
            .ok()
            .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
            .is_some_and(|journal| {
                journal.get("provider").and_then(serde_json::Value::as_str)
                    == Some("Operation Monitoring")
                    && journal.get("phase").and_then(serde_json::Value::as_str)
                        == Some("rollback_reboot_pending")
            })
}

#[cfg(windows)]
fn installed_driver_component_version(kind: &str) -> Option<String> {
    let program_data = std::env::var_os("ProgramData")?;
    let bytes = std::fs::read(
        std::path::PathBuf::from(program_data).join("OperationMonitoring/driver-state.json"),
    )
    .ok()?;
    let state = serde_json::from_slice::<serde_json::Value>(&bytes).ok()?;
    let version = state
        .get("packages")?
        .as_array()?
        .iter()
        .find(|package| package.get("kind").and_then(serde_json::Value::as_str) == Some(kind))?
        .get("driver_version")?
        .as_str()?;
    parse_windows_driver_version(version).map(|_| version.to_string())
}

#[cfg(windows)]
fn unknown_windows_status(
    access_mode: RemoteAccessMode,
    virtual_devices: WindowsVirtualDevices,
) -> RemoteAccessStatus {
    let bundle_supported = crate::windows_driver_assets::BUNDLED;
    let fallback_mode = match (virtual_devices, bundle_supported) {
        (WindowsVirtualDevices::Disabled, _) => RemoteAccessFallbackMode::Disabled,
        (WindowsVirtualDevices::Auto, true) => RemoteAccessFallbackMode::Auto,
        (WindowsVirtualDevices::Auto, false) => RemoteAccessFallbackMode::PhysicalOnly,
    };

    RemoteAccessStatus {
        access_mode,
        fallback_mode,
        display: coordinated_component_status(
            "display",
            None,
            false,
            Some("device_probe_failed"),
            installed_driver_reboot_required(),
            fallback_mode,
        ),
        audio: coordinated_component_status(
            "audio",
            None,
            false,
            Some("device_probe_failed"),
            installed_driver_reboot_required(),
            fallback_mode,
        ),
        reboot_required: installed_driver_reboot_required(),
        checked_at: now_ts(),
    }
}

#[cfg(windows)]
fn current_session_windows_status(
    access_mode: RemoteAccessMode,
    virtual_devices: WindowsVirtualDevices,
) -> RemoteAccessStatus {
    let fallback_mode = match (virtual_devices, crate::windows_driver_assets::BUNDLED) {
        (WindowsVirtualDevices::Disabled, _) => RemoteAccessFallbackMode::Disabled,
        (WindowsVirtualDevices::Auto, true) => RemoteAccessFallbackMode::Auto,
        (WindowsVirtualDevices::Auto, false) => RemoteAccessFallbackMode::PhysicalOnly,
    };
    let reboot_required = installed_driver_reboot_required();
    RemoteAccessStatus {
        access_mode,
        fallback_mode,
        display: coordinated_component_status(
            "display",
            display_device_presence().ok(),
            false,
            None,
            reboot_required,
            fallback_mode,
        ),
        audio: coordinated_component_status(
            "audio",
            audio_device_presence().ok(),
            false,
            None,
            reboot_required,
            fallback_mode,
        ),
        reboot_required,
        checked_at: now_ts(),
    }
}

#[cfg(windows)]
pub fn stage_bundled_windows_drivers(program_data: &std::path::Path) -> anyhow::Result<bool> {
    use std::{
        fs,
        path::{Component, Path},
        process::Command,
    };

    use anyhow::{Context, bail};
    use sha2::{Digest, Sha256};

    use crate::windows_driver_assets as assets;

    if !assets::BUNDLED {
        return Ok(false);
    }
    let version = assets::BUNDLE_VERSION.context("driver_bundle_metadata_missing")?;
    let architecture = assets::ARCHITECTURE.context("driver_bundle_metadata_missing")?;
    let expected_lock_hash = assets::LOCK_SHA256.context("driver_bundle_metadata_missing")?;
    let actual_lock_hash = crate::hex::encode_lower(Sha256::digest(assets::LOCK_BYTES));
    if actual_lock_hash != expected_lock_hash {
        bail!("driver_bundle_hash_mismatch")
    }

    let state_path = program_data.join("driver-state.json");
    let journal_path = program_data.join("driver-stage-journal.json");
    if recover_incomplete_driver_stage(&journal_path)? {
        return Ok(true);
    }
    let existing_state = fs::read(&state_path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok());
    let existing_state = reconcile_driver_reboot_state(&state_path, existing_state)?;
    if existing_state.as_ref().is_some_and(|state| {
        state
            .get("reboot_required")
            .and_then(serde_json::Value::as_bool)
            == Some(true)
    }) {
        return Ok(true);
    }

    let root = program_data
        .join("drivers")
        .join(version)
        .join(architecture);
    fs::create_dir_all(&root).context("driver_stage_directory_failed")?;
    crate::windows_security::restrict_to_system_and_administrators(&root)
        .context("driver_stage_acl_failed")?;

    let lock_path = root.join("bundle-lock.json");
    write_verified_asset(&lock_path, expected_lock_hash, assets::LOCK_BYTES)?;
    for file in assets::FILES {
        let relative = Path::new(file.relative_path);
        if relative.is_absolute()
            || relative
                .components()
                .any(|component| !matches!(component, Component::Normal(_)))
        {
            bail!("driver_bundle_path_invalid")
        }
        write_verified_asset(&root.join(relative), file.sha256, file.bytes)?;
    }

    let installed_inventory = query_product_driver_inventory()?;
    let mut packages_to_install = Vec::new();
    let mut package_states = Vec::new();
    for package in assets::PACKAGES {
        let existing_package = existing_state.as_ref().and_then(|state| {
            state
                .get("packages")
                .and_then(serde_json::Value::as_array)
                .and_then(|packages| {
                    packages.iter().find(|existing| {
                        existing.get("kind").and_then(serde_json::Value::as_str)
                            == Some(package.kind)
                            && existing
                                .get("hardware_id")
                                .and_then(serde_json::Value::as_str)
                                .is_some_and(|hardware_id| {
                                    hardware_id.eq_ignore_ascii_case(package.hardware_id)
                                })
                    })
                })
        });
        let installed_version = newest_inventory_driver_version(&installed_inventory, package.kind);
        let install = should_install_driver_version(installed_version, package.driver_version)?;
        if install {
            packages_to_install.push(package.kind);
        }
        package_states.push(serde_json::json!({
            "kind": package.kind,
            "driver_version": if install {
                package.driver_version
            } else {
                installed_version.unwrap_or(package.driver_version)
            },
            "hardware_id": package.hardware_id,
            "catalog_path": if install {
                package.catalog_path
            } else {
                existing_package
                    .and_then(|package| package.get("catalog_path"))
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or(package.catalog_path)
            },
        }));
    }
    if packages_to_install.is_empty() {
        return Ok(existing_state
            .as_ref()
            .and_then(|state| state.get("reboot_required"))
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false));
    }

    let preexisting_oem_infs = inventory_oem_infs(&installed_inventory);
    write_driver_json(
        &journal_path,
        &serde_json::json!({
            "schema_version": 1,
            "phase": "installing",
            "provider": "Operation Monitoring",
            "bundle_version": version,
            "preexisting_oem_infs": preexisting_oem_infs,
        }),
    )?;

    let mut reboot_required = false;
    let install_result = (|| -> anyhow::Result<()> {
        for package_kind in &packages_to_install {
            let infs = assets::FILES
                .iter()
                .filter(|file| file.package == *package_kind && file.kind == "inf")
                .collect::<Vec<_>>();
            if infs.len() != 1 {
                bail!("driver_bundle_inf_count_invalid")
            }
            let file = infs[0];
            let inf = root.join(file.relative_path);
            let status = Command::new("pnputil.exe")
                .args(["/add-driver", inf.to_string_lossy().as_ref(), "/install"])
                .status()
                .context("driver_stage_command_failed")?;
            match status.code() {
                Some(0) => {}
                Some(1641 | 3010) => reboot_required = true,
                _ => bail!("driver_stage_failed"),
            }
        }
        Ok(())
    })();
    if let Err(error) = install_result {
        match rollback_new_product_oem_infs(&preexisting_oem_infs)
            .context("driver_stage_rollback_failed")?
        {
            DriverRemovalOutcome::Complete => {
                let _ = fs::remove_file(&journal_path);
            }
            DriverRemovalOutcome::RebootRequired => {
                write_rollback_reboot_journal(&journal_path, version, &preexisting_oem_infs)?;
            }
        }
        return Err(error);
    }
    let oem_infs = inventory_oem_infs(&query_product_driver_inventory()?);
    let previous_state = existing_state.as_ref().map(|state| {
        let mut state = state.clone();
        if let Some(object) = state.as_object_mut() {
            object.insert(
                "reboot_required".to_string(),
                serde_json::Value::Bool(false),
            );
            object.remove("reboot_boot_id");
            object.remove("reboot_phase");
            object.remove("rollback");
            object.remove("validation_error");
        }
        state
    });

    let state = serde_json::json!({
        "schema_version": 1,
        "provider": "Operation Monitoring",
        "bundle_version": version,
        "architecture": architecture,
        "lock_sha256": expected_lock_hash,
        "packages": package_states,
        "oem_infs": oem_infs,
        "reboot_required": reboot_required,
        "reboot_boot_id": if reboot_required {
            Some(query_windows_boot_id()?)
        } else {
            None
        },
        "reboot_phase": if reboot_required { Some("install") } else { None },
        "rollback": reboot_required.then(|| serde_json::json!({
            "preexisting_oem_infs": preexisting_oem_infs,
            "previous_state": previous_state,
        })),
    });
    write_driver_json(
        &journal_path,
        &serde_json::json!({
            "schema_version": 1,
            "phase": "committing",
            "provider": "Operation Monitoring",
            "bundle_version": version,
            "preexisting_oem_infs": preexisting_oem_infs,
            "target_state": &state,
        }),
    )?;
    write_driver_json(&state_path, &state)?;
    fs::remove_file(&journal_path).context("driver_stage_journal_cleanup_failed")?;
    Ok(reboot_required)
}

#[cfg(any(windows, test))]
fn should_install_driver_version(existing: Option<&str>, candidate: &str) -> anyhow::Result<bool> {
    use anyhow::{Context, bail};

    let candidate =
        parse_windows_driver_version(candidate).context("driver_bundle_version_invalid")?;
    let Some(existing) = existing else {
        return Ok(true);
    };
    let Some(existing) = parse_windows_driver_version(existing) else {
        bail!("driver_state_version_invalid")
    };
    Ok(candidate > existing)
}

#[cfg(any(windows, test))]
fn parse_windows_driver_version(value: &str) -> Option<[u16; 4]> {
    let values = value
        .split('.')
        .map(str::parse::<u16>)
        .collect::<Result<Vec<_>, _>>()
        .ok()?;
    (values.len() == 4)
        .then(|| values.try_into().ok())
        .flatten()
}

#[cfg(any(windows, test))]
fn product_driver_identity(kind: &str) -> Option<(&'static str, &'static str, &'static str)> {
    match kind {
        "display" => Some((
            "OmVirtualDisplay.inf",
            "ROOT\\OMVIRTUALDISPLAY",
            "SWD\\OperationMonitoring\\OmVirtualDisplay",
        )),
        "audio" => Some((
            "OmVirtualAudio.inf",
            "ROOT\\OMVIRTUALAUDIO",
            "SWD\\OperationMonitoring\\OmVirtualAudio",
        )),
        _ => None,
    }
}

#[cfg(any(windows, test))]
fn product_driver_kind(original_inf: &str) -> Option<&'static str> {
    let name = original_inf.rsplit(['\\', '/']).next()?;
    if name.eq_ignore_ascii_case("OmVirtualDisplay.inf") {
        Some("display")
    } else if name.eq_ignore_ascii_case("OmVirtualAudio.inf") {
        Some("audio")
    } else {
        None
    }
}

#[cfg(any(windows, test))]
fn json_string_values(value: Option<&serde_json::Value>) -> Option<Vec<String>> {
    match value? {
        serde_json::Value::String(value) => Some(vec![value.clone()]),
        serde_json::Value::Array(values) => values
            .iter()
            .map(|value| value.as_str().map(str::to_string))
            .collect(),
        serde_json::Value::Null => Some(Vec::new()),
        _ => None,
    }
}

#[cfg(any(windows, test))]
fn json_object_values(value: Option<&serde_json::Value>) -> Option<Vec<&serde_json::Value>> {
    match value? {
        value @ serde_json::Value::Object(_) => Some(vec![value]),
        serde_json::Value::Array(values) => values
            .iter()
            .all(serde_json::Value::is_object)
            .then(|| values.iter().collect()),
        serde_json::Value::Null => Some(Vec::new()),
        _ => None,
    }
}

#[cfg(any(windows, test))]
fn parse_product_driver_inventory(text: &str) -> anyhow::Result<Vec<ProductDriverPackage>> {
    use anyhow::{Context, bail};
    use std::collections::HashSet;

    if text.trim().is_empty() {
        return Ok(Vec::new());
    }
    let value: serde_json::Value =
        serde_json::from_str(text.trim()).context("driver_inventory_invalid")?;
    let entries = match &value {
        serde_json::Value::Array(entries) => entries.iter().collect::<Vec<_>>(),
        entry @ serde_json::Value::Object(_) => vec![entry],
        _ => bail!("driver_inventory_invalid"),
    };
    let mut seen = HashSet::new();
    let mut inventory = Vec::with_capacity(entries.len());
    for entry in entries {
        let provider = entry
            .get("ProviderName")
            .and_then(serde_json::Value::as_str)
            .context("driver_inventory_invalid")?;
        let original_inf = entry
            .get("OriginalFileName")
            .and_then(serde_json::Value::as_str)
            .context("driver_inventory_invalid")?;
        let Some(kind) = product_driver_kind(original_inf) else {
            bail!("driver_inventory_ownership_mismatch")
        };
        let (_, expected_hardware_id, _) =
            product_driver_identity(kind).context("driver_inventory_invalid")?;
        let oem_inf = entry
            .get("Driver")
            .and_then(serde_json::Value::as_str)
            .filter(|value| valid_oem_inf(value))
            .context("driver_inventory_invalid")?
            .to_ascii_lowercase();
        let driver_version = entry
            .get("DriverVersion")
            .and_then(serde_json::Value::as_str)
            .filter(|value| parse_windows_driver_version(value).is_some())
            .context("driver_inventory_invalid")?;
        let hardware_ids = json_string_values(entry.get("HardwareIds"))
            .filter(|values| {
                !values.is_empty()
                    && values
                        .iter()
                        .all(|value| value.eq_ignore_ascii_case(expected_hardware_id))
            })
            .context("driver_inventory_ownership_mismatch")?;
        if provider != "Operation Monitoring" || !seen.insert(oem_inf.clone()) {
            bail!("driver_inventory_ownership_mismatch")
        }

        let mut devices = Vec::new();
        for device in
            json_object_values(entry.get("Devices")).context("driver_inventory_invalid")?
        {
            let device_oem_inf = device
                .get("InfName")
                .and_then(serde_json::Value::as_str)
                .filter(|value| value.eq_ignore_ascii_case(&oem_inf))
                .context("driver_inventory_ownership_mismatch")?
                .to_ascii_lowercase();
            let device_hardware_ids = json_string_values(device.get("HardwareIds"))
                .filter(|values| {
                    !values.is_empty()
                        && values
                            .iter()
                            .all(|value| value.eq_ignore_ascii_case(expected_hardware_id))
                })
                .context("driver_inventory_ownership_mismatch")?;
            let device_version = device
                .get("DriverVersion")
                .and_then(serde_json::Value::as_str)
                .filter(|value| parse_windows_driver_version(value).is_some())
                .context("driver_inventory_invalid")?;
            devices.push(ProductDriverDevice {
                instance_id: device
                    .get("InstanceId")
                    .and_then(serde_json::Value::as_str)
                    .context("driver_inventory_invalid")?
                    .to_string(),
                oem_inf: device_oem_inf,
                hardware_ids: device_hardware_ids,
                driver_version: device_version.to_string(),
                present: device.get("Present").and_then(serde_json::Value::as_bool),
                status: device
                    .get("Status")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string),
                problem_code: device
                    .get("ProblemCode")
                    .and_then(serde_json::Value::as_u64)
                    .and_then(|value| u32::try_from(value).ok()),
            });
        }
        inventory.push(ProductDriverPackage {
            kind: kind.to_string(),
            oem_inf,
            original_inf: original_inf.to_string(),
            provider: provider.to_string(),
            hardware_ids,
            driver_version: driver_version.to_string(),
            devices,
        });
    }
    inventory.sort_by(|left, right| left.oem_inf.cmp(&right.oem_inf));
    Ok(inventory)
}

#[cfg(windows)]
fn inventory_oem_infs(inventory: &[ProductDriverPackage]) -> Vec<String> {
    inventory
        .iter()
        .map(|package| package.oem_inf.clone())
        .collect()
}

#[cfg(windows)]
fn newest_inventory_driver_version<'a>(
    inventory: &'a [ProductDriverPackage],
    kind: &str,
) -> Option<&'a str> {
    inventory
        .iter()
        .filter(|package| package.kind == kind)
        .max_by_key(|package| parse_windows_driver_version(&package.driver_version))
        .map(|package| package.driver_version.as_str())
}

#[cfg(any(windows, test))]
fn validate_rebooted_product_drivers(
    packages: &[serde_json::Value],
    inventory: &[ProductDriverPackage],
    recorded_oem_infs: &std::collections::HashSet<String>,
) -> DriverRebootValidation {
    let mut pending = false;
    for package in packages {
        let Some(kind) = package.get("kind").and_then(serde_json::Value::as_str) else {
            return DriverRebootValidation::Failed;
        };
        let Some((_, expected_hardware_id, expected_instance_id)) = product_driver_identity(kind)
        else {
            return DriverRebootValidation::Failed;
        };
        let Some(expected_version) = package
            .get("driver_version")
            .and_then(serde_json::Value::as_str)
            .filter(|value| parse_windows_driver_version(value).is_some())
        else {
            return DriverRebootValidation::Failed;
        };
        if !inventory.iter().any(|installed| {
            installed.kind == kind
                && recorded_oem_infs.contains(&installed.oem_inf)
                && parse_windows_driver_version(&installed.driver_version)
                    >= parse_windows_driver_version(expected_version)
                && installed
                    .hardware_ids
                    .iter()
                    .all(|value| value.eq_ignore_ascii_case(expected_hardware_id))
        }) {
            return DriverRebootValidation::Failed;
        }

        let present_devices = inventory
            .iter()
            .filter(|installed| installed.kind == kind)
            .flat_map(|installed| installed.devices.iter())
            .filter(|device| device.present != Some(false))
            .collect::<Vec<_>>();
        if present_devices.is_empty() {
            pending = true;
            continue;
        }
        for device in present_devices {
            if !device
                .instance_id
                .eq_ignore_ascii_case(expected_instance_id)
                || !device
                    .hardware_ids
                    .iter()
                    .all(|value| value.eq_ignore_ascii_case(expected_hardware_id))
                || parse_windows_driver_version(&device.driver_version)
                    < parse_windows_driver_version(expected_version)
            {
                return DriverRebootValidation::Failed;
            }
            if !recorded_oem_infs.contains(&device.oem_inf) {
                pending = true;
                continue;
            }
            match (
                device.present,
                device.status.as_deref(),
                device.problem_code,
            ) {
                (Some(true), Some(status), Some(0)) if status.eq_ignore_ascii_case("OK") => {}
                (Some(true), Some(_), Some(_)) => return DriverRebootValidation::Failed,
                _ => pending = true,
            }
        }
    }
    if pending {
        DriverRebootValidation::Pending
    } else {
        DriverRebootValidation::Healthy
    }
}

#[cfg(windows)]
fn probe_rebooted_product_drivers(
    packages: &[serde_json::Value],
    recorded_oem_infs: &std::collections::HashSet<String>,
) -> anyhow::Result<(DriverRebootValidation, Vec<ProductDriverPackage>)> {
    use std::{thread, time::Duration};

    let mut inventory = query_product_driver_inventory()?;
    let mut validation = validate_rebooted_product_drivers(packages, &inventory, recorded_oem_infs);
    if validation != DriverRebootValidation::Pending {
        return Ok((validation, inventory));
    }

    let display_lease = create_software_device(
        "OmVirtualDisplay",
        "ROOT\\OMVIRTUALDISPLAY",
        "Operation Monitoring Virtual Display",
    )
    .ok();
    let audio_lease = create_software_device(
        "OmVirtualAudio",
        "ROOT\\OMVIRTUALAUDIO",
        "Operation Monitoring Virtual Audio",
    )
    .ok();
    if display_lease.is_none() && audio_lease.is_none() {
        return Ok((validation, inventory));
    }

    for _ in 0..10 {
        thread::sleep(Duration::from_secs(1));
        inventory = query_product_driver_inventory()?;
        validation = validate_rebooted_product_drivers(packages, &inventory, recorded_oem_infs);
        if validation != DriverRebootValidation::Pending {
            break;
        }
    }
    drop((display_lease, audio_lease));
    Ok((validation, inventory))
}

#[cfg(any(windows, test))]
fn package_allowed_for_uninstall(
    installed: &ProductDriverPackage,
    state_packages: &[serde_json::Value],
) -> bool {
    let Some((expected_inf, expected_hardware_id, expected_instance_id)) =
        product_driver_identity(&installed.kind)
    else {
        return false;
    };
    let Some(state_package) = state_packages.iter().find(|package| {
        package.get("kind").and_then(serde_json::Value::as_str) == Some(installed.kind.as_str())
            && package
                .get("hardware_id")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|value| value.eq_ignore_ascii_case(expected_hardware_id))
    }) else {
        return false;
    };
    let Some(state_version) = state_package
        .get("driver_version")
        .and_then(serde_json::Value::as_str)
        .and_then(parse_windows_driver_version)
    else {
        return false;
    };
    let Some(installed_version) = parse_windows_driver_version(&installed.driver_version) else {
        return false;
    };
    installed.provider == "Operation Monitoring"
        && installed
            .original_inf
            .rsplit(['\\', '/'])
            .next()
            .is_some_and(|name| name.eq_ignore_ascii_case(expected_inf))
        && installed_version <= state_version
        && !installed.hardware_ids.is_empty()
        && installed
            .hardware_ids
            .iter()
            .all(|value| value.eq_ignore_ascii_case(expected_hardware_id))
        && installed.devices.iter().all(|device| {
            device
                .instance_id
                .eq_ignore_ascii_case(expected_instance_id)
                && device.oem_inf.eq_ignore_ascii_case(&installed.oem_inf)
                && device.driver_version == installed.driver_version
                && !device.hardware_ids.is_empty()
                && device
                    .hardware_ids
                    .iter()
                    .all(|value| value.eq_ignore_ascii_case(expected_hardware_id))
        })
}

#[cfg(windows)]
fn write_driver_json(path: &std::path::Path, value: &serde_json::Value) -> anyhow::Result<()> {
    use sha2::{Digest, Sha256};

    let bytes = serde_json::to_vec_pretty(value)?;
    let hash = crate::hex::encode_lower(Sha256::digest(&bytes));
    write_verified_asset(path, &hash, &bytes)
}

#[cfg(windows)]
fn query_windows_boot_id() -> anyhow::Result<String> {
    use anyhow::{Context, bail};
    use std::process::Command;

    let script = "$ErrorActionPreference='Stop'; [Console]::OutputEncoding=[Text.UTF8Encoding]::new($false); $boot=(Get-CimInstance -ClassName Win32_OperatingSystem -ErrorAction Stop).LastBootUpTime.ToUniversalTime().ToFileTimeUtc(); [Console]::Out.Write($boot.ToString([Globalization.CultureInfo]::InvariantCulture))";
    let output = Command::new("powershell.exe")
        .args([
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            script,
        ])
        .output()
        .context("driver_boot_id_failed")?;
    if !output.status.success() {
        bail!("driver_boot_id_failed")
    }
    let value = String::from_utf8(output.stdout).context("driver_boot_id_invalid")?;
    let value = value.trim();
    if value.is_empty() || value.len() > 32 || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        bail!("driver_boot_id_invalid")
    }
    Ok(value.to_string())
}

#[cfg(windows)]
fn reconcile_driver_reboot_state(
    state_path: &std::path::Path,
    state: Option<serde_json::Value>,
) -> anyhow::Result<Option<serde_json::Value>> {
    use anyhow::{Context, bail};
    use std::collections::HashSet;

    let Some(mut state) = state else {
        return Ok(None);
    };
    if state
        .get("reboot_required")
        .and_then(serde_json::Value::as_bool)
        != Some(true)
    {
        return Ok(Some(state));
    }
    let current_boot_id = query_windows_boot_id()?;
    let Some(staged_boot_id) = state
        .get("reboot_boot_id")
        .and_then(serde_json::Value::as_str)
    else {
        state
            .as_object_mut()
            .context("driver_state_invalid")?
            .insert(
                "reboot_boot_id".to_string(),
                serde_json::Value::String(current_boot_id),
            );
        write_driver_json(state_path, &state)?;
        return Ok(Some(state));
    };
    if staged_boot_id == current_boot_id {
        return Ok(Some(state));
    }

    let packages = state
        .get("packages")
        .and_then(serde_json::Value::as_array)
        .filter(|packages| product_driver_packages_owned(packages))
        .context("driver_state_invalid")?;
    let recorded = state
        .get("oem_infs")
        .and_then(serde_json::Value::as_array)
        .context("driver_state_invalid")?
        .iter()
        .map(|value| {
            value
                .as_str()
                .filter(|value| valid_oem_inf(value))
                .map(str::to_ascii_lowercase)
                .context("driver_state_invalid")
        })
        .collect::<anyhow::Result<HashSet<_>>>()?;
    if recorded.is_empty() {
        bail!("driver_state_invalid")
    }
    let rollback = state
        .get("rollback")
        .and_then(serde_json::Value::as_object)
        .context("driver_reboot_validation_failed")?;
    let preexisting = rollback
        .get("preexisting_oem_infs")
        .and_then(serde_json::Value::as_array)
        .context("driver_reboot_validation_failed")?
        .iter()
        .map(|value| {
            value
                .as_str()
                .filter(|value| valid_oem_inf(value))
                .map(str::to_ascii_lowercase)
                .context("driver_reboot_validation_failed")
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    let inventory = query_product_driver_inventory()?;
    let current = inventory_oem_infs(&inventory)
        .into_iter()
        .collect::<HashSet<_>>();
    let rollback_phase = state
        .get("reboot_phase")
        .and_then(serde_json::Value::as_str)
        == Some("rollback");
    if rollback_phase {
        if current.is_subset(&preexisting.iter().cloned().collect()) {
            return finish_driver_rollback(state_path, state, &inventory);
        }
        return continue_driver_rollback(state_path, state, &preexisting, &current_boot_id);
    }

    let (validation, inventory) = if recorded.is_subset(&current) {
        probe_rebooted_product_drivers(packages, &recorded)?
    } else {
        (DriverRebootValidation::Failed, inventory)
    };
    match validation {
        DriverRebootValidation::Healthy => {
            reconcile_state_package_versions(&mut state, &inventory)?;
            let object = state.as_object_mut().context("driver_state_invalid")?;
            object.insert(
                "reboot_required".to_string(),
                serde_json::Value::Bool(false),
            );
            object.remove("reboot_boot_id");
            object.remove("reboot_phase");
            object.remove("rollback");
            object.remove("validation_error");
            write_driver_json(state_path, &state)?;
            Ok(Some(state))
        }
        DriverRebootValidation::Pending => {
            state
                .as_object_mut()
                .context("driver_state_invalid")?
                .insert(
                    "validation_error".to_string(),
                    serde_json::Value::String("driver_reboot_validation_pending".to_string()),
                );
            write_driver_json(state_path, &state)?;
            Ok(Some(state))
        }
        DriverRebootValidation::Failed => {
            continue_driver_rollback(state_path, state, &preexisting, &current_boot_id)
        }
    }
}

#[cfg(windows)]
fn reconcile_state_package_versions(
    state: &mut serde_json::Value,
    inventory: &[ProductDriverPackage],
) -> anyhow::Result<()> {
    use anyhow::Context;

    let packages = state
        .get_mut("packages")
        .and_then(serde_json::Value::as_array_mut)
        .context("driver_state_invalid")?;
    for package in packages {
        let kind = package
            .get("kind")
            .and_then(serde_json::Value::as_str)
            .context("driver_state_invalid")?;
        let version = newest_inventory_driver_version(inventory, kind)
            .context("driver_reboot_validation_failed")?;
        package
            .as_object_mut()
            .context("driver_state_invalid")?
            .insert(
                "driver_version".to_string(),
                serde_json::Value::String(version.to_string()),
            );
    }
    Ok(())
}

#[cfg(windows)]
fn continue_driver_rollback(
    state_path: &std::path::Path,
    mut state: serde_json::Value,
    preexisting: &[String],
    current_boot_id: &str,
) -> anyhow::Result<Option<serde_json::Value>> {
    use anyhow::Context;

    match rollback_new_product_oem_infs(preexisting).context("driver_reboot_rollback_failed")? {
        DriverRemovalOutcome::Complete => {
            let inventory = query_product_driver_inventory()?;
            let allowed = preexisting
                .iter()
                .cloned()
                .collect::<std::collections::HashSet<_>>();
            if !inventory_oem_infs(&inventory)
                .into_iter()
                .all(|oem_inf| allowed.contains(&oem_inf))
            {
                anyhow::bail!("driver_reboot_rollback_failed")
            }
            finish_driver_rollback(state_path, state, &inventory)
        }
        DriverRemovalOutcome::RebootRequired => {
            let object = state.as_object_mut().context("driver_state_invalid")?;
            object.insert("reboot_required".to_string(), serde_json::Value::Bool(true));
            object.insert(
                "reboot_boot_id".to_string(),
                serde_json::Value::String(current_boot_id.to_string()),
            );
            object.insert(
                "reboot_phase".to_string(),
                serde_json::Value::String("rollback".to_string()),
            );
            object.insert(
                "validation_error".to_string(),
                serde_json::Value::String("driver_reboot_rollback_pending".to_string()),
            );
            write_driver_json(state_path, &state)?;
            Ok(Some(state))
        }
    }
}

#[cfg(windows)]
fn finish_driver_rollback(
    state_path: &std::path::Path,
    mut state: serde_json::Value,
    inventory: &[ProductDriverPackage],
) -> anyhow::Result<Option<serde_json::Value>> {
    use anyhow::{Context, bail};

    let rollback = state
        .get("rollback")
        .and_then(serde_json::Value::as_object)
        .context("driver_reboot_validation_failed")?;
    if let Some(previous) = rollback
        .get("previous_state")
        .filter(|value| !value.is_null())
    {
        let previous = previous.clone();
        if previous.get("provider").and_then(serde_json::Value::as_str)
            != Some("Operation Monitoring")
            || !previous
                .get("packages")
                .and_then(serde_json::Value::as_array)
                .is_some_and(|packages| product_driver_packages_owned(packages))
        {
            bail!("driver_reboot_rollback_state_invalid")
        }
        write_driver_json(state_path, &previous)?;
        return Ok(Some(previous));
    }
    let object = state.as_object_mut().context("driver_state_invalid")?;
    object.insert(
        "reboot_required".to_string(),
        serde_json::Value::Bool(false),
    );
    object.insert(
        "validation_error".to_string(),
        serde_json::Value::String("driver_reboot_validation_failed".to_string()),
    );
    object.insert(
        "oem_infs".to_string(),
        serde_json::to_value(inventory_oem_infs(inventory))?,
    );
    object.remove("reboot_boot_id");
    object.remove("reboot_phase");
    object.remove("rollback");
    write_driver_json(state_path, &state)?;
    Ok(Some(state))
}

#[cfg(windows)]
fn recover_incomplete_driver_stage(journal_path: &std::path::Path) -> anyhow::Result<bool> {
    use anyhow::{Context, bail};

    let Ok(bytes) = std::fs::read(journal_path) else {
        return Ok(false);
    };
    let journal: serde_json::Value =
        serde_json::from_slice(&bytes).context("driver_stage_journal_invalid")?;
    if journal
        .get("schema_version")
        .and_then(serde_json::Value::as_u64)
        != Some(1)
        || journal.get("provider").and_then(serde_json::Value::as_str)
            != Some("Operation Monitoring")
    {
        bail!("driver_stage_journal_invalid")
    }
    let phase = journal
        .get("phase")
        .and_then(serde_json::Value::as_str)
        .context("driver_stage_journal_invalid")?;
    if phase == "committing" {
        let target_state = journal
            .get("target_state")
            .filter(|state| {
                state.get("provider").and_then(serde_json::Value::as_str)
                    == Some("Operation Monitoring")
                    && state
                        .get("packages")
                        .and_then(serde_json::Value::as_array)
                        .is_some_and(|packages| product_driver_packages_owned(packages))
            })
            .context("driver_stage_journal_invalid")?;
        write_driver_json(
            &journal_path.with_file_name("driver-state.json"),
            target_state,
        )?;
        std::fs::remove_file(journal_path).context("driver_stage_journal_cleanup_failed")?;
        return Ok(false);
    }
    if !matches!(phase, "installing" | "rollback_reboot_pending") {
        bail!("driver_stage_journal_invalid")
    }
    let preexisting = journal
        .get("preexisting_oem_infs")
        .and_then(serde_json::Value::as_array)
        .context("driver_stage_journal_invalid")?
        .iter()
        .map(|value| {
            value
                .as_str()
                .filter(|value| valid_oem_inf(value))
                .map(str::to_ascii_lowercase)
                .context("driver_stage_journal_invalid")
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    let bundle_version = journal
        .get("bundle_version")
        .and_then(serde_json::Value::as_str)
        .context("driver_stage_journal_invalid")?;
    if phase == "rollback_reboot_pending" {
        let current_boot_id = query_windows_boot_id()?;
        let reboot_boot_id = journal
            .get("reboot_boot_id")
            .and_then(serde_json::Value::as_str)
            .context("driver_stage_journal_invalid")?;
        if reboot_boot_id == current_boot_id {
            return Ok(true);
        }
        let allowed = preexisting
            .iter()
            .cloned()
            .collect::<std::collections::HashSet<_>>();
        if inventory_oem_infs(&query_product_driver_inventory()?)
            .into_iter()
            .all(|oem_inf| allowed.contains(&oem_inf))
        {
            std::fs::remove_file(journal_path).context("driver_stage_journal_cleanup_failed")?;
            return Ok(false);
        }
    }
    match rollback_new_product_oem_infs(&preexisting).context("driver_stage_recovery_failed")? {
        DriverRemovalOutcome::Complete => {
            let allowed = preexisting
                .iter()
                .cloned()
                .collect::<std::collections::HashSet<_>>();
            if !inventory_oem_infs(&query_product_driver_inventory()?)
                .into_iter()
                .all(|oem_inf| allowed.contains(&oem_inf))
            {
                bail!("driver_stage_recovery_failed")
            }
            std::fs::remove_file(journal_path).context("driver_stage_journal_cleanup_failed")?;
            Ok(false)
        }
        DriverRemovalOutcome::RebootRequired => {
            write_rollback_reboot_journal(journal_path, bundle_version, &preexisting)?;
            Ok(true)
        }
    }
}

#[cfg(windows)]
fn write_rollback_reboot_journal(
    journal_path: &std::path::Path,
    bundle_version: &str,
    preexisting: &[String],
) -> anyhow::Result<()> {
    write_driver_json(
        journal_path,
        &serde_json::json!({
            "schema_version": 1,
            "phase": "rollback_reboot_pending",
            "provider": "Operation Monitoring",
            "bundle_version": bundle_version,
            "preexisting_oem_infs": preexisting,
            "reboot_boot_id": query_windows_boot_id()?,
        }),
    )
}

#[cfg(windows)]
fn rollback_new_product_oem_infs(preexisting: &[String]) -> anyhow::Result<DriverRemovalOutcome> {
    use anyhow::{Context, bail};
    use std::{collections::HashSet, process::Command};

    let preexisting = preexisting
        .iter()
        .map(|value| value.to_ascii_lowercase())
        .collect::<HashSet<_>>();
    let current = inventory_oem_infs(&query_product_driver_inventory()?)
        .into_iter()
        .collect::<HashSet<_>>();
    let mut reboot_required = false;
    for oem_inf in current.difference(&preexisting) {
        let status = Command::new("pnputil.exe")
            .args(["/delete-driver", oem_inf, "/uninstall"])
            .status()
            .context("driver_stage_rollback_command_failed")?;
        match status.code() {
            Some(0) => {}
            Some(1641 | 3010) => reboot_required = true,
            _ => bail!("driver_stage_rollback_failed"),
        }
    }
    Ok(if reboot_required {
        DriverRemovalOutcome::RebootRequired
    } else {
        DriverRemovalOutcome::Complete
    })
}

#[cfg(windows)]
fn query_product_driver_inventory() -> anyhow::Result<Vec<ProductDriverPackage>> {
    use anyhow::{Context, bail};
    use std::process::Command;

    let script = r#"
$ErrorActionPreference = 'Stop'
[Console]::OutputEncoding = [Text.UTF8Encoding]::new($false)
$OutputEncoding = [Console]::OutputEncoding
$allowed = @('OmVirtualDisplay.inf', 'OmVirtualAudio.inf')
$entities = @{}
foreach ($entity in @(Get-CimInstance -ClassName Win32_PnPEntity -ErrorAction Stop)) {
    if (-not [string]::IsNullOrWhiteSpace("$($entity.PNPDeviceID)")) {
        $entities["$($entity.PNPDeviceID)".ToLowerInvariant()] = $entity
    }
}
$bindings = @(Get-CimInstance -ClassName Win32_PnPSignedDriver -ErrorAction Stop)
$result = foreach ($package in @(Get-WindowsDriver -Online -ErrorAction Stop | Where-Object {
    $_.ProviderName -ceq 'Operation Monitoring' -and
    $allowed -contains (Split-Path -Leaf $_.OriginalFileName)
})) {
    $publishedInf = "$($package.Driver)"
    $infPath = Join-Path $env:windir "INF\$publishedInf"
    $infText = Get-Content -LiteralPath $infPath -Raw -ErrorAction Stop
    $sections = @{}
    $pattern = '(?ms)^\s*\[(?<name>[^\]]+)\]\s*\r?\n(?<body>.*?)(?=^\s*\[|\z)'
    foreach ($match in [regex]::Matches($infText, $pattern)) {
        $sections[$match.Groups['name'].Value.Trim().ToLowerInvariant()] = $match.Groups['body'].Value
    }
    if (-not $sections.ContainsKey('manufacturer')) { throw "Installed INF has no Manufacturer section" }
    $modelSections = [Collections.Generic.HashSet[string]]::new([StringComparer]::OrdinalIgnoreCase)
    foreach ($rawLine in ($sections['manufacturer'] -split '\r?\n')) {
        $line = ($rawLine -replace ';.*$', '').Trim()
        if ($line -match '^[^=]+=(.*)$') {
            [void]$modelSections.Add(($Matches[1].Split(',')[0]).Trim())
        }
    }
    $hardwareIds = [Collections.Generic.List[string]]::new()
    foreach ($sectionName in $modelSections) {
        foreach ($section in $sections.GetEnumerator() | Where-Object {
            $_.Key -ieq $sectionName -or $_.Key.StartsWith("$sectionName.", [StringComparison]::OrdinalIgnoreCase)
        }) {
            foreach ($rawLine in ($section.Value -split '\r?\n')) {
                $line = ($rawLine -replace ';.*$', '').Trim()
                if ($line -notmatch '^[^=]+=(.*)$') { continue }
                $parts = @($Matches[1].Split(',') | ForEach-Object { $_.Trim() })
                if ($parts.Count -lt 2) { continue }
                foreach ($hardwareId in $parts[1..($parts.Count - 1)]) {
                    if (-not [string]::IsNullOrWhiteSpace($hardwareId)) { $hardwareIds.Add($hardwareId) }
                }
            }
        }
    }
    $devices = foreach ($binding in @($bindings | Where-Object { "$($_.InfName)" -ieq $publishedInf })) {
        $entity = $entities["$($binding.DeviceID)".ToLowerInvariant()]
        [pscustomobject]@{
            InstanceId = "$($binding.DeviceID)"
            InfName = "$($binding.InfName)"
            HardwareIds = @($entity.HardwareID)
            DriverVersion = "$($binding.DriverVersion)"
            Present = if ($null -eq $entity.Present) { $null } else { [bool]$entity.Present }
            Status = if ($null -eq $entity.Status) { $null } else { "$($entity.Status)" }
            ProblemCode = if ($null -eq $entity.ConfigManagerErrorCode) { $null } else { [uint32]$entity.ConfigManagerErrorCode }
        }
    }
    [pscustomobject]@{
        Driver = $publishedInf
        OriginalFileName = "$($package.OriginalFileName)"
        ProviderName = "$($package.ProviderName)"
        HardwareIds = @($hardwareIds)
        DriverVersion = "$($package.Version)"
        Devices = @($devices)
    }
}
@($result) | ConvertTo-Json -Compress -Depth 6
"#;
    let output = Command::new("powershell.exe")
        .args([
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            script,
        ])
        .output()
        .context("driver_inventory_failed")?;
    if !output.status.success() {
        bail!("driver_inventory_failed")
    }
    let text = String::from_utf8(output.stdout).context("driver_inventory_invalid")?;
    parse_product_driver_inventory(&text)
}

#[cfg(any(windows, test))]
fn valid_oem_inf(value: &str) -> bool {
    value
        .strip_prefix("oem")
        .and_then(|value| value.strip_suffix(".inf"))
        .is_some_and(|number| {
            !number.is_empty() && number.bytes().all(|byte| byte.is_ascii_digit())
        })
}

#[cfg(windows)]
pub fn uninstall_product_windows_drivers(program_data: &std::path::Path) -> anyhow::Result<()> {
    use anyhow::{Context, bail};
    use std::process::Command;

    if recover_incomplete_driver_stage(&program_data.join("driver-stage-journal.json"))? {
        bail!("driver_uninstall_reboot_required")
    }
    let state_path = program_data.join("driver-state.json");
    let Ok(bytes) = std::fs::read(&state_path) else {
        return Ok(());
    };
    let state: serde_json::Value = serde_json::from_slice(&bytes)?;
    if state.get("provider").and_then(serde_json::Value::as_str) != Some("Operation Monitoring") {
        bail!("driver_uninstall_ownership_mismatch")
    }
    let packages = state
        .get("packages")
        .and_then(serde_json::Value::as_array)
        .context("driver_uninstall_state_invalid")?;
    if !product_driver_packages_owned(packages) {
        bail!("driver_uninstall_ownership_mismatch")
    }
    let recorded = state
        .get("oem_infs")
        .and_then(serde_json::Value::as_array)
        .context("driver_uninstall_state_invalid")?
        .iter()
        .map(|value| {
            value
                .as_str()
                .filter(|value| valid_oem_inf(value))
                .map(str::to_ascii_lowercase)
                .context("driver_uninstall_state_invalid")
        })
        .collect::<anyhow::Result<std::collections::HashSet<_>>>()?;
    let inventory = query_product_driver_inventory()?;
    if inventory
        .iter()
        .filter(|package| recorded.contains(&package.oem_inf))
        .any(|package| !package_allowed_for_uninstall(package, packages))
    {
        bail!("driver_uninstall_ownership_mismatch")
    }
    let current = inventory
        .iter()
        .filter(|package| package_allowed_for_uninstall(package, packages))
        .map(|package| package.oem_inf.clone())
        .collect::<std::collections::HashSet<_>>();
    let mut reboot_required = false;
    for oem_inf in recorded.intersection(&current) {
        let status = Command::new("pnputil.exe")
            .args(["/delete-driver", oem_inf, "/uninstall"])
            .status()
            .context("driver_uninstall_command_failed")?;
        match status.code() {
            Some(0) => {}
            Some(1641 | 3010) => reboot_required = true,
            _ => bail!("driver_uninstall_failed"),
        }
    }
    if reboot_required {
        bail!("driver_uninstall_reboot_required")
    }
    Ok(())
}

#[cfg(any(windows, test))]
fn product_driver_packages_owned(packages: &[serde_json::Value]) -> bool {
    let mut kinds = std::collections::HashSet::new();
    packages.len() == 2
        && packages.iter().all(|package| {
            let Some(kind) = package.get("kind").and_then(serde_json::Value::as_str) else {
                return false;
            };
            let expected_hardware_id = match kind {
                "display" => "ROOT\\OMVIRTUALDISPLAY",
                "audio" => "ROOT\\OMVIRTUALAUDIO",
                _ => return false,
            };
            kinds.insert(kind)
                && package
                    .get("hardware_id")
                    .and_then(serde_json::Value::as_str)
                    .is_some_and(|hardware_id| {
                        hardware_id.eq_ignore_ascii_case(expected_hardware_id)
                    })
                && package
                    .get("driver_version")
                    .and_then(serde_json::Value::as_str)
                    .and_then(parse_windows_driver_version)
                    .is_some()
        })
        && kinds == std::collections::HashSet::from(["display", "audio"])
}

#[cfg(windows)]
pub fn stage_bundled_after_agent_health(config: &AgentConfig) -> anyhow::Result<bool> {
    use anyhow::Context;

    if config.windows_virtual_devices != WindowsVirtualDevices::Auto
        || !crate::windows_driver_assets::BUNDLED
        || !managed_local_system_service()
    {
        return Ok(false);
    }
    let data = std::env::var_os("ProgramData")
        .map(std::path::PathBuf::from)
        .context("driver_stage_program_data_missing")?
        .join("OperationMonitoring");
    stage_bundled_windows_drivers(&data)
}

#[cfg(windows)]
pub fn expected_display_source(config: &AgentConfig) -> String {
    let _ = config;
    WINDOWS_DEVICE_COORDINATOR_STATUS
        .get()
        .and_then(|status| {
            status
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .as_ref()
                .map(|status| match status.display.source {
                    RemoteAccessSource::Physical => "physical",
                    RemoteAccessSource::Virtual => "virtual",
                    RemoteAccessSource::None => "none",
                    RemoteAccessSource::Unknown => "unknown",
                })
        })
        .unwrap_or("unknown")
        .to_string()
}

#[cfg(windows)]
pub fn detected_display_source() -> Option<&'static str> {
    match display_device_presence().ok()? {
        DevicePresence { physical: true, .. } => Some("physical"),
        DevicePresence { any: true, .. } => Some("virtual"),
        _ => None,
    }
}

#[cfg(windows)]
fn write_verified_asset(
    path: &std::path::Path,
    expected: &str,
    bytes: &[u8],
) -> anyhow::Result<()> {
    use std::fs;

    use anyhow::{Context, bail};
    use sha2::{Digest, Sha256};

    let actual = crate::hex::encode_lower(Sha256::digest(bytes));
    if actual != expected {
        bail!("driver_bundle_hash_mismatch")
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).context("driver_stage_directory_failed")?;
    }
    if let Ok(existing) = fs::read(path) {
        let existing_hash = crate::hex::encode_lower(Sha256::digest(existing));
        if existing_hash == expected {
            return Ok(());
        }
    }
    let temporary = path.with_extension(format!("{}.tmp", uuid::Uuid::new_v4().simple()));
    fs::write(&temporary, bytes).context("driver_stage_write_failed")?;
    crate::windows_security::replace_file(&temporary, path)
        .context("driver_stage_replace_failed")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stable_codes_cannot_leak_raw_errors_or_paths() {
        assert_eq!(
            stable_code("driver_bundle_missing").as_deref(),
            Some("driver_bundle_missing")
        );
        for value in [
            "",
            "Access denied",
            r"c:\\windows\\inf\\oem42.inf",
            "error-5",
            &"a".repeat(65),
        ] {
            assert_eq!(stable_code(value), None);
        }
    }

    #[test]
    fn product_audio_endpoint_detection_uses_instance_id_and_stable_name_prefix() {
        assert!(is_product_audio_endpoint(
            r"SWD\OperationMonitoring\OmVirtualAudio",
            "Speakers"
        ));
        assert!(!is_product_audio_endpoint(
            r"SWD\OperationMonitoring\ROOT\OMVIRTUALAUDIO",
            "Speakers"
        ));
        assert!(is_product_audio_endpoint(
            "",
            "Operation Monitoring Virtual Audio Wave Render"
        ));
        assert!(!is_product_audio_endpoint(
            r"HDAUDIO\FUNC_01&VEN_10EC",
            "Speakers (Realtek Audio)"
        ));
    }

    #[test]
    fn driver_versions_only_move_forward_per_package() {
        assert!(should_install_driver_version(None, "1.0.0.0").unwrap());
        assert!(should_install_driver_version(Some("1.0.0.0"), "1.0.0.1").unwrap());
        assert!(!should_install_driver_version(Some("1.0.0.1"), "1.0.0.1").unwrap());
        assert!(!should_install_driver_version(Some("2.0.0.0"), "1.9.9.9").unwrap());
        assert!(should_install_driver_version(Some("invalid"), "1.0.0.0").is_err());
        assert!(should_install_driver_version(None, "1.0.0").is_err());
    }

    fn inventory_json(
        kind: &str,
        oem_inf: &str,
        version: &str,
        device: Option<serde_json::Value>,
    ) -> serde_json::Value {
        let (original_inf, hardware_id, _) = product_driver_identity(kind).unwrap();
        serde_json::json!({
            "Driver": oem_inf,
            "OriginalFileName": format!(r"C:\\Windows\\System32\\DriverStore\\FileRepository\\{original_inf}"),
            "ProviderName": "Operation Monitoring",
            "HardwareIds": [hardware_id],
            "DriverVersion": version,
            "Devices": device.into_iter().collect::<Vec<_>>(),
        })
    }

    fn state_packages() -> Vec<serde_json::Value> {
        vec![
            serde_json::json!({
                "kind": "display",
                "hardware_id": "ROOT\\OMVIRTUALDISPLAY",
                "driver_version": "1.2.3.4",
            }),
            serde_json::json!({
                "kind": "audio",
                "hardware_id": "ROOT\\OMVIRTUALAUDIO",
                "driver_version": "1.2.3.4",
            }),
        ]
    }

    #[test]
    fn product_inventory_requires_provider_inf_and_exact_hardware_id() {
        let valid = serde_json::to_string(&vec![inventory_json(
            "display",
            "oem40.inf",
            "1.2.3.4",
            None,
        )])
        .unwrap();
        assert_eq!(parse_product_driver_inventory(&valid).unwrap().len(), 1);

        for (field, replacement) in [
            ("ProviderName", serde_json::json!("Another Provider")),
            ("OriginalFileName", serde_json::json!(r"C:\\Other.inf")),
            ("HardwareIds", serde_json::json!([r"ROOT\OTHERDEVICE"])),
        ] {
            let mut entry = inventory_json("display", "oem40.inf", "1.2.3.4", None);
            entry[field] = replacement;
            assert!(parse_product_driver_inventory(&entry.to_string()).is_err());
        }
    }

    #[test]
    fn reboot_validation_fails_closed_until_expected_devnodes_are_healthy() {
        let packages = state_packages();
        let recorded =
            std::collections::HashSet::from(["oem40.inf".to_string(), "oem41.inf".to_string()]);
        let no_devices = parse_product_driver_inventory(
            &serde_json::to_string(&vec![
                inventory_json("display", "oem40.inf", "1.2.3.4", None),
                inventory_json("audio", "oem41.inf", "1.2.3.4", None),
            ])
            .unwrap(),
        )
        .unwrap();
        assert_eq!(
            validate_rebooted_product_drivers(&packages, &no_devices, &recorded),
            DriverRebootValidation::Pending
        );

        let healthy_device = |kind: &str, inf: &str| {
            let (_, hardware_id, instance_id) = product_driver_identity(kind).unwrap();
            serde_json::json!({
                "InstanceId": instance_id,
                "InfName": inf,
                "HardwareIds": [hardware_id],
                "DriverVersion": "1.2.3.4",
                "Present": true,
                "Status": "OK",
                "ProblemCode": 0,
            })
        };
        let healthy = parse_product_driver_inventory(
            &serde_json::to_string(&vec![
                inventory_json(
                    "display",
                    "oem40.inf",
                    "1.2.3.4",
                    Some(healthy_device("display", "oem40.inf")),
                ),
                inventory_json(
                    "audio",
                    "oem41.inf",
                    "1.2.3.4",
                    Some(healthy_device("audio", "oem41.inf")),
                ),
            ])
            .unwrap(),
        )
        .unwrap();
        assert_eq!(
            validate_rebooted_product_drivers(&packages, &healthy, &recorded),
            DriverRebootValidation::Healthy
        );

        let mut failed_json = serde_json::to_value(&[
            inventory_json(
                "display",
                "oem40.inf",
                "1.2.3.4",
                Some(healthy_device("display", "oem40.inf")),
            ),
            inventory_json(
                "audio",
                "oem41.inf",
                "1.2.3.4",
                Some(healthy_device("audio", "oem41.inf")),
            ),
        ])
        .unwrap();
        failed_json[1]["Devices"][0]["ProblemCode"] = serde_json::json!(10);
        let failed = parse_product_driver_inventory(&failed_json.to_string()).unwrap();
        assert_eq!(
            validate_rebooted_product_drivers(&packages, &failed, &recorded),
            DriverRebootValidation::Failed
        );
    }

    #[test]
    fn uninstall_rejects_newer_or_foreign_device_bindings() {
        let packages = state_packages();
        let device = serde_json::json!({
            "InstanceId": r"SWD\OperationMonitoring\OmVirtualDisplay",
            "InfName": "oem40.inf",
            "HardwareIds": [r"ROOT\OMVIRTUALDISPLAY"],
            "DriverVersion": "1.2.3.4",
            "Present": true,
            "Status": "OK",
            "ProblemCode": 0,
        });
        let inventory = parse_product_driver_inventory(
            &inventory_json("display", "oem40.inf", "1.2.3.4", Some(device)).to_string(),
        )
        .unwrap();
        assert!(package_allowed_for_uninstall(&inventory[0], &packages));

        let newer = parse_product_driver_inventory(
            &inventory_json("display", "oem40.inf", "2.0.0.0", None).to_string(),
        )
        .unwrap();
        assert!(!package_allowed_for_uninstall(&newer[0], &packages));
    }

    #[test]
    fn oem_inf_names_are_strictly_scoped() {
        assert!(valid_oem_inf("oem0.inf"));
        assert!(valid_oem_inf("oem42.inf"));
        for value in [
            "OEM42.inf",
            "oem.inf",
            "oem-1.inf",
            "oem42.inf.bak",
            "oem42",
        ] {
            assert!(!valid_oem_inf(value));
        }
    }

    #[test]
    fn uninstall_ownership_requires_exact_product_packages() {
        let display = serde_json::json!({
            "kind": "display",
            "hardware_id": "ROOT\\OMVIRTUALDISPLAY",
            "driver_version": "1.0.0.0",
        });
        let audio = serde_json::json!({
            "kind": "audio",
            "hardware_id": "ROOT\\OMVIRTUALAUDIO",
            "driver_version": "1.0.0.0",
        });
        assert!(product_driver_packages_owned(&[
            display.clone(),
            audio.clone()
        ]));
        assert!(!product_driver_packages_owned(&[
            display.clone(),
            display.clone()
        ]));
        let physical = serde_json::json!({
            "kind": "audio",
            "hardware_id": "HDAUDIO\\FUNC_01&VEN_10EC",
            "driver_version": "1.0.0.0",
        });
        assert!(!product_driver_packages_owned(&[display, physical]));
    }

    #[test]
    fn device_presence_uses_initial_activation_and_stable_hysteresis() {
        use std::time::{Duration, Instant};

        let start = Instant::now();
        let mut initial_missing = PresenceHysteresis::new();
        assert!(initial_missing.wants_lease(start, false, false));

        let mut state = PresenceHysteresis::new();
        assert!(!state.wants_lease(start, true, false));
        assert!(!state.wants_lease(start + Duration::from_secs(2), false, false));
        assert!(state.wants_lease(start + Duration::from_secs(5), false, false));
        assert!(state.wants_lease(start + Duration::from_secs(6), true, true));
        assert!(state.wants_lease(start + Duration::from_secs(15), true, true));
        assert!(!state.wants_lease(start + Duration::from_secs(16), true, true));
    }

    #[test]
    fn device_probe_snapshot_rejects_session_mismatch_and_impossible_presence() {
        let valid = DeviceProbeSnapshot {
            schema_version: 1,
            session_id: 7,
            display: Some(DevicePresence {
                physical: false,
                any: false,
            }),
            audio: None,
        };
        assert!(valid.validate(7).is_ok());
        assert!(valid.validate(8).is_err());

        let mut impossible = valid;
        impossible.display = Some(DevicePresence {
            physical: true,
            any: false,
        });
        assert!(impossible.validate(7).is_err());
    }
}
