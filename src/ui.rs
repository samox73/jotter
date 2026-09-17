//! Rendering. Cell bodies (syntax-highlighted source + outputs) are cached in
//! `Rendered` and rebuilt per cell when outputs arrive; prompt gutters and the
//! status line restyle per frame.

use crate::app::App;
use crate::notebook::{join_multiline, Cell, Notebook};
use ansi_to_tui::IntoText;
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::Paragraph;
use ratatui::Frame;
use serde_json::Value;
use syntect::easy::HighlightLines;
use syntect::highlighting::{Theme, ThemeSet};
use syntect::parsing::SyntaxSet;
use syntect::util::LinesWithEndings;

const OUTPUT_STYLE: Style = Style::new().fg(Color::Gray);
const ERROR_STYLE: Style = Style::new().fg(Color::Red);

/// Per-cell pre-rendered body lines (source + outputs).
pub struct Rendered {
    ps: SyntaxSet,
    theme: Theme,
    pub blocks: Vec<Vec<Line<'static>>>,
}

impl Rendered {
    pub fn build(nb: &Notebook) -> Self {
        let ps = SyntaxSet::load_defaults_newlines();
        let theme = ThemeSet::load_defaults()
            .themes
            .remove("base16-ocean.dark")
            .expect("bundled theme");
        let mut r = Self { ps, theme, blocks: Vec::new() };
        r.blocks = nb.cells.iter().map(|c| r.render_cell(c)).collect();
        r
    }

    pub fn rebuild_cell(&mut self, idx: usize, cell: &Cell) {
        self.blocks[idx] = self.render_cell(cell);
    }

    fn render_cell(&self, cell: &Cell) -> Vec<Line<'static>> {
        let ext = match cell.cell_type.as_str() {
            "code" => "py",
            "markdown" => "md",
            _ => "txt",
        };
        let syntax = self
            .ps
            .find_syntax_by_extension(ext)
            .unwrap_or_else(|| self.ps.find_syntax_plain_text());
        let mut hl = HighlightLines::new(syntax, &self.theme);
        let mut lines: Vec<Line<'static>> = LinesWithEndings::from(&cell.source)
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
            .collect();
        for output in cell.outputs.iter().flatten() {
            lines.extend(output_lines(output));
        }
        lines
    }
}

fn output_lines(output: &Value) -> Vec<Line<'static>> {
    let (text, style) = match output["output_type"].as_str() {
        Some("stream") => (join_multiline(&output["text"]), OUTPUT_STYLE),
        Some("execute_result") | Some("display_data") => {
            let data = &output["data"];
            if data.get("image/png").is_some() {
                ("[image/png]".into(), OUTPUT_STYLE) // rendered inline in M3
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
    // ANSI escapes (tracebacks, colored prints) become styled spans; on parse
    // failure fall back to raw lines.
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

fn prompt_line(cell: &Cell, selected: bool, running: bool) -> Line<'static> {
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
        "▶ "
    } else {
        "  "
    };
    Line::from(vec![Span::raw(marker), Span::styled(label, style)])
}

pub fn draw(frame: &mut Frame, app: &mut App, rendered: &Rendered) {
    let [body, status] =
        Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).areas(frame.area());

    // Assemble all lines, tracking the selected cell's line range for auto-scroll.
    let mut lines: Vec<Line> = Vec::new();
    let (mut sel_start, mut sel_end) = (0usize, 0usize);
    for (i, cell) in app.notebook.cells.iter().enumerate() {
        if i == app.selected {
            sel_start = lines.len();
        }
        let running = app.running.values().any(|&c| c == i);
        lines.push(prompt_line(cell, i == app.selected, running));
        lines.extend(rendered.blocks[i].iter().cloned());
        if i == app.selected {
            sel_end = lines.len();
        }
        lines.push(Line::raw(""));
    }

    // Keep selection visible; if the cell is taller than the viewport, pin its top.
    let height = body.height as usize;
    if sel_end > app.scroll + height {
        app.scroll = sel_end - height;
    }
    if sel_start < app.scroll {
        app.scroll = sel_start;
    }
    // ponytail: u16 scroll caps at 65535 rendered lines; switch to windowed rendering if that ever bites
    let scroll = app.scroll.min(u16::MAX as usize) as u16;
    frame.render_widget(Paragraph::new(Text::from(lines)).scroll((scroll, 0)), body);

    let kernel_state = if app.kernel.is_none() {
        Span::styled("◌ no kernel", Style::new().fg(Color::DarkGray))
    } else if app.kernel_busy {
        Span::styled("● busy", Style::new().fg(Color::Yellow))
    } else {
        Span::styled("○ idle", Style::new().fg(Color::Green))
    };
    let status_line = Line::from(vec![
        Span::styled(" VIEW ", Style::new().fg(Color::Black).bg(Color::Cyan).add_modifier(Modifier::BOLD)),
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
