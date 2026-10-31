//! Top-level eframe application.

use crate::config::ConfigState;
use crate::config_panel::{self, Action, PanelCtx};
use crate::dashboard::{self, DashAction};
use crate::format;
use crate::session::Session;
use crate::sysinfo::{self, SysState};
use eframe::egui::{self, RichText};
use std::time::Duration;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Dashboard,
    System,
}

pub struct MemForgeApp {
    cfg: ConfigState,
    sys: SysState,
    session: Option<Session>,
    tab: Tab,
    start_error: Option<String>,
    status_msg: Option<String>,
    confirm_close: bool,
    closing: bool,
}

impl MemForgeApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        cc.egui_ctx.set_theme(egui::Theme::Dark);
        MemForgeApp {
            cfg: ConfigState::default(),
            sys: SysState::load(&cc.egui_ctx),
            session: None,
            tab: Tab::Dashboard,
            start_error: None,
            status_msg: None,
            confirm_close: false,
            closing: false,
        }
    }

    fn running(&self) -> bool {
        self.session.as_ref().is_some_and(|s| s.is_active())
    }

    fn start(&mut self) {
        self.start_error = None;
        self.status_msg = None;
        let avail = self.sys.snapshot().map(|s| s.memory.available_physical);
        let config = match self.cfg.to_run_config(avail) {
            Ok(c) => c,
            Err(e) => {
                self.start_error = Some(e);
                return;
            }
        };
        match memforge_engine::start(config.clone()) {
            Ok(h) => {
                self.session = Some(Session::new(h, config));
                self.tab = Tab::Dashboard;
            }
            Err(e) => self.start_error = Some(format!("Could not start: {e}")),
        }
    }

    fn save_report(&mut self) {
        let Some(r) = self.session.as_ref().and_then(|s| s.report.as_ref()) else {
            return;
        };
        let name = format!(
            "memforge-report-{}.json",
            format::file_timestamp(r.started_at)
        );
        let json = r.to_json_pretty();
        let picked = rfd::FileDialog::new()
            .set_file_name(name.as_str())
            .add_filter("JSON", &["json"])
            .save_file();
        if let Some(path) = picked {
            self.status_msg = Some(match std::fs::write(&path, json) {
                Ok(()) => format!("Saved report to {}", path.display()),
                Err(e) => format!("Failed to save report: {e}"),
            });
        }
    }
}

impl eframe::App for MemForgeApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.sys.poll();
        if let Some(s) = &mut self.session {
            s.poll();
        }
        if self.running() {
            ctx.request_repaint_after(Duration::from_millis(100));
        }

        // Window close handling.
        if ctx.input(|i| i.viewport().close_requested()) && self.running() {
            // Keep cancelling while a stop is in progress; the window closes
            // itself once the engine has finished.
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            if !self.closing {
                self.confirm_close = true;
            }
        }
        if self.closing {
            ctx.request_repaint_after(Duration::from_millis(50));
            if !self.running() {
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
        }
        if self.closing && self.running() {
            egui::Window::new("Stopping...")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .show(&ctx, |ui| {
                    ui.label(
                        "Stopping the test and releasing memory. The window will close shortly.",
                    );
                });
        }
        if self.confirm_close {
            egui::Window::new("Test in progress")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .show(&ctx, |ui| {
                    ui.label("A memory test is running. Stop it and exit?");
                    ui.horizontal(|ui| {
                        if ui.button("Stop and exit").clicked() {
                            if let Some(s) = &mut self.session {
                                s.stop();
                            }
                            self.closing = true;
                            self.confirm_close = false;
                        }
                        if ui.button("Keep running").clicked() {
                            self.confirm_close = false;
                        }
                    });
                });
        }

        let running = self.running();
        let mut action = Action::None;
        egui::Panel::left("config")
            .default_size(310.0)
            .min_size(260.0)
            .show(ui, |ui| {
                ui.add_space(4.0);
                ui.heading("MemForge");
                ui.label(
                    RichText::new("DDR4 / DDR5 memory stability tester")
                        .weak()
                        .small(),
                );
                ui.separator();
                action = config_panel::show(
                    ui,
                    &mut self.cfg,
                    PanelCtx {
                        sys: &self.sys,
                        running,
                        start_error: self.start_error.as_deref(),
                    },
                );
            });
        match action {
            Action::Start => self.start(),
            Action::Stop => {
                if let Some(s) = &mut self.session {
                    s.stop();
                }
            }
            Action::None => {}
        }

        let mut dash_action = DashAction::None;
        let mut refresh_sys = false;
        egui::CentralPanel::default().show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.selectable_value(&mut self.tab, Tab::Dashboard, "Dashboard");
                ui.selectable_value(&mut self.tab, Tab::System, "System info");
                if let Some(m) = &self.status_msg {
                    ui.label(RichText::new(m).weak().small());
                }
            });
            ui.separator();
            match self.tab {
                Tab::Dashboard => match &self.session {
                    Some(s) => dash_action = dashboard::show(ui, s),
                    None => dashboard::idle(ui),
                },
                Tab::System => refresh_sys = sysinfo::show(ui, &self.sys),
            }
        });
        if refresh_sys && !matches!(self.sys, SysState::Loading(_)) {
            self.sys = SysState::load(&ctx);
        }
        match dash_action {
            DashAction::SaveReport => self.save_report(),
            DashAction::CopySummary => {
                if let Some(t) = self.session.as_ref().and_then(|s| s.summary_text()) {
                    ctx.copy_text(t);
                    self.status_msg = Some("Summary copied to clipboard".into());
                }
            }
            DashAction::None => {}
        }
    }

    fn on_exit(&mut self) {
        // Dropping the session requests stop and joins the engine; do that on
        // a helper thread and wait only a bounded time so exit never hangs.
        if let Some(mut s) = self.session.take() {
            s.stop();
            let (tx, rx) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                drop(s);
                let _ = tx.send(());
            });
            let _ = rx.recv_timeout(Duration::from_secs(5));
        }
    }
}
