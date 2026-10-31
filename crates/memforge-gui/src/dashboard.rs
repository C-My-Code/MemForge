//! Live dashboard / results view.

use crate::format;
use crate::session::{Headline, LogKind, Session};
use crate::widgets::{self, AMBER, BLUE, GREEN, GREY, RED};
use eframe::egui::{self, Color32, RichText, Ui, Vec2};
use memforge_engine::Phase;

pub enum DashAction {
    None,
    SaveReport,
    CopySummary,
}

const ERROR_ROWS: usize = 200;

pub fn idle(ui: &mut Ui) {
    ui.vertical_centered(|ui| {
        ui.add_space(80.0);
        ui.label(RichText::new("Ready").size(40.0).strong().color(GREY));
        ui.label("Choose settings on the left and press Start.");
    });
}

fn headline_style(h: Headline) -> (&'static str, Color32) {
    match h {
        Headline::Allocating => ("ALLOCATING", AMBER),
        Headline::Running => ("RUNNING", BLUE),
        Headline::Stopping => ("STOPPING", AMBER),
        Headline::Failing => ("ERRORS DETECTED", RED),
        Headline::Pass => ("PASS", GREEN),
        Headline::Fail => ("FAIL", RED),
        Headline::Incomplete => ("INCOMPLETE", AMBER),
        Headline::SetupError => ("SETUP ERROR", RED),
    }
}

