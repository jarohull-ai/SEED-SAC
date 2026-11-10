use std::sync::mpsc::Sender;
use std::time::Duration;

use crate::{SharedDisplayState, UserCommand, indicators, notifications};

/// Action requested by the user from the HUD, sent back to the sentinel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UserAction {
    Resume(u32),
    Terminate(u32),
}

pub struct SacApp {
    state: SharedDisplayState,
    action_tx: Sender<UserAction>,
    command_tx: Sender<UserCommand>,
    input_buffer: String,
}

impl SacApp {
    pub fn new(
        state: SharedDisplayState,
        action_tx: Sender<UserAction>,
        command_tx: Sender<UserCommand>,
    ) -> Self {
        Self {
            state,
            action_tx,
            command_tx,
            input_buffer: String::new(),
        }
    }

    /// Sends the current input buffer as a [`UserCommand`] if it is non-empty
    /// and clears the buffer. Returns `true` if a command was sent.
    fn submit_input(&mut self) -> bool {
        let text = self.input_buffer.trim();
        if text.is_empty() {
            return false;
        }
        // A closed receiver just means nobody is listening; drop the command.
        let _ = self.command_tx.send(UserCommand(text.to_owned()));
        self.input_buffer.clear();
        true
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

        egui::TopBottomPanel::bottom("sac_input_bar")
            .resizable(false)
            .show(ctx, |ui| {
                let response = ui.add(
                    egui::TextEdit::singleline(&mut self.input_buffer)
                        .id(egui::Id::new("sac_input"))
                        .hint_text("Wpisz polecenie...")
                        .desired_width(f32::INFINITY),
                );

                // Check the key event while the edit still owns focus. Relying
                // on `lost_focus` is renderer/input-backend dependent and can
                // make Enter appear to do nothing.
                if response.has_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    self.submit_input();
                    response.request_focus();
                }

                if let Some(reply) = &state.command_response {
                    egui::ScrollArea::vertical()
                        .id_salt("sac_command_reply")
                        .max_height(36.0)
                        .auto_shrink([false, false])
                        .show(ui, |ui| {
                            ui.label(egui::RichText::new(reply).small().weak());
                        });
                }
            });

        if !state.pending_alerts.is_empty() {
            egui::CentralPanel::default().show(ctx, |ui| {
                egui::ScrollArea::vertical()
                    .id_salt("sac_alerts")
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        notifications::draw_alerts(ui, &state.pending_alerts, &self.action_tx);
                    });
            });
        }

        ctx.request_repaint_after(Duration::from_millis(100));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    fn app() -> (SacApp, mpsc::Receiver<UserCommand>) {
        let (action_tx, _action_rx) = mpsc::channel();
        let (command_tx, command_rx) = mpsc::channel();
        (
            SacApp::new(crate::new_shared_state(), action_tx, command_tx),
            command_rx,
        )
    }

    #[test]
    fn submit_sends_and_clears() {
        let (mut app, rx) = app();
        app.input_buffer = "  status  ".into();
        assert!(app.submit_input());
        assert_eq!(rx.try_recv().unwrap().0, "status");
        assert!(app.input_buffer.is_empty());
    }

    #[test]
    fn submit_ignores_blank_input() {
        let (mut app, rx) = app();
        app.input_buffer = "   ".into();
        assert!(!app.submit_input());
        assert!(rx.try_recv().is_err());
    }
}
