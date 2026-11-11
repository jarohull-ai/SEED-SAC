//! Floating egui HUD for SEED SAC.
//!
//! The sentinel thread publishes a [`DisplayState`] snapshot into a
//! [`SharedDisplayState`]; the HUD loads it lock-free on every frame.

pub mod app;
pub mod indicators;
pub mod notifications;

use arc_swap::ArcSwap;
use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Instant;

pub use app::{SacApp, UserAction};

/// A command typed by the user in the HUD input bar.
#[derive(Debug, Clone)]
pub struct UserCommand(pub String);

#[derive(Debug, Clone)]
pub struct DisplayState {
    /// Whether this snapshot came from the sentinel rather than the initial
    /// placeholder state.
    pub telemetry_available: bool,
    pub cpu_pct: f32,
    pub ram_used_bytes: u64,
    pub ram_total_bytes: u64,
    pub vram_used_bytes: Option<u64>,
    pub vram_total_bytes: Option<u64>,
    pub system_state: SystemState,
    pub pending_alerts: Vec<Alert>,
    /// Feedback from the last executed command.
    pub command_response: Option<String>,
    /// Persistent operator-visible history of sentinel, router, DLP and L1 events.
    pub console: VecDeque<ConsoleEntry>,
    pub thresholds: DisplayThresholds,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogLevel {
    Info,
    L1Response,
    Warning,
    Alert,
}

#[derive(Debug, Clone)]
pub struct ConsoleEntry {
    pub timestamp: Instant,
    pub source: &'static str,
    pub level: LogLevel,
    pub text: String,
}

/// Policy thresholds used to colour the telemetry indicators.
///
/// The sentinel remains the policy authority; this is only a copy for
/// truthful presentation in the HUD.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DisplayThresholds {
    pub cpu_yellow_pct: f32,
    pub ram_yellow_pct: f32,
    pub ram_red_pct: f32,
    pub vram_yellow_pct: f32,
    pub vram_red_pct: f32,
}

impl Default for DisplayThresholds {
    fn default() -> Self {
        Self {
            cpu_yellow_pct: 70.0,
            ram_yellow_pct: 75.0,
            ram_red_pct: 88.0,
            vram_yellow_pct: 80.0,
            vram_red_pct: 92.0,
        }
    }
}

impl Default for DisplayState {
    fn default() -> Self {
        Self {
            telemetry_available: false,
            cpu_pct: 0.0,
            ram_used_bytes: 0,
            ram_total_bytes: 0,
            vram_used_bytes: None,
            vram_total_bytes: None,
            system_state: SystemState::Green,
            pending_alerts: Vec::new(),
            command_response: None,
            console: VecDeque::new(),
            thresholds: DisplayThresholds::default(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SystemState {
    Green,
    Yellow,
    Red,
}

#[derive(Debug, Clone)]
pub struct Alert {
    pub pid: u32,
    pub process_name: String,
    pub message: String,
    pub actions: Vec<AlertAction>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AlertAction {
    Resume,
    Terminate,
}

pub type SharedDisplayState = Arc<ArcSwap<DisplayState>>;

pub fn new_shared_state() -> SharedDisplayState {
    Arc::new(ArcSwap::from_pointee(DisplayState::default()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_state_is_green_and_empty() {
        let shared = new_shared_state();
        let s = shared.load();
        assert_eq!(s.system_state, SystemState::Green);
        assert_eq!(s.cpu_pct, 0.0);
        assert!(s.pending_alerts.is_empty());
        assert!(s.vram_total_bytes.is_none());
        assert!(s.command_response.is_none());
    }

    #[test]
    fn store_replaces_snapshot() {
        let shared = new_shared_state();
        shared.store(Arc::new(DisplayState {
            cpu_pct: 42.0,
            system_state: SystemState::Red,
            ..DisplayState::default()
        }));
        assert_eq!(shared.load().system_state, SystemState::Red);
    }
}
