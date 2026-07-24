use std::{ffi::OsString, process::Command};

#[cfg(any(not(target_os = "linux"), test))]
use std::collections::HashSet;

#[cfg(any(target_os = "linux", target_os = "windows", test))]
use std::collections::HashMap;

#[cfg(any(target_os = "linux", test))]
use std::path::{Path, PathBuf};

#[cfg(target_os = "linux")]
use std::os::unix::fs::MetadataExt;

#[cfg(target_os = "windows")]
use std::mem::size_of;

use sysinfo::{Disk, Disks, Networks, System};

#[cfg(target_os = "windows")]
use windows::{
    Win32::{
        Graphics::Dxgi::{
            CreateDXGIFactory1, DXGI_ADAPTER_DESC1, DXGI_ADAPTER_FLAG_SOFTWARE, IDXGIFactory1,
        },
        System::Performance::{
            PDH_CSTATUS_NEW_DATA, PDH_CSTATUS_VALID_DATA, PDH_FMT_COUNTERVALUE_ITEM_W,
            PDH_FMT_DOUBLE, PDH_MORE_DATA, PdhAddEnglishCounterW, PdhCloseQuery,
            PdhCollectQueryData, PdhGetFormattedCounterArrayW, PdhOpenQueryW,
        },
    },
    core::{PCWSTR, w},
};

use crate::{models::MetricPayload, time::now_ts};

pub struct MetricsCollector {
    system: System,
    disks: Disks,
    networks: Networks,
    gpu: GpuCollector,
}

#[derive(Debug, Default, PartialEq)]
struct GpuMetrics {
    percent: Option<f64>,
    memory_used: Option<i64>,
    memory_total: Option<i64>,
}

#[derive(Debug, Default)]
struct GpuSample {
    percent: Option<f64>,
    memory_used: Option<i64>,
    memory_total: Option<i64>,
}

struct GpuCollector {
    #[cfg(target_os = "windows")]
    windows: WindowsGpuCollector,
}

#[derive(Clone, Debug)]
struct DiskSample {
    #[cfg(any(target_os = "linux", test))]
    source: OsString,
    file_system: OsString,
    #[cfg(any(target_os = "linux", test))]
    mount_point: PathBuf,
    total: u64,
    free: u64,
    #[cfg(any(target_os = "linux", test))]
    device_id: Option<u64>,
}

#[cfg(any(target_os = "linux", test))]
#[derive(Debug, Eq, Hash, PartialEq)]
enum LinuxDiskIdentity {
    Device(u64),
    ZfsPool(OsString),
    Source(OsString),
    MountPoint(PathBuf),
}

impl MetricsCollector {
    pub fn new() -> Self {
        let mut system = System::new_all();
        system.refresh_all();
        Self {
            system,
            disks: Disks::new_with_refreshed_list(),
            networks: Networks::new_with_refreshed_list(),
            gpu: GpuCollector::new(),
        }
    }

    pub fn sample(&mut self) -> MetricPayload {
        self.system.refresh_all();
        self.disks.refresh_list();
        self.networks.refresh();

        let disk_samples = self.disks.iter().map(disk_sample).collect::<Vec<_>>();
        let (disk_used, disk_total) = aggregate_disk_metrics(&disk_samples);

        let (network_rx, network_tx) = self.networks.iter().fold((0_i64, 0_i64), |acc, item| {
            (
                acc.0 + item.1.total_received() as i64,
                acc.1 + item.1.total_transmitted() as i64,
            )
        });

        let load_average = System::load_average();
        let gpu = self
            .gpu
            .sample(self.system.total_memory().min(i64::MAX as u64) as i64);
        MetricPayload {
            ts: now_ts(),
            cpu_percent: self.system.global_cpu_info().cpu_usage() as f64,
            memory_used: self.system.used_memory() as i64,
            memory_total: self.system.total_memory() as i64,
            disk_used,
            disk_total,
            network_rx,
            network_tx,
            gpu_percent: gpu.percent,
            gpu_memory_used: gpu.memory_used,
            gpu_memory_total: gpu.memory_total,
            uptime_seconds: System::uptime() as i64,
            load_average: Some(load_average.one),
        }
    }
}

