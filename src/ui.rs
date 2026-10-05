//! Rendering. Cell bodies (syntax-highlighted source + outputs) are cached in
//! `Rendered` and rebuilt per cell when outputs arrive; the cell being edited
//! renders live from the editor buffer. Images render as kitty/sixel graphics
//! into blank placeholder lines reserved in the text flow.

use crate::app::App;
use crate::notebook::{Cell, Notebook, join_multiline};
use crate::nvim::ModeKind;
use ansi_to_tui::IntoText;
use base64::Engine;
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::Paragraph;
use ratatui_image::picker::{Capability, Picker, ProtocolType};
use ratatui_image::protocol::kitty::Kitty;
use ratatui_image::sliced::{SignedPosition, SlicedImage, SlicedProtocol};
use serde_json::Value;
use syntect::easy::HighlightLines;
use syntect::highlighting::{Theme, ThemeSet};
use syntect::parsing::SyntaxSet;
use syntect::util::LinesWithEndings;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

const OUTPUT_STYLE: Style = Style::new().fg(Color::Gray);
const ERROR_STYLE: Style = Style::new().fg(Color::Red);

/// Images taller than this many rows are downscaled once (config knob).
fn max_image_rows() -> u16 {
    crate::config::get().max_image_rows
}

/// Output rows displayed per cell; longer outputs get a scrollable viewport.
fn max_output_rows() -> usize {
    crate::config::get().max_output_rows
}

pub struct InlineImage {
    /// Line offset within the cell's block (source + outputs).
    line: usize,
    /// Column offset within the line (inline math sits mid-text).
    col: u16,
    cols: u16,
    rows: u16,
    /// Pixel size as transmitted (for the `D` report).
    px: (u32, u32),
    /// None in a layout pass: sized, never decoded or encoded.
    proto: Option<SlicedProtocol>,
    kitty: Option<KittyImage>,
}

/// A kitty image's terminal-side copy, deleted when the image is dropped
/// (cell rebuilt or evicted); kitty would otherwise keep the pixels until
/// quit, and every re-render transmits a fresh copy.
struct KittyImage {
    id: u32,
    tmux: bool,
}

impl Drop for KittyImage {
    fn drop(&mut self) {
        use std::io::Write;
        let seq = format!("\x1b_Ga=d,d=I,i={},q=2\x1b\\", self.id);
        let seq = if self.tmux {
            format!("\x1bPtmux;{}\x1b\\", seq.replace('\x1b', "\x1b\x1b"))
        } else {
            seq
        };
        let _ = std::io::stdout().write_all(seq.as_bytes());
    }
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
    first_row: Vec<usize>,
}

impl Wrapped {
    /// Row + in-row display column of display column `x` on logical `line`.
    fn locate(&self, line: usize, x: usize) -> (usize, usize) {
        let (a, b) = (
            self.first_row[line],
            self.first_row[line + 1].max(self.first_row[line] + 1),
        );
        let r = (a..b.min(self.origin.len()))
            .rev()
            .find(|&r| self.origin[r].x <= x)
            .unwrap_or(a);
        (r, x - self.origin.get(r).map_or(0, |o| o.x))
    }

