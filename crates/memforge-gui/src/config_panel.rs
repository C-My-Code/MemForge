//! Left-hand configuration panel.

use crate::config::{max_exact_gib, ConfigState, MemMode, TimeUnit};
use crate::sysinfo::SysState;
use crate::widgets;
use eframe::egui::{self, RichText, Ui};
use memforge_engine::memforge_core::{default_test_suite, TestId};
use memforge_engine::memforge_platform::ThreadPriority;

pub enum Action {
    None,
    Start,
    Stop,
}

pub struct PanelCtx<'a> {
    pub sys: &'a SysState,
    pub running: bool,
    pub start_error: Option<&'a str>,
}

const GIB: f64 = 1024.0 * 1024.0 * 1024.0;

pub fn show(ui: &mut Ui, cfg: &mut ConfigState, pc: PanelCtx<'_>) -> Action {
    let mut action = Action::None;
    let snap = pc.sys.snapshot();

    if let Some(s) = snap {
        if !s.elevated {
            egui::Frame::new()
                .fill(widgets::AMBER.gamma_multiply(0.2))
                .stroke(egui::Stroke::new(1.0, widgets::AMBER))
                .corner_radius(4.0)
                .inner_margin(6.0)
                .show(ui, |ui| {
                    ui.label(
                        RichText::new(
                            "Not running as administrator: large pages and memory locking may be limited or unavailable.",
                        )
                        .color(widgets::AMBER)
                        .small(),
                    );
                });
            ui.add_space(4.0);
        }
    }

    // Start / Stop stay usable while the rest is locked.
    ui.horizontal(|ui| {
        if ui
            .add_enabled(
                !pc.running,
                egui::Button::new(RichText::new("Start").strong()).min_size(egui::vec2(90.0, 28.0)),
            )
            .clicked()
        {
            action = Action::Start;
        }
        if ui
            .add_enabled(
                pc.running,
                egui::Button::new(RichText::new("Stop").strong()).min_size(egui::vec2(90.0, 28.0)),
            )
            .clicked()
        {
            action = Action::Stop;
        }
    });
    if let Some(e) = pc.start_error {
        ui.colored_label(widgets::RED, e);
    }
    ui.separator();

    let logical = pc.sys.logical_cpus();
    egui::ScrollArea::vertical()
        .auto_shrink([false; 2])
        .show(ui, |ui| {
            ui.add_enabled_ui(!pc.running, |ui| {
                memory_section(
                    ui,
                    cfg,
                    snap.map(|s| (s.memory.total_physical, s.memory.available_physical)),
                );
                threads_section(ui, cfg, logical);
                tests_section(ui, cfg);
                stop_section(ui, cfg);
                options_section(ui, cfg);
            });
        });
    action
}

fn memory_section(ui: &mut Ui, cfg: &mut ConfigState, mem: Option<(u64, u64)>) {
    ui.label(RichText::new("Memory to test").strong());
    ui.horizontal(|ui| {
        ui.selectable_value(&mut cfg.mem_mode, MemMode::Percent, "% of available");
        ui.selectable_value(&mut cfg.mem_mode, MemMode::ExactGib, "Exact GiB");
    });
    let avail_gib = mem.map(|m| m.1 as f64 / GIB);
    match cfg.mem_mode {
        MemMode::Percent => {
            ui.add(
                egui::Slider::new(&mut cfg.mem_percent, 1.0..=100.0)
                    .suffix(" %")
                    .fixed_decimals(0),
            );
            match avail_gib {
                Some(a) => ui.label(format!(
                    "= {:.2} GiB of {:.2} GiB available",
                    a * cfg.mem_percent as f64 / 100.0,
                    a
                )),
                None => ui.label(RichText::new("(available memory not known yet)").weak()),
            };
        }
        MemMode::ExactGib => {
            // Never offer more than the available RAM (minus a small reserve).
            let max = max_exact_gib(mem.map(|m| m.1)).unwrap_or(1024.0).max(0.25);
            if cfg.mem_gib > max {
                cfg.mem_gib = max;
            }
            ui.add(
                egui::Slider::new(&mut cfg.mem_gib, 0.25..=max)
                    .suffix(" GiB")
                    .logarithmic(true),
            );
            ui.horizontal(|ui| {
                ui.label("Exact:");
                ui.add(
                    egui::DragValue::new(&mut cfg.mem_gib)
                        .speed(0.05)
                        .range(0.01..=max)
                        .suffix(" GiB"),
                );
            });
            if let Some(a) = avail_gib {
                ui.label(
                    RichText::new(format!(
                        "max {max:.2} GiB ({a:.2} GiB available, 256 MiB reserved)"
                    ))
                    .weak()
                    .small(),
                );
            }
        }
    }
    ui.add_space(6.0);
}

fn threads_section(ui: &mut Ui, cfg: &mut ConfigState, logical: usize) {
    ui.label(RichText::new("Threads").strong());
    ui.horizontal(|ui| {
        ui.selectable_value(&mut cfg.threads_all, true, format!("All ({logical})"));
        ui.selectable_value(&mut cfg.threads_all, false, "Custom");
    });
    if !cfg.threads_all {
        cfg.threads_custom = cfg.threads_custom.clamp(1, logical);
        ui.add(egui::Slider::new(&mut cfg.threads_custom, 1..=logical));
    }
    ui.add_space(6.0);
}

