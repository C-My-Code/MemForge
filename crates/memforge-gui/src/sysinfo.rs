//! Background system snapshot loading and the "System" tab.

use crate::format;
use eframe::egui::{self, RichText, Ui};
use memforge_engine::{system_snapshot, SystemSnapshot};
use std::sync::mpsc::{channel, Receiver, TryRecvError};

pub enum SysState {
    Loading(Receiver<SystemSnapshot>),
    Ready(Box<SystemSnapshot>),
    Failed,
}

impl SysState {
    /// Spawn the (possibly slow) snapshot on a background thread.
    pub fn load(ctx: &egui::Context) -> Self {
        let (tx, rx) = channel();
        let ctx = ctx.clone();
        let spawned = std::thread::Builder::new()
            .name("memforge-sysinfo".into())
            .spawn(move || {
                let snap = system_snapshot();
                let _ = tx.send(snap);
                ctx.request_repaint();
            });
        match spawned {
            Ok(_) => SysState::Loading(rx),
            Err(_) => SysState::Failed,
        }
    }

    pub fn poll(&mut self) {
        if let SysState::Loading(rx) = self {
            match rx.try_recv() {
                Ok(s) => *self = SysState::Ready(Box::new(s)),
                Err(TryRecvError::Empty) => {}
                Err(TryRecvError::Disconnected) => *self = SysState::Failed,
            }
        }
    }

    pub fn snapshot(&self) -> Option<&SystemSnapshot> {
        match self {
            SysState::Ready(s) => Some(s),
            _ => None,
        }
    }

    pub fn logical_cpus(&self) -> usize {
        self.snapshot()
            .map(|s| s.cpu.logical.len())
            .filter(|n| *n > 0)
            .or_else(|| std::thread::available_parallelism().ok().map(|n| n.get()))
            .unwrap_or(1)
    }
}

/// Returns true when the user asked for a refresh.
pub fn show(ui: &mut Ui, state: &SysState) -> bool {
    let mut refresh = false;
    ui.horizontal(|ui| {
        ui.heading("System");
        if ui.button("Refresh").clicked() {
            refresh = true;
        }
    });
    let snap = match state {
        SysState::Loading(_) => {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label("Reading system information (CPU topology, SMBIOS, memory)...");
            });
            return refresh;
        }
        SysState::Failed => {
            ui.colored_label(crate::widgets::RED, "System information is unavailable.");
            return refresh;
        }
        SysState::Ready(s) => s,
    };
    egui::ScrollArea::vertical()
        .auto_shrink([false; 2])
        .show(ui, |ui| {
            egui::Grid::new("sys_grid")
                .num_columns(2)
                .spacing([16.0, 4.0])
                .show(ui, |ui| {
                    ui.label("OS");
                    ui.label(&snap.os);
                    ui.end_row();
                    ui.label("Elevated");
                    ui.label(if snap.elevated {
                        "yes (administrator)"
                    } else {
                        "no"
                    });
                    ui.end_row();
                    ui.label("CPU");
                    ui.label(&snap.cpu.brand);
                    ui.end_row();
                    ui.label("Cores / threads");
                    ui.label(format!(
                        "{} physical / {} logical",
                        snap.cpu.physical_cores,
                        snap.cpu.logical.len()
                    ));
                    ui.end_row();
                    let f = &snap.cpu.features;
                    ui.label("SIMD");
                    ui.label(format!(
                        "SSE2: {}  AVX2: {}  AVX-512F: {}",
                        yn(f.sse2),
                        yn(f.avx2),
                        yn(f.avx512f)
                    ));
                    ui.end_row();
                    ui.label("Physical memory");
                    ui.label(format!(
                        "{} total, {} available",
                        format::bytes(snap.memory.total_physical),
                        format::bytes(snap.memory.available_physical)
                    ));
                    ui.end_row();
                    ui.label("Page size");
                    ui.label(format!(
                        "{} KiB; large page: {}",
                        snap.memory.page_size / 1024,
                        snap.memory
                            .large_page_size
                            .map(|n| format!("{} MiB", n / (1024 * 1024)))
                            .unwrap_or_else(|| "unavailable".into())
                    ));
                    ui.end_row();
                });
            ui.add_space(12.0);
            ui.label(RichText::new("Memory modules (SMBIOS)").strong());
            if snap.dimms.is_empty() {
                ui.label(RichText::new("No DIMM information available.").weak());
            } else {
                egui::Grid::new("dimm_grid")
                    .striped(true)
                    .spacing([14.0, 4.0])
                    .show(ui, |ui| {
                        for h in [
                            "Slot",
                            "Bank",
                            "Maker",
                            "Part number",
                            "Size",
                            "Type",
                            "Rated",
                            "Configured",
                            "Volt",
                            "Ranks",
                        ] {
                            ui.label(RichText::new(h).strong());
                        }
                        ui.end_row();
                        for d in &snap.dimms {
                            ui.label(&d.locator);
                            ui.label(&d.bank_locator);
                            ui.label(&d.manufacturer);
                            ui.label(d.part_number.trim());
                            ui.label(format::bytes(d.size_bytes));
                            ui.label(&d.memory_type);
                            ui.label(format!("{} MT/s", d.speed_mts));
                            ui.label(format!("{} MT/s", d.configured_speed_mts));
                            ui.label(if d.configured_voltage_mv > 0 {
                                format!("{:.3} V", d.configured_voltage_mv as f64 / 1000.0)
                            } else {
                                "-".into()
                            });
                            ui.label(d.rank.to_string());
                            ui.end_row();
                        }
                    });
            }
        });
    refresh
}

fn yn(b: bool) -> &'static str {
    if b {
        "yes"
    } else {
        "no"
    }
}
