use egui::{Color32, RichText, Sense, vec2};

use crate::{DisplayState, SystemState};

const GREEN: Color32 = Color32::from_rgb(0x4C, 0xAF, 0x50);
const YELLOW: Color32 = Color32::from_rgb(0xFF, 0xC1, 0x07);
const RED: Color32 = Color32::from_rgb(0xF4, 0x43, 0x36);
const UNKNOWN: Color32 = Color32::from_gray(0x80);

const GIB: f64 = 1024.0 * 1024.0 * 1024.0;

fn state_color(state: SystemState) -> Color32 {
    match state {
        SystemState::Green => GREEN,
        SystemState::Yellow => YELLOW,
        SystemState::Red => RED,
    }
}

/// Color for a utilisation percentage (0–100).
fn pct_color(pct: f64, yellow_pct: f32, red_pct: Option<f32>) -> Color32 {
    if red_pct.is_some_and(|threshold| pct >= f64::from(threshold)) {
        RED
    } else if pct >= f64::from(yellow_pct) {
        YELLOW
    } else {
        GREEN
    }
}

fn pct(used: u64, total: u64) -> f64 {
    if total == 0 {
        0.0
    } else {
        used as f64 / total as f64 * 100.0
    }
}

fn mem_text(label: &str, used: u64, total: u64, yellow_pct: f32, red_pct: f32) -> RichText {
    let p = pct(used, total);
    RichText::new(format!(
        "{label}: {:.1}/{:.1} GB ({:.0}%)",
        used as f64 / GIB,
        total as f64 / GIB,
        p
    ))
    .color(pct_color(p, yellow_pct, Some(red_pct)))
}

fn cpu_color(state: &DisplayState) -> Option<Color32> {
    state.telemetry_available.then(|| {
        pct_color(
            f64::from(state.cpu_pct),
            state.thresholds.cpu_yellow_pct,
            None,
        )
    })
}

pub fn draw_status_bar(ui: &mut egui::Ui, state: &DisplayState) {
    ui.horizontal_wrapped(|ui| {
        let size = ui.text_style_height(&egui::TextStyle::Body);
        let (rect, _) = ui.allocate_exact_size(vec2(size, size), Sense::hover());
        ui.painter().circle_filled(
            rect.center(),
            size * 0.4,
            if state.telemetry_available {
                state_color(state.system_state)
            } else {
                UNKNOWN
            },
        );

        let cpu = f64::from(state.cpu_pct);
        let cpu_label = if state.telemetry_available {
            format!("CPU: {cpu:.0}%")
        } else {
            "CPU: waiting".to_owned()
        };
        let cpu_text = RichText::new(cpu_label);
        ui.label(match cpu_color(state) {
            Some(color) => cpu_text.color(color),
            None => cpu_text.weak(),
        });
        ui.label("|");
        ui.label(if state.telemetry_available {
            mem_text(
                "RAM",
                state.ram_used_bytes,
                state.ram_total_bytes,
                state.thresholds.ram_yellow_pct,
                state.thresholds.ram_red_pct,
            )
        } else {
            RichText::new("RAM: waiting").weak()
        });
        ui.label("|");
        match (
            state.telemetry_available,
            state.vram_used_bytes,
            state.vram_total_bytes,
        ) {
            (true, Some(used), Some(total)) => ui.label(mem_text(
                "VRAM",
                used,
                total,
                state.thresholds.vram_yellow_pct,
                state.thresholds.vram_red_pct,
            )),
            _ => ui.label(RichText::new("VRAM: N/A").weak()),
        };
        ui.label("|");
        let (guard_label, guard_color) = if !state.telemetry_available {
            ("PID Guard: waiting", UNKNOWN)
        } else if state.pending_alerts.is_empty() {
            ("PID Guard: OK", GREEN)
        } else {
            ("PID Guard: alert", YELLOW)
        };
        ui.label(RichText::new(guard_label).color(guard_color));
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pct_handles_zero_total() {
        assert_eq!(pct(5, 0), 0.0);
        assert_eq!(pct(50, 100), 50.0);
    }

    #[test]
    fn pct_colors_follow_passed_thresholds() {
        assert_eq!(pct_color(60.0, 60.0, Some(90.0)), YELLOW);
        assert_eq!(pct_color(80.0, 60.0, Some(80.0)), RED);
        assert_eq!(pct_color(95.0, 60.0, None), YELLOW);
    }

    #[test]
    fn unavailable_cpu_has_no_status_color() {
        let state = DisplayState::default();
        assert_eq!(cpu_color(&state), None);

        let mut state = state;
        state.telemetry_available = true;
        state.cpu_pct = 80.0;
        state.thresholds.cpu_yellow_pct = 70.0;
        assert_eq!(cpu_color(&state), Some(YELLOW));
    }

    #[test]
    fn state_colors_match_spec() {
        assert_eq!(
            state_color(SystemState::Green),
            Color32::from_rgb(76, 175, 80)
        );
        assert_eq!(
            state_color(SystemState::Yellow),
            Color32::from_rgb(255, 193, 7)
        );
        assert_eq!(
            state_color(SystemState::Red),
            Color32::from_rgb(244, 67, 54)
        );
    }
}