fn tests_section(ui: &mut Ui, cfg: &mut ConfigState) {
    ui.label(RichText::new("Tests").strong());
    ui.horizontal(|ui| {
        if ui
            .small_button("Default")
            .on_hover_text("Everything except the slow Bit fade test")
            .clicked()
        {
            cfg.tests = default_test_suite().into_iter().collect();
        }
        if ui.small_button("All").clicked() {
            cfg.tests = TestId::ALL.iter().copied().collect();
        }
        if ui.small_button("None").clicked() {
            cfg.tests.clear();
        }
    });
    for &t in TestId::ALL {
        let mut on = cfg.tests.contains(&t);
        if ui
            .checkbox(&mut on, t.title())
            .on_hover_text(t.description())
            .changed()
        {
            if on {
                cfg.tests.insert(t);
            } else {
                cfg.tests.remove(&t);
            }
        }
    }
    if cfg.tests.contains(&TestId::BitFade) {
        ui.horizontal(|ui| {
            ui.label("Bit fade idle:");
            ui.add(
                egui::DragValue::new(&mut cfg.bit_fade_secs)
                    .range(1..=86_400)
                    .suffix(" s"),
            );
        });
    }
    if cfg.tests.is_empty() {
        ui.colored_label(widgets::AMBER, "Select at least one test.");
    }
    ui.add_space(6.0);
}

fn stop_section(ui: &mut Ui, cfg: &mut ConfigState) {
    ui.label(RichText::new("Stop conditions").strong());
    ui.horizontal(|ui| {
        ui.checkbox(&mut cfg.passes_enabled, "Passes");
        ui.add_enabled(
            cfg.passes_enabled,
            egui::DragValue::new(&mut cfg.passes).range(1..=1_000_000),
        );
    });
    ui.horizontal(|ui| {
        ui.checkbox(&mut cfg.duration_enabled, "Duration");
        ui.add_enabled_ui(cfg.duration_enabled, |ui| {
            ui.add(
                egui::DragValue::new(&mut cfg.duration_value)
                    .range(1.0..=100_000.0)
                    .speed(1.0),
            );
            ui.selectable_value(&mut cfg.duration_unit, TimeUnit::Minutes, "min");
            ui.selectable_value(&mut cfg.duration_unit, TimeUnit::Hours, "h");
        });
    });
    ui.checkbox(&mut cfg.stop_on_first_error, "Stop on first error");
    ui.horizontal(|ui| {
        ui.add_enabled_ui(!cfg.stop_on_first_error, |ui| {
            ui.checkbox(&mut cfg.max_errors_enabled, "Max errors");
            ui.add_enabled(
                cfg.max_errors_enabled,
                egui::DragValue::new(&mut cfg.max_errors).range(1..=u64::MAX),
            );
        });
    });
    if !cfg.passes_enabled
        && !cfg.duration_enabled
        && !cfg.stop_on_first_error
        && !cfg.max_errors_enabled
    {
        ui.label(
            RichText::new("No limit set: runs until stopped.")
                .weak()
                .small(),
        );
    }
    ui.add_space(6.0);
}

fn options_section(ui: &mut Ui, cfg: &mut ConfigState) {
    ui.label(RichText::new("Options").strong());
    ui.checkbox(&mut cfg.large_pages, "Large pages")
        .on_hover_text(
        "Needs the 'Lock pages in memory' privilege (administrator). Falls back to normal pages.",
    );
    ui.checkbox(&mut cfg.lock_memory, "Lock memory")
        .on_hover_text("Keep the test region resident so it is never paged out.");
    ui.checkbox(&mut cfg.pin_threads, "Pin threads to CPUs");
    ui.checkbox(&mut cfg.monitor_whea, "Monitor WHEA events")
        .on_hover_text("Poll the Windows hardware error log during the run.");
    ui.checkbox(&mut cfg.keep_awake, "Keep system awake");
    ui.horizontal(|ui| {
        ui.label("Priority:");
        egui::ComboBox::from_id_salt("priority")
            .selected_text(format!("{:?}", cfg.priority))
            .show_ui(ui, |ui| {
                for p in [
                    ThreadPriority::Normal,
                    ThreadPriority::AboveNormal,
                    ThreadPriority::Highest,
                ] {
                    ui.selectable_value(&mut cfg.priority, p, format!("{p:?}"));
                }
            });
    });
    ui.horizontal(|ui| {
        ui.label("Seed:");
        let bad = crate::config::parse_seed(&cfg.seed_text).is_err();
        let mut te = egui::TextEdit::singleline(&mut cfg.seed_text)
            .hint_text("random")
            .desired_width(150.0);
        if bad {
            te = te.text_color(widgets::RED);
        }
        ui.add(te)
            .on_hover_text("Decimal or 0x-hex. Empty = random seed, recorded in the report.");
    });
}
