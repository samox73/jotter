//! Plots in the terminal's own colours. Matplotlib & co. draw black on white;
//! on a dark terminal that's a glaring white box. At display time (never in
//! the notebook, so saved outputs and savefig files stay standard) we map
//! white to the terminal background and black to its foreground, keeping each
//! colour's hue, like zathura's recolor-keephue.
//!
//! The terminal's colours come from an OSC 10/11 query at startup; terminals
//! that don't answer get no recolouring.

use image::{DynamicImage, RgbaImage};
use std::sync::OnceLock;

/// Terminal foreground and background, sRGB 0..1.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Palette {
    pub fg: [f32; 3],
    pub bg: [f32; 3],
}

static PALETTE: OnceLock<Option<Palette>> = OnceLock::new();

/// Set once at startup (None: terminal didn't answer, or recolouring is off).
pub fn init(palette: Option<Palette>) {
    let _ = PALETTE.set(palette);
}

fn palette() -> Option<Palette> {
    PALETTE.get().copied().flatten()
}

/// Ask the terminal for its colours: OSC 10 (fg) and 11 (bg), then DA1 as a
/// sentinel, since every terminal answers DA1, so we stop waiting as soon as
/// it arrives instead of running into the timeout. Needs raw mode, and must
/// run before anything else reads stdin.
pub fn query() -> Option<Palette> {
    use std::io::Write;
    let mut out = std::io::stdout();
    out.write_all(b"\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[c")
        .ok()?;
    out.flush().ok()?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(300);
    let mut buf = Vec::new();
    while !has_da1(&buf) {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        if left.is_zero() {
            break;
        }
        let mut fd = libc::pollfd {
            fd: libc::STDIN_FILENO,
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: one valid pollfd; read into a stack buffer of the given size
        if unsafe { libc::poll(&mut fd, 1, left.as_millis() as i32) } <= 0 {
            break;
        }
        let mut chunk = [0u8; 256];
        let n = unsafe { libc::read(libc::STDIN_FILENO, chunk.as_mut_ptr().cast(), chunk.len()) };
        if n <= 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n as usize]);
    }
    parse_reply(&buf)
}

/// DA1 reply: `ESC [ ? … c`.
fn has_da1(buf: &[u8]) -> bool {
    buf.windows(3)
        .position(|w| w == b"\x1b[?")
        .is_some_and(|i| buf[i..].contains(&b'c'))
}

/// Both colours from a reply buffer, e.g.
/// `ESC ] 11 ; rgb:2b2b/3030/3b3b ESC \`. Components have 1–4 hex digits.
fn parse_reply(buf: &[u8]) -> Option<Palette> {
    let text = String::from_utf8_lossy(buf);
    let color = |code: &str| -> Option<[f32; 3]> {
        let start = text.find(&format!("\x1b]{code};rgb:"))? + code.len() + 7;
        let spec: String = text[start..]
            .chars()
            .take_while(|c| c.is_ascii_hexdigit() || *c == '/')
            .collect();
        let mut rgb = [0.0; 3];
        let mut parts = spec.split('/');
        for c in &mut rgb {
            let p = parts.next().filter(|p| (1..=4).contains(&p.len()))?;
            let max = (1u32 << (4 * p.len())) - 1;
            *c = u32::from_str_radix(p, 16).ok()? as f32 / max as f32;
        }
        Some(rgb)
    };
    Some(Palette {
        fg: color("10")?,
        bg: color("11")?,
    })
}

/// Recolour `img` if it looks like a plot on a white background and the
/// terminal's colours are known; anything else comes back unchanged.
pub fn apply(img: DynamicImage) -> DynamicImage {
    match palette() {
        Some(p) if looks_like_plot(&img) => DynamicImage::ImageRgba8(recolor(img.into_rgba8(), p)),
        _ => img,
    }
}

/// Plots have large white areas and no transparency. Photos rarely have a
/// third of their pixels near-white; a transparent figure was styled for
/// dark backgrounds by its author already.
fn looks_like_plot(img: &DynamicImage) -> bool {
    let rgba = img.to_rgba8();
    let n = rgba.pixels().len().max(1);
    let (mut white, mut clear) = (0, 0);
    for p in rgba.pixels() {
        let [r, g, b, a] = p.0;
        if a < 250 {
            clear += 1;
        } else if r >= 240 && g >= 240 && b >= 240 {
            white += 1;
        }
    }
    clear * 100 < n && white * 3 >= n
}

/// White → background, black → foreground, greys in between; a coloured
/// pixel keeps its offset from its own grey, so hues survive (a blue line
/// stays blue, just lighter on a dark terminal).
fn recolor(mut img: RgbaImage, p: Palette) -> RgbaImage {
    for px in img.pixels_mut() {
        let c = [px.0[0], px.0[1], px.0[2]].map(|v| v as f32 / 255.0);
        let y = 0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2]; // Rec. 709 luma
        let channels = px.0.iter_mut().zip(c).zip(p.bg.iter().zip(p.fg));
        for ((out, ci), (bg, fg)) in channels {
            let grey = bg + (fg - bg) * (1.0 - y);
            *out = ((grey + ci - y).clamp(0.0, 1.0) * 255.0).round() as u8;
        }
    }
    img
}

#[cfg(test)]
mod tests {
    use super::*;

    const OCEAN: Palette = Palette {
        fg: [192.0 / 255.0, 197.0 / 255.0, 206.0 / 255.0],
        bg: [43.0 / 255.0, 48.0 / 255.0, 59.0 / 255.0],
    };

    #[test]
    fn parses_osc_replies_in_any_digit_count_and_terminator() {
        let reply = b"\x1b]10;rgb:c0c0/c5c5/cece\x1b\\\x1b]11;rgb:2b/30/3b\x07\x1b[?62;22c";
        assert!(has_da1(reply));
        let p = parse_reply(reply).unwrap();
        assert!((p.fg[0] - 192.0 / 255.0).abs() < 1e-3);
        assert!((p.bg[2] - 59.0 / 255.0).abs() < 1e-3);
        // a terminal that only answers DA1: no palette, no recolouring
        assert_eq!(parse_reply(b"\x1b[?62c"), None);
    }

    #[test]
    fn white_becomes_background_black_foreground_hue_kept() {
        let img = RgbaImage::from_fn(3, 1, |x, _| {
            image::Rgba(match x {
                0 => [255, 255, 255, 255],
                1 => [0, 0, 0, 255],
                _ => [31, 119, 180, 255], // matplotlib's C0 blue
            })
        });
        let out = recolor(img, OCEAN);
        assert_eq!(out.get_pixel(0, 0).0, [43, 48, 59, 255]);
        assert_eq!(out.get_pixel(1, 0).0, [192, 197, 206, 255]);
        let [r, g, b, _] = out.get_pixel(2, 0).0;
        assert!(b > g && g > r, "still blue: {r} {g} {b}");
    }

    #[test]
    fn only_opaque_white_background_images_count_as_plots() {
        let plot = RgbaImage::from_fn(10, 10, |x, _| {
            image::Rgba(if x < 8 { [255; 4] } else { [0, 0, 0, 255] })
        });
        let photo = RgbaImage::from_pixel(10, 10, image::Rgba([120, 90, 60, 255]));
        let styled = RgbaImage::from_pixel(10, 10, image::Rgba([0, 0, 0, 0]));
        assert!(looks_like_plot(&DynamicImage::ImageRgba8(plot)));
        assert!(!looks_like_plot(&DynamicImage::ImageRgba8(photo)));
        assert!(!looks_like_plot(&DynamicImage::ImageRgba8(styled)));
    }
}
