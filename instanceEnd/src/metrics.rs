use std::process::Command;

#[cfg(target_os = "windows")]
use std::{collections::HashMap, mem::size_of};

use sysinfo::{Disks, Networks, System};

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

        let (disk_used, disk_total) = aggregate_disk_metrics(
            self.disks.iter().map(|disk| {
                (
                    disk.file_system() == "apfs",
                    disk.total_space(),
                    disk.available_space(),
                )
            }),
            cfg!(target_os = "macos"),
        );

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

fn aggregate_disk_metrics(
    disks: impl Iterator<Item = (bool, u64, u64)>,
    deduplicate_shared_apfs: bool,
) -> (i64, i64) {
    let mut seen_apfs_capacities = std::collections::HashSet::new();
    let (total, available) = disks
        .filter(|(is_apfs, total, available)| {
            !deduplicate_shared_apfs
                || !is_apfs
                || seen_apfs_capacities.insert((*total, *available))
        })
        .fold((0_i64, 0_i64), |(total, available), disk| {
            (
                total.saturating_add(disk.1.min(i64::MAX as u64) as i64),
                available.saturating_add(disk.2.min(i64::MAX as u64) as i64),
            )
        });

    ((total - available).max(0), total)
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
    let mut commands = vec![std::path::PathBuf::from("nvidia-smi")];

    #[cfg(target_os = "windows")]
    {
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
    }

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
        GpuSample, aggregate_disk_metrics, aggregate_gpu_samples, parse_macos_ioreg,
        parse_nvidia_smi,
    };

    #[cfg(target_os = "windows")]
    use super::{
        WindowsGpuCollector, aggregate_windows_engine_samples, aggregate_windows_memory_samples,
    };

    #[test]
    fn deduplicates_macos_apfs_volumes_that_share_capacity() {
        let disks = [
            (true, 1_000, 400),
            (true, 1_000, 400),
            (true, 1_000, 250),
            (false, 2_000, 1_000),
        ];

        let (used, total) = aggregate_disk_metrics(disks.into_iter(), true);

        assert_eq!(used, 2_350);
        assert_eq!(total, 4_000);
    }

    #[test]
    fn keeps_all_volumes_when_apfs_deduplication_is_disabled() {
        let disks = [(true, 1_000, 400), (true, 1_000, 400)];

        let (used, total) = aggregate_disk_metrics(disks.into_iter(), false);

        assert_eq!(used, 1_200);
        assert_eq!(total, 2_000);
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
