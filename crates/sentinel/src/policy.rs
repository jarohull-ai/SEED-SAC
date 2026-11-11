//! Maps a [`SystemSnapshot`] onto a [`SystemState`] using configured
//! thresholds.

use crate::config::Config;
use crate::{SystemSnapshot, SystemState};

/// RAM and VRAM drive yellow and red; CPU can only drive yellow. Red wins.
pub fn evaluate(snapshot: &SystemSnapshot, config: &Config) -> SystemState {
    evaluate_with_previous(snapshot, config, SystemState::Green)
}

/// Evaluate resource pressure with hysteresis so values hovering around a
/// threshold do not make the operator-visible state oscillate every sample.
/// The previous state is only used for releasing an already active level;
/// entering YELLOW/RED still uses the configured thresholds exactly.
pub fn evaluate_with_previous(
    snapshot: &SystemSnapshot,
    config: &Config,
    previous: SystemState,
) -> SystemState {
    let thresholds = &config.thresholds;
    const RAM_RELEASE_MARGIN: f32 = 2.0;
    const VRAM_RELEASE_MARGIN: f32 = 2.0;
    const CPU_RELEASE_MARGIN: f32 = 5.0;

    let ram_pct = percent(snapshot.ram_used_bytes, snapshot.ram_total_bytes);
    let vram_pct = match (snapshot.vram_used_bytes, snapshot.vram_total_bytes) {
        (Some(used), Some(total)) => Some(percent(used, total)),
        _ => None,
    };

    let red_entered = ram_pct >= thresholds.ram.red_pct
        || vram_pct.is_some_and(|pct| pct >= thresholds.vram.red_pct);
    let red_held = previous == SystemState::Red
        && (ram_pct >= thresholds.ram.red_pct - RAM_RELEASE_MARGIN
            || vram_pct.is_some_and(|pct| pct >= thresholds.vram.red_pct - VRAM_RELEASE_MARGIN));
    if red_entered || red_held {
        return SystemState::Red;
    }

    let yellow_entered = ram_pct >= thresholds.ram.yellow_pct
        || vram_pct.is_some_and(|pct| pct >= thresholds.vram.yellow_pct)
        || snapshot.cpu_usage_pct >= thresholds.cpu.yellow_pct;
    let yellow_held = previous == SystemState::Yellow
        && (ram_pct >= thresholds.ram.yellow_pct - RAM_RELEASE_MARGIN
            || vram_pct.is_some_and(|pct| pct >= thresholds.vram.yellow_pct - VRAM_RELEASE_MARGIN)
            || snapshot.cpu_usage_pct >= thresholds.cpu.yellow_pct - CPU_RELEASE_MARGIN);
    if yellow_entered || yellow_held {
        return SystemState::Yellow;
    }

    SystemState::Green
}

/// Whether a process is protected from mitigation.
pub fn is_immune(name: &str, config: &Config) -> bool {
    let name = name.trim();
    crate::config::immutable_process_names()
        .iter()
        .copied()
        .chain(config.guard.immune_process_names.iter().map(String::as_str))
        .any(|immune| immune.eq_ignore_ascii_case(name))
}

fn percent(used: u64, total: u64) -> f32 {
    if total == 0 {
        return 0.0;
    }
    (used as f64 / total as f64 * 100.0) as f32
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ProcessInfo, SystemSnapshot};

    const TOTAL_BYTES: u64 = 10_000;

    fn snapshot(cpu: f32, ram_pct: f32, vram_pct: Option<f32>) -> SystemSnapshot {
        SystemSnapshot {
            cpu_usage_pct: cpu,
            ram_used_bytes: bytes_for(ram_pct),
            ram_total_bytes: TOTAL_BYTES,
            vram_used_bytes: vram_pct.map(bytes_for),
            vram_total_bytes: vram_pct.map(|_| TOTAL_BYTES),
            processes: Vec::<ProcessInfo>::new(),
        }
    }

    fn bytes_for(pct: f32) -> u64 {
        (TOTAL_BYTES as f32 * pct / 100.0) as u64
    }

    #[test]
    fn green_when_everything_is_low() {
        assert_eq!(
            evaluate(&snapshot(10.0, 50.0, Some(50.0)), &Config::default()),
            SystemState::Green
        );
    }

    #[test]
    fn cpu_only_reaches_yellow() {
        assert_eq!(
            evaluate(&snapshot(99.0, 20.0, None), &Config::default()),
            SystemState::Yellow
        );
    }

    #[test]
    fn ram_and_vram_reach_red() {
        assert_eq!(
            evaluate(&snapshot(0.0, 90.0, None), &Config::default()),
            SystemState::Red
        );
        assert_eq!(
            evaluate(&snapshot(0.0, 10.0, Some(95.0)), &Config::default()),
            SystemState::Red
        );
    }

    #[test]
    fn yellow_bands() {
        assert_eq!(
            evaluate(&snapshot(0.0, 80.0, None), &Config::default()),
            SystemState::Yellow
        );
        assert_eq!(
            evaluate(&snapshot(0.0, 10.0, Some(85.0)), &Config::default()),
            SystemState::Yellow
        );
    }

    #[test]
    fn threshold_hysteresis_prevents_yellow_red_oscillation() {
        let config = Config::default();
        assert_eq!(
            evaluate_with_previous(&snapshot(0.0, 88.5, None), &config, SystemState::Yellow),
            SystemState::Red
        );
        assert_eq!(
            evaluate_with_previous(&snapshot(0.0, 87.5, None), &config, SystemState::Red),
            SystemState::Red
        );
        assert_eq!(
            evaluate_with_previous(&snapshot(0.0, 85.5, None), &config, SystemState::Red),
            SystemState::Yellow
        );
    }

    #[test]
    fn threshold_hysteresis_keeps_yellow_until_pressure_clears() {
        let config = Config::default();
        assert_eq!(
            evaluate_with_previous(&snapshot(0.0, 74.0, None), &config, SystemState::Yellow),
            SystemState::Yellow
        );
        assert_eq!(
            evaluate_with_previous(&snapshot(0.0, 72.5, None), &config, SystemState::Yellow),
            SystemState::Green
        );
    }

    #[test]
    fn immune_names_are_case_insensitive() {
        let config = Config::default();
        assert!(is_immune("Xorg", &config));
        assert!(is_immune("xorg", &config));
        assert!(is_immune(" systemd ", &config));
        assert!(!is_immune("cargo-test", &config));
    }

    #[test]
    fn immutable_system_names_survive_custom_config() {
        let mut config = Config::default();
        config.guard.immune_process_names.clear();

        assert!(is_immune("systemd", &config));
        assert!(is_immune("Xorg", &config));
        assert!(is_immune("sac", &config));
    }
}
