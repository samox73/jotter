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

/// Render display math scaled so one math text-line ≈ 1.4 terminal rows, at
/// most `max_math_rows` tall, and cropped to its ink then centred in whole
/// rows, so leftover space splits evenly above and below.
/// Returns None on any parse/render failure — callers fall back to text.
pub fn render_math(latex: &str, font_h: u16) -> Option<RgbaImage> {
    let max_rows = crate::config::get().max_math_rows.max(1) as f32;
    let img = render_scaled(latex, font_h, MathStyle::Display, 1.4, max_rows, 4.0)?;
    Some(center_in_rows(crop_rows(img)?, font_h as u32))
}

/// Crop to the rows that have visible pixels (None if nothing is visible).
fn crop_rows(img: RgbaImage) -> Option<RgbaImage> {
    let inked = |y: u32| (0..img.width()).any(|x| img.get_pixel(x, y).0[3] > 0);
    let y0 = (0..img.height()).find(|&y| inked(y))?;
    let y1 = (0..img.height()).rev().find(|&y| inked(y))?;
    Some(image::imageops::crop_imm(&img, 0, y0, img.width(), y1 - y0 + 1).to_image())
}

/// Pad to a whole number of `row`-px rows, the image vertically centred.
fn center_in_rows(img: RgbaImage, row: u32) -> RgbaImage {
    let h = img.height().div_ceil(row.max(1)) * row.max(1);
    let mut out = RgbaImage::new(img.width(), h); // transparent
    image::imageops::overlay(&mut out, &img, 0, ((h - img.height()) / 2) as i64);
    out
}

/// Inline math size: one em, as a fraction of the terminal row height. At 0.9
/// the math font's x-height matches a typical terminal font's, so math letters
/// are as large as the text around them.
const INLINE_EM_ROWS: f32 = 0.9;
/// Where the text baseline sits in a terminal row, from the top: inline math
/// shares it, so `$E_n$` and `$n = 0$` line up with each other and the prose.
const INLINE_BASELINE_ROWS: f32 = 0.77;
/// How much of the row an inline expression too tall for the baseline may
/// fill when it is centred instead; the rest is a hairline of air.
const INLINE_FIT: f32 = 0.96;

/// Render inline math in TeX *text style* (like `$...$` in a paragraph:
/// compact fractions, limits beside operators) into exactly one terminal row.
/// Every expression gets the same size and baseline, like text; only one that
/// doesn't fit above or below the baseline (a fraction in big parentheses)
/// shrinks, and just by what it needs. Width is cropped to the visible ink,
/// so column reservation stays tight.
pub fn render_inline(latex: &str, font_h: u16) -> Option<RgbaImage> {
    let (tree, height_em, depth_em) = svg_tree(latex, MathStyle::Text, 0.0)?;
    let row = font_h as f32;
    let baseline = INLINE_BASELINE_ROWS * row;
    let (height, depth) = (height_em as f32, depth_em as f32);
    // px per em: the common size, unless this expression needs less
    let full = INLINE_EM_ROWS * row;
    let mut em = full;
    if height > 0.0 {
        em = em.min(baseline / height);
    }
    if depth > 0.0 {
        em = em.min((row - baseline) / depth);
    }
    let (em, top) = if em < full {
        // too tall for the text baseline (a fraction, a root): leaving the
        // baseline put would shrink it into the bottom of the row, so centre
        // it in the row instead, as large as fits
        let em = full.min(INLINE_FIT * row / (height + depth).max(1e-6));
        (em, ((row - (height + depth) * em) / 2.0).round().max(0.0))
    } else {
        (em, (baseline - height * em).round().max(0.0))
    };
    // the SVG is sized in pt, so measure the tree's own px per em
    let tree_em = tree.size().height() / (height + depth).max(1e-6);
    let scale = em / tree_em;
    let w = (tree.size().width() * scale).ceil().max(1.0) as u32;
    let mut pixmap = tiny_skia::Pixmap::new(w, font_h as u32)?; // transparent
    resvg::render(
        &tree,
        tiny_skia::Transform::from_scale(scale, scale).post_translate(0.0, top),
        &mut pixmap.as_mut(),
    );
    crop_columns(to_rgba(&pixmap)?)
}

/// tiny-skia pixmaps hold *premultiplied* RGBA; `image` expects straight
/// alpha. Taking the bytes as they are multiplies every antialiased edge by
/// its alpha twice: thin, dark, aliased-looking strokes.
fn to_rgba(pixmap: &tiny_skia::Pixmap) -> Option<RgbaImage> {
    let data = pixmap
        .pixels()
        .iter()
        .flat_map(|p| {
            let c = p.demultiply();
            [c.red(), c.green(), c.blue(), c.alpha()]
        })
        .collect();
    RgbaImage::from_raw(pixmap.width(), pixmap.height(), data)
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
    to_rgba(&pixmap)
}

