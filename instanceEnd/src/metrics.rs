use sysinfo::{Disks, Networks, System};

use crate::{models::MetricPayload, time::now_ts};

pub struct MetricsCollector {
    system: System,
    disks: Disks,
    networks: Networks,
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

        let disk_total = self
            .disks
            .iter()
            .map(|disk| disk.total_space() as i64)
            .sum::<i64>();
        let disk_available = self
            .disks
            .iter()
            .map(|disk| disk.available_space() as i64)
            .sum::<i64>();
        let disk_used = (disk_total - disk_available).max(0);

        let (network_rx, network_tx) = self.networks.iter().fold((0_i64, 0_i64), |acc, item| {
            (
                acc.0 + item.1.total_received() as i64,
                acc.1 + item.1.total_transmitted() as i64,
            )
        });

        let load_average = System::load_average();
        MetricPayload {
            ts: now_ts(),
            cpu_percent: self.system.global_cpu_info().cpu_usage() as f64,
            memory_used: self.system.used_memory() as i64,
            memory_total: self.system.total_memory() as i64,
            disk_used,
            disk_total,
            network_rx,
            network_tx,
            gpu_percent: None,
            gpu_memory_used: None,
            gpu_memory_total: None,
            uptime_seconds: System::uptime() as i64,
            load_average: Some(load_average.one),
        }
    }
}
