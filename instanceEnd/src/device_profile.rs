use std::{
    process::{Command, Output, Stdio},
    sync::OnceLock,
    time::{Duration, Instant},
};

use network_interface::{Addr, NetworkInterface, NetworkInterfaceConfig};
use sysinfo::{Disks, System};

use crate::{
    metrics::aggregate_disk_total,
    models::{
        DeviceCpuInfo, DeviceDiskInfo, DeviceGpuInfo, DeviceNetworkInterface, DeviceProfile,
        DeviceSystemInfo,
    },
    time::now_ts,
};

const SCHEMA_VERSION: u32 = 1;
const MAX_STRING_BYTES: usize = 256;
const MAX_GPUS: usize = 16;
const MAX_DISKS: usize = 32;
const MAX_INTERFACES: usize = 32;
const MAX_ADDRESSES_PER_INTERFACE: usize = 16;
const MAX_PROFILE_BYTES: usize = 64 * 1024;
#[cfg(target_os = "linux")]
const MAX_DRM_GPU_CANDIDATES: usize = 32;
#[cfg(target_os = "linux")]
const LINUX_GPU_PROBE_BUDGET: Duration = Duration::from_secs(4);
#[cfg(target_os = "linux")]
const UDEV_PROBE_TIMEOUT: Duration = Duration::from_millis(750);

static STATIC_PROFILE: OnceLock<DeviceProfile> = OnceLock::new();

pub fn collect_device_profile() -> DeviceProfile {
    let mut profile = STATIC_PROFILE.get_or_init(collect_static_profile).clone();
    profile.collected_at = now_ts();
    profile.network_interfaces = collect_network_interfaces();
    enforce_transport_budget(&mut profile);
    profile
}

fn collect_static_profile() -> DeviceProfile {
    let mut system = System::new_all();
    system.refresh_all();
    let disks = Disks::new_with_refreshed_list();
    let first_cpu = system.cpus().first();
    let frequency_mhz = system
        .cpus()
        .iter()
        .map(|cpu| cpu.frequency())
        .filter(|frequency| *frequency > 0)
        .max();

    let mut disk_profiles = disks
        .iter()
        .filter(|disk| disk.total_space() > 0)
        .map(|disk| DeviceDiskInfo {
            name: clean_text(&disk.name().to_string_lossy()),
            mount_point: clean_text(&disk.mount_point().to_string_lossy()),
            file_system: clean_text(&disk.file_system().to_string_lossy()),
            kind: clean_text(&format!("{:?}", disk.kind()).to_ascii_lowercase()),
            total_bytes: to_i64(disk.total_space()),
        })
        .collect::<Vec<_>>();
    disk_profiles.sort_by(|left, right| {
        left.mount_point
            .cmp(&right.mount_point)
            .then_with(|| left.name.cmp(&right.name))
    });
    disk_profiles.dedup();
    disk_profiles.truncate(MAX_DISKS);

    DeviceProfile {
        schema_version: SCHEMA_VERSION,
        collected_at: now_ts(),
        system: DeviceSystemInfo {
            os_name: clean_text(&System::name().unwrap_or_else(|| std::env::consts::OS.into())),
            os_version: clean_text(
                &System::long_os_version()
                    .or_else(System::os_version)
                    .unwrap_or_default(),
            ),
            kernel_version: clean_text(&System::kernel_version().unwrap_or_default()),
            architecture: clean_text(std::env::consts::ARCH),
        },
        cpu: DeviceCpuInfo {
            model: clean_text(first_cpu.map(|cpu| cpu.brand()).unwrap_or_default()),
            vendor: clean_text(first_cpu.map(|cpu| cpu.vendor_id()).unwrap_or_default()),
            physical_cores: system
                .physical_core_count()
                .and_then(|count| u32::try_from(count).ok()),
            logical_cores: u32::try_from(system.cpus().len()).unwrap_or(u32::MAX),
            frequency_mhz,
        },
        memory_total: to_i64(system.total_memory()),
        storage_total: aggregate_disk_total(&disks),
        gpus: collect_gpus(),
        disks: disk_profiles,
        network_interfaces: Vec::new(),
    }
}

fn collect_network_interfaces() -> Vec<DeviceNetworkInterface> {
    let Ok(interfaces) = NetworkInterface::show() else {
        return Vec::new();
    };
    device_network_interfaces(interfaces)
}