    /// Row holding char `col` of logical `line`, and that row's first char.
    fn row_of_char(&self, line: usize, col: usize) -> (usize, usize) {
        let (a, b) = (self.first_row[line], self.first_row[line + 1]);
        let r = (a..b)
            .rev()
            .find(|&r| self.origin[r].col <= col)
            .unwrap_or(a);
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
    let mut out = Wrapped {
        width,
        ..Default::default()
    };
    for (li, line) in lines.iter().enumerate() {
        out.first_row.push(out.rows.len());
        if w == 0 || line.width() <= w {
            out.rows.push(line.clone());
            out.origin.push(RowOrigin {
                line: li,
                col: 0,
                x: 0,
            });
            continue;
        }
        let chars: Vec<(char, Style)> = line
            .spans
            .iter()
            .flat_map(|s| s.content.chars().map(move |c| (c, s.style)))
            .collect();
        let cw: Vec<usize> = chars.iter().map(|(c, _)| c.width().unwrap_or(0)).collect();
        let mut starts = vec![0usize];
        let (mut row_w, mut i) = (0usize, 0usize);
        while i < chars.len() {
            if chars[i].0 == ' ' {
                row_w += cw[i];
                i += 1;
                continue;
            }
            let j = i + chars[i..]
                .iter()
                .position(|(c, _)| *c == ' ')
                .unwrap_or(chars.len() - i);
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
            out.origin.push(RowOrigin {
                line: li,
                col: a,
                x: cw[..a].iter().sum(),
            });
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
    lines: Vec<Line<'static>>,
    /// How many of `lines` are source (the rest are outputs).
    src_lines: usize,
    images: Vec<InlineImage>,
    /// Rendered source row -> raw source line (markdown cells; empty = 1:1).
    src_map: Vec<usize>,
    /// First output row shown when the output is clipped to the viewport
    /// budget. usize::MAX = follow the tail (streaming output stays live).
    pub out_scroll: usize,
    pub collapsed: bool,
    /// Output images shown at native size instead of the height cap (`Z`).
    pub full_images: bool,
    /// Wall time of the cell's last completed execution (session-only).
    pub elapsed: Option<std::time::Duration>,
    /// `lines` wrapped to the body width: what is actually displayed.
    wrap: Wrapped,
    /// Content (lines, images, wrap) is present; else only `rows` is.
    rendered: bool,
    /// Displayed (source rows, output rows, output image rows).
    rows: (usize, usize, usize),
    /// Body width `rows` holds for (None = never laid out).
    laid_for: Option<u16>,
    /// Height in the last frame drawn, for scroll anchoring.
    last_h: Option<usize>,
    /// Frame it was last near the screen (render window), for eviction.
    seen: u64,
}

impl CellBlock {
    /// Not yet laid out: `Rendered::relayout` measures it.
    fn empty() -> Self {
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
    fn cost(&self) -> usize {
        let px: usize = self
            .images
            .iter()
            .map(|i| i.px.0 as usize * i.px.1 as usize * 4)
            .sum();
        px + self.lines.len() * 256
    }

    fn evict(&mut self) {
        self.lines = Vec::new();
        self.images = Vec::new();
        self.src_map = Vec::new();
        self.wrap = Wrapped::default();
        self.rendered = false;
    }

    /// Re-wrap for body `width`: source rows lose the gutter's columns,
    /// outputs use the full width. No-op when already wrapped for it.
    fn rewrap(&mut self, width: u16) {
        if !self.rendered {
            return;
        }
        if self.wrap.width != width || self.wrap.first_row.len() != self.lines.len() + 1 {
            let src_w = if width == 0 {
                0
            } else {
                width.saturating_sub(GUTTER).max(1)
            };
            let (src, out) = self.lines.split_at(self.src_lines);
            let mut w = wrap_lines(src, src_w);
            let o = wrap_lines(out, width);
            let base_row = w.rows.len();
            w.first_row.pop(); // the source end sentinel == first output row
            w.first_row.extend(o.first_row.iter().map(|r| r + base_row));
            w.rows.extend(o.rows);
            w.origin.extend(o.origin.into_iter().map(|r| RowOrigin {
                line: r.line + self.src_lines,
                ..r
            }));
            w.width = width;
            self.wrap = w;
        }
        let src = self.wrap.first_row[self.src_lines];
        let image_rows = self
            .images
            .iter()
            .filter(|i| i.line >= self.src_lines)
            .map(|i| i.rows as usize)
            .sum(); // placeholder lines are empty: one row each, never wrapped
        self.rows = (src, self.wrap.rows.len() - src, image_rows);
        self.laid_for = Some(width);
    }

    /// Displayed source rows.
    pub fn src_rows(&self) -> usize {
        self.rows.0
    }

    /// Displayed output rows (the viewport works in these).
    fn out_len(&self) -> usize {
        self.rows.1
    }

    /// Top-left of an image in (block row, display column).
    fn image_pos(&self, img: &InlineImage) -> (usize, usize) {
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
    fn window(&self) -> (usize, usize, bool) {
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
            if self.rendered {
                ""
            } else {
                " · not rendered"
            },
            if self.collapsed { " · collapsed" } else { "" },
            if self.full_images {
                " · native-size"
            } else {
                ""
            },
        )
    }
}

/// Rendered content (see `CellBlock::cost`) kept before the least recently
/// seen cells are evicted. A notebook within it renders each cell once.
const RENDER_BUDGET: usize = 64 << 20;

/// Full cell renders, counted for the tests.
#[cfg(test)]
static RENDERS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

pub struct Rendered {
    ps: SyntaxSet,
    theme: Theme,
    picker: Option<Picker>,
    /// Notebook directory: markdown image paths resolve relative to it.
    dir: std::path::PathBuf,
    /// Code-cell language (syntect token: "python", "r", ...), from the
    /// notebook's language_info / kernelspec metadata.
    lang: String,
    /// Body width blocks are wrapped to (0 until the first draw).
    width: u16,
    pub blocks: Vec<CellBlock>,
    /// Frames drawn, for the blocks' `seen`.
    frame: u64,
}

/// Kernel language recorded in the notebook: language_info.name (written by
/// jupyter from the kernel), else kernelspec.language, else python.
pub fn notebook_language(nb: &Notebook) -> String {
    let meta = nb.extra.get("metadata");
    let get = |a: &str, b: &str| meta?.get(a)?.get(b)?.as_str().map(str::to_lowercase);
    get("language_info", "name")
        .or_else(|| get("kernelspec", "language"))
        .unwrap_or_else(|| "python".into())
}

/// Raw cell destined for latex (nbconvert convention: metadata.format mime).
pub fn is_latex_raw(cell: &Cell) -> bool {
    cell.cell_type == "raw"
        && cell
            .extra
            .get("metadata")
            .and_then(|m| m.get("format"))
            .and_then(Value::as_str)
            .is_some_and(|f| f.contains("latex"))
}

impl Rendered {
    pub fn build(nb: &Notebook, picker: Option<Picker>, dir: std::path::PathBuf) -> Self {
        let ps = SyntaxSet::load_defaults_newlines();
        let mut themes = ThemeSet::load_defaults().themes;
        let theme = themes
            .remove(&crate::config::get().theme)
            .or_else(|| themes.remove("base16-ocean.dark"))
            .expect("bundled theme");
        let mut r = Self {
            ps,
            theme,
            picker,
            dir,
            lang: notebook_language(nb),
            width: 0,
            blocks: Vec::new(),
            frame: 0,
        };
        r.blocks = nb.cells.iter().map(|_| CellBlock::empty()).collect();
        r
    }

    /// Lay every block out for `width`: rendered blocks rewrap, the rest get
    /// measured by a layout pass. Only blocks laid out for another width do
    /// work, so steady-state frames pay nothing.
    pub fn relayout(&mut self, width: u16, nb: &Notebook) {
        self.width = width;
        for (i, cell) in nb.cells.iter().enumerate().take(self.blocks.len()) {
            let b = &self.blocks[i];
            if b.laid_for == Some(width) {
                continue;
            }
            if b.rendered {
                self.blocks[i].rewrap(width);
            } else {
                let rows = self.render_cell(cell, b.full_images, true).rows;
                let b = &mut self.blocks[i];
                (b.rows, b.laid_for) = (rows, Some(width));
            }
        }
    }

    /// Give block `idx` its content, if it has none. Returns whether it rendered.
    pub fn ensure(&mut self, idx: usize, cell: &Cell) -> bool {
        if self.blocks[idx].rendered {
            return false;
        }
        #[cfg(test)]
        RENDERS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let new = self.render_cell(cell, self.blocks[idx].full_images, false);
        self.replace(idx, new);
        true
    }

    /// Keep rendered content until it exceeds `RENDER_BUDGET`, then drop the
    /// blocks seen least recently (never those `keep` accepts: the render
    /// window). Distance-based eviction re-rendered every cell on each sweep
    /// through a long notebook (and re-sent every kitty image); with a budget,
    /// a notebook that fits stays rendered once it has been seen.
    pub fn evict(&mut self, keep: impl Fn(usize) -> bool) {
        let mut total: usize = self
            .blocks
            .iter()
            .filter(|b| b.rendered)
            .map(CellBlock::cost)
            .sum();
        if total <= RENDER_BUDGET {
            return;
        }
        let mut old: Vec<usize> = (0..self.blocks.len())
            .filter(|&i| self.blocks[i].rendered && !keep(i))
            .collect();
        old.sort_by_key(|&i| self.blocks[i].seen);
        for i in old {
            if total <= RENDER_BUDGET {
                break;
            }
            total -= self.blocks[i].cost();
            self.blocks[i].evict();
        }
    }

    /// Install `new` at `idx`, keeping the viewport position, image sizing,
    /// timing and anchoring height of the block it replaces.
    fn replace(&mut self, idx: usize, mut new: CellBlock) {
        let old = &self.blocks[idx];
        new.out_scroll = old.out_scroll;
        new.collapsed = old.collapsed;
        new.elapsed = old.elapsed;
        new.full_images = old.full_images;
        new.last_h = old.last_h;
        new.seen = old.seen;
        self.blocks[idx] = new;
    }

    /// Kernel language became known (new notebook): affects later renders.
    pub fn set_language(&mut self, lang: &str) {
        self.lang = lang.to_lowercase();
    }

    /// Highlighting token for a cell's source.
    fn cell_lang(&self, cell: &Cell) -> &str {
        match cell.cell_type.as_str() {
            "code" => &self.lang,
            "markdown" => "md",
            _ => "txt",
        }
    }

    /// File extension for the `E` external-edit temp file of `cell`, so
    /// $EDITOR picks the right filetype ("py", "R", "jl", ...).
    pub fn edit_suffix(&self, cell: &Cell) -> String {
        let token = self.cell_lang(cell);
        self.ps
            .find_syntax_by_token(token)
            .and_then(|s| s.file_extensions.first().cloned())
            .unwrap_or_else(|| match cell.cell_type.as_str() {
                "code" => token.to_string(),
                _ => "txt".into(),
            })
    }

    /// The cell changed: re-render it if it has content (it is on or near
    /// the screen), else only re-measure it.
    pub fn rebuild_cell(&mut self, idx: usize, cell: &Cell) {
        let old = &self.blocks[idx];
        let new = self.render_cell(cell, old.full_images, !old.rendered);
        self.replace(idx, new);
    }

    pub fn insert_cell(&mut self, idx: usize) {
        self.blocks.insert(idx, CellBlock::empty());
    }

    pub fn remove_cell(&mut self, idx: usize) {
        self.blocks.remove(idx);
    }

    pub fn swap_cells(&mut self, a: usize, b: usize) {
        self.blocks.swap(a, b);
    }

    /// Re-render every cell (external reload, font-size change).
    pub fn rebuild_all(&mut self, nb: &Notebook) {
        self.blocks = nb.cells.iter().map(|_| CellBlock::empty()).collect();
    }

    /// Terminal resized. Reflow is free, but a *font-size* change (terminal
    /// zoom) invalidates every image's cell dimensions: detect it from the
    /// pixel size the terminal reports (no stdin round-trip) and rebuild.
    pub fn on_resize(&mut self, nb: &Notebook) {
        let Some(picker) = &self.picker else { return };
        let Ok(ws) = crossterm::terminal::window_size() else {
            return;
        };
        if ws.columns == 0 || ws.rows == 0 || ws.width == 0 || ws.height == 0 {
            return; // terminal doesn't report pixels
        }
        let font = ratatui_image::FontSize::new(ws.width / ws.columns, ws.height / ws.rows);
        let cur = picker.font_size();
        if (font.width, font.height) == (cur.width, cur.height) {
            return;
        }
        let ptype = picker.protocol_type();
        #[allow(deprecated)]
        let mut p = Picker::from_fontsize(font);
        p.set_protocol_type(ptype);
        self.picker = Some(p);
        self.rebuild_all(nb);
    }

    /// Graphics protocol + font cell size, for the `D` debug overlay.
    pub fn graphics_summary(&self) -> String {
        match &self.picker {
            Some(p) => {
                let f = p.font_size();
                format!("{:?}, font {}x{} px", p.protocol_type(), f.width, f.height)
            }
            None => "off".into(),
        }
    }

    /// Syntax-highlight source text (used for cached blocks and the live editor).
    /// `lang` may be a name ("rust", "python") or an extension ("rs", "py").
    pub fn highlight(&self, source: &str, lang: &str) -> Vec<Line<'static>> {
        let syntax = self
            .ps
            .find_syntax_by_token(lang)
            .unwrap_or_else(|| self.ps.find_syntax_plain_text());
        let mut hl = HighlightLines::new(syntax, &self.theme);
        let theme_bg = self.theme.settings.background;
        LinesWithEndings::from(source)
            .map(|line| {
                let spans = hl
                    .highlight_line(line, &self.ps)
                    .unwrap_or_default()
                    .into_iter()
                    .flat_map(|(st, txt)| {
                        let (fg, bg) = (st.foreground, st.background);
                        let mut style = Style::default().fg(Color::Rgb(fg.r, fg.g, fg.b));
                        // the terminal's background stands in for the theme's,
                        // but a token's own (invalid code: background-coloured
                        // text on red) must be kept or its text disappears
                        if Some(bg) != theme_bg {
                            style = style.bg(Color::Rgb(bg.r, bg.g, bg.b));
                        }
                        show_tabs(txt.trim_end_matches('\n'), style)
                    })
                    .collect::<Vec<_>>();
                Line::from(spans)
            })
            .collect()
    }

    /// Source lines as `highlight` splits them, uncoloured: a layout pass
    /// wraps these to the same rows.
    fn plain(source: &str) -> Vec<Line<'static>> {
        LinesWithEndings::from(source)
            .map(|l| Line::from(show_tabs(l.trim_end_matches('\n'), Style::default())))
            .collect()
    }

    /// Render a cell. `est` is the layout pass: only the row counts of the
    /// result are kept, so it skips highlighting and image decoding (sizes
    /// come from image headers) and guesses rendered math's size.
    fn render_cell(&self, cell: &Cell, full_images: bool, est: bool) -> CellBlock {
        let (mut lines, mut images, src_map) = if cell.cell_type == "markdown" {
            self.render_markdown(&cell.source, cell.extra.get("attachments"), est)
        } else if is_latex_raw(cell) {
            // raw cell with metadata.format = text/latex: whole cell is math
            let (mut l, mut im) = (Vec::new(), Vec::new());
            self.push_math(cell.source.trim().trim_matches('$'), &mut l, &mut im, est);
            (l, im, Vec::new())
        } else if est {
            (Self::plain(&cell.source), Vec::new(), Vec::new())
        } else {
            (
                self.highlight(&cell.source, self.cell_lang(cell)),
                Vec::new(),
                Vec::new(),
            )
        };
        let src_lines = lines.len();
        for output in cell.outputs.iter().flatten() {
            if let Some(img) = self
                .try_image(output, full_images, est)
                .or_else(|| self.try_latex(output, est))
            {
                self.push_image(img, &mut lines, &mut images);
            } else if let Some(md) = output.get("data").and_then(|d| d.get("text/markdown")) {
                let (md_lines, md_images, _) = self.render_markdown(&join_multiline(md), None, est);
                let base = lines.len();
                lines.extend(md_lines);
                images.extend(md_images.into_iter().map(|im| InlineImage {
                    line: im.line + base,
                    ..im
                }));
            } else {
                lines.extend(output_lines(output));
            }
        }
        let mut block = CellBlock {
            lines,
            src_lines,
            images,
            src_map,
            out_scroll: usize::MAX,
            collapsed: false,
            full_images,
            rendered: true,
            ..CellBlock::empty()
        };
        block.rewrap(self.width);
        if est {
            block.evict();
        }
        block
    }

    /// Reserve blank lines for `entry` at the current position and record it.
    fn push_image(
        &self,
        entry: InlineImage,
        lines: &mut Vec<Line<'static>>,
        images: &mut Vec<InlineImage>,
    ) {
        let entry = InlineImage {
            line: lines.len(),
            ..entry
        };
        for _ in 0..entry.rows {
            lines.push(Line::raw(""));
        }
        images.push(entry);
    }

    /// Wrap a decoded image into a protocol + terminal-cell dimensions.
    /// Anything taller than max_image_rows is downscaled once, here, with a
    /// real filter; render-time then never rescales (its default filter is
    /// nearest-neighbor, which aliases plot lines badly). SlicedProtocol
    /// transmits once and clips per-row when partially scrolled off.
    fn image_entry(&self, img: image::DynamicImage, full: bool) -> Option<InlineImage> {
        let picker = self.picker.as_ref()?;
        let (w, h) = self.shown_px((img.width(), img.height()), full)?;
        let img = if (w, h) != (img.width(), img.height()) {
            img.resize_exact(w, h, image::imageops::FilterType::Lanczos3)
        } else {
            img
        };
        let mut entry = self.sized_px((w, h))?;
        // kitty: our own id, so the terminal's copy can be deleted on drop
        entry.proto = Some(if picker.protocol_type() == ProtocolType::Kitty {
            use std::sync::atomic::{AtomicU32, Ordering};
            static NEXT: AtomicU32 = AtomicU32::new(1);
            let id = NEXT.fetch_add(1, Ordering::Relaxed);
            let size = ratatui::layout::Size::new(entry.cols, entry.rows);
            let compress = picker
                .capabilities()
                .contains(&Capability::KittyCompression);
            let tmux = picker.tmux_detected();
            let kitty = Kitty::new(img, size, id, tmux, compress).ok()?;
            entry.kitty = Some(KittyImage { id, tmux });
            SlicedProtocol::Kitty(kitty)
        } else {
            SlicedProtocol::new(picker, img, None).ok()?
        });
        Some(entry)
    }

    /// Pixel size an image of `px` is shown at: taller than max_image_rows
    /// is scaled down to it, unless `full`. None without graphics.
    fn shown_px(&self, (w, h): (u32, u32), full: bool) -> Option<(u32, u32)> {
        let fh = self.picker.as_ref()?.font_size().height as u32;
        let max_h = max_image_rows() as u32 * fh;
        Some(if !full && h > max_h {
            ((w as u64 * max_h as u64 / h as u64).max(1) as u32, max_h)
        } else {
            (w, h)
        })
    }

    /// An image of `px` shown pixels, sized in cells (as the protocols round:
    /// up) but not encoded.
    fn sized_px(&self, (w, h): (u32, u32)) -> Option<InlineImage> {
        let font = self.picker.as_ref()?.font_size();
        Some(InlineImage {
            line: 0,
            col: 0,
            px: (w, h),
            cols: (w.div_ceil(font.width.max(1) as u32) as u16).max(1),
            rows: (h.div_ceil(font.height.max(1) as u32) as u16).max(1),
            proto: None,
            kitty: None,
        })
    }

    /// Layout pass: an image whose decoded size would be `px`.
    fn sized(&self, px: (u32, u32), full: bool) -> Option<InlineImage> {
        self.sized_px(self.shown_px(px, full)?)
    }

    /// Layout pass: rendered math, guessed at `cols` x `rows` cells.
    fn sized_math(&self, cols: usize, rows: u16) -> Option<InlineImage> {
        let font = self.picker.as_ref()?.font_size();
        self.sized_px((
            cols.max(1) as u32 * font.width as u32,
            rows as u32 * font.height as u32,
        ))
    }

    /// Pixel size of an output's raster/SVG image, from its header.
    fn output_image_px(&self, output: &Value) -> Option<(u32, u32)> {
        let data = output.get("data")?;
        for mime in ["image/png", "image/jpeg", "image/gif"] {
            let Some(v) = data.get(mime) else { continue };
            return bytes_px(&b64_bytes(v)?);
        }
        crate::latex::svg_px(&join_multiline(data.get("image/svg+xml")?))
    }

    /// Decode an output's raster/SVG image at its native size.
    fn decode_output_image(&self, output: &Value) -> Option<image::DynamicImage> {
        let data = output.get("data")?;
        for mime in ["image/png", "image/jpeg", "image/gif"] {
            let Some(v) = data.get(mime) else { continue };
            return image::load_from_memory(&b64_bytes(v)?)
                .ok()
                .map(crate::recolor::apply);
        }
        let svg = join_multiline(data.get("image/svg+xml")?);
        Some(crate::recolor::apply(image::DynamicImage::ImageRgba8(
            crate::latex::render_svg(&svg)?,
        )))
    }

    /// If the output carries a raster/SVG image and graphics are available,
    /// build a protocol; `full` skips the height cap (native-size toggle).
    fn try_image(&self, output: &Value, full: bool, est: bool) -> Option<InlineImage> {
        self.picker.as_ref()?;
        if est {
            return self.sized(self.output_image_px(output)?, full);
        }
        self.image_entry(self.decode_output_image(output)?, full)
    }

    /// `z`: the cell's first image output, re-decoded at native resolution and
    /// fitted to the screen for a fullscreen overlay. One-off, nothing cached.
    pub fn zoom_image(&self, cell: &Cell, area: Rect) -> Option<SlicedProtocol> {
        let picker = self.picker.as_ref()?;
        let img = cell
            .outputs
            .iter()
            .flatten()
            .find_map(|o| self.decode_output_image(o))?;
        let font = picker.font_size();
        let (max_w, max_h) = (
            (area.width as u32 * font.width as u32).max(1),
            (area.height as u32 * font.height as u32).max(1),
        );
        let img = if img.width() > max_w || img.height() > max_h {
            img.resize(max_w, max_h, image::imageops::FilterType::Lanczos3)
        } else {
            img
        };
        SlicedProtocol::new(picker, img, None).ok()
    }

    /// `Z`: re-render a cell with its output images at native size (or back).
    /// Returns the new state.
    pub fn toggle_full_images(&mut self, idx: usize, cell: &Cell) -> bool {
        let Some(block) = self.blocks.get_mut(idx) else {
            return false;
        };
        block.full_images = !block.full_images;
        let state = block.full_images;
        self.rebuild_cell(idx, cell);
        state
    }

    /// text/latex output (e.g. sympy) -> rendered math image.
    fn try_latex(&self, output: &Value, est: bool) -> Option<InlineImage> {
        let picker = self.picker.as_ref()?;
        let tex = join_multiline(output.get("data")?.get("text/latex")?);
        if est {
            return self.sized_math(1, MATH_ROWS_GUESS.min(crate::config::get().max_math_rows));
        }
        let tex = tex.trim().trim_matches('$').replace("\\displaystyle", "");
        let img = crate::latex::render_math(&tex, picker.font_size().height)?;
        self.image_entry(image::DynamicImage::ImageRgba8(img), false)
    }

    fn push_math(
        &self,
        tex: &str,
        lines: &mut Vec<Line<'static>>,
        images: &mut Vec<InlineImage>,
        est: bool,
    ) {
        let rendered = if est {
            self.sized_math(1, MATH_ROWS_GUESS.min(crate::config::get().max_math_rows))
        } else {
            self.picker
                .as_ref()
                .and_then(|p| crate::latex::render_math(tex, p.font_size().height))
                .and_then(|img| self.image_entry(image::DynamicImage::ImageRgba8(img), false))
        };
        match rendered {
            Some(entry) => self.push_image(entry, lines, images),
            None => {
                // no graphics or parse failure: unicode approximation
                for l in crate::latex::to_unicode_approx(tex).lines() {
                    lines.push(Line::styled(
                        format!("    {l}"),
                        Style::new()
                            .fg(Color::Magenta)
                            .add_modifier(Modifier::ITALIC),
                    ));
                }
            }
        }
    }

    /// Minimal markdown: headings, fenced code, $$math$$, bullets, tables,
    /// links, emphasis, images (attachments + local paths), `code`, $math$.
    /// Returns rendered lines, inline images, and a rendered-line -> source-line
    /// map so mouse clicks land on the right raw line. Fence bodies and table
    /// rows map exactly; math blocks map to their closing `$$` line.
    fn render_markdown(
        &self,
        source: &str,
        attachments: Option<&Value>,
        est: bool,
    ) -> (Vec<Line<'static>>, Vec<InlineImage>, Vec<usize>) {
        let mut lines = Vec::new();
        let mut images = Vec::new();
        let mut src_map: Vec<usize> = Vec::new();
        let pad = |map: &mut Vec<usize>, upto: usize, idx: usize| {
            while map.len() < upto {
                map.push(idx);
            }
        };
        let mut fence: Option<(String, String)> = None;
        let mut math: Option<String> = None;
        let mut table: Vec<String> = Vec::new();
        let mut total = 0;
        for (i, raw) in source.split('\n').enumerate() {
            total = i + 1;
            let t = raw.trim();
            if let Some((lang, buf)) = &mut fence {
                if t.starts_with("```") {
                    let lang = if lang.is_empty() {
                        self.lang.as_str()
                    } else {
                        lang.as_str()
                    };
                    let body = if est {
                        Self::plain(buf)
                    } else {
                        self.highlight(buf, lang)
                    };
                    src_map.extend(i - body.len()..i); // fence body maps 1:1
                    lines.extend(body);
                    fence = None;
                } else {
                    buf.push_str(raw);
                    buf.push('\n');
                }
                continue;
            }
            if let Some(buf) = &mut math {
                if let Some(head) = t.strip_suffix("$$") {
                    buf.push_str(head);
                    let tex = std::mem::take(buf);
                    math = None;
                    self.push_math(&tex, &mut lines, &mut images, est);
                    pad(&mut src_map, lines.len(), i);
                } else {
                    buf.push_str(raw);
                    buf.push('\n');
                }
                continue;
            }
            if t.starts_with('|') {
                table.push(t.to_string());
                continue;
            }
            if !table.is_empty() {
                src_map.extend(i - table.len()..i); // one rendered row per source row
                lines.extend(render_table(&table));
                table.clear();
            }
            if let Some((alt, src)) = parse_md_image(t) {
                match self.md_image(src, attachments, est) {
                    Some(entry) => self.push_image(entry, &mut lines, &mut images),
                    None => lines.push(Line::styled(
                        format!("[image: {}]", if alt.is_empty() { src } else { alt }),
                        OUTPUT_STYLE,
                    )),
                }
                pad(&mut src_map, lines.len(), i);
                continue;
            }
            if let Some(rest) = t.strip_prefix("```") {
                fence = Some((rest.trim().to_string(), String::new()));
                continue;
            }
            if let Some(rest) = t.strip_prefix("$$") {
                match rest.strip_suffix("$$") {
                    Some(inner) if !rest.is_empty() => {
                        self.push_math(inner, &mut lines, &mut images, est)
                    }
                    _ => math = Some(format!("{rest}\n")),
                }
                pad(&mut src_map, lines.len(), i);
                continue;
            }
            if t.starts_with('#') {
                let level = t.chars().take_while(|c| *c == '#').count();
                let text = t.trim_start_matches('#').trim_start().to_string();
                let color = match level {
                    1 => Color::Green,
                    2 => Color::Cyan,
                    _ => Color::Blue,
                };
                let mut style = Style::new().fg(color).add_modifier(Modifier::BOLD);
                if level == 1 {
                    style = style.add_modifier(Modifier::UNDERLINED);
                }
                lines.push(Line::styled(text, style));
                src_map.push(i);
                continue;
            }
            let line_idx = lines.len();
            let line = self.md_line(raw, line_idx, &mut images, est);
            lines.push(line);
            src_map.push(i);
        }
        if !table.is_empty() {
            src_map.extend(total - table.len()..total);
            lines.extend(render_table(&table));
        }
        // unterminated fence/math: show what we buffered, raw
        for buf in [fence.map(|f| f.1), math].into_iter().flatten() {
            lines.extend(buf.lines().map(|l| Line::raw(l.to_string())));
        }
        pad(&mut src_map, lines.len(), total.saturating_sub(1));
        (lines, images, src_map)
    }

    /// Load a markdown image: `attachment:name` from the cell's attachments,
    /// otherwise a path relative to the notebook directory. No network.
    fn md_image(&self, src: &str, attachments: Option<&Value>, est: bool) -> Option<InlineImage> {
        self.picker.as_ref()?;
        if let Some(name) = src.strip_prefix("attachment:") {
            for v in attachments?.get(name)?.as_object()?.values() {
                let Some(bytes) = b64_bytes(v) else { continue };
                if est {
                    if let Some(px) = bytes_px(&bytes) {
                        return self.sized(px, false);
                    }
                } else if let Ok(img) = image::load_from_memory(&bytes) {
                    return self.image_entry(img, false);
                }
            }
            return None;
        }
        if src.starts_with("http://") || src.starts_with("https://") {
            return None; // alt-text fallback
        }
        let path = self.dir.join(src);
        if path
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("svg"))
        {
            let svg = std::fs::read_to_string(path).ok()?;
            if est {
                return self.sized(crate::latex::svg_px(&svg)?, false);
            }
            return self.image_entry(
                image::DynamicImage::ImageRgba8(crate::latex::render_svg(&svg)?),
                false,
            );
        }
        if est {
            return self.sized(image::image_dimensions(path).ok()?, false);
        }
        self.image_entry(image::open(path).ok()?, false)
    }

    /// Inline markdown spans: bullets, `code`, $math$, [links](url),
    /// *emphasis*/**bold**, ![inline images] as alt text. Inline math renders
    /// in TeX text style as a one-row image in reserved columns.
    fn md_line(
        &self,
        raw: &str,
        line_idx: usize,
        images: &mut Vec<InlineImage>,
        est: bool,
    ) -> Line<'static> {
        let code_style = Style::new().fg(Color::Yellow);
        let math_style = Style::new()
            .fg(Color::Magenta)
            .add_modifier(Modifier::ITALIC);
        let link_style = Style::new()
            .fg(Color::Blue)
            .add_modifier(Modifier::UNDERLINED);
        let url_style = Style::new().fg(Color::DarkGray);
        let mut spans: Vec<Span<'static>> = Vec::new();
        let mut col = 0usize; // display column (unicode width)
        let mut text = String::new();
        let raw = &raw.replace('\t', "    "); // rendered prose: tabs as spaces
        let rest = match raw
            .trim_start()
            .strip_prefix("- ")
            .or_else(|| raw.trim_start().strip_prefix("* "))
        {
            Some(rest) => {
                let indent = raw.len() - raw.trim_start().len();
                text.push_str(&" ".repeat(indent));
                text.push_str("• ");
                rest
            }
            None => raw,
        };
        let flush = |text: &mut String, spans: &mut Vec<Span<'static>>, col: &mut usize| {
            if !text.is_empty() {
                *col += text.width();
                spans.push(Span::raw(std::mem::take(text)));
            }
        };
        let push_styled = |s: String,
                           style: Style,
                           text: &mut String,
                           spans: &mut Vec<Span<'static>>,
                           col: &mut usize| {
            flush(text, spans, col);
            *col += s.width();
            spans.push(Span::styled(s, style));
        };
        let mut chars = rest.chars().peekable();
        while let Some(c) = chars.next() {
            match c {
                '`' | '$' => {
                    let is_math = c == '$';
                    let mut inner = String::new();
                    let mut closed = false;
                    for n in chars.by_ref() {
                        if n == c {
                            closed = true;
                            break;
                        }
                        inner.push(n);
                    }
                    if !closed || (is_math && inner.trim().is_empty()) {
                        text.push(c);
                        text.push_str(&inner);
                        if closed {
                            text.push(c);
                        }
                        continue;
                    }
                    let inner = inner.trim().to_string();
                    flush(&mut text, &mut spans, &mut col);
                    if is_math {
                        let entry = if est {
                            let guess = crate::latex::to_unicode_approx(&inner).width();
                            self.sized_math(guess, 1)
                        } else {
                            self.picker
                                .as_ref()
                                .and_then(|p| {
                                    crate::latex::render_inline(&inner, p.font_size().height)
                                })
                                .and_then(|img| {
                                    self.image_entry(image::DynamicImage::ImageRgba8(img), false)
                                })
                        };
                        if let Some(entry) = entry {
                            // reserve the columns in the text flow; NBSP so
                            // soft wrapping never splits the equation
                            spans.push(Span::raw("\u{a0}".repeat(entry.cols as usize)));
                            images.push(InlineImage {
                                line: line_idx,
                                col: col as u16,
                                ..entry
                            });
                            col += entry.cols as usize;
                            continue;
                        }
                    }
                    let (shown, style) = if is_math {
                        (crate::latex::to_unicode_approx(&inner), math_style)
                    } else {
                        (inner, code_style)
                    };
                    col += shown.width();
                    spans.push(Span::styled(shown, style));
                }
                '*' => {
                    let bold = chars.peek() == Some(&'*');
                    if bold {
                        chars.next();
                    }
                    let mut inner = String::new();
                    let mut closed = false;
                    let mut prev_star = false;
                    for n in chars.by_ref() {
                        if n == '*' {
                            if !bold || prev_star {
                                closed = true;
                                break;
                            }
                            prev_star = true;
                        } else {
                            if prev_star {
                                inner.push('*');
                                prev_star = false;
                            }
                            inner.push(n);
                        }
                    }
                    if !closed || inner.is_empty() {
                        text.push_str(if bold { "**" } else { "*" });
                        text.push_str(&inner);
                        continue;
                    }
                    let mods = if bold {
                        Modifier::BOLD
                    } else {
                        Modifier::ITALIC
                    };
                    push_styled(
                        inner,
                        Style::new().add_modifier(mods),
                        &mut text,
                        &mut spans,
                        &mut col,
                    );
                }
                '[' => match take_link(&mut chars) {
                    Ok((label, url)) => {
                        push_styled(label, link_style, &mut text, &mut spans, &mut col);
                        push_styled(
                            format!(" ⟨{url}⟩"),
                            url_style,
                            &mut text,
                            &mut spans,
                            &mut col,
                        );
                    }
                    Err(lit) => text.push_str(&lit),
                },
                '!' if chars.peek() == Some(&'[') => {
                    chars.next();
                    match take_link(&mut chars) {
                        // mid-text image: show the alt text (block images render per-line)
                        Ok((label, url)) => push_styled(
                            format!("[image: {}]", if label.is_empty() { url } else { label }),
                            Style::new().fg(Color::Magenta),
                            &mut text,
                            &mut spans,
                            &mut col,
                        ),
                        Err(lit) => {
                            text.push('!');
                            text.push_str(&lit);
                        }
                    }
                }
                _ => text.push(c),
            }
        }
        flush(&mut text, &mut spans, &mut col);
        Line::from(spans)
    }
}

