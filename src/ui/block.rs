//! Cached cell content: inline images, wrapped lines and the per-cell block.

use ratatui::{
    style::Style,
    text::{Line, Span},
};
use ratatui_image::sliced::SlicedProtocol;
use unicode_width::UnicodeWidthChar;

use super::{GUTTER, max_output_rows};

pub struct InlineImage {
    /// Line offset within the cell's block (source + outputs).
    pub(super) line: usize,
    /// Column offset within the line (inline math sits mid-text).
    pub(super) col: u16,
    pub(super) cols: u16,
    pub(super) rows: u16,
    /// Pixel size as transmitted (for the `D` report).
    pub(super) px: (u32, u32),
    /// None in a layout pass: sized, never decoded or encoded.
    pub(super) proto: Option<SlicedProtocol>,
    pub(super) kitty: Option<KittyImage>,
}

/// A kitty image's terminal-side copy, deleted when the image is dropped
/// (cell rebuilt or evicted); kitty would otherwise keep the pixels until
/// quit, and every re-render transmits a fresh copy.
pub(super) struct KittyImage {
    pub(super) id: u32,
    pub(super) tmux: bool,
}

/// Deletes of dropped kitty images, held back until the next frame has
/// placed their replacements: deleting at drop time would blank an updating
/// image (a `display` handle, a re-run cell) until the redraw.
static KITTY_DELETES: std::sync::Mutex<String> = std::sync::Mutex::new(String::new());

/// A kitty image was created since the last frame, so that frame carries
/// its transmit (see the synchronized update in the main loop).
pub(super) static KITTY_TRANSMIT: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

pub fn take_kitty_transmit() -> bool {
    KITTY_TRANSMIT.swap(false, std::sync::atomic::Ordering::Relaxed)
}

impl Drop for KittyImage {
    fn drop(&mut self) {
        let seq = format!("\x1b_Ga=d,d=I,i={},q=2\x1b\\", self.id);
        let seq = if self.tmux {
            format!("\x1bPtmux;{}\x1b\\", seq.replace('\x1b', "\x1b\x1b"))
        } else {
            seq
        };
        if let Ok(mut deletes) = KITTY_DELETES.lock() {
            deletes.push_str(&seq);
        }
    }
}

/// Write the deletes queued since the last call; call after a draw.
pub fn flush_kitty_deletes(out: &mut impl std::io::Write) -> std::io::Result<()> {
    let deletes = KITTY_DELETES.lock().map(|mut d| std::mem::take(&mut *d)).unwrap_or_default();
    if !deletes.is_empty() {
        out.write_all(deletes.as_bytes())?;
        out.flush()?;
    }
    Ok(())
}

/// Where a wrapped screen row comes from.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RowOrigin {
    /// Logical line index.
    pub line: usize,
    /// Char index of the row's first char within that line.
    pub col: usize,
    /// Display column of the row's first char within that line.
    pub x: usize,
}

/// Logical lines soft-wrapped to a width: the screen rows.
#[derive(Default)]
pub struct Wrapped {
    /// Width this was wrapped for (0 = unwrapped).
    width: u16,
    pub rows: Vec<Line<'static>>,
    pub origin: Vec<RowOrigin>,
    /// Logical line -> its first row (len = lines + 1; last = rows.len()).
    pub(super) first_row: Vec<usize>,
}

impl Wrapped {
    /// Row + in-row display column of display column `x` on logical `line`.
    pub(super) fn locate(&self, line: usize, x: usize) -> (usize, usize) {
        let (a, b) = (self.first_row[line], self.first_row[line + 1].max(self.first_row[line] + 1));
        let r = (a..b.min(self.origin.len())).rev().find(|&r| self.origin[r].x <= x).unwrap_or(a);
        (r, x - self.origin.get(r).map_or(0, |o| o.x))
    }

    /// Row holding char `col` of logical `line`, and that row's first char.
    pub(super) fn row_of_char(&self, line: usize, col: usize) -> (usize, usize) {
        let (a, b) = (self.first_row[line], self.first_row[line + 1]);
        let r = (a..b).rev().find(|&r| self.origin[r].col <= col).unwrap_or(a);
        (r, self.origin.get(r).map_or(0, |o| o.col))
    }
}

