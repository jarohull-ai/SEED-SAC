//! Mitigation actions for resource pressure.
//!
//! Yellow pressure lowers scheduling priority of the biggest offenders; red
//! pressure freezes the process that is currently allocating fastest instead of
//! killing it, so no unsaved work is lost.

use std::cell::RefCell;
use std::collections::HashSet;

use nix::sys::signal::Signal;

use crate::config::Config;
use crate::policy::is_immune;
use crate::process::ProcessDelta;
use crate::{ProcessIdentity, SystemState};

/// Number of processes that get deprioritised in the yellow state.
const YELLOW_OFFENDER_LIMIT: usize = 3;

#[derive(Default)]
struct YellowMitigationState {
    applied: HashSet<YellowIdentity>,
    reniced: HashSet<YellowIdentity>,
    ioniced: HashSet<YellowIdentity>,
}

#[derive(Debug, Clone, Hash, PartialEq, Eq)]
struct YellowIdentity {
    pid: u32,
    name: String,
    start_time: u64,
}

thread_local! {
    /// Yellow mitigation is stateful for the sampler/guard loop. Keeping it
    /// thread-local avoids sharing a loop's recovery state with another
    /// independent guard loop (and keeps the state lock-free).
    static YELLOW_STATE: RefCell<YellowMitigationState> = RefCell::new(YellowMitigationState::default());
}

/// A single mitigation command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GuardAction {
    Renice {
        pid: u32,
    },
    Ionice {
        pid: u32,
    },
    Freeze {
        pid: u32,
    },
    Thaw {
        pid: u32,
    },
    /// Continue a stopped process before requesting termination.
    Kill {
        pid: u32,
    },
    /// Kept for compatibility with older callers; prefer [`GuardAction::Kill`].
    Terminate {
        pid: u32,
    },
}

/// Decide which actions to take for the given state. Immune processes are
/// never touched.
pub fn mitigate(state: SystemState, deltas: &[ProcessDelta], config: &Config) -> Vec<GuardAction> {
    match state {
        SystemState::Green | SystemState::Red => {
            YELLOW_STATE.with(|state| {
                let mut state = state.borrow_mut();
                state.applied.clear();
                state.reniced.clear();
                state.ioniced.clear();
            });
            if state == SystemState::Green {
                return Vec::new();
            }

            let target = deltas
                .iter()
                .filter(|delta| {
                    config.guard.ram_spike_mb_per_sec.is_finite()
                        && config.guard.ram_spike_mb_per_sec >= 0.0
                        && (delta.delta_bytes_per_sec as f64)
                            > f64::from(config.guard.ram_spike_mb_per_sec) * 1024.0 * 1024.0
                })
                .filter(|delta| !is_immune(&delta.name, config))
                .max_by(|a, b| {
                    a.delta_bytes_per_sec
                        .cmp(&b.delta_bytes_per_sec)
                        .then_with(|| a.rss_bytes.cmp(&b.rss_bytes))
                        .then_with(|| b.pid.cmp(&a.pid))
                });

            target
                .map(|delta| vec![GuardAction::Freeze { pid: delta.pid }])
                .unwrap_or_default()
        }

        SystemState::Yellow => {
            let mut offenders: Vec<&ProcessDelta> = deltas
                .iter()
                .filter(|delta| !is_immune(&delta.name, config))
                .collect();
            offenders.sort_by(|a, b| {
                b.rss_bytes
                    .cmp(&a.rss_bytes)
                    .then_with(|| b.delta_bytes_per_sec.cmp(&a.delta_bytes_per_sec))
                    .then_with(|| a.pid.cmp(&b.pid))
            });

            YELLOW_STATE.with(|state| {
                let mut state = state.borrow_mut();
                let current: HashSet<YellowIdentity> = deltas
                    .iter()
                    .map(|delta| YellowIdentity {
                        pid: delta.pid,
                        name: delta.name.clone(),
                        start_time: delta.start_time,
                    })
                    .collect();
                // A vanished process is no longer considered mitigated. The
                // start time in the key also prevents PID reuse from inheriting
                // the old process's Yellow state.
                state.applied.retain(|identity| current.contains(identity));
                state.reniced.retain(|identity| current.contains(identity));
                state.ioniced.retain(|identity| current.contains(identity));

                let mut actions = Vec::with_capacity(YELLOW_OFFENDER_LIMIT * 2);
                for offender in offenders.into_iter().take(YELLOW_OFFENDER_LIMIT) {
                    let identity = YellowIdentity {
                        pid: offender.pid,
                        name: offender.name.clone(),
                        start_time: offender.start_time,
                    };
                    if !state.applied.contains(&identity) {
                        if !state.reniced.contains(&identity) {
                            actions.push(GuardAction::Renice { pid: offender.pid });
                        }
                        if !state.ioniced.contains(&identity) {
                            actions.push(GuardAction::Ionice { pid: offender.pid });
                        }
                    }
                }
                actions
            })
        }
    }
}

