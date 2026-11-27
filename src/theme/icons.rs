//! Line icons drawn with `egui::Painter` on a 24-unit grid, plus the brand mark.

use egui::{Color32, CornerRadius, Painter, Pos2, Rect, Shape, Stroke, StrokeKind, Vec2};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VectorIcon {
    Overview,
    Folder,
    Snapshots,
    History,
    Settings,
    Plus,
    Trash,
    Refresh,
    Sun,
    Moon,
    Terminal,
    Check,
    Close,
    ChevronRight,
    ChevronDown,
    ArrowRight,
    Copy,
    Eye,
    EyeOff,
    Play,
    Stop,
    Upload,
    Download,
    Alert,
    File,
    Compare,
    HideToTray,
    Power,
    Key,
    Edit,
    Minus,
    ShieldCheck,
}

/// Points along a circular arc. Angles are degrees in screen space (0 = right, 90 = down).
pub(crate) fn arc_points(center: Pos2, radius: f32, from_deg: f32, to_deg: f32) -> Vec<Pos2> {
    let steps = (((to_deg - from_deg).abs() / 8.0).ceil() as usize).max(2);
    (0..=steps)
        .map(|i| {
            let a = (from_deg + (to_deg - from_deg) * i as f32 / steps as f32).to_radians();
            center + Vec2::new(a.cos(), a.sin()) * radius
        })
        .collect()
}

struct Pen<'a> {
    painter: &'a Painter,
    origin: Pos2,
    unit: f32,
    stroke: Stroke,
    color: Color32,
}

impl Pen<'_> {
    fn p(&self, x: f32, y: f32) -> Pos2 {
        self.origin + Vec2::new(x, y) * self.unit
    }

    fn line(&self, pts: &[(f32, f32)]) {
        let pts = pts.iter().map(|&(x, y)| self.p(x, y)).collect();
        self.painter.add(Shape::line(pts, self.stroke));
    }

    fn closed(&self, pts: &[(f32, f32)]) {
        let pts = pts.iter().map(|&(x, y)| self.p(x, y)).collect();
        self.painter.add(Shape::closed_line(pts, self.stroke));
    }

    fn circle(&self, x: f32, y: f32, r: f32) {
        self.painter
            .circle_stroke(self.p(x, y), r * self.unit, self.stroke);
    }

    fn dot(&self, x: f32, y: f32, r: f32) {
        self.painter
            .circle_filled(self.p(x, y), r * self.unit, self.color);
    }

    fn arc(&self, x: f32, y: f32, r: f32, from_deg: f32, to_deg: f32) -> Vec<Pos2> {
        let pts = arc_points(self.p(x, y), r * self.unit, from_deg, to_deg);
        self.painter.add(Shape::line(pts.clone(), self.stroke));
        pts
    }

    fn rect(&self, x: f32, y: f32, w: f32, h: f32, r: f32) {
        let rect = Rect::from_min_size(self.p(x, y), Vec2::new(w, h) * self.unit);
        self.painter.rect_stroke(
            rect,
            CornerRadius::same((r * self.unit).round() as u8),
            self.stroke,
            StrokeKind::Middle,
        );
    }
}

