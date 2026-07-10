use std::process::Command;

use sysinfo::{Disks, Networks, System};

use crate::{models::MetricPayload, time::now_ts};

pub struct MetricsCollector {
    system: System,
    disks: Disks,
    networks: Networks,
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

impl MetricsCollector {
    pub fn new() -> Self {
        let mut system = System::new_all();
        system.refresh_all();
        Self {
            system,
            disks: Disks::new_with_refreshed_list(),
            networks: Networks::new_with_refreshed_list(),
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
        let gpu = collect_gpu_metrics(self.system.total_memory() as i64);
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

fn collect_gpu_metrics(_system_memory_total: i64) -> GpuMetrics {
    let samples = collect_nvidia_samples();

    #[cfg(target_os = "linux")]
    let samples = {
        let mut samples = samples;
        samples.extend(collect_linux_drm_samples(!samples.is_empty()));
        samples
    };

    #[cfg(target_os = "macos")]
    let samples = {
        let mut samples = samples;
        if let Some(sample) = collect_macos_apple_gpu_sample(_system_memory_total) {
            samples.push(sample);
        }
        samples
    };

    aggregate_gpu_samples(&samples)
}

fn collect_nvidia_samples() -> Vec<GpuSample> {
    let output = match Command::new("nvidia-smi")
        .args([
            "--query-gpu=utilization.gpu,memory.used,memory.total",
            "--format=csv,noheader,nounits",
        ])
        .output()
    {
        Ok(output) if output.status.success() => output,
        _ => return Vec::new(),
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
}
