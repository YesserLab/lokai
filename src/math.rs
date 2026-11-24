//! LaTeX math in answers: `$…$` inline and `$$…$$` display formulas, typeset by latex-rust
//! (TeX layout, STIX Two Math) into SVG in the theme's text color and drawn as images.

use eframe::egui::{self, Color32, Rect, RichText, Sense, Vec2, pos2, vec2};
use latex_rust::{Color, Dim, MathFont, MathStyle, SvgOptions};
use std::borrow::Cow;
use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::OnceLock;

/// A typeset formula: SVG bytes for egui, its size and its baseline (from the top), in points.
#[derive(Clone)]
struct Formula {
    uri: String,
    bytes: egui::load::Bytes,
    size: Vec2,
    baseline: f32,
}

type Key = (String, bool, [u8; 4], u32);

/// Formulas typeset so far (`None`: latex-rust can't render it), so each is typeset only once.
#[derive(Default)]
pub struct MathCache(RefCell<HashMap<Key, Option<Formula>>>);

fn font() -> Option<&'static MathFont> {
    static FONT: OnceLock<Option<MathFont>> = OnceLock::new();
    FONT.get_or_init(|| MathFont::stix_two_math().ok()).as_ref()
}

fn points(d: &Dim) -> f32 {
    d.to_dec_string().parse().unwrap_or(0.0)
}

fn typeset(tex: &str, inline: bool, color: Color32, size: f32) -> Option<Formula> {
    let font = font()?;
    let em = Dim::ratio((size * 100.0).round() as i64, 100);
    let node = latex_rust::parse(tex).ok()?;
    let style = if inline { MathStyle::Text } else { MathStyle::Display };
    let tree = latex_rust::layout_with_em_size_pt(&node, font, style, &em).ok()?;
    let mut options = SvgOptions::new();
    options.font_size_pt = em;
    options.color = Color::rgb(color.r(), color.g(), color.b());
    options.display = !inline;
    let image = latex_rust::render_svg_with_metrics(&tree, font, &options).ok()?;
    let size = vec2(points(&image.width_pt), points(&image.height_pt));
    if size.x <= 0.0 || size.y <= 0.0 {
        return None;
    }
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    image.svg.hash(&mut h);
    Some(Formula {
        uri: format!("bytes://lokai-math/{:x}.svg", h.finish()),
        bytes: image.svg.into_bytes().into(),
        size,
        baseline: points(&image.baseline_pt),
    })
}

impl MathCache {
    fn get(&self, tex: &str, inline: bool, color: Color32, size: f32) -> Option<Formula> {
        let key = (tex.to_string(), inline, color.to_array(), size.to_bits());
        self.0.borrow_mut().entry(key).or_insert_with(|| typeset(tex, inline, color, size)).clone()
    }

    /// Draws a formula (the callback for `CommonMarkViewer::render_math_fn`). Inline formulas sit
    /// on the text baseline; display formulas are centered on their own line. LaTeX that can't be
    /// typeset is shown as source.
    pub fn show(&self, ui: &mut egui::Ui, tex: &str, inline: bool, color: Color32, fallback: Color32) {
        let body = egui::TextStyle::Body.resolve(ui.style());
        let size = if inline { body.size } else { body.size * 1.2 };
        let Some(f) = self.get(tex.trim(), inline, color, size) else {
            let source = if inline { format!("${tex}$") } else { format!("$${tex}$$") };
            ui.label(RichText::new(source).monospace().color(fallback));
            return;
        };
        let image = egui::Image::from_bytes(f.uri, f.bytes);
        if inline {
            // Rows are centered: put the formula's baseline where a text label's baseline is.
            let galley = ui.painter().layout_no_wrap("x".into(), body, color);
            let row = galley.size().y;
            let text_baseline = galley.rows.first().and_then(|r| r.row.glyphs.first().map(|g| r.pos.y + g.pos.y)).unwrap_or(row * 0.75);
            let below_center = text_baseline - row / 2.0; // baseline offset from the row's center
            let half = (f.baseline - below_center).max(f.size.y - f.baseline + below_center).max(row / 2.0);
            let (rect, _) = ui.allocate_exact_size(vec2(f.size.x, 2.0 * half), Sense::hover());
            let top = rect.center().y + below_center - f.baseline;
            image.paint_at(ui, Rect::from_min_size(pos2(rect.left(), top), f.size));
        } else {
            let width = ui.max_rect().width();
            let scale = (width / f.size.x).min(1.0);
            let (rect, _) = ui.allocate_exact_size(vec2(width, f.size.y * scale + 8.0), Sense::hover());
            image.paint_at(ui, Rect::from_center_size(rect.center(), f.size * scale));
        }
    }
}