/// Display math rows a layout pass assumes before rendering (capped by
/// max_math_rows); the real height corrects it once the cell is on screen.
const MATH_ROWS_GUESS: u16 = 2;

/// An output's base64 payload, decoded.
fn b64_bytes(v: &Value) -> Option<Vec<u8>> {
    let b64: String = join_multiline(v).split_whitespace().collect();
    base64::engine::general_purpose::STANDARD.decode(b64).ok()
}

/// Pixel size of an encoded raster image, read from its header.
fn bytes_px(bytes: &[u8]) -> Option<(u32, u32)> {
    image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .ok()?
        .into_dimensions()
        .ok()
}

/// Source text as spans, with each tab shown as a dim `→` (one column, so
/// char columns still map 1:1 onto the editor cursor). ratatui draws a raw
/// `\t` as zero width: a stray tab would be invisible — and in Python, an
/// IndentationError nobody can see.
fn show_tabs(text: &str, style: Style) -> Vec<Span<'static>> {
    if !text.contains('\t') {
        return vec![Span::styled(text.to_string(), style)];
    }
    let tab = Style::new().fg(Color::DarkGray);
    let mut out = Vec::new();
    for (i, part) in text.split('\t').enumerate() {
        if i > 0 {
            out.push(Span::styled("→", tab));
        }
        if !part.is_empty() {
            out.push(Span::styled(part.to_string(), style));
        }
    }
    out
}

/// Expand tabs in output text to 8-column stops (terminal semantics),
/// skipping ANSI escape sequences when counting columns.
fn expand_tabs(text: &str) -> String {
    if !text.contains('\t') {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len() + 16);
    let (mut col, mut in_esc) = (0usize, false);
    for c in text.chars() {
        match c {
            '\t' => {
                let n = 8 - col % 8;
                out.extend(std::iter::repeat_n(' ', n));
                col += n;
            }
            '\n' => {
                out.push(c);
                col = 0;
            }
            '\x1b' => {
                out.push(c);
                in_esc = true;
            }
            _ if in_esc => {
                out.push(c);
                in_esc = !c.is_ascii_alphabetic(); // CSI ends at its final letter
            }
            _ => {
                out.push(c);
                col += c.width().unwrap_or(0);
            }
        }
    }
    out
}

/// After a consumed `[`, take `label](url)`. Err carries the literal text to
/// emit when the shape doesn't match (those chars are already consumed).
fn take_link(chars: &mut std::iter::Peekable<std::str::Chars>) -> Result<(String, String), String> {
    let mut label = String::new();
    let mut closed = false;
    for n in chars.by_ref() {
        if n == ']' {
            closed = true;
            break;
        }
        label.push(n);
    }
    if !closed || chars.peek() != Some(&'(') {
        let mut lit = format!("[{label}");
        if closed {
            lit.push(']');
        }
        return Err(lit);
    }
    chars.next(); // '('
    let mut url = String::new();
    for n in chars.by_ref() {
        if n == ')' {
            return Ok((label, url));
        }
        url.push(n);
    }
    Err(format!("[{label}]({url}"))
}

/// `![alt](src)` alone on a line -> (alt, src).
fn parse_md_image(t: &str) -> Option<(&str, &str)> {
    let (alt, rest) = t.strip_prefix("![")?.split_once("](")?;
    let src = rest.strip_suffix(')')?;
    (!src.contains(')') && !alt.contains(']')).then_some((alt, src))
}

/// Pipe table -> aligned rows; separator rows become rules, header goes bold.
fn render_table(rows: &[String]) -> Vec<Line<'static>> {
    let parsed: Vec<Vec<String>> = rows
        .iter()
        .map(|r| {
            r.trim()
                .trim_start_matches('|')
                .trim_end_matches('|')
                .split('|')
                .map(|c| c.trim().to_string())
                .collect()
        })
        .collect();
    let is_sep = |row: &[String]| {
        !row.is_empty()
            && row
                .iter()
                .all(|c| !c.is_empty() && c.chars().all(|ch| matches!(ch, '-' | ':')))
    };
    let ncols = parsed.iter().map(Vec::len).max().unwrap_or(0);
    let mut widths = vec![0usize; ncols];
    for row in parsed.iter().filter(|r| !is_sep(r)) {
        for (j, c) in row.iter().enumerate() {
            widths[j] = widths[j].max(c.chars().count());
        }
    }
    let header = parsed.len() > 1 && is_sep(&parsed[1]);
    let frame_style = Style::new().fg(Color::DarkGray);
    parsed
        .iter()
        .enumerate()
        .map(|(i, row)| {
            if is_sep(row) {
                let rule = widths
                    .iter()
                    .map(|w| "─".repeat(w + 2))
                    .collect::<Vec<_>>()
                    .join("┼");
                return Line::styled(rule, frame_style);
            }
            let style = if header && i == 0 {
                Style::new().add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            // dim bars, so the frame reads as one structure with the rule
            let mut spans = Vec::with_capacity(ncols * 2);
            for (j, w) in widths.iter().enumerate() {
                if j > 0 {
                    spans.push(Span::styled("│", frame_style));
                }
                spans.push(Span::styled(
                    format!(" {:<w$} ", row.get(j).map(String::as_str).unwrap_or("")),
                    style,
                ));
            }
            Line::from(spans)
        })
        .collect()
}

