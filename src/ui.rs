//! Rendering. Cell bodies (syntax-highlighted source + outputs) are cached in
//! `Rendered` and rebuilt per cell when outputs arrive; the cell being edited
//! renders live from the editor buffer. Images render as kitty/sixel graphics
//! into blank placeholder lines reserved in the text flow.

use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::Paragraph,
};
use ratatui_image::sliced::{SignedPosition, SlicedImage};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::{
    app::{Align, App},
    notebook::Cell,
    nvim::ModeKind,
};

mod block;
mod cache;
mod markdown;
mod overlays;
#[cfg(test)]
mod tests;

use block::{Wrapped, wrap_lines};
pub use block::{flush_kitty_deletes, take_kitty_transmit};
pub use cache::{Rendered, is_latex_raw, notebook_language};
pub use overlays::{COMPLETION_ROWS, HELP, scroll_window};
use overlays::{
    centered, draw_completion, draw_debug, draw_help, draw_kernels, draw_logs, draw_pager,
    draw_signature, draw_stdin,
};

const OUTPUT_STYLE: Style = Style::new().fg(Color::Gray);
const ERROR_STYLE: Style = Style::new().fg(Color::Red);
/// stderr: told apart from stdout, quieter than a traceback.
const STDERR_STYLE: Style = Style::new().fg(Color::Red).add_modifier(Modifier::DIM);

/// Images taller than this many rows are downscaled once (config knob).
fn max_image_rows() -> u16 {
    crate::config::get().max_image_rows
}

