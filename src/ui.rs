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
use ratatui_image::picker::Picker;
use ratatui_image::sliced::{SignedPosition, SlicedImage, SlicedProtocol};
use serde_json::Value;
use syntect::easy::HighlightLines;
use syntect::highlighting::{Theme, ThemeSet};
use syntect::parsing::SyntaxSet;
use syntect::util::LinesWithEndings;

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
    proto: SlicedProtocol,
}

pub struct CellBlock {
    pub lines: Vec<Line<'static>>,
    /// How many of `lines` are source (the rest are outputs).
    pub src_lines: usize,
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
}

impl CellBlock {
    fn out_len(&self) -> usize {
        self.lines.len() - self.src_lines
    }

    /// Output-viewport budget: the row cap is for *text* spam — images are
    /// display units and extend the budget instead of being windowed away
    /// (a default matplotlib figure must never arrive cropped).
    fn out_cap(&self) -> usize {
        let image_rows: usize = self
            .images
            .iter()
            .filter(|i| i.line >= self.src_lines)
            .map(|i| i.rows as usize)
            .sum();
        max_output_rows() + image_rows
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
}

pub struct Rendered {
    ps: SyntaxSet,
    theme: Theme,
    picker: Option<Picker>,
    /// Notebook directory: markdown image paths resolve relative to it.
    dir: std::path::PathBuf,
    pub blocks: Vec<CellBlock>,
}

fn cell_ext(cell: &Cell) -> &'static str {
    match cell.cell_type.as_str() {
        "code" => "py",
        "markdown" => "md",
        _ => "txt",
    }
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
            blocks: Vec::new(),
        };
        r.blocks = nb.cells.iter().map(|c| r.render_cell(c, false)).collect();
        r
    }

    pub fn rebuild_cell(&mut self, idx: usize, cell: &Cell) {
        // keep viewport position, image sizing, and timing across rebuilds
        let old = &self.blocks[idx];
        let (out_scroll, collapsed, elapsed, full) =
            (old.out_scroll, old.collapsed, old.elapsed, old.full_images);
        self.blocks[idx] = self.render_cell(cell, full);
        self.blocks[idx].out_scroll = out_scroll;
        self.blocks[idx].collapsed = collapsed;
        self.blocks[idx].elapsed = elapsed;
    }

    pub fn insert_cell(&mut self, idx: usize, cell: &Cell) {
        self.blocks.insert(idx, self.render_cell(cell, false));
    }

    pub fn remove_cell(&mut self, idx: usize) {
        self.blocks.remove(idx);
    }

    pub fn swap_cells(&mut self, a: usize, b: usize) {
        self.blocks.swap(a, b);
    }

    /// Re-render every cell (external reload, font-size change).
    pub fn rebuild_all(&mut self, nb: &Notebook) {
        self.blocks = nb
            .cells
            .iter()
            .map(|c| self.render_cell(c, false))
            .collect();
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

    /// Syntax-highlight source text (used for cached blocks and the live editor).
    /// `lang` may be a name ("rust", "python") or an extension ("rs", "py").
    pub fn highlight(&self, source: &str, lang: &str) -> Vec<Line<'static>> {
        let syntax = self
            .ps
            .find_syntax_by_token(lang)
            .unwrap_or_else(|| self.ps.find_syntax_plain_text());
        let mut hl = HighlightLines::new(syntax, &self.theme);
        LinesWithEndings::from(source)
            .map(|line| {
                let spans = hl
                    .highlight_line(line, &self.ps)
                    .unwrap_or_default()
                    .into_iter()
                    .map(|(st, txt)| {
                        let fg = st.foreground;
                        Span::styled(
                            txt.trim_end_matches('\n').to_string(),
                            Style::default().fg(Color::Rgb(fg.r, fg.g, fg.b)),
                        )
                    })
                    .collect::<Vec<_>>();
                Line::from(spans)
            })
            .collect()
    }

    fn render_cell(&self, cell: &Cell, full_images: bool) -> CellBlock {
        let (mut lines, mut images, src_map) = if cell.cell_type == "markdown" {
            self.render_markdown(&cell.source, cell.extra.get("attachments"))
        } else if is_latex_raw(cell) {
            // raw cell with metadata.format = text/latex: whole cell is math
            let (mut l, mut im) = (Vec::new(), Vec::new());
            self.push_math(cell.source.trim().trim_matches('$'), &mut l, &mut im);
            (l, im, Vec::new())
        } else {
            (
                self.highlight(&cell.source, cell_ext(cell)),
                Vec::new(),
                Vec::new(),
            )
        };
        let src_lines = lines.len();
        for output in cell.outputs.iter().flatten() {
            if let Some(img) = self
                .try_image(output, full_images)
                .or_else(|| self.try_latex(output))
            {
                self.push_image(img, &mut lines, &mut images);
            } else if let Some(md) = output.get("data").and_then(|d| d.get("text/markdown")) {
                let (md_lines, md_images, _) = self.render_markdown(&join_multiline(md), None);
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
        CellBlock {
            lines,
            src_lines,
            images,
            src_map,
            out_scroll: usize::MAX,
            collapsed: false,
            full_images,
            elapsed: None,
        }
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
        let fh = picker.font_size().height as u32;
        let max_h = max_image_rows() as u32 * fh;
        let img = if !full && img.height() > max_h {
            let w = (img.width() as u64 * max_h as u64 / img.height() as u64).max(1) as u32;
            img.resize_exact(w, max_h, image::imageops::FilterType::Lanczos3)
        } else {
            img
        };
        let proto = SlicedProtocol::new(picker, img, None).ok()?;
        let size = proto.size();
        Some(InlineImage {
            line: 0,
            col: 0,
            cols: size.width.max(1),
            rows: size.height.max(1),
            proto,
        })
    }

    /// Decode an output's raster/SVG image at its native size.
    fn decode_output_image(&self, output: &Value) -> Option<image::DynamicImage> {
        let data = output.get("data")?;
        for mime in ["image/png", "image/jpeg", "image/gif"] {
            let Some(v) = data.get(mime) else { continue };
            let b64: String = join_multiline(v).split_whitespace().collect();
            let bytes = base64::engine::general_purpose::STANDARD.decode(b64).ok()?;
            return image::load_from_memory(&bytes).ok();
        }
        let svg = join_multiline(data.get("image/svg+xml")?);
        Some(image::DynamicImage::ImageRgba8(crate::latex::render_svg(
            &svg,
        )?))
    }

    /// If the output carries a raster/SVG image and graphics are available,
    /// build a protocol; `full` skips the height cap (native-size toggle).
    fn try_image(&self, output: &Value, full: bool) -> Option<InlineImage> {
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
    fn try_latex(&self, output: &Value) -> Option<InlineImage> {
        let picker = self.picker.as_ref()?;
        let tex = join_multiline(output.get("data")?.get("text/latex")?);
        let tex = tex.trim().trim_matches('$').replace("\\displaystyle", "");
        let img = crate::latex::render_math(&tex, picker.font_size().height)?;
        self.image_entry(image::DynamicImage::ImageRgba8(img), false)
    }

    fn push_math(&self, tex: &str, lines: &mut Vec<Line<'static>>, images: &mut Vec<InlineImage>) {
        let rendered = self
            .picker
            .as_ref()
            .and_then(|p| crate::latex::render_math(tex, p.font_size().height))
            .and_then(|img| self.image_entry(image::DynamicImage::ImageRgba8(img), false));
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
                    let lang = if lang.is_empty() { "py" } else { lang.as_str() };
                    let body = self.highlight(buf, lang);
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
                    self.push_math(&tex, &mut lines, &mut images);
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
                match self.md_image(src, attachments) {
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
                        self.push_math(inner, &mut lines, &mut images)
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
            let line = self.md_line(raw, line_idx, &mut images);
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
    fn md_image(&self, src: &str, attachments: Option<&Value>) -> Option<InlineImage> {
        if let Some(name) = src.strip_prefix("attachment:") {
            for v in attachments?.get(name)?.as_object()?.values() {
                let b64: String = join_multiline(v).split_whitespace().collect();
                if let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(b64)
                    && let Ok(img) = image::load_from_memory(&bytes)
                {
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
            return self.image_entry(
                image::DynamicImage::ImageRgba8(crate::latex::render_svg(&svg)?),
                false,
            );
        }
        self.image_entry(image::open(path).ok()?, false)
    }

    /// Inline markdown spans: bullets, `code`, $math$, [links](url),
    /// *emphasis*/**bold**, ![inline images] as alt text. Inline math renders
    /// in TeX text style as a one-row image in reserved columns.
    fn md_line(&self, raw: &str, line_idx: usize, images: &mut Vec<InlineImage>) -> Line<'static> {
        let code_style = Style::new().fg(Color::Yellow);
        let math_style = Style::new()
            .fg(Color::Magenta)
            .add_modifier(Modifier::ITALIC);
        let link_style = Style::new()
            .fg(Color::Blue)
            .add_modifier(Modifier::UNDERLINED);
        let url_style = Style::new().fg(Color::DarkGray);
        let mut spans: Vec<Span<'static>> = Vec::new();
        let mut col = 0usize; // ponytail: chars == columns; wide chars drift, fix with unicode-width if it bites
        let mut text = String::new();
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
                *col += text.chars().count();
                spans.push(Span::raw(std::mem::take(text)));
            }
        };
        let push_styled = |s: String,
                           style: Style,
                           text: &mut String,
                           spans: &mut Vec<Span<'static>>,
                           col: &mut usize| {
            flush(text, spans, col);
            *col += s.chars().count();
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
                        let entry = self
                            .picker
                            .as_ref()
                            .and_then(|p| crate::latex::render_inline(&inner, p.font_size().height))
                            .and_then(|img| {
                                self.image_entry(image::DynamicImage::ImageRgba8(img), false)
                            });
                        if let Some(entry) = entry {
                            // reserve the columns in the text flow
                            spans.push(Span::raw(" ".repeat(entry.cols as usize)));
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
                    col += shown.chars().count();
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
    let text = text.replace('\r', "");
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

/// `run`: None = idle, Some(false) = queued, Some(true) = executing.
fn prompt_line(
    cell: &Cell,
    selected: bool,
    run: Option<bool>,
    editing: bool,
    elapsed: Option<std::time::Duration>,
) -> Line<'static> {
    let label = match cell.cell_type.as_str() {
        "code" => match run {
            Some(true) => "In [*]:".to_string(),
            Some(false) => "In [·]:".to_string(),
            None => match cell.execution_count() {
                Some(n) => format!("In [{n}]:"),
                None => "In [ ]:".to_string(),
            },
        },
        other => format!("[{other}]"),
    };
    let mut style = Style::default()
        .fg(if cell.cell_type == "code" {
            Color::Cyan
        } else {
            Color::Magenta
        })
        .add_modifier(Modifier::BOLD);
    let marker = if selected {
        style = style.add_modifier(Modifier::REVERSED);
        if editing { "✎ " } else { "▶ " }
    } else {
        "  "
    };
    let mut spans = vec![Span::raw(marker), Span::styled(label, style)];
    if run.is_none()
        && let Some(d) = elapsed
    {
        spans.push(Span::styled(
            format!(" {}", fmt_duration(d)),
            Style::new().fg(Color::DarkGray),
        ));
    }
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
    // one chokepoint records every status message into the history + log
    if let Some(m) = &app.message
        && app.history.last() != Some(m)
    {
        log::info!(target: "status", "{m}");
        app.history.push(m.clone());
        if app.history.len() > 200 {
            app.history.remove(0);
        }
    }
    let [body, status] =
        Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).areas(frame.area());
    let height = body.height as usize;

    // The editing cell renders live from the editor buffer, highlighted once
    // per frame; editor.lines is authoritative for its displayed row count.
    let editing_cell = app.editor.as_ref().map(|_| app.selected);
    let editor_lines: Vec<Line> = match (&app.editor, app.notebook.cells.get(app.selected)) {
        (Some(ed), Some(cell)) => rendered.highlight(&ed.source(), cell_ext(cell)),
        _ => Vec::new(),
    };
    let editor_rows = app.editor.as_ref().map_or(0, |e| e.line_count());
    // visual-mode selection (nvim backend), applied as an overlay on the
    // highlighted source spans of the editing cell
    let visual = app.editor.as_ref().and_then(|e| e.visual());

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
    let view = |i: usize| -> CellView {
        let block = &rendered.blocks[i];
        let src = if editing_cell == Some(i) {
            editor_rows
        } else {
            block.src_lines
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
    let mut starts: Vec<usize> = Vec::with_capacity(app.notebook.cells.len());
    let mut total = 0usize;
    for i in 0..app.notebook.cells.len() {
        starts.push(total);
        total += 1 + view(i).body() + 1; // prompt + body + separator
    }
    app.content_lines = total;
    app.body = body;

    // Keep selection visible (cursor line when editing, whole cell otherwise) —
    // unless the user wheel-scrolled away; then only clamp to the content.
    if app.manual_scroll || starts.is_empty() {
        app.scroll = app.scroll.min(total.saturating_sub(1));
    } else if let Some(editor) = &app.editor {
        let cur = starts[app.selected] + 1 + editor.cursor().0;
        if cur >= app.scroll + height {
            app.scroll = cur + 1 - height;
        }
        if cur < app.scroll {
            app.scroll = cur;
        }
    } else {
        let sel_start = starts[app.selected];
        let sel_end = sel_start + 1 + view(app.selected).body();
        if sel_end > app.scroll + height {
            app.scroll = sel_end - height;
        }
        if sel_start < app.scroll {
            app.scroll = sel_start;
        }
    }

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
        let v = view(ci);
        let local = a - starts[ci];
        let (line, kind) = if local == 0 {
            let run = app
                .running
                .iter()
                .find_map(|(m, &c)| (c == ci).then(|| app.started.contains_key(m)));
            (
                prompt_line(cell, ci == app.selected, run, editing, block.elapsed),
                HitKind::Other,
            )
        } else if local <= v.src {
            let b = local - 1;
            let line = if editing {
                let mut l = editor_lines
                    .get(b)
                    .cloned()
                    .unwrap_or_else(|| Line::raw(""));
                if let Some((kind, anchor, cursor)) = visual
                    && let Some((from, to)) = sel_range(kind, anchor, cursor, b, line_chars(&l))
                {
                    overlay_reversed(&mut l, from, to);
                }
                l
            } else {
                block.lines[b].clone()
            };
            let kind = if editing || (cell.cell_type != "markdown" && !is_latex_raw(cell)) {
                HitKind::Source(b)
            } else if let Some(&s) = block.src_map.get(b) {
                HitKind::Source(s) // rendered markdown row -> raw source line
            } else {
                HitKind::Other // latex raw: no per-row mapping
            };
            (line, kind)
        } else if local <= v.src + v.win_rows {
            let out_row = v.win_start + (local - 1 - v.src);
            (
                block.lines[block.src_lines + out_row].clone(),
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
        let v = view(i);
        for img in &block.images {
            let in_src = img.line < block.src_lines;
            // editing replaces the source region: markdown-math images hide
            if in_src && editing {
                continue;
            }
            if img.col >= body.width {
                continue;
            }
            let (area, pos) = if in_src {
                // source images: straight mapping, clipped by the viewport
                let rel = starts[i] as isize + 1 + img.line as isize - app.scroll as isize;
                if rel + (img.rows as isize) <= 0 || rel >= body.height as isize {
                    continue;
                }
                (
                    body,
                    SignedPosition {
                        x: img.col as i16,
                        y: rel as i16,
                    },
                )
            } else {
                // output images: clipped by the output window, then the viewport
                let ol = img.line - block.src_lines;
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
                        x: img.col as i16,
                        y: (img_rel - y0) as i16,
                    },
                )
            };
            frame.render_widget(SlicedImage::new(&img.proto, pos), area);
        }
    }

    // Editor cursor: real terminal cursor at the edit position.
    if let Some(editor) = &app.editor {
        let (crow, ccol) = editor.cursor();
        let abs = starts[app.selected] + 1 + crow;
        if abs >= app.scroll && ((abs - app.scroll) as u16) < body.height {
            frame.set_cursor_position((
                body.x + (ccol as u16).min(body.width.saturating_sub(1)),
                body.y + (abs - app.scroll) as u16,
            ));
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
    // rightmost slot: search prompt, then nvim's cmdline, then the message
    let tail = if let Some(q) = &app.search_input {
        format!("/{q}▏")
    } else if let Some(c) = app.editor.as_ref().and_then(|e| e.cmdline()) {
        format!("{c}▏")
    } else {
        app.message.clone().unwrap_or_default()
    };
    let status_line = Line::from(vec![
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
    ]);
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
    if app.show_help {
        draw_help(frame);
    }
    if app.show_history {
        draw_history(frame, &app.history);
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

/// Kernel `input()`: a centered modal prompt (all keys already route to it).
fn draw_stdin(frame: &mut Frame, req: &crate::app::StdinReq) {
    use ratatui::widgets::{Block, Clear};
    let area = frame.area();
    let width = area.width.saturating_sub(6).clamp(24, 70).min(area.width);
    let popup = Rect {
        x: (area.width - width) / 2,
        y: area.height.saturating_sub(3) / 2,
        width,
        height: 3.min(area.height),
    };
    let shown: String = if req.password {
        "•".repeat(req.buf.chars().count())
    } else {
        req.buf.clone()
    };
    // keep the tail (and the cursor) inside the box when the input outgrows it
    let inner = width.saturating_sub(2) as usize;
    let skip = shown
        .chars()
        .count()
        .saturating_sub(inner.saturating_sub(1));
    let tail: String = shown.chars().skip(skip).collect();
    let title = match req.prompt.trim() {
        "" => " input() — Enter sends, Esc sends empty ".to_string(),
        p => format!(" {p} "),
    };
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(tail.as_str()).block(Block::bordered().title(title)),
        popup,
    );
    frame.set_cursor_position((popup.x + 1 + tail.chars().count() as u16, popup.y + 1));
}

/// `M`: recent status messages (kernel errors, saves, ...), newest at the
/// bottom. Any key closes.
fn draw_history(frame: &mut Frame, history: &[String]) {
    use ratatui::widgets::{Block, Clear};
    let area = frame.area();
    let width = area.width.saturating_sub(4).clamp(20, 100);
    let rows = (area.height.saturating_sub(4) as usize).min(history.len().max(1));
    let text = history
        .iter()
        .rev()
        .take(rows)
        .rev()
        .map(|m| Line::raw(m.as_str()))
        .collect::<Vec<_>>();
    let popup = Rect {
        x: (area.width - width) / 2,
        y: area.height.saturating_sub(rows as u16 + 2) / 2,
        width,
        height: rows as u16 + 2,
    };
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(text).block(Block::bordered().title(" messages (any key closes) ")),
        popup,
    );
}

const HELP: &str = "\
 j/k g/G      select / first / last cell    5G  goto cell 5
 Enter, i, A  edit cell (vi: hjkl wbe 0$^ ggG x dd yy p u ...)
 Esc          exit editing (normal mode)
 E            edit cell in $EDITOR
 Shift+Enter/r  run + advance  Ctrl+Enter  run
 Ctrl+r       run all          < / >  run all above / cell+below
 a/b          new cell after / before
 dd / p       delete cell / paste it        u  undo cell op
 yy           copy cell source to clipboard
 J/K          move cell down / up
 m            cycle code -> markdown -> latex
 o            hide/show output   [ ]  scroll long output
 z / Z        zoom image fullscreen / toggle native size
 /  n  N      search cell sources / next / previous
 w            save (W: force)    q   quit
 Ctrl+C       interrupt          R   restart kernel
 M            message history
 mouse        click: select/edit, wheel: scroll, Shift+drag: select text";

fn draw_help(frame: &mut Frame) {
    use ratatui::widgets::{Block, Clear};
    let lines: u16 = HELP.lines().count() as u16 + 2;
    let width: u16 = 62.min(frame.area().width);
    let area = Rect {
        x: frame.area().width.saturating_sub(width) / 2,
        y: frame.area().height.saturating_sub(lines) / 2,
        width,
        height: lines.min(frame.area().height),
    };
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(HELP).block(Block::bordered().title(" jOtter 🦦 — keys (any key closes) ")),
        area,
    );
}

#[cfg(test)]
mod tests {
    #![allow(deprecated)] // Picker::from_fontsize is the only headless constructor
    use super::*;
    use ratatui_image::FontSize;
    use ratatui_image::picker::Picker;

    // 1x1 transparent png
    const PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAAC0lEQVR4nGNgAAIAAAUAAXpeqz8AAAAASUVORK5CYII=";

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
        let r = Rendered::build(&nb, Some(picker), Default::default());
        let block = &r.blocks[0];
        assert_eq!(block.src_lines, 1);
        assert_eq!(block.images.len(), 1);
        // 1px image -> 1 row reserved
        assert_eq!(block.lines.len(), 1 + block.images[0].rows as usize);

        // without a picker: placeholder text instead
        let r = Rendered::build(&nb, None, Default::default());
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
        let r = Rendered::build(&nb, Some(picker), Default::default());
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
            elapsed: None,
        };
        assert_eq!(block.window(), (0, rows + 1, false), "plot arrives whole");
        // only genuine text spam scrolls: image rows extend the budget
        block.lines.extend(vec![Line::raw(""); 40]);
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
            r.render_markdown(src, None)
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
        let r = Rendered::build(&nb, None, Default::default());
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
            rows[0].contains("▶ In [3]:"),
            "selected prompt: {}",
            rows[0]
        );
        // 30 output lines -> viewport pinned to the tail, footer describes it
        assert!(all.contains("line-29"), "tail visible:\n{all}");
        assert!(!all.contains("line-0 "), "head clipped:\n{all}");
        assert!(all.contains("··· output"), "footer present:\n{all}");
        assert!(
            all.contains("[markdown]") && all.contains("Big Title"),
            "{all}"
        );
        assert!(rows[29].contains("cell 1/2"), "status line: {}", rows[29]);
        // hit map is window-relative and covers the viewport
        assert!(matches!(app.hit[0].kind, crate::app::HitKind::Other));
        assert_eq!(app.hit[0].cell, 0);
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
        let r = Rendered::build(&nb, None, Default::default()); // no graphics -> math becomes unicode approx
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
}
