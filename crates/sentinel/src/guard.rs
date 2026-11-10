//! Mitigation actions for resource pressure.
//!
//! Yellow pressure lowers scheduling priority of the biggest offenders; red
//! pressure freezes the process that is currently allocating fastest instead of
//! killing it, so no unsaved work is lost.

use std::process::Command;

use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;

use crate::config::Config;
use crate::policy::is_immune;
use crate::process::ProcessDelta;
use crate::{ProcessIdentity, SystemState};

/// Number of processes that get deprioritised in the yellow state.
const YELLOW_OFFENDER_LIMIT: usize = 3;
const RENICE_PATH: &str = "/usr/bin/renice";
const IONICE_PATH: &str = "/usr/bin/ionice";

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
        SystemState::Green => Vec::new(),

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

            let mut actions = Vec::with_capacity(YELLOW_OFFENDER_LIMIT * 2);
            for offender in offenders.into_iter().take(YELLOW_OFFENDER_LIMIT) {
                actions.push(GuardAction::Renice { pid: offender.pid });
                actions.push(GuardAction::Ionice { pid: offender.pid });
            }
            actions
        }

        SystemState::Red => {
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
    }
}

/// Run a mitigation action. Errors are reported as human-readable strings so
/// the caller can surface them in the HUD.
pub fn execute(action: &GuardAction) -> Result<(), String> {
    let identity = current_identity(action_pid(action))?;
    execute_for(action, &identity)
}

/// Execute only when the PID still has the expected kernel identity.
pub fn execute_for(action: &GuardAction, expected: &ProcessIdentity) -> Result<(), String> {
    if action_pid(action) != expected.pid {
        return Err("action PID does not match validated process identity".to_owned());
    }
    validate_identity(expected)?;
    match *action {
        GuardAction::Renice { pid } => run_tool(RENICE_PATH, &["-n", "10", "-p", &pid.to_string()]),
        GuardAction::Ionice { pid } => run_tool(IONICE_PATH, &["-c", "3", "-p", &pid.to_string()]),
        GuardAction::Freeze { pid } => send_signal(pid, Signal::SIGSTOP),
        GuardAction::Thaw { pid } => send_signal(pid, Signal::SIGCONT),
        GuardAction::Kill { pid } => {
            send_signal(pid, Signal::SIGCONT)?;
            validate_identity(expected)?;
            send_signal(pid, Signal::SIGTERM)
        }
        // Callers that need to terminate a stopped process must issue Thaw
        // explicitly; keeping Terminate to one signal avoids duplicate thaw
        // races in HUD actions.
        GuardAction::Terminate { pid } => send_signal(pid, Signal::SIGTERM),
    }
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

fn send_signal(pid: u32, signal: Signal) -> Result<(), String> {
    if pid == 0 || pid > i32::MAX as u32 {
        return Err(format!("invalid pid {pid}"));
    }

    match kill(Pid::from_raw(pid as i32), signal) {
        Ok(()) => Ok(()),
        Err(nix::errno::Errno::ESRCH) => Err(format!("process {pid} no longer exists")),
        Err(err) => Err(format!("failed to send {signal} to pid {pid}: {err}")),
    }
}

fn run_tool(tool: &str, args: &[&str]) -> Result<(), String> {
    let output = Command::new(tool)
        .args(args)
        .output()
        .map_err(|err| format!("failed to run {tool}: {err}"))?;

    if output.status.success() {
        return Ok(());
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    let detail = stderr.trim();

    if detail.contains("No such process") {
        return Err(format!("{tool} could not find the target process"));
    }

    if detail.is_empty() {
        Err(format!("{tool} exited with {}", output.status))
    } else {
        Err(format!("{tool} failed: {detail}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    fn delta(pid: u32, name: &str, rss: u64, growth: i64) -> ProcessDelta {
        ProcessDelta {
            pid,
            name: name.to_owned(),
            start_time: 1,
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
        assert!(execute(&GuardAction::Freeze { pid: 0 }).is_err());
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

        let result = execute_for(&GuardAction::Freeze { pid: child.id() }, &wrong);
        let _ = child.kill();
        let _ = child.wait();

        assert!(result.is_err());
        assert!(result.unwrap_err().contains("identity changed"));
    }
}
