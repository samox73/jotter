//! Floating panels: kernel picker, input prompt, logs, completion, signature help, pager, help.

use ansi_to_tui::IntoText;
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::Paragraph,
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use super::{block::wrap_lines, cache::Rendered};

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
pub(super) fn centered(outer: Rect, w: u16, h: u16) -> Rect {
    let (w, h) = (w.min(outer.width), h.min(outer.height));
    Rect {
        x: outer.x + (outer.width - w) / 2,
        y: outer.y + (outer.height - h) / 2,
        width: w,
        height: h,
    }
}

/// Where an overlay goes: `w`x`h` centered, or the whole screen once it
/// would cover 70% of the width or the height anyway.
fn overlay_rect(outer: Rect, w: u16, h: u16) -> Rect {
    let most = |part: u16, whole: u16| part as u32 * 10 >= whole as u32 * 7;
    if most(w, outer.width) || most(h, outer.height) { outer } else { centered(outer, w, h) }
}

/// The notebook's kernel isn't installed: a filter field over the ones that
/// are (`App::on_key_kernel_pick` takes the keys).
pub(super) fn draw_kernels(frame: &mut Frame, pick: &crate::app::KernelPick) {
    const ROWS: usize = 10;
    let shown = pick.shown();
    // the window of ROWS matches that keeps the selection in view
    let first = pick.selected.saturating_sub(ROWS - 1);
    let mut lines = vec![Line::from(format!("› {}▏", pick.filter))];
    lines.extend(shown.iter().enumerate().skip(first).take(ROWS).map(|(i, k)| {
        let line = Line::from(format!("  {k}"));
        if i == pick.selected {
            line.style(Style::new().add_modifier(Modifier::REVERSED))
        } else {
            line
        }
    }));
    if shown.is_empty() {
        lines.push(Line::styled("  no match", Style::new().fg(Color::DarkGray)));
    }
    let title = format!("kernel '{}' not found — start which?", pick.wanted);
    let width = lines.iter().map(Line::width).max().unwrap_or(0).max(title.chars().count() + 2) + 6;
    let area = centered(frame.area(), width as u16, lines.len() as u16 + 2);
    let inner = modal(frame, area, &title, "type to filter · ↑↓ · Enter starts · Esc none");
    frame.render_widget(Paragraph::new(lines), inner);
}

/// Kernel `input()`: a centered modal prompt (all keys already route to it).
pub(super) fn draw_stdin(frame: &mut Frame, req: &crate::app::StdinReq) {
    let area = frame.area();
    let width = area.width.saturating_sub(6).clamp(26, 72);
    let title = match req.prompt.trim() {
        "" => "input()",
        p => p,
    };
    let inner = modal(frame, centered(area, width, 3), title, "Enter sends · Esc sends empty");
    let shown: String =
        if req.password { "•".repeat(req.buf.chars().count()) } else { req.buf.clone() };
    // keep the tail (and the cursor) inside the box when the input outgrows it
    let skip = shown.chars().count().saturating_sub((inner.width as usize).saturating_sub(1));
    let tail: String = shown.chars().skip(skip).collect();
    frame.set_cursor_position((inner.x + tail.chars().count() as u16, inner.y));
    frame.render_widget(Paragraph::new(tail), inner);
}

