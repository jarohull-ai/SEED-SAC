//! VRAM telemetry.
//!
//! NVML support is compiled in with the `nvidia` feature. Without it (or
//! without a usable NVIDIA driver) VRAM readings are simply unavailable.

/// Returns `(used_bytes, total_bytes)` for the first NVIDIA GPU, if NVML is
/// available. The NVML handle is initialised once and reused.
#[cfg(feature = "nvidia")]
pub fn read_vram() -> Option<(u64, u64)> {
    use std::sync::OnceLock;

    use nvml_wrapper::Nvml;

    static NVML: OnceLock<Option<Nvml>> = OnceLock::new();

    let nvml = NVML.get_or_init(|| match Nvml::init() {
        Ok(nvml) => Some(nvml),
        Err(err) => {
            tracing::debug!(%err, "NVML unavailable, VRAM telemetry disabled");
            None
        }
    });
    let nvml = nvml.as_ref()?;

    let device = nvml.device_by_index(0).ok()?;
    let memory = device.memory_info().ok()?;

    Some((memory.used, memory.total))
}

/// Stub used when the crate is built without the `nvidia` feature.
#[cfg(not(feature = "nvidia"))]
pub fn read_vram() -> Option<(u64, u64)> {
    None
}

#[cfg(test)]
mod tests {
    #[test]
    fn read_vram_is_optional() {
        // Must never panic, whether or not a GPU is present.
        let _ = super::read_vram();
    }
}