fn device_network_interfaces(interfaces: Vec<NetworkInterface>) -> Vec<DeviceNetworkInterface> {
    let mut profiles = interfaces
        .into_iter()
        .filter(|interface| !interface.internal)
        .filter_map(|interface| {
            let mut ipv4 = Vec::new();
            let mut ipv6 = Vec::new();
            let mut saw_loopback = false;
            for address in interface.addr {
                match address {
                    Addr::V4(address)
                        if !address.ip.is_loopback() && !address.ip.is_unspecified() =>
                    {
                        ipv4.push(address.ip.to_string())
                    }
                    Addr::V6(address)
                        if !address.ip.is_loopback() && !address.ip.is_unspecified() =>
                    {
                        ipv6.push(address.ip.to_string())
                    }
                    Addr::V4(address) => saw_loopback |= address.ip.is_loopback(),
                    Addr::V6(address) => saw_loopback |= address.ip.is_loopback(),
                }
            }
            ipv4.sort();
            ipv4.dedup();
            ipv4.truncate(MAX_ADDRESSES_PER_INTERFACE);
            ipv6.sort();
            ipv6.dedup();
            ipv6.truncate(MAX_ADDRESSES_PER_INTERFACE);
            let mac_address = interface
                .mac_addr
                .as_deref()
                .and_then(normalize_mac_address);
            let name = clean_text(&interface.name);
            (!name.is_empty()
                && (!ipv4.is_empty()
                    || !ipv6.is_empty()
                    || (mac_address.is_some() && !saw_loopback)))
                .then(|| DeviceNetworkInterface {
                    name,
                    mac_address,
                    ipv4,
                    ipv6,
                })
        })
        .collect::<Vec<_>>();
    profiles.sort_by(|left, right| left.name.cmp(&right.name));
    profiles = merge_network_interfaces(profiles);
    profiles.truncate(MAX_INTERFACES);
    profiles
}

fn merge_network_interfaces(profiles: Vec<DeviceNetworkInterface>) -> Vec<DeviceNetworkInterface> {
    let mut merged: Vec<DeviceNetworkInterface> = Vec::new();
    for mut profile in profiles {
        if let Some(existing) = merged.last_mut()
            && existing.name == profile.name
        {
            if existing.mac_address.is_none() {
                existing.mac_address = profile.mac_address.take();
            }
            existing.ipv4.append(&mut profile.ipv4);
            existing.ipv6.append(&mut profile.ipv6);
            existing.ipv4.sort();
            existing.ipv4.dedup();
            existing.ipv4.truncate(MAX_ADDRESSES_PER_INTERFACE);
            existing.ipv6.sort();
            existing.ipv6.dedup();
            existing.ipv6.truncate(MAX_ADDRESSES_PER_INTERFACE);
            continue;
        }
        merged.push(profile);
    }
    merged
}

fn normalize_mac_address(value: &str) -> Option<String> {
    let compact = value
        .chars()
        .filter(|character| !matches!(character, ':' | '-'))
        .collect::<String>();
    if compact.len() != 12
        || !compact.bytes().all(|byte| byte.is_ascii_hexdigit())
        || compact.bytes().all(|byte| byte == b'0')
    {
        return None;
    }
    let bytes = compact
        .as_bytes()
        .chunks_exact(2)
        .map(|pair| {
            std::str::from_utf8(pair)
                .ok()
                .and_then(|pair| u8::from_str_radix(pair, 16).ok())
        })
        .collect::<Option<Vec<_>>>()?;
    if bytes.iter().all(|byte| *byte == u8::MAX) || bytes[0] & 1 != 0 {
        return None;
    }
    Some(
        compact
            .as_bytes()
            .chunks(2)
            .map(|pair| String::from_utf8_lossy(pair).to_ascii_uppercase())
            .collect::<Vec<_>>()
            .join(":"),
    )
}

fn enforce_transport_budget(profile: &mut DeviceProfile) {
    while serde_json::to_vec(profile).is_ok_and(|encoded| encoded.len() > MAX_PROFILE_BYTES) {
        if let Some(interface) = profile.network_interfaces.last_mut() {
            if interface.ipv6.pop().is_some() || interface.ipv4.pop().is_some() {
                continue;
            }
            profile.network_interfaces.pop();
        } else if profile.disks.pop().is_some() {
            continue;
        } else if profile.gpus.pop().is_none() {
            break;
        }
    }
}

