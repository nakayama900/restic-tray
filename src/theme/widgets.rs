//! Custom-painted widgets. Each one sets AccessKit widget info, a pointer cursor and a
//! visible keyboard focus ring, because egui does not draw those for painter-based widgets.

use std::ops::RangeInclusive;
use std::sync::Arc;

use egui::text::{LayoutJob, TextWrapping};
use egui::{
    Align, Align2, Color32, CornerRadius, CursorIcon, FontId, Frame, Galley, Margin, Painter, Pos2,
    Rect, Response, RichText, Role, Sense, Shape, Stroke, StrokeKind, TextEdit, TextFormat, Ui,
    Vec2, WidgetInfo,
};

use super::icons::{VectorIcon, arc_points, paint_icon};
use super::{DesignTokens, body, display, mono, strong};

pub fn mix(a: Color32, b: Color32, t: f32) -> Color32 {
    a.lerp_to_gamma(b, t.clamp(0.0, 1.0))
}

fn hover_t(ui: &Ui, response: &Response) -> f32 {
    ui.ctx()
        .animate_bool_with_time(response.id.with("hover"), response.hovered(), 0.12)
}

pub fn paint_focus_ring(ui: &Ui, t: &DesignTokens, response: &Response, rect: Rect, radius: u8) {
    if response.has_focus() {
        ui.painter().rect_stroke(
            rect.expand(3.0),
            CornerRadius::same(radius.saturating_add(3)),
            Stroke::new(2.0_f32, t.accent_text),
            StrokeKind::Outside,
        );
    }
}

// ---------------------------------------------------------------------------------------------
// Buttons
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BtnKind {
    Primary,
    Secondary,
    Ghost,
    Danger,
    DangerGhost,
}

#[must_use = "call `.show(ui, tokens)` to add the button"]
pub struct Btn<'a> {
    kind: BtnKind,
    label: &'a str,
    icon: Option<VectorIcon>,
    large: bool,
    enabled: bool,
    busy: bool,
    min_width: f32,
}

impl<'a> Btn<'a> {
    fn new(kind: BtnKind, label: &'a str) -> Self {
        Self {
            kind,
            label,
            icon: None,
            large: false,
            enabled: true,
            busy: false,
            min_width: 0.0,
        }
    }
    pub fn primary(label: &'a str) -> Self {
        Self::new(BtnKind::Primary, label)
    }
    pub fn secondary(label: &'a str) -> Self {
        Self::new(BtnKind::Secondary, label)
    }
    pub fn ghost(label: &'a str) -> Self {
        Self::new(BtnKind::Ghost, label)
    }
    pub fn danger(label: &'a str) -> Self {
        Self::new(BtnKind::Danger, label)
    }
    pub fn danger_ghost(label: &'a str) -> Self {
        Self::new(BtnKind::DangerGhost, label)
    }
    pub fn icon(mut self, icon: VectorIcon) -> Self {
        self.icon = Some(icon);
        self
    }
    pub fn large(mut self) -> Self {
        self.large = true;
        self
    }
    pub fn enabled(mut self, enabled: bool) -> Self {
        self.enabled = enabled;
        self
    }
    /// Keeps stacked buttons the same width.
    pub fn min_width(mut self, width: f32) -> Self {
        self.min_width = width;
        self
    }
    /// Shows a spinner in place of the icon and blocks clicks while work is in flight.
    pub fn busy(mut self, busy: bool) -> Self {
        self.busy = busy;
        self
    }

