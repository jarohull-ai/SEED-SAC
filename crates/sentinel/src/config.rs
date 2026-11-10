//! Configuration loading for the sentinel.
//!
//! The user config lives at `$XDG_CONFIG_HOME/seed-sac/config.toml` (or
//! `~/.config/seed-sac/config.toml` when `XDG_CONFIG_HOME` is unset). If it is
//! missing or unreadable the compiled defaults are used.

use std::path::PathBuf;

use serde::Deserialize;

const IMMUTABLE_PROCESSES: &[&str] = &[
    "systemd",
    "pipewire",
    "dbus-daemon",
    "Xorg",
    "Xwayland",
    "kwin",
    "mutter",
    "gnome-shell",
    "sway",
    "hyprland",
    "sac",
];

pub(crate) fn immutable_process_names() -> &'static [&'static str] {
    IMMUTABLE_PROCESSES
}

/// Top-level configuration tree.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Config {
    pub sentinel: SentinelConfig,
    pub thresholds: ThresholdsConfig,
    pub guard: GuardConfig,
    pub l1_provider: L1ProviderConfig,
}

/// Configuration for the local-first L1 model provider.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct L1ProviderConfig {
    pub enabled: bool,
    pub backend: String,
    pub base_url: String,
    pub model: String,
    pub timeout_ms: u64,
    pub max_tokens: u32,
    pub temperature: f32,
}

impl Default for L1ProviderConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            backend: "openai_compatible".to_owned(),
            base_url: "http://127.0.0.1:1234/v1".to_owned(),
            model: "Ling-3.0-tiny".to_owned(),
            timeout_ms: 5_000,
            max_tokens: 512,
            temperature: 0.2,
        }
    }
}

impl L1ProviderConfig {
    pub fn validated(mut self) -> Self {
        self.timeout_ms = self.timeout_ms.clamp(100, 60_000);
        self.max_tokens = self.max_tokens.clamp(1, 4096);
        self.temperature = if self.temperature.is_finite() {
            self.temperature.clamp(0.0, 2.0)
        } else {
            0.2
        };
        if self.backend.trim().is_empty() {
            self.backend = "openai_compatible".to_owned();
        }
        self
    }
}

/// Sampler tuning.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct SentinelConfig {
    pub poll_interval_ms: u64,
}

impl Default for SentinelConfig {
    fn default() -> Self {
        Self {
            poll_interval_ms: 250,
        }
    }
}

/// Thresholds that map a snapshot onto a [`crate::SystemState`].
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct ThresholdsConfig {
    pub ram: RamThresholds,
    pub cpu: CpuThresholds,
    pub vram: VramThresholds,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct RamThresholds {
    pub yellow_pct: f32,
    pub red_pct: f32,
}

impl Default for RamThresholds {
    fn default() -> Self {
        Self {
            yellow_pct: 75.0,
            red_pct: 88.0,
        }
    }
}

/// CPU pressure only has a yellow level; it never escalates to red on its own.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct CpuThresholds {
    pub yellow_pct: f32,
}

impl Default for CpuThresholds {
    fn default() -> Self {
        Self { yellow_pct: 70.0 }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct VramThresholds {
    pub yellow_pct: f32,
    pub red_pct: f32,
}

impl Default for VramThresholds {
    fn default() -> Self {
        Self {
            yellow_pct: 80.0,
            red_pct: 92.0,
        }
    }
}

/// Mitigation behaviour.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct GuardConfig {
    pub ram_spike_mb_per_sec: f32,
    #[serde(
        alias = "immune_processes",
        deserialize_with = "deserialize_immune_names"
    )]
    pub immune_process_names: Vec<String>,
}

impl Default for GuardConfig {
    fn default() -> Self {
        Self {
            ram_spike_mb_per_sec: 500.0,
            immune_process_names: IMMUTABLE_PROCESSES
                .iter()
                .map(|name| (*name).to_owned())
                .collect(),
        }
    }
}

/// Load the configuration, falling back to compiled defaults when the file is
/// absent or cannot be parsed.
pub fn load() -> Config {
    let Some(path) = config_path() else {
        tracing::debug!("no config directory found, using default sentinel config");
        return Config::default();
    };

    let contents = match std::fs::read_to_string(&path) {
        Ok(contents) => contents,
        Err(err) => {
            tracing::debug!(
                path = %path.display(),
                %err,
                "sentinel config not found, using defaults"
            );
            return Config::default();
        }
    };

    match toml::from_str::<Config>(&contents) {
        Ok(mut config) => {
            config.l1_provider = config.l1_provider.validated();
            config
        }
        Err(err) => {
            tracing::warn!(
                path = %path.display(),
                %err,
                "invalid sentinel config, using defaults"
            );
            Config::default()
        }
    }
}

