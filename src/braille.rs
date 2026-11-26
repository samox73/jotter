//! Plots as Braille text, for terminals without a graphics protocol (or by
//! choice, `--braille`): every kernel's PNG/SVG output becomes characters
//! from the Unicode Braille block, 2x4 dots per cell, in the colours of the
//! plot's lines. drawille-style, but from the rendered image, so it works
//! for any kernel (Python, R, Giac, Octave, ...) without its help.

use image::{DynamicImage, GenericImageView, Rgba};
use ratatui::{
    style::{Color, Style},
    text::{Line, Span},
};

/// Dots per cell.
const DOT_W: u32 = 2;
const DOT_H: u32 = 4;
/// A dot is on when its pixels differ from the background by more than this
/// (0..=1 of the largest colour distance in the image): low enough for thin
/// anti-aliased lines, high enough to skip JPEG noise and faint grid lines.
const INK: f32 = 0.28;
/// Widest a plot is drawn, in cells: wider is hard to read as Braille.
const MAX_COLS: u16 = 100;

/// Cells a picture of `px` pixels takes when drawn in a body `width` cells
/// wide, at most `max_rows` rows (unless `full`). Dots are close to square
/// in a usual 1:2 terminal cell, so the picture keeps its shape.
pub fn size((w, h): (u32, u32), width: u16, max_rows: u16, full: bool) -> (u16, u16) {
    let avail = if width == 0 { 80 } else { width.saturating_sub(2) };
    let mut cols = avail.clamp(8, MAX_COLS).min(w.div_ceil(DOT_W).max(1) as u16);
    let rows_for = |cols: u16| {
        let dots_h = (cols as f64 * DOT_W as f64 * h as f64 / w.max(1) as f64).round() as u32;
        (dots_h.div_ceil(DOT_H).max(1)) as u16
    };
    let mut rows = rows_for(cols);
    if !full && rows > max_rows.max(1) {
        // too tall: narrow it until it fits, keeping the shape
        cols = ((cols as u32 * max_rows.max(1) as u32) / rows as u32).max(4) as u16;
        rows = rows_for(cols).min(max_rows.max(1));
    }
    (cols, rows)
}

/// The picture as `rows` lines of `cols` Braille characters.
pub fn render(img: &DynamicImage, cols: u16, rows: u16) -> Vec<Line<'static>> {
    let (iw, ih) = img.dimensions();
    if iw == 0 || ih == 0 || cols == 0 || rows == 0 {
        return Vec::new();
    }
    let rgba = img.to_rgba8();
    let bg = background(&rgba);
    let (dw, dh) = (cols as u32 * DOT_W, rows as u32 * DOT_H);

    // per dot: the strongest ink under it (max, not mean: a one-pixel line
    // in a 6x6 pixel area must still light its dot), and that pixel's colour
    let mut strength = vec![0f32; (dw * dh) as usize];
    let mut colour = vec![[0u8; 3]; (dw * dh) as usize];
    let mut peak = 0f32;
    for dy in 0..dh {
        let (y0, y1) = span(dy, dh, ih);
        for dx in 0..dw {
            let (x0, x1) = span(dx, dw, iw);
            let (mut best, mut best_c) = (0f32, [0u8; 3]);
            for y in y0..y1 {
                for x in x0..x1 {
                    let p = *rgba.get_pixel(x, y);
                    let d = distance(p, bg);
                    if d > best {
                        (best, best_c) = (d, [p[0], p[1], p[2]]);
                    }
                }
            }
            let i = (dy * dw + dx) as usize;
            (strength[i], colour[i]) = (best, best_c);
            peak = peak.max(best);
        }
    }
    let threshold = INK * peak.max(f32::EPSILON);

    (0..rows as u32)
        .map(|row| {
            let spans: Vec<Span<'static>> = (0..cols as u32)
                .map(|col| {
                    let (mut bits, mut sum, mut n) = (0u8, [0u32; 3], 0u32);
                    for (bit, (ox, oy)) in DOTS.iter().enumerate() {
                        let (x, y) = (col * DOT_W + ox, row * DOT_H + oy);
                        let i = (y * dw + x) as usize;
                        if strength[i] > threshold {
                            bits |= 1 << bit;
                            for (s, c) in sum.iter_mut().zip(colour[i]) {
                                *s += c as u32;
                            }
                            n += 1;
                        }
                    }
                    let ch = char::from_u32(0x2800 + bits as u32).unwrap_or(' ');
                    let style = if n == 0 {
                        Style::default()
                    } else {
                        ink_style([sum[0] / n, sum[1] / n, sum[2] / n])
                    };
                    Span::styled(ch.to_string(), style)
                })
                .collect();
            Line::from(merge(spans))
        })
        .collect()
}

