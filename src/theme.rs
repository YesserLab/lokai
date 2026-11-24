//! Look & feel: palette derived from the KDE color scheme, system fonts, and the app icon.

use eframe::egui::{self, Color32, CornerRadius, FontData, FontDefinitions, FontFamily, FontId, Stroke, TextStyle, Vec2};
use std::sync::{Arc, OnceLock};

#[derive(Clone, Copy)]
pub struct Palette {
    pub dark: bool,
    pub accent: Color32,
    pub on_accent: Color32,
    /// Launcher panel background.
    pub bg: Color32,
    /// Cards, input fields.
    pub surface: Color32,
    /// Code blocks, hovered widgets.
    pub surface2: Color32,
    pub border: Color32,
    pub text: Color32,
    pub muted: Color32,
    pub faint: Color32,
    pub ok: Color32,
    pub warn: Color32,
    pub err: Color32,
    pub user_bubble: Color32,
}

fn parse_rgb(v: &str) -> Option<Color32> {
    let mut it = v.split(',').map(|p| p.trim().parse::<u8>().ok());
    Some(Color32::from_rgb(it.next()??, it.next()??, it.next()??))
}

/// Reads `group/key` from ~/.config/kdeglobals.
fn kde_value(group: &str, key: &str) -> Option<String> {
    let text = std::fs::read_to_string(crate::config::home_dir().join(".config/kdeglobals")).ok()?;
    let mut in_group = false;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_group = line == format!("[{group}]");
        } else if in_group
            && let Some((k, v)) = line.split_once('=')
            && k.trim() == key
        {
            return Some(v.trim().to_string());
        }
    }
    None
}

fn mix(a: Color32, b: Color32, t: f32) -> Color32 {
    let l = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    Color32::from_rgb(l(a.r(), b.r()), l(a.g(), b.g()), l(a.b(), b.b()))
}

impl Palette {
    pub fn detect() -> Self {
        let window_bg = kde_value("Colors:Window", "BackgroundNormal").and_then(|v| parse_rgb(&v));
        let dark = window_bg.is_none_or(|c| (c.r() as u32 * 299 + c.g() as u32 * 587 + c.b() as u32 * 114) / 1000 < 128);
        let accent = kde_value("General", "AccentColor")
            .and_then(|v| parse_rgb(&v))
            .or_else(|| kde_value("Colors:Selection", "BackgroundNormal").and_then(|v| parse_rgb(&v)))
            .unwrap_or(Color32::from_rgb(0x7c, 0x6c, 0xf6));
        if dark {
            let bg = Color32::from_rgb(0x16, 0x17, 0x1c);
            let surface = Color32::from_rgb(0x1f, 0x21, 0x28);
            Self {
                dark,
                accent,
                on_accent: Color32::WHITE,
                bg,
                surface,
                surface2: Color32::from_rgb(0x29, 0x2c, 0x35),
                border: Color32::from_rgb(0x30, 0x33, 0x3d),
                text: Color32::from_rgb(0xd8, 0xdb, 0xe2),
                muted: Color32::from_rgb(0x9a, 0x9f, 0xab),
                faint: Color32::from_rgb(0x62, 0x67, 0x72),
                ok: Color32::from_rgb(0x4a, 0xd6, 0x8a),
                warn: Color32::from_rgb(0xf3, 0xb3, 0x4c),
                err: Color32::from_rgb(0xf2, 0x6d, 0x6d),
                user_bubble: mix(surface, accent, 0.28),
            }
        } else {
            let surface = Color32::from_rgb(0xf1, 0xf2, 0xf6);
            Self {
                dark,
                accent,
                on_accent: Color32::WHITE,
                bg: Color32::from_rgb(0xfb, 0xfb, 0xfd),
                surface,
                surface2: Color32::from_rgb(0xe8, 0xea, 0xf0),
                border: Color32::from_rgb(0xdd, 0xe0, 0xe8),
                text: Color32::from_rgb(0x1b, 0x1d, 0x24),
                muted: Color32::from_rgb(0x62, 0x67, 0x75),
                faint: Color32::from_rgb(0xa0, 0xa4, 0xb0),
                ok: Color32::from_rgb(0x1f, 0xa8, 0x5f),
                warn: Color32::from_rgb(0xc9, 0x80, 0x10),
                err: Color32::from_rgb(0xd6, 0x3a, 0x3a),
                user_bubble: mix(surface, accent, 0.22),
            }
        }
    }