/// Run a mitigation action. Errors are reported as human-readable strings so
/// the caller can surface them in the HUD.
pub fn execute(action: &GuardAction, config: &Config) -> Result<(), String> {
    let identity = current_identity(action_pid(action))?;
    execute_for(action, &identity, config)
}

/// Execute only when the PID still has the expected kernel identity.
pub fn execute_for(
    action: &GuardAction,
    expected: &ProcessIdentity,
    config: &Config,
) -> Result<(), String> {
    if action_pid(action) != expected.pid {
        return Err("action PID does not match validated process identity".to_owned());
    }
    if matches!(
        action,
        GuardAction::Kill { .. } | GuardAction::Terminate { .. }
    ) && is_immune(&expected.name, config)
    {
        return Err(format!(
            "refusing to terminate immune process {}",
            expected.name
        ));
    }
    validate_identity(expected)?;
    let result = match *action {
        GuardAction::Renice { .. } | GuardAction::Ionice { .. } => {
            Err("identity-bound priority execution is unavailable; refusing unbound PID priority change".to_owned())
        }
        GuardAction::Freeze { .. } => send_signal_for(expected, Signal::SIGSTOP),
        GuardAction::Thaw { .. } => send_signal_for(expected, Signal::SIGCONT),
        GuardAction::Kill { .. } => {
            send_signal_for(expected, Signal::SIGCONT)?;
            send_signal_for(expected, Signal::SIGTERM)
        }
        // Callers that need to terminate a stopped process must issue Thaw
        // explicitly; keeping Terminate to one signal avoids duplicate thaw
        // races in HUD actions.
        GuardAction::Terminate { .. } => send_signal_for(expected, Signal::SIGTERM),
    };
    if result.is_ok() {
        record_yellow_success(action, expected);
    }
    result
}

fn record_yellow_success(action: &GuardAction, identity: &ProcessIdentity) {
    let yellow = YellowIdentity {
        pid: identity.pid,
        name: identity.name.clone(),
        start_time: identity.start_time,
    };
    YELLOW_STATE.with(|state| {
        let mut state = state.borrow_mut();
        match action {
            GuardAction::Renice { .. } => {
                state.reniced.insert(yellow.clone());
            }
            GuardAction::Ionice { .. } => {
                state.ioniced.insert(yellow.clone());
            }
            _ => return,
        }
        if state.reniced.contains(&yellow) && state.ioniced.contains(&yellow) {
            state.applied.insert(yellow);
        }
    });
}

fn action_pid(action: &GuardAction) -> u32 {
    match *action {
        GuardAction::Renice { pid }
        | GuardAction::Ionice { pid }
        | GuardAction::Freeze { pid }
        | GuardAction::Thaw { pid }
        | GuardAction::Kill { pid }
        | GuardAction::Terminate { pid } => pid,
    }
}

fn current_identity(pid: u32) -> Result<ProcessIdentity, String> {
    let mut system = sysinfo::System::new();
    system.refresh_processes(sysinfo::ProcessesToUpdate::All, true);
    let process = system
        .process(sysinfo::Pid::from_u32(pid))
        .ok_or_else(|| format!("process {pid} no longer exists"))?;
    Ok(ProcessIdentity {
        pid,
        name: process.name().to_string_lossy().into_owned(),
        start_time: process.start_time(),
    })
}

fn validate_identity(expected: &ProcessIdentity) -> Result<(), String> {
    if expected.pid == 0 || expected.pid > i32::MAX as u32 {
        return Err(format!("invalid pid {}", expected.pid));
    }
    if expected.pid == std::process::id() {
        return Err("refusing to signal the SAC process".to_owned());
    }
    let actual = current_identity(expected.pid)?;
    if actual != *expected {
        return Err(format!(
            "process {} identity changed; action blocked",
            expected.pid
        ));
    }
    Ok(())
}

