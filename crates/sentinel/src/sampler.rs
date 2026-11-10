//! CPU/RAM/process sampling built on `sysinfo`.

use sysinfo::{ProcessesToUpdate, System};

use crate::{ProcessInfo, SystemSnapshot};

/// How many fast ticks (CPU + RAM only) between full process table refreshes.
pub(crate) const PROCESS_REFRESH_EVERY: u32 = 4;

/// Stateful sampler. Reuse one instance so CPU deltas have a previous sample.
pub struct Sampler {
    system: System,
    tick: u32,
    last_processes: Vec<ProcessInfo>,
}

impl Sampler {
    pub fn new() -> Self {
        let mut system = System::new();
        system.refresh_cpu_usage();
        system.refresh_memory();
        system.refresh_processes(ProcessesToUpdate::All, true);

        let last_processes = collect_processes(&system);

        Self {
            system,
            tick: 0,
            last_processes,
        }
    }

    /// Refresh CPU and memory every tick; the process table only every
    /// [`PROCESS_REFRESH_EVERY`] ticks (~1 s at 250 ms interval) to keep the
    /// per-tick cost low on systems with hundreds of processes.
    pub fn poll(&mut self) -> SystemSnapshot {
        self.system.refresh_cpu_usage();
        self.system.refresh_memory();

        self.tick = self.tick.wrapping_add(1);
        if self.tick.is_multiple_of(PROCESS_REFRESH_EVERY) {
            self.system.refresh_processes(ProcessesToUpdate::All, true);
            self.last_processes = collect_processes(&self.system);
        }

        let (vram_used_bytes, vram_total_bytes) =
            crate::gpu::read_vram().map_or((None, None), |(used, total)| (Some(used), Some(total)));

        SystemSnapshot {
            cpu_usage_pct: self.system.global_cpu_usage(),
            ram_used_bytes: self.system.used_memory(),
            ram_total_bytes: self.system.total_memory(),
            vram_used_bytes,
            vram_total_bytes,
            processes: self.last_processes.clone(),
        }
    }
}

fn collect_processes(system: &System) -> Vec<ProcessInfo> {
    system
        .processes()
        .iter()
        .map(|(pid, process)| ProcessInfo {
            pid: pid.as_u32(),
            name: process.name().to_string_lossy().into_owned(),
            start_time: process.start_time(),
            rss_bytes: process.memory(),
            cpu_pct: process.cpu_usage(),
        })
        .collect()
}

impl Default for Sampler {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn poll_reports_memory_and_processes() {
        let mut sampler = Sampler::new();
        let snapshot = sampler.poll();

        assert!(snapshot.ram_total_bytes > 0);
        assert!(snapshot.ram_used_bytes <= snapshot.ram_total_bytes);
        assert!(!snapshot.processes.is_empty());
        match (snapshot.vram_used_bytes, snapshot.vram_total_bytes) {
            (Some(used), Some(total)) => assert!(used <= total),
            (None, None) => {}
            (Some(_), None) | (None, Some(_)) => {
                panic!("VRAM used and total values must be reported together")
            }
        }
    }

    #[test]
    fn process_refresh_cadence() {
        let mut sampler = Sampler::new();
        // tick starts at 0, first poll increments to 1 (no process refresh)
        let _ = sampler.poll();
        assert_eq!(sampler.tick, 1);
        // ticks 2,3 skip; tick 4 refreshes
        let _ = sampler.poll();
        let _ = sampler.poll();
        let snap = sampler.poll();
        assert_eq!(sampler.tick, 4);
        assert!(!snap.processes.is_empty());
    }
}
