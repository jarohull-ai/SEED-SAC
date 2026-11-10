//! SEED SAC binary entry point.
//!
//! The process is split into two halves:
//!
//! * the **sentinel thread** samples the machine, evaluates the resource
//!   policy, runs mitigation actions and publishes a [`hud::DisplayState`]
//!   into a lock-free `ArcSwap`;
//! * the **main thread** runs the egui event loop, which only reads that
//!   snapshot and sends user decisions back over an `mpsc` channel.
//!
//! Keeping telemetry off the UI thread means a slow `/proc` read or a stalled
//! `renice` never freezes the window.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use tracing_subscriber::EnvFilter;

use hud::{Alert, AlertAction, DisplayState, SacApp, SharedDisplayState, UserAction};
use sentinel::config::Config;
use sentinel::guard::{self, GuardAction};
use sentinel::process::ProcessTracker;
use sentinel::sampler::Sampler;
use sentinel::{policy, SystemSnapshot, SystemState};

/// Smallest poll interval we accept, to keep a malformed config from turning
/// the sentinel into a busy loop.
const MIN_POLL_INTERVAL_MS: u64 = 50;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    init_tracing();

    let config = sentinel::config::load();
    let poll_interval = Duration::from_millis(
        config
            .sentinel
            .poll_interval_ms
            .max(MIN_POLL_INTERVAL_MS),
    );

    tracing::info!(
        poll_interval_ms = poll_interval.as_millis(),
        "starting SEED SAC"
    );

    let shared_state = hud::new_shared_state();
    let (action_tx, action_rx) = mpsc::channel::<UserAction>();
    let running = Arc::new(AtomicBool::new(true));

    let sentinel_handle = {
        let shared_state = Arc::clone(&shared_state);
        let running = Arc::clone(&running);
        thread::Builder::new()
            .name("sentinel".to_owned())
            .spawn(move || run_sentinel(config, shared_state, action_rx, running, poll_interval))
            .map_err(std::io::Error::other)?
    };

    let native_options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_title("SAC · System-Aware Copilot")
            .with_inner_size([600.0, 80.0])
            .with_min_inner_size([480.0, 64.0])
            .with_always_on_top()
            .with_decorations(true)
            .with_resizable(true),
        ..Default::default()
    };

    let eframe_result = eframe::run_native(
        "SAC · System-Aware Copilot",
        native_options,
        Box::new(move |_cc| Ok(Box::new(SacApp::new(shared_state, action_tx)))),
    );

    // The window is gone: tell the sentinel to stop and wait for it so the
    // process exits cleanly instead of leaving a detached sampler behind.
    running.store(false, Ordering::Release);
    let sentinel_panicked = sentinel_handle.join().is_err();

    eframe_result?;

    if sentinel_panicked {
        return Err("sentinel thread panicked".into());
    }

    tracing::info!("SEED SAC stopped");
    Ok(())
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .init();
}

