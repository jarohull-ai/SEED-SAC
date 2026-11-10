//! SEED SAC binary entry point.
//!
//! The process is split into two halves:
//!
//! * the **sentinel thread** samples the machine, evaluates the resource
//!   policy, runs mitigation actions, serves HUD commands through the
//!   [`router`] crate and publishes a [`hud::DisplayState`] into a lock-free
//!   `ArcSwap`;
//! * the **main thread** runs the egui event loop, which only reads that
//!   snapshot and sends user decisions back over `mpsc` channels.
//!
//! Keeping telemetry off the UI thread means a slow `/proc` read or a stalled
//! `renice` never freezes the window.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender};
use std::thread;
use std::time::{Duration, Instant};

use tracing_subscriber::EnvFilter;

use hud::{
    Alert, AlertAction, DisplayState, DisplayThresholds, SacApp, SharedDisplayState, UserAction,
    UserCommand,
};
use router::executor::ExecResult;
use router::l1_client::{L1Client, L1Config, L1Error};
use sentinel::ProcessIdentity;
use sentinel::config::Config;
use sentinel::guard::{self, GuardAction};
use sentinel::process::ProcessTracker;
use sentinel::sampler::Sampler;
use sentinel::{SystemSnapshot, SystemState, policy};

/// Smallest poll interval we accept, to keep a malformed config from turning
/// the sentinel into a busy loop.
const MIN_POLL_INTERVAL_MS: u64 = 50;

/// How long a router reply stays visible in the HUD.
const COMMAND_RESPONSE_TTL: Duration = Duration::from_secs(5);
const L1_OFFLINE_MESSAGE: &str = "L1 offline (LM Studio niedostępne na porcie 1234)";