fn disk_sample(disk: &Disk) -> DiskSample {
    DiskSample {
        #[cfg(any(target_os = "linux", test))]
        source: disk.name().to_owned(),
        file_system: disk.file_system().to_owned(),
        #[cfg(any(target_os = "linux", test))]
        mount_point: disk.mount_point().to_owned(),
        total: disk.total_space(),
        free: disk_free_space(disk),
        #[cfg(any(target_os = "linux", test))]
        device_id: disk_device_id(disk.mount_point()),
    }
}

#[cfg(target_os = "linux")]
fn disk_free_space(disk: &Disk) -> u64 {
    fs2::free_space(disk.mount_point()).unwrap_or_else(|_| disk.available_space())
}

#[cfg(not(target_os = "linux"))]
fn disk_free_space(disk: &Disk) -> u64 {
    disk.available_space()
}

#[cfg(target_os = "linux")]
fn disk_device_id(mount_point: &Path) -> Option<u64> {
    std::fs::metadata(mount_point)
        .ok()
        .map(|metadata| metadata.dev())
}

#[cfg(all(not(target_os = "linux"), test))]
fn disk_device_id(_mount_point: &Path) -> Option<u64> {
    None
}

fn aggregate_disk_metrics(disks: &[DiskSample]) -> (i64, i64) {
    #[cfg(target_os = "linux")]
    {
        return aggregate_linux_disk_metrics(disks);
    }

    #[cfg(not(target_os = "linux"))]
    aggregate_standard_disk_metrics(disks, cfg!(target_os = "macos"))
}

#[cfg(any(not(target_os = "linux"), test))]
fn aggregate_standard_disk_metrics(
    disks: &[DiskSample],
    deduplicate_shared_apfs: bool,
) -> (i64, i64) {
    let mut seen_apfs_capacities = HashSet::new();
    sum_disk_metrics(disks.iter().filter(|disk| {
        !deduplicate_shared_apfs
            || disk.file_system != "apfs"
            || seen_apfs_capacities.insert((disk.total, disk.free))
    }))
}

#[cfg(any(target_os = "linux", test))]
fn aggregate_linux_disk_metrics(disks: &[DiskSample]) -> (i64, i64) {
    let mut unique = HashMap::<LinuxDiskIdentity, &DiskSample>::new();
    let mut root_container_layer = None;

    for disk in disks {
        if disk.total == 0 {
            continue;
        }
        if is_linux_container_layer(disk) {
            if disk.mount_point == Path::new("/") {
                root_container_layer = Some(disk);
            }
            continue;
        }
        if is_linux_ignored_disk(disk) {
            continue;
        }

        let identity = linux_disk_identity(disk);
        unique
            .entry(identity)
            .and_modify(|current| {
                if disk.total > current.total
                    || (disk.total == current.total && disk.free > current.free)
                {
                    *current = disk;
                }
            })
            .or_insert(disk);
    }

    if unique.is_empty() {
        return sum_disk_metrics(root_container_layer.into_iter());
    }
    sum_disk_metrics(unique.into_values())
}

#[cfg(any(target_os = "linux", test))]
fn linux_disk_identity(disk: &DiskSample) -> LinuxDiskIdentity {
    if disk.file_system == "zfs" {
        let pool = disk
            .source
            .to_string_lossy()
            .split('/')
            .next()
            .map(OsString::from)
            .filter(|pool| !pool.is_empty());
        if let Some(pool) = pool {
            return LinuxDiskIdentity::ZfsPool(pool);
        }
    }
    if let Some(device_id) = disk.device_id {
        return LinuxDiskIdentity::Device(device_id);
    }
    if !disk.source.is_empty() && disk.source != "none" {
        return LinuxDiskIdentity::Source(disk.source.clone());
    }
    LinuxDiskIdentity::MountPoint(disk.mount_point.clone())
}

#[cfg(any(target_os = "linux", test))]
fn is_linux_container_layer(disk: &DiskSample) -> bool {
    matches!(
        disk.file_system.to_string_lossy().as_ref(),
        "overlay" | "overlay2" | "aufs" | "unionfs" | "fuse-overlayfs" | "fuse.overlayfs"
    ) || is_container_runtime_root(&disk.mount_point)
}