    pub fn show(self, ui: &mut Ui, t: &DesignTokens) -> Response {
        let height = if self.large { 46.0 } else { 34.0 };
        let pad = if self.large { 22.0 } else { 14.0 };
        let font_size = if self.large { 15.0 } else { 14.0 };
        let font = match self.kind {
            BtnKind::Primary | BtnKind::Danger => strong(font_size),
            _ => body(font_size),
        };
        let galley = ui
            .painter()
            .layout_no_wrap(self.label.to_owned(), font, Color32::PLACEHOLDER);
        let has_icon = self.icon.is_some() || self.busy;
        let icon_size = if self.large { 18.0 } else { 16.0 };
        let icon_w = if has_icon { icon_size + 8.0 } else { 0.0 };
        let size = Vec2::new(
            (pad * 2.0 + icon_w + galley.size().x).max(self.min_width),
            height,
        );

        let clickable = self.enabled && !self.busy;
        let sense = if clickable {
            Sense::click()
        } else {
            Sense::hover()
        };
        let (rect, response) = ui.allocate_exact_size(size, sense);
        response.widget_info(|| WidgetInfo::labeled(Role::Button, clickable, self.label));

        if ui.is_rect_visible(rect) {
            let h = if clickable {
                hover_t(ui, &response)
            } else {
                0.0
            };
            let pressed = clickable && response.is_pointer_button_down_on();
            let lift = if t.is_dark { Color32::WHITE } else { t.text_3 };
            let (fill, stroke, fg) = match self.kind {
                BtnKind::Primary => (
                    mix(
                        t.primary_fill,
                        lift,
                        0.14 * h - if pressed { 0.1 } else { 0.0 },
                    ),
                    Stroke::NONE,
                    t.on_primary,
                ),
                BtnKind::Secondary => (
                    mix(t.surface, t.surface_hover, h),
                    Stroke::new(1.0_f32, mix(t.line_strong, t.text_3, h)),
                    t.text,
                ),
                BtnKind::Ghost => (
                    t.surface_hover.gamma_multiply(h),
                    Stroke::NONE,
                    mix(t.text_2, t.text, h),
                ),
                BtnKind::Danger => (mix(t.err, lift, 0.12 * h), Stroke::NONE, t.on_err),
                BtnKind::DangerGhost => (t.err.gamma_multiply(0.12 * h), Stroke::NONE, t.err),
            };
            let opacity = if self.enabled { 1.0 } else { 0.4 };
            let radius = 8;
            let painter = ui.painter();
            painter.rect(
                rect,
                CornerRadius::same(radius),
                fill.gamma_multiply(opacity),
                Stroke::new(stroke.width, stroke.color.gamma_multiply(opacity)),
                StrokeKind::Inside,
            );
            let fg = fg.gamma_multiply(opacity);
            let content_w = icon_w + galley.size().x;
            let mut x = rect.center().x - content_w / 2.0;
            if has_icon {
                let icon_rect = Rect::from_center_size(
                    Pos2::new(x + icon_size / 2.0, rect.center().y),
                    Vec2::splat(icon_size),
                );
                if self.busy {
                    paint_spinner(ui, icon_rect.center(), icon_size * 0.4, fg);
                } else if let Some(icon) = self.icon {
                    paint_icon(painter, icon_rect, icon, fg);
                }
                x += icon_w;
            }
            painter.galley(
                Pos2::new(x, rect.center().y - galley.size().y / 2.0),
                galley,
                fg,
            );
            paint_focus_ring(ui, t, &response, rect, radius);
        }
        response
    }
}