fn collect_gpus() -> Vec<DeviceGpuInfo> {
    let mut gpus = collect_nvidia_gpus();

    #[cfg(target_os = "windows")]
    gpus.extend(collect_windows_gpus(!gpus.is_empty()));

    #[cfg(target_os = "linux")]
    gpus.extend(collect_linux_gpus(!gpus.is_empty()));

    #[cfg(target_os = "macos")]
    gpus.extend(collect_macos_gpus());

    gpus.sort_by(|left, right| {
        left.vendor
            .cmp(&right.vendor)
            .then_with(|| left.name.cmp(&right.name))
            .then_with(|| left.memory_total.cmp(&right.memory_total))
    });
    gpus.dedup();
    gpus.truncate(MAX_GPUS);
    gpus
}

fn collect_nvidia_gpus() -> Vec<DeviceGpuInfo> {
    let args = [
        "--query-gpu=name,memory.total",
        "--format=csv,noheader,nounits",
    ];

    #[cfg(target_os = "windows")]
    let commands = {
        let mut commands = vec![std::path::PathBuf::from("nvidia-smi")];
        for root in [
            std::env::var_os("ProgramW6432"),
            std::env::var_os("ProgramFiles"),
        ]
        .into_iter()
        .flatten()
        {
            commands.push(
                std::path::PathBuf::from(root)
                    .join("NVIDIA Corporation")
                    .join("NVSMI")
                    .join("nvidia-smi.exe"),
            );
        }
        commands
    };

    #[cfg(not(target_os = "windows"))]
    let commands = vec![std::path::PathBuf::from("nvidia-smi")];

    let Some(output) = commands.into_iter().find_map(|command| {
        run_bounded_command(Command::new(command).args(args))
            .filter(|output| output.status.success())
    }) else {
        return Vec::new();
    };
    parse_nvidia_gpus(&String::from_utf8_lossy(&output.stdout))
}

fn parse_nvidia_gpus(output: &str) -> Vec<DeviceGpuInfo> {
    output
        .lines()
        .filter_map(|line| {
            let (name, memory) = line.split_once(',')?;
            let name = clean_text(name.trim());
            if name.is_empty() {
                return None;
            }
            Some(DeviceGpuInfo {
                name,
                vendor: "NVIDIA".to_string(),
                memory_total: memory
                    .trim()
                    .parse::<i64>()
                    .ok()
                    .filter(|memory| *memory > 0)
                    .and_then(|memory| memory.checked_mul(1024 * 1024)),
            })
        })
        .collect()
}

#[cfg(target_os = "windows")]
fn collect_windows_gpus(skip_nvidia: bool) -> Vec<DeviceGpuInfo> {
    use windows::Win32::Graphics::Dxgi::{
        CreateDXGIFactory1, DXGI_ADAPTER_DESC1, DXGI_ADAPTER_FLAG_SOFTWARE, IDXGIFactory1,
    };

    let Ok(factory) = (unsafe { CreateDXGIFactory1::<IDXGIFactory1>() }) else {
        return Vec::new();
    };
    let mut gpus = Vec::new();
    for index in 0.. {
        let Ok(adapter) = (unsafe { factory.EnumAdapters1(index) }) else {
            break;
        };
        let mut description = DXGI_ADAPTER_DESC1::default();
        if unsafe { adapter.GetDesc1(&mut description) }.is_err()
            || description.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 != 0
            || description.VendorId == 0
            || (skip_nvidia && description.VendorId == 0x10de)
        {
            continue;
        }
        let name_length = description
            .Description
            .iter()
            .position(|character| *character == 0)
            .unwrap_or(description.Description.len());
        let name = clean_text(&String::from_utf16_lossy(
            &description.Description[..name_length],
        ));
        if name.is_empty() {
            continue;
        }
        gpus.push(DeviceGpuInfo {
            name,
            vendor: pci_vendor_name(description.VendorId),
            memory_total: i64::try_from(description.DedicatedVideoMemory)
                .ok()
                .filter(|memory| *memory > 0),
        });
    }
    gpus
}

