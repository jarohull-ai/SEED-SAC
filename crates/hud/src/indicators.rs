use egui::{Color32, RichText, Sense, vec2};

use crate::{DisplayState, SystemState};

const GREEN: Color32 = Color32::from_rgb(0x4C, 0xAF, 0x50);
const YELLOW: Color32 = Color32::from_rgb(0xFF, 0xC1, 0x07);
const RED: Color32 = Color32::from_rgb(0xF4, 0x43, 0x36);

const GIB: f64 = 1024.0 * 1024.0 * 1024.0;

fn state_color(state: SystemState) -> Color32 {
    match state {
        SystemState::Green => GREEN,
        SystemState::Yellow => YELLOW,
        SystemState::Red => RED,
    }
}

/// Color for a utilisation percentage (0–100).
fn pct_color(pct: f64) -> Color32 {
    if pct >= 90.0 {
        RED
    } else if pct >= 75.0 {
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

fn mem_text(label: &str, used: u64, total: u64) -> RichText {
    let p = pct(used, total);
    RichText::new(format!(
        "{label}: {:.1}/{:.1} GB ({:.0}%)",
        used as f64 / GIB,
        total as f64 / GIB,
        p
    ))
    .color(pct_color(p))
}

pub fn draw_status_bar(ui: &mut egui::Ui, state: &DisplayState) {
    ui.horizontal(|ui| {
        let size = ui.text_style_height(&egui::TextStyle::Body);
        let (rect, _) = ui.allocate_exact_size(vec2(size, size), Sense::hover());
        ui.painter()
            .circle_filled(rect.center(), size * 0.4, state_color(state.system_state));

        let cpu = f64::from(state.cpu_pct);
        ui.label(RichText::new(format!("CPU: {cpu:.0}%")).color(pct_color(cpu)));
        ui.label("|");
        ui.label(mem_text("RAM", state.ram_used_bytes, state.ram_total_bytes));
        ui.label("|");
        match (state.vram_used_bytes, state.vram_total_bytes) {
            (Some(used), Some(total)) => ui.label(mem_text("VRAM", used, total)),
            _ => ui.label(RichText::new("VRAM: N/A").weak()),
        };
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
    fn state_colors_match_spec() {
        assert_eq!(state_color(SystemState::Green), Color32::from_rgb(76, 175, 80));
        assert_eq!(state_color(SystemState::Yellow), Color32::from_rgb(255, 193, 7));
        assert_eq!(state_color(SystemState::Red), Color32::from_rgb(244, 67, 54));
    }
}
