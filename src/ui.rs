//! Rendering. Cell bodies (syntax-highlighted source + outputs) are cached in
//! `Rendered` and rebuilt per cell when outputs arrive; the cell being edited
//! renders live from the editor buffer. Images render as kitty/sixel graphics
//! into blank placeholder lines reserved in the text flow.

use crate::app::App;
use crate::editor::Mode as EdMode;
use crate::notebook::{join_multiline, Cell, Notebook};
use ansi_to_tui::IntoText;
use base64::Engine;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::Paragraph;
use ratatui::Frame;
use ratatui_image::picker::Picker;
use ratatui_image::protocol::StatefulProtocol;
use ratatui_image::StatefulImage;
use serde_json::Value;
use syntect::easy::HighlightLines;
use syntect::highlighting::{Theme, ThemeSet};
use syntect::parsing::SyntaxSet;
use syntect::util::LinesWithEndings;

const OUTPUT_STYLE: Style = Style::new().fg(Color::Gray);
const ERROR_STYLE: Style = Style::new().fg(Color::Red);
const MAX_IMAGE_ROWS: u16 = 18;

pub struct InlineImage {
    /// Line offset within the cell's block (source + outputs).
    line: usize,
    /// Column offset within the line (inline math sits mid-text).
    col: u16,
    cols: u16,
    rows: u16,
    proto: StatefulProtocol,
}

pub struct CellBlock {
    pub lines: Vec<Line<'static>>,
    /// How many of `lines` are source (the rest are outputs).
    pub src_lines: usize,
    images: Vec<InlineImage>,
}

pub struct Rendered {
    ps: SyntaxSet,
    theme: Theme,
    picker: Option<Picker>,
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
    pub fn build(nb: &Notebook, picker: Option<Picker>) -> Self {
        let ps = SyntaxSet::load_defaults_newlines();
        let theme = ThemeSet::load_defaults()
            .themes
            .remove("base16-ocean.dark")
            .expect("bundled theme");
        let mut r = Self { ps, theme, picker, blocks: Vec::new() };
        r.blocks = nb.cells.iter().map(|c| r.render_cell(c)).collect();
        r
    }

    pub fn rebuild_cell(&mut self, idx: usize, cell: &Cell) {
        self.blocks[idx] = self.render_cell(cell);
    }

    pub fn insert_cell(&mut self, idx: usize, cell: &Cell) {
        self.blocks.insert(idx, self.render_cell(cell));
    }

    pub fn remove_cell(&mut self, idx: usize) {
        self.blocks.remove(idx);
    }