#[cfg(target_os = "linux")]
fn collect_linux_gpus(skip_nvidia: bool) -> Vec<DeviceGpuInfo> {
    let Ok(cards) = std::fs::read_dir("/sys/class/drm") else {
        return Vec::new();
    };
    let mut cards = cards
        .filter_map(Result::ok)
        .filter(|entry| {
            entry.file_name().to_str().is_some_and(|name| {
                name.strip_prefix("card").is_some_and(|suffix| {
                    !suffix.is_empty() && suffix.bytes().all(|b| b.is_ascii_digit())
                })
            })
        })
        .collect::<Vec<_>>();
    cards.sort_by_key(|entry| entry.file_name());
    cards.truncate(MAX_DRM_GPU_CANDIDATES);

    let deadline = Instant::now() + LINUX_GPU_PROBE_BUDGET;
    let mut profiles = Vec::new();
    for entry in cards {
        if profiles.len() >= MAX_GPUS {
            break;
        }
        let device = entry.path().join("device");
        let Some(vendor_id) = read_trimmed(device.join("vendor")) else {
            continue;
        };
        if skip_nvidia && vendor_id.eq_ignore_ascii_case("0x10de") {
            continue;
        }
        let numeric_vendor = u32::from_str_radix(vendor_id.trim_start_matches("0x"), 16).ok();
        let vendor = numeric_vendor
            .map(pci_vendor_name)
            .unwrap_or_else(|| clean_text(&vendor_id));
        let device_id = read_trimmed(device.join("device")).unwrap_or_default();
        let udev_timeout = deadline
            .checked_duration_since(Instant::now())
            .filter(|remaining| !remaining.is_zero())
            .map(|remaining| remaining.min(UDEV_PROBE_TIMEOUT));
        let model = udev_timeout
            .and_then(|timeout| udev_model(&device, timeout))
            .or_else(|| {
                std::fs::read_link(device.join("driver"))
                    .ok()
                    .and_then(|path| {
                        path.file_name()
                            .map(|name| name.to_string_lossy().into_owned())
                    })
            });
        let name = clean_text(
            &model.unwrap_or_else(|| format!("{vendor} GPU {device_id}").trim().to_string()),
        );
        if name.is_empty() {
            continue;
        }
        profiles.push(DeviceGpuInfo {
            name,
            vendor,
            memory_total: read_trimmed(device.join("mem_info_vram_total"))
                .and_then(|value| value.parse::<i64>().ok())
                .filter(|memory| *memory > 0),
        });
    }
    profiles
}

#[cfg(target_os = "linux")]
fn read_trimmed(path: impl AsRef<std::path::Path>) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

#[cfg(target_os = "linux")]
fn udev_model(device: &std::path::Path, timeout: Duration) -> Option<String> {
    let output = run_bounded_command_with_timeout(
        Command::new("udevadm")
            .arg("info")
            .arg("--query=property")
            .arg(format!("--path={}", device.display())),
        timeout,
    )
    .filter(|output| output.status.success())?;
    let properties = String::from_utf8_lossy(&output.stdout);
    ["ID_MODEL_FROM_DATABASE", "ID_MODEL"]
        .into_iter()
        .find_map(|key| {
            properties
                .lines()
                .find_map(|line| line.strip_prefix(&format!("{key}=")))
                .map(str::to_string)
        })
}

#[cfg(target_os = "macos")]
fn collect_macos_gpus() -> Vec<DeviceGpuInfo> {
    let Some(output) = run_bounded_command(
        Command::new("/usr/sbin/system_profiler").args(["SPDisplaysDataType", "-json"]),
    )
    .filter(|output| output.status.success()) else {
        return Vec::new();
    };
    parse_macos_gpus(&String::from_utf8_lossy(&output.stdout))
}

#[cfg(any(target_os = "macos", test))]
fn parse_macos_gpus(output: &str) -> Vec<DeviceGpuInfo> {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(output) else {
        return Vec::new();
    };
    value
        .get("SPDisplaysDataType")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|display| {
            let name = display
                .get("sppci_model")
                .or_else(|| display.get("_name"))
                .and_then(serde_json::Value::as_str)
                .map(clean_text)?;
            (!name.is_empty()).then(|| DeviceGpuInfo {
                vendor: display
                    .get("spdisplays_vendor")
                    .and_then(serde_json::Value::as_str)
                    .map(clean_text)
                    .filter(|vendor| !vendor.is_empty())
                    .unwrap_or_else(|| "Apple".to_string()),
                name,
                memory_total: display
                    .get("spdisplays_vram")
                    .and_then(serde_json::Value::as_str)
                    .and_then(parse_memory_size),
            })
        })
        .collect()
}