/// Main loop of the background sentinel thread.
///
/// It publishes a fresh display snapshot on every tick, keeps frozen-process
/// alerts alive until the user resolves them, and serves HUD actions while
/// waiting for the next tick.
fn run_sentinel(
    config: Config,
    shared_state: SharedDisplayState,
    action_rx: Receiver<UserAction>,
    running: Arc<AtomicBool>,
    poll_interval: Duration,
) {
    let mut sampler = Sampler::new();
    let mut tracker = ProcessTracker::new();
    let mut alerts: BTreeMap<u32, Alert> = BTreeMap::new();

    while running.load(Ordering::Acquire) {
        let snapshot = sampler.poll();
        let deltas = tracker.update(&snapshot);
        let state = policy::evaluate(&snapshot, &config);
        let actions = guard::mitigate(state, &deltas, &config);

        for action in &actions {
            match guard::execute(action) {
                Ok(()) => {
                    if let GuardAction::Freeze { pid } = *action {
                        let name = process_name(&snapshot, pid);
                        tracing::warn!(pid, process = %name, "froze process to prevent OOM");
                        alerts.insert(
                            pid,
                            Alert {
                                pid,
                                process_name: name,
                                message: "Gwałtowny wzrost pamięci — proces zamrożony \
                                          (SIGSTOP). Wznów go lub zakończ."
                                    .to_owned(),
                                actions: vec![AlertAction::Resume, AlertAction::Terminate],
                            },
                        );
                    }
                }
                Err(err) => {
                    tracing::error!(?action, %err, "guard action failed");
                }
            }
        }

        // A frozen process can exit on its own (or be killed elsewhere); drop
        // alerts whose PID is no longer present so the HUD does not offer
        // actions for ghosts.
        alerts.retain(|pid, _| snapshot.processes.iter().any(|process| process.pid == *pid));

        shared_state.store(Arc::new(display_state(&snapshot, state, &alerts)));

        match action_rx.recv_timeout(poll_interval) {
            Ok(action) => apply_user_action(action, &mut alerts),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }

    tracing::debug!("sentinel thread stopped");
}

/// Execute a decision made in the HUD and retire its alert on success.
fn apply_user_action(action: UserAction, alerts: &mut BTreeMap<u32, Alert>) {
    let (pid, guard_action) = match action {
        UserAction::Resume(pid) => (pid, GuardAction::Thaw { pid }),
        UserAction::Terminate(pid) => (pid, GuardAction::Terminate { pid }),
    };

    match guard::execute(&guard_action) {
        Ok(()) => {
            alerts.remove(&pid);
            tracing::info!(pid, ?action, "user action applied");
        }
        Err(err) => {
            // Keep the alert: the user may want to retry or choose the other
            // action (e.g. SIGCONT failed but SIGTERM may still work).
            tracing::warn!(pid, ?action, %err, "user action failed");
        }
    }
}

fn display_state(
    snapshot: &SystemSnapshot,
    state: SystemState,
    alerts: &BTreeMap<u32, Alert>,
) -> DisplayState {
    DisplayState {
        cpu_pct: snapshot.cpu_usage_pct,
        ram_used_bytes: snapshot.ram_used_bytes,
        ram_total_bytes: snapshot.ram_total_bytes,
        vram_used_bytes: snapshot.vram_used_bytes,
        vram_total_bytes: snapshot.vram_total_bytes,
        system_state: map_state(state),
        pending_alerts: alerts.values().cloned().collect(),
    }
}

fn map_state(state: SystemState) -> hud::SystemState {
    match state {
        SystemState::Green => hud::SystemState::Green,
        SystemState::Yellow => hud::SystemState::Yellow,
        SystemState::Red => hud::SystemState::Red,
    }
}

fn process_name(snapshot: &SystemSnapshot, pid: u32) -> String {
    snapshot
        .processes
        .iter()
        .find(|process| process.pid == pid)
        .map(|process| process.name.clone())
        .unwrap_or_else(|| format!("pid {pid}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use hud::SystemState as HudState;
    use sentinel::{ProcessInfo, SystemSnapshot};

    fn snapshot() -> SystemSnapshot {
        SystemSnapshot {
            cpu_usage_pct: 12.5,
            ram_used_bytes: 4 * 1024 * 1024 * 1024,
            ram_total_bytes: 16 * 1024 * 1024 * 1024,
            vram_used_bytes: Some(1024 * 1024 * 1024),
            vram_total_bytes: Some(8 * 1024 * 1024 * 1024),
            processes: vec![ProcessInfo {
                pid: 42,
                name: "cargo".to_owned(),
                rss_bytes: 512 * 1024 * 1024,
                cpu_pct: 3.0,
            }],
        }
    }

    #[test]
    fn display_state_copies_telemetry_and_alerts() {
        let mut alerts = BTreeMap::new();
        alerts.insert(
            42,
            Alert {
                pid: 42,
                process_name: "cargo".to_owned(),
                message: "frozen".to_owned(),
                actions: vec![AlertAction::Resume, AlertAction::Terminate],
            },
        );

        let display = display_state(&snapshot(), SystemState::Red, &alerts);

        assert_eq!(display.cpu_pct, 12.5);
        assert_eq!(display.ram_total_bytes, 16 * 1024 * 1024 * 1024);
        assert_eq!(display.system_state, HudState::Red);
        assert_eq!(display.pending_alerts.len(), 1);
        assert_eq!(display.pending_alerts[0].pid, 42);
    }

    #[test]
    fn state_mapping_is_total() {
        assert_eq!(map_state(SystemState::Green), HudState::Green);
        assert_eq!(map_state(SystemState::Yellow), HudState::Yellow);
        assert_eq!(map_state(SystemState::Red), HudState::Red);
    }

    #[test]
    fn missing_process_falls_back_to_pid_label() {
        assert_eq!(process_name(&snapshot(), 7), "pid 7");
        assert_eq!(process_name(&snapshot(), 42), "cargo");
    }
}
