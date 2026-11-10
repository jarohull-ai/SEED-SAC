//! Floating egui HUD for SEED SAC.
//!
//! The sentinel thread publishes a [`DisplayState`] snapshot into a
//! [`SharedDisplayState`]; the HUD loads it lock-free on every frame.

pub mod app;
pub mod indicators;
pub mod notifications;

use arc_swap::ArcSwap;
use std::sync::Arc;

pub use app::{SacApp, UserAction};

#[derive(Debug, Clone)]
pub struct DisplayState {
    pub cpu_pct: f32,
    pub ram_used_bytes: u64,
    pub ram_total_bytes: u64,
    pub vram_used_bytes: Option<u64>,
    pub vram_total_bytes: Option<u64>,
    pub system_state: SystemState,
    pub pending_alerts: Vec<Alert>,
}

impl Default for DisplayState {
    fn default() -> Self {
        Self {
            cpu_pct: 0.0,
            ram_used_bytes: 0,
            ram_total_bytes: 0,
            vram_used_bytes: None,
            vram_total_bytes: None,
            system_state: SystemState::Green,
            pending_alerts: Vec::new(),
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
