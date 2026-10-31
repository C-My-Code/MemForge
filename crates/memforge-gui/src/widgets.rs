//! Small reusable painted widgets and shared colors.

use eframe::egui::{
    Align2, Color32, FontId, Pos2, Rect, RichText, Sense, Stroke, StrokeKind, Ui, Vec2,
};

pub const GREEN: Color32 = Color32::from_rgb(70, 200, 110);
pub const RED: Color32 = Color32::from_rgb(235, 70, 70);
pub const AMBER: Color32 = Color32::from_rgb(240, 180, 60);
pub const BLUE: Color32 = Color32::from_rgb(90, 160, 240);
pub const GREY: Color32 = Color32::from_rgb(140, 140, 150);

/// Small colored pill label.
pub fn badge(ui: &mut Ui, text: &str, color: Color32) {
    eframe::egui::Frame::new()
        .fill(color.gamma_multiply(0.25))
        .stroke(Stroke::new(1.0, color))
        .corner_radius(8.0)
        .inner_margin(eframe::egui::Margin::symmetric(8, 2))
        .show(ui, |ui| {
            ui.label(RichText::new(text).color(color).small().strong());
        });
}

/// Label/value pair in a stat tile.
pub fn stat(ui: &mut Ui, label: &str, value: impl Into<String>, color: Option<Color32>) {
    ui.vertical(|ui| {
        ui.label(RichText::new(label).weak().small());
        let mut t = RichText::new(value.into()).size(20.0).strong();
        if let Some(c) = color {
            t = t.color(c);
        }
        ui.label(t);
    });
}

/// Hand-painted line chart of `values` (auto-scaled from 0).
pub fn sparkline(ui: &mut Ui, values: &[f32], size: Vec2, color: Color32, unit: &str) {
    let (resp, painter) = ui.allocate_painter(size, Sense::hover());
    let rect = resp.rect;
    painter.rect_filled(rect, 4.0, ui.visuals().extreme_bg_color);
    painter.rect_stroke(
        rect,
        4.0,
        Stroke::new(1.0, ui.visuals().widgets.noninteractive.bg_stroke.color),
        StrokeKind::Inside,
    );
    if values.len() < 2 {
        painter.text(
            rect.center(),
            Align2::CENTER_CENTER,
            "collecting...",
            FontId::proportional(11.0),
            GREY,
        );
        return;
    }
    let max = values.iter().copied().fold(0.0f32, f32::max).max(1e-6) * 1.1;
    let inner = rect.shrink(4.0);
    let n = values.len();
    let pt = |i: usize, v: f32| -> Pos2 {
        Pos2::new(
            inner.left() + inner.width() * i as f32 / (n - 1) as f32,
            inner.bottom() - inner.height() * (v / max).clamp(0.0, 1.0),
        )
    };
    let pts: Vec<Pos2> = values.iter().enumerate().map(|(i, v)| pt(i, *v)).collect();
    painter.add(eframe::egui::Shape::line(pts, Stroke::new(1.5, color)));
    painter.text(
        rect.right_top() + Vec2::new(-6.0, 4.0),
        Align2::RIGHT_TOP,
        format!("{:.1} {unit}", values[n - 1]),
        FontId::proportional(11.0),
        ui.visuals().text_color(),
    );
    painter.text(
        rect.left_top() + Vec2::new(6.0, 4.0),
        Align2::LEFT_TOP,
        format!("max {:.1}", max / 1.1),
        FontId::proportional(10.0),
        GREY,
    );
}

/// Per-bit error histogram: 64 bars grouped into 8 byte lanes.
pub fn bit_histogram(ui: &mut Ui, by_bit: &[u64], height: f32) {
    let width = ui.available_width().max(200.0);
    let label_h = 16.0;
    let (resp, painter) = ui.allocate_painter(Vec2::new(width, height + label_h), Sense::hover());
    let rect = resp.rect;
    let plot = Rect::from_min_size(rect.min, Vec2::new(width, height));
    painter.rect_filled(plot, 4.0, ui.visuals().extreme_bg_color);
    let max = by_bit.iter().copied().max().unwrap_or(0);
    let slot = width / 64.0;
    for bit in 0..64usize {
        let lane = bit / 8;
        let x0 = plot.left() + slot * bit as f32;
        let count = by_bit.get(bit).copied().unwrap_or(0);
        let lane_col = if lane % 2 == 0 {
            BLUE
        } else {
            Color32::from_rgb(120, 120, 220)
        };
        let base = Rect::from_min_max(
            Pos2::new(x0 + 1.0, plot.bottom() - 2.0),
            Pos2::new(x0 + slot - 1.0, plot.bottom()),
        );
        painter.rect_filled(base, 0.0, lane_col.gamma_multiply(0.35));
        if count > 0 && max > 0 {
            let h = ((count as f32 / max as f32) * (height - 6.0)).max(3.0);
            let bar = Rect::from_min_max(
                Pos2::new(x0 + 1.0, plot.bottom() - h),
                Pos2::new(x0 + slot - 1.0, plot.bottom()),
            );
            painter.rect_filled(bar, 1.0, RED);
        }
    }
    for lane in 0..8usize {
        let x0 = plot.left() + slot * (lane * 8) as f32;
        if lane > 0 {
            painter.line_segment(
                [Pos2::new(x0, plot.top()), Pos2::new(x0, plot.bottom())],
                Stroke::new(1.0, GREY.gamma_multiply(0.4)),
            );
        }
        painter.text(
            Pos2::new(x0 + slot * 4.0, plot.bottom() + 2.0),
            Align2::CENTER_TOP,
            format!("B{lane} ({}-{})", lane * 8, lane * 8 + 7),
            FontId::proportional(10.0),
            GREY,
        );
    }
    if let Some(p) = resp.hover_pos() {
        if plot.contains(p) {
            let bit = (((p.x - plot.left()) / slot) as usize).min(63);
            let count = by_bit.get(bit).copied().unwrap_or(0);
            resp.on_hover_text_at_pointer(format!("bit {bit} (byte {}): {count} errors", bit / 8));
        }
    }
}