/// Destructive confirmation button that must be pressed and held for `hold_secs`
/// (mouse press or keyboard Space/Enter while focused) before returning `true`.
/// Sweeps a solid danger fill from left to right with clipped dual-color text inversion
/// so accidental clicks cannot trigger irreversible deletion.
pub fn hold_danger_button(
    ui: &mut Ui,
    t: &DesignTokens,
    idle_label: &str,
    hold_secs: f32,
    icon: VectorIcon,
    enabled: bool,
    hold_started: &mut Option<f64>,
) -> bool {
    let height = 36.0;
    let pad = 16.0;
    let icon_size = 16.0;
    let icon_w = icon_size + 8.0;
    let font = strong(14.0);

    let idle_w = ui
        .painter()
        .layout_no_wrap(idle_label.to_owned(), font.clone(), Color32::PLACEHOLDER)
        .size()
        .x;
    let holding_sample_w = ui
        .painter()
        .layout_no_wrap(
            "押し続け… 0.0秒".to_owned(),
            font.clone(),
            Color32::PLACEHOLDER,
        )
        .size()
        .x;
    let size = Vec2::new(pad * 2.0 + icon_w + idle_w.max(holding_sample_w), height);

    let sense = if enabled {
        Sense::click_and_drag()
    } else {
        Sense::hover()
    };
    let (rect, response) = ui.allocate_exact_size(size, sense);
    let response = if enabled {
        response.on_hover_cursor(CursorIcon::PointingHand)
    } else {
        response
    };
    response.widget_info(|| WidgetInfo::labeled(Role::Button, enabled, idle_label));

    let pointer_holding = enabled
        && response.is_pointer_button_down_on()
        && ui.input(|i| {
            i.pointer
                .interact_pos()
                .is_some_and(|p| rect.expand(6.0).contains(p))
        });
    let key_holding = enabled
        && response.has_focus()
        && ui.input(|i| i.key_down(egui::Key::Space) || i.key_down(egui::Key::Enter));
    let holding = pointer_holding || key_holding;

    let now = ui.input(|i| i.time);
    let duration = hold_secs.max(0.1);
    let (progress, remaining, triggered) = if holding {
        let start = *hold_started.get_or_insert(now);
        let elapsed = (now - start).max(0.0) as f32;
        let p = (elapsed / duration).clamp(0.0, 1.0);
        let rem = (duration - elapsed).max(0.0);
        ui.ctx().request_repaint();
        if p >= 1.0 {
            *hold_started = None;
            (1.0, 0.0, true)
        } else {
            (p, rem, false)
        }
    } else {
        *hold_started = None;
        (0.0, duration, false)
    };

    if ui.is_rect_visible(rect) {
        let h = if enabled { hover_t(ui, &response) } else { 0.0 };
        let opacity = if enabled { 1.0 } else { 0.4 };
        let radius = 8;
        let painter = ui.painter();

        let base_fill = t.err.gamma_multiply((0.12 + 0.08 * h) * opacity);
        let border = Stroke::new(1.5, t.err.gamma_multiply(0.85 * opacity));
        painter.rect(
            rect,
            CornerRadius::same(radius),
            base_fill,
            border,
            StrokeKind::Inside,
        );

        let text = if holding {
            format!("押し続け… {remaining:.1}秒")
        } else {
            idle_label.to_owned()
        };
        let galley = painter.layout_no_wrap(text, font, Color32::PLACEHOLDER);
        let content_w = icon_w + galley.size().x;
        let start_x = rect.center().x - content_w / 2.0;
        let icon_rect = Rect::from_center_size(
            Pos2::new(start_x + icon_size / 2.0, rect.center().y),
            Vec2::splat(icon_size),
        );
        let text_pos = Pos2::new(start_x + icon_w, rect.center().y - galley.size().y / 2.0);

        if progress > 0.0 {
            let split_x = egui::lerp(rect.left()..=rect.right(), progress);
            let filled = Rect::from_min_max(rect.min, Pos2::new(split_x, rect.max.y));
            let unfilled = Rect::from_min_max(Pos2::new(split_x, rect.min.y), rect.max);

            let filled_painter = painter.with_clip_rect(filled);
            filled_painter.rect_filled(rect, CornerRadius::same(radius), t.err);
            paint_icon(&filled_painter, icon_rect, icon, t.on_err);
            filled_painter.galley(text_pos, Arc::clone(&galley), t.on_err);

            let unfilled_painter = painter.with_clip_rect(unfilled);
            paint_icon(&unfilled_painter, icon_rect, icon, t.err);
            unfilled_painter.galley(text_pos, galley, t.err);
        } else {
            let fg = t.err.gamma_multiply(opacity);
            paint_icon(painter, icon_rect, icon, fg);
            painter.galley(text_pos, galley, fg);
        }

        paint_focus_ring(ui, t, &response, rect, radius);
    }

    triggered
}

/// Square ghost button with an icon. `label` becomes the tooltip and the accessible name.
pub fn icon_button(ui: &mut Ui, t: &DesignTokens, icon: VectorIcon, label: &str) -> Response {
    icon_button_sized(ui, t, icon, label, 32.0)
}

pub fn icon_button_sized(
    ui: &mut Ui,
    t: &DesignTokens,
    icon: VectorIcon,
    label: &str,
    size: f32,
) -> Response {
    let (rect, response) = ui.allocate_exact_size(Vec2::splat(size), Sense::click());
    response.widget_info(|| WidgetInfo::labeled(Role::Button, true, label));
    if ui.is_rect_visible(rect) {
        let h = hover_t(ui, &response);
        ui.painter().rect_filled(
            rect,
            CornerRadius::same(8),
            t.surface_hover.gamma_multiply(h),
        );
        let icon_rect = Rect::from_center_size(rect.center(), Vec2::splat(size * 0.55));
        paint_icon(ui.painter(), icon_rect, icon, mix(t.text_2, t.text, h));
        paint_focus_ring(ui, t, &response, rect, 8);
    }
    response.on_hover_text(label)
}

/// Flush-left chevron toggle for optional sections ("詳細設定").
pub fn disclosure(ui: &mut Ui, t: &DesignTokens, label: &str, open: &mut bool) -> Response {
    let galley = ui
        .painter()
        .layout_no_wrap(label.to_owned(), body(13.5), Color32::PLACEHOLDER);
    let size = Vec2::new(galley.size().x + 22.0, 28.0);
    let (rect, mut response) = ui.allocate_exact_size(size, Sense::click());
    if response.clicked() {
        *open = !*open;
        response.mark_changed();
    }
    response.widget_info(|| WidgetInfo::selected(Role::DisclosureTriangle, true, *open, label));
    if ui.is_rect_visible(rect) {
        let h = hover_t(ui, &response);
        let color = mix(t.text_2, t.text, h);
        let icon = if *open {
            VectorIcon::ChevronDown
        } else {
            VectorIcon::ChevronRight
        };
        paint_icon(
            ui.painter(),
            Rect::from_center_size(
                Pos2::new(rect.left() + 7.0, rect.center().y),
                Vec2::splat(14.0),
            ),
            icon,
            color,
        );
        ui.painter().galley(
            Pos2::new(rect.left() + 22.0, rect.center().y - galley.size().y / 2.0),
            galley,
            color,
        );
        paint_focus_ring(ui, t, &response, rect, 4);
    }
    response
}

