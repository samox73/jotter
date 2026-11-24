//! The render cache: one `CellBlock` per cell, built lazily and evicted by budget.

use ansi_to_tui::IntoText;
use base64::Engine;
use ratatui::{
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
};
use ratatui_image::{
    picker::{Capability, Picker, ProtocolType},
    protocol::kitty::Kitty,
    sliced::SlicedProtocol,
};
use serde_json::Value;
use syntect::{
    easy::HighlightLines,
    highlighting::{Theme, ThemeSet},
    parsing::SyntaxSet,
    util::LinesWithEndings,
};
use unicode_width::UnicodeWidthChar;

use super::{
    ERROR_STYLE, OUTPUT_STYLE, STDERR_STYLE,
    block::{CellBlock, InlineImage, KITTY_TRANSMIT, KittyImage},
    max_image_rows,
};
use crate::notebook::{Cell, Notebook, join_multiline};

/// Rendered content (see `CellBlock::cost`) kept before the least recently
/// seen cells are evicted. A notebook within it renders each cell once.
const RENDER_BUDGET: usize = 64 << 20;

/// Full cell renders, counted for the tests.
#[cfg(test)]
pub(super) static RENDERS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

pub struct Rendered {
    pub(super) ps: SyntaxSet,
    pub(super) theme: Theme,
    pub(super) picker: Option<Picker>,
    /// Notebook directory: markdown image paths resolve relative to it.
    pub(super) dir: std::path::PathBuf,
    /// Code-cell language (syntect token: "python", "r", ...), from the
    /// notebook's language_info / kernelspec metadata.
    pub(super) lang: String,
    /// Body width blocks are wrapped to (0 until the first draw).
    pub(super) width: u16,
    pub blocks: Vec<CellBlock>,
    /// Frames drawn, for the blocks' `seen`.
    pub(super) frame: u64,
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
        // syntect's own set plus bat's: Julia, TOML, TypeScript, ...
        let ps = two_face::syntax::extra_newlines();
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
        let mut total: usize = self.blocks.iter().filter(|b| b.rendered).map(CellBlock::cost).sum();
        if total <= RENDER_BUDGET {
            return;
        }
        let mut old: Vec<usize> =
            (0..self.blocks.len()).filter(|&i| self.blocks[i].rendered && !keep(i)).collect();
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
    pub(super) fn cell_lang(&self, cell: &Cell) -> &str {
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
        let syntax =
            self.ps.find_syntax_by_token(lang).unwrap_or_else(|| self.ps.find_syntax_plain_text());
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
    pub(super) fn plain(source: &str) -> Vec<Line<'static>> {
        LinesWithEndings::from(source)
            .map(|l| Line::from(show_tabs(l.trim_end_matches('\n'), Style::default())))
            .collect()
    }