/// Soft-wrap `lines` to `width` display columns (0 = no wrapping). Greedy
/// word wrap that breaks only at ASCII spaces — NBSP never breaks, which
/// keeps inline-math reservations whole — and hard-breaks words longer than
/// a row. Spaces at a break stay at the end of the upper row (clipped if they
/// overflow), so char columns map 1:1 onto rows.
pub fn wrap_lines(lines: &[Line<'static>], width: u16) -> Wrapped {
    let w = width as usize;
    let mut out = Wrapped { width, ..Default::default() };
    for (li, line) in lines.iter().enumerate() {
        out.first_row.push(out.rows.len());
        if w == 0 || line.width() <= w {
            out.rows.push(line.clone());
            out.origin.push(RowOrigin { line: li, col: 0, x: 0 });
            continue;
        }
        let chars: Vec<(char, Style)> =
            line.spans.iter().flat_map(|s| s.content.chars().map(move |c| (c, s.style))).collect();
        let cw: Vec<usize> = chars.iter().map(|(c, _)| c.width().unwrap_or(0)).collect();
        let mut starts = vec![0usize];
        let (mut row_w, mut i) = (0usize, 0usize);
        while i < chars.len() {
            if chars[i].0 == ' ' {
                row_w += cw[i];
                i += 1;
                continue;
            }
            let j = i + chars[i..].iter().position(|(c, _)| *c == ' ').unwrap_or(chars.len() - i);
            let word: usize = cw[i..j].iter().sum();
            if row_w + word <= w {
                row_w += word;
            } else if row_w > 0 && word <= w {
                starts.push(i);
                row_w = word;
            } else {
                for (k, &c) in cw.iter().enumerate().take(j).skip(i) {
                    if row_w + c > w && row_w > 0 {
                        starts.push(k);
                        row_w = 0;
                    }
                    row_w += c;
                }
            }
            i = j;
        }
        starts.push(chars.len());
        for pair in starts.windows(2) {
            let (a, b) = (pair[0], pair[1]);
            let mut spans: Vec<Span<'static>> = Vec::new();
            for &(c, st) in &chars[a..b] {
                match spans.last_mut() {
                    Some(sp) if sp.style == st => sp.content.to_mut().push(c),
                    _ => spans.push(Span::styled(c.to_string(), st)),
                }
            }
            out.rows.push(Line::from(spans).style(line.style));
            out.origin.push(RowOrigin { line: li, col: a, x: cw[..a].iter().sum() });
        }
    }
    out.first_row.push(out.rows.len());
    out
}

/// A cell's displayed form. Only cells near the viewport hold their content
/// (`rendered`); the rest keep just their row counts, from a layout pass
/// that skips highlighting and image decoding (math rows are estimated), or
/// from content that was rendered and evicted again.
pub struct CellBlock {
    /// Logical (unwrapped) lines; images and `src_map` index into these.
    pub(super) lines: Vec<Line<'static>>,
    /// How many of `lines` are source (the rest are outputs).
    pub(super) src_lines: usize,
    pub(super) images: Vec<InlineImage>,
    /// Rendered source row -> raw source line (markdown cells; empty = 1:1).
    pub(super) src_map: Vec<usize>,
    /// First output row shown when the output is clipped to the viewport
    /// budget. usize::MAX = follow the tail (streaming output stays live).
    pub out_scroll: usize,
    pub collapsed: bool,
    /// Output images shown at native size instead of the height cap (`Z`).
    pub full_images: bool,
    /// Wall time of the cell's last completed execution (session-only).
    pub elapsed: Option<std::time::Duration>,
    /// `lines` wrapped to the body width: what is actually displayed.
    pub(super) wrap: Wrapped,
    /// Content (lines, images, wrap) is present; else only `rows` is.
    pub(super) rendered: bool,
    /// Displayed (source rows, output rows, output image rows).
    pub(super) rows: (usize, usize, usize),
    /// Body width `rows` holds for (None = never laid out).
    pub(super) laid_for: Option<u16>,
    /// Height in the last frame drawn, for scroll anchoring.
    pub(super) last_h: Option<usize>,
    /// Frame it was last near the screen (render window), for eviction.
    pub(super) seen: u64,
}

impl CellBlock {
    /// Not yet laid out: `Rendered::relayout` measures it.
    pub(super) fn empty() -> Self {
        CellBlock {
            lines: Vec::new(),
            src_lines: 0,
            images: Vec::new(),
            src_map: Vec::new(),
            out_scroll: usize::MAX,
            collapsed: false,
            full_images: false,
            elapsed: None,
            wrap: Wrapped::default(),
            rendered: false,
            rows: (0, 0, 0),
            laid_for: None,
            last_h: None,
            seen: 0,
        }
    }

    /// Drop the content, keeping its row counts.
    /// Rough memory held by the rendered content: image pixels (RGBA) plus
    /// a share per line. Only compared against the eviction budget.
    pub(super) fn cost(&self) -> usize {
        let px: usize = self.images.iter().map(|i| i.px.0 as usize * i.px.1 as usize * 4).sum();
        px + self.lines.len() * 256
    }

    pub(super) fn evict(&mut self) {
        self.lines = Vec::new();
        self.images = Vec::new();
        self.src_map = Vec::new();
        self.wrap = Wrapped::default();
        self.rendered = false;
    }

    /// Re-wrap for body `width`: source rows lose the gutter's columns,
    /// outputs use the full width. No-op when already wrapped for it.
    pub(super) fn rewrap(&mut self, width: u16) {
        if !self.rendered {
            return;
        }
        if self.wrap.width != width || self.wrap.first_row.len() != self.lines.len() + 1 {
            let src_w = if width == 0 { 0 } else { width.saturating_sub(GUTTER).max(1) };
            let (src, out) = self.lines.split_at(self.src_lines);
            let mut w = wrap_lines(src, src_w);
            let o = wrap_lines(out, width);
            let base_row = w.rows.len();
            w.first_row.pop(); // the source end sentinel == first output row
            w.first_row.extend(o.first_row.iter().map(|r| r + base_row));
            w.rows.extend(o.rows);
            w.origin.extend(
                o.origin.into_iter().map(|r| RowOrigin { line: r.line + self.src_lines, ..r }),
            );
            w.width = width;
            self.wrap = w;
        }
        let src = self.wrap.first_row[self.src_lines];
        let image_rows =
            self.images.iter().filter(|i| i.line >= self.src_lines).map(|i| i.rows as usize).sum(); // placeholder lines are empty: one row each, never wrapped
        self.rows = (src, self.wrap.rows.len() - src, image_rows);
        self.laid_for = Some(width);
    }

    /// Displayed source rows.
    pub fn src_rows(&self) -> usize {
        self.rows.0
    }

    /// Displayed output rows (the viewport works in these).
    pub(super) fn out_len(&self) -> usize {
        self.rows.1
    }

    /// Top-left of an image in (block row, display column).
    pub(super) fn image_pos(&self, img: &InlineImage) -> (usize, usize) {
        self.wrap.locate(img.line, img.col as usize)
    }

    /// Output-viewport budget: the row cap is for *text* spam — images are
    /// display units and extend the budget instead of being windowed away
    /// (a default matplotlib figure must never arrive cropped).
    fn out_cap(&self) -> usize {
        max_output_rows() + self.rows.2
    }

    fn max_out_scroll(&self) -> usize {
        self.out_len().saturating_sub(self.out_cap())
    }

    /// The clamped first visible output row.
    pub fn win_start(&self) -> usize {
        self.out_scroll.min(self.max_out_scroll())
    }

    /// Output window as displayed: (first row, rows shown, footer line?).
    pub(super) fn window(&self) -> (usize, usize, bool) {
        let len = self.out_len();
        if self.collapsed && len > 0 {
            (0, 0, true)
        } else if len > self.out_cap() {
            (self.win_start(), self.out_cap(), true)
        } else {
            (0, len, false)
        }
    }

    /// Scroll the output viewport; scrolling to the very end re-pins to tail.
    pub fn scroll_output(&mut self, delta: isize) {
        let max = self.max_out_scroll();
        let cur = self.win_start();
        let new = cur.saturating_add_signed(delta).min(max);
        self.out_scroll = if new == max { usize::MAX } else { new };
    }

    /// Whether the output viewport can scroll (wheel-over-output routing).
    pub fn output_scrollable(&self) -> bool {
        !self.collapsed && self.max_out_scroll() > 0
    }

    /// Render/viewport state for the `D` debug overlay.
    pub fn debug_summary(&self) -> String {
        let follow = if self.out_scroll == usize::MAX {
            "follow".to_string()
        } else {
            self.out_scroll.to_string()
        };
        format!(
            "src {} rows · out {} rows (cap {}) · out_scroll {follow} → win {} · images {}{} · wrap {}{}{}{}",
            self.src_rows(),
            self.out_len(),
            self.out_cap(),
            self.win_start(),
            self.images.len(),
            self.images
                .iter()
                .map(|im| format!(
                    " [{}x{} px → {}x{} cells @ line {} col {}]",
                    im.px.0, im.px.1, im.cols, im.rows, im.line, im.col
                ))
                .collect::<String>(),
            self.wrap.width,
            if self.rendered { "" } else { " · not rendered" },
            if self.collapsed { " · collapsed" } else { "" },
            if self.full_images { " · native-size" } else { "" },
        )
    }
}