/// Text-only link-style button, e.g. "すべて表示".
pub fn link_button(ui: &mut Ui, t: &DesignTokens, label: &str) -> Response {
    let galley = ui
        .painter()
        .layout_no_wrap(label.to_owned(), body(13.0), Color32::PLACEHOLDER);
    let size = galley.size() + Vec2::new(22.0, 8.0);
    let (rect, response) = ui.allocate_exact_size(size, Sense::click());
    response.widget_info(|| WidgetInfo::labeled(Role::Link, true, label));
    if ui.is_rect_visible(rect) {
        let h = hover_t(ui, &response);
        let color = mix(t.text_2, t.text, h);
        ui.painter().galley(
            Pos2::new(rect.left(), rect.center().y - galley.size().y / 2.0),
            galley,
            color,
        );
        let arrow = Rect::from_center_size(
            Pos2::new(rect.right() - 7.0 + 2.0 * h, rect.center().y),
            Vec2::splat(13.0),
        );
        paint_icon(ui.painter(), arrow, VectorIcon::ArrowRight, color);
        paint_focus_ring(ui, t, &response, rect, 4);
    }
    response
}

// ---------------------------------------------------------------------------------------------
// Switches, segmented control, steppers
// ---------------------------------------------------------------------------------------------

fn paint_switch(ui: &Ui, t: &DesignTokens, rect: Rect, k: f32, hover: f32) {
    let off_track = mix(t.line_strong, t.text_3, hover * 0.5);
    let track = mix(off_track, t.primary_fill, k);
    ui.painter()
        .rect_filled(rect, CornerRadius::same((rect.height() / 2.0) as u8), track);
    let r = rect.height() / 2.0 - 3.0;
    let x = egui::lerp(rect.left() + 3.0 + r..=rect.right() - 3.0 - r, k);
    let knob_off = if t.is_dark { t.text } else { Color32::WHITE };
    ui.painter().circle_filled(
        Pos2::new(x, rect.center().y),
        r,
        mix(knob_off, t.on_primary, k),
    );
}

/// Bare switch for places where the label is drawn elsewhere (list rows).
pub fn switch(ui: &mut Ui, t: &DesignTokens, on: &mut bool, label: &str) -> Response {
    let (rect, mut response) = ui.allocate_exact_size(Vec2::new(40.0, 22.0), Sense::click());
    if response.clicked() {
        *on = !*on;
        response.mark_changed();
    }
    response.widget_info(|| WidgetInfo::selected(Role::Switch, true, *on, label));
    if ui.is_rect_visible(rect) {
        let h = hover_t(ui, &response);
        let k = ui
            .ctx()
            .animate_bool_with_time(response.id.with("on"), *on, 0.16);
        paint_switch(ui, t, rect, k, h);
        paint_focus_ring(ui, t, &response, rect, 11);
    }
    response
}

/// Settings row: title and description on the left, a switch on the right.
/// The whole row toggles, so the hit target is never smaller than the text.
pub fn toggle_row(
    ui: &mut Ui,
    t: &DesignTokens,
    on: &mut bool,
    title: &str,
    desc: Option<&str>,
) -> Response {
    let width = ui.available_width();
    let text_w = (width - 64.0).max(80.0);
    let title_g = ui
        .painter()
        .layout(title.to_owned(), body(14.0), t.text, text_w);
    let desc_g = desc.map(|d| {
        ui.painter()
            .layout(d.to_owned(), body(12.5), t.text_3, text_w)
    });
    let text_h = title_g.size().y + desc_g.as_ref().map_or(0.0, |g| g.size().y + 3.0);
    let height = (text_h + 20.0).max(48.0);
    let (rect, mut response) = ui.allocate_exact_size(Vec2::new(width, height), Sense::click());
    if response.clicked() {
        *on = !*on;
        response.mark_changed();
    }
    response.widget_info(|| WidgetInfo::selected(Role::Switch, true, *on, title));
    if ui.is_rect_visible(rect) {
        let h = hover_t(ui, &response);
        let k = ui
            .ctx()
            .animate_bool_with_time(response.id.with("on"), *on, 0.16);
        let mut y = rect.center().y - text_h / 2.0;
        let title_h = title_g.size().y;
        ui.painter()
            .galley(Pos2::new(rect.left(), y), title_g, t.text);
        y += title_h + 3.0;
        if let Some(g) = desc_g {
            ui.painter().galley(Pos2::new(rect.left(), y), g, t.text_3);
        }
        let sw = Rect::from_center_size(
            Pos2::new(rect.right() - 20.0, rect.center().y),
            Vec2::new(40.0, 22.0),
        );
        paint_switch(ui, t, sw, k, h);
        paint_focus_ring(ui, t, &response, sw, 11);
    }
    response
}