#[cfg(any(target_os = "linux", test))]
fn is_container_runtime_root(mount_point: &Path) -> bool {
    let path = mount_point.to_string_lossy();
    (path.starts_with("/var/lib/docker/overlay2/") && path.ends_with("/merged"))
        || (path.starts_with("/var/lib/docker/aufs/mnt/"))
        || (path.starts_with("/var/lib/docker/devicemapper/mnt/") && path.ends_with("/rootfs"))
        || (path.starts_with("/var/lib/containers/storage/overlay/") && path.ends_with("/merged"))
        || (path.starts_with("/run/containerd/io.containerd.runtime.v2.task/")
            && path.ends_with("/rootfs"))
        || (path
            .starts_with("/var/lib/containerd/io.containerd.snapshotter.v1.overlayfs/snapshots/")
            && path.ends_with("/fs"))
}

#[cfg(any(target_os = "linux", test))]
fn is_linux_ignored_disk(disk: &DiskSample) -> bool {
    let file_system = disk.file_system.to_string_lossy();
    if matches!(
        file_system.as_ref(),
        "rootfs"
            | "sysfs"
            | "proc"
            | "devtmpfs"
            | "devpts"
            | "tmpfs"
            | "ramfs"
            | "cgroup"
            | "cgroup2"
            | "pstore"
            | "securityfs"
            | "debugfs"
            | "tracefs"
            | "configfs"
            | "fusectl"
            | "mqueue"
            | "hugetlbfs"
            | "autofs"
            | "binfmt_misc"
            | "efivarfs"
            | "nsfs"
            | "squashfs"
            | "iso9660"
            | "udf"
            | "rpc_pipefs"
            | "nfs"
            | "nfs4"
            | "cifs"
            | "smb3"
            | "ceph"
            | "glusterfs"
            | "lustre"
            | "9p"
            | "virtiofs"
            | "fuse.sshfs"
            | "fuse.rclone"
            | "davfs"
            | "davfs2"
    ) {
        return true;
    }

    let source = disk.source.to_string_lossy();
    source.starts_with("/dev/loop")
        || source.starts_with("/dev/ram")
        || source.starts_with("/dev/zram")
}

fn sum_disk_metrics<'a>(disks: impl Iterator<Item = &'a DiskSample>) -> (i64, i64) {
    let (total, free) = disks.fold((0_i64, 0_i64), |(total, free), disk| {
        let disk_total = disk.total.min(i64::MAX as u64) as i64;
        let disk_free = disk.free.min(disk.total).min(i64::MAX as u64) as i64;
        (
            total.saturating_add(disk_total),
            free.saturating_add(disk_free),
        )
    });
    (total.saturating_sub(free), total)
}

impl GpuCollector {
    fn new() -> Self {
        Self {
            #[cfg(target_os = "windows")]
            windows: WindowsGpuCollector::new(),
        }
    }

    fn sample(&mut self, _system_memory_total: i64) -> GpuMetrics {
        let mut samples = collect_nvidia_samples();

        #[cfg(target_os = "windows")]
        samples.extend(self.windows.sample(!samples.is_empty()));

        #[cfg(target_os = "linux")]
        samples.extend(collect_linux_drm_samples(!samples.is_empty()));

        #[cfg(target_os = "macos")]
        if let Some(sample) = collect_macos_apple_gpu_sample(_system_memory_total) {
            samples.push(sample);
        }

        aggregate_gpu_samples(&samples)
    }
}

fn collect_nvidia_samples() -> Vec<GpuSample> {
    let args = [
        "--query-gpu=utilization.gpu,memory.used,memory.total",
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
        Command::new(command)
            .args(args)
            .output()
            .ok()
            .filter(|output| output.status.success())
    }) else {
        return Vec::new();
    };

    parse_nvidia_smi(&String::from_utf8_lossy(&output.stdout))
}

fn parse_nvidia_smi(output: &str) -> Vec<GpuSample> {
    output
        .lines()
        .filter_map(|line| {
            let mut fields = line.split(',').map(str::trim);
            let percent = parse_number(fields.next()?);
            let memory_used = mib_to_bytes(parse_integer(fields.next()?));
            let memory_total = mib_to_bytes(parse_integer(fields.next()?));

            (percent.is_some() || memory_used.is_some() || memory_total.is_some()).then_some(
                GpuSample {
                    percent,
                    memory_used,
                    memory_total,
                },
            )
        })
        .collect()
}

fn parse_number(value: &str) -> Option<f64> {
    value.parse::<f64>().ok().filter(|value| value.is_finite())
}

fn parse_integer(value: &str) -> Option<i64> {
    value.parse::<i64>().ok().filter(|value| *value >= 0)
}

