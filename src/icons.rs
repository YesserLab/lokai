//! Lucide icons (https://lucide.dev, ISC license, see assets/lucide). The SVGs are embedded,
//! rasterized by egui_extras at the exact on-screen size and tinted with the requested color.

use eframe::egui::{self, Color32, Rect, Response, Sense, Vec2};
use std::sync::OnceLock;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Icon {
    Settings,
    Trash,
    FolderOpen,
    Check,
    X,
    Ban,
    Warning,
    Error,
    Copy,
    ArrowUp,
    Square,
    ChevronRight,
    ChevronDown,
    Loader,
    Bot,
}

macro_rules! lucide {
    ($($icon:ident => $file:literal),* $(,)?) => {
        const ALL: &[(Icon, &str, &str)] = &[$((Icon::$icon, $file, include_str!(concat!("../assets/lucide/", $file, ".svg")))),*];
    };
}

lucide! {
    Settings => "settings",
    Trash => "trash",
    FolderOpen => "folder-open",
    Check => "check",
    X => "x",
    Ban => "ban",
    Warning => "triangle-exclamation-point",
    Error => "circle-exclamation-point",
    Copy => "copy",
    ArrowUp => "arrow-up",
    Square => "square",
    ChevronRight => "chevron-right",
    ChevronDown => "chevron-down",
    Loader => "loader-circle",
    Bot => "bot-message-square",
}

impl Icon {
    fn index(self) -> usize {
        ALL.iter().position(|e| e.0 == self).expect("every icon is listed")
    }

    /// The original Lucide SVG (`stroke="currentColor"`).
    pub fn svg(self) -> &'static str {
        ALL[self.index()].2
    }

    /// The SVG drawn in white, so that `Image::tint` produces the final color.
    fn white(self) -> &'static [u8] {
        static WHITE: OnceLock<Vec<Vec<u8>>> = OnceLock::new();
        &WHITE.get_or_init(|| ALL.iter().map(|e| e.2.replace("currentColor", "#ffffff").into_bytes()).collect())[self.index()]
    }

    pub fn image(self, color: Color32) -> egui::Image<'static> {
        egui::Image::from_bytes(format!("bytes://lucide/{}.svg", ALL[self.index()].1), self.white()).tint(color)
    }
}

pub fn paint(ui: &egui::Ui, rect: Rect, icon: Icon, color: Color32) {
    icon.image(color).paint_at(ui, rect);
}

/// Allocates `size` and paints the icon there.
pub fn show(ui: &mut egui::Ui, icon: Icon, size: f32, color: Color32) -> Response {
    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(size), Sense::hover());
    paint(ui, rect, icon, color);
    resp
}

/// A rotating loader, replacing egui's spinner.
pub fn spinner(ui: &mut egui::Ui, size: f32, color: Color32) -> Response {
    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(size), Sense::hover());
    let angle = (ui.input(|i| i.time) * std::f64::consts::TAU * 0.9) as f32;
    Icon::Loader.image(color).rotate(angle, Vec2::splat(0.5)).paint_at(ui, rect);
    ui.ctx().request_repaint();
    resp
}

/// A frameless icon button with a subtle hover background.
pub fn button(ui: &mut egui::Ui, icon: Icon, color: Color32, hover_bg: Color32, tip: &str) -> Response {
    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(26.0), Sense::click());
    if resp.hovered() {
        ui.painter().rect_filled(rect, 6.0, hover_bg);
    }
    paint(ui, rect.shrink(5.5), icon, color);
    resp.on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_text(tip)
}

/// Lucide chevron for combo boxes (`ComboBox::icon`).
pub fn combo_chevron(ui: &egui::Ui, rect: Rect, visuals: &egui::style::WidgetVisuals, _open: bool) {
    paint(ui, Rect::from_center_size(rect.center(), Vec2::splat(rect.height().max(12.0))), Icon::ChevronDown, visuals.fg_stroke.color);
}