/// Pill-shaped segmented control with a sliding highlight.
pub fn segmented<T: PartialEq + Copy>(
    ui: &mut Ui,
    t: &DesignTokens,
    value: &mut T,
    options: &[(T, &str)],
) -> Response {
    let pad = 3.0;
    let seg_h = 30.0;
    let galleys: Vec<Arc<Galley>> = options
        .iter()
        .map(|(_, label)| {
            ui.painter()
                .layout_no_wrap((*label).to_owned(), body(13.5), Color32::PLACEHOLDER)
        })
        .collect();
    let widths: Vec<f32> = galleys.iter().map(|g| g.size().x + 28.0).collect();
    let total = widths.iter().sum::<f32>() + pad * 2.0;
    let (rect, mut response) =
        ui.allocate_exact_size(Vec2::new(total, seg_h + pad * 2.0), Sense::hover());

    let mut seg_rects = Vec::with_capacity(options.len());
    let mut x = rect.left() + pad;
    for w in &widths {
        seg_rects.push(Rect::from_min_size(
            Pos2::new(x, rect.top() + pad),
            Vec2::new(*w, seg_h),
        ));
        x += w;
    }

    // A value outside the options (e.g. a custom interval) highlights nothing.
    let selected = options.iter().position(|(v, _)| v == value);

    let painter = ui.painter();
    painter.rect(
        rect,
        CornerRadius::same(10),
        t.surface,
        Stroke::new(1.0_f32, t.line),
        StrokeKind::Inside,
    );
    if let Some(selected) = selected {
        let anim = ui
            .ctx()
            .animate_value_with_time(response.id.with("sel"), selected as f32, 0.18);
        let lo = (anim.floor() as usize).min(seg_rects.len() - 1);
        let hi = (anim.ceil() as usize).min(seg_rects.len() - 1);
        let f = anim - anim.floor();
        let hl = Rect::from_min_max(
            seg_rects[lo].min.lerp(seg_rects[hi].min, f),
            seg_rects[lo].max.lerp(seg_rects[hi].max, f),
        );
        let hl_fill = if t.is_dark {
            t.surface_hover
        } else {
            Color32::WHITE
        };
        painter.rect(
            hl,
            CornerRadius::same(8),
            hl_fill,
            Stroke::new(1.0_f32, t.line_strong),
            StrokeKind::Inside,
        );
    }

    for (i, ((opt, label), galley)) in options.iter().zip(galleys).enumerate() {
        let seg = seg_rects[i];
        let r = ui.interact(seg, response.id.with(i), Sense::click());
        let is_sel = Some(i) == selected;
        r.widget_info(|| WidgetInfo::selected(Role::RadioButton, true, is_sel, *label));
        if r.clicked() && !is_sel {
            *value = *opt;
            response.mark_changed();
        }
        let h = hover_t(ui, &r);
        let color = if is_sel {
            t.text
        } else {
            mix(t.text_2, t.text, h)
        };
        ui.painter()
            .galley(seg.center() - galley.size() / 2.0, galley, color);
        paint_focus_ring(ui, t, &r, seg, 8);
    }
    response
}

/// `[−] value [+]` stepper. The value itself is a DragValue, so it can also be typed.
pub fn stepper(
    ui: &mut Ui,
    t: &DesignTokens,
    value: &mut u32,
    range: RangeInclusive<u32>,
    suffix: &str,
) -> Response {
    let (lo, hi) = (*range.start(), *range.end());
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 2.0;
        if icon_button_sized(ui, t, VectorIcon::Minus, "減らす", 30.0).clicked() && *value > lo {
            *value -= 1;
        }
        let r = ui.add(
            egui::DragValue::new(value)
                .range(range)
                .suffix(suffix)
                .speed(0.2),
        );
        if icon_button_sized(ui, t, VectorIcon::Plus, "増やす", 30.0).clicked() && *value < hi {
            *value += 1;
        }
        r
    })
    .inner
}

// ---------------------------------------------------------------------------------------------
// Navigation, chips, badges
// ---------------------------------------------------------------------------------------------