fn mib_to_bytes(value: Option<i64>) -> Option<i64> {
    value?.checked_mul(1024 * 1024)
}

fn aggregate_gpu_samples(samples: &[GpuSample]) -> GpuMetrics {
    let percentages = samples
        .iter()
        .filter_map(|sample| sample.percent)
        .collect::<Vec<_>>();
    let percent = (!percentages.is_empty())
        .then(|| (percentages.iter().sum::<f64>() / percentages.len() as f64).clamp(0.0, 100.0));

    GpuMetrics {
        percent,
        memory_used: sum_optional(samples.iter().map(|sample| sample.memory_used)),
        memory_total: sum_optional(samples.iter().map(|sample| sample.memory_total)),
    }
}

fn sum_optional(values: impl Iterator<Item = Option<i64>>) -> Option<i64> {
    let values = values.flatten().collect::<Vec<_>>();
    (!values.is_empty()).then(|| values.into_iter().sum())
}

#[cfg(target_os = "windows")]
struct WindowsGpuCollector {
    performance_query: Option<WindowsGpuPerformanceQuery>,
    adapters: Vec<WindowsGpuAdapter>,
}

#[cfg(target_os = "windows")]
struct WindowsGpuPerformanceQuery {
    query: isize,
    engine_counter: isize,
    memory_counter: Option<isize>,
}

#[cfg(target_os = "windows")]
struct WindowsGpuAdapter {
    id: String,
    vendor_id: u32,
    memory_total: Option<i64>,
}

#[cfg(target_os = "windows")]
#[derive(Default)]
struct WindowsGpuPerformanceSample {
    percentages: HashMap<String, f64>,
    memory_used: HashMap<String, i64>,
}

#[cfg(target_os = "windows")]
impl WindowsGpuCollector {
    fn new() -> Self {
        Self {
            performance_query: WindowsGpuPerformanceQuery::new(),
            adapters: enumerate_windows_gpu_adapters(),
        }
    }

    fn sample(&mut self, skip_nvidia: bool) -> Vec<GpuSample> {
        let WindowsGpuPerformanceSample {
            mut percentages,
            mut memory_used,
        } = self
            .performance_query
            .as_mut()
            .map(WindowsGpuPerformanceQuery::sample)
            .unwrap_or_default();
        let has_engine_samples = !percentages.is_empty();
        let mut samples = Vec::new();
        let mut selected_adapter_count = 0;

        for adapter in &self.adapters {
            if skip_nvidia && adapter.vendor_id == 0x10de {
                continue;
            }
            selected_adapter_count += 1;

            let percent = percentages.remove(&adapter.id);
            if has_engine_samples && percent.is_none() {
                continue;
            }
            let memory_used = memory_used.remove(&adapter.id);
            let memory_total = adapter.memory_total;

            if percent.is_some() || memory_used.is_some() || memory_total.is_some() {
                samples.push(GpuSample {
                    percent,
                    memory_used,
                    memory_total,
                });
            }
        }

        if selected_adapter_count == 0 {
            samples.extend(percentages.into_values().map(|percent| GpuSample {
                percent: Some(percent),
                ..GpuSample::default()
            }));
        }

        samples
    }
}

#[cfg(target_os = "windows")]
impl WindowsGpuPerformanceQuery {
    fn new() -> Option<Self> {
        let mut query = 0;
        if unsafe { PdhOpenQueryW(PCWSTR::null(), 0, &mut query) } != 0 {
            return None;
        }

        let mut engine_counter = 0;
        if unsafe {
            PdhAddEnglishCounterW(
                query,
                w!("\\GPU Engine(*)\\Utilization Percentage"),
                0,
                &mut engine_counter,
            )
        } != 0
        {
            unsafe { PdhCloseQuery(query) };
            return None;
        }

        let mut memory_counter = 0;
        let memory_counter = (unsafe {
            PdhAddEnglishCounterW(
                query,
                w!("\\GPU Adapter Memory(*)\\Dedicated Usage"),
                0,
                &mut memory_counter,
            )
        } == 0)
            .then_some(memory_counter);

        let performance_query = Self {
            query,
            engine_counter,
            memory_counter,
        };
        if unsafe { PdhCollectQueryData(performance_query.query) } != 0 {
            return None;
        }
        Some(performance_query)
    }