fn output_lines(output: &Value) -> Vec<Line<'static>> {
    let (text, style) = match output["output_type"].as_str() {
        Some("stream") => (join_multiline(&output["text"]), OUTPUT_STYLE),
        Some("execute_result") | Some("display_data") => {
            let data = &output["data"];
            let plain = join_multiline(&data["text/plain"]);
            if !plain.is_empty() {
                (plain, OUTPUT_STYLE)
            } else if let Some(obj) = data.as_object() {
                // no renderable rep (or no graphics): name what's there
                (
                    obj.keys()
                        .map(|k| format!("[{k}]"))
                        .collect::<Vec<_>>()
                        .join(" "),
                    OUTPUT_STYLE,
                )
            } else {
                return vec![];
            }
        }
        Some("error") => (
            match &output["traceback"] {
                Value::Array(tb) => tb
                    .iter()
                    .filter_map(|l| l.as_str())
                    .collect::<Vec<_>>()
                    .join("\n"),
                other => join_multiline(other),
            },
            ERROR_STYLE,
        ),
        _ => return vec![],
    };
    // any remaining \r is a pending-overwrite marker from stream collapsing
    let text = expand_tabs(&text.replace('\r', ""));
    match text.into_text() {
        Ok(t) => t
            .lines
            .into_iter()
            .map(|l| {
                if l.spans.iter().all(|s| s.style == Style::default()) {
                    l.style(style)
                } else {
                    l
                }
            })
            .collect(),
        Err(_) => text
            .lines()
            .map(|l| Line::styled(l.to_string(), style))
            .collect(),
    }
}

/// Columns of the left gutter bar on source rows (`▌ `).
pub const GUTTER: u16 = 2;

/// Cell-type accent: code cyan, markdown magenta, raw yellow.
fn type_color(cell: &Cell) -> Color {
    match cell.cell_type.as_str() {
        "code" => Color::Cyan,
        "markdown" => Color::Magenta,
        _ => Color::Yellow,
    }
}

/// The gutter bar: bright on the selected cell, dimmed elsewhere. Every
/// modifier a markdown heading line carries is masked off.
fn gutter_span(cell: &Cell, selected: bool) -> Span<'static> {
    let style = Style::new()
        .fg(type_color(cell))
        .remove_modifier(Modifier::all());
    let style = if selected {
        style.add_modifier(Modifier::BOLD)
    } else {
        style.add_modifier(Modifier::DIM)
    };
    Span::styled("▌ ", style)
}

/// Cell header: gutter, label (`In [3]`, `markdown`, ...), edit marker,
/// timing, then a rule to the right edge so cells read as separate blocks.
/// `run`: None = idle, Some(false) = queued, Some(true) = executing.
fn header_line(
    cell: &Cell,
    selected: bool,
    run: Option<bool>,
    editing: bool,
    elapsed: Option<std::time::Duration>,
    width: u16,
) -> Line<'static> {
    let label = match cell.cell_type.as_str() {
        "code" => match run {
            Some(true) => "In [*]".to_string(),
            Some(false) => "In [·]".to_string(),
            None => match cell.execution_count() {
                Some(n) => format!("In [{n}]"),
                None => "In [ ]".to_string(),
            },
        },
        "raw" if is_latex_raw(cell) => "latex".to_string(),
        other => other.to_string(),
    };
    let color = type_color(cell);
    let mut label_style = Style::new().fg(color).add_modifier(Modifier::BOLD);
    if selected {
        label_style = label_style.add_modifier(Modifier::REVERSED);
    }
    let rule = if selected {
        Style::new().fg(color)
    } else {
        Style::new().fg(Color::DarkGray)
    };
    let mut spans = vec![
        gutter_span(cell, selected),
        Span::styled(format!(" {label} "), label_style),
    ];
    if editing {
        spans.push(Span::styled(" ✎", Style::new().fg(color)));
    }
    if run.is_none()
        && let Some(d) = elapsed
    {
        spans.push(Span::styled(
            format!(" · {}", fmt_duration(d)),
            Style::new().fg(Color::DarkGray),
        ));
    }
    let used: usize = spans.iter().map(Span::width).sum();
    let fill = (width as usize).saturating_sub(used + 1);
    spans.push(Span::styled(format!(" {}", "─".repeat(fill)), rule));
    Line::from(spans)
}

fn fmt_duration(d: std::time::Duration) -> String {
    let s = d.as_secs_f64();
    if s < 1.0 {
        format!("{:.0}ms", s * 1e3)
    } else if s < 60.0 {
        format!("{s:.1}s")
    } else {
        format!("{}m{:02}s", d.as_secs() / 60, d.as_secs() % 60)
    }
}

