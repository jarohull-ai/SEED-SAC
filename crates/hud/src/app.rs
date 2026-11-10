use std::sync::mpsc::Sender;
use std::time::Duration;

use crate::{SharedDisplayState, indicators, notifications};

/// Action requested by the user from the HUD, sent back to the sentinel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UserAction {
    Resume(u32),
    Terminate(u32),
}

pub struct SacApp {
    state: SharedDisplayState,
    action_tx: Sender<UserAction>,
}

impl SacApp {
    pub fn new(state: SharedDisplayState, action_tx: Sender<UserAction>) -> Self {
        Self { state, action_tx }
    }
}

impl eframe::App for SacApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let state = self.state.load_full();

        egui::TopBottomPanel::top("sac_status_bar")
            .resizable(false)
            .show(ctx, |ui| {
                indicators::draw_status_bar(ui, &state);
            });

        if !state.pending_alerts.is_empty() {
            egui::CentralPanel::default().show(ctx, |ui| {
                notifications::draw_alerts(ui, &state.pending_alerts, &self.action_tx);
            });
        }

        ctx.request_repaint_after(Duration::from_secs(1));
    }
}