    fn sample(&mut self) -> WindowsGpuPerformanceSample {
        if unsafe { PdhCollectQueryData(self.query) } != 0 {
            return WindowsGpuPerformanceSample::default();
        }

        WindowsGpuPerformanceSample {
            percentages: aggregate_windows_engine_samples(formatted_counter_values(
                self.engine_counter,
            )),
            memory_used: self
                .memory_counter
                .map(formatted_counter_values)
                .map(aggregate_windows_memory_samples)
                .unwrap_or_default(),
        }
    }
}

#[cfg(target_os = "windows")]
impl Drop for WindowsGpuPerformanceQuery {
    fn drop(&mut self) {
        unsafe { PdhCloseQuery(self.query) };
    }
}

#[cfg(target_os = "windows")]
fn formatted_counter_values(counter: isize) -> Vec<(String, f64)> {
    for _ in 0..3 {
        let mut buffer_size = 0;
        let mut item_count = 0;
        let status = unsafe {
            PdhGetFormattedCounterArrayW(
                counter,
                PDH_FMT_DOUBLE,
                &mut buffer_size,
                &mut item_count,
                None,
            )
        };
        if status == 0 && item_count == 0 {
            return Vec::new();
        }
        if status != PDH_MORE_DATA || buffer_size == 0 {
            return Vec::new();
        }

        let word_count = (buffer_size as usize).div_ceil(size_of::<usize>());
        let mut buffer = vec![0_usize; word_count];
        let items = buffer.as_mut_ptr().cast::<PDH_FMT_COUNTERVALUE_ITEM_W>();
        let status = unsafe {
            PdhGetFormattedCounterArrayW(
                counter,
                PDH_FMT_DOUBLE,
                &mut buffer_size,
                &mut item_count,
                Some(items),
            )
        };
        if status == PDH_MORE_DATA {
            continue;
        }
        if status != 0 {
            return Vec::new();
        }

        let items = unsafe { std::slice::from_raw_parts(items, item_count as usize) };
        return items
            .iter()
            .filter_map(|item| {
                if !matches!(
                    item.FmtValue.CStatus,
                    PDH_CSTATUS_VALID_DATA | PDH_CSTATUS_NEW_DATA
                ) {
                    return None;
                }
                let name = unsafe { item.szName.to_string().ok()? };
                let value = unsafe { item.FmtValue.Anonymous.doubleValue };
                value.is_finite().then_some((name, value))
            })
            .collect();
    }

    Vec::new()
}

#[cfg(target_os = "windows")]
fn aggregate_windows_engine_samples(
    samples: impl IntoIterator<Item = (String, f64)>,
) -> HashMap<String, f64> {
    let mut engines = HashMap::<(String, String), f64>::new();
    for (name, value) in samples {
        if !value.is_finite() || value < 0.0 {
            continue;
        }
        let name = name.to_ascii_lowercase();
        let Some((adapter_id, engine_id)) = windows_engine_ids(&name) else {
            continue;
        };
        *engines.entry((adapter_id, engine_id)).or_default() += value;
    }

    let mut adapters = HashMap::<String, f64>::new();
    for ((adapter_id, _), percent) in engines {
        let percent = percent.clamp(0.0, 100.0);
        adapters
            .entry(adapter_id)
            .and_modify(|current| *current = current.max(percent))
            .or_insert(percent);
    }
    adapters
}

#[cfg(target_os = "windows")]
fn aggregate_windows_memory_samples(
    samples: impl IntoIterator<Item = (String, f64)>,
) -> HashMap<String, i64> {
    let mut adapters = HashMap::<String, i64>::new();
    for (name, value) in samples {
        if !value.is_finite() || value < 0.0 || value > i64::MAX as f64 {
            continue;
        }
        let name = name.to_ascii_lowercase();
        let Some(adapter_id) = windows_adapter_id(&name) else {
            continue;
        };
        let value = value as i64;
        adapters
            .entry(adapter_id)
            .and_modify(|current| *current = current.saturating_add(value))
            .or_insert(value);
    }
    adapters
}

#[cfg(target_os = "windows")]
fn windows_engine_ids(name: &str) -> Option<(String, String)> {
    let luid_start = name.find("luid_")?;
    let engtype_start = name[luid_start..].find("_engtype_")? + luid_start;
    let engine_id = &name[luid_start..engtype_start];
    engine_id.rfind("_eng_")?;

    Some((windows_adapter_id(name)?, engine_id.to_owned()))
}

