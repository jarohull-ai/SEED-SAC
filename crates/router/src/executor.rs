//! Execution of parsed [`Action`]s against the live system.
//!
//! Process actions resolve a process name to a PID with `sysinfo` and are then
//! delegated to [`sentinel::guard`], which owns signal delivery. Cache cleanup
//! is a direct `/proc` write and therefore needs root.

use std::fs::OpenOptions;
use std::io::Write;
use std::process::Command;

use sentinel::ProcessIdentity;
use sentinel::config::{self, Config};
use sentinel::guard::{self, GuardAction};
use sentinel::policy;
use sysinfo::{ProcessesToUpdate, System};

use crate::Action;

/// Result of executing an action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExecResult {
    /// Success message.
    Done(String),
    /// Forward to the LLM.
    NeedsL1(String),
    /// Failure message.
    Error(String),
}

/// Execute a parsed action.
///
/// Process actions are checked against the immune list and the app's own PID
/// before any signal is sent.
pub fn execute(action: &Action) -> ExecResult {
    let config = config::load();
    execute_with_config(action, &config)
}

/// Execute an action with an explicit configuration.
///
/// This is useful to callers that already loaded configuration and to tests;
/// the public convenience entry point [`execute`] loads the normal config.
pub fn execute_with_config(action: &Action, config: &Config) -> ExecResult {
    if matches!(action, Action::Kill(_) | Action::CleanCache) {
        return ExecResult::Error(
            "Akcja destrukcyjna wymaga jawnego potwierdzenia (execute_confirmed_with_config)"
                .to_owned(),
        );
    }
    execute_confirmed_with_config(action, config)
}

/// Execute an action after the caller has obtained an explicit confirmation.
///
/// This entry point is intentionally separate from [`execute_with_config`]:
/// callers cannot accidentally turn a normal router dispatch into a
/// destructive operation merely by passing a parsed action through it.
pub fn execute_confirmed_with_config(action: &Action, config: &Config) -> ExecResult {
    match action {
        Action::Kill(name) => signal(name, "Zamknięto", |pid| GuardAction::Kill { pid }, config),
        Action::Freeze(name) => {
            signal(name, "Zamrożono", |pid| GuardAction::Freeze { pid }, config)
        }
        Action::Resume(name) => signal(name, "Wznowiono", |pid| GuardAction::Thaw { pid }, config),
        Action::CleanCache => clean_cache(),
        Action::QueryL1(text) => ExecResult::NeedsL1(text.clone()),
    }
}

/// Resolve `name` to a PID and apply the guard action chosen by `make_action`.
fn signal(
    name: &str,
    verb: &str,
    make_action: impl Fn(u32) -> GuardAction,
    config: &Config,
) -> ExecResult {
    if policy::is_immune(name, config) {
        return ExecResult::Error(format!(
            "{name} jest na liście procesów chronionych — akcja zablokowana"
        ));
    }

    let target = match find_pid(name) {
        Ok(target) => target,
        Err(ProcessLookupError::NotFound) => {
            return ExecResult::Error(format!("Nie znaleziono procesu o nazwie {name}"));
        }
        Err(ProcessLookupError::Ambiguous { matches }) => {
            return ExecResult::Error(format!(
                "Nie można jednoznacznie wybrać procesu o nazwie {name} — znaleziono {matches} procesy"
            ));
        }
    };
    let pid = target.pid;
    let resolved = target.name.clone();

    if pid == std::process::id() {
        return ExecResult::Error(
            "Nie można wykonać tej akcji na własnym procesie (SAC)".to_owned(),
        );
    }

    if resolved != name && policy::is_immune(&resolved, config) {
        return ExecResult::Error(format!(
            "{resolved} jest na liście procesów chronionych — akcja zablokowana"
        ));
    }

    if !target_is_current(&target) {
        return ExecResult::Error(format!(
            "Proces {resolved} (PID {pid}) zniknął lub zmienił tożsamość — akcja zablokowana"
        ));
    }

    let identity = ProcessIdentity {
        pid: target.pid,
        name: target.name.clone(),
        start_time: target.start_time,
    };
    match guard::execute_for(&make_action(pid), &identity, config) {
        Ok(()) => {
            tracing::debug!(pid, process = %resolved, action = verb, "router executed process action");
            ExecResult::Done(format!("{verb} {resolved} (PID {pid})"))
        }
        Err(err) => ExecResult::Error(format!(
            "Nie udało się wykonać akcji na {resolved} (PID {pid}): {err}"
        )),
    }
}

