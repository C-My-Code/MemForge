#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod config;
mod config_panel;
mod dashboard;
mod format;
mod session;
mod sysinfo;
mod widgets;

fn main() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_title("MemForge")
            .with_inner_size([1100.0, 750.0])
            .with_min_inner_size([800.0, 500.0]),
        ..Default::default()
    };
    eframe::run_native(
        "MemForge",
        options,
        Box::new(|cc| Ok(Box::new(app::MemForgeApp::new(cc)))),
    )
}
