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
    /// Line offset within the cell's *output* region.
    line: usize,
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
    pub fn highlight(&self, source: &str, ext: &str) -> Vec<Line<'static>> {
        let syntax = self
            .ps
            .find_syntax_by_extension(ext)
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
        let mut lines = self.highlight(&cell.source, cell_ext(cell));
        let src_lines = lines.len();
        let mut images = Vec::new();
        for output in cell.outputs.iter().flatten() {
            let out_off = lines.len() - src_lines;
            if let Some(img) = self.try_image(output) {
                for _ in 0..img.rows {
                    lines.push(Line::raw(""));
                }
                images.push(InlineImage { line: out_off, ..img });
            } else {
                lines.extend(output_lines(output));
            }
        }
        CellBlock { lines, src_lines, images }
    }

    /// If the output carries a PNG and graphics are available, build a protocol.
    fn try_image(&self, output: &Value) -> Option<InlineImage> {
        let picker = self.picker.as_ref()?;
        let b64: String = join_multiline(output.get("data")?.get("image/png")?)
            .split_whitespace()
            .collect();
        let bytes = base64::engine::general_purpose::STANDARD.decode(b64).ok()?;
        let img = image::load_from_memory(&bytes).ok()?;
        let font = picker.font_size();
        let (fw, fh) = (font.width, font.height);
        let rows = ((img.height() as f32 / fh as f32).ceil() as u16).min(MAX_IMAGE_ROWS);
        // width that preserves aspect at the (possibly capped) height
        let scale = rows as f32 * fh as f32 / img.height() as f32;
        let cols = ((img.width() as f32 * scale / fw as f32).ceil() as u16).max(1);
        let proto = picker.new_resize_protocol(img);
        Some(InlineImage { line: 0, cols, rows, proto })
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
        for r in 0..lines.len() - starts[i] - 1 {
            hit.push(Hit {
                cell: i,
                kind: if r < src_len { HitKind::Source(r) } else { HitKind::Other },
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
        let drawn_src = match (&app.editor, i == app.selected) {
            (Some(ed), true) => ed.lines.len(),
            _ => block.src_lines,
        };
        for img in &mut block.images {
            let abs = starts[i] + 1 + drawn_src + img.line;
            if abs < app.scroll {
                continue; // ponytail: images pop in only once their top line is visible
            }
            let rel = (abs - app.scroll) as u16;
            if rel >= body.height {
                continue;
            }
            let area = Rect {
                x: body.x,
                y: body.y + rel,
                width: img.cols.min(body.width),
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
}