fn main() -> Result<(), Box<dyn std::error::Error>> {
    init_tracing();

    let config = sentinel::config::load();
    let poll_interval =
        Duration::from_millis(config.sentinel.poll_interval_ms.max(MIN_POLL_INTERVAL_MS));

    tracing::info!(
        poll_interval_ms = poll_interval.as_millis(),
        "starting SEED SAC"
    );

    let shared_state = hud::new_shared_state();
    let (action_tx, action_rx) = mpsc::channel::<UserAction>();
    let (command_tx, command_rx) = mpsc::channel::<UserCommand>();
    let running = Arc::new(AtomicBool::new(true));

    let sentinel_handle = {
        let shared_state = Arc::clone(&shared_state);
        let running = Arc::clone(&running);
        thread::Builder::new()
            .name("sentinel".to_owned())
            .spawn(move || {
                run_sentinel(
                    config,
                    shared_state,
                    action_rx,
                    command_rx,
                    running,
                    poll_interval,
                );
            })
            .map_err(std::io::Error::other)?
    };

    let native_options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_title("SAC · System-Aware Copilot")
            .with_inner_size([600.0, 120.0])
            .with_min_inner_size([480.0, 100.0])
            .with_always_on_top()
            .with_decorations(true)
            .with_resizable(true),
        ..Default::default()
    };

    let eframe_result = eframe::run_native(
        "SAC · System-Aware Copilot",
        native_options,
        Box::new(move |_cc| Ok(Box::new(SacApp::new(shared_state, action_tx, command_tx)))),
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
/// alerts alive until the user resolves them, and serves HUD actions and
/// commands while waiting for the next tick.
fn run_sentinel(
    config: Config,
    shared_state: SharedDisplayState,
    action_rx: Receiver<UserAction>,
    command_rx: Receiver<UserCommand>,
    running: Arc<AtomicBool>,
    poll_interval: Duration,
) {
    let mut sampler = Sampler::new();
    let mut tracker = ProcessTracker::new();
    let mut alerts: BTreeMap<u32, Alert> = BTreeMap::new();
    let mut alert_targets: BTreeMap<u32, ProcessIdentity> = BTreeMap::new();
    let mut banner: Option<CommandBanner> = None;
    let mut l1_worker = L1Worker::spawn(&config);
    let mut command_generation = 0_u64;
    while running.load(Ordering::Acquire) {
        let snapshot = sampler.poll();
        let deltas = tracker.update(&snapshot);
        let state = policy::evaluate(&snapshot, &config);
        let actions = guard::mitigate(state, &deltas, &config);

        for action in &actions {
            let Some(process) = snapshot
                .processes
                .iter()
                .find(|p| p.pid == action_pid(*action))
            else {
                tracing::warn!(?action, "mitigation target disappeared before execution");
                continue;
            };
            match guard::execute_for(action, &process.identity()) {
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
                        alert_targets.insert(pid, process.identity());
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
        alert_targets.retain(|pid, _| alerts.contains_key(pid));

        while let Some((generation, response)) = l1_worker.try_response() {
            if generation == command_generation {
                banner = Some(CommandBanner::new(response, Instant::now()));
            }
        }

        // Commands are polled without blocking: the sentinel must keep
        // sampling even when the user never types anything.
        while let Ok(UserCommand(text)) = command_rx.try_recv() {
            command_generation = command_generation.wrapping_add(1);
            banner = None;
            if let Some(response) =
                handle_command(&text, &mut l1_worker, &config, command_generation)
            {
                banner = Some(CommandBanner::new(response, Instant::now()));
            }
            tracing::info!("HUD command handled");
        }

        shared_state.store(Arc::new(display_state(
            &snapshot,
            state,
            &alerts,
            banner.as_ref().map(|banner| banner.text.as_str()),
            &config,
        )));

        // Age the reply only after it has been published, so it stays visible
        // for a full TTL window.
        if banner
            .as_ref()
            .is_some_and(|banner| banner.is_expired(Instant::now()))
        {
            banner = None;
        }

        match action_rx.recv_timeout(poll_interval) {
            Ok(action) => apply_user_action(action, &mut alerts, &mut alert_targets),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }

    tracing::debug!("sentinel thread stopped");
}

/// Route a HUD command through the router and render its result as text for
/// the HUD.
fn handle_command(
    text: &str,
    l1_worker: &mut L1Worker,
    config: &Config,
    generation: u64,
) -> Option<String> {
    let action = router::parse(text);
    if matches!(action, router::Action::QueryL1(_)) {
        if let router::Action::QueryL1(query) = action
            && !l1_worker.submit(query, generation)
        {
            return Some("L1 busy — polecenie nie zostało zakolejkowane".to_owned());
        }
        return Some("L1: oczekiwanie na lokalny model…".to_owned());
    }
    match router::executor::execute_with_config(&action, config) {
        ExecResult::Done(message) => Some(message),
        ExecResult::NeedsL1(query) => {
            if l1_worker.submit(query, generation) {
                None
            } else {
                Some("L1 busy — polecenie nie zostało zakolejkowane".to_owned())
            }
        }
        ExecResult::Error(message) => Some(format!("Błąd: {message}")),
    }
}

/// The blocking L1 client lives on its own worker. The sentinel only performs
/// non-blocking channel operations, so neither sampling nor the HUD can stall
/// while LM Studio is starting or timing out.
struct L1Worker {
    request_tx: SyncSender<(u64, String)>,
    response_rx: Receiver<(u64, String)>,
}

impl L1Worker {
    fn spawn(config: &Config) -> Self {
        let (request_tx, request_rx) = mpsc::sync_channel::<(u64, String)>(4);
        let (response_tx, response_rx) = mpsc::channel::<(u64, String)>();
        let provider = config.l1_provider.clone().validated();
        thread::Builder::new()
            .name("l1-client".to_owned())
            .spawn(move || {
                let client = if provider.enabled && provider.backend == "openai_compatible" {
                    L1Client::new(L1Config {
                        endpoint: provider.base_url,
                        model: provider.model,
                        max_tokens: provider.max_tokens,
                        temperature: provider.temperature,
                        timeout: Duration::from_millis(provider.timeout_ms),
                        api_key: None,
                    })
                    .ok()
                } else {
                    None
                };
                while let Ok((generation, prompt)) = request_rx.recv() {
                    let response = client
                        .as_ref()
                        .map(|client| client.complete(&prompt))
                        .unwrap_or(Err(L1Error::Offline));
                    let response = map_l1_response(response);
                    if response_tx.send((generation, response)).is_err() {
                        break;
                    }
                }
            })
            .expect("spawn L1 worker");
        Self {
            request_tx,
            response_rx,
        }
    }

    fn submit(&mut self, prompt: String, generation: u64) -> bool {
        if self.request_tx.try_send((generation, prompt)).is_err() {
            tracing::debug!("L1 worker unavailable or busy");
            false
        } else {
            true
        }
    }

    fn try_response(&self) -> Option<(u64, String)> {
        self.response_rx.try_recv().ok()
    }
}

fn map_l1_response(response: Result<String, L1Error>) -> String {
    response.unwrap_or_else(|_| L1_OFFLINE_MESSAGE.to_owned())
}

/// A router reply waiting to be shown in the HUD until a wall-clock deadline.
#[derive(Debug)]
struct CommandBanner {
    text: String,
    expires_at: Instant,
}

impl CommandBanner {
    fn new(text: String, created_at: Instant) -> Self {
        Self {
            text,
            expires_at: created_at + COMMAND_RESPONSE_TTL,
        }
    }

    fn is_expired(&self, now: Instant) -> bool {
        now >= self.expires_at
    }
}

/// Return the signal actions needed for a decision made in the HUD.
///
/// A stopped process cannot act on a pending SIGTERM until it is continued,
/// so termination must explicitly thaw it first.
fn user_action_guard_actions(action: UserAction) -> Vec<GuardAction> {
    match action {
        UserAction::Resume(pid) => vec![GuardAction::Thaw { pid }],
        UserAction::Terminate(pid) => {
            vec![GuardAction::Thaw { pid }, GuardAction::Terminate { pid }]
        }
    }
}

/// Execute a decision made in the HUD.
///
/// A successful signal send only means that the kernel accepted the signal;
/// it does not prove that a termination request has taken effect. Keep
/// terminate alerts pending until the next snapshot confirms that the PID is
/// gone. Resume is different: a successful thaw resolves the frozen-process
/// alert immediately.
fn apply_user_action(
    action: UserAction,
    alerts: &mut BTreeMap<u32, Alert>,
    alert_targets: &mut BTreeMap<u32, ProcessIdentity>,
) {
    let pid = match action {
        UserAction::Resume(pid) | UserAction::Terminate(pid) => pid,
    };

    let Some(identity) = alert_targets.get(&pid).cloned() else {
        tracing::warn!(pid, "user action refused: no verified alert identity");
        return;
    };
    for guard_action in user_action_guard_actions(action) {
        if let Err(err) = guard::execute_for(&guard_action, &identity) {
            // Keep the alert: the user may want to retry or choose the other
            // action (e.g. SIGCONT failed, so SIGTERM must not be attempted).
            tracing::warn!(pid, ?action, %err, "user action failed");
            return;
        }
    }

    if matches!(action, UserAction::Resume(_)) {
        alerts.remove(&pid);
        alert_targets.remove(&pid);
    }
    tracing::info!(pid, ?action, "user action applied");
}

fn display_state(
    snapshot: &SystemSnapshot,
    state: SystemState,
    alerts: &BTreeMap<u32, Alert>,
    command_response: Option<&str>,
    config: &Config,
) -> DisplayState {
    DisplayState {
        telemetry_available: true,
        cpu_pct: snapshot.cpu_usage_pct,
        ram_used_bytes: snapshot.ram_used_bytes,
        ram_total_bytes: snapshot.ram_total_bytes,
        vram_used_bytes: snapshot.vram_used_bytes,
        vram_total_bytes: snapshot.vram_total_bytes,
        system_state: map_state(state),
        pending_alerts: alerts.values().cloned().collect(),
        command_response: command_response.map(str::to_owned),
        thresholds: DisplayThresholds {
            cpu_yellow_pct: config.thresholds.cpu.yellow_pct,
            ram_yellow_pct: config.thresholds.ram.yellow_pct,
            ram_red_pct: config.thresholds.ram.red_pct,
            vram_yellow_pct: config.thresholds.vram.yellow_pct,
            vram_red_pct: config.thresholds.vram.red_pct,
        },
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

fn action_pid(action: GuardAction) -> u32 {
    match action {
        GuardAction::Renice { pid }
        | GuardAction::Ionice { pid }
        | GuardAction::Freeze { pid }
        | GuardAction::Thaw { pid }
        | GuardAction::Kill { pid }
        | GuardAction::Terminate { pid } => pid,
    }
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
                start_time: 1,
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

        let display = display_state(
            &snapshot(),
            SystemState::Red,
            &alerts,
            None,
            &Config::default(),
        );

        assert_eq!(display.cpu_pct, 12.5);
        assert_eq!(display.ram_total_bytes, 16 * 1024 * 1024 * 1024);
        assert_eq!(display.system_state, HudState::Red);
        assert_eq!(display.pending_alerts.len(), 1);
        assert_eq!(display.pending_alerts[0].pid, 42);
        assert!(display.command_response.is_none());
        assert!(display.telemetry_available);
    }

    #[test]
    fn display_state_carries_command_response() {
        let alerts = BTreeMap::new();

        let display = display_state(
            &snapshot(),
            SystemState::Green,
            &alerts,
            Some("ok"),
            &Config::default(),
        );

        assert_eq!(display.command_response.as_deref(), Some("ok"));
    }

    #[test]
    fn command_banner_expires_by_wall_clock() {
        let created_at = Instant::now();
        let banner = CommandBanner::new("done".to_owned(), created_at);

        assert_eq!(banner.text, "done");
        assert!(!banner.is_expired(created_at + COMMAND_RESPONSE_TTL - Duration::from_millis(1)));
        assert!(banner.is_expired(created_at + COMMAND_RESPONSE_TTL));
    }

    #[test]
    fn state_mapping_is_total() {
        assert_eq!(map_state(SystemState::Green), HudState::Green);
        assert_eq!(map_state(SystemState::Yellow), HudState::Yellow);
        assert_eq!(map_state(SystemState::Red), HudState::Red);
    }

    #[test]
    fn display_state_exposes_configured_thresholds() {
        let mut config = Config::default();
        config.thresholds.ram.yellow_pct = 61.0;
        config.thresholds.ram.red_pct = 81.0;
        config.thresholds.cpu.yellow_pct = 63.0;
        config.thresholds.vram.yellow_pct = 71.0;
        config.thresholds.vram.red_pct = 91.0;

        let display = display_state(
            &snapshot(),
            SystemState::Green,
            &BTreeMap::new(),
            None,
            &config,
        );

        assert_eq!(
            display.thresholds,
            DisplayThresholds {
                cpu_yellow_pct: 63.0,
                ram_yellow_pct: 61.0,
                ram_red_pct: 81.0,
                vram_yellow_pct: 71.0,
                vram_red_pct: 91.0,
            }
        );
    }

    #[test]
    fn missing_process_falls_back_to_pid_label() {
        assert_eq!(process_name(&snapshot(), 7), "pid 7");
        assert_eq!(process_name(&snapshot(), 42), "cargo");
    }

    #[test]
    fn terminate_thaws_before_sending_sigterm() {
        assert_eq!(
            user_action_guard_actions(UserAction::Terminate(42)),
            vec![
                GuardAction::Thaw { pid: 42 },
                GuardAction::Terminate { pid: 42 }
            ]
        );
    }

    #[test]
    fn resume_only_sends_sigcont() {
        assert_eq!(
            user_action_guard_actions(UserAction::Resume(42)),
            vec![GuardAction::Thaw { pid: 42 }]
        );
    }

    #[test]
    fn l1_offline_mapping_does_not_expose_client_details() {
        assert_eq!(map_l1_response(Err(L1Error::Timeout)), L1_OFFLINE_MESSAGE);
        assert_eq!(
            map_l1_response(Err(L1Error::Offline)),
            "L1 offline (LM Studio niedostępne na porcie 1234)"
        );
    }

    #[test]
    fn l1_handoff_is_nonblocking_when_provider_is_disabled() {
        let mut config = Config::default();
        config.l1_provider.enabled = false;
        let mut worker = L1Worker::spawn(&config);
        let started = Instant::now();

        assert!(worker.submit("private prompt must not be logged".to_owned(), 1));

        assert!(started.elapsed() < Duration::from_millis(100));
        let deadline = Instant::now() + Duration::from_secs(1);
        let response = loop {
            if let Some((_, response)) = worker.try_response() {
                break response;
            }
            assert!(Instant::now() < deadline, "L1 worker did not respond");
            thread::yield_now();
        };
        assert_eq!(response, L1_OFFLINE_MESSAGE);
    }
}