#[cfg(test)]
fn send_signal(pid: u32, signal: Signal) -> Result<(), String> {
    if pid == 0 || pid > i32::MAX as u32 {
        return Err(format!("invalid pid {pid}"));
    }

    #[cfg(target_os = "linux")]
    {
        let Some(fd) = open_pidfd(pid)? else {
            return Err(
                "pidfd signaling is unavailable; refusing an unbound PID signal".to_owned(),
            );
        };
        let result = send_signal_on_pidfd(fd, pid, signal);
        libc_close(fd);
        result
    }

    #[cfg(not(target_os = "linux"))]
    {
        Err("pidfd signaling is unavailable; refusing an unbound PID signal".to_owned())
    }
}

fn send_signal_for(expected: &ProcessIdentity, signal: Signal) -> Result<(), String> {
    #[cfg(target_os = "linux")]
    {
        send_signal_pidfd_for(expected, signal)
    }

    #[cfg(not(target_os = "linux"))]
    {
        let _ = (expected, signal);
        Err("identity-bound signaling is unavailable on this platform".to_owned())
    }
}

#[cfg(target_os = "linux")]
fn send_signal_pidfd_for(expected: &ProcessIdentity, signal: Signal) -> Result<(), String> {
    // Open first, then validate, and signal through this same descriptor. The
    // descriptor pins the task identity even if the numeric PID is reused.
    let Some(fd) = open_pidfd(expected.pid)? else {
        return Err("pidfd signaling is unavailable; refusing an unbound PID signal".to_owned());
    };
    let result =
        validate_identity(expected).and_then(|()| send_signal_on_pidfd(fd, expected.pid, signal));
    libc_close(fd);
    result
}

#[cfg(target_os = "linux")]
fn open_pidfd(pid: u32) -> Result<Option<std::os::raw::c_int>, String> {
    use std::os::raw::{c_int, c_long};
    #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
    const SYS_PIDFD_OPEN: c_long = 434;
    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    return Ok(None);
    unsafe extern "C" {
        fn syscall(number: c_long, ...) -> c_long;
    }
    let fd = unsafe { syscall(SYS_PIDFD_OPEN, pid as c_int, 0) };
    if fd >= 0 {
        return Ok(Some(fd as c_int));
    }
    let error = std::io::Error::last_os_error();
    if error.raw_os_error().is_some_and(is_unsupported_syscall) {
        Ok(None)
    } else if error.raw_os_error() == Some(nix::libc::ESRCH) {
        Err(format!("process {pid} no longer exists"))
    } else {
        Err(format!("failed to open pidfd for pid {pid}: {error}"))
    }
}

#[cfg(target_os = "linux")]
fn send_signal_on_pidfd(fd: std::os::raw::c_int, pid: u32, signal: Signal) -> Result<(), String> {
    use std::os::raw::c_long;
    #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
    const SYS_PIDFD_SEND_SIGNAL: c_long = 424;
    unsafe extern "C" {
        fn syscall(number: c_long, ...) -> c_long;
    }
    let result = unsafe {
        syscall(
            SYS_PIDFD_SEND_SIGNAL,
            fd,
            signal as i32,
            std::ptr::null::<std::ffi::c_void>(),
            0,
        )
    };
    if result == 0 {
        return Ok(());
    }
    let error = std::io::Error::last_os_error();
    if error.raw_os_error() == Some(nix::libc::ESRCH) {
        Err(format!("process {pid} no longer exists"))
    } else {
        Err(format!("failed to send {signal} to pid {pid}: {error}"))
    }
}

#[cfg(target_os = "linux")]
fn is_unsupported_syscall(errno: i32) -> bool {
    errno == nix::libc::ENOSYS
}

#[cfg(target_os = "linux")]
unsafe extern "C" {
    fn close(fd: std::os::raw::c_int) -> std::os::raw::c_int;
}