pub fn paint_icon(painter: &Painter, rect: Rect, icon: VectorIcon, color: Color32) {
    let size = rect.width().min(rect.height());
    let unit = size / 24.0;
    let pen = Pen {
        painter,
        origin: rect.center() - Vec2::splat(size / 2.0),
        unit,
        stroke: Stroke::new((1.7 * unit).max(1.15), color),
        color,
    };

    match icon {
        VectorIcon::Overview => pen.line(&[
            (2.5, 12.5),
            (6.5, 12.5),
            (9.5, 5.0),
            (14.5, 19.0),
            (17.5, 12.5),
            (21.5, 12.5),
        ]),
        VectorIcon::Folder => pen.closed(&[
            (3.0, 5.5),
            (9.5, 5.5),
            (11.5, 8.0),
            (21.0, 8.0),
            (21.0, 19.0),
            (3.0, 19.0),
        ]),
        VectorIcon::Snapshots => {
            pen.closed(&[(12.0, 3.5), (21.0, 8.0), (12.0, 12.5), (3.0, 8.0)]);
            pen.line(&[(3.0, 12.0), (12.0, 16.5), (21.0, 12.0)]);
            pen.line(&[(3.0, 16.0), (12.0, 20.5), (21.0, 16.0)]);
        }
        VectorIcon::History => {
            let pts = pen.arc(12.0, 12.0, 8.5, 200.0, 520.0);
            if let Some(&start) = pts.first() {
                let tip = start;
                painter.add(Shape::line(
                    vec![
                        tip + Vec2::new(-3.2, -0.6) * unit,
                        tip,
                        tip + Vec2::new(0.4, -3.4) * unit,
                    ],
                    pen.stroke,
                ));
            }
            pen.line(&[(12.0, 7.5), (12.0, 12.0), (15.0, 14.0)]);
        }
        VectorIcon::Settings => {
            pen.line(&[(3.5, 7.0), (6.8, 7.0)]);
            pen.line(&[(11.2, 7.0), (20.5, 7.0)]);
            pen.circle(9.0, 7.0, 2.2);
            pen.line(&[(3.5, 17.0), (12.8, 17.0)]);
            pen.line(&[(17.2, 17.0), (20.5, 17.0)]);
            pen.circle(15.0, 17.0, 2.2);
        }
        VectorIcon::Plus => {
            pen.line(&[(12.0, 5.0), (12.0, 19.0)]);
            pen.line(&[(5.0, 12.0), (19.0, 12.0)]);
        }
        VectorIcon::Minus => pen.line(&[(5.0, 12.0), (19.0, 12.0)]),
        VectorIcon::Trash => {
            pen.line(&[(3.5, 6.5), (20.5, 6.5)]);
            pen.line(&[(9.0, 6.5), (9.0, 3.8), (15.0, 3.8), (15.0, 6.5)]);
            pen.line(&[(5.8, 6.5), (6.8, 20.2), (17.2, 20.2), (18.2, 6.5)]);
            pen.line(&[(10.0, 10.5), (10.0, 16.5)]);
            pen.line(&[(14.0, 10.5), (14.0, 16.5)]);
        }
        VectorIcon::Refresh => {
            let pts = pen.arc(12.0, 12.0, 7.5, -30.0, 255.0);
            if let Some(&tip) = pts.first() {
                painter.add(Shape::line(
                    vec![
                        tip + Vec2::new(-3.6, -0.4) * unit,
                        tip,
                        tip + Vec2::new(0.6, -3.6) * unit,
                    ],
                    pen.stroke,
                ));
            }
        }
        VectorIcon::Sun => {
            pen.circle(12.0, 12.0, 3.8);
            for i in 0..8 {
                let a = (i as f32 * 45.0).to_radians();
                let (s, c) = a.sin_cos();
                pen.line(&[
                    (12.0 + c * 6.6, 12.0 + s * 6.6),
                    (12.0 + c * 8.8, 12.0 + s * 8.8),
                ]);
            }
        }
        VectorIcon::Moon => {
            // Outer circle (12,12) r8 minus a circle at (16,8) r6.5; the arcs meet at the
            // two intersection points, 8.6 and 261.4 degrees on the outer circle.
            let mut pts = arc_points(pen.p(12.0, 12.0), 8.0 * unit, 8.6, 261.4);
            let inner = arc_points(pen.p(16.0, 8.0), 6.5 * unit, 217.0, 53.0);
            pts.extend(inner);
            painter.add(Shape::closed_line(pts, pen.stroke));
        }
        VectorIcon::Terminal => {
            pen.line(&[(5.0, 7.0), (10.0, 12.0), (5.0, 17.0)]);
            pen.line(&[(12.5, 17.5), (19.5, 17.5)]);
        }
        VectorIcon::Check => pen.line(&[(5.0, 12.5), (10.0, 17.5), (19.5, 7.0)]),
        VectorIcon::Close => {
            pen.line(&[(6.0, 6.0), (18.0, 18.0)]);
            pen.line(&[(18.0, 6.0), (6.0, 18.0)]);
        }
        VectorIcon::ChevronRight => pen.line(&[(9.0, 5.5), (15.5, 12.0), (9.0, 18.5)]),
        VectorIcon::ChevronDown => pen.line(&[(5.5, 9.0), (12.0, 15.5), (18.5, 9.0)]),
        VectorIcon::ArrowRight => {
            pen.line(&[(4.5, 12.0), (19.0, 12.0)]);
            pen.line(&[(13.0, 6.0), (19.0, 12.0), (13.0, 18.0)]);
        }
        VectorIcon::Copy => {
            pen.rect(8.5, 8.5, 12.0, 12.0, 2.0);
            pen.line(&[
                (15.5, 8.5),
                (15.5, 3.5),
                (3.5, 3.5),
                (3.5, 15.5),
                (8.5, 15.5),
            ]);
        }
        VectorIcon::Eye | VectorIcon::EyeOff => {
            let lid: Vec<(f32, f32)> = (0..=12)
                .map(|i| {
                    let x = 2.0 + 20.0 * i as f32 / 12.0;
                    (
                        x,
                        12.0 - 6.5 * (std::f32::consts::PI * (x - 2.0) / 20.0).sin(),
                    )
                })
                .chain((0..=12).rev().map(|i| {
                    let x = 2.0 + 20.0 * i as f32 / 12.0;
                    (
                        x,
                        12.0 + 6.5 * (std::f32::consts::PI * (x - 2.0) / 20.0).sin(),
                    )
                }))
                .collect();
            pen.closed(&lid);
            pen.circle(12.0, 12.0, 3.0);
            if icon == VectorIcon::EyeOff {
                pen.line(&[(4.0, 4.0), (20.0, 20.0)]);
            }
        }
        VectorIcon::Play => pen.closed(&[(8.0, 5.0), (19.0, 12.0), (8.0, 19.0)]),
        VectorIcon::Stop => pen.rect(6.5, 6.5, 11.0, 11.0, 2.0),
        VectorIcon::Upload => {
            pen.line(&[(12.0, 15.5), (12.0, 4.0)]);
            pen.line(&[(7.0, 9.0), (12.0, 4.0), (17.0, 9.0)]);
            pen.line(&[(4.0, 14.5), (4.0, 20.0), (20.0, 20.0), (20.0, 14.5)]);
        }
        VectorIcon::Download => {
            pen.line(&[(12.0, 4.0), (12.0, 15.0)]);
            pen.line(&[(7.0, 10.0), (12.0, 15.0), (17.0, 10.0)]);
            pen.line(&[(4.0, 14.5), (4.0, 20.0), (20.0, 20.0), (20.0, 14.5)]);
        }
        VectorIcon::Alert => {
            pen.closed(&[(12.0, 3.5), (21.5, 20.0), (2.5, 20.0)]);
            pen.line(&[(12.0, 9.5), (12.0, 14.0)]);
            pen.dot(12.0, 17.0, 1.1);
        }
        VectorIcon::File => {
            pen.closed(&[
                (6.0, 3.0),
                (14.0, 3.0),
                (19.0, 8.0),
                (19.0, 21.0),
                (6.0, 21.0),
            ]);
            pen.line(&[(14.0, 3.0), (14.0, 8.0), (19.0, 8.0)]);
        }
        VectorIcon::Compare => {
            pen.line(&[(4.0, 8.0), (18.0, 8.0)]);
            pen.line(&[(14.5, 4.5), (18.0, 8.0), (14.5, 11.5)]);
            pen.line(&[(20.0, 16.0), (6.0, 16.0)]);
            pen.line(&[(9.5, 12.5), (6.0, 16.0), (9.5, 19.5)]);
        }
        VectorIcon::HideToTray => {
            pen.rect(3.5, 4.5, 17.0, 15.0, 2.5);
            pen.line(&[(3.5, 8.5), (20.5, 8.5)]);
            pen.line(&[(9.0, 15.5), (15.0, 15.5)]);
        }
        VectorIcon::Power => {
            pen.arc(12.0, 13.0, 7.8, -55.0, 235.0);
            pen.line(&[(12.0, 3.0), (12.0, 11.0)]);
        }
        VectorIcon::Key => {
            pen.circle(8.0, 15.5, 4.2);
            pen.line(&[(11.0, 12.5), (20.0, 3.5)]);
            pen.line(&[(17.0, 6.5), (19.5, 9.0)]);
            pen.line(&[(14.5, 9.0), (16.5, 11.0)]);
        }
        VectorIcon::ShieldCheck => {
            pen.closed(&[
                (12.0, 2.8),
                (19.5, 5.8),
                (19.5, 11.5),
                (17.8, 16.0),
                (12.0, 21.0),
                (6.2, 16.0),
                (4.5, 11.5),
                (4.5, 5.8),
            ]);
            pen.line(&[(8.5, 12.0), (11.0, 14.5), (15.5, 9.5)]);
        }
        VectorIcon::Edit => {
            pen.closed(&[
                (15.5, 4.5),
                (19.5, 8.5),
                (8.5, 19.5),
                (4.5, 19.5),
                (4.5, 15.5),
            ]);
            pen.line(&[(13.0, 7.0), (17.0, 11.0)]);
        }
    }
}

/// App mark: a lime tile with an open ring. The gap reads as an ongoing cycle of snapshots.
pub fn paint_brand_mark(painter: &Painter, rect: Rect, tile: Color32, ink: Color32) {
    let s = rect.width().min(rect.height());
    let tile_rect = Rect::from_center_size(rect.center(), Vec2::splat(s));
    painter.rect_filled(tile_rect, CornerRadius::same((s * 0.27) as u8), tile);
    let c = tile_rect.center();
    let ring = arc_points(c, s * 0.25, -10.0, 280.0);
    // egui strokes have butt caps; dots at both ends give the round caps of the app icon.
    for end in [ring.first(), ring.last()].into_iter().flatten() {
        painter.circle_filled(*end, s * 0.055, ink);
    }
    painter.add(Shape::line(ring, Stroke::new(s * 0.11, ink)));
    painter.circle_filled(c, s * 0.08, ink);
}
