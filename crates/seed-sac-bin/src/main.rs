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
use memory::{EventType, EventWriter, SystemEvent};
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
/// Largest poll interval accepted, so malformed configuration cannot disable
/// sentinel monitoring by making the wait effectively unbounded.
const MAX_POLL_INTERVAL_MS: u64 = 5_000;
/// Maximum time the sentinel waits before checking for a newly submitted HUD
/// command. This is independent of telemetry sampling, so a large sampling
/// interval cannot hide the pending acknowledgement.
const COMMAND_CHECK_INTERVAL: Duration = Duration::from_millis(50);

/// How long a router reply stays visible in the HUD.
const COMMAND_RESPONSE_TTL: Duration = Duration::from_secs(5);
const L1_OFFLINE_MESSAGE: &str = "L1 offline (LM Studio niedostępne na porcie 1234)";
const CONFIRM_COMMAND: &str = "__sac_confirm";
const CANCEL_COMMAND: &str = "__sac_cancel";
const CONFIRMATION_TTL: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, PartialEq, Eq)]
enum PendingAction {
    Router {
        action: router::Action,
        identity: Option<ProcessIdentity>,
    },
    AlertTerminate {
        identity: ProcessIdentity,
    },
}

#[derive(Debug, Clone)]
struct PendingConfirmation {
    action: PendingAction,
    expires_at: Instant,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    init_tracing();

    let config = sentinel::config::load();
    let poll_interval =
        Duration::from_millis(normalize_poll_interval_ms(config.sentinel.poll_interval_ms));

    tracing::info!(
        poll_interval_ms = poll_interval.as_millis(),
        "starting SEED SAC"
    );

    let event_writer = match EventWriter::open(memory::default_database_path().join("events.db")) {
        Ok(writer) => {
            tracing::info!("memory writer started");
            Some(writer)
        }
        Err(err) => {
            tracing::warn!("could not start memory writer: {err}");
            None
        }
    };

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
                    event_writer,
                )
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

    // Wait for sentinel to complete (including thaw-on-exit) and gracefully shutdown
    let writer = sentinel_handle.join().map_err(|_| "sentinel thread panicked")?;
    if let Some(w) = writer
        && let Err(err) = w.shutdown()
    {
        tracing::warn!("error shutting down event writer: {err}");
    }

    eframe_result?;

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

fn normalize_poll_interval_ms(poll_interval_ms: u64) -> u64 {
    poll_interval_ms.clamp(MIN_POLL_INTERVAL_MS, MAX_POLL_INTERVAL_MS)
}

fn sentinel_wait_duration(next_sample_at: Instant, now: Instant) -> Duration {
    next_sample_at
        .saturating_duration_since(now)
        .min(COMMAND_CHECK_INTERVAL)
}