pub fn draw(frame: &mut Frame, app: &mut App, rendered: &mut Rendered) {
    use crate::app::{Hit, HitKind};
    // one chokepoint sends every status message to the log (viewer: L)
    if let Some(m) = &app.message
        && app.logged_status.as_ref() != Some(m)
    {
        log::info!(target: "status", "{m}");
        app.logged_status = Some(m.clone());
    }
    let [body, status] =
        Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).areas(frame.area());
    let height = body.height as usize;

    rendered.relayout(body.width, &app.notebook);

    // The editing cell renders live from the editor buffer: highlighted and
    // wrapped once per frame (one cell; cached blocks rewrap only on resize).
    // The editor's line count is authoritative for its logical lines.
    let editing_cell = app.editor.as_ref().map(|_| app.selected);
    let editor_src = app.editor.as_ref().map(|e| e.source()).unwrap_or_default();
    let editor_wrap = match (&app.editor, app.notebook.cells.get(app.selected)) {
        (Some(ed), Some(cell)) => {
            let mut logical = rendered.highlight(&editor_src, rendered.cell_lang(cell));
            logical.resize(ed.line_count().max(1), Line::raw(""));
            // visual-mode selection (nvim backend) overlays the highlighting
            if let Some((kind, anchor, cursor)) = ed.visual() {
                for (row, l) in logical.iter_mut().enumerate() {
                    if let Some((from, to)) = sel_range(kind, anchor, cursor, row, line_chars(l)) {
                        overlay_reversed(l, from, to);
                    }
                }
            }
            wrap_lines(&logical, body.width.saturating_sub(GUTTER).max(1))
        }
        _ => Wrapped::default(),
    };
    // editor cursor as (wrapped row within the cell, display column)
    let editor_cursor = app.editor.as_ref().and_then(|ed| {
        let (crow, ccol) = ed.cursor();
        let crow = crow.min(editor_wrap.first_row.len().checked_sub(2)?);
        let (row, col0) = editor_wrap.row_of_char(crow, ccol);
        let line = editor_src.split('\n').nth(crow).unwrap_or("");
        let x: usize = line
            .chars()
            .skip(col0)
            .take(ccol.saturating_sub(col0))
            .map(|c| if c == '\t' { 1 } else { c.width().unwrap_or(0) }) // show_tabs: `→`
            .sum();
        Some((row, x))
    });

    // Layout pass: per-cell displayed shape and start lines — no clones.
    // Body = source rows + a window onto the output (+ footer when clipped).
    struct CellView {
        src: usize,
        win_start: usize,
        win_rows: usize,
        out_len: usize,
        footer: bool,
    }
    impl CellView {
        fn body(&self) -> usize {
            self.src + self.win_rows + self.footer as usize
        }
    }
    let view = |rendered: &Rendered, i: usize| -> CellView {
        let block = &rendered.blocks[i];
        let src = if editing_cell == Some(i) {
            editor_wrap.rows.len()
        } else {
            block.src_rows()
        };
        let (win_start, win_rows, footer) = block.window();
        CellView {
            src,
            win_start,
            win_rows,
            out_len: block.out_len(),
            footer,
        }
    };
    // Only cells on or near the screen are rendered; the rest are laid out
    // from row counts (estimated for math). Rendering a cell can change its
    // height, which moves the view, which can bring more cells near: repeat
    // until the neighbourhood is rendered. Each round renders at least one
    // cell, so this ends.
    let n = app.notebook.cells.len();
    let (mut starts, mut heights) = (Vec::with_capacity(n), Vec::with_capacity(n));
    let mut total;
    loop {
        heights.clear();
        heights.extend((0..n).map(|i| 1 + view(rendered, i).body() + 1)); // prompt + body + separator
        // Scroll anchoring: a cell wholly above the view that changed height
        // since it was last laid out (estimate corrected, output grew) moves
        // the view with it, so the content on screen stays put.
        let (mut old_end, mut shift) = (0usize, 0isize);
        for (b, &h) in rendered.blocks.iter_mut().zip(&heights) {
            if let Some(old) = b.last_h.replace(h) {
                old_end += old;
                if old_end <= app.scroll {
                    shift += h as isize - old as isize;
                }
            }
        }
        app.scroll = app.scroll.saturating_add_signed(shift);
        starts.clear();
        total = 0;
        for &h in &heights {
            starts.push(total);
            total += h;
        }

        // Keep selection visible (cursor line when editing, whole cell
        // otherwise) — unless the user wheel-scrolled away; then only clamp
        // to the content.
        if app.manual_scroll || starts.is_empty() {
            app.scroll = app.scroll.min(total.saturating_sub(1));
        } else if let Some((row, _)) = editor_cursor {
            let cur = starts[app.selected] + 1 + row;
            if cur >= app.scroll + height {
                app.scroll = cur + 1 - height;
            }
            if cur < app.scroll {
                app.scroll = cur;
            }
        } else {
            let sel_start = starts[app.selected];
            let sel_end = sel_start + heights[app.selected] - 1;
            if sel_end > app.scroll + height {
                app.scroll = sel_end - height;
            }
            if sel_start < app.scroll {
                app.scroll = sel_start;
            }
        }

        // render the screen plus one screen of margin on each side
        let (lo, hi) = (app.scroll.saturating_sub(height), app.scroll + 2 * height);
        let mut any = false;
        for i in 0..n {
            if starts[i] < hi && starts[i] + heights[i] > lo {
                any |= rendered.ensure(i, &app.notebook.cells[i]);
                rendered.blocks[i].seen = rendered.frame;
            }
        }
        if !any {
            break;
        }
    }
    // over the memory budget: drop the least recently seen content (keep the
    // row counts), never the render window
    let (lo, hi) = (app.scroll.saturating_sub(height), app.scroll + 2 * height);
    rendered.evict(|i| starts[i] < hi && starts[i] + heights[i] > lo);
    rendered.frame += 1;
    app.content_lines = total;
    app.body = body;

    // Materialize only the visible window (notebooks can be huge; frames are not).
    let end = (app.scroll + height).min(total);
    let mut lines: Vec<Line> = Vec::with_capacity(end.saturating_sub(app.scroll));
    let mut hit: Vec<Hit> = Vec::with_capacity(end.saturating_sub(app.scroll));
    let mut ci = match starts.binary_search(&app.scroll) {
        Ok(i) => i,
        Err(i) => i.saturating_sub(1),
    };
    for a in app.scroll..end {
        while ci + 1 < starts.len() && a >= starts[ci + 1] {
            ci += 1;
        }
        let cell = &app.notebook.cells[ci];
        let block = &rendered.blocks[ci];
        let editing = editing_cell == Some(ci);
        let v = view(rendered, ci);
        let local = a - starts[ci];
        let (line, kind) = if local == 0 {
            let run = app
                .running
                .iter()
                .find_map(|(m, &c)| (c == ci).then(|| app.started.contains_key(m)));
            (
                header_line(
                    cell,
                    ci == app.selected,
                    run,
                    editing,
                    block.elapsed,
                    body.width,
                ),
                HitKind::Other,
            )
        } else if local <= v.src {
            let b = local - 1;
            let wrap = if editing { &editor_wrap } else { &block.wrap };
            let o = wrap.origin[b];
            let kind = if editing || (cell.cell_type != "markdown" && !is_latex_raw(cell)) {
                HitKind::Source {
                    line: o.line,
                    col: o.col,
                }
            } else if let Some(&s) = block.src_map.get(o.line) {
                // rendered markdown row -> raw source line
                HitKind::Source { line: s, col: 0 }
            } else {
                HitKind::Other // latex raw: no per-row mapping
            };
            let mut row = wrap.rows[b].clone();
            row.spans.insert(0, gutter_span(cell, ci == app.selected));
            (row, kind)
        } else if local <= v.src + v.win_rows {
            let out_row = v.win_start + (local - 1 - v.src);
            (
                block.wrap.rows[block.src_rows() + out_row].clone(),
                HitKind::Output,
            )
        } else if v.footer && local == 1 + v.src + v.win_rows {
            let label = if block.collapsed {
                format!("··· {} output lines hidden (o) ···", v.out_len)
            } else {
                format!(
                    "··· output {}–{} of {} · [ ] or wheel scrolls, o hides ···",
                    v.win_start + 1,
                    v.win_start + v.win_rows,
                    v.out_len
                )
            };
            (
                Line::styled(label, Style::new().fg(Color::DarkGray)),
                HitKind::Output,
            )
        } else {
            (Line::raw(""), HitKind::Other) // separator
        };
        lines.push(line);
        hit.push(Hit { cell: ci, kind });
    }
    app.hit = hit;
    frame.render_widget(Paragraph::new(Text::from(lines)), body);

    // Image pass: render graphics into their reserved blank lines. SlicedImage
    // clips rows outside the area it is given: the image was transmitted once,
    // only row-offset placeholders move on scroll. Skipped while the zoom
    // overlay owns the screen.
    for (i, block) in rendered
        .blocks
        .iter()
        .enumerate()
        .filter(|_| app.zoom.is_none())
    {
        let editing = editing_cell == Some(i);
        let v = view(rendered, i);
        for img in &block.images {
            let in_src = img.line < block.src_lines;
            // editing replaces the source region: markdown-math images hide
            if in_src && editing {
                continue;
            }
            let (row, x) = block.image_pos(img);
            if x >= body.width as usize {
                continue;
            }
            let (area, pos) = if in_src {
                // source images: straight mapping, clipped by the viewport
                let rel = starts[i] as isize + 1 + row as isize - app.scroll as isize;
                if rel + (img.rows as isize) <= 0 || rel >= body.height as isize {
                    continue;
                }
                (
                    body,
                    SignedPosition {
                        x: (x as u16 + GUTTER) as i16,
                        y: rel as i16,
                    },
                )
            } else {
                // output images: clipped by the output window, then the viewport
                let ol = row - block.src_rows();
                if ol + img.rows as usize <= v.win_start || ol >= v.win_start + v.win_rows {
                    continue;
                }
                let wtop = starts[i] as isize + 1 + v.src as isize - app.scroll as isize;
                let y0 = wtop.max(0);
                let y1 = (wtop + v.win_rows as isize).min(body.height as isize);
                if y1 <= y0 {
                    continue;
                }
                let area = Rect {
                    x: body.x,
                    y: body.y + y0 as u16,
                    width: body.width,
                    height: (y1 - y0) as u16,
                };
                let img_rel = wtop + ol as isize - v.win_start as isize;
                (
                    area,
                    SignedPosition {
                        x: x as i16,
                        y: (img_rel - y0) as i16,
                    },
                )
            };
            if let Some(proto) = &img.proto {
                frame.render_widget(SlicedImage::new(proto, pos), area);
            }
        }
    }

    // Editor cursor: real terminal cursor at the edit position.
    let mut cursor_at = None;
    if let Some((row, x)) = editor_cursor {
        let abs = starts[app.selected] + 1 + row;
        if abs >= app.scroll && ((abs - app.scroll) as u16) < body.height {
            let pos = (
                body.x + (x as u16 + GUTTER).min(body.width.saturating_sub(1)),
                body.y + (abs - app.scroll) as u16,
            );
            frame.set_cursor_position(pos);
            cursor_at = Some(pos);
        }
    }

    let (mode_label, mode_bg) = match app.editor.as_ref().map(|e| e.mode()) {
        None => (" VIEW ", Color::Cyan),
        Some(ModeKind::Insert) => (" INSERT ", Color::Green),
        Some(ModeKind::Visual) => (" VISUAL ", Color::Magenta),
        Some(ModeKind::VisualLine) => (" V-LINE ", Color::Magenta),
        Some(ModeKind::VisualBlock) => (" V-BLOCK ", Color::Magenta),
        Some(ModeKind::Replace) => (" REPLACE ", Color::Red),
        Some(ModeKind::Pending) => (" O-PEND ", Color::Yellow),
        Some(ModeKind::Cmdline) => (" CMD ", Color::Yellow),
        Some(ModeKind::Normal) => (" NORMAL ", Color::Yellow),
    };
    let kernel_state = if app.kernel.is_none() {
        Span::styled("◌ no kernel", Style::new().fg(Color::DarkGray))
    } else if app.kernel_busy {
        Span::styled("● busy", Style::new().fg(Color::Yellow))
    } else {
        Span::styled("○ idle", Style::new().fg(Color::Green))
    };
    // rightmost slot: prompt input, nvim's cmdline, a pending question, then
    // the message
    let tail = if let Some(p) = &app.prompt {
        match p.kind {
            crate::app::PromptKind::Search => format!("/{}▏", p.buf),
            crate::app::PromptKind::SaveAs => format!("save as: {}▏", p.buf),
        }
    } else if let Some(c) = app.editor.as_ref().and_then(|e| e.cmdline()) {
        format!("{c}▏")
    } else if let Some(q) = &app.confirm {
        q.question()
    } else {
        app.message.clone().unwrap_or_default()
    };
    // a pending question takes the whole line, so its choices always fit
    let status_line = if app.confirm.is_some() {
        Line::from(vec![
            Span::styled(
                mode_label,
                Style::new()
                    .fg(Color::Black)
                    .bg(mode_bg)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(format!(" {tail}")),
        ])
    } else {
        Line::from(vec![
            Span::styled(
                mode_label,
                Style::new()
                    .fg(Color::Black)
                    .bg(mode_bg)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(format!(
                " {}{}  cell {}/{}  ",
                app.path.display(),
                if app.dirty { " [+]" } else { "" },
                app.selected + 1,
                app.notebook.cells.len()
            )),
            kernel_state,
            Span::raw(format!("  {tail}")),
        ])
    };
    frame.render_widget(Paragraph::new(status_line), status);

    // `z`: fullscreen image overlay, centered, any key closes.
    if let Some(proto) = &app.zoom {
        use ratatui::widgets::Clear;
        frame.render_widget(Clear, body);
        let size = proto.size();
        let pos = SignedPosition {
            x: (body.width.saturating_sub(size.width) / 2) as i16,
            y: (body.height.saturating_sub(size.height) / 2) as i16,
        };
        frame.render_widget(SlicedImage::new(proto, pos), body);
    }
    // the completion popup belongs to the text; overlays (docs pager, help,
    // logs, ...) draw over it
    if let (Some(c), Some(at)) = (&mut app.completion, cursor_at)
        && !c.items.is_empty()
    {
        // align the popup's text with the token start (the live completer's
        // `.attr` matches display without the dot)
        let (start, end) = c.span();
        let typed: String = editor_src
            .chars()
            .skip(start)
            .take(end.saturating_sub(start))
            .collect();
        let typed_w = typed.trim_start_matches('.').width() as u16;
        draw_completion(frame, c, at, typed_w, body, &rendered.panel());
    }
    // signature help: under the line, or above it while completion has the
    // space below (nvim-style)
    if let (Some(sig), Some(at), Some(editor)) = (&app.signature, cursor_at, &app.editor)
        && !sig.text.is_empty()
        && editor.mode() == ModeKind::Insert
    {
        let completion_below = app.completion.as_ref().is_some_and(|c| {
            let shown = c.items.len().min(COMPLETION_ROWS);
            shown > 0 && (body.y + body.height).saturating_sub(at.1 + 1) as usize >= shown
        });
        // the callee's column, when it is on the cursor's line
        let (row, col) = editor.cursor();
        let line_start = crate::app::char_offset(&editor_src, (row, 0));
        let back = if sig.name_start() >= line_start {
            let from = sig.name_start() - line_start;
            let line = editor_src.split('\n').nth(row).unwrap_or("");
            let typed: String = line
                .chars()
                .skip(from)
                .take(col.saturating_sub(from))
                .collect();
            typed.width() as u16
        } else {
            0
        };
        draw_signature(
            frame,
            sig,
            at,
            back,
            completion_below,
            body,
            &rendered.panel(),
        );
    }
    if app.show_help {
        draw_help(frame);
    }
    if let Some(up) = &mut app.logs {
        draw_logs(frame, up);
    }
    if let Some(text) = &app.debug {
        draw_debug(frame, text);
    }
    if let Some(pager) = &mut app.pager {
        draw_pager(frame, pager);
    }
    if let Some(req) = &app.stdin_req {
        draw_stdin(frame, req);
    }
}

fn line_chars(line: &Line) -> usize {
    line.spans.iter().map(|s| s.content.chars().count()).sum()
}

/// Char-column range (inclusive) of `row`'s visual selection, if any.
/// Anchor/cursor are (row, col) in chars; vim selections include the cursor.
fn sel_range(
    kind: ModeKind,
    anchor: (usize, usize),
    cursor: (usize, usize),
    row: usize,
    len: usize,
) -> Option<(usize, usize)> {
    let (lo, hi) = if anchor <= cursor {
        (anchor, cursor)
    } else {
        (cursor, anchor)
    };
    if row < lo.0 || row > hi.0 {
        return None;
    }
    let eol = len.saturating_sub(1);
    match kind {
        ModeKind::VisualLine => Some((0, eol)),
        ModeKind::VisualBlock => Some((lo.1.min(hi.1), lo.1.max(hi.1))),
        ModeKind::Visual => Some((
            if row == lo.0 { lo.1 } else { 0 },
            if row == hi.0 { hi.1 } else { eol },
        )),
        _ => None,
    }
}

/// Restyle chars [from..=to] of a highlighted line as REVERSED, splitting
/// spans at the boundaries.
fn overlay_reversed(line: &mut Line<'static>, from: usize, to: usize) {
    if line_chars(line) == 0 {
        // empty line inside a selection: show one reversed cell
        line.spans = vec![Span::styled(
            " ",
            Style::new().add_modifier(Modifier::REVERSED),
        )];
        return;
    }
    let byte_at = |s: &str, ci: usize| s.char_indices().nth(ci).map_or(s.len(), |(b, _)| b);
    let mut out: Vec<Span<'static>> = Vec::with_capacity(line.spans.len() + 2);
    let mut pos = 0usize; // char position across the whole line
    for span in line.spans.drain(..) {
        let len = span.content.chars().count();
        let (s0, s1) = (pos, pos + len);
        pos = s1;
        if s1 <= from || s0 > to || len == 0 {
            out.push(span);
            continue;
        }
        let a = byte_at(&span.content, from.saturating_sub(s0));
        let b = byte_at(&span.content, (to + 1 - s0).min(len));
        let text = span.content.to_string();
        if a > 0 {
            out.push(Span::styled(text[..a].to_string(), span.style));
        }
        out.push(Span::styled(
            text[a..b].to_string(),
            span.style.add_modifier(Modifier::REVERSED),
        ));
        if b < text.len() {
            out.push(Span::styled(text[b..].to_string(), span.style));
        }
    }
    line.spans = out;
}

/// Floating panel: clears `area`, draws a rounded border with the title on
/// top, an optional hint on the bottom edge, and one column of horizontal
/// padding. Returns the content area.
fn modal(frame: &mut Frame, area: Rect, title: &str, hint: &str) -> Rect {
    use ratatui::widgets::{Block, BorderType, Clear, Padding};
    let dim = Style::new().fg(Color::DarkGray);
    let mut block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(dim)
        .padding(Padding::horizontal(1));
    if !title.is_empty() {
        block = block.title(Span::styled(
            format!(" {title} "),
            Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        ));
    }
    if !hint.is_empty() {
        block = block.title_bottom(Line::styled(format!(" {hint} "), dim).right_aligned());
    }
    let inner = block.inner(area);
    frame.render_widget(Clear, area);
    frame.render_widget(block, area);
    inner
}

/// A `w`x`h` rect centered in `outer`, clamped to fit.
fn centered(outer: Rect, w: u16, h: u16) -> Rect {
    let (w, h) = (w.min(outer.width), h.min(outer.height));
    Rect {
        x: outer.x + (outer.width - w) / 2,
        y: outer.y + (outer.height - h) / 2,
        width: w,
        height: h,
    }
}

/// Kernel `input()`: a centered modal prompt (all keys already route to it).
fn draw_stdin(frame: &mut Frame, req: &crate::app::StdinReq) {
    let area = frame.area();
    let width = area.width.saturating_sub(6).clamp(26, 72);
    let title = match req.prompt.trim() {
        "" => "input()",
        p => p,
    };
    let inner = modal(
        frame,
        centered(area, width, 3),
        title,
        "Enter sends · Esc sends empty",
    );
    let shown: String = if req.password {
        "•".repeat(req.buf.chars().count())
    } else {
        req.buf.clone()
    };
    // keep the tail (and the cursor) inside the box when the input outgrows it
    let skip = shown
        .chars()
        .count()
        .saturating_sub((inner.width as usize).saturating_sub(1));
    let tail: String = shown.chars().skip(skip).collect();
    frame.set_cursor_position((inner.x + tail.chars().count() as u16, inner.y));
    frame.render_widget(Paragraph::new(tail), inner);
}

/// `L`: the in-memory log (status messages, kernel lifecycle, warnings from
/// dependencies), newest at the bottom. `up` = rows scrolled up; clamped here.
fn draw_logs(frame: &mut Frame, up: &mut usize) {
    let area = frame.area();
    let panel = centered(
        area,
        area.width.saturating_sub(4).clamp(40, 140),
        area.height.saturating_sub(2),
    );
    let rows = panel.height.saturating_sub(2) as usize;
    let lines: Vec<Line> = crate::log::with_entries(|entries| {
        *up = (*up).min(entries.len().saturating_sub(rows));
        let end = entries.len() - *up;
        entries
            .range(end.saturating_sub(rows)..end)
            .map(|e| {
                let level = match e.level {
                    log::Level::Error => Style::new().fg(Color::Red).add_modifier(Modifier::BOLD),
                    log::Level::Warn => Style::new().fg(Color::Yellow),
                    _ => Style::new().fg(Color::Green),
                };
                Line::from(vec![
                    Span::styled(
                        format!("{:>8.2}s ", e.secs),
                        Style::new().fg(Color::DarkGray),
                    ),
                    Span::styled(format!("{:<5} ", e.level), level),
                    Span::styled(format!("{} ", e.target), Style::new().fg(Color::Blue)),
                    Span::raw(e.msg.clone()),
                ])
            })
            .collect()
    });
    let title = if *up == 0 {
        "logs".to_string()
    } else {
        format!("logs · {} newer below", *up)
    };
    let inner = modal(
        frame,
        panel,
        &title,
        "j/k PgUp/PgDn g/G scroll · any other key closes",
    );
    if lines.is_empty() {
        frame.render_widget(
            Paragraph::new(Line::styled("(empty)", Style::new().fg(Color::DarkGray))),
            inner,
        );
    } else {
        frame.render_widget(Paragraph::new(lines), inner);
    }
}

/// Popup palette derived from the syntax theme's background: a panel a
/// step lighter (darker on light themes), a brighter selection row, and the
/// scrollbar thumb.
pub struct Panel {
    bg: Color,
    sel: Color,
    thumb: Color,
}

impl Rendered {
    pub fn panel(&self) -> Panel {
        let base = self
            .theme
            .settings
            .background
            .map_or((0x2b, 0x30, 0x3b), |c| (c.r, c.g, c.b));
        let light = (base.0 as u32 * 299 + base.1 as u32 * 587 + base.2 as u32 * 114) / 1000 > 128;
        let target = if light { 0.0 } else { 255.0 };
        let mix = |t: f32| {
            let m = |c: u8| (c as f32 + (target - c as f32) * t) as u8;
            Color::Rgb(m(base.0), m(base.1), m(base.2))
        };
        Panel {
            bg: mix(0.08),
            sel: mix(0.20),
            thumb: mix(0.35),
        }
    }
}

/// Completion popup height (rows); the app resolves this many at a time.
pub const COMPLETION_ROWS: usize = 12;

/// Completion kinds as the popup shows them, with their color.
fn kind_label(kind: &str) -> (&'static str, Color) {
    match kind {
        "function" | "method" => ("Function", Color::Green),
        "class" => ("Class", Color::Yellow),
        "module" => ("Module", Color::Cyan),
        "keyword" => ("Keyword", Color::Magenta),
        "property" => ("Property", Color::Blue),
        "instance" | "statement" | "param" => ("Variable", Color::Blue),
        // the live completer's generic kinds: blank until resolved (showing
        // "Variable" and flipping to "Function" a moment later would lie)
        "attribute" | "variable" => ("", Color::DarkGray),
        "path" => ("Path", Color::DarkGray),
        "magic" => ("Magic", Color::Magenta),
        "word" => ("Text", Color::DarkGray),
        _ => ("", Color::DarkGray),
    }
}

/// Keep `sel` inside a `shown`-row window starting at `top`, moving the
/// window only when the selection leaves it (no jumping on every step).
pub fn scroll_window(top: usize, sel: usize, shown: usize, len: usize) -> usize {
    let top = if sel < top {
        sel
    } else if sel >= top + shown {
        sel + 1 - shown
    } else {
        top
    };
    top.min(len.saturating_sub(shown))
}

/// `s` cut to `w` display columns, with `…` when shortened.
fn fit(s: &str, w: usize) -> String {
    if s.width() <= w {
        return s.to_string();
    }
    let mut out = String::new();
    let mut used = 0;
    for c in s.chars() {
        let cw = c.width().unwrap_or(0);
        if used + cw + 1 > w {
            break;
        }
        out.push(c);
        used += cw;
    }
    out.push('…');
    out
}

/// Signature help popup: one row under the cursor line (above it when
/// `above`, or when there is no room below), its text starting `back`
/// columns left of the cursor (at the callee), the parameter being typed
/// highlighted. Too wide for the body: it scrolls to keep that parameter in
/// view.
fn draw_signature(
    frame: &mut Frame,
    sig: &crate::app::SigHelp,
    (cx, cy): (u16, u16),
    back: u16,
    above: bool,
    body: Rect,
    panel: &Panel,
) {
    let bottom = body.y + body.height;
    let y = if (above || cy + 1 >= bottom) && cy > body.y {
        cy - 1
    } else if cy + 1 < bottom {
        cy + 1
    } else {
        return;
    };
    let text = &sig.text;
    let active = sig.active();
    // horizontal window: whole text if it fits, else ending just after the
    // active parameter (or at the start when none is active)
    let room = body.width.saturating_sub(2) as usize;
    let mut skip = 0; // bytes cut from the left
    if text.width() > room {
        let need_end = active.as_ref().map_or(0, |r| r.end);
        while text[skip..need_end.max(skip)].width() + 1 > room {
            skip += text[skip..].chars().next().map_or(1, char::len_utf8);
        }
    }
    let base = Style::new().bg(panel.bg);
    let hot = Style::new().bg(panel.sel).add_modifier(Modifier::BOLD);
    let mut spans = vec![Span::styled(if skip > 0 { "…" } else { " " }, base)];
    let mut push = |range: std::ops::Range<usize>, style: Style| {
        let (a, b) = (range.start.max(skip), range.end.max(skip));
        if a < b {
            spans.push(Span::styled(text[a..b].to_string(), style));
        }
    };
    match active {
        Some(r) => {
            push(0..r.start, base);
            push(r.clone(), hot);
            push(r.end..text.len(), base);
        }
        None => push(0..text.len(), base),
    }
    spans.push(Span::styled(" ", base));
    let line = Line::from(spans);
    let width = (line.width() as u16).min(body.width);
    let x = cx
        .saturating_sub(back + 1)
        .max(body.x)
        .min(body.x + body.width - width);
    let area = Rect {
        x,
        y,
        width,
        height: 1,
    };
    frame.render_widget(ratatui::widgets::Clear, area);
    frame.render_widget(Paragraph::new(line), area);
}

/// Completion popup (nvim-cmp style): a borderless panel under the cursor
/// (above it when there's no room), text column aligned with the token
/// being completed, columns name · kind · signature, and a scrollbar.
/// `typed_w`: display width typed since the token start.
fn draw_completion(
    frame: &mut Frame,
    c: &mut crate::app::Completion,
    (cx, cy): (u16, u16),
    typed_w: u16,
    body: Rect,
    panel: &Panel,
) {
    const MAX_NAME: usize = 36;
    let below = (body.y + body.height).saturating_sub(cy + 1) as usize;
    let above = cy.saturating_sub(body.y) as usize;
    let room = below.max(above);
    let shown = c.items.len().min(COMPLETION_ROWS).min(room);
    if shown == 0 {
        return;
    }
    c.top = scroll_window(c.top, c.sel, shown, c.items.len());
    let scrollbar = c.items.len() > shown;
    // column widths over all items (stable while scrolling)
    let name_w = c
        .items
        .iter()
        .map(|i| i.label().width())
        .max()
        .unwrap_or(0)
        .min(MAX_NAME);
    let kind_w = c
        .items
        .iter()
        .map(|i| kind_label(&i.kind).0.len())
        .max()
        .unwrap_or(0);
    let detail_w = c.items.iter().map(|i| i.detail.width()).max().unwrap_or(0);
    let fixed = 1 + name_w + 2 + kind_w + if detail_w > 0 { 2 } else { 0 } + 1 + scrollbar as usize;
    let width = (fixed + detail_w).min(body.width as usize) as u16;
    let detail_room = (width as usize).saturating_sub(fixed);
    let x = cx
        .saturating_sub(typed_w + 1)
        .max(body.x)
        .min(body.x + body.width.saturating_sub(width));
    let y = if below >= shown {
        cy + 1
    } else {
        cy - shown as u16
    };
    let area = Rect {
        x,
        y,
        width,
        height: shown as u16,
    };
    frame.render_widget(ratatui::widgets::Clear, area);
    let thumb = if scrollbar {
        let len = ((shown * shown) / c.items.len()).max(1);
        let pos = (c.top * shown) / c.items.len();
        pos.min(shown - len)..pos.min(shown - len) + len
    } else {
        0..0
    };
    let lines: Vec<Line> = (0..shown)
        .map(|row| {
            let i = c.top + row;
            let item = &c.items[i];
            let bg = if i == c.sel { panel.sel } else { panel.bg };
            let base = Style::new().bg(bg);
            let (kind, kind_color) = kind_label(&item.kind);
            let name = fit(item.label(), name_w);
            let mut spans = vec![
                Span::styled(" ", base),
                Span::styled(
                    format!("{name:<name_w$}"),
                    if i == c.sel {
                        base.add_modifier(Modifier::BOLD)
                    } else {
                        base
                    },
                ),
                Span::styled(format!("  {kind:<kind_w$}"), base.fg(kind_color)),
            ];
            if detail_w > 0 {
                let d = fit(&item.detail, detail_room);
                spans.push(Span::styled(
                    format!("  {d:<detail_room$}"),
                    if i == c.sel {
                        base
                    } else {
                        base.fg(Color::Gray)
                    },
                ));
            }
            spans.push(Span::styled(" ", base));
            if scrollbar {
                let bar = if thumb.contains(&row) {
                    panel.thumb
                } else {
                    panel.bg
                };
                spans.push(Span::styled(" ", Style::new().bg(bar)));
            }
            Line::from(spans)
        })
        .collect();
    frame.render_widget(Paragraph::new(lines), area);
}

/// Shift+Tab docs: scrollable (ANSI-colored) text; `top` clamped here.
fn draw_pager(frame: &mut Frame, pager: &mut crate::app::Pager) {
    let area = frame.area();
    let panel = centered(
        area,
        area.width.saturating_sub(4).clamp(40, 110),
        area.height.saturating_sub(2),
    );
    let text = pager
        .text
        .into_text()
        .unwrap_or_else(|_| Text::raw(pager.text.clone()));
    let rows = panel.height.saturating_sub(2) as usize;
    let total = text.lines.len();
    pager.top = pager.top.min(total.saturating_sub(rows));
    let title = if total > rows {
        format!(
            "{} · {}–{} of {total}",
            pager.title,
            pager.top + 1,
            (pager.top + rows).min(total)
        )
    } else {
        pager.title.clone()
    };
    let inner = modal(
        frame,
        panel,
        &title,
        "j/k PgUp/PgDn g/G scroll · any other key closes",
    );
    let lines: Vec<Line> = text.lines.into_iter().skip(pager.top).take(rows).collect();
    frame.render_widget(Paragraph::new(lines), inner);
}

/// `D`: debug facts about the selected cell (already on the clipboard).
fn draw_debug(frame: &mut Frame, text: &str) {
    let lines: Vec<Line> = text
        .lines()
        .map(|l| match l.split_once(": ") {
            Some((k, v)) if !k.contains(' ') => Line::from(vec![
                Span::styled(format!("{k}: "), Style::new().fg(Color::Yellow)),
                Span::raw(v.to_string()),
            ]),
            _ => Line::raw(l.to_string()),
        })
        .collect();
    let width = lines.iter().map(Line::width).max().unwrap_or(0) as u16 + 4;
    let panel = centered(frame.area(), width.max(40), lines.len() as u16 + 2);
    let inner = modal(
        frame,
        panel,
        "debug · copied to clipboard",
        "any key closes",
    );
    frame.render_widget(Paragraph::new(lines), inner);
}

/// Help sections: (heading, [(keys, what)]). Keys are space-separated chords.
pub const HELP: &[(&str, &[(&str, &str)])] = &[
    (
        "Navigate",
        &[
            ("j k", "next / previous cell (counts: 3j)"),
            ("g G", "first / last cell"),
            ("5G", "go to cell 5"),
            ("/ n N", "search sources / next / previous"),
        ],
    ),
    (
        "Edit",
        &[
            ("Enter", "edit cell"),
            ("i A", "edit in insert mode / at the end"),
            ("Esc", "leave the editor (normal mode)"),
            ("E", "edit cell in $EDITOR"),
            ("m", "cycle code → markdown → latex"),
        ],
    ),
    (
        "Inside the editor",
        &[
            ("Tab", "complete (opens by itself after `.`)"),
            ("Shift+Tab", "docs (cursor name / highlighted item)"),
            ("Tab ↓ ↑ Enter", "completion: next / prev / accept"),
            ("Ctrl+\\", "split the cell at the cursor"),
            ("h j k l w b e", "builtin: move (also 0 $ ^ gg G)"),
            ("i a I A o O", "builtin: insert"),
            ("x D dd yy p P", "builtin: delete / yank / put"),
            ("u Ctrl+r", "builtin: undo / redo"),
        ],
    ),
    (
        "Run",
        &[
            ("Shift+Enter r", "run + advance"),
            ("Space Ctrl+Enter", "run in place"),
            ("X", "run all (selection follows)"),
            ("< >", "run all above / this and below"),
            ("Ctrl+c", "interrupt kernel"),
            ("R", "restart kernel (asks · a: + run all)"),
        ],
    ),
    (
        "Cells",
        &[
            ("a b", "new cell after / before"),
            ("dd yy p", "delete / copy / paste cell"),
            ("M", "merge with the cell below"),
            ("J K", "move down / up"),
            ("u Ctrl+r", "undo / redo cell operation"),
        ],
    ),
    (
        "Outputs",
        &[
            ("o", "hide / show output"),
            ("c C", "clear output / all outputs"),
            ("[ ]", "scroll long output"),
            ("z", "zoom image fullscreen"),
            ("Z", "toggle native-size images"),
        ],
    ),
    (
        "File & app",
        &[
            ("w W", "save / force save"),
            ("S", "save as"),
            ("q", "quit"),
            ("L", "view logs"),
            ("D", "debug info for this cell"),
            ("?", "this help"),
        ],
    ),
    (
        "Mouse",
        &[
            ("click", "select · double-click edits"),
            ("wheel", "scroll view, or output under it"),
            ("Shift+drag", "native text selection"),
        ],
    ),
];

/// Width of the key column: the longest chord list.
const HELP_KEY_W: usize = 17;

fn help_section(title: &str, rows: &[(&str, &str)]) -> Vec<Line<'static>> {
    let key = Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD);
    let sep = Style::new().fg(Color::DarkGray);
    let mut lines = vec![Line::styled(
        title.to_string(),
        Style::new().fg(Color::Magenta).add_modifier(Modifier::BOLD),
    )];
    for (keys, what) in rows {
        // chords in the key color, the gaps between them dimmed
        let mut spans = vec![Span::raw("  ")];
        for (i, k) in keys.split(' ').enumerate() {
            if i > 0 {
                spans.push(Span::styled(" ", sep));
            }
            spans.push(Span::styled(k.to_string(), key));
        }
        spans.push(Span::raw(
            " ".repeat(HELP_KEY_W.saturating_sub(keys.chars().count())),
        ));
        spans.push(Span::raw(what.to_string()));
        lines.push(Line::from(spans));
    }
    lines.push(Line::raw(""));
    lines
}