pub fn show(ui: &mut Ui, s: &Session) -> DashAction {
    let mut action = DashAction::None;
    let head = s.headline();
    let (label, color) = headline_style(head);
    let st = s.status.as_ref();
    let finished = s.report.is_some();

    egui::ScrollArea::vertical()
        .auto_shrink([false; 2])
        .show(ui, |ui| {
            // Verdict banner
            egui::Frame::new()
                .fill(color.gamma_multiply(0.18))
                .stroke(egui::Stroke::new(2.0, color))
                .corner_radius(8.0)
                .inner_margin(12.0)
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(label).size(34.0).strong().color(color));
                        ui.add_space(16.0);
                        ui.vertical(|ui| {
                            if let Some(r) = &s.report {
                                ui.label(format!("Stop reason: {}", r.stop_reason));
                            } else if let Some(st) = st {
                                ui.label(match st.phase {
                                    Phase::Allocating => {
                                        "Allocating and preparing memory...".to_string()
                                    }
                                    Phase::Running => st
                                        .current_test
                                        .map(|t| format!("Test: {}", t.title()))
                                        .unwrap_or_default(),
                                    Phase::Stopping => "Stopping workers...".into(),
                                    Phase::Finished => "Finished".into(),
                                });
                            }
                        });
                    });
                });
            ui.add_space(8.0);

            if let Some(st) = st {
                // Key stats
                let errors = st.errors;
                ui.horizontal_wrapped(|ui| {
                    widgets::stat(ui, "Elapsed", format::duration(st.elapsed), None);
                    ui.add_space(20.0);
                    let target = s
                        .config
                        .stop
                        .passes
                        .map(|n| format!(" / {n}"))
                        .unwrap_or_default();
                    widgets::stat(
                        ui,
                        "Passes",
                        format!("{}{target}", st.passes_completed),
                        None,
                    );
                    ui.add_space(20.0);
                    widgets::stat(ui, "Throughput", format::rate(st.throughput), None);
                    ui.add_space(20.0);
                    widgets::stat(
                        ui,
                        "Errors",
                        errors.to_string(),
                        Some(if errors > 0 { RED } else { GREEN }),
                    );
                    ui.add_space(20.0);
                    widgets::stat(
                        ui,
                        "WHEA",
                        st.whea_events.to_string(),
                        Some(if st.whea_events > 0 { RED } else { GREEN }),
                    );
                    ui.add_space(20.0);
                    widgets::stat(
                        ui,
                        "Memory under test",
                        format::bytes(st.bytes_under_test),
                        None,
                    );
                    ui.add_space(20.0);
                    widgets::stat(ui, "Processed", format::bytes(st.bytes_processed), None);
                });
                ui.horizontal(|ui| {
                    widgets::badge(
                        ui,
                        if st.large_pages {
                            "large pages"
                        } else {
                            "normal pages"
                        },
                        if st.large_pages { GREEN } else { GREY },
                    );
                    widgets::badge(
                        ui,
                        if st.locked { "locked" } else { "not locked" },
                        if st.locked { GREEN } else { AMBER },
                    );
                    if let Some(seed) = s.seed {
                        widgets::badge(ui, &format!("seed {seed:#x}"), GREY);
                    }
                });
                ui.add_space(6.0);

                // Progress
                if !finished {
                    let cur = st
                        .current_test
                        .map(|t| {
                            format!(
                                "{} ({}/{})",
                                t.title(),
                                st.current_test_index + 1,
                                s.config.tests.len().max(1)
                            )
                        })
                        .unwrap_or_else(|| "-".into());
                    ui.label(format!("Current test: {cur}"));
                    ui.add(
                        egui::ProgressBar::new(st.pass_progress.clamp(0.0, 1.0) as f32)
                            .show_percentage()
                            .text(format!("Pass progress {:.0}%", st.pass_progress * 100.0)),
                    );
                    if let Some(p) = s.overall_progress() {
                        ui.add_space(2.0);
                        ui.add(
                            egui::ProgressBar::new(p as f32)
                                .text(format!("Overall {:.0}%", p * 100.0)),
                        );
                    }
                    ui.add_space(6.0);
                }

                ui.label(RichText::new("Throughput").strong());
                widgets::sparkline(
                    ui,
                    &s.history,
                    Vec2::new(ui.available_width(), 70.0),
                    BLUE,
                    "GB/s",
                );
                ui.add_space(6.0);

                // Threads
                egui::CollapsingHeader::new(format!("Worker threads ({})", st.threads.len()))
                    .default_open(true)
                    .show(ui, |ui| {
                        ui.horizontal_wrapped(|ui| {
                            ui.spacing_mut().item_spacing = Vec2::splat(3.0);
                            for t in &st.threads {
                                let bad = t.errors > 0;
                                let (rect, resp) =
                                    ui.allocate_exact_size(Vec2::splat(22.0), egui::Sense::hover());
                                let col = if bad {
                                    RED
                                } else if t.current_test.is_some() {
                                    GREEN.gamma_multiply(0.7)
                                } else {
                                    GREY.gamma_multiply(0.5)
                                };
                                ui.painter().rect_filled(rect, 3.0, col);
                                ui.painter().text(
                                    rect.center(),
                                    egui::Align2::CENTER_CENTER,
                                    t.index.to_string(),
                                    egui::FontId::proportional(9.0),
                                    Color32::WHITE,
                                );
                                resp.on_hover_text(format!(
                                    "Thread {}\nCPU: {}\nTest: {}\nProcessed: {}\nErrors: {}",
                                    t.index,
                                    t.cpu
                                        .map(|c| c.to_string())
                                        .unwrap_or_else(|| "unpinned".into()),
                                    t.current_test.map(|x| x.title()).unwrap_or("-"),
                                    format::bytes(t.bytes_processed),
                                    t.errors
                                ));
                            }
                        });
                    });

                // Per test
                egui::CollapsingHeader::new("Per-test results")
                    .default_open(true)
                    .show(ui, |ui| {
                        egui::Grid::new("per_test")
                            .striped(true)
                            .spacing([18.0, 3.0])
                            .show(ui, |ui| {
                                for h in ["Test", "Runs", "Errors", "Time"] {
                                    ui.label(RichText::new(h).strong());
                                }
                                ui.end_row();
                                for t in &st.per_test {
                                    let Some(id) = t.test else { continue };
                                    let active = !finished && st.current_test == Some(id);
                                    let name = if active {
                                        RichText::new(format!("> {}", id.title()))
                                            .strong()
                                            .color(BLUE)
                                    } else {
                                        RichText::new(id.title())
                                    };
                                    ui.label(name).on_hover_text(id.description());
                                    ui.label(t.runs.to_string());
                                    ui.label(RichText::new(t.errors.to_string()).color(
                                        if t.errors > 0 {
                                            RED
                                        } else {
                                            ui.visuals().text_color()
                                        },
                                    ));
                                    ui.label(format::duration(t.time));
                                    ui.end_row();
                                }
                            });
                    });

                // Histogram
                egui::CollapsingHeader::new("Error bit histogram")
                    .default_open(true)
                    .show(ui, |ui| {
                        widgets::bit_histogram(ui, &st.errors_by_bit, 80.0);
                    });
            }

            // Error table
            egui::CollapsingHeader::new(format!("Errors ({} recorded)", s.errors.len()))
                .default_open(true)
                .show(ui, |ui| {
                    if s.errors.is_empty() {
                        ui.label(RichText::new("No errors.").weak());
                        return;
                    }
                    if s.errors.len() > ERROR_ROWS {
                        ui.label(
                            RichText::new(format!("Showing the most recent {ERROR_ROWS}."))
                                .weak()
                                .small(),
                        );
                    }
                    egui::ScrollArea::both()
                        .id_salt("err_scroll")
                        .max_height(240.0)
                        .show(ui, |ui| {
                            egui::Grid::new("err_grid")
                                .striped(true)
                                .spacing([12.0, 2.0])
                                .show(ui, |ui| {
                                    for h in [
                                        "Pass",
                                        "Test",
                                        "Thread",
                                        "Address",
                                        "Expected",
                                        "Actual",
                                        "XOR",
                                        "Flipped bits",
                                    ] {
                                        ui.label(RichText::new(h).strong());
                                    }
                                    ui.end_row();
                                    for e in s.errors.iter().rev().take(ERROR_ROWS) {
                                        ui.label(e.pass.to_string());
                                        ui.label(e.test.title());
                                        ui.label(e.thread.to_string());
                                        ui.monospace(format!("0x{:016X}", e.address));
                                        ui.monospace(format::hex64(e.expected));
                                        ui.monospace(format::hex64(e.actual));
                                        ui.monospace(format::hex64(e.xor_mask()));
                                        ui.label(format!(
                                            "{} ({})",
                                            format::bit_list(e.xor_mask()),
                                            e.flipped_bits()
                                        ));
                                        ui.end_row();
                                    }
                                });
                        });
                });

            // Log
            egui::CollapsingHeader::new(format!("Log ({})", s.log.len()))
                .default_open(true)
                .show(ui, |ui| {
                    egui::ScrollArea::vertical()
                        .id_salt("log_scroll")
                        .max_height(200.0)
                        .stick_to_bottom(true)
                        .show(ui, |ui| {
                            for l in &s.log {
                                let c = match l.kind {
                                    LogKind::Info => ui.visuals().text_color(),
                                    LogKind::Warn => AMBER,
                                    LogKind::Error => RED,
                                    LogKind::Pass => GREEN,
                                    LogKind::Whea => Color32::from_rgb(230, 130, 230),
                                };
                                ui.label(
                                    RichText::new(format!(
                                        "[{}] {}",
                                        format::duration(l.elapsed),
                                        l.text
                                    ))
                                    .monospace()
                                    .color(c),
                                );
                            }
                        });
                });

            // Finished actions
            if finished {
                ui.add_space(8.0);
                ui.separator();
                ui.horizontal(|ui| {
                    if ui.button("Save report (JSON)...").clicked() {
                        action = DashAction::SaveReport;
                    }
                    if ui.button("Copy summary").clicked() {
                        action = DashAction::CopySummary;
                    }
                });
                if let Some(txt) = s.summary_text() {
                    ui.add_space(4.0);
                    ui.label(RichText::new(txt).monospace().small());
                }
            }
        });
    action
}