#[cfg(any(target_os = "macos", test))]
fn parse_memory_size(value: &str) -> Option<i64> {
    let mut parts = value.split_whitespace();
    let amount = parts.next()?.parse::<f64>().ok()?;
    let multiplier = match parts.next()?.to_ascii_lowercase().as_str() {
        "mb" => 1024_f64.powi(2),
        "gb" => 1024_f64.powi(3),
        "tb" => 1024_f64.powi(4),
        _ => return None,
    };
    let bytes = amount * multiplier;
    (bytes.is_finite() && bytes > 0.0 && bytes <= i64::MAX as f64).then_some(bytes as i64)
}

#[cfg(any(target_os = "linux", target_os = "windows"))]
fn pci_vendor_name(vendor_id: u32) -> String {
    match vendor_id {
        0x1002 | 0x1022 => "AMD".to_string(),
        0x10de => "NVIDIA".to_string(),
        0x106b => "Apple".to_string(),
        0x8086 => "Intel".to_string(),
        _ => format!("PCI {vendor_id:#06x}"),
    }
}

fn clean_text(value: &str) -> String {
    let cleaned = value
        .chars()
        .filter(|character| !character.is_control())
        .collect::<String>();
    let trimmed = cleaned.trim();
    if trimmed.len() <= MAX_STRING_BYTES {
        return trimmed.to_string();
    }
    let mut end = MAX_STRING_BYTES;
    while !trimmed.is_char_boundary(end) {
        end -= 1;
    }
    trimmed[..end].to_string()
}

fn to_i64(value: u64) -> i64 {
    value.min(i64::MAX as u64) as i64
}

pub(crate) fn run_bounded_command(command: &mut Command) -> Option<Output> {
    run_bounded_command_with_timeout(command, Duration::from_secs(5))
}