fn config_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))?;

    Some(base.join("seed-sac").join("config.toml"))
}

/// Accept both `immune_process_names = ["a", "b"]` and the legacy
/// `[guard.immune_processes] names = ["a", "b"]` shape.
#[derive(Deserialize)]
#[serde(untagged)]
enum ImmuneNames {
    List(Vec<String>),
    Table { names: Vec<String> },
}

fn deserialize_immune_names<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(match ImmuneNames::deserialize(deserializer)? {
        ImmuneNames::List(names) => names,
        ImmuneNames::Table { names } => names,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
        [sentinel]
        poll_interval_ms = 500

        [thresholds.ram]
        yellow_pct = 70.0
        red_pct = 85.0

        [thresholds.cpu]
        yellow_pct = 65.0

        [thresholds.vram]
        yellow_pct = 78.0
        red_pct = 90.0

        [guard]
        ram_spike_mb_per_sec = 250.0

        [guard.immune_processes]
        names = ["systemd", "sac"]
    "#;

    #[test]
    fn parses_nested_config() {
        let config: Config = toml::from_str(SAMPLE).expect("sample config should parse");
        assert_eq!(config.sentinel.poll_interval_ms, 500);
        assert_eq!(config.thresholds.ram.red_pct, 85.0);
        assert_eq!(config.thresholds.cpu.yellow_pct, 65.0);
        assert_eq!(config.thresholds.vram.yellow_pct, 78.0);
        assert_eq!(config.guard.ram_spike_mb_per_sec, 250.0);
        assert_eq!(config.guard.immune_process_names, ["systemd", "sac"]);
    }

    #[test]
    fn missing_fields_use_defaults() {
        let config: Config = toml::from_str("[sentinel]\npoll_interval_ms = 1000\n").unwrap();
        assert_eq!(config.sentinel.poll_interval_ms, 1000);
        assert_eq!(config.thresholds.ram.yellow_pct, 75.0);
        assert_eq!(config.thresholds.ram.red_pct, 88.0);
        assert_eq!(config.guard.ram_spike_mb_per_sec, 500.0);
        assert!(
            config
                .guard
                .immune_process_names
                .contains(&"systemd".to_owned())
        );
        assert_eq!(config.l1_provider.model, "Ling-3.0-tiny");
        assert!(config.l1_provider.enabled);
    }

    #[test]
    fn l1_provider_defaults_match_contract() {
        let config = Config::default();
        assert!(config.l1_provider.enabled);
        assert_eq!(config.l1_provider.backend, "openai_compatible");
        assert_eq!(config.l1_provider.base_url, "http://127.0.0.1:1234/v1");
        assert_eq!(config.l1_provider.model, "Ling-3.0-tiny");
        assert_eq!(config.l1_provider.timeout_ms, 5_000);
        assert_eq!(config.l1_provider.max_tokens, 512);
        assert_eq!(config.l1_provider.temperature, 0.2);
    }

    #[test]
    fn l1_provider_can_be_overridden_without_other_sections() {
        let config: Config = toml::from_str(
            r#"
                [l1_provider]
                enabled = false
                model = "custom-model"
                timeout_ms = 9000
            "#,
        )
        .unwrap();

        assert!(!config.l1_provider.enabled);
        assert_eq!(config.l1_provider.model, "custom-model");
        assert_eq!(config.l1_provider.timeout_ms, 9000);
        assert_eq!(config.l1_provider.backend, "openai_compatible");
        assert_eq!(config.l1_provider.max_tokens, 512);
    }

    #[test]
    fn l1_provider_bounds_are_safe() {
        let config: Config =
            toml::from_str("[l1_provider]\ntimeout_ms=0\nmax_tokens=999999\ntemperature=-4.0\n")
                .unwrap();
        let provider = config.l1_provider.validated();
        assert_eq!(provider.timeout_ms, 100);
        assert_eq!(provider.max_tokens, 4096);
        assert_eq!(provider.temperature, 0.0);
    }

    #[test]
    fn accepts_flat_immune_list() {
        let config: Config = toml::from_str("[guard]\nimmune_process_names = [\"sac\"]\n").unwrap();
        assert_eq!(config.guard.immune_process_names, ["sac"]);
    }
}