/// Find a process by (case-insensitive) exact name.
///
/// Destructive actions must never infer a target from a substring: doing so
/// could select an unrelated process (or bypass an immune-name check). If
/// several processes have the same executable name, reject the lookup rather
/// than guessing which process the user intended.
///
/// Returns the PID together with the process name as reported by the kernel.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ProcessTarget {
    pid: u32,
    name: String,
    start_time: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProcessLookupError {
    NotFound,
    Ambiguous { matches: usize },
}

fn find_pid(name: &str) -> Result<ProcessTarget, ProcessLookupError> {
    let mut system = System::new();
    system.refresh_processes(ProcessesToUpdate::All, true);

    let needle = name.trim().to_lowercase();
    if needle.is_empty() {
        return Err(ProcessLookupError::NotFound);
    }

    let mut exact = Vec::new();

    for (pid, process) in system.processes() {
        let process_name = process.name().to_string_lossy().into_owned();
        let lowered = process_name.to_lowercase();
        if lowered == needle {
            exact.push(ProcessTarget {
                pid: pid.as_u32(),
                name: process_name,
                start_time: process.start_time(),
            });
        }
    }

    match exact.len() {
        0 => Err(ProcessLookupError::NotFound),
        1 => Ok(exact.pop().expect("length checked above")),
        matches => Err(ProcessLookupError::Ambiguous { matches }),
    }
}

/// Re-check the PID's kernel identity immediately before sending a signal.
/// A PID can be recycled after name lookup; any mismatch fails closed.
fn target_is_current(target: &ProcessTarget) -> bool {
    let mut system = System::new();
    system.refresh_processes(ProcessesToUpdate::All, true);
    let Some(process) = system.process(sysinfo::Pid::from_u32(target.pid)) else {
        return false;
    };
    process.name().to_string_lossy() == target.name && process.start_time() == target.start_time
}

/// Synchronise file systems and drop the page cache.
fn clean_cache() -> ExecResult {
    // Check authorization before syncing: a denied request must not perform
    // even the preparatory system-wide sync as a side effect.
    let mut file = match OpenOptions::new()
        .write(true)
        .open("/proc/sys/vm/drop_caches")
    {
        Ok(file) => file,
        Err(err) => return cache_error(&err),
    };

    let status = match Command::new("/usr/bin/sync").status() {
        Ok(status) => status,
        Err(err) => {
            return ExecResult::Error(format!("Nie udało się zsynchronizować dysku: {err}"));
        }
    };
    if !status.success() {
        return ExecResult::Error(format!(
            "Synchronizacja dysku zakończyła się niepowodzeniem: {status}"
        ));
    }

    match file.write_all(b"3") {
        Ok(()) => ExecResult::Done("Wyczyszczono cache pamięci (drop_caches=3)".to_owned()),
        Err(err) => cache_error(&err),
    }
}