pub enum NavTrailing<'a> {
    None,
    Text(&'a str),
    Dot(Color32),
}

pub fn nav_item(
    ui: &mut Ui,
    t: &DesignTokens,
    icon: VectorIcon,
    label: &str,
    active: bool,
    trailing: NavTrailing<'_>,
) -> Response {
    let (rect, response) =
        ui.allocate_exact_size(Vec2::new(ui.available_width(), 38.0), Sense::click());
    response.widget_info(|| WidgetInfo::selected(Role::Button, true, active, label));
    if ui.is_rect_visible(rect) {
        let h = hover_t(ui, &response);
        let a = ui
            .ctx()
            .animate_bool_with_time(response.id.with("active"), active, 0.14);
        let fill = mix(t.surface.gamma_multiply(h), t.surface_hover, a);
        let painter = ui.painter();
        painter.rect_filled(rect, CornerRadius::same(8), fill);
        let icon_rect = Rect::from_center_size(
            Pos2::new(rect.left() + 20.0, rect.center().y),
            Vec2::splat(18.0),
        );
        let fg = mix(mix(t.text_2, t.text, h), t.text, a);
        paint_icon(painter, icon_rect, icon, mix(fg, t.accent_text, a));
        let font = if active { strong(14.0) } else { body(14.0) };
        painter.text(
            Pos2::new(rect.left() + 40.0, rect.center().y),
            Align2::LEFT_CENTER,
            label,
            font,
            fg,
        );
        match trailing {
            NavTrailing::None => {}
            NavTrailing::Text(text) => {
                painter.text(
                    Pos2::new(rect.right() - 12.0, rect.center().y),
                    Align2::RIGHT_CENTER,
                    text,
                    display(13.0),
                    t.text_3,
                );
            }
            NavTrailing::Dot(color) => {
                painter.circle_filled(Pos2::new(rect.right() - 16.0, rect.center().y), 3.5, color);
            }
        }
        paint_focus_ring(ui, t, &response, rect, 8);
    }
    response
}

/// Returns `true` when the chip's remove button was clicked.
pub fn chip(ui: &mut Ui, t: &DesignTokens, text: &str, monospace: bool, removable: bool) -> bool {
    let font = if monospace { mono(12.5) } else { body(12.5) };
    let galley = ui
        .painter()
        .layout_no_wrap(text.to_owned(), font, Color32::PLACEHOLDER);
    let x_w = if removable { 20.0 } else { 0.0 };
    let size = Vec2::new(galley.size().x + 20.0 + x_w, 26.0);
    let (rect, _) = ui.allocate_exact_size(size, Sense::hover());
    ui.painter().rect(
        rect,
        CornerRadius::same(13),
        t.surface,
        Stroke::new(1.0_f32, t.line_strong),
        StrokeKind::Inside,
    );
    ui.painter().galley(
        Pos2::new(rect.left() + 10.0, rect.center().y - galley.size().y / 2.0),
        galley,
        t.text_2,
    );
    if !removable {
        return false;
    }
    let x_rect = Rect::from_center_size(
        Pos2::new(rect.right() - 14.0, rect.center().y),
        Vec2::splat(18.0),
    );
    let r = ui.interact(x_rect, ui.scope_id().with(("chip-x", text)), Sense::click());
    r.widget_info(|| WidgetInfo::labeled(Role::Button, true, format!("{text} を外す")));
    let h = hover_t(ui, &r);
    ui.painter()
        .circle_filled(x_rect.center(), 8.0, t.surface_hover.gamma_multiply(h));
    paint_icon(
        ui.painter(),
        x_rect.shrink(4.0),
        VectorIcon::Close,
        mix(t.text_3, t.text, h),
    );
    paint_focus_ring(ui, t, &r, x_rect, 9);
    r.on_hover_cursor(CursorIcon::PointingHand).clicked()
}

pub fn badge(ui: &mut Ui, text: &str, fg: Color32) -> Response {
    let galley = ui
        .painter()
        .layout_no_wrap(text.to_owned(), strong(11.5), fg);
    let size = galley.size() + Vec2::new(14.0, 6.0);
    let (rect, response) = ui.allocate_exact_size(size, Sense::hover());
    ui.painter()
        .rect_filled(rect, CornerRadius::same(5), fg.gamma_multiply(0.14));
    ui.painter()
        .galley(rect.center() - galley.size() / 2.0, galley, fg);
    response
}

// ---------------------------------------------------------------------------------------------
// Text helpers
// ---------------------------------------------------------------------------------------------

