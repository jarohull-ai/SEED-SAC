//! VRAM telemetry.
//!
//! NVML support is compiled in with the `nvidia` feature. On Linux, AMD VRAM
//! telemetry is also read from the DRM sysfs interface, without requiring a
//! feature or an additional dependency.

use std::path::Path;

/// Returns `(used_bytes, total_bytes)` for the first available GPU telemetry
/// source.
#[cfg(feature = "nvidia")]
pub fn read_vram() -> Option<(u64, u64)> {
    read_nvidia_vram().or_else(read_sysfs_vram)
}

/// Returns `(used_bytes, total_bytes)` from Linux DRM sysfs, when available.
#[cfg(not(feature = "nvidia"))]
pub fn read_vram() -> Option<(u64, u64)> {
    read_sysfs_vram()
}

/// Returns telemetry for the first NVIDIA GPU when NVML is available.
#[cfg(feature = "nvidia")]
fn read_nvidia_vram() -> Option<(u64, u64)> {
    use std::sync::OnceLock;

    use nvml_wrapper::Nvml;

    static NVML: OnceLock<Option<Nvml>> = OnceLock::new();

    let nvml = NVML.get_or_init(|| match Nvml::init() {
        Ok(nvml) => Some(nvml),
        Err(err) => {
            tracing::debug!(%err, "NVML unavailable, trying sysfs VRAM telemetry");
            None
        }
    });
    let nvml = nvml.as_ref()?;

    let device = nvml.device_by_index(0).ok()?;
    let memory = device.memory_info().ok()?;

    Some((memory.used, memory.total))
}

#[cfg(target_os = "linux")]
fn read_sysfs_vram() -> Option<(u64, u64)> {
    read_sysfs_vram_from(Path::new("/sys/class/drm"))
}

#[cfg(not(target_os = "linux"))]
fn read_sysfs_vram() -> Option<(u64, u64)> {
    None
}

fn read_sysfs_vram_from(drm_path: &Path) -> Option<(u64, u64)> {
    let mut entries: Vec<_> = std::fs::read_dir(drm_path).ok()?.flatten().collect();
    entries.sort_by_key(|entry| entry.file_name());

    for entry in entries {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name.starts_with("card") || name[4..].parse::<u32>().is_err() {
            continue;
        }

        let device_path = entry.path().join("device");
        let card_total = read_sysfs_value(&device_path.join("mem_info_vram_total"));
        let card_used = read_sysfs_value(&device_path.join("mem_info_vram_used"));
        if let (Some(card_total), Some(card_used)) = (card_total, card_used)
            && card_total > 0
            && card_used <= card_total
        {
            return Some((card_used, card_total));
        }
    }

    None
}

fn read_sysfs_value(path: &Path) -> Option<u64> {
    let value = std::fs::read_to_string(path).ok()?;
    value.trim().parse().ok()
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_TEST_DIR: AtomicU64 = AtomicU64::new(0);

    fn test_dir() -> PathBuf {
        let id = NEXT_TEST_DIR.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("sentinel-gpu-{id}-{}", std::process::id()))
    }

    #[test]
    fn reads_first_valid_sysfs_vram_values() {
        let root = test_dir();
        let card0 = root.join("card0/device");
        let card1 = root.join("card1/device");
        fs::create_dir_all(&card0).unwrap();
        fs::create_dir_all(&card1).unwrap();
        fs::write(card0.join("mem_info_vram_total"), b"100\n").unwrap();
        fs::write(card0.join("mem_info_vram_used"), b"25\n").unwrap();
        fs::write(card1.join("mem_info_vram_total"), b"200\n").unwrap();
        fs::write(card1.join("mem_info_vram_used"), b"75\n").unwrap();
        fs::create_dir(root.join("renderD128")).unwrap();

        assert_eq!(super::read_sysfs_vram_from(&root), Some((25, 100)));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn skips_sysfs_card_when_used_exceeds_total() {
        let root = test_dir();
        let card0 = root.join("card0/device");
        let card1 = root.join("card1/device");
        fs::create_dir_all(&card0).unwrap();
        fs::create_dir_all(&card1).unwrap();
        fs::write(card0.join("mem_info_vram_total"), b"100\n").unwrap();
        fs::write(card0.join("mem_info_vram_used"), b"101\n").unwrap();
        fs::write(card1.join("mem_info_vram_total"), b"200\n").unwrap();
        fs::write(card1.join("mem_info_vram_used"), b"75\n").unwrap();

        assert_eq!(super::read_sysfs_vram_from(&root), Some((75, 200)));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn skips_sysfs_card_when_total_is_zero() {
        let root = test_dir();
        let card0 = root.join("card0/device");
        let card1 = root.join("card1/device");
        fs::create_dir_all(&card0).unwrap();
        fs::create_dir_all(&card1).unwrap();
        fs::write(card0.join("mem_info_vram_total"), b"0\n").unwrap();
        fs::write(card0.join("mem_info_vram_used"), b"0\n").unwrap();
        fs::write(card1.join("mem_info_vram_total"), b"200\n").unwrap();
        fs::write(card1.join("mem_info_vram_used"), b"75\n").unwrap();

        assert_eq!(super::read_sysfs_vram_from(&root), Some((75, 200)));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn ignores_missing_or_malformed_sysfs_values() {
        let root = test_dir();
        let card0 = root.join("card0/device");
        let card1 = root.join("card1/device");
        fs::create_dir_all(&card0).unwrap();
        fs::create_dir_all(&card1).unwrap();
        fs::write(card0.join("mem_info_vram_total"), b"not-a-number").unwrap();
        fs::write(card0.join("mem_info_vram_used"), b"1").unwrap();
        fs::write(card1.join("mem_info_vram_total"), b"512").unwrap();
        fs::write(card1.join("mem_info_vram_used"), b"128").unwrap();

        assert_eq!(super::read_sysfs_vram_from(&root), Some((128, 512)));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn read_vram_is_optional() {
        // Must never panic, whether or not a GPU is present.
        let _ = super::read_vram();
    }
}
