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

use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use tracing_subscriber::EnvFilter;

use hud::{
    Alert, AlertAction, ConsoleEntry, DisplayState, DisplayThresholds, LogLevel, SacApp,
    SharedDisplayState, UserAction, UserCommand,
};
use memory::{EventRecorder, EventType, EventWriter, SystemEvent};
use router::executor::ExecResult;
use router::l1_client::{sanitize_prompt, DLPReport, L1Client, L1Config};
use sentinel::config::Config;
use sentinel::guard::{self, GuardAction};
use sentinel::process::ProcessTracker;
use sentinel::sampler::Sampler;
use sentinel::ProcessIdentity;
use sentinel::{policy, ProcessDelta, SystemSnapshot, SystemState};

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
const COMMAND_RESPONSE_TTL: Duration = Duration::from_secs(15);
const L1_OFFLINE_MESSAGE: &str = "L1 offline (LM Studio niedostępne na porcie 1234)";
const CONFIRM_COMMAND: &str = "__sac_confirm";
const CANCEL_COMMAND: &str = "__sac_cancel";
const HELP_COMMAND: &str = "__sac_help";
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

#[derive(Debug, Clone, PartialEq, Eq)]
enum SuggestedAction {
    Ignore,
    Kill(ProcessIdentity),
    Freeze(ProcessIdentity),
}

#[derive(Debug, Clone)]
struct DeterministicDiagnosis {
    diagnosis: String,
    cause_facts: String,
    suggested_action: SuggestedAction,
    allowed_pids: Vec<u32>,
    allowed_process_names: Vec<String>,
}

impl DeterministicDiagnosis {
    fn suggested_command(&self) -> String {
        match &self.suggested_action {
            SuggestedAction::Ignore => "ignoruj".to_owned(),
            SuggestedAction::Kill(identity) => format!("ubij {}", identity.pid),
            SuggestedAction::Freeze(identity) => format!("zamroz {}", identity.pid),
        }
    }

    fn render(&self, explanation: Option<&str>) -> String {
        let mut rendered = format!(
            "Diagnoza: {}\nPrzyczyna: {}\nSugerowana komenda: {}",
            self.diagnosis,
            self.cause_facts,
            self.suggested_command()
        );
        if let Some(explanation) = explanation {
            rendered.push_str("\nObjaśnienie L1: ");
            rendered.push_str(explanation.trim());
        }
        rendered
    }
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
            .with_inner_size([760.0, 520.0])
            .with_min_inner_size([560.0, 320.0])
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
    let writer = sentinel_handle
        .join()
        .map_err(|_| "sentinel thread panicked")?;
    if let Some(w) = writer {
        if let Err(err) = w.shutdown() {
            tracing::warn!("error shutting down event writer: {err}");
        }
    }

    eframe_result?;