/// Output rows displayed per cell; longer outputs get a scrollable viewport.
fn max_output_rows() -> usize {
    crate::config::get().max_output_rows
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

/// Colour of a `V` selection: no cell type uses blue.
const VISUAL_COLOR: Color = Color::Blue;

/// Whether a cell is highlighted: not, as the selected cell, or as part of
/// a `V` selection (in the selection colour).
#[derive(Clone, Copy, PartialEq)]
enum Pick {
    Off,
    Selected,
    Visual,
}

impl Pick {
    fn color(self, cell: &Cell) -> Color {
        if self == Pick::Visual { VISUAL_COLOR } else { type_color(cell) }
    }
}

/// The gutter bar: bright on picked cells, dimmed elsewhere. Every modifier
/// a markdown heading line carries is masked off.
fn gutter_span(cell: &Cell, pick: Pick) -> Span<'static> {
    let style = Style::new().fg(pick.color(cell)).remove_modifier(Modifier::all());
    let style = if pick != Pick::Off {
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
    pick: Pick,
    run: Option<bool>,
    editing: bool,
    elapsed: Option<std::time::Duration>,
    marks: String,
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
    let color = pick.color(cell);
    let mut label_style = Style::new().fg(color).add_modifier(Modifier::BOLD);
    if pick != Pick::Off {
        label_style = label_style.add_modifier(Modifier::REVERSED);
    }
    let rule =
        if pick != Pick::Off { Style::new().fg(color) } else { Style::new().fg(Color::DarkGray) };
    let mut spans = vec![gutter_span(cell, pick), Span::styled(format!(" {label} "), label_style)];
    if editing {
        spans.push(Span::styled(" ✎", Style::new().fg(color)));
    }
    if !marks.is_empty() {
        spans.push(Span::styled(format!(" {marks}"), MARK_STYLE));
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
    // marks sidebar on the right, when there are marks and room for it
    let marks = mark_entries(app);
    let (body, side) = if !marks.is_empty() && body.width >= 80 {
        let [b, s] =
            Layout::horizontal([Constraint::Min(0), Constraint::Length(MARKS_W)]).areas(body);
        (b, Some(s))
    } else {
        (body, None)
    };
    let height = body.height as usize;

    rendered.relayout(body.width, &app.notebook);

    // The editing cell renders live from the editor buffer: highlighted and
    // wrapped once per frame (one cell; cached blocks rewrap only on resize).
    // The editor's line count is authoritative for its logical lines.
    let editing_cell = app.editor.as_ref().map(|_| app.selected);
    let marked = app.sel_range(); // selected cell, or the whole `V` selection
    let pick = |ci: usize| match (marked.contains(&ci), app.visual.is_some()) {
        (false, _) => Pick::Off,
        (true, false) => Pick::Selected,
        (true, true) => Pick::Visual,
    };
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
        let src = if editing_cell == Some(i) { editor_wrap.rows.len() } else { block.src_rows() };
        let (win_start, win_rows, footer) = block.window();
        CellView { src, win_start, win_rows, out_len: block.out_len(), footer }
    };
    // Only cells on or near the screen are rendered; the rest are laid out
    // from row counts (estimated for math). Rendering a cell can change its
    // height, which moves the view, which can bring more cells near: repeat
    // until the neighbourhood is rendered. Each round renders at least one
    // cell, so this ends.
    let n = app.notebook.cells.len();
    let (mut starts, mut heights) = (Vec::with_capacity(n), Vec::with_capacity(n));
    let mut total;
    let align = app.align.take();
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
            // zt/zz/zb; a cell taller than the screen still shows its top
            match align {
                Some(Align::Top) => app.scroll = sel_start,
                Some(Align::Middle) => {
                    app.scroll = ((sel_start + sel_end) / 2).saturating_sub(height / 2)
                }
                Some(Align::Bottom) => app.scroll = sel_end.saturating_sub(height),
                None => {}
            }
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
                    pick(ci),
                    run,
                    editing,
                    block.elapsed,
                    marks.iter().filter(|m| m.0 == ci).map(|m| format!("'{}", m.1)).collect(),
                    body.width,
                ),
                HitKind::Other,
            )
        } else if local <= v.src {
            let b = local - 1;
            let wrap = if editing { &editor_wrap } else { &block.wrap };
            let o = wrap.origin[b];
            let kind = if editing || (cell.cell_type != "markdown" && !is_latex_raw(cell)) {
                HitKind::Source { line: o.line, col: o.col }
            } else if let Some(&s) = block.src_map.get(o.line) {
                // rendered markdown row -> raw source line
                HitKind::Source { line: s, col: 0 }
            } else {
                HitKind::Other // latex raw: no per-row mapping
            };
            let mut row = wrap.rows[b].clone();
            row.spans.insert(0, gutter_span(cell, pick(ci)));
            (row, kind)
        } else if local <= v.src + v.win_rows {
            let out_row = v.win_start + (local - 1 - v.src);
            (block.wrap.rows[block.src_rows() + out_row].clone(), HitKind::Output)
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
            (Line::styled(label, Style::new().fg(Color::DarkGray)), HitKind::Output)
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
    for (i, block) in rendered.blocks.iter().enumerate().filter(|_| app.zoom.is_none()) {
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
                (body, SignedPosition { x: (x as u16 + GUTTER) as i16, y: rel as i16 })
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
                (area, SignedPosition { x: x as i16, y: (img_rel - y0) as i16 })
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
        None if app.visual.is_some() => (" V-LINE ", VISUAL_COLOR),
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
    // (the `:` command line has its own popup, see draw_cmdline)
    let prompt = app.prompt.as_ref().and_then(|p| match p.kind {
        crate::app::PromptKind::Search => Some(format!("/{}▏", p.buf)),
        crate::app::PromptKind::SaveAs => Some(format!("save as: {}▏", p.buf)),
        crate::app::PromptKind::Command => None,
    });
    let tail = if let Some(p) = prompt {
        p
    } else if let Some(c) =
        app.editor.as_ref().and_then(|e| e.cmdline()).filter(|c| !c.starts_with(':'))
    {
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
                Style::new().fg(Color::Black).bg(mode_bg).add_modifier(Modifier::BOLD),
            ),
            Span::raw(format!(" {tail}")),
        ])
    } else {
        Line::from(vec![
            Span::styled(
                mode_label,
                Style::new().fg(Color::Black).bg(mode_bg).add_modifier(Modifier::BOLD),
            ),
            Span::raw(format!(
                " {}{}  cell {}/{}  ",
                if app.is_scratch() { "[scratch]".into() } else { app.path.display().to_string() },
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
        let typed: String =
            editor_src.chars().skip(start).take(end.saturating_sub(start)).collect();
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
            let typed: String = line.chars().skip(from).take(col.saturating_sub(from)).collect();
            typed.width() as u16
        } else {
            0
        };
        draw_signature(frame, sig, at, back, completion_below, body, &rendered.panel());
    }
    if let Some(help) = &mut app.help {
        draw_help(frame, help);
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
    if let Some(p) = &app.prompt
        && p.kind == crate::app::PromptKind::Command
    {
        draw_cmdline(frame, &p.buf);
    } else if let Some(c) =
        app.editor.as_ref().and_then(|e| e.cmdline()).and_then(|c| c.strip_prefix(':'))
    {
        draw_cmdline(frame, c); // nvim's own `:` line, same look
    }
    if let Some(side) = side {
        draw_marks(frame, side, app, &marks);
    }
    if let Some(req) = &app.stdin_req {
        draw_stdin(frame, req);
    }
    if let Some(pick) = &app.kernel_pick {
        draw_kernels(frame, pick);
    }
}

/// Width of the marks sidebar, borders included.
const MARKS_W: u16 = 28;

/// How mark letters show: in cell headers and the sidebar.
const MARK_STYLE: Style = Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD);

/// (cell index, letter) for every mark whose cell still exists, in notebook
/// order (an outline), letters in order within a cell.
fn mark_entries(app: &App) -> Vec<(usize, char)> {
    let mut v: Vec<(usize, char)> =
        (app.marks.keys()).filter_map(|&c| Some((app.mark_cell(c)?, c))).collect();
    v.sort();
    v
}

/// The marks sidebar: one row per mark, `'a  12  first line of the cell`,
/// the selected cell's marks highlighted.
fn draw_marks(frame: &mut Frame, area: Rect, app: &App, marks: &[(usize, char)]) {
    use ratatui::widgets::{Block, BorderType};
    let dim = Style::new().fg(Color::DarkGray);
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(dim)
        .title(Span::styled(" Marks ", Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD)))
        .title_bottom(Line::styled(" 'a jumps ", dim).right_aligned());
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let num_w = app.notebook.cells.len().to_string().len();
    let lines: Vec<Line> = marks
        .iter()
        .map(|&(i, c)| {
            let first = app.notebook.cells[i]
                .source
                .lines()
                .map(str::trim)
                .find(|l| !l.is_empty())
                .unwrap_or("(empty)");
            let room = (inner.width as usize).saturating_sub(num_w + 6);
            let first: String = first.chars().take(room).collect();
            let text = if i == app.selected {
                Style::new().add_modifier(Modifier::BOLD | Modifier::REVERSED)
            } else {
                Style::default()
            };
            Line::from(vec![
                Span::styled(format!(" '{c} "), MARK_STYLE),
                Span::styled(format!("{:>num_w$} ", i + 1), dim),
                Span::styled(first, text),
            ])
        })
        .collect();
    frame.render_widget(Paragraph::new(lines), inner);
}

/// The `:` command line: a rounded box in the accent colour a quarter of
/// the way down, titled and prompted like noice.nvim's cmdline popup.
fn draw_cmdline(frame: &mut Frame, text: &str) {
    use ratatui::widgets::{Block, BorderType, Clear, Padding};
    let area = frame.area();
    let mut rect = centered(area, area.width.saturating_sub(6).clamp(26, 72), 3);
    rect.y = (area.y + area.height / 4).min(area.bottom().saturating_sub(rect.height));
    let accent = Style::new().fg(Color::Cyan);
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(accent)
        .title(Line::styled(" Cmdline ", accent).centered())
        .padding(Padding::horizontal(1));
    let inner = block.inner(rect);
    frame.render_widget(Clear, rect);
    frame.render_widget(block, rect);
    // keep the tail (and the cursor) inside the box when the line outgrows it
    let room = (inner.width as usize).saturating_sub(3);
    let skip = text.chars().count().saturating_sub(room);
    let tail: String = text.chars().skip(skip).collect();
    frame.set_cursor_position((inner.x + 2 + tail.width() as u16, inner.y));
    frame.render_widget(
        Paragraph::new(Line::from(vec![Span::styled("› ", accent), Span::raw(tail)])),
        inner,
    );
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
    let (lo, hi) = if anchor <= cursor { (anchor, cursor) } else { (cursor, anchor) };
    if row < lo.0 || row > hi.0 {
        return None;
    }
    let eol = len.saturating_sub(1);
    match kind {
        ModeKind::VisualLine => Some((0, eol)),
        ModeKind::VisualBlock => Some((lo.1.min(hi.1), lo.1.max(hi.1))),
        ModeKind::Visual => {
            Some((if row == lo.0 { lo.1 } else { 0 }, if row == hi.0 { hi.1 } else { eol }))
        }
        _ => None,
    }
}

/// Restyle chars [from..=to] of a highlighted line as REVERSED, splitting
/// spans at the boundaries.
fn overlay_reversed(line: &mut Line<'static>, from: usize, to: usize) {
    if line_chars(line) == 0 {
        // empty line inside a selection: show one reversed cell
        line.spans = vec![Span::styled(" ", Style::new().add_modifier(Modifier::REVERSED))];
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
        out.push(Span::styled(text[a..b].to_string(), span.style.add_modifier(Modifier::REVERSED)));
        if b < text.len() {
            out.push(Span::styled(text[b..].to_string(), span.style));
        }
    }
    line.spans = out;
}
