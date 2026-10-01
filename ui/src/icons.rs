//! Crisp vector icons drawn with epaint primitives — no icon font, no
//! binary assets, resolution independent.

use egui::epaint::PathShape;
use egui::{Color32, CornerRadius, Painter, Pos2, Rect, Shape, Stroke, Vec2};

/// Icon identifiers (16px design grid, scaled at draw time).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Icon {
    /// Back arrow.
    Back,
    /// Forward arrow.
    Forward,
    /// Reload (circular arrow).
    Reload,
    /// Stop (X).
    Stop,
    /// Home.
    Home,
    /// Star (bookmark).
    Star,
    /// Star filled.
    StarFilled,
    /// Downloads (tray + arrow).
    Download,
    /// Privacy shield.
    Shield,
    /// Devtools (</>).
    Code,
    /// Close (×).
    Close,
    /// Plus.
    Plus,
    /// Search (magnifier).
    Search,
    /// Lock (secure).
    Lock,
    /// Warning triangle (insecure).
    Warning,
    /// Info circle.
    Info,
    /// Settings gear.
    Gear,
    /// History clock.
    Clock,
    /// Bookmarks.
    Bookmarks,
    /// Extension puzzle.
    Puzzle,
    /// Print.
    Print,
    /// Save.
    Save,
    /// Find in page.
    Find,
    /// Zoom in.
    ZoomIn,
    /// Zoom out.
    ZoomOut,
    /// Fullscreen.
    Fullscreen,
    /// Muted speaker.
    Mute,
    /// External link (↗).
    External,
    /// Check.
    Check,
}

