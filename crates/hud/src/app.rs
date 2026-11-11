use std::collections::BTreeSet;
use std::sync::mpsc::Sender;
use std::time::Duration;

use crate::{indicators, notifications, ConsoleEntry, LogLevel, SharedDisplayState, UserCommand};

const MAX_COMMAND_INPUT_CHARS: usize = 4096;
const CONFIRM_COMMAND: &str = "__sac_confirm";
const CANCEL_COMMAND: &str = "__sac_cancel";
const HELP_COMMAND: &str = "__sac_help";

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
    input_focus_requested: bool,
    input_was_truncated: bool,
    confirmation_in_flight: bool,
    alert_action_in_flight: bool,
    last_alert_pids: BTreeSet<u32>,
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
            input_focus_requested: false,
            input_was_truncated: false,
            confirmation_in_flight: false,
            alert_action_in_flight: false,
            last_alert_pids: BTreeSet::new(),
        }
    }

    fn sync_alert_action_state(&mut self, state: &crate::DisplayState) {
        let alert_pids = state.pending_alerts.iter().map(|alert| alert.pid).collect();
        let confirmation_visible = state
            .command_response
            .as_deref()
            .is_some_and(|reply| reply.starts_with("POTWIERDZENIE:"));

        // Resume succeeds without producing a command banner. The sentinel
        // removes that alert from the next snapshot, which is the positive
        // acknowledgement that releases the action latch.
        if alert_pids != self.last_alert_pids {
            self.alert_action_in_flight = false;
        }
        self.last_alert_pids = alert_pids;

        if !confirmation_visible {
            self.confirmation_in_flight = false;
            if state.command_response.is_some() {
                self.alert_action_in_flight = false;
            }
        } else {
            self.alert_action_in_flight = false;
        }
    }

    /// Sends the current input buffer as a [`UserCommand`] if it is non-empty
    /// and clears the buffer. Returns `true` if a command was sent.
    fn submit_input(&mut self) -> bool {
        self.enforce_input_limit();
        let text = self.input_buffer.trim();
        if text.is_empty() {
            return false;
        }
        // A closed receiver just means nobody is listening; drop the command.
        let _ = self.command_tx.send(UserCommand(text.to_owned()));
        self.input_buffer.clear();
        self.input_was_truncated = false;
        true
    }

    fn submit_confirmation(&mut self, command: &str) {
        if self.confirmation_in_flight {
            return;
        }
        let _ = self.command_tx.send(UserCommand(command.to_owned()));
        self.input_buffer.clear();
        self.input_was_truncated = false;
        self.confirmation_in_flight = true;
    }

    fn enforce_input_limit(&mut self) {
        if let Some((byte_index, _)) = self
            .input_buffer
            .char_indices()
            .nth(MAX_COMMAND_INPUT_CHARS)
        {
            self.input_buffer.truncate(byte_index);
            self.input_was_truncated = true;
        } else if self.input_buffer.chars().count() < MAX_COMMAND_INPUT_CHARS {
            // Keep the warning while the buffer remains exactly at the
            // enforced boundary, but clear it once the user edits below it.
            self.input_was_truncated = false;
        }
    }
}

impl eframe::App for SacApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.render(ctx);
    }
}