    tracing::info!("SEED SAC stopped");
    Ok(())
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,seed_sac=debug,l0=debug"));
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
    let mut console = VecDeque::with_capacity(100);
    let mut l1_worker =
        L1Worker::spawn_with_audit(&config, event_writer.as_ref().map(EventWriter::recorder));
    let l0_classifier = l0::IntentClassifier::new(l0_classifier_config(&config));
    let mut command_generation = 0_u64;
    let mut pending_confirmation: Option<PendingConfirmation> = None;
    let mut next_sample_at = Instant::now();
    let mut latest_snapshot: Option<SystemSnapshot> = None;
    let mut latest_deltas: Vec<ProcessDelta> = Vec::new();
    let mut latest_state = SystemState::Green;
    let mut pending_l1_diagnosis: Option<(u64, DeterministicDiagnosis)> = None;
    let mut state_dirty = true;
    while running.load(Ordering::Acquire) {
        if Instant::now() >= next_sample_at {
            let snapshot = sampler.poll();
            let deltas = tracker.update(&snapshot);
            let state = policy::evaluate(&snapshot, &config);
            let actions = guard::mitigate(state, &deltas, &config);

            // Log state transitions
            if state != latest_state {
                let state_name = match state {
                    SystemState::Green => "GREEN",
                    SystemState::Yellow => "YELLOW",
                    SystemState::Red => "RED",
                };
                push_console(
                    &mut console,
                    "SENTINEL",
                    if state == SystemState::Red {
                        LogLevel::Alert
                    } else {
                        LogLevel::Info
                    },
                    format!("Stan zasobów: {state_name}"),
                );
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
                            push_console(
                                &mut console,
                                "SENTINEL",
                                LogLevel::Alert,
                                format!("[SIGSTOP] Frozen PID {pid} ({name})"),
                            );

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
            latest_deltas = deltas;
            latest_state = state;
            state_dirty = true;
            next_sample_at = Instant::now() + poll_interval;
        }

        while let Some((generation, response, dlp_report)) = l1_worker.try_response() {
            if generation == command_generation {
                let diagnosis = pending_l1_diagnosis
                    .take()
                    .map(|(_, diagnosis)| diagnosis)
                    .unwrap_or_else(|| deterministic_diagnosis(None, latest_state, &[], &alerts));
                let validated = validate_l1_explanation(response, &diagnosis);
                let rendered = diagnosis.render(validated.text.as_deref());
                let response_with_dlp = format_l1_response(rendered, &dlp_report);
                push_console(
                    &mut console,
                    "L1",
                    LogLevel::L1Response,
                    format!("{}\n{}", validated.status, response_with_dlp),
                );
                if dlp_report.redaction_count > 0 {
                    push_console(
                        &mut console,
                        "DLP",
                        LogLevel::Warning,
                        format!(
                            "Redacted {} items ({})",
                            dlp_report.redaction_count,
                            dlp_report.rules_applied.join(", ")
                        ),
                    );
                }
                banner = Some(CommandBanner::new(
                    format!("{}\n{}", validated.status, response_with_dlp),
                    Instant::now(),
                ));
                state_dirty = true;
            }
        }

        // Commands are polled without blocking: the sentinel must keep
        // sampling even when the user never types anything.
        while let Ok(UserCommand(text)) = command_rx.try_recv() {
            command_generation = command_generation.wrapping_add(1);
            banner = None;
            let diagnosis = deterministic_diagnosis(
                latest_snapshot.as_ref(),
                latest_state,
                &latest_deltas,
                &alerts,
            );
            pending_l1_diagnosis = Some((command_generation, diagnosis.clone()));
            if let Some(response) = handle_command(
                &text,
                &l0_classifier,
                &mut l1_worker,
                &config,
                command_generation,
                latest_snapshot.as_ref(),
                &mut pending_confirmation,
                Some(&l1_prompt_context(
                    &text,
                    latest_snapshot.as_ref(),
                    latest_state,
                    &latest_deltas,
                    &alerts,
                    &diagnosis,
                )),
            ) {
                push_console(&mut console, "ROUTER", LogLevel::Info, response.clone());
                banner = Some(CommandBanner::new(response, Instant::now()));
            }
            state_dirty = true;
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
            state_dirty = true;
        }

        // Expire transient replies before publishing the next state snapshot.
        if banner
            .as_ref()
            .is_some_and(|banner| banner.is_expired(Instant::now()))
        {
            banner = None;
            state_dirty = true;
        }

        if state_dirty {
            shared_state.store(Arc::new(display_state(
                latest_snapshot
                    .as_ref()
                    .expect("sentinel samples before publishing display state"),
                latest_state,
                &alerts,
                banner.as_ref().map(|banner| banner.text.as_str()),
                &config,
                &console,
                Some(&deterministic_diagnosis(
                    latest_snapshot.as_ref(),
                    latest_state,
                    &latest_deltas,
                    &alerts,
                )),
            )));
            state_dirty = false;
        }

        let wait_for = sentinel_wait_duration(next_sample_at, Instant::now());
        match action_rx.recv_timeout(wait_for) {
            Ok(action) => {
                let was_resume = matches!(action, UserAction::Resume(_));
                let response = if matches!(
                    action,
                    UserAction::ArmSuggestedKill(_) | UserAction::ArmSuggestedFreeze(_)
                ) {
                    arm_suggested_action(
                        action,
                        latest_snapshot.as_ref(),
                        latest_state,
                        &latest_deltas,
                        &alerts,
                        &config,
                        &mut pending_confirmation,
                    )
                } else {
                    handle_user_action(
                        action,
                        latest_snapshot.as_ref(),
                        &config,
                        &mut alerts,
                        &mut alert_targets,
                        &mut pending_confirmation,
                        &event_writer,
                    )
                };
                if let Some(response) = response {
                    banner = Some(CommandBanner::new(response.clone(), Instant::now()));
                    push_console(&mut console, "SENTINEL", LogLevel::Info, response);
                    state_dirty = true;
                } else if was_resume {
                    push_console(
                        &mut console,
                        "SENTINEL",
                        LogLevel::Info,
                        "[SIGCONT] Proces wznowiony".to_owned(),
                    );
                    state_dirty = true;
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
#[allow(clippy::too_many_arguments)]
fn handle_command(
    text: &str,
    classifier: &l0::IntentClassifier,
    l1_worker: &mut L1Worker,
    config: &Config,
    generation: u64,
    snapshot: Option<&SystemSnapshot>,
    pending_confirmation: &mut Option<PendingConfirmation>,
    l1_context: Option<&str>,
) -> Option<String> {
    // Handle confirmation/cancellation commands first (no L0 classification needed)
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
    if matches!(
        text.trim().to_lowercase().as_str(),
        HELP_COMMAND | "help" | "pomoc" | "komendy" | "commands" | "?"
    ) {
        pending_confirmation.take();
        return Some(executable_commands_help());
    }
    if let Some(response) = local_system_response(text, snapshot) {
        pending_confirmation.take();
        return Some(response);
    }

    // Triage: L0 intent classification for regular commands
    let intent = classifier.classify(text);

    match &intent {
        l0::Intent::Unknown { .. } => {
            pending_confirmation.take();
            // Fallback: check if router recognizes it as a command
            if let router::Action::QueryL1(query) = router::parse(text) {
                if !l1_worker.submit(l1_context.unwrap_or(&query).to_owned(), generation) {
                    return Some("L1 busy — polecenie nie zostało zakolejkowane".to_owned());
                }
                return Some("L1: oczekiwanie na lokalny model…".to_owned());
            }
        }
        l0::Intent::Analysis { .. } => {
            pending_confirmation.take();
            // Submit to L1 worker
            if !l1_worker.submit(l1_context.unwrap_or(text).to_owned(), generation) {
                return Some("L1 busy — polecenie nie zostało zakolejkowane".to_owned());
            }
            return Some("L1: oczekiwanie na lokalny model…".to_owned());
        }
        l0::Intent::Command { .. } => {
            // Fall through to grammar parser
        }
    }

    // For Command intent (or fallback recognized command), parse and execute via grammar
    let action = router::parse(text);
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

fn local_system_response(text: &str, snapshot: Option<&SystemSnapshot>) -> Option<String> {
    let normalized = text
        .trim()
        .to_lowercase()
        .trim_end_matches(['?', '!'])
        .to_owned();
    let kind = match normalized.as_str() {
        "status" | "stan systemu" | "system" | "monitoring" => "status",
        "procesy" | "processes" | "lista procesow" | "lista procesów" => "processes",
        "ram" | "pamiec" | "pamięć" => "ram",
        "cpu" | "procesor" => "cpu",
        _ => return None,
    };
    let Some(snapshot) = snapshot else {
        return Some("Sentinel nie dostarczył jeszcze snapshotu systemu.".to_owned());
    };
    let ram_pct = if snapshot.ram_total_bytes == 0 {
        0.0
    } else {
        snapshot.ram_used_bytes as f64 * 100.0 / snapshot.ram_total_bytes as f64
    };
    match kind {
        "status" => Some(format!(
            "Lokalny status Sentinel:\nCPU: {:.1}%\nRAM: {:.1}% ({:.1}/{:.1} GiB)\nProcesów: {}",
            snapshot.cpu_usage_pct,
            ram_pct,
            snapshot.ram_used_bytes as f64 / (1024.0 * 1024.0 * 1024.0),
            snapshot.ram_total_bytes as f64 / (1024.0 * 1024.0 * 1024.0),
            snapshot.processes.len()
        )),
        "ram" => Some(format!(
            "RAM: {:.1}% ({:.1}/{:.1} GiB)",
            ram_pct,
            snapshot.ram_used_bytes as f64 / (1024.0 * 1024.0 * 1024.0),
            snapshot.ram_total_bytes as f64 / (1024.0 * 1024.0 * 1024.0)
        )),
        "cpu" => Some(format!("CPU: {:.1}%", snapshot.cpu_usage_pct)),
        "processes" => {
            let mut processes = snapshot.processes.iter().collect::<Vec<_>>();
            processes.sort_by_key(|process| std::cmp::Reverse(process.rss_bytes));
            let lines = processes
                .into_iter()
                .take(10)
                .map(|process| {
                    format!(
                        "{} (PID {}, RSS {:.1} MiB, CPU {:.1}%)",
                        process.name,
                        process.pid,
                        process.rss_bytes as f64 / (1024.0 * 1024.0),
                        process.cpu_pct
                    )
                })
                .collect::<Vec<_>>();
            Some(if lines.is_empty() {
                "Brak procesów w snapshotcie Sentinel.".to_owned()
            } else {
                format!("Procesy wg RSS:\n{}", lines.join("\n"))
            })
        }
        _ => unreachable!(),
    }
}

fn executable_commands_help() -> String {
    "Wykonywalne polecenia SAC:\n\
     • zabij/ubij/kill <proces> — zakończ proces (wymaga potwierdzenia)\n\
     • zamroz/freeze/pause <proces> — zamroź proces (SIGSTOP)\n\
     • wznow/resume/unpause <PID> — wznów proces (SIGCONT)\n\
     • wyczysc cache/drop caches — wyczyść page cache (wymaga potwierdzenia)\n\
     • pomoc/help/komendy — pokaż tę listę\n\
Pytania opisowe są przekazywane do lokalnego L1 i nie wykonują zmian same z siebie.\n\
Przy zamrożonym procesie możesz też użyć przycisków Wznów/Zakończ w alercie."
        .to_owned()
}

fn l0_classifier_config(config: &Config) -> l0::ClassifierConfig {
    let model_path = config.l0.model_path.trim();
    l0::ClassifierConfig {
        use_model: config.l0.use_model,
        model_path: (!model_path.is_empty()).then(|| model_path.to_owned()),
        confidence_threshold: config.l0.confidence_threshold,
        verbose: false,
    }
}

fn system_state_name(state: SystemState) -> &'static str {
    match state {
        SystemState::Green => "GREEN",
        SystemState::Yellow => "YELLOW",
        SystemState::Red => "RED",
    }
}

fn deterministic_diagnosis(
    snapshot: Option<&SystemSnapshot>,
    state: SystemState,
    deltas: &[ProcessDelta],
    alerts: &BTreeMap<u32, Alert>,
) -> DeterministicDiagnosis {
    let Some(snapshot) = snapshot else {
        return DeterministicDiagnosis {
            diagnosis: "Brak aktualnego snapshotu Sentinel — nie wykonano diagnozy.".to_owned(),
            cause_facts: "Brak wystarczających danych telemetrycznych.".to_owned(),
            suggested_action: SuggestedAction::Ignore,
            allowed_pids: Vec::new(),
            allowed_process_names: Vec::new(),
        };
    };

    let allowed_pids = snapshot
        .processes
        .iter()
        .map(|process| process.pid)
        .collect();
    let allowed_process_names = snapshot
        .processes
        .iter()
        .map(|process| process.name.clone())
        .collect();
    let ram_pct = if snapshot.ram_total_bytes == 0 {
        0.0
    } else {
        snapshot.ram_used_bytes as f64 * 100.0 / snapshot.ram_total_bytes as f64
    };

    let candidate = alerts
        .keys()
        .filter_map(|pid| {
            snapshot
                .processes
                .iter()
                .find(|process| process.pid == *pid)
        })
        .next()
        .or_else(|| {
            deltas
                .iter()
                .filter(|delta| delta.delta_bytes_per_sec > 0)
                .max_by_key(|delta| delta.delta_bytes_per_sec)
                .and_then(|delta| snapshot.processes.iter().find(|p| p.pid == delta.pid))
        });

    let Some(process) = candidate else {
        return DeterministicDiagnosis {
            diagnosis: format!(
                "Stan zasobów jest {} bez procesu wskazanego jako sprawca wzrostu.",
                system_state_name(state)
            ),
            cause_facts: format!(
                "Snapshot Sentinel raportuje RAM {:.1}% i brak dodatniego przyrostu procesu.",
                ram_pct
            ),
            suggested_action: SuggestedAction::Ignore,
            allowed_pids,
            allowed_process_names,
        };
    };

    let growth_mb_s = deltas
        .iter()
        .find(|delta| delta.pid == process.pid)
        .map(|delta| delta.delta_bytes_per_sec.max(0) as f64 / (1024.0 * 1024.0))
        .unwrap_or(0.0);
    let frozen = alerts.contains_key(&process.pid);
    let identity = process.identity();
    let status = if frozen { "FROZEN(SIGSTOP)" } else { "running" };
    let diagnosis = match state {
        SystemState::Red => format!(
            "Wykryto krytyczny stan RED związany z procesem {} (PID {}).",
            process.name, process.pid
        ),
        SystemState::Yellow => format!(
            "Stan YELLOW wskazuje na podwyższone obciążenie procesu {} (PID {}).",
            process.name, process.pid
        ),
        SystemState::Green => format!(
            "System jest stabilny; proces {} (PID {}) nie wymaga interwencji.",
            process.name, process.pid
        ),
    };
    let cause_facts = format!(
        "Sentinel raportuje RAM {:.1}%, proces {} (PID {}) ma RSS {:.1} MiB, przyrost {:.1} MB/s i status {}.",
        ram_pct,
        process.name,
        process.pid,
        process.rss_bytes as f64 / (1024.0 * 1024.0),
        growth_mb_s,
        status
    );
    let suggested_action = match state {
        SystemState::Red => SuggestedAction::Kill(identity),
        SystemState::Yellow if growth_mb_s > 0.0 && !frozen => SuggestedAction::Freeze(identity),
        SystemState::Green | SystemState::Yellow => SuggestedAction::Ignore,
    };

    DeterministicDiagnosis {
        diagnosis,
        cause_facts,
        suggested_action,
        allowed_pids,
        allowed_process_names,
    }
}

fn l1_prompt_context(
    query: &str,
    snapshot: Option<&SystemSnapshot>,
    state: SystemState,
    deltas: &[ProcessDelta],
    alerts: &BTreeMap<u32, Alert>,
    diagnosis: &DeterministicDiagnosis,
) -> String {
    let Some(snapshot) = snapshot else {
        return format!(
            "Deterministic diagnosis: {}\nFacts: {}\nSuggested action: {}\nUser query:\n{query}",
            diagnosis.diagnosis,
            diagnosis.cause_facts,
            diagnosis.suggested_command()
        );
    };
    let ram_pct = if snapshot.ram_total_bytes == 0 {
        0.0
    } else {
        snapshot.ram_used_bytes as f64 * 100.0 / snapshot.ram_total_bytes as f64
    };
    let mut processes = snapshot.processes.iter().collect::<Vec<_>>();
    processes.sort_by_key(|process| std::cmp::Reverse(process.rss_bytes));
    let top_processes = processes
        .into_iter()
        .take(3)
        .map(|process| {
            let growth_mb_s = deltas
                .iter()
                .find(|delta| delta.pid == process.pid)
                .map(|delta| delta.delta_bytes_per_sec as f64 / (1024.0 * 1024.0))
                .unwrap_or(0.0);
            let process_state = if alerts.contains_key(&process.pid) {
                "FROZEN(SIGSTOP)"
            } else {
                "running"
            };
            format!(
                "{} (PID {}, RSS {:.1} MiB, CPU {:.1}%, growth {:.1} MB/s, status {})",
                process.name,
                process.pid,
                process.rss_bytes as f64 / (1024.0 * 1024.0),
                process.cpu_pct,
                growth_mb_s,
                process_state
            )
        })
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "Deterministic diagnosis: {}\nDeterministic facts: {}\nDeterministic suggested action: {}\nSentinel grounding snapshot: state={}; CPU={:.1}%; RAM={:.1}% ({:.1}/{:.1} GiB); top 3 processes={}. Frozen processes are marked FROZEN(SIGSTOP).\nUser query:\n{query}",
        diagnosis.diagnosis,
        diagnosis.cause_facts,
        diagnosis.suggested_command(),
        system_state_name(state),
        snapshot.cpu_usage_pct,
        ram_pct,
        snapshot.ram_used_bytes as f64 / (1024.0 * 1024.0 * 1024.0),
        snapshot.ram_total_bytes as f64 / (1024.0 * 1024.0 * 1024.0),
        top_processes
    )
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
            if matches!(action, router::Action::Kill(_) | router::Action::Freeze(_)) {
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
                let guard_action = match action {
                    router::Action::Kill(_) => GuardAction::Kill { pid: identity.pid },
                    router::Action::Freeze(_) => GuardAction::Freeze { pid: identity.pid },
                    _ => unreachable!(),
                };
                return match guard::execute_for(&guard_action, &identity, config) {
                    Ok(()) => Some(match guard_action {
                        GuardAction::Kill { .. } => format!("Zamknięto {}", identity.name),
                        GuardAction::Freeze { .. } => format!("Zamrożono {}", identity.name),
                        _ => unreachable!(),
                    }),
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
        if let router::Action::QueryL1(query) = action {
            if !l1_worker.submit(query, generation) {
                return Some("L1 busy — polecenie nie zostało zakolejkowane".to_owned());
            }
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
    response_rx: Receiver<(u64, String, router::l1_client::DLPReport)>,
}

impl L1Worker {
    #[cfg(test)]
    fn spawn(config: &Config) -> Self {
        Self::spawn_with_audit(config, None)
    }

    fn spawn_with_audit(config: &Config, audit_recorder: Option<EventRecorder>) -> Self {
        let (request_tx, request_rx) = mpsc::sync_channel::<(u64, String)>(4);
        let (response_tx, response_rx) =
            mpsc::channel::<(u64, String, router::l1_client::DLPReport)>();
        let provider = config.l1_provider.clone().validated();
        thread::Builder::new()
            .name("l1-client".to_owned())
            .spawn(move || {
                while let Ok((generation, prompt)) = request_rx.recv() {
                    let (sanitized, dlp_report) = sanitize_prompt(&prompt);
                    let redaction_timestamp = unix_timestamp_millis();
                    if let (Some(recorder), Some(event)) = (
                        audit_recorder.as_ref(),
                        dlp_redaction_event(&dlp_report, redaction_timestamp),
                    ) {
                        if let Err(error) = recorder.record_confirmed(event) {
                            tracing::error!(%error, "could not queue pre-request DLP audit event");
                        }
                    }
                    // Rebuild the local client for each request. LM Studio may
                    // start after SAC, restart, or briefly drop its listener;
                    // a client captured at worker startup would stay offline
                    // forever after the first failed initialization.
                    let response = if provider.enabled && provider.backend == "openai_compatible" {
                        match L1Client::new(L1Config {
                            endpoint: provider.base_url.clone(),
                            model: provider.model.clone(),
                            max_tokens: provider.max_tokens,
                            temperature: provider.temperature,
                            timeout: Duration::from_millis(provider.timeout_ms),
                            api_key: None,
                        }) {
                            Ok(client) => client
                                .complete_sanitized(&prompt, &sanitized)
                                .unwrap_or_else(|error| format!("L1 request failed: {error}")),
                            Err(error) => format!("L1 request failed: {error}"),
                        }
                    } else {
                        L1_OFFLINE_MESSAGE.to_owned()
                    };
                    if response_tx
                        .send((generation, response, dlp_report.clone()))
                        .is_err()
                    {
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

    fn try_response(&self) -> Option<(u64, String, router::l1_client::DLPReport)> {
        self.response_rx.try_recv().ok()
    }
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
        UserAction::ArmSuggestedKill(_) | UserAction::ArmSuggestedFreeze(_) => {
            unreachable!("suggested actions do not execute directly")
        }
    }
}

fn arm_suggested_action(
    action: UserAction,
    snapshot: Option<&SystemSnapshot>,
    state: SystemState,
    deltas: &[ProcessDelta],
    alerts: &BTreeMap<u32, Alert>,
    config: &Config,
    pending_confirmation: &mut Option<PendingConfirmation>,
) -> Option<String> {
    let (expected_pid, expected_kind) = match action {
        UserAction::ArmSuggestedKill(pid) => (pid, "ubij"),
        UserAction::ArmSuggestedFreeze(pid) => (pid, "zamroz"),
        UserAction::Resume(_) | UserAction::Terminate(_) => {
            unreachable!("only suggested actions reach this function")
        }
    };
    let diagnosis = deterministic_diagnosis(snapshot, state, deltas, alerts);
    let (router_action, identity) = match (&diagnosis.suggested_action, expected_kind) {
        (SuggestedAction::Kill(identity), "ubij") if expected_pid == identity.pid => {
            if sentinel::policy::is_immune(&identity.name, config) {
                return Some(format!(
                    "{} jest na liście procesów chronionych — akcja zablokowana",
                    identity.name
                ));
            }
            (
                router::Action::Kill(identity.name.clone()),
                identity.clone(),
            )
        }
        (SuggestedAction::Freeze(identity), "zamroz") if expected_pid == identity.pid => (
            router::Action::Freeze(identity.name.clone()),
            identity.clone(),
        ),
        _ => {
            return Some(
                "Diagnoza wygasła lub tożsamość procesu zmieniła się — akcja zablokowana"
                    .to_owned(),
            );
        }
    };
    if pending_confirmation
        .as_ref()
        .is_some_and(|pending| Instant::now() < pending.expires_at)
    {
        return Some("POTWIERDZENIE: Oczekuje już decyzja dla akcji".to_owned());
    }
    *pending_confirmation = Some(PendingConfirmation {
        action: PendingAction::Router {
            action: router_action,
            identity: Some(identity.clone()),
        },
        expires_at: Instant::now() + CONFIRMATION_TTL,
    });
    Some(format!(
        "POTWIERDZENIE: Wykonanie '{}' dla procesu '{}' (PID {})\nNaciśnij Enter lub wybierz Potwierdź, aby wykonać.",
        expected_kind,
        identity.name,
        identity.pid
    ))
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
        UserAction::ArmSuggestedKill(_) | UserAction::ArmSuggestedFreeze(_) => {
            unreachable!("suggested actions are handled before alert actions")
        }
    };

    let Some(identity) = alert_targets.get(&pid).cloned() else {
        tracing::warn!(pid, "user action refused: no verified alert identity");
        return Some("Brak zweryfikowanej tożsamości alertu — akcja zablokowana".to_owned());
    };
    if matches!(action, UserAction::Terminate(_)) {
        if let Some(pending) = pending_confirmation.as_ref() {
            if Instant::now() < pending.expires_at {
                return Some(
                    "POTWIERDZENIE: Oczekuje już decyzja dla akcji destrukcyjnej".to_owned(),
                );
            }
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
    console: &VecDeque<ConsoleEntry>,
    diagnosis: Option<&DeterministicDiagnosis>,
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
        suggested_action: diagnosis.and_then(|diagnosis| match &diagnosis.suggested_action {
            SuggestedAction::Kill(identity) => Some(hud::SuggestedAction::Kill(identity.pid)),
            SuggestedAction::Freeze(identity) => Some(hud::SuggestedAction::Freeze(identity.pid)),
            SuggestedAction::Ignore => None,
        }),
        console: console.clone(),
        thresholds: DisplayThresholds {
            cpu_yellow_pct: config.thresholds.cpu.yellow_pct,
            ram_yellow_pct: config.thresholds.ram.yellow_pct,
            ram_red_pct: config.thresholds.ram.red_pct,
            vram_yellow_pct: config.thresholds.vram.yellow_pct,
            vram_red_pct: config.thresholds.vram.red_pct,
        },
    }
}

fn push_console(
    console: &mut VecDeque<ConsoleEntry>,
    source: &'static str,
    level: LogLevel,
    text: String,
) {
    console.push_back(ConsoleEntry {
        timestamp: Instant::now(),
        source,
        level,
        text,
    });
    while console.len() > 100 {
        console.pop_front();
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

fn dlp_redaction_event(report: &DLPReport, timestamp: i64) -> Option<SystemEvent> {
    if report.redaction_count == 0 {
        return None;
    }

    let details = format!(
        "count={}; types={}",
        report.redaction_count,
        report.rules_applied.join(",")
    );
    Some(SystemEvent {
        timestamp,
        event_type: EventType::DLPRedactionApplied,
        target_pid: None,
        process_name: None,
        ram_usage_mb: None,
        cpu_usage_pct: None,
        vram_usage_mb: None,
        details: Some(details),
    })
}

fn unix_timestamp_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

fn format_l1_response(response: String, report: &DLPReport) -> String {
    if report.redaction_count == 0 {
        return response;
    }
    format!(
        "[DLP] Zanonimizowano {} sekretów przed wysłaniem do L1 (typy: {})\n{}",
        report.redaction_count,
        report.rules_applied.join(", "),
        response
    )
}

struct ValidatedL1Explanation {
    status: &'static str,
    text: Option<String>,
}

fn validate_l1_explanation(
    response: String,
    diagnosis: &DeterministicDiagnosis,
) -> ValidatedL1Explanation {
    let trimmed = response.trim();
    if trimmed.is_empty()
        || trimmed.starts_with("L1 request failed:")
        || trimmed.starts_with("L1 offline")
        || has_repeated_content(trimmed)
    {
        return ValidatedL1Explanation {
            status: "[L1 UNVERIFIED]",
            text: None,
        };
    }

    let Some(cause) = trimmed.strip_prefix("Przyczyna:").map(str::trim) else {
        return ValidatedL1Explanation {
            status: "[L1 UNVERIFIED]",
            text: None,
        };
    };
    if cause.is_empty()
        || trimmed.contains("Diagnoza:")
        || trimmed.contains("Sugerowana komenda:")
        || contains_unknown_entities(cause, diagnosis)
    {
        return ValidatedL1Explanation {
            status: "[L1 UNVERIFIED]",
            text: None,
        };
    }
    ValidatedL1Explanation {
        status: "[L1 VERIFIED]",
        text: Some(cause.to_owned()),
    }
}

fn contains_unknown_entities(text: &str, diagnosis: &DeterministicDiagnosis) -> bool {
    let words = text.split_whitespace().collect::<Vec<_>>();
    for (index, word) in words.iter().enumerate() {
        let normalized = word.trim_matches(|character: char| {
            !character.is_ascii_alphanumeric() && character != '_' && character != '-'
        });
        let lower = normalized.to_ascii_lowercase();
        if lower == "pid" {
            let Some(next) = words.get(index + 1) else {
                return true;
            };
            let number = next.trim_matches(|character: char| !character.is_ascii_digit());
            if number.is_empty()
                || number
                    .parse::<u32>()
                    .map_or(true, |pid| !diagnosis.allowed_pids.contains(&pid))
            {
                return true;
            }
        }
        if lower == "proces" || lower == "process" {
            let Some(next) = words.get(index + 1) else {
                return true;
            };
            let name = next.trim_matches(|character: char| {
                !character.is_ascii_alphanumeric() && character != '_' && character != '-'
            });
            if name.is_empty()
                || !diagnosis
                    .allowed_process_names
                    .iter()
                    .any(|known| known.eq_ignore_ascii_case(name))
            {
                return true;
            }
        }
    }
    false
}

fn has_repeated_content(text: &str) -> bool {
    let lines = text.lines().map(str::trim).filter(|line| !line.is_empty());
    let mut counts = std::collections::HashMap::new();
    for line in lines {
        let count = counts.entry(line).or_insert(0_u8);
        *count = count.saturating_add(1);
        if *count >= 3 {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use hud::SystemState as HudState;
    use sentinel::{ProcessInfo, SystemSnapshot};
    use std::io::{Read, Write};
    use std::net::TcpListener;

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

    fn test_handle_command(
        text: &str,
        l1_worker: &mut L1Worker,
        config: &Config,
        generation: u64,
        snapshot: Option<&SystemSnapshot>,
        pending_confirmation: &mut Option<PendingConfirmation>,
    ) -> Option<String> {
        let classifier = l0::IntentClassifier::new(l0_classifier_config(config));
        handle_command(
            text,
            &classifier,
            l1_worker,
            config,
            generation,
            snapshot,
            pending_confirmation,
            None,
        )
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

        let response =
            test_handle_command("ask local model", &mut worker, &config, 1, None, &mut None);

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

        let response = test_handle_command(
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

        let response = test_handle_command(
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

        let response = test_handle_command(
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

        let response =
            test_handle_command(CONFIRM_COMMAND, &mut worker, &config, 1, None, &mut pending);

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

        let response =
            test_handle_command(CONFIRM_COMMAND, &mut worker, &config, 1, None, &mut pending);

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

        let _ = test_handle_command("status", &mut worker, &config, 1, None, &mut pending);
        assert!(pending.is_none());

        pending = Some(PendingConfirmation {
            action: PendingAction::Router {
                action: router::Action::CleanCache,
                identity: None,
            },
            expires_at: Instant::now() + CONFIRMATION_TTL,
        });
        let response =
            test_handle_command(CANCEL_COMMAND, &mut worker, &config, 2, None, &mut pending);
        assert_eq!(response.as_deref(), Some("Anulowano oczekującą akcję"));
        assert!(pending.is_none());
    }

    #[test]
    fn l0_and_router_share_system_command_aliases() {
        let classifier = l0::IntentClassifier::new(l0::ClassifierConfig::default());
        let cases = [
            ("ubij chrome", router::Action::Kill("chrome".to_owned())),
            ("pause java", router::Action::Freeze("java".to_owned())),
            ("drop caches", router::Action::CleanCache),
        ];

        for (input, expected) in cases {
            assert!(matches!(
                classifier.classify(input),
                l0::Intent::Command { .. }
            ));
            assert_eq!(router::parse(input), expected);
        }
    }

    #[test]
    fn l0_aliases_enter_system_execution_path() {
        let mut config = Config::default();
        config.l1_provider.enabled = false;
        config.guard.immune_process_names.push("java".to_owned());
        let mut worker = L1Worker::spawn(&config);
        let mut chrome_snapshot = snapshot();
        chrome_snapshot.processes[0].name = "chrome".to_owned();

        let mut pending = None;
        let kill_response = test_handle_command(
            "ubij chrome",
            &mut worker,
            &config,
            1,
            Some(&chrome_snapshot),
            &mut pending,
        )
        .expect("kill alias response");
        assert!(kill_response.starts_with("POTWIERDZENIE:"));
        assert!(matches!(
            pending.as_ref().map(|value| &value.action),
            Some(PendingAction::Router {
                action: router::Action::Kill(name),
                ..
            }) if name == "chrome"
        ));

        pending = None;
        let freeze_response =
            test_handle_command("pause java", &mut worker, &config, 2, None, &mut pending);
        assert_eq!(
            freeze_response.as_deref(),
            Some("Błąd: java jest na liście procesów chronionych — akcja zablokowana")
        );
        assert!(pending.is_none());

        let cache_response =
            test_handle_command("drop caches", &mut worker, &config, 3, None, &mut pending)
                .expect("cache alias response");
        assert!(cache_response.starts_with("POTWIERDZENIE:"));
        assert!(matches!(
            pending.as_ref().map(|value| &value.action),
            Some(PendingAction::Router {
                action: router::Action::CleanCache,
                ..
            })
        ));
    }

    #[test]
    fn unknown_intent_is_queued_for_l1() {
        let mut config = Config::default();
        config.l1_provider.enabled = false;
        let mut worker = L1Worker::spawn(&config);

        let response = test_handle_command("abc123xyz", &mut worker, &config, 9, None, &mut None);
        assert_eq!(
            response.as_deref(),
            Some("L1: oczekiwanie na lokalny model…")
        );

        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            if let Some((generation, response, _)) = worker.try_response() {
                assert_eq!(generation, 9);
                assert_eq!(response, L1_OFFLINE_MESSAGE);
                break;
            }
            assert!(
                Instant::now() < deadline,
                "L1 worker did not receive prompt"
            );
            thread::yield_now();
        }
    }

    #[test]
    fn help_lists_only_executable_commands_without_calling_l1() {
        let mut config = Config::default();
        config.l1_provider.enabled = false;
        let mut worker = L1Worker::spawn(&config);

        let response = test_handle_command("pomoc", &mut worker, &config, 10, None, &mut None)
            .expect("help response");

        assert!(response.contains("Wykonywalne polecenia SAC"));
        assert!(response.contains("kill"));
        assert!(response.contains("drop caches"));
        assert!(worker.try_response().is_none());
    }

    #[test]
    fn system_queries_use_local_snapshot_instead_of_l1() {
        let status = local_system_response("stan systemu", Some(&snapshot())).unwrap();
        assert!(status.contains("CPU: 12.5%"));
        assert!(status.contains("RAM: 25.0%"));

        let processes = local_system_response("procesy", Some(&snapshot())).unwrap();
        assert!(processes.contains("cargo (PID 42"));
    }

    #[test]
    fn l1_validator_marks_structured_and_fallback_responses() {
        let frame =
            deterministic_diagnosis(Some(&snapshot()), SystemState::Green, &[], &BTreeMap::new());
        let verified =
            validate_l1_explanation("Przyczyna: proces cargo ma RSS 512 MiB.".to_owned(), &frame);
        assert_eq!(verified.status, "[L1 VERIFIED]");

        let timeout =
            validate_l1_explanation("L1 request failed: L1 request timed out".to_owned(), &frame);
        assert_eq!(timeout.status, "[L1 UNVERIFIED]");
        assert!(timeout.text.is_none());

        let repeated = validate_l1_explanation("to samo\nto samo\nto samo".to_owned(), &frame);
        assert!(repeated.text.is_none());

        let unknown_pid =
            validate_l1_explanation("Przyczyna: proces cargo ma PID 99999.".to_owned(), &frame);
        assert_eq!(unknown_pid.status, "[L1 UNVERIFIED]");

        let model_owned_fields = validate_l1_explanation(
            "Przyczyna: proces cargo rośnie.\nSugerowana komenda: ubij 42".to_owned(),
            &frame,
        );
        assert_eq!(model_owned_fields.status, "[L1 UNVERIFIED]");
    }

    #[test]
    fn deterministic_diagnosis_owns_action_and_facts() {
        let process = snapshot().processes[0].clone();
        let diagnosis = deterministic_diagnosis(
            Some(&snapshot()),
            SystemState::Red,
            &[ProcessDelta {
                pid: process.pid,
                name: process.name.clone(),
                start_time: process.start_time,
                rss_bytes: process.rss_bytes,
                delta_bytes_per_sec: 768 * 1024 * 1024,
            }],
            &BTreeMap::new(),
        );
        assert!(diagnosis.diagnosis.contains("PID 42"));
        assert_eq!(diagnosis.suggested_command(), "ubij 42");
        assert!(diagnosis.cause_facts.contains("768.0 MB/s"));
        assert!(diagnosis
            .render(Some("proces cargo ma wysoki RSS."))
            .contains("Przyczyna: Sentinel raportuje"));
        assert!(diagnosis
            .render(Some("proces cargo ma wysoki RSS."))
            .contains("Objaśnienie L1: proces cargo ma wysoki RSS."));
    }

    #[test]
    fn suggested_action_rechecks_current_identity_before_arming() {
        let mut pending = None;
        let config = Config::default();
        let delta = ProcessDelta {
            pid: 42,
            name: "cargo".to_owned(),
            start_time: 1,
            rss_bytes: 512 * 1024 * 1024,
            delta_bytes_per_sec: 768 * 1024 * 1024,
        };
        let response = arm_suggested_action(
            UserAction::ArmSuggestedKill(42),
            Some(&snapshot()),
            SystemState::Red,
            &[delta],
            &BTreeMap::new(),
            &config,
            &mut pending,
        );
        assert!(response
            .as_deref()
            .is_some_and(|text| text.starts_with("POTWIERDZENIE:")));
        assert!(matches!(
            pending,
            Some(PendingConfirmation {
                action: PendingAction::Router {
                    action: router::Action::Kill(_),
                    identity: Some(_),
                },
                ..
            })
        ));

        let mut stale_pending = None;
        let stale_response = arm_suggested_action(
            UserAction::ArmSuggestedKill(99999),
            Some(&snapshot()),
            SystemState::Red,
            &[],
            &BTreeMap::new(),
            &config,
            &mut stale_pending,
        );
        assert!(stale_response
            .as_deref()
            .is_some_and(|text| text.contains("zablokowana")));
        assert!(stale_pending.is_none());
    }

    #[test]
    fn user_scenario_matrix_60_inputs() {
        let inputs = [
            "pomoc",
            "help",
            "komendy",
            "commands",
            "?",
            "status",
            "stan systemu",
            "system",
            "monitoring",
            "procesy",
            "processes",
            "lista procesow",
            "ram",
            "pamiec",
            "cpu",
            "procesor",
            "kill chrome",
            "zabij firefox",
            "ubij node",
            "zamknij cargo",
            "terminate java",
            "freeze chrome",
            "zamroz firefox",
            "pause node",
            "stop cargo",
            "wstrzymaj java",
            "resume 42",
            "wznow 42",
            "unpause 42",
            "odblokuj 42",
            "odmroz 42",
            "drop caches",
            "wyczysc cache",
            "clear cache",
            "clean cache",
            "dlaczego ram rosnie?",
            "why is cpu high?",
            "what is system state?",
            "jakie procesy dzialaja?",
            "wyjasnij alert",
            "explain memory",
            "analizuj ten stan",
            "analyze current load",
            "czy system jest bezpieczny?",
            "tak",
            "nie",
            "co widzisz?",
            "raport",
            "sprawdz system",
            "jaki jest stan?",
            "pamięć",
            "lista procesów",
            "UBIJ chrome",
            "zamroź firefox",
            "dlaczego pamięć rośnie?",
            "co obciąża CPU?",
            "wyjaśnij wzrost RAM",
            "czy jest problem?",
            "opisz sytuację",
            "co się stało?",
        ];
        assert_eq!(inputs.len(), 60);

        let config = Config::default();
        let classifier = l0::IntentClassifier::new(l0_classifier_config(&config));
        let snapshot = snapshot();
        let local = [
            "status",
            "stan systemu",
            "system",
            "monitoring",
            "procesy",
            "processes",
            "lista procesow",
            "lista procesów",
            "ram",
            "pamiec",
            "pamięć",
            "cpu",
            "procesor",
        ];
        let help = ["pomoc", "help", "komendy", "commands", "?"];
        let mut routed_commands = 0;
        let mut local_answers = 0;
        let mut help_answers = 0;
        let mut l1_questions = 0;

        for input in inputs {
            if help.contains(&input) {
                let response = executable_commands_help();
                assert!(response.contains("Wykonywalne polecenia SAC"), "{input}");
                help_answers += 1;
            } else if local.contains(&input) {
                let response = local_system_response(input, Some(&snapshot)).expect(input);
                assert!(!response.is_empty(), "{input}");
                local_answers += 1;
            } else if !matches!(router::parse(input), router::Action::QueryL1(_)) {
                routed_commands += 1;
            } else {
                let intent = classifier.classify(input);
                assert!(
                    matches!(
                        intent,
                        l0::Intent::Analysis { .. } | l0::Intent::Unknown { .. }
                    ),
                    "{input}"
                );
                l1_questions += 1;
            }
        }

        assert_eq!(help_answers, 5);
        assert_eq!(local_answers, 13);
        assert_eq!(routed_commands, 21);
        assert_eq!(l1_questions, 21);
    }

    #[test]
    fn sentinel_l0_config_maps_to_classifier_config() {
        let mut config = Config::default();
        config.l0.use_model = true;
        config.l0.model_path = " /tmp/intent.onnx ".to_owned();
        config.l0.confidence_threshold = 0.72;

        let mapped = l0_classifier_config(&config);
        assert!(mapped.use_model);
        assert_eq!(mapped.model_path.as_deref(), Some("/tmp/intent.onnx"));
        assert_eq!(mapped.confidence_threshold, 0.72);
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
            &VecDeque::new(),
            None,
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
            &VecDeque::new(),
            None,
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
            &VecDeque::new(),
            None,
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
    fn live_console_keeps_latest_100_entries() {
        let mut console = VecDeque::new();
        for index in 0..105 {
            push_console(
                &mut console,
                "ROUTER",
                LogLevel::Info,
                format!("entry-{index}"),
            );
        }

        assert_eq!(console.len(), 100);
        assert_eq!(
            console.front().map(|entry| entry.text.as_str()),
            Some("entry-5")
        );
        assert_eq!(
            console.back().map(|entry| entry.text.as_str()),
            Some("entry-104")
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

        assert!(response
            .as_deref()
            .is_some_and(|text| text.starts_with("POTWIERDZENIE:")));
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
    fn l1_handoff_is_nonblocking_when_provider_is_disabled() {
        let mut config = Config::default();
        config.l1_provider.enabled = false;
        let mut worker = L1Worker::spawn(&config);
        let started = Instant::now();

        assert!(worker.submit("private prompt must not be logged".to_owned(), 1));

        assert!(started.elapsed() < Duration::from_millis(100));
        let deadline = Instant::now() + Duration::from_secs(1);
        let response = loop {
            if let Some((_, response, _dlp)) = worker.try_response() {
                break response;
            }
            assert!(Instant::now() < deadline, "L1 worker did not respond");
            thread::yield_now();
        };
        assert_eq!(response, L1_OFFLINE_MESSAGE);
    }

    #[test]
    fn l1_http_error_still_persists_dlp_audit_event() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock L1 server");
        let address = listener.local_addr().expect("mock server address");
        let (request_seen_tx, request_seen_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let mock_server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept L1 request");
            let mut request = [0_u8; 4096];
            let size = stream.read(&mut request).expect("read L1 request");
            assert!(
                String::from_utf8_lossy(&request[..size]).starts_with("POST /v1/chat/completions")
            );
            request_seen_tx.send(()).expect("signal request received");
            release_rx.recv().expect("release mock response");
            stream
                .write_all(
                    b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .expect("return mock L1 failure");
        });

        let mut config = Config::default();
        config.l1_provider.enabled = true;
        config.l1_provider.base_url = format!("http://{address}/v1");
        config.l1_provider.timeout_ms = 1_000;
        let directory = tempfile::tempdir().expect("create temporary data directory");
        let database_path = directory.path().join("events.db");
        let writer = EventWriter::open(&database_path).expect("open event writer");
        let mut worker = L1Worker::spawn_with_audit(&config, Some(writer.recorder()));
        let prompt = "run command --passwd=StrictSecret123 now";
        let earliest_timestamp = unix_timestamp_millis();
        assert!(worker.submit(prompt.to_owned(), 1));
        request_seen_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("mock server received request");

        let connection = rusqlite::Connection::open(&database_path).expect("open audit database");
        let deadline = Instant::now() + Duration::from_secs(2);
        let (timestamp, event_type, details): (i64, String, String) = loop {
            let result = connection.query_row(
                "SELECT timestamp, event_type, details FROM system_events ORDER BY id DESC LIMIT 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            );
            match result {
                Ok(event) => break event,
                Err(rusqlite::Error::QueryReturnedNoRows) => {
                    assert!(Instant::now() < deadline, "DLP audit was not persisted");
                    thread::yield_now();
                }
                Err(error) => panic!("read DLP audit event: {error}"),
            }
        };
        assert!(
            worker.try_response().is_none(),
            "L1 response was still blocked"
        );
        assert!(timestamp >= earliest_timestamp);
        assert_eq!(event_type, "DLP_REDACTION_APPLIED");
        assert_eq!(details, "count=1; types=Command Password");
        assert!(!details.contains("StrictSecret123"));

        release_tx.send(()).expect("release mock response");

        let deadline = Instant::now() + Duration::from_secs(2);
        let report = loop {
            if let Some((_, response, report)) = worker.try_response() {
                assert!(response.contains("L1 request failed: L1 service returned HTTP 500"));
                break report;
            }
            assert!(Instant::now() < deadline, "L1 worker did not respond");
            thread::yield_now();
        };
        assert_eq!(report.redaction_count, 1);
        assert_eq!(report.rules_applied, vec!["Command Password"]);
        writer.shutdown().expect("flush event writer");
        mock_server.join().expect("mock L1 server thread");
    }

    #[test]
    fn dlp_hud_notice_contains_count_and_rule_types() {
        let report = DLPReport {
            redaction_count: 2,
            rules_applied: vec!["GitHub Token".to_owned(), "Command Password".to_owned()],
        };
        let message = format_l1_response("answer".to_owned(), &report);
        assert!(message.contains("2 sekretów"));
        assert!(message.contains("GitHub Token, Command Password"));
        assert!(message.ends_with("answer"));
    }
}