fn draw_help(frame: &mut Frame) {
    let sections: Vec<Vec<Line>> = HELP.iter().map(|(t, r)| help_section(t, r)).collect();
    let col_w = sections
        .iter()
        .flatten()
        .map(Line::width)
        .max()
        .unwrap_or(0) as u16;
    let area = frame.area();
    // two columns when they fit: split where the halves balance by height
    let two = area.width >= 2 * col_w + 4 + 4;
    let total: usize = sections.iter().map(Vec::len).sum();
    let mut split = sections.len();
    if two {
        let mut acc = 0;
        for (i, s) in sections.iter().enumerate() {
            if acc + s.len() / 2 >= total / 2 {
                split = i;
                break;
            }
            acc += s.len();
        }
    }
    let (left, right) = sections.split_at(split);
    let left: Vec<Line> = left.iter().flatten().cloned().collect();
    let right: Vec<Line> = right.iter().flatten().cloned().collect();
    let height = left.len().max(right.len()).saturating_sub(1) as u16; // drop trailing blank
    let width = if two { 2 * col_w + 4 } else { col_w };
    let inner = modal(
        frame,
        centered(area, width + 4, height + 2),
        "jOtter 🦦 keys",
        "any key closes",
    );
    if two {
        let [l, _, r] = Layout::horizontal([
            Constraint::Length(col_w),
            Constraint::Length(4),
            Constraint::Length(col_w),
        ])
        .areas(inner);
        frame.render_widget(Paragraph::new(left), l);
        frame.render_widget(Paragraph::new(right), r);
    } else {
        frame.render_widget(Paragraph::new(left), inner);
    }
}

#[cfg(test)]
mod tests {
    #![allow(deprecated)] // Picker::from_fontsize is the only headless constructor
    use super::*;
    use ratatui_image::FontSize;
    use ratatui_image::picker::Picker;

    // 1x1 transparent png
    const PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAAC0lEQVR4nGNgAAIAAAUAAXpeqz8AAAAASUVORK5CYII=";

    /// Invalid code (`f"{}"`, `np.(`) gets the theme's `invalid` style: text in
    /// the background colour on a coloured background. It must stay visible.
    #[test]
    fn invalid_code_stays_visible_in_every_theme() {
        let nb = Notebook {
            cells: Vec::new(),
            extra: serde_json::Map::new(),
        };
        let mut r = Rendered::build(&nb, None, Default::default());
        for (name, theme) in ThemeSet::load_defaults().themes {
            let bg = theme.settings.background.map(|c| Color::Rgb(c.r, c.g, c.b));
            r.theme = theme;
            for src in ["x = f\"{}\"\n", "return np.(x, ddof=1) / np.sqrt(N)\n"] {
                for span in r.highlight(src, "python").iter().flat_map(|l| &l.spans) {
                    let hidden = span.style.fg == bg && span.style.bg.is_none();
                    assert!(
                        !hidden || span.content.trim().is_empty(),
                        "{name}: {:?} drawn in the background colour",
                        span.content
                    );
                }
            }
        }
    }

    #[test]
    fn visual_selection_ranges() {
        use ModeKind::*;
        // charwise, anchor below/after cursor (selection is direction-free)
        assert_eq!(sel_range(Visual, (1, 4), (0, 2), 0, 10), Some((2, 9)));
        assert_eq!(sel_range(Visual, (1, 4), (0, 2), 1, 10), Some((0, 4)));
        assert_eq!(sel_range(Visual, (0, 4), (0, 2), 0, 10), Some((2, 4)));
        assert_eq!(sel_range(Visual, (0, 0), (0, 0), 3, 5), None);
        // linewise: whole line for every row in range
        assert_eq!(sel_range(VisualLine, (0, 3), (2, 0), 1, 7), Some((0, 6)));
        // blockwise: same col range on every row
        assert_eq!(sel_range(VisualBlock, (0, 5), (2, 2), 1, 10), Some((2, 5)));
        assert_eq!(sel_range(Normal, (0, 0), (2, 0), 1, 5), None);
    }

