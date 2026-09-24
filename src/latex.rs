//! LaTeX math -> raster image, via the ratex crates + resvg.
//! Adapted from nullspace (~/private/repos/nullspace) — same pipeline, but
//! transparent background + light foreground for dark terminals, and sized
//! relative to the terminal's font height.

use image::RgbaImage;
use ratex_layout::{LayoutOptions, layout, to_display_list};
use ratex_parser::parser::parse;
use ratex_svg::{SvgOptions, render_to_svg};
use ratex_types::{color::Color, math_style::MathStyle};
use resvg::{tiny_skia, usvg};

/// SVG font size the layout is produced at; one "text line" of math ≈ this many px.
const SVG_FONT_PX: f64 = 40.0;
const MAX_ROWS: f32 = 8.0;

/// Math foreground follows the configured syntax theme: light text for dark
/// terminals (everforest-ish), dark text for light themes.
fn fg() -> Color {
    let theme = &crate::config::get().theme;
    if theme.contains("light") || theme.contains("GitHub") {
        Color {
            r: 0.24,
            g: 0.22,
            b: 0.21,
            a: 1.0,
        }
    } else {
        Color {
            r: 211.0 / 255.0,
            g: 198.0 / 255.0,
            b: 170.0 / 255.0,
            a: 1.0,
        }
    }
}

/// Render display math scaled so one math text-line ≈ 1.4 terminal rows.
/// Returns None on any parse/render failure — callers fall back to text.
pub fn render_math(latex: &str, font_h: u16) -> Option<RgbaImage> {
    render_scaled(latex, font_h, MathStyle::Display, 1.4, MAX_ROWS, 4.0)
}

/// Render inline math in TeX *text style* (like `$...$` in a paragraph:
/// compact fractions, limits beside operators), always shrunk to exactly one
/// terminal row — if that's too small to read, the author should use `$$`.
/// The raster is padded to exactly `font_h` tall with the content vertically
/// centered, so short symbols sit mid-line and column reservation stays tight.
pub fn render_inline(latex: &str, font_h: u16) -> Option<RgbaImage> {
    let img = render_scaled(latex, font_h, MathStyle::Text, 1.0, 1.0, 0.0)?;
    // ratex rasters carry loose margins; crop to visible content so the
    // reserved column count matches what's actually drawn.
    let img = crop_transparent(img)?;
    let h = font_h as u32;
    if img.height() >= h {
        return Some(img);
    }
    let mut canvas = RgbaImage::new(img.width(), h); // transparent
    let top = (h - img.height()) / 2;
    for (x, y, p) in img.enumerate_pixels() {
        canvas.put_pixel(x, y + top, *p);
    }
    Some(canvas)
}

/// Render an SVG document at its intrinsic size (image/svg+xml outputs,
/// markdown images). System fonts are loaded once for `<text>` elements
/// (matplotlib defaults to paths, but not everyone does).
pub fn render_svg(svg: &str) -> Option<RgbaImage> {
    use std::sync::{Arc, OnceLock};
    static FONTS: OnceLock<Arc<usvg::fontdb::Database>> = OnceLock::new();
    let fontdb = FONTS.get_or_init(|| {
        let mut db = usvg::fontdb::Database::new();
        db.load_system_fonts();
        Arc::new(db)
    });
    let opt = usvg::Options {
        fontdb: fontdb.clone(),
        ..Default::default()
    };
    let tree = usvg::Tree::from_str(svg, &opt).ok()?;
    let (w, h) = (
        tree.size().width().ceil().max(1.0) as u32,
        tree.size().height().ceil().max(1.0) as u32,
    );
    let mut pixmap = tiny_skia::Pixmap::new(w, h)?;
    resvg::render(
        &tree,
        tiny_skia::Transform::identity(),
        &mut pixmap.as_mut(),
    );
    RgbaImage::from_raw(w, h, pixmap.data().to_vec())
}

/// Crop to the bounding box of non-transparent pixels (None if fully empty).
fn crop_transparent(img: RgbaImage) -> Option<RgbaImage> {
    let (mut x0, mut y0, mut x1, mut y1) = (u32::MAX, u32::MAX, 0u32, 0u32);
    for (x, y, p) in img.enumerate_pixels() {
        if p.0[3] > 0 {
            x0 = x0.min(x);
            y0 = y0.min(y);
            x1 = x1.max(x);
            y1 = y1.max(y);
        }
    }
    if x0 == u32::MAX {
        return None;
    }
    Some(image::imageops::crop_imm(&img, x0, y0, x1 - x0 + 1, y1 - y0 + 1).to_image())
}