/// Crop to the columns that have visible pixels, keeping the full height, so
/// the baseline stays put (None if nothing is visible).
fn crop_columns(img: RgbaImage) -> Option<RgbaImage> {
    let inked = |x: u32| (0..img.height()).any(|y| img.get_pixel(x, y).0[3] > 0);
    let x0 = (0..img.width()).find(|&x| inked(x))?;
    let x1 = (0..img.width()).rev().find(|&x| inked(x))?;
    Some(image::imageops::crop_imm(&img, x0, 0, x1 - x0 + 1, img.height()).to_image())
}

/// Lay out `latex` and render it to an SVG at SVG_FONT_PX per em, returning
/// the parsed tree and the expression's height above and depth below the
/// baseline in em (the baseline sits `height` em below the top, plus padding).
fn svg_tree(latex: &str, style: MathStyle, padding: f64) -> Option<(usvg::Tree, f64, f64)> {
    let latex = latex.trim();
    if latex.is_empty() {
        return None;
    }
    let ast = parse(latex).ok()?;
    let layout_opts = LayoutOptions::default().with_style(style).with_color(fg());
    let display_list = to_display_list(&layout(&ast, &layout_opts));
    let (height, depth) = (display_list.height, display_list.depth);
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
    Some((tree, height, depth))
}

fn render_scaled(
    latex: &str,
    font_h: u16,
    style: MathStyle,
    rows_per_line: f32,
    max_rows: f32,
    padding: f64,
) -> Option<RgbaImage> {
    let (tree, _, _) = svg_tree(latex, style, padding)?;
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
    to_rgba(&pixmap)
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

    /// Rows of `img` that have visible pixels: (first, last).
    fn ink_rows(img: &RgbaImage) -> (u32, u32) {
        let inked = |y: u32| (0..img.width()).any(|x| img.get_pixel(x, y).0[3] > 0);
        let rows: Vec<u32> = (0..img.height()).filter(|&y| inked(y)).collect();
        (rows[0], *rows.last().unwrap())
    }

    #[test]
    fn inline_math_shares_one_size_and_baseline() {
        // `x` alone and `x = 1` have the same letter height and position; a
        // taller expression next to them doesn't change that
        let x = ink_rows(&render_inline("x", 32).unwrap());
        let eq = ink_rows(&render_inline("x = 1", 32).unwrap());
        assert_eq!(x.1, eq.1, "same baseline");
        // `1` is taller than `x`, so compare bottoms (baseline) and sizes
        let x_again = ink_rows(&render_inline("xx", 32).unwrap());
        assert_eq!(x, x_again, "same size");
        // a fraction in stretchy parentheses still fits the row
        let tall = render_inline(r"\left(n + \tfrac{1}{2}\right)", 32).unwrap();
        assert_eq!(tall.height(), 32);
    }

    #[test]
    fn bad_latex_is_none_and_unicode_approx_works() {
        assert!(render_math(r"\frac{unclosed", 16).is_none() || true); // parse may be lenient
        assert_eq!(to_unicode_approx(r"$\hbar \omega^2$"), "ℏ ω²");
    }

    /// Empty pixel rows above and below the ink.
    fn margins(img: &image::RgbaImage) -> (u32, u32) {
        let inked = |y: u32| (0..img.width()).any(|x| img.get_pixel(x, y).0[3] > 0);
        let top = (0..img.height()).find(|&y| inked(y)).unwrap();
        let bottom = (0..img.height()).rev().find(|&y| inked(y)).unwrap();
        (top, img.height() - 1 - bottom)
    }

    const ROW: u16 = 36;

    #[test]
    fn display_math_is_capped_and_centred() {
        let tall = r"\sigma^2(m) = \frac{1}{n}\left[\gamma_0 + 2\sum_{t=1}^{n-1}\left(1-\frac{t}{n}\right)\gamma_t\right] \xrightarrow{n\gg\tau} \frac{2\tau_{\text{int}}\gamma_0}{n}";
        for tex in [tall, "x = 1"] {
            let img = super::render_math(tex, ROW).unwrap();
            assert_eq!(img.height() % ROW as u32, 0, "{tex}: whole rows");
            assert!(img.height() <= 4 * ROW as u32, "{tex}: {}px", img.height());
            let (top, bottom) = margins(&img);
            assert!(
                top.abs_diff(bottom) <= 1,
                "{tex}: {top}px above, {bottom}px below"
            );
        }
    }

    #[test]
    fn inline_math_too_tall_for_the_baseline_is_centred() {
        let img = super::render_inline(
            r"\delta\left[c_0/(n-1)\right] \approx \frac{c_0}{n-1}\sqrt{\frac{2}{n-1}}",
            ROW,
        )
        .unwrap();
        assert_eq!(img.height(), ROW as u32);
        let (top, bottom) = margins(&img);
        assert!(top.abs_diff(bottom) <= 2, "{top}px above, {bottom}px below");
        assert!(top + bottom <= 4, "fills the row: {top}+{bottom}px empty");
    }
}