    #[test]
    fn overlay_splits_spans_at_selection_bounds() {
        let mut line = Line::from(vec![Span::raw("abc"), Span::raw("def")]);
        overlay_reversed(&mut line, 2, 4); // "c" + "de"
        let flags: Vec<(String, bool)> = line
            .spans
            .iter()
            .map(|s| {
                (
                    s.content.to_string(),
                    s.style.add_modifier.contains(Modifier::REVERSED),
                )
            })
            .collect();
        assert_eq!(
            flags,
            [
                ("ab".into(), false),
                ("c".into(), true),
                ("de".into(), true),
                ("f".into(), false)
            ]
        );
        // empty line inside a linewise selection still shows a mark
        let mut empty = Line::raw("");
        overlay_reversed(&mut empty, 0, 0);
        assert_eq!(line_chars(&empty), 1);
    }

    #[test]
    fn try_image_steps() {
        let b64: String = PNG.split_whitespace().collect();
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(b64)
            .expect("b64 decode");
        let img = image::load_from_memory(&bytes).expect("png load");
        assert_eq!((img.width(), img.height()), (1, 1));
    }

    #[test]
    fn image_output_reserves_lines_and_creates_protocol() {
        let cell: Cell = serde_json::from_value(serde_json::json!({
            "cell_type": "code",
            "source": ["x"],
            "outputs": [{"output_type": "display_data", "metadata": {},
                         "data": {"image/png": PNG}}],
            "metadata": {}
        }))
        .unwrap();
        let nb = Notebook {
            cells: vec![cell],
            extra: serde_json::Map::new(),
        };
        let picker = Picker::from_fontsize(FontSize::new(8, 16));
        let mut r = Rendered::build(&nb, Some(picker), Default::default());
        r.ensure(0, &nb.cells[0]);
        let block = &r.blocks[0];
        assert_eq!(block.src_lines, 1);
        assert_eq!(block.images.len(), 1);
        // 1px image -> 1 row reserved
        assert_eq!(block.lines.len(), 1 + block.images[0].rows as usize);

        // without a picker: placeholder text instead
        let mut r = Rendered::build(&nb, None, Default::default());
        r.ensure(0, &nb.cells[0]);
        assert!(r.blocks[0].images.is_empty());
        assert_eq!(r.blocks[0].lines.len(), 2);
    }

    #[test]
    fn tall_images_prescale_to_row_cap_preserving_aspect() {
        let nb = Notebook {
            cells: vec![],
            extra: serde_json::Map::new(),
        };
        let picker = Picker::from_fontsize(FontSize::new(8, 16));
        let r = Rendered::build(&nb, Some(picker), Default::default());
        // 640x480 > 18*16=288 px tall -> prescaled to exactly the cap
        let entry = r
            .image_entry(image::DynamicImage::new_rgba8(640, 480), false)
            .unwrap();
        assert_eq!(entry.rows, max_image_rows());
        assert_eq!(entry.cols, 48); // 640 * 288/480 = 384 px / 8 px per col

        // small image: untouched, ceil to cells
        let entry = r
            .image_entry(image::DynamicImage::new_rgba8(100, 100), false)
            .unwrap();
        assert_eq!((entry.cols, entry.rows), (13, 7));

        // native-size toggle: no prescale, full 480 px = 30 rows
        let entry = r
            .image_entry(image::DynamicImage::new_rgba8(640, 480), true)
            .unwrap();
        assert_eq!((entry.cols, entry.rows), (80, 30));
    }

    #[test]
    fn all_inline_math_becomes_images() {
        let cell: Cell = serde_json::from_value(serde_json::json!({
            "cell_type": "markdown",
            "metadata": {},
            "source": "on the $\\tau$ grid, and $ \\int_0^\\infty \\frac{1}{x^2} $ too"
        }))
        .unwrap();
        let nb = Notebook {
            cells: vec![cell],
            extra: serde_json::Map::new(),
        };
        let picker = Picker::from_fontsize(FontSize::new(8, 16));
        let mut r = Rendered::build(&nb, Some(picker), Default::default());
        r.ensure(0, &nb.cells[0]);
        let block = &r.blocks[0];
        assert_eq!(block.images.len(), 2);
        // ALL inline math is exactly one row; too small => author uses $$
        assert!(block.images.iter().all(|i| i.rows == 1));
        assert_eq!(block.images[0].col, "on the ".len() as u16);
        assert_eq!(block.lines.len(), 1);
    }