/// Preconfigured single-line input. Chain `.desired_width()`, `.password()` etc.
pub fn input<'a>(t: &DesignTokens, value: &'a mut String, hint: &str) -> TextEdit<'a> {
    TextEdit::singleline(value)
        .hint_text(RichText::new(hint).color(t.text_3))
        .font(body(14.0))
        .margin(Margin::symmetric(11, 9))
        .align(Align2::LEFT_CENTER)
}

pub fn field_label(ui: &mut Ui, t: &DesignTokens, text: &str) {
    ui.label(RichText::new(text).font(strong(12.5)).color(t.text_2));
}

pub fn help_text(ui: &mut Ui, t: &DesignTokens, text: &str) {
    ui.label(RichText::new(text).font(body(12.5)).color(t.text_3));
}

/// One-line galley ending in "…" when wider than `max_width`.
pub fn elided(ui: &Ui, text: &str, font: FontId, color: Color32, max_width: f32) -> Arc<Galley> {
    let mut job = LayoutJob::single_section(text.to_owned(), TextFormat::simple(font, color));
    job.wrap = TextWrapping {
        max_width: max_width.max(1.0),
        max_rows: 1,
        break_anywhere: true,
        overflow_character: Some('…'),
    };
    ui.fonts_mut(|f| f.layout_job(job))
}

/// Elides from the start so the end of a path (the folder name) stays visible.
pub fn elided_start(
    ui: &Ui,
    text: &str,
    font: FontId,
    color: Color32,
    max_width: f32,
) -> Arc<Galley> {
    let full = ui
        .painter()
        .layout_no_wrap(text.to_owned(), font.clone(), color);
    if full.size().x <= max_width {
        return full;
    }
    let chars: Vec<char> = text.chars().collect();
    let (mut lo, mut hi) = (0, chars.len());
    while lo < hi {
        let mid = (lo + hi) / 2;
        let candidate: String = std::iter::once('…')
            .chain(chars[mid..].iter().copied())
            .collect();
        let w = ui
            .painter()
            .layout_no_wrap(candidate, font.clone(), color)
            .size()
            .x;
        if w <= max_width {
            hi = mid;
        } else {
            lo = mid + 1;
        }
    }
    let text: String = std::iter::once('…')
        .chain(chars[lo..].iter().copied())
        .collect();
    ui.painter().layout_no_wrap(text, font, color)
}

fn baseline(galley: &Galley) -> f32 {
    galley
        .rows
        .first()
        .and_then(|row| row.glyphs.first())
        .map_or(galley.size().y * 0.8, |g| g.pos.y)
}

/// A large DIN figure followed by a small unit that sits on the same baseline ("12 分前").
pub fn figure(ui: &mut Ui, t: &DesignTokens, value: &str, unit: &str, size: f32) -> Response {
    let fig = ui
        .painter()
        .layout_no_wrap(value.to_owned(), display(size), t.text);
    let unit_g =
        ui.painter()
            .layout_no_wrap(unit.to_owned(), body((size * 0.3).max(13.0)), t.text_2);
    let gap = if unit.is_empty() { 0.0 } else { size * 0.08 };
    // Digits have no descenders, so the box ends just under the baseline.
    let size_v = Vec2::new(
        fig.size().x + gap + unit_g.size().x,
        baseline(&fig) + size * 0.06,
    );
    let (rect, response) = ui.allocate_exact_size(size_v, Sense::hover());
    let base = rect.top() + baseline(&fig);
    let unit_pos = Pos2::new(rect.left() + fig.size().x + gap, base - baseline(&unit_g));
    ui.painter().galley(rect.min, fig, t.text);
    ui.painter().galley(unit_pos, unit_g, t.text_2);
    response
}

// ---------------------------------------------------------------------------------------------
// Layout pieces
// ---------------------------------------------------------------------------------------------

pub fn hairline(ui: &mut Ui, t: &DesignTokens) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 1.0), Sense::hover());
    ui.painter().hline(
        rect.x_range(),
        rect.center().y,
        Stroke::new(1.0_f32, t.line),
    );
}

pub fn panel_frame(t: &DesignTokens) -> Frame {
    Frame::new()
        .fill(t.surface)
        .stroke(Stroke::new(1.0_f32, t.line))
        .corner_radius(CornerRadius::same(12))
        .inner_margin(Margin::same(20))
}