/// Bit of each dot in a Braille character, as (x, y) within the 2x4 cell
/// (Unicode's order: 1 2 3 down the left, 4 5 6 down the right, then 7 8).
const DOTS: [(u32, u32); 8] = [(0, 0), (0, 1), (0, 2), (1, 0), (1, 1), (1, 2), (0, 3), (1, 3)];

/// Source pixels [start, end) covered by dot `i` of `n` across `len` pixels:
/// at least one, so a picture smaller than the dot grid still shows.
fn span(i: u32, n: u32, len: u32) -> (u32, u32) {
    let a = (i as u64 * len as u64 / n as u64) as u32;
    let b = ((i as u64 + 1) * len as u64 / n as u64) as u32;
    (a.min(len - 1), b.max(a + 1).min(len))
}

/// The plot's background: the most common colour along the border
/// (transparent counts as itself, so a transparent figure's ink is anything
/// opaque).
fn background(img: &image::RgbaImage) -> Rgba<u8> {
    let (w, h) = img.dimensions();
    let mut counts: std::collections::HashMap<[u8; 4], u32> = std::collections::HashMap::new();
    let mut add = |x: u32, y: u32| {
        let p = img.get_pixel(x, y).0;
        // coarse buckets: anti-aliasing and JPEG noise vote together
        let key =
            if p[3] < 16 { [0, 0, 0, 0] } else { [p[0] & 0xf0, p[1] & 0xf0, p[2] & 0xf0, 255] };
        *counts.entry(key).or_default() += 1;
    };
    for x in 0..w {
        add(x, 0);
        add(x, h - 1);
    }
    for y in 0..h {
        add(0, y);
        add(w - 1, y);
    }
    let key = counts.into_iter().max_by_key(|&(_, n)| n).map(|(k, _)| k).unwrap_or([255; 4]);
    if key[3] == 0 { Rgba([0, 0, 0, 0]) } else { Rgba([key[0] | 8, key[1] | 8, key[2] | 8, 255]) }
}

/// How far a pixel is from the background, 0..=~1.7 (RGB distance, with
/// transparency as distance from a transparent background).
fn distance(p: Rgba<u8>, bg: Rgba<u8>) -> f32 {
    let a = p[3] as f32 / 255.0;
    if bg[3] == 0 {
        return a; // transparent figure: any opaque pixel is ink
    }
    let d: f32 = (0..3)
        .map(|i| {
            let c = p[i] as f32 / 255.0 * a + bg[i] as f32 / 255.0 * (1.0 - a);
            let b = bg[i] as f32 / 255.0;
            (c - b) * (c - b)
        })
        .sum();
    d.sqrt()
}

/// Coloured ink keeps its colour (one series per colour stays apart);
/// black, white and grey use the terminal's own foreground, readable on
/// light and dark themes alike.
fn ink_style([r, g, b]: [u32; 3]) -> Style {
    let (mx, mn) = (r.max(g).max(b), r.min(g).min(b));
    if mx - mn < 40 {
        Style::default()
    } else {
        Style::default().fg(Color::Rgb(r as u8, g as u8, b as u8))
    }
}