#[cfg(target_os = "windows")]
fn windows_adapter_id(name: &str) -> Option<String> {
    let luid_start = name.find("luid_")?;
    let phys_start = name[luid_start..].find("_phys_")? + luid_start;
    Some(name[luid_start..phys_start].to_owned())
}

#[cfg(target_os = "windows")]
fn enumerate_windows_gpu_adapters() -> Vec<WindowsGpuAdapter> {
    let Ok(factory) = (unsafe { CreateDXGIFactory1::<IDXGIFactory1>() }) else {
        return Vec::new();
    };
    let mut adapters = Vec::new();

    for index in 0.. {
        let Ok(adapter) = (unsafe { factory.EnumAdapters1(index) }) else {
            break;
        };
        let mut description = DXGI_ADAPTER_DESC1::default();
        if unsafe { adapter.GetDesc1(&mut description) }.is_err()
            || description.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 != 0
            || description.VendorId == 0
        {
            continue;
        }

        adapters.push(WindowsGpuAdapter {
            id: format!(
                "luid_0x{:08x}_0x{:08x}",
                description.AdapterLuid.HighPart as u32, description.AdapterLuid.LowPart
            ),
            vendor_id: description.VendorId,
            memory_total: i64::try_from(description.DedicatedVideoMemory)
                .ok()
                .filter(|total| *total > 0),
        });
    }

    adapters
}

#[cfg(target_os = "linux")]
fn collect_linux_drm_samples(skip_nvidia: bool) -> Vec<GpuSample> {
    let Ok(cards) = std::fs::read_dir("/sys/class/drm") else {
        return Vec::new();
    };

    cards
        .filter_map(Result::ok)
        .filter(|entry| {
            entry
                .file_name()
                .to_str()
                .is_some_and(|name| name.starts_with("card") && !name.contains('-'))
        })
        .filter_map(|entry| {
            let device = entry.path().join("device");
            if skip_nvidia && read_trimmed(device.join("vendor")).as_deref() == Some("0x10de") {
                return None;
            }

            let sample = GpuSample {
                percent: read_trimmed(device.join("gpu_busy_percent"))
                    .as_deref()
                    .and_then(parse_number),
                memory_used: read_trimmed(device.join("mem_info_vram_used"))
                    .as_deref()
                    .and_then(parse_integer),
                memory_total: read_trimmed(device.join("mem_info_vram_total"))
                    .as_deref()
                    .and_then(parse_integer),
            };
            (sample.percent.is_some()
                || sample.memory_used.is_some()
                || sample.memory_total.is_some())
            .then_some(sample)
        })
        .collect()
}

#[cfg(target_os = "linux")]
fn read_trimmed(path: impl AsRef<std::path::Path>) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|value| value.trim().to_owned())
}

#[cfg(target_os = "macos")]
fn collect_macos_apple_gpu_sample(system_memory_total: i64) -> Option<GpuSample> {
    let output = Command::new("/usr/sbin/ioreg")
        .args(["-r", "-d", "1", "-w", "0", "-c", "AGXAccelerator"])
        .output()
        .ok()
        .filter(|output| output.status.success())?;

    parse_macos_ioreg(
        &String::from_utf8_lossy(&output.stdout),
        system_memory_total,
    )
}

#[cfg(any(target_os = "macos", test))]
fn parse_macos_ioreg(output: &str, system_memory_total: i64) -> Option<GpuSample> {
    let percent = parse_ioreg_integer(output, "Device Utilization %").map(|value| value as f64);
    let memory_used = parse_ioreg_integer(output, "In use system memory");

    (percent.is_some() || memory_used.is_some()).then_some(GpuSample {
        percent,
        memory_used,
        memory_total: (system_memory_total > 0).then_some(system_memory_total),
    })
}

#[cfg(any(target_os = "macos", test))]
fn parse_ioreg_integer(output: &str, key: &str) -> Option<i64> {
    let property = format!("\"{key}\"=");
    let value = output.split_once(&property)?.1;
    let digits = value
        .trim_start()
        .chars()
        .take_while(char::is_ascii_digit)
        .collect::<String>();
    parse_integer(&digits)
}

#[cfg(test)]
mod tests {
    use super::{
        DiskSample, GpuSample, aggregate_gpu_samples, aggregate_linux_disk_metrics,
        aggregate_standard_disk_metrics, parse_macos_ioreg, parse_nvidia_smi,
    };