fn cache_error(err: &std::io::Error) -> ExecResult {
    if err.kind() == std::io::ErrorKind::PermissionDenied {
        ExecResult::Error(
            "Brak uprawnień do czyszczenia cache. Uruchom: sudo sac lub sudo sysctl vm.drop_caches=3"
                .to_owned(),
        )
    } else {
        ExecResult::Error(format!("Nie udało się wyczyścić cache: {err}"))
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[test]
    fn query_l1_is_forwarded_untouched() {
        let text = "dlaczego kompilacja się wywala?".to_owned();
        assert_eq!(
            execute(&Action::QueryL1(text.clone())),
            ExecResult::NeedsL1(text)
        );
    }

    #[test]
    fn missing_process_returns_error() {
        let name = "seed-sac-missing-process-xyz";

        for action in [
            Action::Freeze(name.to_owned()),
            Action::Resume(name.to_owned()),
        ] {
            match execute(&action) {
                ExecResult::Error(message) => assert!(message.contains(name)),
                other => panic!("expected Error for {action:?}, got {other:?}"),
            }
        }
    }

    #[test]
    fn clean_cache_reports_a_result() {
        match execute(&Action::CleanCache) {
            ExecResult::Error(message) => assert!(message.contains("jawnego potwierdzenia")),
            other => panic!("unexpected CleanCache result: {other:?}"),
        }
    }

    #[test]
    fn finds_a_running_child_by_name() {
        let mut child = Command::new("sleep")
            .arg("5")
            .spawn()
            .expect("spawn sleep child");
        std::thread::sleep(Duration::from_millis(100));

        let found = find_pid("sleep");
        let _ = child.kill();
        let _ = child.wait();

        match found {
            Ok(target) => {
                assert!(target.pid > 0);
                assert_eq!(target.name.to_lowercase(), "sleep");
                assert!(target.start_time > 0);
            }
            Err(ProcessLookupError::Ambiguous { matches }) => assert!(matches >= 2),
            Err(error) => panic!("sleep process should be visible: {error:?}"),
        }
    }

    #[test]
    fn name_lookup_is_case_insensitive() {
        let mut child = Command::new("sleep")
            .arg("5")
            .spawn()
            .expect("spawn sleep child");
        std::thread::sleep(Duration::from_millis(100));

        let found = find_pid("SlEeP");
        let _ = child.kill();
        let _ = child.wait();

        assert!(matches!(
            found,
            Ok(_) | Err(ProcessLookupError::Ambiguous { matches: 2.. })
        ));
    }

    #[test]
    fn name_lookup_never_uses_a_partial_match() {
        let mut child = Command::new("sleep")
            .arg("5")
            .spawn()
            .expect("spawn sleep child");
        std::thread::sleep(Duration::from_millis(100));

        assert_eq!(find_pid("lee"), Err(ProcessLookupError::NotFound));

        let _ = child.kill();
        let _ = child.wait();
    }

    #[test]
    fn current_router_process_is_rejected_before_signal_delivery() {
        let mut system = System::new();
        system.refresh_processes(ProcessesToUpdate::All, true);
        let current_pid = sysinfo::Pid::from_u32(std::process::id());
        let current_name = system
            .process(current_pid)
            .expect("current process should be visible")
            .name()
            .to_string_lossy()
            .into_owned();

        match execute_confirmed_with_config(&Action::Kill(current_name), &Config::default()) {
            ExecResult::Error(message) => assert!(message.contains("własnym procesie")),
            other => panic!("current process must be protected, got {other:?}"),
        }
    }

    #[test]
    fn unconfirmed_destructive_actions_are_rejected_before_side_effects() {
        let config = Config::default();

        assert!(matches!(
            execute_with_config(&Action::Kill("sleep".to_owned()), &config),
            ExecResult::Error(message) if message.contains("jawnego potwierdzenia")
        ));
        assert!(matches!(
            execute_with_config(&Action::CleanCache, &config),
            ExecResult::Error(message) if message.contains("jawnego potwierdzenia")
        ));
    }

    #[test]
    fn confirmed_kill_uses_the_destructive_entry_point() {
        let result = execute_confirmed_with_config(
            &Action::Kill("seed-sac-missing-process-xyz".to_owned()),
            &Config::default(),
        );

        assert!(matches!(
            result,
            ExecResult::Error(message) if message.contains("Nie znaleziono procesu")
        ));
    }

    #[test]
    fn immune_name_matching_does_not_block_similar_names() {
        let mut config = Config::default();
        config.guard.immune_process_names = vec!["sleep".to_owned()];

        assert!(matches!(
            execute_confirmed_with_config(&Action::Kill("sleep-worker".to_owned()), &config),
            ExecResult::Error(message) if message.contains("Nie znaleziono procesu")
        ));
    }

    #[test]
    fn duplicate_process_names_are_rejected_instead_of_guessed() {
        let mut first = Command::new("sleep")
            .arg("5")
            .spawn()
            .expect("spawn first sleep child");
        let mut second = Command::new("sleep")
            .arg("5")
            .spawn()
            .expect("spawn second sleep child");
        std::thread::sleep(Duration::from_millis(100));

        assert!(matches!(
            find_pid("sleep"),
            Err(ProcessLookupError::Ambiguous { matches }) if matches >= 2
        ));

        let _ = first.kill();
        let _ = second.kill();
        let _ = first.wait();
        let _ = second.wait();
    }
}