    pub fn swap_cells(&mut self, a: usize, b: usize) {
        self.blocks.swap(a, b);
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

    fn render_cell(&self, cell: &Cell) -> CellBlock {
        let (mut lines, mut images) = if cell.cell_type == "markdown" {
            self.render_markdown(&cell.source)
        } else if is_latex_raw(cell) {
            // raw cell with metadata.format = text/latex: whole cell is math
            let (mut l, mut im) = (Vec::new(), Vec::new());
            self.push_math(cell.source.trim().trim_matches('$'), &mut l, &mut im);
            (l, im)
        } else {
            (self.highlight(&cell.source, cell_ext(cell)), Vec::new())
        };
        let src_lines = lines.len();
        for output in cell.outputs.iter().flatten() {
            if let Some(img) = self.try_image(output).or_else(|| self.try_latex(output)) {
                self.push_image(img, &mut lines, &mut images);
            } else {
                lines.extend(output_lines(output));
            }
        }
        CellBlock { lines, src_lines, images }
    }

    /// Reserve blank lines for `entry` at the current position and record it.
    fn push_image(
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
    fn image_entry(&self, img: image::DynamicImage) -> Option<InlineImage> {
        let picker = self.picker.as_ref()?;
        let font = picker.font_size();
        let (fw, fh) = (font.width, font.height);
        let rows = ((img.height() as f32 / fh as f32).ceil() as u16).min(MAX_IMAGE_ROWS);
        // width that preserves aspect at the (possibly capped) height
        let scale = rows as f32 * fh as f32 / img.height() as f32;
        let cols = ((img.width() as f32 * scale / fw as f32).ceil() as u16).max(1);
        let proto = picker.new_resize_protocol(img);
        Some(InlineImage { line: 0, col: 0, cols, rows, proto })
    }

    /// If the output carries a PNG and graphics are available, build a protocol.
    fn try_image(&self, output: &Value) -> Option<InlineImage> {
        let b64: String = join_multiline(output.get("data")?.get("image/png")?)
            .split_whitespace()
            .collect();
        let bytes = base64::engine::general_purpose::STANDARD.decode(b64).ok()?;
        self.image_entry(image::load_from_memory(&bytes).ok()?)
    }

    /// text/latex output (e.g. sympy) -> rendered math image.
    fn try_latex(&self, output: &Value) -> Option<InlineImage> {
        let picker = self.picker.as_ref()?;
        let tex = join_multiline(output.get("data")?.get("text/latex")?);
        let tex = tex.trim().trim_matches('$').replace("\\displaystyle", "");
        let img = crate::latex::render_math(&tex, picker.font_size().height)?;
        self.image_entry(image::DynamicImage::ImageRgba8(img))
    }

    fn push_math(
        &self,
        tex: &str,
        lines: &mut Vec<Line<'static>>,
        images: &mut Vec<InlineImage>,
    ) {
        let rendered = self
            .picker
            .as_ref()
            .and_then(|p| crate::latex::render_math(tex, p.font_size().height))
            .and_then(|img| self.image_entry(image::DynamicImage::ImageRgba8(img)));
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

    /// Minimal markdown: headings, fenced code, $$math$$, bullets, `code`,
    /// $inline math$. ponytail: no tables/links/emphasis — extend when missed.
    fn render_markdown(&self, source: &str) -> (Vec<Line<'static>>, Vec<InlineImage>) {
        let mut lines = Vec::new();
        let mut images = Vec::new();
        let mut fence: Option<(String, String)> = None;
        let mut math: Option<String> = None;
        for raw in source.split('\n') {
            let t = raw.trim();
            if let Some((lang, buf)) = &mut fence {
                if t.starts_with("```") {
                    let lang = if lang.is_empty() { "py" } else { lang.as_str() };
                    lines.extend(self.highlight(buf, lang));
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
                } else {
                    buf.push_str(raw);
                    buf.push('\n');
                }
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
                continue;
            }
            let line_idx = lines.len();
            let line = self.md_line(raw, line_idx, &mut images);
            lines.push(line);
        }
        // unterminated fence/math: show what we buffered, raw
        for buf in [fence.map(|f| f.1), math].into_iter().flatten() {
            lines.extend(buf.lines().map(|l| Line::raw(l.to_string())));
        }
        (lines, images)
    }

    /// Inline markdown spans: bullets, `code`, $math$. Inline math renders in
    /// TeX text style as a one-row image in reserved columns.
    fn md_line(
        &self,
        raw: &str,
        line_idx: usize,
        images: &mut Vec<InlineImage>,
    ) -> Line<'static> {
        let code_style = Style::new().fg(Color::Yellow);
        let math_style = Style::new().fg(Color::Magenta).add_modifier(Modifier::ITALIC);
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
        let mut chars = rest.chars().peekable();
        while let Some(c) = chars.next() {
            let (delim, style, is_math) = match c {
                '`' => ('`', code_style, false),
                '$' => ('$', math_style, true),
                _ => {
                    text.push(c);
                    continue;
                }
            };
            let mut inner = String::new();
            let mut closed = false;
            for n in chars.by_ref() {
                if n == delim {
                    closed = true;
                    break;
                }
                inner.push(n);
            }
            if !closed || (is_math && inner.trim().is_empty()) {
                text.push(delim);
                text.push_str(&inner);
                if closed {
                    text.push(delim);
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
                    .and_then(|img| self.image_entry(image::DynamicImage::ImageRgba8(img)));
                if let Some(entry) = entry {
                    // reserve the columns in the text flow
                    spans.push(Span::raw(" ".repeat(entry.cols as usize)));
                    images.push(InlineImage { line: line_idx, col: col as u16, ..entry });
                    col += entry.cols as usize;
                    continue;
                }
            }
            let shown = if is_math { crate::latex::to_unicode_approx(&inner) } else { inner };
            col += shown.chars().count();
            spans.push(Span::styled(shown, style));
        }
        flush(&mut text, &mut spans, &mut col);
        Line::from(spans)
    }
}

fn output_lines(output: &Value) -> Vec<Line<'static>> {
    let (text, style) = match output["output_type"].as_str() {
        Some("stream") => (join_multiline(&output["text"]), OUTPUT_STYLE),
        Some("execute_result") | Some("display_data") => {
            let data = &output["data"];
            if data.get("image/png").is_some() {
                ("[image/png]".into(), OUTPUT_STYLE) // no graphics support
            } else {
                (join_multiline(&data["text/plain"]), OUTPUT_STYLE)
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
        Err(_) => text.lines().map(|l| Line::styled(l.to_string(), style)).collect(),
    }
}

fn prompt_line(cell: &Cell, selected: bool, running: bool, editing: bool) -> Line<'static> {
    let label = match cell.cell_type.as_str() {
        "code" => {
            if running {
                "In [*]:".to_string()
            } else {
                match cell.execution_count() {
                    Some(n) => format!("In [{n}]:"),
                    None => "In [ ]:".to_string(),
                }
            }
        }
        other => format!("[{other}]"),
    };
    let mut style = Style::default()
        .fg(if cell.cell_type == "code" { Color::Cyan } else { Color::Magenta })
        .add_modifier(Modifier::BOLD);
    let marker = if selected {
        style = style.add_modifier(Modifier::REVERSED);
        if editing { "✎ " } else { "▶ " }
    } else {
        "  "
    };
    Line::from(vec![Span::raw(marker), Span::styled(label, style)])
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui_image::picker::Picker;
    use ratatui_image::FontSize;

    // 1x1 transparent png
    const PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAAC0lEQVR4nGNgAAIAAAUAAXpeqz8AAAAASUVORK5CYII=";

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
        let nb = Notebook { cells: vec![cell], extra: serde_json::Map::new() };
        let picker = Picker::from_fontsize(FontSize::new(8, 16));
        let r = Rendered::build(&nb, Some(picker));
        let block = &r.blocks[0];
        assert_eq!(block.src_lines, 1);
        assert_eq!(block.images.len(), 1);
        // 1px image -> 1 row reserved
        assert_eq!(block.lines.len(), 1 + block.images[0].rows as usize);

        // without a picker: placeholder text instead
        let r = Rendered::build(&nb, None);
        assert!(r.blocks[0].images.is_empty());
        assert_eq!(r.blocks[0].lines.len(), 2);
    }

    #[test]
    fn all_inline_math_becomes_images() {
        let cell: Cell = serde_json::from_value(serde_json::json!({
            "cell_type": "markdown",
            "metadata": {},
            "source": "on the $\\tau$ grid, and $ \\int_0^\\infty \\frac{1}{x^2} $ too"
        }))
        .unwrap();
        let nb = Notebook { cells: vec![cell], extra: serde_json::Map::new() };
        let picker = Picker::from_fontsize(FontSize::new(8, 16));
        let r = Rendered::build(&nb, Some(picker));
        let block = &r.blocks[0];
        assert_eq!(block.images.len(), 2);
        // ALL inline math is exactly one row; too small => author uses $$
        assert!(block.images.iter().all(|i| i.rows == 1));
        assert_eq!(block.images[0].col, "on the ".len() as u16);
        assert_eq!(block.lines.len(), 1);
    }

    #[test]
    fn markdown_renders_rich() {
        let cell: Cell = serde_json::from_value(serde_json::json!({
            "cell_type": "markdown",
            "metadata": {},
            "source": "# Title\n- item\ncall `f(x)` on $\\tau$\n$$E = \\hbar \\omega^2$$"
        }))
        .unwrap();
        let nb = Notebook { cells: vec![cell], extra: serde_json::Map::new() };
        let r = Rendered::build(&nb, None); // no graphics -> math becomes unicode approx
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

pub fn draw(frame: &mut Frame, app: &mut App, rendered: &mut Rendered) {
    let [body, status] =
        Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).areas(frame.area());

    // Assemble all lines, tracking each cell's first line for images/cursor
    // and a per-line hit map for the mouse.
    let mut lines: Vec<Line> = Vec::new();
    let mut hit: Vec<crate::app::Hit> = Vec::new();
    let mut starts: Vec<usize> = Vec::with_capacity(app.notebook.cells.len());
    let (mut sel_start, mut sel_end) = (0usize, 0usize);
    for (i, cell) in app.notebook.cells.iter().enumerate() {
        use crate::app::{Hit, HitKind};
        starts.push(lines.len());
        if i == app.selected {
            sel_start = lines.len();
        }
        let editing = app.editor.is_some() && i == app.selected;
        let running = app.running.values().any(|&c| c == i);
        lines.push(prompt_line(cell, i == app.selected, running, editing));
        hit.push(Hit { cell: i, kind: HitKind::Other });
        let block = &rendered.blocks[i];
        let src_len = if editing {
            let editor = app.editor.as_ref().unwrap();
            lines.extend(rendered.highlight(&editor.source(), cell_ext(cell)));
            lines.extend(block.lines[block.src_lines..].iter().cloned());
            editor.lines.len()
        } else {
            lines.extend(block.lines.iter().cloned());
            block.src_lines
        };
        // rendered markdown/latex rows don't map to source rows: select-only
        let clickable = editing || (cell.cell_type != "markdown" && !is_latex_raw(cell));
        for r in 0..lines.len() - starts[i] - 1 {
            hit.push(Hit {
                cell: i,
                kind: if clickable && r < src_len { HitKind::Source(r) } else { HitKind::Other },
            });
        }
        if i == app.selected {
            sel_end = lines.len();
        }
        lines.push(Line::raw(""));
        hit.push(Hit { cell: i, kind: HitKind::Other });
    }
    app.hit = hit;
    app.body = body;

    // Keep selection visible (cursor line when editing, whole cell otherwise) —
    // unless the user wheel-scrolled away; then only clamp to the content.
    let height = body.height as usize;
    if app.manual_scroll {
        app.scroll = app.scroll.min(lines.len().saturating_sub(1));
    } else if let Some(editor) = &app.editor {
        let cur = starts[app.selected] + 1 + editor.cursor.0;
        if cur >= app.scroll + height {
            app.scroll = cur + 1 - height;
        }
        if cur < app.scroll {
            app.scroll = cur;
        }
    } else {
        if sel_end > app.scroll + height {
            app.scroll = sel_end - height;
        }
        if sel_start < app.scroll {
            app.scroll = sel_start;
        }
    }
    // ponytail: u16 scroll caps at 65535 rendered lines; switch to windowed rendering if that ever bites
    let scroll = app.scroll.min(u16::MAX as usize) as u16;
    frame.render_widget(Paragraph::new(Text::from(lines)).scroll((scroll, 0)), body);

    // Image pass: render graphics into their reserved blank lines.
    for (i, block) in rendered.blocks.iter_mut().enumerate() {
        let editing = app.editor.is_some() && i == app.selected;
        // editing replaces the source region: output images shift, source-region
        // images (markdown math) hide behind the raw text.
        let delta = match (&app.editor, editing) {
            (Some(ed), true) => ed.lines.len() as isize - block.src_lines as isize,
            _ => 0,
        };
        for img in &mut block.images {
            let in_src = img.line < block.src_lines;
            if in_src && editing {
                continue;
            }
            let abs = starts[i] as isize + 1 + img.line as isize + if in_src { 0 } else { delta };
            if abs < app.scroll as isize {
                continue; // ponytail: images pop in only once their top line is visible
            }
            let abs = abs as usize;
            let rel = (abs - app.scroll) as u16;
            if rel >= body.height || img.col >= body.width {
                continue;
            }
            let area = Rect {
                x: body.x + img.col,
                y: body.y + rel,
                width: img.cols.min(body.width - img.col),
                height: img.rows.min(body.height - rel),
            };
            frame.render_stateful_widget(StatefulImage::new(), area, &mut img.proto);
        }
    }

    // Editor cursor: real terminal cursor at the edit position.
    if let Some(editor) = &app.editor {
        let abs = starts[app.selected] + 1 + editor.cursor.0;
        if abs >= app.scroll && ((abs - app.scroll) as u16) < body.height {
            frame.set_cursor_position((
                body.x + (editor.cursor.1 as u16).min(body.width.saturating_sub(1)),
                body.y + (abs - app.scroll) as u16,
            ));
        }
    }

    let (mode_label, mode_bg) = match &app.editor {
        None => (" VIEW ", Color::Cyan),
        Some(e) if e.mode == EdMode::Insert => (" INSERT ", Color::Green),
        Some(_) => (" NORMAL ", Color::Yellow),
    };
    let kernel_state = if app.kernel.is_none() {
        Span::styled("◌ no kernel", Style::new().fg(Color::DarkGray))
    } else if app.kernel_busy {
        Span::styled("● busy", Style::new().fg(Color::Yellow))
    } else {
        Span::styled("○ idle", Style::new().fg(Color::Green))
    };
    let status_line = Line::from(vec![
        Span::styled(
            mode_label,
            Style::new().fg(Color::Black).bg(mode_bg).add_modifier(Modifier::BOLD),
        ),
        Span::raw(format!(
            " {}{}  cell {}/{}  ",
            app.path.display(),
            if app.dirty { " [+]" } else { "" },
            app.selected + 1,
            app.notebook.cells.len()
        )),
        kernel_state,
        Span::raw(format!("  {}", app.message.as_deref().unwrap_or(""))),
    ]);
    frame.render_widget(Paragraph::new(status_line), status);

    if app.show_help {
        draw_help(frame);
    }
}

const HELP: &str = "\
 j/k g/G      select / first / last cell
 Enter, i, A  edit cell (vi: hjkl wbe 0$^ ggG x dd yy p u ...)
 Esc          exit editing (normal mode)
 E            edit cell in $EDITOR
 Shift+Enter  run + advance    Ctrl+Enter/r  run
 a/b          new cell after / before
 dd / p       delete cell / paste it
 yy           copy cell source to clipboard
 J/K          move cell down / up
 m            cycle code -> markdown -> latex
 w            save            q   quit
 Ctrl+C       interrupt       R   restart kernel
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
        Paragraph::new(HELP).block(Block::bordered().title(" keys (any key closes) ")),
        area,
    );
}