/// `L`: the in-memory log (status messages, kernel lifecycle, warnings from
/// dependencies), newest at the bottom. `up` = rows scrolled up; clamped here.
pub(super) fn draw_logs(frame: &mut Frame, up: &mut usize) {
    let area = frame.area();
    let panel = overlay_rect(
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
                    Span::styled(format!("{:>8.2}s ", e.secs), Style::new().fg(Color::DarkGray)),
                    Span::styled(format!("{:<5} ", e.level), level),
                    Span::styled(format!("{} ", e.target), Style::new().fg(Color::Blue)),
                    Span::raw(e.msg.clone()),
                ])
            })
            .collect()
    });
    let title = if *up == 0 { "logs".to_string() } else { format!("logs · {} newer below", *up) };
    let inner = modal(frame, panel, &title, "j/k PgUp/PgDn g/G scroll · any other key closes");
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
        let base = self.theme.settings.background.map_or((0x2b, 0x30, 0x3b), |c| (c.r, c.g, c.b));
        let light = (base.0 as u32 * 299 + base.1 as u32 * 587 + base.2 as u32 * 114) / 1000 > 128;
        let target = if light { 0.0 } else { 255.0 };
        let mix = |t: f32| {
            let m = |c: u8| (c as f32 + (target - c as f32) * t) as u8;
            Color::Rgb(m(base.0), m(base.1), m(base.2))
        };
        Panel { bg: mix(0.08), sel: mix(0.20), thumb: mix(0.35) }
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
pub(super) fn fit(s: &str, w: usize) -> String {
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
pub(super) fn draw_signature(
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
    let x = cx.saturating_sub(back + 1).max(body.x).min(body.x + body.width - width);
    let area = Rect { x, y, width, height: 1 };
    frame.render_widget(ratatui::widgets::Clear, area);
    frame.render_widget(Paragraph::new(line), area);
}

/// Completion popup (nvim-cmp style): a borderless panel under the cursor
/// (above it when there's no room), text column aligned with the token
/// being completed, columns name · kind · signature, and a scrollbar.
/// `typed_w`: display width typed since the token start.
pub(super) fn draw_completion(
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
    let name_w = c.items.iter().map(|i| i.label().width()).max().unwrap_or(0).min(MAX_NAME);
    let kind_w = c.items.iter().map(|i| kind_label(&i.kind).0.len()).max().unwrap_or(0);
    let detail_w = c.items.iter().map(|i| i.detail.width()).max().unwrap_or(0);
    let fixed = 1 + name_w + 2 + kind_w + if detail_w > 0 { 2 } else { 0 } + 1 + scrollbar as usize;
    let width = (fixed + detail_w).min(body.width as usize) as u16;
    let detail_room = (width as usize).saturating_sub(fixed);
    let x =
        cx.saturating_sub(typed_w + 1).max(body.x).min(body.x + body.width.saturating_sub(width));
    let y = if below >= shown { cy + 1 } else { cy - shown as u16 };
    let area = Rect { x, y, width, height: shown as u16 };
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
                    if i == c.sel { base.add_modifier(Modifier::BOLD) } else { base },
                ),
                Span::styled(format!("  {kind:<kind_w$}"), base.fg(kind_color)),
            ];
            if detail_w > 0 {
                let d = fit(&item.detail, detail_room);
                spans.push(Span::styled(
                    format!("  {d:<detail_room$}"),
                    if i == c.sel { base } else { base.fg(Color::Gray) },
                ));
            }
            spans.push(Span::styled(" ", base));
            if scrollbar {
                let bar = if thumb.contains(&row) { panel.thumb } else { panel.bg };
                spans.push(Span::styled(" ", Style::new().bg(bar)));
            }
            Line::from(spans)
        })
        .collect();
    frame.render_widget(Paragraph::new(lines), area);
}