    /// Render a cell. `est` is the layout pass: only the row counts of the
    /// result are kept, so it skips highlighting and image decoding (sizes
    /// come from image headers) and guesses rendered math's size.
    pub(super) fn render_cell(&self, cell: &Cell, full_images: bool, est: bool) -> CellBlock {
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
            (self.highlight(&cell.source, self.cell_lang(cell)), Vec::new(), Vec::new())
        };
        let src_lines = lines.len();
        for output in cell.outputs.iter().flatten() {
            let data = output.get("data");
            // A kernel that offers text/html means latex and markdown as
            // stand-ins for it (IRkernel sends all of them for `1:3`, the
            // markdown being a numbered list): text/plain reads better here.
            let typeset = data.is_none_or(|d| d.get("text/html").is_none());
            if let Some(img) = self
                .try_image(output, full_images, est)
                .or_else(|| typeset.then(|| self.try_latex(output, est))?)
            {
                self.push_image(img, &mut lines, &mut images);
            } else if let Some(md) = data.and_then(|d| d.get("text/markdown")).filter(|_| typeset) {
                let (md_lines, md_images, _) = self.render_markdown(&join_multiline(md), None, est);
                let base = lines.len();
                lines.extend(md_lines);
                images.extend(
                    md_images.into_iter().map(|im| InlineImage { line: im.line + base, ..im }),
                );
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
    pub(super) fn push_image(
        &self,
        entry: InlineImage,
        lines: &mut Vec<Line<'static>>,
        images: &mut Vec<InlineImage>,
    ) {
        let entry = InlineImage { line: lines.len(), ..entry };
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
    pub(super) fn image_entry(&self, img: image::DynamicImage, full: bool) -> Option<InlineImage> {
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
            let compress = picker.capabilities().contains(&Capability::KittyCompression);
            let tmux = picker.tmux_detected();
            let kitty = Kitty::new(img, size, id, tmux, compress).ok()?;
            entry.kitty = Some(KittyImage { id, tmux });
            KITTY_TRANSMIT.store(true, std::sync::atomic::Ordering::Relaxed);
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
    pub(super) fn sized(&self, px: (u32, u32), full: bool) -> Option<InlineImage> {
        self.sized_px(self.shown_px(px, full)?)
    }

    /// Layout pass: rendered math, guessed at `cols` x `rows` cells.
    pub(super) fn sized_math(&self, cols: usize, rows: u16) -> Option<InlineImage> {
        let font = self.picker.as_ref()?.font_size();
        self.sized_px((cols.max(1) as u32 * font.width as u32, rows as u32 * font.height as u32))
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
            return image::load_from_memory(&b64_bytes(v)?).ok().map(crate::recolor::apply);
        }
        let svg = join_multiline(data.get("image/svg+xml")?);
        Some(crate::recolor::apply(image::DynamicImage::ImageRgba8(crate::latex::render_svg(
            &svg,
        )?)))
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
        let img = cell.outputs.iter().flatten().find_map(|o| self.decode_output_image(o))?;
        let font = picker.font_size();
        let (max_w, max_h) = (
            (area.width as u32 * font.width as u32).max(1),
            (area.height as u32 * font.height as u32).max(1),
        );
        // fits the screen both ways: a small figure is enlarged
        let img = img.resize(max_w, max_h, image::imageops::FilterType::Lanczos3);
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

    pub(super) fn push_math(
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
                        Style::new().fg(Color::Magenta).add_modifier(Modifier::ITALIC),
                    ));
                }
            }
        }
    }
}

/// Display math rows a layout pass assumes before rendering (capped by
/// max_math_rows); the real height corrects it once the cell is on screen.
const MATH_ROWS_GUESS: u16 = 2;

/// An output's base64 payload, decoded.
pub(super) fn b64_bytes(v: &Value) -> Option<Vec<u8>> {
    let b64: String = join_multiline(v).split_whitespace().collect();
    base64::engine::general_purpose::STANDARD.decode(b64).ok()
}

/// Pixel size of an encoded raster image, read from its header.
pub(super) fn bytes_px(bytes: &[u8]) -> Option<(u32, u32)> {
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
pub(super) fn show_tabs(text: &str, style: Style) -> Vec<Span<'static>> {
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
pub(super) fn expand_tabs(text: &str) -> String {
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

pub(super) fn output_lines(output: &Value) -> Vec<Line<'static>> {
    let (text, style) = match output["output_type"].as_str() {
        Some("stream") => (
            join_multiline(&output["text"]),
            if output["name"] == "stderr" { STDERR_STYLE } else { OUTPUT_STYLE },
        ),
        Some("execute_result") | Some("display_data") => {
            let data = &output["data"];
            let plain = join_multiline(&data["text/plain"]);
            if !plain.is_empty() {
                (plain, OUTPUT_STYLE)
            } else if let Some(obj) = data.as_object() {
                // no renderable rep (or no graphics): name what's there
                (obj.keys().map(|k| format!("[{k}]")).collect::<Vec<_>>().join(" "), OUTPUT_STYLE)
            } else {
                return vec![];
            }
        }
        Some("error") => (
            match &output["traceback"] {
                Value::Array(tb) => {
                    tb.iter().filter_map(|l| l.as_str()).collect::<Vec<_>>().join("\n")
                }
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
            .map(|mut l| {
                if l.spans.iter().all(|s| s.style == Style::default()) {
                    return l.style(style);
                }
                // IPython < 9.14 marks a traceback's error range with only
                // bg:ansiyellow, leaving syntax colours unreadable on it
                for s in &mut l.spans {
                    if matches!(s.style.bg, Some(Color::Yellow | Color::LightYellow)) {
                        s.style.fg = Some(Color::Black);
                    }
                }
                l
            })
            .collect(),
        Err(_) => text.lines().map(|l| Line::styled(l.to_string(), style)).collect(),
    }
}