    #[cfg(target_os = "windows")]
    use super::{
        WindowsGpuCollector, aggregate_windows_engine_samples, aggregate_windows_memory_samples,
    };

    fn disk(
        source: &str,
        file_system: &str,
        mount_point: &str,
        total: u64,
        free: u64,
        device_id: Option<u64>,
    ) -> DiskSample {
        DiskSample {
            source: source.into(),
            file_system: file_system.into(),
            mount_point: mount_point.into(),
            total,
            free,
            device_id,
        }
    }

    #[test]
    fn deduplicates_macos_apfs_volumes_that_share_capacity() {
        let disks = [
            disk("disk3s1", "apfs", "/", 1_000, 400, None),
            disk("disk3s2", "apfs", "/System", 1_000, 400, None),
            disk("disk3s3", "apfs", "/Data", 1_000, 250, None),
            disk("disk4s1", "hfs", "/Volumes/data", 2_000, 1_000, None),
        ];

        let (used, total) = aggregate_standard_disk_metrics(&disks, true);

        assert_eq!(used, 2_350);
        assert_eq!(total, 4_000);
    }

    #[test]
    fn keeps_all_volumes_when_apfs_deduplication_is_disabled() {
        let disks = [
            disk("disk3s1", "apfs", "/", 1_000, 400, None),
            disk("disk3s2", "apfs", "/System", 1_000, 400, None),
        ];

        let (used, total) = aggregate_standard_disk_metrics(&disks, false);

        assert_eq!(used, 1_200);
        assert_eq!(total, 2_000);
    }

    #[test]
    fn linux_ignores_docker_layers_and_deduplicates_bind_mounts() {
        let disks = [
            disk("/dev/vda1", "ext4", "/", 10_000, 4_000, Some(1)),
            disk(
                "/dev/vdb1",
                "xfs",
                "/var/lib/docker",
                20_000,
                8_000,
                Some(2),
            ),
            disk(
                "/dev/vdb1",
                "xfs",
                "/var/lib/docker/volumes/app/_data",
                20_000,
                8_000,
                Some(2),
            ),
            disk(
                "overlay",
                "overlay",
                "/var/lib/docker/overlay2/abc/merged",
                20_000,
                8_000,
                Some(20),
            ),
            disk(
                "/dev/mapper/docker-container",
                "ext4",
                "/var/lib/docker/devicemapper/mnt/abc/rootfs",
                20_000,
                8_000,
                Some(21),
            ),
        ];

        assert_eq!(aggregate_linux_disk_metrics(&disks), (18_000, 30_000));
    }

    #[test]
    fn linux_keeps_distinct_local_filesystems_with_equal_capacities() {
        let disks = [
            disk("/dev/vda1", "ext4", "/", 10_000, 4_000, Some(1)),
            disk("/dev/vdb1", "xfs", "/data", 10_000, 4_000, Some(2)),
        ];

        assert_eq!(aggregate_linux_disk_metrics(&disks), (12_000, 20_000));
    }

    #[test]
    fn linux_excludes_memory_loop_and_remote_filesystems() {
        let disks = [
            disk("/dev/vda1", "ext4", "/", 10_000, 4_000, Some(1)),
            disk("tmpfs", "tmpfs", "/tmp", 2_000, 1_500, Some(10)),
            disk("/dev/loop0", "ext4", "/mnt/image", 3_000, 1_000, Some(11)),
            disk("server:/data", "nfs4", "/mnt/nfs", 20_000, 5_000, Some(12)),
        ];

        assert_eq!(aggregate_linux_disk_metrics(&disks), (6_000, 10_000));
    }

    #[test]
    fn linux_uses_root_overlay_when_running_inside_a_container() {
        let disks = [
            disk("overlay", "overlay", "/", 10_000, 4_000, Some(20)),
            disk("overlay", "overlay", "/workspace", 10_000, 4_000, Some(21)),
        ];

        assert_eq!(aggregate_linux_disk_metrics(&disks), (6_000, 10_000));
    }

    #[test]
    fn linux_groups_zfs_datasets_by_pool() {
        let disks = [
            disk("tank/root", "zfs", "/", 10_000, 4_000, Some(30)),
            disk("tank/data", "zfs", "/data", 8_000, 3_000, Some(31)),
            disk("backup", "zfs", "/backup", 5_000, 2_000, Some(32)),
        ];

        assert_eq!(aggregate_linux_disk_metrics(&disks), (9_000, 15_000));
    }