    pub fn tint(&self, c: Color32, alpha: u8) -> Color32 {
        Color32::from_rgba_unmultiplied(c.r(), c.g(), c.b(), alpha)
    }

    pub fn apply(&self, ctx: &egui::Context) {
        let mut v = if self.dark { egui::Visuals::dark() } else { egui::Visuals::light() };
        v.panel_fill = self.bg;
        v.window_fill = self.surface;
        v.window_stroke = Stroke::new(1.0, self.border);
        v.extreme_bg_color = self.surface;
        v.faint_bg_color = self.surface;
        v.code_bg_color = self.surface2;
        v.hyperlink_color = self.accent;
        v.selection.bg_fill = self.tint(self.accent, 110);
        v.selection.stroke = Stroke::new(1.0, self.accent);
        v.window_corner_radius = CornerRadius::same(12);
        v.menu_corner_radius = CornerRadius::same(10);
        v.popup_shadow =
            egui::Shadow { offset: [0, 6], blur: 18, spread: 0, color: Color32::from_black_alpha(if self.dark { 120 } else { 40 }) };
        v.window_shadow = v.popup_shadow;
        v.striped = false;
        let r = CornerRadius::same(8);
        let w = &mut v.widgets;
        w.noninteractive.bg_stroke = Stroke::new(1.0, self.border);
        w.noninteractive.fg_stroke = Stroke::new(1.0, self.text);
        w.noninteractive.corner_radius = r;
        for (state, fill) in [
            (&mut w.inactive, self.surface2),
            (&mut w.hovered, mix(self.surface2, self.text, 0.08)),
            (&mut w.active, mix(self.surface2, self.text, 0.14)),
            (&mut w.open, self.surface2),
        ] {
            state.bg_fill = fill;
            state.weak_bg_fill = fill;
            state.corner_radius = r;
            // Same stroke width in every state: egui shrinks the padding by the stroke width,
            // so a border that only appears on hover would resize (and jitter) the widget.
            state.bg_stroke = Stroke::new(1.0, Color32::TRANSPARENT);
            state.fg_stroke = Stroke::new(1.0, self.text);
            state.expansion = 0.0;
        }
        // Text colors: normal for labels, brighter "strong" (used for **bold** in Markdown).
        w.noninteractive.fg_stroke = Stroke::new(1.0, self.text);
        w.inactive.fg_stroke = Stroke::new(1.0, self.text);
        w.active.fg_stroke = Stroke::new(1.0, if self.dark { Color32::WHITE } else { Color32::BLACK });
        w.hovered.bg_stroke = Stroke::new(1.0, self.border);
        w.active.bg_stroke = Stroke::new(1.0, self.accent);
        ctx.set_visuals(v);

        ctx.global_style_mut(|s| {
            s.spacing.item_spacing = Vec2::new(8.0, 6.0);
            s.spacing.button_padding = Vec2::new(10.0, 5.0);
            s.spacing.interact_size.y = 28.0;
            s.spacing.scroll.bar_width = 6.0;
            s.spacing.scroll.floating = true;
            s.text_styles = [
                (TextStyle::Small, FontId::proportional(12.0)),
                (TextStyle::Body, FontId::proportional(14.5)),
                (TextStyle::Button, FontId::proportional(14.0)),
                (TextStyle::Heading, FontId::new(17.0, FontFamily::Name("bold".into()))),
                (TextStyle::Monospace, FontId::monospace(13.0)),
            ]
            .into();
        });
    }
}

fn font_file(pattern: &str) -> Option<Vec<u8>> {
    let out = std::process::Command::new("fc-match").args(["-f", "%{file}", pattern]).output().ok()?;
    std::fs::read(String::from_utf8(out.stdout).ok()?.trim()).ok()
}

