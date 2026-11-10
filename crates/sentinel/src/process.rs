//! Per-process memory deltas between consecutive snapshots.

use std::collections::HashMap;
use std::time::Instant;

use crate::SystemSnapshot;

/// Memory growth of a single process since the previous poll.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessDelta {
    pub pid: u32,
    pub name: String,
    pub rss_bytes: u64,
    pub delta_bytes_per_sec: i64,
}

/// Remembers the previous RSS of every process to compute growth rates.
pub struct ProcessTracker {
    previous_rss: HashMap<u32, u64>,
    last_poll: Option<Instant>,
}

impl ProcessTracker {
    pub fn new() -> Self {
        Self {
            previous_rss: HashMap::new(),
            last_poll: None,
        }
    }

    /// Update the tracked RSS values and return deltas for the current process
    /// set. The first call reports zero deltas because no interval is known yet.
    pub fn update(&mut self, snapshot: &SystemSnapshot) -> Vec<ProcessDelta> {
        let now = Instant::now();
        let elapsed_secs = self
            .last_poll
            .map(|last| now.duration_since(last).as_secs_f64())
            .filter(|secs| *secs > 0.0);

        let mut deltas = Vec::with_capacity(snapshot.processes.len());
        for process in &snapshot.processes {
            let delta_bytes_per_sec = match (self.previous_rss.get(&process.pid), elapsed_secs) {
                (Some(previous), Some(secs)) => {
                    let delta = process.rss_bytes as i128 - *previous as i128;
                    (delta as f64 / secs) as i64
                }
                _ => 0,
            };

            deltas.push(ProcessDelta {
                pid: process.pid,
                name: process.name.clone(),
                rss_bytes: process.rss_bytes,
                delta_bytes_per_sec,
            });
        }

        self.previous_rss = snapshot
            .processes
            .iter()
            .map(|process| (process.pid, process.rss_bytes))
            .collect();
        self.last_poll = Some(now);

        deltas
    }
}

impl Default for ProcessTracker {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use std::thread;
    use std::time::Duration;

    use super::*;
    use crate::{ProcessInfo, SystemSnapshot};

    fn snapshot(rss: u64) -> SystemSnapshot {
        SystemSnapshot {
            cpu_usage_pct: 0.0,
            ram_used_bytes: 0,
            ram_total_bytes: 1,
            vram_used_bytes: None,
            vram_total_bytes: None,
            processes: vec![ProcessInfo {
                pid: 42,
                name: "cargo".to_owned(),
                rss_bytes: rss,
                cpu_pct: 0.0,
            }],
        }
    }

    #[test]
    fn first_poll_has_no_delta_and_growth_is_positive() {
        let mut tracker = ProcessTracker::new();

        let first = tracker.update(&snapshot(1_000));
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].delta_bytes_per_sec, 0);
        assert_eq!(first[0].rss_bytes, 1_000);

        thread::sleep(Duration::from_millis(20));
        let second = tracker.update(&snapshot(2_000));
        assert_eq!(second.len(), 1);
        assert!(second[0].delta_bytes_per_sec > 0);
    }

    #[test]
    fn stale_processes_are_forgotten() {
        let mut tracker = ProcessTracker::new();
        tracker.update(&snapshot(1_000));
        let empty = tracker.update(&SystemSnapshot {
            processes: Vec::new(),
            ..snapshot(0)
        });
        assert!(empty.is_empty());
        assert_eq!(tracker.update(&snapshot(1_000))[0].delta_bytes_per_sec, 0);
    }
}