    #[test]
    fn disk_usage_is_calculated_from_free_blocks() {
        let disks = [disk("/dev/vda1", "ext4", "/", 10_000, 4_000, Some(1))];

        assert_eq!(aggregate_linux_disk_metrics(&disks), (6_000, 10_000));
    }

    #[test]
    fn parses_nvidia_smi_metrics_and_converts_mib_to_bytes() {
        let samples = parse_nvidia_smi("25, 1024, 8192\n75, 2048, 16384\n");
        let metrics = aggregate_gpu_samples(&samples);

        assert_eq!(metrics.percent, Some(50.0));
        assert_eq!(metrics.memory_used, Some(3 * 1024 * 1024 * 1024));
        assert_eq!(metrics.memory_total, Some(24 * 1024 * 1024 * 1024));
    }

    #[test]
    fn ignores_unavailable_nvidia_values_without_dropping_valid_fields() {
        let samples = parse_nvidia_smi("N/A, 512, 4096\n");
        let metrics = aggregate_gpu_samples(&samples);

        assert_eq!(metrics.percent, None);
        assert_eq!(metrics.memory_used, Some(512 * 1024 * 1024));
        assert_eq!(metrics.memory_total, Some(4096 * 1024 * 1024));
    }

    #[test]
    fn clamps_aggregated_gpu_utilization_to_percentage_range() {
        let metrics = aggregate_gpu_samples(&[
            GpuSample {
                percent: Some(120.0),
                ..GpuSample::default()
            },
            GpuSample {
                percent: Some(100.0),
                ..GpuSample::default()
            },
        ]);

        assert_eq!(metrics.percent, Some(100.0));
    }

    #[test]
    fn parses_apple_silicon_gpu_metrics_with_unified_memory_total() {
        let output = r#"
            "PerformanceStatistics" = {
                "In use system memory (driver)"=0,
                "Alloc system memory"=6768050176,
                "Device Utilization %"=42,
                "In use system memory"=2510995456
            }
        "#;

        let sample = parse_macos_ioreg(output, 17_179_869_184).unwrap();

        assert_eq!(sample.percent, Some(42.0));
        assert_eq!(sample.memory_used, Some(2_510_995_456));
        assert_eq!(sample.memory_total, Some(17_179_869_184));
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn aggregates_windows_process_counters_by_adapter_and_busiest_engine() {
        let metrics = aggregate_windows_engine_samples([
            (
                "pid_1_luid_0x00000000_0x00000001_phys_0_eng_0_engtype_3D".to_owned(),
                20.0,
            ),
            (
                "pid_2_luid_0x00000000_0x00000001_phys_0_eng_0_engtype_3D".to_owned(),
                25.0,
            ),
            (
                "pid_2_luid_0x00000000_0x00000001_phys_0_eng_1_engtype_Copy".to_owned(),
                70.0,
            ),
            (
                "pid_3_luid_0x00000000_0x00000002_phys_0_eng_0_engtype_3D".to_owned(),
                15.0,
            ),
        ]);

        assert_eq!(metrics.get("luid_0x00000000_0x00000001"), Some(&70.0));
        assert_eq!(metrics.get("luid_0x00000000_0x00000002"), Some(&15.0));
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn aggregates_windows_dedicated_memory_by_adapter() {
        let metrics = aggregate_windows_memory_samples([
            (
                "luid_0x00000000_0x00000001_phys_0".to_owned(),
                1_500_000_000.0,
            ),
            (
                "luid_0x00000000_0x00000001_phys_1".to_owned(),
                500_000_000.0,
            ),
        ]);

        assert_eq!(
            metrics.get("luid_0x00000000_0x00000001"),
            Some(&2_000_000_000)
        );
    }

    #[cfg(target_os = "windows")]
    #[test]
    #[ignore = "hardware-specific diagnostic"]
    fn reports_live_windows_gpu_metrics() {
        let mut collector = WindowsGpuCollector::new();
        std::thread::sleep(std::time::Duration::from_secs(1));
        let samples = collector.sample(false);

        eprintln!("live Windows GPU samples: {samples:?}");
        assert!(samples.iter().any(|sample| sample.percent.is_some()));
    }
}
