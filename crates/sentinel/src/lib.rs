//! Hardware telemetry and OOM protection for SEED SAC.
//!
//! The sentinel polls CPU, RAM and (optionally) VRAM usage, tracks per-process
//! memory growth and decides whether mitigation is required before the kernel
//! OOM killer gets involved.

pub mod config;
pub mod gpu;
pub mod guard;
pub mod policy;
pub mod process;
pub mod sampler;

pub use config::Config;
pub use process::{ProcessDelta, ProcessTracker};

/// A single point-in-time view of the machine.
#[derive(Debug, Clone)]
pub struct SystemSnapshot {
    pub cpu_usage_pct: f32,
    pub ram_used_bytes: u64,
    pub ram_total_bytes: u64,
    pub vram_used_bytes: Option<u64>,
    pub vram_total_bytes: Option<u64>,
    pub processes: Vec<ProcessInfo>,
}

/// One process as reported by the sampler.
#[derive(Debug, Clone)]
pub struct ProcessInfo {
    pub pid: u32,
    pub name: String,
    pub rss_bytes: u64,
    pub cpu_pct: f32,
}

/// Overall resource state. Red wins over Yellow.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SystemState {
    Green,
    Yellow,
    Red,
}