#[cfg(target_os = "linux")]
fn libc_close(fd: std::os::raw::c_int) {
    // SAFETY: callers pass a descriptor owned by this module.
    let _ = unsafe { close(fd) };
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    fn delta(pid: u32, name: &str, rss: u64, growth: i64) -> ProcessDelta {
        delta_at(pid, name, rss, growth, 1)
    }

    fn delta_at(pid: u32, name: &str, rss: u64, growth: i64, start_time: u64) -> ProcessDelta {
        ProcessDelta {
            pid,
            name: name.to_owned(),
            start_time,
            rss_bytes: rss,
            delta_bytes_per_sec: growth,
        }
    }

    #[test]
    fn green_does_nothing() {
        let actions = mitigate(
            SystemState::Green,
            &[delta(1, "cargo", 10, 10)],
            &Config::default(),
        );
        assert!(actions.is_empty());
    }

    #[test]
    fn yellow_renices_and_ionices_top_offenders_only() {
        let deltas = [
            delta(1, "cargo", 500, 5),
            delta(2, "Xorg", 900, 900),
            delta(3, "rustc", 400, 900),
            delta(4, "node", 300, 50),
            delta(5, "python", 50, 500),
        ];

        let actions = mitigate(SystemState::Yellow, &deltas, &Config::default());

        assert_eq!(
            actions,
            vec![
                GuardAction::Renice { pid: 1 },
                GuardAction::Ionice { pid: 1 },
                GuardAction::Renice { pid: 3 },
                GuardAction::Ionice { pid: 3 },
                GuardAction::Renice { pid: 4 },
                GuardAction::Ionice { pid: 4 },
            ]
        );
    }

    #[test]
    fn yellow_does_not_repeat_actions_until_recovery() {
        let deltas = [delta(101, "worker", 500, 5)];
        let config = Config::default();

        assert_eq!(mitigate(SystemState::Yellow, &deltas, &config).len(), 2);
        // Emitting actions does not claim success: both actions are retried
        // until the executor records successful completion.
        assert_eq!(mitigate(SystemState::Yellow, &deltas, &config).len(), 2);
        let identity = ProcessIdentity {
            pid: 101,
            name: "worker".to_owned(),
            start_time: 1,
        };
        record_yellow_success(&GuardAction::Renice { pid: 101 }, &identity);
        assert_eq!(
            mitigate(SystemState::Yellow, &deltas, &config),
            vec![GuardAction::Ionice { pid: 101 }]
        );
        record_yellow_success(&GuardAction::Ionice { pid: 101 }, &identity);
        assert!(mitigate(SystemState::Yellow, &deltas, &config).is_empty());

        // Recovery resets the application state, so a later Yellow episode
        // applies the mitigation again.
        assert!(mitigate(SystemState::Green, &[], &config).is_empty());
        assert_eq!(mitigate(SystemState::Yellow, &deltas, &config).len(), 2);
    }

    #[test]
    fn vanished_process_does_not_poison_pid_reuse() {
        let config = Config::default();
        let original = [delta_at(102, "worker", 500, 5, 10)];
        let replacement = [delta_at(102, "worker", 500, 5, 11)];

        assert_eq!(mitigate(SystemState::Yellow, &original, &config).len(), 2);
        assert!(mitigate(SystemState::Yellow, &[], &config).is_empty());
        assert_eq!(
            mitigate(SystemState::Yellow, &replacement, &config).len(),
            2
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn pidfd_fallback_is_only_for_unsupported_syscalls() {
        assert!(is_unsupported_syscall(nix::libc::ENOSYS));
        assert!(!is_unsupported_syscall(nix::libc::EINVAL));
        assert!(!is_unsupported_syscall(nix::libc::EPERM));
    }

    #[test]
    fn red_freezes_highest_growth_and_skips_immune() {
        let deltas = [
            delta(1, "cargo", 100, 50),
            delta(2, "Xorg", 900, 900),
            delta(3, "chromium", 300, 400),
        ];

        let mut config = Config::default();
        config.guard.ram_spike_mb_per_sec = 0.0;
        let actions = mitigate(SystemState::Red, &deltas, &config);

        assert_eq!(actions, vec![GuardAction::Freeze { pid: 3 }]);
    }

    #[test]
    fn red_ignores_shrinking_processes() {
        let actions = mitigate(
            SystemState::Red,
            &[delta(1, "cargo", 100, 0), delta(2, "node", 100, -5)],
            &Config::default(),
        );
        assert!(actions.is_empty());
    }

    #[test]
    fn red_requires_configured_memory_spike() {
        let mut config = Config::default();
        config.guard.ram_spike_mb_per_sec = 10.0;
        let deltas = [
            delta(1, "memory-worker", 100, 11 * 1024 * 1024),
            delta(2, "node", 100, 9 * 1024 * 1024),
        ];

        assert_eq!(
            mitigate(SystemState::Red, &deltas, &config),
            vec![GuardAction::Freeze { pid: 1 }]
        );
    }

    #[test]
    fn vanished_signal_is_an_error() {
        let result = send_signal(2_000_000_000, Signal::SIGSTOP);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("no longer exists"));
    }

    #[test]
    fn rejects_invalid_pid() {
        assert!(execute(&GuardAction::Freeze { pid: 0 }, &Config::default()).is_err());
    }

    #[test]
    fn identity_mismatch_is_rejected_before_signal() {
        let mut child = std::process::Command::new("sleep")
            .arg("5")
            .spawn()
            .expect("spawn sleep child");
        std::thread::sleep(std::time::Duration::from_millis(50));
        let identity = current_identity(child.id()).expect("child identity");
        let wrong = ProcessIdentity {
            start_time: identity.start_time.saturating_add(1),
            ..identity
        };

        let result = execute_for(
            &GuardAction::Freeze { pid: child.id() },
            &wrong,
            &Config::default(),
        );
        let _ = child.kill();
        let _ = child.wait();

        assert!(result.is_err());
        assert!(result.unwrap_err().contains("identity changed"));
    }

    #[test]
    fn priority_actions_fail_closed_without_identity_bound_operation() {
        let mut child = std::process::Command::new("sleep")
            .arg("5")
            .spawn()
            .expect("spawn sleep child");
        let identity = current_identity(child.id()).expect("child identity");
        let config = Config::default();
        let deltas = [delta_at(
            identity.pid,
            &identity.name,
            500,
            5,
            identity.start_time,
        )];
        assert!(mitigate(SystemState::Green, &[], &config).is_empty());

        for action in [
            GuardAction::Renice { pid: child.id() },
            GuardAction::Ionice { pid: child.id() },
        ] {
            let result = execute_for(&action, &identity, &config);
            assert_eq!(
                result.unwrap_err(),
                "identity-bound priority execution is unavailable; refusing unbound PID priority change"
            );
        }
        assert_eq!(mitigate(SystemState::Yellow, &deltas, &config).len(), 2);

        let _ = child.kill();
        let _ = child.wait();
    }

    #[cfg(not(target_os = "linux"))]
    #[test]
    fn non_linux_signaling_fails_closed_without_pid_fallback() {
        let expected = ProcessIdentity {
            pid: std::process::id(),
            name: "self".to_owned(),
            start_time: 0,
        };

        let result = send_signal_for(&expected, Signal::SIGSTOP);

        assert_eq!(
            result.unwrap_err(),
            "identity-bound signaling is unavailable on this platform"
        );
    }

    #[test]
    fn live_child_can_be_signalled_through_identity_bound_path() {
        let mut child = std::process::Command::new("sleep")
            .arg("5")
            .spawn()
            .expect("spawn sleep child");
        assert!(send_signal(child.id(), Signal::SIGSTOP).is_ok());
        assert!(send_signal(child.id(), Signal::SIGCONT).is_ok());
        let _ = child.kill();
        let _ = child.wait();
    }

    #[test]
    fn public_termination_paths_reject_immune_processes() {
        let mut child = std::process::Command::new("sleep")
            .arg("5")
            .spawn()
            .expect("spawn sleep child");
        let identity = current_identity(child.id()).expect("child identity");
        let mut config = Config::default();
        config.guard.immune_process_names = vec![identity.name.clone()];

        for action in [
            GuardAction::Kill { pid: child.id() },
            GuardAction::Terminate { pid: child.id() },
        ] {
            let result = execute_for(&action, &identity, &config);
            assert_eq!(
                result.unwrap_err(),
                format!("refusing to terminate immune process {}", identity.name)
            );
        }

        let result = execute(&GuardAction::Kill { pid: child.id() }, &config);
        assert_eq!(
            result.unwrap_err(),
            format!("refusing to terminate immune process {}", identity.name)
        );

        let immutable_identity = ProcessIdentity {
            name: "sac".to_owned(),
            ..identity.clone()
        };
        let result = execute_for(
            &GuardAction::Terminate { pid: child.id() },
            &immutable_identity,
            &Config::default(),
        );
        assert_eq!(
            result.unwrap_err(),
            "refusing to terminate immune process sac"
        );

        let _ = child.kill();
        let _ = child.wait();
    }

    #[test]
    fn failed_first_yellow_action_does_not_mark_identity_mitigated() {
        let config = Config::default();
        let deltas = [delta(103, "worker", 500, 5)];
        let identity = ProcessIdentity {
            pid: 103,
            name: "worker".to_owned(),
            start_time: 1,
        };

        assert_eq!(mitigate(SystemState::Yellow, &deltas, &config).len(), 2);
        record_yellow_success(&GuardAction::Renice { pid: 103 }, &identity);
        assert_eq!(
            mitigate(SystemState::Yellow, &deltas, &config),
            vec![GuardAction::Ionice { pid: 103 }]
        );
    }
}