impl SacApp {
    fn render(&mut self, ctx: &egui::Context) {
        let state = self.state.load_full();
        let confirmation_visible = state
            .command_response
            .as_deref()
            .is_some_and(|reply| reply.starts_with("POTWIERDZENIE:"));
        self.sync_alert_action_state(&state);

        egui::TopBottomPanel::top("sac_status_bar")
            .resizable(false)
            .show(ctx, |ui| {
                indicators::draw_status_bar(ui, &state);
            });

        egui::TopBottomPanel::bottom("sac_live_console")
            .resizable(true)
            .default_height(170.0)
            .min_height(72.0)
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.strong("Live Console");
                    ui.label("historia zdarzeń i pełne odpowiedzi L1");
                    if ui.small_button("Kopiuj L1").clicked() {
                        let text = state
                            .console
                            .iter()
                            .filter(|entry| entry.source == "L1")
                            .map(|entry| entry.text.as_str())
                            .collect::<Vec<_>>()
                            .join("\n\n");
                        if !text.is_empty() {
                            ui.ctx().copy_text(text);
                        }
                    }
                    if ui.small_button("Kopiuj system").clicked() {
                        let text = state
                            .console
                            .iter()
                            .filter(|entry| entry.source != "L1")
                            .map(|entry| format!("[{}] {}", entry.source, entry.text))
                            .collect::<Vec<_>>()
                            .join("\n");
                        if !text.is_empty() {
                            ui.ctx().copy_text(text);
                        }
                    }
                    if ui.small_button("Kopiuj wszystko").clicked() {
                        let text = state
                            .console
                            .iter()
                            .map(|entry| format!("[{}] {}", entry.source, entry.text))
                            .collect::<Vec<_>>()
                            .join("\n");
                        if !text.is_empty() {
                            ui.ctx().copy_text(text);
                        }
                    }
                });
                egui::ScrollArea::vertical()
                    .id_salt("sac_live_console_scroll")
                    .stick_to_bottom(true)
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        for entry in &state.console {
                            draw_console_entry(ui, entry);
                        }
                    });
            });

        egui::TopBottomPanel::bottom("sac_input_bar")
            .resizable(false)
            .show(ctx, |ui| {
                let mut submitted = false;
                self.enforce_input_limit();
                ui.horizontal(|ui| {
                    let button_width = 96.0;
                    let help_button_width = 112.0;
                    let response = ui.add_sized(
                        [
                            (ui.available_width()
                                - button_width
                                - help_button_width
                                - 2.0 * ui.spacing().item_spacing.x)
                                .max(1.0),
                            0.0,
                        ],
                        egui::TextEdit::singleline(&mut self.input_buffer)
                            .id(egui::Id::new("sac_input"))
                            .hint_text("Wpisz polecenie..."),
                    );
                    self.enforce_input_limit();

                    if !self.input_focus_requested {
                        response.request_focus();
                        self.input_focus_requested = true;
                    }

                    // In egui, a single-line edit loses focus when it consumes Enter.
                    // Pair that transition with the key event so Enter submits reliably.
                    // Keep this as one boolean gate so a key event and a same-frame
                    // click submit only once.
                    let enter_pressed =
                        response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                    let send_clicked = ui
                        .add_sized(
                            [button_width, response.rect.height()],
                            egui::Button::new("Wyślij"),
                        )
                        .clicked();
                    let help_clicked = ui
                        .add_sized(
                            [help_button_width, response.rect.height()],
                            egui::Button::new("Polecenia"),
                        )
                        .clicked();

                    if enter_pressed
                        && confirmation_visible
                        && self.input_buffer.trim().is_empty()
                        && !submitted
                    {
                        submitted = true;
                        self.submit_confirmation(CONFIRM_COMMAND);
                        response.request_focus();
                    } else if help_clicked && !submitted {
                        submitted = true;
                        let _ = self.command_tx.send(UserCommand(HELP_COMMAND.to_owned()));
                        response.request_focus();
                    } else if (enter_pressed || send_clicked) && !submitted {
                        submitted = true;
                        self.submit_input();
                        response.request_focus();
                    }
                });

                if self.input_was_truncated {
                    ui.label(
                        egui::RichText::new(format!(
                            "Polecenie skrócone do {} znaków.",
                            MAX_COMMAND_INPUT_CHARS
                        ))
                        .small()
                        .color(egui::Color32::from_rgb(220, 150, 50)),
                    );
                }

                if let Some(reply) = &state.command_response {
                    egui::ScrollArea::vertical()
                        .id_salt("sac_command_reply")
                        .max_height(36.0)
                        .auto_shrink([false, false])
                        .show(ui, |ui| {
                            ui.label(egui::RichText::new(reply).small().weak());
                        });

                    if reply.starts_with("POTWIERDZENIE:") {
                        if ui.input(|input| input.key_pressed(egui::Key::Escape))
                            && !self.confirmation_in_flight
                        {
                            self.submit_confirmation(CANCEL_COMMAND);
                            self.input_focus_requested = false;
                        }
                        ui.label(
                            egui::RichText::new("Enter: potwierdź · Esc: anuluj")
                                .small()
                                .weak(),
                        );
                        ui.horizontal(|ui| {
                            if ui
                                .add_enabled(
                                    !self.confirmation_in_flight,
                                    egui::Button::new("Potwierdź"),
                                )
                                .clicked()
                            {
                                self.submit_confirmation(CONFIRM_COMMAND);
                                self.input_focus_requested = false;
                            }
                            if ui
                                .add_enabled(
                                    !self.confirmation_in_flight,
                                    egui::Button::new("Anuluj"),
                                )
                                .clicked()
                            {
                                self.submit_confirmation(CANCEL_COMMAND);
                                self.input_focus_requested = false;
                            }
                        });
                    }
                }
            });

        if !state.pending_alerts.is_empty() {
            egui::CentralPanel::default().show(ctx, |ui| {
                egui::ScrollArea::vertical()
                    .id_salt("sac_alerts")
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        let alert_action_sent = notifications::draw_alerts(
                            ui,
                            &state.pending_alerts,
                            &self.action_tx,
                            !confirmation_visible
                                && !self.confirmation_in_flight
                                && !self.alert_action_in_flight,
                        );
                        self.alert_action_in_flight |= alert_action_sent;
                    });
            });
        }

        ctx.request_repaint_after(Duration::from_millis(100));
    }
}