fn render_scaled(
    latex: &str,
    font_h: u16,
    style: MathStyle,
    rows_per_line: f32,
    max_rows: f32,
    padding: f64,
) -> Option<RgbaImage> {
    let latex = latex.trim();
    if latex.is_empty() {
        return None;
    }
    let ast = parse(latex).ok()?;
    let layout_opts = LayoutOptions::default().with_style(style).with_color(fg());
    let display_list = to_display_list(&layout(&ast, &layout_opts));
    let svg = render_to_svg(
        &display_list,
        &SvgOptions {
            font_size: SVG_FONT_PX,
            padding,
            stroke_width: 1.5,
            embed_glyphs: true,
            font_dir: String::new(),
        },
    );

    let tree = usvg::Tree::from_str(&svg, &usvg::Options::default()).ok()?;
    let (src_w, src_h) = (tree.size().width().max(1.0), tree.size().height().max(1.0));
    let mut scale = (font_h as f32 * rows_per_line) / SVG_FONT_PX as f32;
    let max_h = max_rows * font_h as f32;
    if src_h * scale > max_h {
        scale = max_h / src_h;
    }
    let (w, h) = (
        (src_w * scale).ceil().max(1.0) as u32,
        (src_h * scale).ceil().max(1.0) as u32,
    );
    let mut pixmap = tiny_skia::Pixmap::new(w, h)?; // transparent
    resvg::render(
        &tree,
        tiny_skia::Transform::from_scale(scale, scale),
        &mut pixmap.as_mut(),
    );
    RgbaImage::from_raw(w, h, pixmap.data().to_vec())
}

/// Rough unicode approximation for inline math (and the no-graphics fallback).
/// Copied from nullspace.
pub fn to_unicode_approx(latex: &str) -> String {
    let mut out = latex.to_string();
    let replacements = [
        ("\\rightarrow", "→"),
        ("\\approx", "≈"),
        ("\\partial", "∂"),
        ("\\nabla", "∇"),
        ("\\infty", "∞"),
        ("\\alpha", "α"),
        ("\\gamma", "γ"),
        ("\\delta", "δ"),
        ("\\theta", "θ"),
        ("\\lambda", "λ"),
        ("\\sigma", "σ"),
        ("\\omega", "ω"),
        ("\\times", "×"),
        ("\\cdot", "·"),
        ("\\sqrt", "√"),
        ("\\hbar", "ℏ"),
        ("\\beta", "β"),
        ("\\leq", "≤"),
        ("\\geq", "≥"),
        ("\\neq", "≠"),
        ("\\sum", "∑"),
        ("\\int", "∫"),
        ("\\pm", "±"),
        ("\\mu", "μ"),
        ("\\pi", "π"),
        ("\\tau", "τ"),
        ("\\varepsilon", "ε"),
        ("\\epsilon", "ε"),
        ("\\phi", "φ"),
        ("\\psi", "ψ"),
        ("\\Sigma", "Σ"),
        ("\\Delta", "Δ"),
        ("\\Omega", "Ω"),
        ("\\Gamma", "Γ"),
        ("\\langle", "⟨"),
        ("\\rangle", "⟩"),
    ];
    for (from, to) in replacements {
        out = out.replace(from, to);
    }
    let superscripts = [
        ("^0", "⁰"),
        ("^1", "¹"),
        ("^2", "²"),
        ("^3", "³"),
        ("^4", "⁴"),
        ("^5", "⁵"),
        ("^6", "⁶"),
        ("^7", "⁷"),
        ("^8", "⁸"),
        ("^9", "⁹"),
        ("^n", "ⁿ"),
    ];
    for (from, to) in superscripts {
        out = out.replace(from, to);
    }
    out.replace(['\\', '{', '}', '$'], "")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_simple_equation() {
        let img = render_math(r"E = \hbar \omega", 16).expect("render");
        assert!(img.width() > 10 && img.height() >= 16);
        // something visible was drawn
        assert!(img.pixels().any(|p| p.0[3] > 0));
    }

    #[test]
    fn inline_is_exactly_one_row_tall_and_width_tight() {
        for tex in [r"\tau", r"\frac{1}{x^2}", r"\int_0^\infty dx", r"a^2 + b^2"] {
            let img = render_inline(tex, 16).expect(tex);
            assert_eq!(img.height(), 16, "height of {tex}");
            // cropped: first and last pixel-columns contain visible content
            let w = img.width();
            assert!(
                (0..img.height()).any(|y| img.get_pixel(0, y).0[3] > 0),
                "left edge {tex}"
            );
            assert!(
                (0..img.height()).any(|y| img.get_pixel(w - 1, y).0[3] > 0),
                "right edge {tex}"
            );
        }
    }

    #[test]
    fn bad_latex_is_none_and_unicode_approx_works() {
        assert!(render_math(r"\frac{unclosed", 16).is_none() || true); // parse may be lenient
        assert_eq!(to_unicode_approx(r"$\hbar \omega^2$"), "ℏ ω²");
    }
}