/// Adjacent characters of the same style as one span.
fn merge(spans: Vec<Span<'static>>) -> Vec<Span<'static>> {
    let mut out: Vec<Span<'static>> = Vec::new();
    for s in spans {
        match out.last_mut() {
            Some(last) if last.style == s.style => last.content.to_mut().push_str(&s.content),
            _ => out.push(s),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(lines: &[Line]) -> Vec<String> {
        lines.iter().map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect()).collect()
    }

    #[test]
    fn diagonal_line_on_white() {
        let mut img = image::RgbaImage::from_pixel(40, 40, Rgba([255, 255, 255, 255]));
        for i in 0..40 {
            img.put_pixel(i, 39 - i, Rgba([0, 0, 0, 255]));
        }
        let lines = render(&DynamicImage::ImageRgba8(img), 10, 5);
        let t = text(&lines);
        assert_eq!(t.len(), 5);
        assert!(t.iter().all(|l| l.chars().count() == 10));
        // bottom-left and top-right corners have ink, the opposite corners don't
        assert_ne!(t[4].chars().next(), Some('\u{2800}'));
        assert_ne!(t[0].chars().last(), Some('\u{2800}'));
        assert_eq!(t[0].chars().next(), Some('\u{2800}'));
        assert_eq!(t[4].chars().last(), Some('\u{2800}'));
    }

    #[test]
    fn one_pixel_line_survives_downscaling() {
        let mut img = image::RgbaImage::from_pixel(600, 400, Rgba([255, 255, 255, 255]));
        for x in 0..600 {
            img.put_pixel(x, 200, Rgba([30, 30, 30, 255]));
        }
        let lines = render(&DynamicImage::ImageRgba8(img), 30, 10);
        let lit: usize =
            text(&lines).iter().map(|l| l.chars().filter(|&c| c != '\u{2800}').count()).sum();
        assert_eq!(lit, 30, "the line lights one character per column");
    }

    #[test]
    fn colours_kept_greys_use_terminal_colour() {
        let mut img = image::RgbaImage::from_pixel(20, 8, Rgba([255, 255, 255, 255]));
        for x in 0..10 {
            img.put_pixel(x, 1, Rgba([31, 119, 180, 255])); // matplotlib blue
        }
        for x in 10..20 {
            img.put_pixel(x, 1, Rgba([0, 0, 0, 255]));
        }
        let lines = render(&DynamicImage::ImageRgba8(img), 10, 2);
        let first = &lines[0].spans;
        assert_eq!(first[0].style.fg, Some(Color::Rgb(31, 119, 180)));
        assert!(
            first
                .iter()
                .any(|s| s.style.fg.is_none() && s.content.chars().any(|c| c != '\u{2800}'))
        );
    }

    #[test]
    fn transparent_background() {
        let mut img = image::RgbaImage::from_pixel(8, 8, Rgba([0, 0, 0, 0]));
        img.put_pixel(0, 0, Rgba([200, 0, 0, 255]));
        let t = text(&render(&DynamicImage::ImageRgba8(img), 4, 2));
        assert_ne!(t[0].chars().next(), Some('\u{2800}'));
        assert_eq!(t[1].chars().last(), Some('\u{2800}'));
    }

    #[test]
    fn sizing_keeps_shape_and_caps_rows() {
        // 640x480 in a wide body: 100 cols max, 4:3 picture -> 200x150 dots -> 38 rows
        assert_eq!(size((640, 480), 200, 100, false), (100, 38));
        // capped at 20 rows: narrowed to keep the shape
        let (c, r) = size((640, 480), 200, 20, false);
        assert!(r <= 20 && (c as f64 * 2.0 / (r as f64 * 4.0) - 640.0 / 480.0).abs() < 0.15);
        // full ignores the cap
        assert_eq!(size((640, 480), 200, 20, true), (100, 38));
        // narrow body
        assert_eq!(size((640, 480), 42, 100, false).0, 40);
    }
}
