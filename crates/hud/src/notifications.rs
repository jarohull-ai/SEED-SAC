use std::sync::mpsc::Sender;

use egui::{Color32, RichText, Stroke};

use crate::{Alert, AlertAction, UserAction};

const WARN_FILL: Color32 = Color32::from_rgb(0x4A, 0x3A, 0x10);
const WARN_STROKE: Color32 = Color32::from_rgb(0xFF, 0xC1, 0x07);

pub fn draw_alerts(
    ui: &mut egui::Ui,
    alerts: &[Alert],
    action_tx: &Sender<UserAction>,
    terminate_enabled: bool,
) -> bool {
    let mut action_sent = false;
    for alert in alerts {
        egui::Frame::new()
            .fill(WARN_FILL)
            .stroke(Stroke::new(1.0_f32, WARN_STROKE))
            .corner_radius(4)
            .inner_margin(6)
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.label(
                    RichText::new(format!("⚠ {} (PID {})", alert.process_name, alert.pid))
                        .color(WARN_STROKE)
                        .strong(),
                );
                ui.label(RichText::new(&alert.message).color(Color32::WHITE));
                ui.horizontal(|ui| {
                    for &action in &alert.actions {
                        let (label, user_action) = match action {
                            AlertAction::Resume => ("Wznów", UserAction::Resume(alert.pid)),
                            AlertAction::Terminate => ("Zakończ", UserAction::Terminate(alert.pid)),
                        };
                        let button = ui.add_enabled(
                            !matches!(action, AlertAction::Terminate) || terminate_enabled,
                            egui::Button::new(label),
                        );
                        if button.clicked() {
                            // Receiver gone means the sentinel shut down; nothing to do.
                            let _ = action_tx.send(user_action);
                            action_sent = true;
                        }
                    }
                });
            });
        ui.add_space(4.0);
    }
    action_sent
}