/// Page title with an optional subtitle. `actions` is laid out right to left in the
/// top-right corner, so the first widget added ends up on the far right.
pub fn page_header(
    ui: &mut Ui,
    t: &DesignTokens,
    title: &str,
    subtitle: &str,
    actions: impl FnOnce(&mut Ui),
) {
    let area = ui.available_rect_before_wrap();
    let slot = Rect::from_min_max(
        Pos2::new(area.right() - 440.0, area.top()),
        Pos2::new(area.right(), area.top() + 40.0),
    );
    // A detached child: placing the actions must not move the parent's cursor.
    let mut slot_ui = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(slot)
            .layout(egui::Layout::right_to_left(Align::Center)),
    );
    actions(&mut slot_ui);
    ui.scope(|ui| {
        ui.set_max_width((area.width() - 320.0).max(240.0));
        ui.label(RichText::new(title).font(strong(28.0)).color(t.text));
        if !subtitle.is_empty() {
            ui.add_space(-2.0);
            ui.label(RichText::new(subtitle).font(body(14.0)).color(t.text_2));
        }
    });
}

pub fn section_title(ui: &mut Ui, t: &DesignTokens, title: &str) {
    ui.label(RichText::new(title).font(strong(16.0)).color(t.text));
}

pub fn empty_state(ui: &mut Ui, t: &DesignTokens, icon: VectorIcon, title: &str, text: &str) {
    ui.vertical_centered(|ui| {
        ui.add_space(28.0);
        let (rect, _) = ui.allocate_exact_size(Vec2::splat(64.0), Sense::hover());
        ui.painter().rect(
            rect,
            CornerRadius::same(18),
            t.surface,
            Stroke::new(1.0_f32, t.line),
            StrokeKind::Inside,
        );
        paint_icon(ui.painter(), rect.shrink(18.0), icon, t.text_2);
        ui.add_space(14.0);
        ui.label(RichText::new(title).font(strong(17.0)).color(t.text));
        ui.add_space(2.0);
        ui.label(RichText::new(text).font(body(13.5)).color(t.text_2));
        ui.add_space(14.0);
    });
}

// ---------------------------------------------------------------------------------------------
// Status and progress
// ---------------------------------------------------------------------------------------------

/// Filled dot; with `pulse` it emits an expanding halo to signal live work.
pub fn paint_status_dot(ui: &Ui, center: Pos2, color: Color32, pulse: bool) {
    if pulse {
        let time = ui.input(|i| i.time);
        let phase = (time % 1.6 / 1.6) as f32;
        ui.painter().circle_filled(
            center,
            4.0 + 7.0 * phase,
            color.gamma_multiply(0.45 * (1.0 - phase)),
        );
        ui.ctx().request_repaint();
    }
    ui.painter().circle_filled(center, 4.0, color);
}

pub fn paint_spinner(ui: &Ui, center: Pos2, radius: f32, color: Color32) {
    let time = ui.input(|i| i.time) as f32;
    let start = time * 300.0;
    let pts = arc_points(center, radius, start, start + 270.0);
    ui.painter().add(Shape::line(
        pts,
        Stroke::new((radius * 0.32).max(1.6), color),
    ));
    ui.ctx().request_repaint();
}

/// Thin bar. `None` renders an indeterminate sweep while the total is still unknown.
pub fn progress_bar(ui: &mut Ui, t: &DesignTokens, fraction: Option<f32>, height: f32) -> Response {
    let (rect, response) =
        ui.allocate_exact_size(Vec2::new(ui.available_width(), height), Sense::hover());
    let radius = CornerRadius::same((height / 2.0) as u8);
    let painter = ui.painter();
    painter.rect_filled(rect, radius, t.line);
    match fraction {
        Some(f) => {
            let f =
                ui.ctx()
                    .animate_value_with_time(response.id.with("v"), f.clamp(0.0, 1.0), 0.35);
            if f > 0.0 {
                let fill = Rect::from_min_size(rect.min, Vec2::new(rect.width() * f, height));
                painter.rect_filled(fill, radius, t.accent_text);
            }
        }
        None => {
            let time = ui.input(|i| i.time) as f32;
            let w = rect.width() * 0.28;
            let x = rect.left() + ((time * 0.7) % 1.3 - 0.28) * rect.width();
            let seg = Rect::from_min_max(
                Pos2::new(x.max(rect.left()), rect.top()),
                Pos2::new((x + w).min(rect.right()), rect.bottom()),
            );
            if seg.width() > 0.0 {
                painter.rect_filled(seg, radius, t.accent_text);
            }
            ui.ctx().request_repaint();
        }
    }
    response
}

/// Background for a hovered list row.
pub fn paint_row_hover(painter: &Painter, t: &DesignTokens, rect: Rect, h: f32, selected: bool) {
    let fill = if selected {
        t.surface_hover
    } else {
        t.surface.gamma_multiply(h)
    };
    painter.rect_filled(rect, CornerRadius::same(8), fill);
}

pub fn row_hover_t(ui: &Ui, response: &Response) -> f32 {
    hover_t(ui, response)
}