fn run_bounded_command_with_timeout(command: &mut Command, timeout: Duration) -> Option<Output> {
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return child.wait_with_output().ok(),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(25)),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_and_rejects_mac_addresses() {
        assert_eq!(
            normalize_mac_address("aa-bb-cc-dd-ee-ff"),
            Some("AA:BB:CC:DD:EE:FF".to_string())
        );
        assert_eq!(normalize_mac_address("00:00:00:00:00:00"), None);
        assert_eq!(normalize_mac_address("FF:FF:FF:FF:FF:FF"), None);
        assert_eq!(normalize_mac_address("01:00:5E:00:00:01"), None);
        assert_eq!(normalize_mac_address("not-a-mac"), None);
    }

    #[test]
    fn merges_duplicate_network_interfaces_and_addresses() {
        let profiles = merge_network_interfaces(vec![
            DeviceNetworkInterface {
                name: "eth0".to_string(),
                mac_address: None,
                ipv4: vec!["192.0.2.1".to_string()],
                ipv6: Vec::new(),
            },
            DeviceNetworkInterface {
                name: "eth0".to_string(),
                mac_address: Some("02:00:00:00:00:01".to_string()),
                ipv4: vec!["192.0.2.1".to_string(), "192.0.2.2".to_string()],
                ipv6: vec!["2001:db8::1".to_string()],
            },
        ]);
        assert_eq!(profiles.len(), 1);
        assert_eq!(
            profiles[0].mac_address.as_deref(),
            Some("02:00:00:00:00:01")
        );
        assert_eq!(profiles[0].ipv4, ["192.0.2.1", "192.0.2.2"]);
        assert_eq!(profiles[0].ipv6, ["2001:db8::1"]);
    }

    #[test]
    fn excludes_internal_network_interfaces() {
        let profiles = device_network_interfaces(vec![
            NetworkInterface {
                name: "internal0".to_string(),
                addr: vec![Addr::V4(network_interface::V4IfAddr {
                    ip: "192.0.2.10".parse().unwrap(),
                    broadcast: None,
                    netmask: None,
                })],
                mac_addr: Some("02:00:00:00:00:01".to_string()),
                index: 1,
                internal: true,
            },
            NetworkInterface {
                name: "external0".to_string(),
                addr: vec![Addr::V4(network_interface::V4IfAddr {
                    ip: "198.51.100.10".parse().unwrap(),
                    broadcast: None,
                    netmask: None,
                })],
                mac_addr: Some("02:00:00:00:00:02".to_string()),
                index: 2,
                internal: false,
            },
        ]);

        assert_eq!(profiles.len(), 1);
        assert_eq!(profiles[0].name, "external0");
        assert_eq!(profiles[0].ipv4, ["198.51.100.10"]);
    }

    #[test]
    fn parses_nvidia_device_profiles() {
        assert_eq!(
            parse_nvidia_gpus("NVIDIA RTX 4090, 24564\n"),
            vec![DeviceGpuInfo {
                name: "NVIDIA RTX 4090".to_string(),
                vendor: "NVIDIA".to_string(),
                memory_total: Some(24_564 * 1024 * 1024),
            }]
        );
    }

    #[test]
    fn parses_macos_display_profiles() {
        let profiles = parse_macos_gpus(
            r#"{"SPDisplaysDataType":[{"sppci_model":"Apple M3 Max","spdisplays_vendor":"Apple","spdisplays_vram":"36 GB"}]}"#,
        );
        assert_eq!(profiles.len(), 1);
        assert_eq!(profiles[0].name, "Apple M3 Max");
        assert_eq!(profiles[0].memory_total, Some(36 * 1024 * 1024 * 1024));
    }

    #[test]
    fn cleans_control_characters_and_limits_utf8_safely() {
        let input = format!("{}\nsecret", "测".repeat(100));
        let cleaned = clean_text(&input);
        assert!(cleaned.len() <= MAX_STRING_BYTES);
        assert!(!cleaned.contains('\n'));
    }

    #[test]
    fn device_profile_stays_within_transport_budget() {
        let profile = collect_device_profile();
        assert!(serde_json::to_vec(&profile).unwrap().len() <= MAX_PROFILE_BYTES);
    }

    #[test]
    fn worst_case_device_profile_is_trimmed_to_transport_budget() {
        let long = "x".repeat(MAX_STRING_BYTES);
        let mut profile = DeviceProfile {
            schema_version: SCHEMA_VERSION,
            collected_at: 1,
            system: DeviceSystemInfo {
                os_name: long.clone(),
                os_version: long.clone(),
                kernel_version: long.clone(),
                architecture: long.clone(),
            },
            cpu: DeviceCpuInfo {
                model: long.clone(),
                vendor: long.clone(),
                physical_cores: Some(64),
                logical_cores: 128,
                frequency_mhz: Some(5_000),
            },
            memory_total: i64::MAX,
            storage_total: i64::MAX,
            gpus: (0..MAX_GPUS)
                .map(|_| DeviceGpuInfo {
                    name: long.clone(),
                    vendor: long.clone(),
                    memory_total: Some(i64::MAX),
                })
                .collect(),
            disks: (0..MAX_DISKS)
                .map(|_| DeviceDiskInfo {
                    name: long.clone(),
                    mount_point: long.clone(),
                    file_system: long.clone(),
                    kind: long.clone(),
                    total_bytes: i64::MAX,
                })
                .collect(),
            network_interfaces: (0..MAX_INTERFACES)
                .map(|index| DeviceNetworkInterface {
                    name: format!("eth{index}-{long}"),
                    mac_address: Some("02:00:00:00:00:01".to_string()),
                    ipv4: (0..MAX_ADDRESSES_PER_INTERFACE)
                        .map(|address| format!("192.0.2.{address}"))
                        .collect(),
                    ipv6: (0..MAX_ADDRESSES_PER_INTERFACE)
                        .map(|address| format!("2001:db8:ffff:ffff:ffff:ffff:ffff:{address:x}"))
                        .collect(),
                })
                .collect(),
        };

        assert!(serde_json::to_vec(&profile).unwrap().len() > MAX_PROFILE_BYTES);
        enforce_transport_budget(&mut profile);
        assert!(serde_json::to_vec(&profile).unwrap().len() <= MAX_PROFILE_BYTES);
    }
}