/// Rewrites `\(…\)` and `\[…\]` (which Markdown doesn't know) as `$…$` and `$$…$$`, outside
/// inline code. Spaces just inside the delimiters are dropped: `$ x $` isn't math in Markdown.
pub fn normalize_delimiters(md: &str) -> Cow<'_, str> {
    if !md.contains("\\(") && !md.contains("\\[") {
        return Cow::Borrowed(md);
    }
    let b = md.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(md.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'`' => {
                // Copy a code span verbatim: a run of n backticks up to the next run of exactly n.
                let n = b[i..].iter().take_while(|&&c| c == b'`').count();
                let mut j = i + n;
                let end = loop {
                    match b[j..].iter().position(|&c| c == b'`') {
                        None => break None,
                        Some(p) => {
                            let start = j + p;
                            let m = b[start..].iter().take_while(|&&c| c == b'`').count();
                            if m == n {
                                break Some(start + m);
                            }
                            j = start + m;
                        }
                    }
                };
                let end = end.unwrap_or(i + n);
                out.extend_from_slice(&b[i..end]);
                i = end;
            }
            b'\\' if i + 1 < b.len() => {
                match b[i + 1] {
                    b'(' | b'[' => {
                        out.extend_from_slice(if b[i + 1] == b'(' { b"$" } else { b"$$" });
                        i += 2;
                        while i < b.len() && b[i] == b' ' {
                            i += 1;
                        }
                        continue;
                    }
                    b')' | b']' => {
                        while out.last() == Some(&b' ') {
                            out.pop();
                        }
                        out.extend_from_slice(if b[i + 1] == b')' { b"$" } else { b"$$" });
                    }
                    // An escaped backslash (or a TeX line break) is never a delimiter.
                    _ => out.extend_from_slice(&b[i..i + 2]),
                }
                i += 2;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    // Only ASCII sequences were replaced, so the bytes are still UTF-8.
    Cow::Owned(String::from_utf8(out).unwrap_or_else(|_| md.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delimiters() {
        assert_eq!(normalize_delimiters(r"plain $x$ text"), r"plain $x$ text");
        assert_eq!(normalize_delimiters(r"area \( \pi r^2 \) and"), r"area $\pi r^2$ and");
        assert_eq!(normalize_delimiters("\\[\n\\int_0^1 x\\,dx\n\\]"), "$$\n\\int_0^1 x\\,dx\n$$");
        assert_eq!(normalize_delimiters(r"`code \(x\)` but \(y\)"), r"`code \(x\)` but $y$");
        assert_eq!(normalize_delimiters(r"\begin{cases} a \\ b \end{cases} \(z\)"), r"\begin{cases} a \\ b \end{cases} $z$");
        assert_eq!(normalize_delimiters("é \\(α\\) ü"), "é $α$ ü");
    }

    #[test]
    fn typesets_formulas() {
        let f = typeset(r"\frac{a}{b} + \sqrt{x^2+1}", true, Color32::WHITE, 14.5).expect("typeset");
        assert!(f.size.x > 20.0 && f.size.y > 14.0 && f.baseline > 0.0 && f.baseline < f.size.y);
        assert!(std::str::from_utf8(&f.bytes).unwrap().contains("#ffffff"));
        let display = typeset(r"\sum_{n=1}^{\infty} \frac{1}{n^2} = \frac{\pi^2}{6}", false, Color32::WHITE, 17.0).unwrap();
        assert!(display.size.y > f.size.y);
        assert!(typeset(r"\frac{", true, Color32::WHITE, 14.5).is_none());
    }
}