/// Main loop of the background sentinel thread.
///
/// It publishes a fresh display snapshot on every tick, keeps frozen-process
/// alerts alive until the user resolves them, and serves HUD actions and
/// commands while waiting for the next tick. Returns the event_writer for
/// graceful shutdown in main().
fn run_sentinel(
    config: Config,
    shared_state: SharedDisplayState,
    action_rx: Receiver<UserAction>,
    command_rx: Receiver<UserCommand>,
    running: Arc<AtomicBool>,
    poll_interval: Duration,
    event_writer: Option<EventWriter>,
) -> Option<EventWriter> {
    let mut sampler = Sampler::new();
    let mut tracker = ProcessTracker::new();
    let mut alerts: BTreeMap<u32, Alert> = BTreeMap::new();
    let mut alert_targets: BTreeMap<u32, ProcessIdentity> = BTreeMap::new();
    let mut banner: Option<CommandBanner> = None;
    let mut l1_worker = L1Worker::spawn(&config);
    let mut command_generation = 0_u64;
    let mut pending_confirmation: Option<PendingConfirmation> = None;
    let mut next_sample_at = Instant::now();
    let mut latest_snapshot: Option<SystemSnapshot> = None;
    let mut latest_state = SystemState::Green;
    while running.load(Ordering::Acquire) {
        if Instant::now() >= next_sample_at {
            let snapshot = sampler.poll();
            let deltas = tracker.update(&snapshot);
            let state = policy::evaluate(&snapshot, &config);
            let actions = guard::mitigate(state, &deltas, &config);

            // Log state transitions
            if state != latest_state {
                let event_type = match state {
                    SystemState::Yellow => Some(EventType::YellowEnter),
                    SystemState::Red => Some(EventType::RedEnter),
                    SystemState::Green => None,
                };
                if let (Some(writer), Some(event_type)) = (&event_writer, event_type) {
                    let event = SystemEvent {
                        timestamp: std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_millis() as i64,
                        event_type,
                        target_pid: None,
                        process_name: None,
                        ram_usage_mb: Some(snapshot.ram_used_bytes as f64 / (1024.0 * 1024.0)),
                        cpu_usage_pct: None,
                        vram_usage_mb: snapshot
                            .vram_used_bytes
                            .map(|v| v as f64 / (1024.0 * 1024.0)),
                        details: None,
                    };
                    let _ = writer.record(event);
                }
            }

            for action in &actions {
                let Some(process) = snapshot
                    .processes
                    .iter()
                    .find(|p| p.pid == action_pid(*action))
                else {
                    tracing::warn!(?action, "mitigation target disappeared before execution");
                    continue;
                };
                match guard::execute_for(action, &process.identity(), &config) {
                    Ok(()) => {
                        if let GuardAction::Freeze { pid } = *action {
                            let name = process_name(&snapshot, pid);
                            tracing::warn!(pid, process = %name, "froze process to prevent OOM");

                            // Log SIGSTOP event
                            if let Some(writer) = &event_writer {
                                let proc_ram = snapshot
                                    .processes
                                    .iter()
                                    .find(|p| p.pid == pid)
                                    .map(|p| p.rss_bytes as f64 / (1024.0 * 1024.0));
                                let event = SystemEvent {
                                    timestamp: std::time::SystemTime::now()
                                        .duration_since(std::time::UNIX_EPOCH)
                                        .unwrap_or_default()
                                        .as_millis()
                                        as i64,
                                    event_type: EventType::SigstopSent,
                                    target_pid: Some(pid),
                                    process_name: Some(name.clone()),
                                    ram_usage_mb: proc_ram,
                                    cpu_usage_pct: None,
                                    vram_usage_mb: snapshot
                                        .vram_used_bytes
                                        .map(|v| v as f64 / (1024.0 * 1024.0)),
                                    details: None,
                                };
                                let _ = writer.record(event);
                            }

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
            latest_snapshot = Some(snapshot);
            latest_state = state;
            next_sample_at = Instant::now() + poll_interval;
        }

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
            if let Some(response) = handle_command(
                &text,
                &mut l1_worker,
                &config,
                command_generation,
                latest_snapshot.as_ref(),
                &mut pending_confirmation,
            ) {
                banner = Some(CommandBanner::new(response, Instant::now()));
            }
            tracing::info!("HUD command handled");
        }

        if pending_confirmation
            .as_ref()
            .is_some_and(|pending| Instant::now() >= pending.expires_at)
        {
            pending_confirmation = None;
            banner = Some(CommandBanner::new(
                "Potwierdzenie wygasło — akcja nie została wykonana".to_owned(),
                Instant::now(),
            ));
        }

        shared_state.store(Arc::new(display_state(
            latest_snapshot
                .as_ref()
                .expect("sentinel samples before publishing display state"),
            latest_state,
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

        let wait_for = sentinel_wait_duration(next_sample_at, Instant::now());
        match action_rx.recv_timeout(wait_for) {
            Ok(action) => {
                if let Some(response) = handle_user_action(
                    action,
                    latest_snapshot.as_ref(),
                    &config,
                    &mut alerts,
                    &mut alert_targets,
                    &mut pending_confirmation,
                    &event_writer,
                ) {
                    banner = Some(CommandBanner::new(response, Instant::now()));
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }

    // Thaw any frozen processes before exiting. A frozen process that remains
    // SIGSTOP'd indefinitely after SAC exits creates an unrecoverable system state.
    for (pid, identity) in alert_targets.iter() {
        let action = guard::GuardAction::Thaw { pid: *pid };
        if let Err(err) = guard::execute_for(&action, identity, &config) {
            tracing::warn!(pid, %err, "failed to thaw process on exit");
        } else {
            tracing::info!(pid, "thawed process on exit");
            // Log SIGCONT event for thawed process
            if let Some(writer) = &event_writer {
                let event = SystemEvent {
                    timestamp: std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_millis() as i64,
                    event_type: EventType::SigcontSent,
                    target_pid: Some(*pid),
                    process_name: Some(identity.name.clone()),
                    ram_usage_mb: None,
                    cpu_usage_pct: None,
                    vram_usage_mb: None,
                    details: Some("thaw-on-exit".to_owned()),
                };
                let _ = writer.record(event);
            }
        }
    }

    tracing::debug!("sentinel thread stopped");
    event_writer
}

/// Route a HUD command through the router and render its result as text for
/// the HUD.
fn handle_command(
    text: &str,
    l1_worker: &mut L1Worker,
    config: &Config,
    generation: u64,
    snapshot: Option<&SystemSnapshot>,
    pending_confirmation: &mut Option<PendingConfirmation>,
) -> Option<String> {
    let action = router::parse(text);
    if text.trim() == CONFIRM_COMMAND {
        let Some(pending) = pending_confirmation.take() else {
            return Some("Brak oczekującej akcji do potwierdzenia".to_owned());
        };
        if Instant::now() >= pending.expires_at {
            return Some("Potwierdzenie wygasło — akcja nie została wykonana".to_owned());
        }
        return execute_pending_action(pending.action, snapshot, l1_worker, config, generation);
    }
    if text.trim() == CANCEL_COMMAND {
        if pending_confirmation.take().is_some() {
            return Some("Anulowano oczekującą akcję".to_owned());
        }
        return Some("Brak oczekującej akcji do anulowania".to_owned());
    }
    if is_destructive(&action) {
        let description = confirmation_description(&action);
        let identity = match &action {
            router::Action::Kill(name) => {
                if sentinel::policy::is_immune(name, config) {
                    return Some(format!(
                        "{name} jest na liście procesów chronionych — akcja zablokowana"
                    ));
                }
                let Some(snapshot) = snapshot else {
                    return Some("Brak aktualnego obrazu procesów — akcja zablokowana".to_owned());
                };
                let mut matches = snapshot
                    .processes
                    .iter()
                    .filter(|process| process.name.eq_ignore_ascii_case(name.trim()));
                let Some(process) = matches.next() else {
                    return Some(format!("Nie znaleziono procesu o nazwie {name}"));
                };
                if matches.next().is_some() {
                    return Some(format!(
                        "Nie można jednoznacznie wybrać procesu o nazwie {name}"
                    ));
                }
                Some(process.identity())
            }
            _ => None,
        };
        *pending_confirmation = Some(PendingConfirmation {
            action: PendingAction::Router { action, identity },
            expires_at: Instant::now() + CONFIRMATION_TTL,
        });
        let description = match pending_confirmation.as_ref().map(|pending| &pending.action) {
            Some(PendingAction::Router {
                action: router::Action::Kill(_),
                identity: Some(identity),
            }) => format!(
                "Zakończenie procesu '{}' (PID {})",
                identity.name, identity.pid
            ),
            _ => description,
        };
        return Some(format!(
            "POTWIERDZENIE: {description}\nNaciśnij Enter lub wybierz Potwierdź, aby wykonać."
        ));
    }
    // A normal command replaces an outstanding confirmation, so an old
    // destructive action can never execute after the user changes context.
    pending_confirmation.take();
    execute_command_action(action, l1_worker, config, generation)
}

fn is_destructive(action: &router::Action) -> bool {
    matches!(action, router::Action::Kill(_) | router::Action::CleanCache)
}

fn confirmation_description(action: &router::Action) -> String {
    match action {
        router::Action::Kill(name) => format!("Zakończenie procesu '{name}'"),
        router::Action::CleanCache => "Wyczyszczenie pamięci podręcznej (drop_caches=3)".to_owned(),
        _ => unreachable!("only destructive actions require confirmation"),
    }
}

fn execute_pending_action(
    action: PendingAction,
    snapshot: Option<&SystemSnapshot>,
    l1_worker: &mut L1Worker,
    config: &Config,
    generation: u64,
) -> Option<String> {
    match action {
        PendingAction::Router { action, identity } => {
            if let router::Action::Kill(_) = action {
                let Some(identity) = identity else {
                    return Some(
                        "Brak zweryfikowanej tożsamości procesu — akcja zablokowana".to_owned(),
                    );
                };
                if sentinel::policy::is_immune(&identity.name, config) {
                    return Some(format!(
                        "{} jest na liście procesów chronionych — akcja zablokowana",
                        identity.name
                    ));
                }
                return match guard::execute_for(
                    &GuardAction::Kill { pid: identity.pid },
                    &identity,
                    config,
                ) {
                    Ok(()) => Some(format!("Zamknięto {}", identity.name)),
                    Err(error) => Some(format!("Błąd: {error}")),
                };
            }
            execute_confirmed_command_action(action, l1_worker, config, generation)
        }
        PendingAction::AlertTerminate { identity } => {
            let Some(snapshot) = snapshot else {
                return Some("Brak aktualnego obrazu procesów — akcja zablokowana".to_owned());
            };
            let Some(current) = snapshot
                .processes
                .iter()
                .find(|process| process.pid == identity.pid)
            else {
                return Some("Proces zniknął — akcja anulowana".to_owned());
            };
            if current.identity() != identity {
                return Some("Tożsamość procesu zmieniła się — akcja zablokowana".to_owned());
            }
            if sentinel::policy::is_immune(&identity.name, config) {
                return Some(format!(
                    "{} jest na liście procesów chronionych — akcja zablokowana",
                    identity.name
                ));
            }
            match apply_terminate_identity(&identity, config) {
                Ok(()) => Some(format!("Zamknięto {}", identity.name)),
                Err(error) => Some(format!("Błąd: {error}")),
            }
        }
    }
}

fn apply_terminate_identity(identity: &ProcessIdentity, config: &Config) -> Result<(), String> {
    guard::execute_for(&GuardAction::Thaw { pid: identity.pid }, identity, config)?;
    guard::execute_for(
        &GuardAction::Terminate { pid: identity.pid },
        identity,
        config,
    )
}

fn execute_command_action(
    action: router::Action,
    l1_worker: &mut L1Worker,
    config: &Config,
    generation: u64,
) -> Option<String> {
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

fn execute_confirmed_command_action(
    action: router::Action,
    l1_worker: &mut L1Worker,
    config: &Config,
    generation: u64,
) -> Option<String> {
    match router::executor::execute_confirmed_with_config(&action, config) {
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
        let ttl = if text.starts_with("POTWIERDZENIE:") {
            CONFIRMATION_TTL
        } else {
            COMMAND_RESPONSE_TTL
        };
        Self {
            text,
            expires_at: created_at + ttl,
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
fn handle_user_action(
    action: UserAction,
    snapshot: Option<&SystemSnapshot>,
    config: &Config,
    alerts: &mut BTreeMap<u32, Alert>,
    alert_targets: &mut BTreeMap<u32, ProcessIdentity>,
    pending_confirmation: &mut Option<PendingConfirmation>,
    event_writer: &Option<EventWriter>,
) -> Option<String> {
    let pid = match action {
        UserAction::Resume(pid) | UserAction::Terminate(pid) => pid,
    };

    let Some(identity) = alert_targets.get(&pid).cloned() else {
        tracing::warn!(pid, "user action refused: no verified alert identity");
        return Some("Brak zweryfikowanej tożsamości alertu — akcja zablokowana".to_owned());
    };
    if matches!(action, UserAction::Terminate(_)) {
        if let Some(pending) = pending_confirmation.as_ref()
            && Instant::now() < pending.expires_at
        {
            return Some("POTWIERDZENIE: Oczekuje już decyzja dla akcji destrukcyjnej".to_owned());
        }
        return Some(confirm_alert_termination(
            identity,
            snapshot,
            &mut *alerts,
            alert_targets,
            pending_confirmation,
        ));
    };
    let cancelled_confirmation = cancel_pending_for_resume(action, pending_confirmation);
    for guard_action in user_action_guard_actions(action) {
        if let Err(err) = guard::execute_for(&guard_action, &identity, config) {
            // Keep the alert: the user may want to retry or choose the other
            // action (e.g. SIGCONT failed, so SIGTERM must not be attempted).
            tracing::warn!(pid, ?action, %err, "user action failed");
            return Some(format!("Błąd: {err}"));
        }
    }

    if matches!(action, UserAction::Resume(_)) {
        // Log SIGCONT event
        if let Some(writer) = event_writer {
            let proc_ram = snapshot.and_then(|s| {
                s.processes
                    .iter()
                    .find(|p| p.pid == pid)
                    .map(|p| p.rss_bytes as f64 / (1024.0 * 1024.0))
            });
            let event = SystemEvent {
                timestamp: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as i64,
                event_type: EventType::SigcontSent,
                target_pid: Some(pid),
                process_name: Some(identity.name.clone()),
                ram_usage_mb: proc_ram,
                cpu_usage_pct: None,
                vram_usage_mb: None,
                details: None,
            };
            let _ = writer.record(event);
        }
        alerts.remove(&pid);
        alert_targets.remove(&pid);
    }
    tracing::info!(pid, ?action, "user action applied");
    cancelled_confirmation
        .then_some("Anulowano oczekującą akcję przed wznowieniem procesu".to_owned())
}

fn cancel_pending_for_resume(
    action: UserAction,
    pending_confirmation: &mut Option<PendingConfirmation>,
) -> bool {
    matches!(action, UserAction::Resume(_)) && pending_confirmation.take().is_some()
}

fn confirm_alert_termination(
    identity: ProcessIdentity,
    snapshot: Option<&SystemSnapshot>,
    alerts: &mut BTreeMap<u32, Alert>,
    alert_targets: &mut BTreeMap<u32, ProcessIdentity>,
    pending_confirmation: &mut Option<PendingConfirmation>,
) -> String {
    let Some(snapshot) = snapshot else {
        return "Brak aktualnego obrazu procesów — akcja zablokowana".to_owned();
    };
    let Some(current) = snapshot
        .processes
        .iter()
        .find(|process| process.pid == identity.pid)
    else {
        alerts.remove(&identity.pid);
        alert_targets.remove(&identity.pid);
        return "Proces zniknął — akcja anulowana".to_owned();
    };
    if current.identity() != identity {
        alerts.remove(&identity.pid);
        alert_targets.remove(&identity.pid);
        return "Tożsamość procesu zmieniła się — akcja zablokowana".to_owned();
    }
    // The actual SIGTERM is deliberately deferred until the explicit HUD
    // confirmation command is received.
    *pending_confirmation = Some(PendingConfirmation {
        action: PendingAction::AlertTerminate {
            identity: identity.clone(),
        },
        expires_at: Instant::now() + CONFIRMATION_TTL,
    });
    format!(
        "POTWIERDZENIE: Zakończenie procesu '{}' (PID {})\nNaciśnij Enter lub wybierz Potwierdź, aby wykonać.",
        identity.name, identity.pid
    )
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
    fn poll_interval_is_clamped_to_safe_bounds() {
        assert_eq!(normalize_poll_interval_ms(0), MIN_POLL_INTERVAL_MS);
        assert_eq!(normalize_poll_interval_ms(250), 250);
        assert_eq!(normalize_poll_interval_ms(u64::MAX), MAX_POLL_INTERVAL_MS);
    }

    #[test]
    fn command_check_wait_is_bounded_independently_of_sample_interval() {
        let now = Instant::now();
        assert_eq!(
            sentinel_wait_duration(now + Duration::from_secs(30), now),
            COMMAND_CHECK_INTERVAL
        );
        assert_eq!(
            sentinel_wait_duration(now + Duration::from_millis(10), now),
            Duration::from_millis(10)
        );
    }

    #[test]
    fn l1_command_shows_pending_ack_before_worker_response() {
        let mut config = Config::default();
        config.l1_provider.enabled = false;
        let mut worker = L1Worker::spawn(&config);

        let response = handle_command("ask local model", &mut worker, &config, 1, None, &mut None);

        assert_eq!(
            response.as_deref(),
            Some("L1: oczekiwanie na lokalny model…")
        );
    }

    #[test]
    fn destructive_command_fails_closed_without_process_snapshot() {
        let mut config = Config::default();
        config.l1_provider.enabled = false;
        let mut worker = L1Worker::spawn(&config);
        let mut pending = None;

        let response = handle_command(
            "kill seed-sac-missing-process-xyz",
            &mut worker,
            &config,
            1,
            None,
            &mut pending,
        );

        assert_eq!(
            response.as_deref(),
            Some("Brak aktualnego obrazu procesów — akcja zablokowana")
        );
        assert!(pending.is_none());
    }

    #[test]
    fn kill_rejects_immune_process_case_insensitively() {
        let mut config = Config::default();
        config.l1_provider.enabled = false;
        config.guard.immune_process_names = vec!["CaRgO".to_owned()];
        let mut worker = L1Worker::spawn(&config);
        let mut pending = None;

        let response = handle_command(
            "kill CARGO",
            &mut worker,
            &config,
            1,
            Some(&snapshot()),
            &mut pending,
        );

        assert_eq!(
            response.as_deref(),
            Some("cargo jest na liście procesów chronionych — akcja zablokowana")
        );
        assert!(pending.is_none());
    }

    #[test]
    fn kill_lookup_is_case_insensitive_and_confirmation_has_name_and_pid() {
        let mut config = Config::default();
        config.l1_provider.enabled = false;
        let mut worker = L1Worker::spawn(&config);
        let mut pending = None;

        let response = handle_command(
            "kill CARGO",
            &mut worker,
            &config,
            1,
            Some(&snapshot()),
            &mut pending,
        )
        .unwrap();

        assert!(response.contains("'cargo' (PID 42)"));
        assert!(pending.is_some());
    }

    #[test]
    fn explicit_confirmation_executes_pending_action() {
        let mut config = Config::default();
        config.l1_provider.enabled = false;
        let mut worker = L1Worker::spawn(&config);
        let mut pending = Some(PendingConfirmation {
            action: PendingAction::Router {
                action: router::Action::Kill("seed-sac-missing-process-xyz".to_owned()),
                identity: None,
            },
            expires_at: Instant::now() + CONFIRMATION_TTL,
        });

        let response = handle_command(CONFIRM_COMMAND, &mut worker, &config, 1, None, &mut pending);

        assert_eq!(
            response.as_deref(),
            Some("Brak zweryfikowanej tożsamości procesu — akcja zablokowana")
        );
        assert!(pending.is_none());
    }

    #[test]
    fn expired_confirmation_is_rejected_when_consumed() {
        let mut config = Config::default();
        config.l1_provider.enabled = false;
        let mut worker = L1Worker::spawn(&config);
        let mut pending = Some(PendingConfirmation {
            action: PendingAction::Router {
                action: router::Action::CleanCache,
                identity: None,
            },
            expires_at: Instant::now() - Duration::from_secs(1),
        });

        let response = handle_command(CONFIRM_COMMAND, &mut worker, &config, 1, None, &mut pending);

        assert_eq!(
            response.as_deref(),
            Some("Potwierdzenie wygasło — akcja nie została wykonana")
        );
        assert!(pending.is_none());
    }

    #[test]
    fn normal_command_replaces_pending_confirmation_and_cancel_clears_it() {
        let mut config = Config::default();
        config.l1_provider.enabled = false;
        let mut worker = L1Worker::spawn(&config);
        let mut pending = Some(PendingConfirmation {
            action: PendingAction::Router {
                action: router::Action::Kill("old-target".to_owned()),
                identity: None,
            },
            expires_at: Instant::now() + CONFIRMATION_TTL,
        });

        let _ = handle_command("status", &mut worker, &config, 1, None, &mut pending);
        assert!(pending.is_none());

        pending = Some(PendingConfirmation {
            action: PendingAction::Router {
                action: router::Action::CleanCache,
                identity: None,
            },
            expires_at: Instant::now() + CONFIRMATION_TTL,
        });
        let response = handle_command(CANCEL_COMMAND, &mut worker, &config, 2, None, &mut pending);
        assert_eq!(response.as_deref(), Some("Anulowano oczekującą akcję"));
        assert!(pending.is_none());
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
    fn confirmation_banner_remains_visible_for_backend_confirmation_ttl() {
        let created_at = Instant::now();
        let banner = CommandBanner::new("POTWIERDZENIE: kill\n...".to_owned(), created_at);

        assert!(!banner.is_expired(created_at + COMMAND_RESPONSE_TTL));
        assert!(!banner.is_expired(created_at + CONFIRMATION_TTL - Duration::from_millis(1)));
        assert!(banner.is_expired(created_at + CONFIRMATION_TTL));
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
    fn alert_termination_requires_explicit_confirmation() {
        let mut alerts = BTreeMap::from([(
            42,
            Alert {
                pid: 42,
                process_name: "cargo".to_owned(),
                message: "frozen".to_owned(),
                actions: vec![AlertAction::Terminate],
            },
        )]);
        let identity = snapshot().processes[0].identity();
        let mut targets = BTreeMap::from([(42, identity)]);
        let mut pending = None;

        let response = handle_user_action(
            UserAction::Terminate(42),
            Some(&snapshot()),
            &Config::default(),
            &mut alerts,
            &mut targets,
            &mut pending,
            &None,
        );

        assert!(
            response
                .as_deref()
                .is_some_and(|text| text.starts_with("POTWIERDZENIE:"))
        );
        assert!(pending.is_some());
        assert!(alerts.contains_key(&42));
    }

    #[test]
    fn alert_confirmation_identifies_process_by_name_and_pid() {
        let mut alerts = BTreeMap::from([(
            42,
            Alert {
                pid: 42,
                process_name: "cargo".to_owned(),
                message: "frozen".to_owned(),
                actions: vec![AlertAction::Terminate],
            },
        )]);
        let mut targets = BTreeMap::from([(42, snapshot().processes[0].identity())]);
        let mut pending = None;

        let response = confirm_alert_termination(
            snapshot().processes[0].identity(),
            Some(&snapshot()),
            &mut alerts,
            &mut targets,
            &mut pending,
        );

        assert!(response.contains("'cargo' (PID 42)"));
    }

    #[test]
    fn alert_confirmation_rejects_changed_identity() {
        let original = snapshot();
        let mut changed = original.clone();
        changed.processes[0].start_time += 1;
        let identity = original.processes[0].identity();
        let mut alerts = BTreeMap::from([(
            42,
            Alert {
                pid: 42,
                process_name: "cargo".to_owned(),
                message: "frozen".to_owned(),
                actions: vec![AlertAction::Terminate],
            },
        )]);
        let mut targets = BTreeMap::from([(42, identity)]);
        let mut pending = None;

        let response = confirm_alert_termination(
            original.processes[0].identity(),
            Some(&changed),
            &mut alerts,
            &mut targets,
            &mut pending,
        );

        assert_eq!(
            response,
            "Tożsamość procesu zmieniła się — akcja zablokowana"
        );
        assert!(pending.is_none());
        assert!(!alerts.contains_key(&42));
    }

    #[test]
    fn resume_only_sends_sigcont() {
        assert_eq!(
            user_action_guard_actions(UserAction::Resume(42)),
            vec![GuardAction::Thaw { pid: 42 }]
        );
    }

    #[test]
    fn resume_cancels_pending_destructive_confirmation() {
        let mut pending = Some(PendingConfirmation {
            action: PendingAction::Router {
                action: router::Action::CleanCache,
                identity: None,
            },
            expires_at: Instant::now() + CONFIRMATION_TTL,
        });

        assert!(cancel_pending_for_resume(
            UserAction::Resume(42),
            &mut pending
        ));
        assert!(pending.is_none());
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