fn draw_console_entry(ui: &mut egui::Ui, entry: &ConsoleEntry) {
    let color = match entry.level {
        LogLevel::Info => egui::Color32::LIGHT_GRAY,
        LogLevel::L1Response => egui::Color32::from_rgb(130, 200, 255),
        LogLevel::Warning => egui::Color32::from_rgb(255, 196, 90),
        LogLevel::Alert => egui::Color32::from_rgb(255, 110, 110),
    };
    ui.horizontal_top(|ui| {
        ui.label(
            egui::RichText::new(format!("[{}]", entry.source))
                .color(color)
                .strong(),
        );
        ui.add(
            egui::Label::new(egui::RichText::new(&entry.text).color(color))
                .wrap_mode(egui::TextWrapMode::Wrap),
        );
        if ui.small_button("Kopiuj").clicked() {
            ui.ctx().copy_text(entry.text.clone());
        }
    });
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
        assert!(!app.submit_input());
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn submit_ignores_blank_input() {
        let (mut app, rx) = app();
        app.input_buffer = "   ".into();
        assert!(!app.submit_input());
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn submit_truncates_oversized_unicode_input() {
        let (mut app, rx) = app();
        app.input_buffer = "ą".repeat(MAX_COMMAND_INPUT_CHARS + 1);

        assert!(app.submit_input());
        let command = rx.try_recv().unwrap().0;
        assert_eq!(command.chars().count(), MAX_COMMAND_INPUT_CHARS);
        assert!(command.chars().all(|character| character == 'ą'));
        assert!(app.input_buffer.is_empty());
    }

    #[test]
    fn enforce_input_limit_preserves_utf8_boundaries() {
        let mut app = app().0;
        app.input_buffer = format!("{}z", "ą".repeat(MAX_COMMAND_INPUT_CHARS));

        app.enforce_input_limit();

        assert_eq!(app.input_buffer.chars().count(), MAX_COMMAND_INPUT_CHARS);
        assert!(!app.input_buffer.ends_with('z'));
        assert!(app.input_was_truncated);
    }

    #[test]
    fn enforce_input_limit_does_not_report_valid_input_as_truncated() {
        let mut app = app().0;
        app.input_buffer = "status".into();

        app.enforce_input_limit();

        assert!(!app.input_was_truncated);
    }

    #[test]
    fn editing_truncated_input_below_limit_clears_feedback() {
        let mut app = app().0;
        app.input_buffer = format!("{}z", "a".repeat(MAX_COMMAND_INPUT_CHARS));
        app.enforce_input_limit();
        assert!(app.input_was_truncated);

        app.input_buffer.pop();
        app.enforce_input_limit();

        assert!(!app.input_was_truncated);
    }

    #[test]
    fn successful_submit_clears_truncation_feedback() {
        let (mut app, _rx) = app();
        app.input_buffer = format!("{}z", "a".repeat(MAX_COMMAND_INPUT_CHARS));

        assert!(app.submit_input());
        assert!(!app.input_was_truncated);
    }

    #[test]
    fn resolved_alert_clears_action_latch_without_command_response() {
        let (mut app, _rx) = app();
        let mut state = crate::DisplayState::default();
        state.pending_alerts.push(crate::Alert {
            pid: 42,
            process_name: "cargo".to_owned(),
            message: "frozen".to_owned(),
            actions: vec![crate::AlertAction::Resume],
        });

        app.sync_alert_action_state(&state);
        app.alert_action_in_flight = true;
        state.pending_alerts.clear();
        app.sync_alert_action_state(&state);

        assert!(!app.alert_action_in_flight);
    }

    #[test]
    fn egui_frame_renders_console_and_confirmation_flow() {
        let (action_tx, _action_rx) = mpsc::channel();
        let (command_tx, command_rx) = mpsc::channel();
        let shared = crate::new_shared_state();
        let mut state = crate::DisplayState {
            command_response: Some("POTWIERDZENIE: kill".to_owned()),
            ..crate::DisplayState::default()
        };
        state.console.push_back(crate::ConsoleEntry {
            timestamp: std::time::Instant::now(),
            source: "L1",
            level: crate::LogLevel::L1Response,
            text: "Diagnoza: test".to_owned(),
        });
        shared.store(std::sync::Arc::new(state));
        let mut app = SacApp::new(shared, action_tx, command_tx);

        let context = egui::Context::default();
        let output = context.run(Default::default(), |ctx| app.render(ctx));
        assert!(!output.shapes.is_empty());

        app.submit_confirmation(CONFIRM_COMMAND);
        assert_eq!(command_rx.try_recv().unwrap().0, CONFIRM_COMMAND);
    }
}