/// IPython's inspect text regrouped for reading: the signature first, then
/// the docstring(s), then the remaining fields (`Type`, `File`, ...) dimmed;
/// fields without a value are dropped. A field starts at a colored
/// `Label:` (which is what tells it from a docstring line like `Note:`), so
/// text from other kernels passes through unchanged.
pub(super) fn docs_layout(text: Text<'static>) -> Text<'static> {
    let mut head: Vec<Line> = Vec::new();
    let mut fields: Vec<(Span, Vec<Line>)> = Vec::new();
    for mut line in text.lines {
        let is_label = line.spans.first().is_some_and(|s| {
            s.style.fg.is_some_and(|c| c != Color::Reset)
                && s.content.starts_with(|c: char| c.is_ascii_uppercase())
                && s.content.trim_end().ends_with(':')
        });
        if is_label {
            let label = line.spans.remove(0);
            // the value may start on the label's line, after padding
            while let Some(first) = line.spans.first_mut() {
                let rest = first.content.trim_start();
                if !rest.is_empty() {
                    first.content = rest.to_string().into();
                    break;
                }
                line.spans.remove(0);
            }
            fields.push((label, if line.spans.is_empty() { Vec::new() } else { vec![line] }));
        } else {
            match fields.last_mut() {
                Some((_, body)) => body.push(line),
                None => head.push(line),
            }
        }
    }
    let dim = Style::new().fg(Color::DarkGray);
    let (mut signature, mut docs, mut meta) = (Vec::new(), Vec::new(), Vec::new());
    for (label, mut body) in fields {
        while body.last().is_some_and(|l| l.to_string().trim().is_empty()) {
            body.pop();
        }
        if body.is_empty() {
            continue;
        }
        let name = label.content.trim_end().to_string();
        if name.ends_with("ignature:") && signature.is_empty() {
            signature = body;
        } else if name.ends_with("ocstring:") || name == "Source:" {
            if name != "Docstring:" {
                body.insert(0, Line::from(label));
            }
            docs.push(body);
        } else {
            meta.push((name, body));
        }
    }
    let pad = meta.iter().map(|(name, _)| name.chars().count()).max().unwrap_or(0);
    let meta: Vec<Line> = meta
        .into_iter()
        .flat_map(|(name, mut body)| {
            body[0].spans.insert(0, Span::raw(format!("{name:<pad$} ")));
            body
        })
        .map(|mut line| {
            line.spans.iter_mut().for_each(|s| s.style = dim);
            line
        })
        .collect();
    let mut lines: Vec<Line> = Vec::new();
    for block in [head, signature].into_iter().chain(docs).chain([meta]) {
        if !block.is_empty() {
            if !lines.is_empty() {
                lines.push(Line::default());
            }
            lines.extend(block);
        }
    }
    Text::from(lines)
}

/// Shift+Tab docs: scrollable text (see `docs_layout`), wrapped, in a panel
/// only as large as its content (see `overlay_rect`); `top` clamped here.
pub(super) fn draw_pager(frame: &mut Frame, pager: &mut crate::app::Pager) {
    const SCROLL_HINT: &str = "j/k PgUp/PgDn g/G scroll · any other key closes";
    let area = frame.area();
    // the widest panel, less borders + padding
    let max = (area.width.saturating_sub(4).min(110) as usize).saturating_sub(4);
    let text = pager.text.into_text().unwrap_or_else(|_| Text::raw(pager.text.clone()));
    let lines = wrap_lines(&docs_layout(text).lines, max as u16).rows;
    let total = lines.len();
    let rows = total.min(area.height.saturating_sub(2) as usize);
    pager.top = pager.top.min(total - rows);
    let (title, hint) = if total > rows {
        let (from, to) = (pager.top + 1, pager.top + rows);
        (format!("{} · {from}–{to} of {total}", pager.title), SCROLL_HINT)
    } else {
        (pager.title.clone(), "any key closes")
    };
    // the frame's own labels must fit too
    let content = lines.iter().map(Line::width).max().unwrap_or(0);
    let labels = title.chars().count().max(hint.chars().count());
    let panel = overlay_rect(area, (content.max(labels).min(max) + 4) as u16, rows as u16 + 2);
    let inner = modal(frame, panel, &title, hint);
    let lines: Vec<Line> = lines.into_iter().skip(pager.top).take(rows).collect();
    frame.render_widget(Paragraph::new(lines), inner);
}

/// `D`: debug facts about the selected cell (already on the clipboard).
pub(super) fn draw_debug(frame: &mut Frame, text: &str) {
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
    let panel = overlay_rect(frame.area(), width.max(40), lines.len() as u16 + 2);
    let inner = modal(frame, panel, "debug · copied to clipboard", "any key closes");
    frame.render_widget(Paragraph::new(lines), inner);
}

/// Help sections: (heading, [(keys, what)]). Keys are space-separated chords.
pub const HELP: &[(&str, &[(&str, &str)])] = &[
    (
        "Navigate",
        &[
            ("j k", "next / previous cell (counts: 3j)"),
            ("gg G", "first / last cell"),
            ("Ctrl+o Ctrl+i", "jump back / forward (also Tab)"),
            ("5G", "go to cell 5"),
            ("zt zz zb", "scroll: cell at top / middle / bottom"),
            ("ma 'a", "mark cell as a / jump to mark a"),
            ("/ n N", "search sources / next / previous"),
        ],
    ),
    (
        "Edit",
        &[
            ("Enter", "edit cell"),
            ("i A", "edit in insert mode / at the end"),
            ("Esc", "leave the editor (normal mode)"),
            ("Ctrl+g", "edit cell in $EDITOR"),
            ("t", "cycle code → markdown → latex"),
            ("f F", "format cell / all code cells"),
        ],
    ),
    (
        "Inside the editor",
        &[
            ("Tab", "complete (opens by itself after `.`)"),
            ("Shift+Tab", "docs for the name under the cursor"),
            ("Tab Shift+Tab", "completion: next / prev (also ↓ ↑)"),
            ("Enter", "completion: accept"),
            ("Ctrl+k", "split the cell at the cursor"),
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
            ("dd yy p P", "delete / copy / paste below, above"),
            ("M", "merge with the cell below"),
            ("V", "select cells (j k gg G extend)"),
            ("V: d y J K", "delete / copy / move selection"),
            ("V: r c f M", "run / clear / format / merge it"),
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
            ("zi", "zoom image fullscreen"),
            ("Z", "toggle native-size images"),
        ],
    ),
    (
        "File & app",
        &[
            ("w W", "save / force save"),
            ("S", "save as"),
            ("q", "quit"),
            (":", "command line (see Commands)"),
            ("L", "view logs"),
            ("D", "debug info for this cell"),
            ("?", "this help (/ filters it)"),
        ],
    ),
    (
        "Commands",
        &[
            (":w :w!", "save / even if changed on disk"),
            (":w\u{a0}file", "save as (:w! file overwrites)"),
            (":q :q!", "in a cell: leave it / discard edit"),
            (":q :q! :qa", "notebook: quit / without saving"),
            (":wq :x", "save, then leave the cell or quit"),
            (":5", "go to cell 5"),
            (":delm a :delm!", "delete mark a / all marks"),
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
        spans.push(Span::raw(" ".repeat(HELP_KEY_W.saturating_sub(keys.chars().count()))));
        spans.push(Span::raw(what.to_string()));
        lines.push(Line::from(spans));
    }
    lines.push(Line::raw(""));
    lines
}

/// Index of the first section of the right column: where the two halves
/// balance by height.
fn help_split(sections: &[Vec<Line>]) -> usize {
    let total: usize = sections.iter().map(Vec::len).sum();
    let mut acc = 0;
    for (i, s) in sections.iter().enumerate() {
        if acc + s.len() / 2 >= total / 2 {
            return i;
        }
        acc += s.len();
    }
    sections.len()
}

/// `?`: the key reference. Two columns where they fit, one once the filter
/// leaves no more than a column's worth; scrolls when the screen is short.
/// The panel keeps the size of the unfiltered help.
pub(super) fn draw_help(frame: &mut Frame, help: &mut crate::app::Help) {
    let area = frame.area();
    let rows = area.height.saturating_sub(2) as usize;
    let needle = help.filter.to_lowercase();
    let all: Vec<Vec<Line>> = HELP.iter().map(|(t, r)| help_section(t, r)).collect();
    // sections keep their heading, entries that don't match go
    let mut sections: Vec<Vec<Line>> = HELP
        .iter()
        .filter_map(|(title, entries)| {
            let hit = |s: &str| s.to_lowercase().contains(&needle);
            let entries: Vec<_> =
                entries.iter().copied().filter(|(k, w)| hit(k) || hit(w)).collect();
            (!entries.is_empty()).then(|| help_section(title, &entries))
        })
        .collect();
    if sections.is_empty() {
        sections
            .push(vec![Line::styled("no match", Style::new().fg(Color::DarkGray)), Line::raw("")]);
    }
    let col_w = all.iter().flatten().map(Line::width).max().unwrap_or(0) as u16;
    // height of the columns when `s` is split at `at` (less the trailing blank)
    let tall = |s: &[Vec<Line>], at: usize| {
        let (l, r) = s.split_at(at);
        let len = |c: &[Vec<Line>]| c.iter().map(Vec::len).sum::<usize>();
        len(l).max(len(r)).saturating_sub(1)
    };
    // the panel is sized for the whole help, so it stays put while filtering
    let fits_two = area.width >= 2 * col_w + 4 + 4;
    let full_w = if fits_two { 2 * col_w + 4 } else { col_w };
    let full_h = tall(&all, if fits_two { help_split(&all) } else { all.len() });
    let two = fits_two && tall(&sections, sections.len()) > full_h.min(rows);
    let split = if two { help_split(&sections) } else { sections.len() };
    let height = tall(&sections, split);
    help.top = help.top.min(height.saturating_sub(rows));
    let title = match (help.typing, help.filter.is_empty()) {
        (true, _) => format!("jOtter 🦦 keys · /{}▏", help.filter),
        (false, false) => format!("jOtter 🦦 keys · /{}", help.filter),
        (false, true) => "jOtter 🦦 keys".to_string(),
    };
    let hint = if help.typing {
        "Enter keeps · Esc clears"
    } else if height > rows {
        "j/k PgUp/PgDn g/G scroll · / filters · any other key closes"
    } else {
        "/ filters · any other key closes"
    };
    // the frame's own labels must fit too
    let labels = Line::raw(title.as_str()).width().max(hint.chars().count()) as u16 + 2;
    let panel = overlay_rect(area, full_w.max(labels) + 4, full_h.min(rows) as u16 + 2);
    let inner = modal(frame, panel, &title, hint);
    let body = centered(inner, full_w, full_h as u16);
    let (left, right) = sections.split_at(split);
    let shown = |col: &[Vec<Line<'static>>]| -> Vec<Line<'static>> {
        col.iter().flatten().skip(help.top).take(rows).cloned().collect()
    };
    if two {
        let [l, _, r] = Layout::horizontal([
            Constraint::Length(col_w),
            Constraint::Length(4),
            Constraint::Length(col_w),
        ])
        .areas(body);
        frame.render_widget(Paragraph::new(shown(left)), l);
        frame.render_widget(Paragraph::new(shown(right)), r);
    } else {
        frame.render_widget(
            Paragraph::new(shown(left)),
            Rect { width: col_w.min(body.width), ..body },
        );
    }
}