    #[test]
    fn long_output_windows_scrolls_and_collapses() {
        let text: String = (0..40)
            .map(|i| format!("l{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let cell: Cell = serde_json::from_value(serde_json::json!({
            "cell_type": "code", "source": "x", "metadata": {},
            "outputs": [{"output_type": "stream", "name": "stdout", "text": text}]
        }))
        .unwrap();
        let nb = Notebook {
            cells: vec![cell],
            extra: serde_json::Map::new(),
        };
        let mut r = Rendered::build(&nb, None, Default::default());
        r.ensure(0, &nb.cells[0]);
        let b = &mut r.blocks[0];
        let len = b.out_len();
        assert!(len > max_output_rows());
        // default follows the tail
        assert_eq!(
            b.window(),
            (len - max_output_rows(), max_output_rows(), true)
        );
        b.scroll_output(-5);
        assert_eq!(b.win_start(), len - max_output_rows() - 5);
        b.scroll_output(100); // clamped and re-pinned to the tail
        assert_eq!(b.window().0, len - max_output_rows());
        b.collapsed = true;
        assert_eq!(b.window(), (0, 0, true));
        assert!(!b.output_scrollable());
    }

    #[test]
    fn images_never_force_an_output_viewport() {
        let nb = Notebook {
            cells: vec![],
            extra: serde_json::Map::new(),
        };
        let picker = Picker::from_fontsize(FontSize::new(8, 16));
        let r = Rendered::build(&nb, Some(picker), Default::default());
        // a full-size plot: 18 reserved rows, above the 15-row text budget
        let entry = r
            .image_entry(image::DynamicImage::new_rgba8(640, 480), false)
            .unwrap();
        let rows = entry.rows as usize;
        let mut block = CellBlock {
            lines: vec![Line::raw(""); 1 + rows + 1], // source + image + one text line
            src_lines: 1,
            images: vec![InlineImage { line: 1, ..entry }],
            src_map: Vec::new(),
            out_scroll: usize::MAX,
            collapsed: false,
            full_images: false,
            rendered: true,
            ..CellBlock::empty()
        };
        block.rewrap(0);
        assert_eq!(block.window(), (0, rows + 1, false), "plot arrives whole");
        // only genuine text spam scrolls: image rows extend the budget
        block.lines.extend(vec![Line::raw(""); 40]);
        block.rewrap(0);
        let (start, shown, footer) = block.window();
        assert!(footer);
        assert_eq!(shown, max_output_rows() + rows);
        assert_eq!(start, block.out_len() - shown);
    }

    #[test]
    fn markdown_click_map_points_at_source_lines() {
        let src = "# title\n\ntext\n| a |\n|---|\n| 1 |\nafter";
        let (_, _, map) = {
            let nb = Notebook {
                cells: vec![],
                extra: serde_json::Map::new(),
            };
            let r = Rendered::build(&nb, None, Default::default());
            r.render_markdown(src, None, false)
        };
        // heading, blank, text, 3 table rows, trailing line -> identity here
        assert_eq!(map, vec![0, 1, 2, 3, 4, 5, 6]);
    }

    #[test]
    fn markdown_links_emphasis_tables() {
        let src = "see [docs](https://x.y) with **bold** or *it*\n| a | b |\n|---|---|\n| 1 | 22 |";
        let cell: Cell = serde_json::from_value(serde_json::json!({
            "cell_type": "markdown", "metadata": {}, "source": src
        }))
        .unwrap();
        let nb = Notebook {
            cells: vec![cell],
            extra: serde_json::Map::new(),
        };
        let mut r = Rendered::build(&nb, None, Default::default());
        r.ensure(0, &nb.cells[0]);
        let text: Vec<String> = r.blocks[0]
            .lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect();
        assert!(
            text[0].contains("docs") && text[0].contains("⟨https://x.y⟩"),
            "{}",
            text[0]
        );
        assert!(text[0].contains("bold") && text[0].contains("it") && !text[0].contains('*'));
        assert!(text[1].contains(" a ") && text[1].contains('│'));
        assert!(text[2].contains('┼'));
        assert!(text[3].contains(" 1 ") && text[3].contains(" 22 "));
    }

    fn buffer_rows(term: &ratatui::Terminal<ratatui::backend::TestBackend>) -> Vec<String> {
        let buf = term.backend().buffer();
        (0..buf.area.height)
            .map(|y| {
                (0..buf.area.width)
                    .map(|x| buf[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect()
    }

    #[test]
    fn wrap_breaks_at_spaces_hard_breaks_long_words_and_keeps_nbsp() {
        let text = |w: &Wrapped| -> Vec<String> {
            w.rows
                .iter()
                .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
                .collect()
        };
        let red = Style::new().fg(Color::Red);
        let line = Line::from(vec![Span::raw("aaa bbb "), Span::styled("ccc", red)]);
        let w = wrap_lines(std::slice::from_ref(&line), 8);
        assert_eq!(text(&w), ["aaa bbb ", "ccc"]);
        assert_eq!(w.rows[1].spans[0].style, red, "styles survive the split");
        assert_eq!(
            w.origin[1],
            RowOrigin {
                line: 0,
                col: 8,
                x: 8
            }
        );
        // unwrapped when it fits, or with width 0
        assert_eq!(wrap_lines(std::slice::from_ref(&line), 11).rows.len(), 1);
        assert_eq!(wrap_lines(&[line], 0).rows.len(), 1);
        // a word longer than the row hard-breaks
        assert_eq!(
            text(&wrap_lines(&[Line::raw("abcdefghij")], 4)),
            ["abcd", "efgh", "ij"]
        );
        // NBSP-reserved inline math moves whole to the next row
        let math = format!("ab {}", "\u{a0}".repeat(4));
        let w = wrap_lines(&[Line::raw(math)], 5);
        assert_eq!(w.rows.len(), 2);
        assert_eq!(w.locate(0, 3), (1, 0), "image column maps onto the new row");
        // wide chars count double
        assert_eq!(text(&wrap_lines(&[Line::raw("日本語")], 4)), ["日本", "語"]);
        // row lookup for the editor cursor
        let w = wrap_lines(&[Line::raw("one two three"), Line::raw("x")], 8);
        assert_eq!(w.row_of_char(0, 10), (1, 8));
        assert_eq!(w.row_of_char(1, 0), (2, 0));
    }

    #[test]
    fn completion_popup_draws_aligned_columns_and_a_scrollbar() {
        use crate::app::Completion;
        let nb_json = serde_json::json!({
            "cells": [{"cell_type": "code", "id": "c", "metadata": {}, "execution_count": null,
                       "outputs": [], "source": ["x = np.li"]}],
            "metadata": {}, "nbformat": 4, "nbformat_minor": 5
        });
        let path = std::env::temp_dir().join(format!("jotter-pop-{}.ipynb", std::process::id()));
        std::fs::write(&path, nb_json.to_string()).unwrap();
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = crate::app::App::open(path.clone(), None, tx).unwrap();
        std::fs::remove_file(&path).ok();
        let mut rendered = Rendered::build(&app.notebook, None, Default::default());
        app.open_editor_for_test();
        let mut c = Completion::for_test(6, 9);
        for (i, name) in (0..20).map(|i| (i, format!(".li{i:02}"))) {
            c.push_for_test(
                &name,
                if i % 2 == 0 { "function" } else { "instance" },
                if i % 2 == 0 {
                    "(start, stop, num=50)"
                } else {
                    "int"
                },
            );
        }
        c.sel = 13; // scrolled past the first window
        app.completion = Some(c);
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(70, 20)).unwrap();
        terminal.draw(|f| draw(f, &mut app, &mut rendered)).unwrap();
        let rows = buffer_rows(&terminal);
        if std::env::var_os("JOTTER_SHOW").is_some() {
            println!("{}", rows.join("\n"));
        }
        let pop: Vec<&String> = rows
            .iter()
            .filter(|r| r.contains("Function") || r.contains("Variable"))
            .collect();
        assert_eq!(pop.len(), 12, "12-row window");
        // text column starts under the token after `np.` ("▌ x = np." = 9 cols)
        let col = |r: &str, pat: &str| r.find(pat).map(|b| r[..b].chars().count());
        assert!(
            pop.iter().all(|r| col(r, "li").is_some_and(|c| c == 9)),
            "{pop:#?}"
        );
        let kind_col = col(pop[0], "Variable").or(col(pop[0], "Function"));
        assert!(
            pop.iter()
                .all(|r| col(r, "Variable").or(col(r, "Function")) == kind_col)
        );
        assert!(pop.iter().any(|r| r.contains("li13")), "selection visible");
    }

    #[test]
    fn signature_popup_sits_under_the_line_with_the_argument_highlighted() {
        use crate::app::SigHelp;
        let nb_json = serde_json::json!({
            "cells": [{"cell_type": "code", "id": "c", "metadata": {}, "execution_count": null,
                       "outputs": [], "source": ["x = np.std(a, "]}],
            "metadata": {}, "nbformat": 4, "nbformat_minor": 5
        });
        let path = std::env::temp_dir().join(format!("jotter-sigpop-{}.ipynb", std::process::id()));
        std::fs::write(&path, nb_json.to_string()).unwrap();
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = crate::app::App::open(path.clone(), None, tx).unwrap();
        std::fs::remove_file(&path).ok();
        let mut rendered = Rendered::build(&app.notebook, None, Default::default());
        app.open_editor_for_test(); // insert mode, cursor at the end
        app.signature =
            Some(SigHelp::for_test("np.std(a, axis=None, ddof=0)", 1, None).placed(10, "np.std"));
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(60, 10)).unwrap();
        terminal.draw(|f| draw(f, &mut app, &mut rendered)).unwrap();
        let rows = buffer_rows(&terminal);
        let code = rows
            .iter()
            .position(|r| r.contains("x = np.std(a,"))
            .unwrap();
        let pop = &rows[code + 1];
        assert!(pop.contains("np.std(a, axis=None, ddof=0)"), "{rows:#?}");
        // its text starts under the callee
        let col = |r: &str, pat: &str| r.find(pat).map(|b| r[..b].chars().count());
        assert_eq!(col(pop, "np.std"), col(&rows[code], "np.std"));
        // `axis=None` is highlighted, the rest is not
        let buf = terminal.backend().buffer();
        let x = col(pop, "axis").unwrap() as u16;
        let style = |x: u16| buf[(x, code as u16 + 1)].style();
        assert!(style(x).add_modifier.contains(Modifier::BOLD));
        assert!(!style(x - 3).add_modifier.contains(Modifier::BOLD)); // `a, `
    }

    #[test]
    fn completion_window_scrolls_only_when_the_selection_leaves_it() {
        // down past the bottom: window follows by one
        assert_eq!(scroll_window(0, 12, 12, 30), 1);
        // back up inside the window: window stays put (the reported bug)
        assert_eq!(scroll_window(1, 11, 12, 30), 1);
        assert_eq!(scroll_window(1, 1, 12, 30), 1);
        // above the top: window follows up
        assert_eq!(scroll_window(1, 0, 12, 30), 0);
        // wrap to the last item from the top
        assert_eq!(scroll_window(0, 29, 12, 30), 18);
        assert_eq!(fit("abcdef", 4), "abc…");
        assert_eq!(fit("abc", 4), "abc");
    }

    #[test]
    fn tabs_are_visible_in_source_and_expanded_in_outputs() {
        let spans = show_tabs("\tx = 1", Style::new());
        let text: String = spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text, "→x = 1", "one visible column per tab");
        assert_eq!(
            expand_tabs("a\tb\n\x1b[31mab\tc"),
            "a       b\n\x1b[31mab      c"
        );
    }

    #[test]
    fn long_markdown_wraps_instead_of_clipping() {
        let prose = "word ".repeat(30);
        let nb_json = serde_json::json!({
            "cells": [{"cell_type": "markdown", "id": "m", "metadata": {}, "source": [prose.trim_end()]}],
            "metadata": {}, "nbformat": 4, "nbformat_minor": 5
        });
        let path = std::env::temp_dir().join(format!("jotter-wrap-{}.ipynb", std::process::id()));
        std::fs::write(&path, nb_json.to_string()).unwrap();
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = crate::app::App::open(path.clone(), None, tx).unwrap();
        std::fs::remove_file(&path).ok();
        let mut rendered = Rendered::build(&app.notebook, None, Default::default());
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(40, 12)).unwrap();
        terminal.draw(|f| draw(f, &mut app, &mut rendered)).unwrap();
        let words: usize = buffer_rows(&terminal)
            .iter()
            .map(|r| r.matches("word").count())
            .sum();
        assert_eq!(words, 30, "every word visible across wrapped rows");
        // clicks on a continuation row map back to the one source line
        assert!(matches!(
            app.hit[2].kind,
            crate::app::HitKind::Source { line: 0, .. }
        ));
    }

    #[test]
    fn overlays_are_padded_and_help_is_grouped() {
        let render = |w: u16, h: u16, f: &dyn Fn(&mut Frame)| {
            let mut t = ratatui::Terminal::new(ratatui::backend::TestBackend::new(w, h)).unwrap();
            t.draw(|fr| f(fr)).unwrap();
            buffer_rows(&t)
        };
        let rows = render(120, 45, &|f| draw_help(f));
        let all = rows.join("\n");
        if std::env::var_os("JOTTER_SHOW").is_some() {
            println!("{all}");
            println!(
                "{}",
                render(100, 14, &|f| draw_debug(
                    f,
                    "jotter 0.1.0\ncell 1/2 · id a1 · code\nrender: src 1 · out 40"
                ))
                .join("\n")
            );
        }
        for (heading, _) in HELP {
            assert!(all.contains(heading), "section {heading} missing:\n{all}");
        }
        assert!(all.contains("Navigate") && all.contains("Mouse"));
        // two columns at this width: Navigate and Mouse share a row range
        let row_of = |needle: &str| rows.iter().position(|r| r.contains(needle)).unwrap();
        assert!(row_of("Mouse") < row_of("Inside the editor") + 20);
        // padding: no content glyph directly after the left border
        for r in rows.iter().filter(|r| r.contains('│')) {
            let after = r.split('│').nth(1).unwrap_or("");
            assert!(after.starts_with(' '), "unpadded row: {r:?}");
        }
    }

    #[test]
    fn draw_windows_long_output_and_renders_cells() {
        let nb_json = serde_json::json!({
            "cells": [
                {"cell_type": "code", "execution_count": 3, "metadata": {},
                 "source": ["print('hi')"],
                 "outputs": [{"output_type": "stream", "name": "stdout",
                              "text": (0..30).map(|i| format!("line-{i}\n")).collect::<String>()}]},
                {"cell_type": "markdown", "metadata": {}, "source": ["# Big Title"]}
            ],
            "metadata": {}, "nbformat": 4, "nbformat_minor": 5
        });
        let path = std::env::temp_dir().join(format!("jotter-uitest-{}.ipynb", std::process::id()));
        std::fs::write(&path, nb_json.to_string()).unwrap();
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = crate::app::App::open(path.clone(), None, tx).unwrap();
        std::fs::remove_file(&path).ok();
        let mut rendered = Rendered::build(&app.notebook, None, Default::default());
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(60, 30)).unwrap();
        terminal.draw(|f| draw(f, &mut app, &mut rendered)).unwrap();
        let rows = buffer_rows(&terminal);
        let all = rows.join("\n");
        assert!(
            rows[0].starts_with("▌  In [3] ") && rows[0].trim_end().ends_with('─'),
            "selected prompt: {}",
            rows[0]
        );
        // 30 output lines -> viewport pinned to the tail, footer describes it
        assert!(all.contains("line-29"), "tail visible:\n{all}");
        assert!(!all.contains("line-0 "), "head clipped:\n{all}");
        assert!(all.contains("··· output"), "footer present:\n{all}");
        assert!(
            all.contains(" markdown  ─") && all.contains("Big Title"),
            "{all}"
        );
        assert!(rows[29].contains("cell 1/2"), "status line: {}", rows[29]);
        // hit map is window-relative and covers the viewport
        assert!(matches!(app.hit[0].kind, crate::app::HitKind::Other));
        assert_eq!(app.hit[0].cell, 0);
    }

    /// The layout pass measures what a render would, without highlighting
    /// or decoding: exactly, for everything but math.
    #[test]
    fn layout_pass_rows_match_the_render() {
        let png = {
            let mut buf = Vec::new();
            image::DynamicImage::new_rgba8(300, 500)
                .write_to(&mut std::io::Cursor::new(&mut buf), image::ImageFormat::Png)
                .unwrap();
            base64::engine::general_purpose::STANDARD.encode(buf)
        };
        let long = "word ".repeat(20);
        let cells: Vec<Cell> = [
            serde_json::json!({"cell_type": "code", "metadata": {},
                "source": format!("def f(x):\n\treturn x  # {long}\n"),
                "outputs": [
                    {"output_type": "stream", "name": "stdout", "text": format!("a\tb\n{long}\n")},
                    {"output_type": "display_data", "metadata": {}, "data": {"image/png": png}},
                    {"output_type": "execute_result", "metadata": {}, "data": {"text/plain": "3"}}]}),
            serde_json::json!({"cell_type": "markdown", "metadata": {},
                "source": format!("# Title\n- {long}\n```python\nx = 1  # {long}\n```\n| a | b |\n|---|---|\n| 1 | 2 |\n![p](attachment:p.png)"),
                "attachments": {"p.png": {"image/png": png}}}),
        ]
        .into_iter()
        .map(|v| serde_json::from_value(v).unwrap())
        .collect();
        let nb = Notebook {
            cells,
            extra: serde_json::Map::new(),
        };
        let picker = Picker::from_fontsize(FontSize::new(8, 16));
        for p in [Some(picker), None] {
            let mut r = Rendered::build(&nb, p, Default::default());
            for width in [0, 30, 80] {
                r.width = width;
                for (i, cell) in nb.cells.iter().enumerate() {
                    let est = r.render_cell(cell, false, true);
                    let real = r.render_cell(cell, false, false);
                    assert!(!est.rendered && real.rendered);
                    assert_eq!(est.rows, real.rows, "cell {i} at width {width}");
                }
            }
        }
    }

    /// A long notebook renders only what is near the screen, renders the end
    /// when it gets there, and keeps the view still when a cell above it
    /// turns from estimated to exact height.
    #[test]
    fn only_cells_near_the_screen_render() {
        let math = "$$\\sum_{n=0}^{\\infty} \\frac{x^n}{n!} = \\int_0^1 \\frac{\\mathrm{d}t}{\\sqrt{1-t^2}}$$";
        let cells: Vec<serde_json::Value> = (0..300)
            .map(|i| {
                if i % 2 == 0 {
                    serde_json::json!({"cell_type": "markdown", "metadata": {},
                                       "source": format!("cell {i}\n{math}")})
                } else {
                    serde_json::json!({"cell_type": "code", "metadata": {}, "execution_count": i,
                        "source": format!("print({i})"),
                        "outputs": [{"output_type": "stream", "name": "stdout", "text": format!("out-{i}\n")}]})
                }
            })
            .collect();
        let nb_json =
            serde_json::json!({"cells": cells, "metadata": {}, "nbformat": 4, "nbformat_minor": 5});
        let path = std::env::temp_dir().join(format!("jotter-lazy-{}.ipynb", std::process::id()));
        std::fs::write(&path, nb_json.to_string()).unwrap();
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = crate::app::App::open(path.clone(), None, tx).unwrap();
        std::fs::remove_file(&path).ok();
        let picker = Picker::from_fontsize(FontSize::new(8, 16));
        let mut rendered = Rendered::build(&app.notebook, Some(picker), Default::default());
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(60, 20)).unwrap();
        let mut draw_rows = |app: &mut crate::app::App, rendered: &mut Rendered| {
            terminal.draw(|f| draw(f, app, rendered)).unwrap();
            buffer_rows(&terminal)
        };
        let count = |r: &Rendered| r.blocks.iter().filter(|b| b.rendered).count();

        draw_rows(&mut app, &mut rendered);
        assert!(rendered.blocks[0].rendered);
        // the screen plus a screen of margin below: a handful of cells
        assert!(count(&rendered) < 20, "{} rendered", count(&rendered));

        // G: the end renders, the cells in between never do (the start stays
        // cached: it fits the memory budget; eviction has its own test)
        app.selected = 299;
        let rows = draw_rows(&mut app, &mut rendered);
        assert!(rows.iter().any(|r| r.contains("out-299")), "{rows:#?}");
        assert!(rendered.blocks[299].rendered && !rendered.blocks[150].rendered);
        assert!(count(&rendered) < 30, "{} rendered", count(&rendered));

        // a math cell above the view gets its exact height: the screen stays put
        app.manual_scroll = true;
        let before = draw_rows(&mut app, &mut rendered);
        let above = (0..299)
            .rev()
            .find(|&i| i % 2 == 0 && !rendered.blocks[i].rendered)
            .unwrap();
        let est = rendered.blocks[above].rows;
        rendered.ensure(above, &app.notebook.cells[above]);
        assert_ne!(rendered.blocks[above].rows, est, "the estimate was off");
        let scroll = app.scroll;
        assert_eq!(draw_rows(&mut app, &mut rendered), before);
        assert_ne!(app.scroll, scroll, "the view moved with the content");
    }

    #[test]
    fn markdown_renders_rich() {
        let cell: Cell = serde_json::from_value(serde_json::json!({
            "cell_type": "markdown",
            "metadata": {},
            "source": "# Title\n- item\ncall `f(x)` on $\\tau$\n$$E = \\hbar \\omega^2$$"
        }))
        .unwrap();
        let nb = Notebook {
            cells: vec![cell],
            extra: serde_json::Map::new(),
        };
        let mut r = Rendered::build(&nb, None, Default::default()); // no graphics -> math becomes unicode approx
        r.ensure(0, &nb.cells[0]);
        let text: Vec<String> = r.blocks[0]
            .lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect();
        assert_eq!(text[0], "Title"); // hashes stripped
        assert_eq!(text[1], "• item");
        assert!(text[2].contains("f(x)") && text[2].contains('τ'));
        assert!(text[3].contains("E = ℏ ω²"));
    }

    /// Sweeping through a long notebook renders each cell once: content
    /// stays cached within the budget instead of being evicted and rendered
    /// again on the way back (which also re-sent every kitty image).
    #[test]
    fn sweeping_a_notebook_renders_each_cell_once() {
        use std::sync::atomic::Ordering::Relaxed;
        let src = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/showcase.ipynb"))
            .unwrap();
        let mut nb: serde_json::Value = serde_json::from_str(&src).unwrap();
        let cells = nb["cells"].as_array().unwrap().clone();
        nb["cells"] = serde_json::Value::Array((0..5).flat_map(|_| cells.clone()).collect());
        for (i, c) in nb["cells"].as_array_mut().unwrap().iter_mut().enumerate() {
            c["id"] = format!("c{i}").into();
        }
        let path = std::env::temp_dir().join(format!("jotter-sweep-{}.ipynb", std::process::id()));
        std::fs::write(&path, nb.to_string()).unwrap();
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = crate::app::App::open(path.clone(), None, tx).unwrap();
        std::fs::remove_file(&path).ok();
        let picker = Picker::from_fontsize(FontSize::new(8, 16));
        let mut r = Rendered::build(&app.notebook, Some(picker), Default::default());
        let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(120, 36)).unwrap();
        let before = RENDERS.load(Relaxed);
        let n = app.notebook.cells.len();
        let key = |c| {
            crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Char(c),
                crossterm::event::KeyModifiers::NONE,
            )
        };
        term.draw(|f| draw(f, &mut app, &mut r)).unwrap();
        for _ in 0..3 {
            for c in ['j', 'k'] {
                for _ in 0..n - 1 {
                    app.on_key(key(c), &mut r);
                    term.draw(|f| draw(f, &mut app, &mut r)).unwrap();
                }
            }
        }
        // other tests render concurrently: the count is an upper bound for
        // this one, so allow their share without letting a re-render per
        // sweep (3 sweeps x 70 cells) through
        let renders = RENDERS.load(Relaxed) - before;
        assert!(
            renders < 2 * n,
            "{renders} renders for {n} cells in 3 sweeps"
        );
    }

    /// Over the budget, the least recently seen blocks go first, and only
    /// until the rest fits; the render window is never evicted.
    #[test]
    fn eviction_drops_least_recently_seen_until_within_budget() {
        let nb = Notebook {
            cells: Vec::new(),
            extra: serde_json::Map::new(),
        };
        let mut r = Rendered::build(&nb, None, Default::default());
        // three blocks of ~31 MiB each (lines cost 256 bytes): 92 MiB > 64
        let big = |seen| {
            let mut b = CellBlock::empty();
            b.lines = vec![Line::default(); 120_000];
            b.rendered = true;
            b.seen = seen;
            b
        };
        r.blocks = vec![big(5), big(1), big(9), big(3)];
        r.evict(|i| i == 1); // block 1 is in the render window
        let kept: Vec<bool> = r.blocks.iter().map(|b| b.rendered).collect();
        // oldest outside the window is 3 (seen 3), then 0 (seen 5): two go
        assert_eq!(kept, [false, true, true, false]);
    }
}