/// System UI fonts (Noto Sans on Plasma) with egui's bundled fonts as fallback for symbols.
pub fn install_fonts(ctx: &egui::Context) {
    static FONTS: OnceLock<[Option<Vec<u8>>; 3]> = OnceLock::new();
    let fonts = FONTS.get_or_init(|| [font_file("sans-serif"), font_file("sans-serif:bold"), font_file("monospace")]);
    let mut defs = FontDefinitions::default();
    let fallback = defs.families[&FontFamily::Proportional].clone();
    for (name, bytes, family) in [
        ("sys-sans", &fonts[0], FontFamily::Proportional),
        ("sys-bold", &fonts[1], FontFamily::Name("bold".into())),
        ("sys-mono", &fonts[2], FontFamily::Monospace),
    ] {
        let list = defs.families.entry(family).or_insert_with(|| fallback.clone());
        if let Some(bytes) = bytes {
            defs.font_data.insert(name.into(), Arc::new(FontData::from_owned(bytes.clone())));
            list.insert(0, name.into());
        }
    }
    ctx.set_fonts(defs);
    ctx.add_font_provider(Arc::new(system_fallback));
}

/// Finds a system font (via fontconfig) for characters the UI fonts lack: emoji, symbols, CJK, …
/// egui calls this at most once per character, so fonts are only loaded when actually needed.
fn system_fallback(req: &egui::epaint::text::FallbackRequest<'_>) -> Option<egui::epaint::text::FontInsert> {
    use egui::epaint::text::{FontInsert, FontPriority, InsertFontFamily};
    let ch = req.cluster.chars().next()?;
    // Color (bitmap) emoji fonts can't be rasterized by egui, so ask for outline fonts only.
    let out = std::process::Command::new("fc-list")
        .args([format!(":charset={:x}:color=false:scalable=true", ch as u32).as_str(), "file"])
        .output()
        .ok()?;
    let text = String::from_utf8(out.stdout).ok()?;
    let mut files: Vec<&str> = text.lines().map(|l| l.trim().trim_end_matches(':')).filter(|f| !f.is_empty()).collect();
    files.sort_by_key(|f| (!f.contains("Noto"), !f.contains("DejaVu"), f.len()));
    let file = files.into_iter().find(|f| f.ends_with(".ttf") || f.ends_with(".otf"))?;
    let bytes = std::fs::read(file).ok()?;
    Some(FontInsert {
        name: file.to_string(),
        data: FontData::from_owned(bytes),
        families: vec![InsertFontFamily { family: req.family.clone(), priority: FontPriority::Lowest }],
    })
}

/// The LoKI icon (accent squircle with a white sparkle), rendered at `size`×`size` as RGBA.
pub fn icon_rgba(size: u32) -> Vec<u8> {
    let (c1, c2) = ([0x8b, 0x7c, 0xff], [0x3d, 0xae, 0xe9]);
    let mut out = Vec::with_capacity((size * size * 4) as usize);
    const SS: u32 = 4;
    for py in 0..size {
        for px in 0..size {
            let (mut cover, mut star) = (0.0f32, 0.0f32);
            for sy in 0..SS {
                for sx in 0..SS {
                    let x = ((px * SS + sx) as f32 + 0.5) / (size * SS) as f32 * 2.0 - 1.0;
                    let y = ((py * SS + sy) as f32 + 0.5) / (size * SS) as f32 * 2.0 - 1.0;
                    if x.abs().powi(4) + y.abs().powi(4) <= 0.94f32.powi(4) {
                        cover += 1.0;
                        // astroid sparkle, slightly offset small one in the corner
                        let big = (x.abs().powf(2.0 / 3.0) + (y + 0.04).abs().powf(2.0 / 3.0)) <= 0.62f32.powf(2.0 / 3.0);
                        let (dx, dy) = (x - 0.45, y + 0.45);
                        let small = (dx.abs().powf(2.0 / 3.0) + dy.abs().powf(2.0 / 3.0)) <= 0.2f32.powf(2.0 / 3.0);
                        if big || small {
                            star += 1.0;
                        }
                    }
                }
            }
            let n = (SS * SS) as f32;
            let t = (px + py) as f32 / (2 * size) as f32;
            let bg: Vec<f32> = (0..3).map(|i| c1[i] as f32 + (c2[i] as f32 - c1[i] as f32) * t).collect();
            let s = if cover > 0.0 { star / cover } else { 0.0 };
            for b in &bg {
                out.push((b + (255.0 - b) * s).round() as u8);
            }
            out.push((cover / n * 255.0).round() as u8);
        }
    }
    out
}

/// ARGB32 (network byte order) for the StatusNotifierItem pixmap.
pub fn icon_argb(size: u32) -> Vec<u8> {
    icon_rgba(size).as_chunks::<4>().0.iter().flat_map(|p| [p[3], p[0], p[1], p[2]]).collect()
}