impl Icon {
    /// Draws the icon centered in `rect`, stroked with `color`.
    pub fn paint(self, painter: &Painter, rect: Rect, color: Color32) {
        let size = rect.size().min_elem().min(24.0);
        let stroke = Stroke::new((size / 9.0).clamp(1.4, 2.4), color);
        let c = rect.center();
        let s = size / 2.0;
        match self {
            Icon::Back => {
                // shaft + arrowhead
                painter.line_segment(
                    [pos2(c.x - s * 0.55, c.y), pos2(c.x + s * 0.45, c.y)],
                    stroke,
                );
                painter.line_segment(
                    [
                        pos2(c.x - s * 0.55, c.y),
                        pos2(c.x - s * 0.05, c.y - s * 0.5),
                    ],
                    stroke,
                );
                painter.line_segment(
                    [
                        pos2(c.x - s * 0.55, c.y),
                        pos2(c.x - s * 0.05, c.y + s * 0.5),
                    ],
                    stroke,
                );
            }
            Icon::Forward => {
                painter.line_segment(
                    [pos2(c.x - s * 0.45, c.y), pos2(c.x + s * 0.55, c.y)],
                    stroke,
                );
                painter.line_segment(
                    [
                        pos2(c.x + s * 0.55, c.y),
                        pos2(c.x + s * 0.05, c.y - s * 0.5),
                    ],
                    stroke,
                );
                painter.line_segment(
                    [
                        pos2(c.x + s * 0.55, c.y),
                        pos2(c.x + s * 0.05, c.y + s * 0.5),
                    ],
                    stroke,
                );
            }
            Icon::Reload => {
                let r = s * 0.75;
                // ~300° arc with an arrowhead.
                let start_deg = -60.0_f32.to_radians();
                let end_deg = 200.0_f32.to_radians();
                painter.add(Shape::Path(PathShape::line(
                    arc_points(c, r, start_deg, end_deg, 18),
                    stroke,
                )));
                let tip = Pos2 {
                    x: c.x + r * end_deg.cos(),
                    y: c.y + r * end_deg.sin(),
                };
                painter.line_segment([tip, tip + vec2(-s * 0.30, -s * 0.10)], stroke);
                painter.line_segment([tip, tip + vec2(-s * 0.05, s * 0.35)], stroke);
            }
            Icon::Stop => {
                painter.line_segment(
                    [c + vec2(-s * 0.5, -s * 0.5), c + vec2(s * 0.5, s * 0.5)],
                    stroke,
                );
                painter.line_segment(
                    [c + vec2(-s * 0.5, s * 0.5), c + vec2(s * 0.5, -s * 0.5)],
                    stroke,
                );
            }
            Icon::Home => {
                painter.line_segment(
                    [pos2(c.x - s * 0.8, c.y), pos2(c.x, c.y - s * 0.85)],
                    stroke,
                );
                painter.line_segment(
                    [pos2(c.x, c.y - s * 0.85), pos2(c.x + s * 0.8, c.y)],
                    stroke,
                );
                painter.line_segment(
                    [
                        pos2(c.x - s * 0.55, c.y - s * 0.25),
                        pos2(c.x - s * 0.55, c.y + s * 0.75),
                    ],
                    stroke,
                );
                painter.line_segment(
                    [
                        pos2(c.x + s * 0.55, c.y - s * 0.25),
                        pos2(c.x + s * 0.55, c.y + s * 0.75),
                    ],
                    stroke,
                );
                painter.line_segment(
                    [
                        pos2(c.x - s * 0.55, c.y + s * 0.75),
                        pos2(c.x + s * 0.55, c.y + s * 0.75),
                    ],
                    stroke,
                );
            }
            Icon::Star | Icon::StarFilled => {
                let points = star_points(
                    c,
                    s * 0.9,
                    if self == Icon::StarFilled { 0.5 } else { 0.42 },
                );
                if self == Icon::StarFilled {
                    painter.add(Shape::Path(PathShape {
                        points: points.clone(),
                        closed: true,
                        fill: color,
                        stroke: Stroke::NONE.into(),
                    }));
                } else {
                    painter.add(Shape::Path(PathShape::closed_line(points, stroke)));
                }
            }
            Icon::Download => {
                painter.line_segment(
                    [pos2(c.x, c.y - s * 0.8), pos2(c.x, c.y + s * 0.25)],
                    stroke,
                );
                painter.line_segment(
                    [
                        pos2(c.x - s * 0.4, c.y - s * 0.1),
                        pos2(c.x, c.y + s * 0.35),
                    ],
                    stroke,
                );
                painter.line_segment(
                    [
                        pos2(c.x + s * 0.4, c.y - s * 0.1),
                        pos2(c.x, c.y + s * 0.35),
                    ],
                    stroke,
                );
                painter.line_segment(
                    [
                        pos2(c.x - s * 0.75, c.y + s * 0.65),
                        pos2(c.x + s * 0.75, c.y + s * 0.65),
                    ],
                    stroke,
                );
            }
            Icon::Shield => {
                let top = c + vec2(0.0, -s * 0.85);
                let left = c + vec2(-s * 0.7, -s * 0.45);
                let right = c + vec2(s * 0.7, -s * 0.45);
                let bottom = c + vec2(0.0, s * 0.9);
                painter.add(Shape::Path(PathShape::closed_line(
                    vec![
                        top,
                        left,
                        pos2(left.x, c.y + s * 0.05),
                        bottom,
                        pos2(right.x, c.y + s * 0.05),
                        right,
                    ],
                    stroke,
                )));
                if self == Icon::Shield {
                    // check inside
                    painter.line_segment(
                        [c + vec2(-s * 0.25, 0.0), c + vec2(-s * 0.05, s * 0.25)],
                        stroke,
                    );
                    painter.line_segment(
                        [c + vec2(-s * 0.05, s * 0.25), c + vec2(s * 0.35, -s * 0.3)],
                        stroke,
                    );
                }
            }
            Icon::Code => {
                painter.line_segment(
                    [c + vec2(-s * 0.65, -s * 0.4), c + vec2(-s * 0.15, 0.0)],
                    stroke,
                );
                painter.line_segment(
                    [c + vec2(-s * 0.65, s * 0.4), c + vec2(-s * 0.15, 0.0)],
                    stroke,
                );
                painter.line_segment(
                    [c + vec2(s * 0.65, -s * 0.4), c + vec2(s * 0.15, 0.0)],
                    stroke,
                );
                painter.line_segment(
                    [c + vec2(s * 0.65, s * 0.4), c + vec2(s * 0.15, 0.0)],
                    stroke,
                );
                painter.line_segment(
                    [
                        pos2(c.x - s * 0.05, c.y + s * 0.55),
                        pos2(c.x + s * 0.05, c.y - s * 0.55),
                    ],
                    stroke,
                );
            }
            Icon::Close => {
                painter.line_segment(
                    [c + vec2(-s * 0.45, -s * 0.45), c + vec2(s * 0.45, s * 0.45)],
                    stroke,
                );
                painter.line_segment(
                    [c + vec2(-s * 0.45, s * 0.45), c + vec2(s * 0.45, -s * 0.45)],
                    stroke,
                );
            }
            Icon::Plus => {
                painter.line_segment([pos2(c.x - s * 0.6, c.y), pos2(c.x + s * 0.6, c.y)], stroke);
                painter.line_segment([pos2(c.x, c.y - s * 0.6), pos2(c.x, c.y + s * 0.6)], stroke);
            }
            Icon::Search => {
                painter.circle_stroke(c + vec2(-s * 0.15, -s * 0.15), s * 0.55, stroke);
                painter.line_segment(
                    [c + vec2(s * 0.25, s * 0.25), c + vec2(s * 0.7, s * 0.7)],
                    stroke,
                );
            }
            Icon::Lock => {
                let body = Rect::from_center_size(c + vec2(0.0, s * 0.2), vec2(s * 1.0, s * 0.75));
                painter.rect_filled(body, CornerRadius::same(3), color);
                painter.add(Shape::Path(PathShape::line(
                    arc_points(
                        Pos2::new(body.center().x, body.top()),
                        s * 0.4,
                        std::f32::consts::PI,
                        std::f32::consts::TAU,
                        10,
                    ),
                    stroke,
                )));
            }
            Icon::Warning => {
                painter.add(Shape::Path(PathShape::closed_line(
                    vec![
                        c + vec2(0.0, -s * 0.8),
                        c + vec2(s * 0.85, s * 0.7),
                        c + vec2(-s * 0.85, s * 0.7),
                    ],
                    stroke,
                )));
                painter.line_segment(
                    [pos2(c.x, c.y - s * 0.25), pos2(c.x, c.y + s * 0.25)],
                    stroke,
                );
                painter.circle_filled(pos2(c.x, c.y + s * 0.5), (size / 9.0).max(1.5), color);
            }
            Icon::Info => {
                painter.circle_stroke(c, s * 0.8, stroke);
                painter.line_segment(
                    [pos2(c.x, c.y - s * 0.15), pos2(c.x, c.y + s * 0.35)],
                    stroke,
                );
                painter.circle_filled(pos2(c.x, c.y - s * 0.5), (size / 9.0).max(1.5), color);
            }
            Icon::Gear => {
                let r = s * 0.62;
                painter.circle_stroke(c, r * 0.45, stroke);
                for i in 0..8 {
                    let a = i as f32 * (std::f32::consts::TAU / 8.0);
                    let outer = c + vec2(a.cos() * s * 0.9, a.sin() * s * 0.9);
                    let inner = c + vec2(a.cos() * r, a.sin() * r);
                    painter.line_segment([inner, outer], stroke);
                }
            }
            Icon::Clock => {
                painter.circle_stroke(c, s * 0.8, stroke);
                painter.line_segment([c, c + vec2(0.0, -s * 0.5)], stroke);
                painter.line_segment([c, c + vec2(s * 0.35, s * 0.1)], stroke);
            }
            Icon::Bookmarks => {
                for dx in [-s * 0.45, 0.0, s * 0.45] {
                    painter.line_segment(
                        [
                            pos2(c.x + dx, c.y - s * 0.75),
                            pos2(c.x + dx, c.y + s * 0.7),
                        ],
                        stroke,
                    );
                    painter.line_segment(
                        [
                            pos2(c.x + dx, c.y + s * 0.7),
                            pos2(c.x + dx - s * 0.15, c.y + s * 0.45),
                        ],
                        stroke,
                    );
                    painter.line_segment(
                        [
                            pos2(c.x + dx, c.y + s * 0.7),
                            pos2(c.x + dx + s * 0.15, c.y + s * 0.45),
                        ],
                        stroke,
                    );
                }
            }
            Icon::Puzzle => {
                let body = Rect::from_center_size(c, vec2(s * 1.1, s * 0.8));
                painter.rect_stroke(
                    body,
                    CornerRadius::same(3),
                    stroke,
                    egui::epaint::StrokeKind::Inside,
                );
                painter.circle_stroke(Pos2::new(body.center().x, body.top()), s * 0.18, stroke);
            }
            Icon::Print => {
                let body = Rect::from_center_size(c + vec2(0.0, s * 0.1), vec2(s * 1.2, s * 0.55));
                painter.rect_stroke(
                    body,
                    CornerRadius::same(3),
                    stroke,
                    egui::epaint::StrokeKind::Inside,
                );
                let paper =
                    Rect::from_center_size(c + vec2(0.0, -s * 0.35), vec2(s * 0.7, s * 0.55));
                painter.rect_stroke(
                    paper,
                    CornerRadius::same(2),
                    stroke,
                    egui::epaint::StrokeKind::Inside,
                );
                let tray = Rect::from_center_size(c + vec2(0.0, s * 0.55), vec2(s * 0.8, s * 0.25));
                painter.rect_stroke(
                    tray,
                    CornerRadius::same(2),
                    stroke,
                    egui::epaint::StrokeKind::Inside,
                );
            }
            Icon::Save => {
                painter.rect_stroke(
                    Rect::from_center_size(c, vec2(s * 1.2, s * 1.2)),
                    CornerRadius::same(3),
                    stroke,
                    egui::epaint::StrokeKind::Inside,
                );
                painter.rect_filled(
                    Rect::from_center_size(c + vec2(0.0, -s * 0.35), vec2(s * 0.9, s * 0.35)),
                    CornerRadius::same(2),
                    color,
                );
                painter.rect_filled(
                    Rect::from_center_size(c + vec2(0.0, s * 0.3), vec2(s * 0.9, s * 0.5)),
                    CornerRadius::same(2),
                    egui::Color32::TRANSPARENT,
                );
                painter.rect_stroke(
                    Rect::from_center_size(c + vec2(0.0, s * 0.3), vec2(s * 0.6, s * 0.4)),
                    CornerRadius::same(2),
                    stroke,
                    egui::epaint::StrokeKind::Inside,
                );
            }
            Icon::Find => {
                painter.line_segment(
                    [
                        pos2(c.x - s * 0.7, c.y - s * 0.2),
                        pos2(c.x + s * 0.5, c.y - s * 0.2),
                    ],
                    stroke,
                );
                painter.line_segment(
                    [
                        pos2(c.x - s * 0.5, c.y - s * 0.7),
                        pos2(c.x - s * 0.5, c.y + s * 0.7),
                    ],
                    stroke,
                );
                for i in 0..3 {
                    let y = c.y + s * 0.1 + i as f32 * s * 0.3;
                    painter.line_segment(
                        [pos2(c.x - s * 0.6, y), pos2(c.x + s * 0.6, y)],
                        Stroke::new(stroke.width * 0.8, color),
                    );
                }
            }
            Icon::ZoomIn => {
                painter.circle_stroke(c + vec2(-s * 0.12, -s * 0.12), s * 0.55, stroke);
                painter.line_segment(
                    [c + vec2(s * 0.3, s * 0.3), c + vec2(s * 0.75, s * 0.75)],
                    stroke,
                );
                painter.line_segment(
                    [c + vec2(-s * 0.35, -s * 0.12), c + vec2(s * 0.1, -s * 0.12)],
                    stroke,
                );
                painter.line_segment(
                    [c + vec2(-s * 0.12, -s * 0.35), c + vec2(-s * 0.12, s * 0.1)],
                    stroke,
                );
            }
            Icon::ZoomOut => {
                painter.circle_stroke(c + vec2(-s * 0.12, -s * 0.12), s * 0.55, stroke);
                painter.line_segment(
                    [c + vec2(s * 0.3, s * 0.3), c + vec2(s * 0.75, s * 0.75)],
                    stroke,
                );
                painter.line_segment(
                    [c + vec2(-s * 0.35, -s * 0.12), c + vec2(s * 0.1, -s * 0.12)],
                    stroke,
                );
            }
            Icon::Fullscreen => {
                for (dx, dy) in [(-1.0, -1.0), (1.0, -1.0), (-1.0, 1.0), (1.0, 1.0)] {
                    let corner = c + vec2(dx * s * 0.7, dy * s * 0.7);
                    let along_x = corner + vec2(-dx * s * 0.35, 0.0);
                    let along_y = corner + vec2(0.0, -dy * s * 0.35);
                    painter.line_segment([corner, along_x], stroke);
                    painter.line_segment([corner, along_y], stroke);
                }
            }
            Icon::Mute => {
                painter.line_segment(
                    [c + vec2(-s * 0.7, -s * 0.25), c + vec2(-s * 0.3, -s * 0.25)],
                    stroke,
                );
                painter.line_segment(
                    [c + vec2(-s * 0.3, -s * 0.25), c + vec2(-s * 0.3, s * 0.25)],
                    stroke,
                );
                painter.line_segment(
                    [c + vec2(-s * 0.7, s * 0.25), c + vec2(-s * 0.3, s * 0.25)],
                    stroke,
                );
                // speaker cone
                painter.line_segment(
                    [
                        c + vec2(-s * 0.3, -s * 0.25),
                        c + vec2(-s * 0.05, -s * 0.55),
                    ],
                    stroke,
                );
                painter.line_segment(
                    [c + vec2(-s * 0.3, s * 0.25), c + vec2(-s * 0.05, s * 0.55)],
                    stroke,
                );
                // cross
                painter.line_segment(
                    [c + vec2(s * 0.1, -s * 0.35), c + vec2(s * 0.7, s * 0.35)],
                    stroke,
                );
                painter.line_segment(
                    [c + vec2(s * 0.1, s * 0.35), c + vec2(s * 0.7, -s * 0.35)],
                    stroke,
                );
            }
            Icon::External => {
                painter.line_segment(
                    [c + vec2(-s * 0.6, s * 0.6), c + vec2(s * 0.6, -s * 0.6)],
                    stroke,
                );
                painter.line_segment(
                    [c + vec2(s * 0.6, -s * 0.6), c + vec2(s * 0.6, -s * 0.1)],
                    stroke,
                );
                painter.line_segment(
                    [c + vec2(s * 0.6, -s * 0.6), c + vec2(s * 0.1, -s * 0.6)],
                    stroke,
                );
            }
            Icon::Check => {
                painter.line_segment(
                    [c + vec2(-s * 0.6, 0.0), c + vec2(-s * 0.15, s * 0.45)],
                    stroke,
                );
                painter.line_segment(
                    [c + vec2(-s * 0.15, s * 0.45), c + vec2(s * 0.65, -s * 0.5)],
                    stroke,
                );
            }
        }
    }
}

fn star_points(c: Pos2, r: f32, inner_ratio: f32) -> Vec<Pos2> {
    let mut pts = Vec::with_capacity(10);
    for i in 0..10 {
        let a = (i as f32 - 2.0) * std::f32::consts::FRAC_PI_2 / 5.0 * 2.0;
        let radius = if i % 2 == 0 { r } else { r * inner_ratio };
        pts.push(c + vec2(a.sin() * radius, -a.cos() * radius));
    }
    pts
}

/// Points along a circular arc (radians, screen space).
fn arc_points(center: Pos2, radius: f32, start: f32, end: f32, segments: usize) -> Vec<Pos2> {
    let mut pts = Vec::with_capacity(segments + 1);
    for i in 0..=segments {
        let t = i as f32 / segments as f32;
        let a = start + (end - start) * t;
        pts.push(center + vec2(a.cos() * radius, a.sin() * radius));
    }
    pts
}

fn vec2(x: f32, y: f32) -> Vec2 {
    Vec2::new(x, y)
}

fn pos2(x: f32, y: f32) -> Pos2 {
    Pos2::new(x, y)
}
