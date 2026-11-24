//! Markdown cells: headings, fences, math, tables, links, images.

use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};
use serde_json::Value;
use unicode_width::UnicodeWidthStr;

use super::{
    OUTPUT_STYLE,
    block::InlineImage,
    cache::{Rendered, b64_bytes, bytes_px},
};

impl Rendered {
    /// Minimal markdown: headings, fenced code, $$math$$, bullets, tables,
    /// links, emphasis, images (attachments + local paths), `code`, $math$.
    /// Returns rendered lines, inline images, and a rendered-line -> source-line
    /// map so mouse clicks land on the right raw line. Fence bodies and table
    /// rows map exactly; math blocks map to their closing `$$` line.
    pub(super) fn render_markdown(
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
                    let lang = if lang.is_empty() { self.lang.as_str() } else { lang.as_str() };
                    let body = if est { Self::plain(buf) } else { self.highlight(buf, lang) };
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
                let text = t.trim_start_matches('#').trim_start();
                let color = match level {
                    1 => Color::Green,
                    2 => Color::Cyan,
                    _ => Color::Blue,
                };
                let mut style = Style::new().fg(color).add_modifier(Modifier::BOLD);
                if level == 1 {
                    style = style.add_modifier(Modifier::UNDERLINED);
                }
                // inline spans ($math$, `code`, links) work in headings too;
                // the line style colours whatever spans leave unstyled
                let line = self.md_line(text, lines.len(), &mut images, est);
                lines.push(line.style(style));
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
        if path.extension().is_some_and(|e| e.eq_ignore_ascii_case("svg")) {
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
        let math_style = Style::new().fg(Color::Magenta).add_modifier(Modifier::ITALIC);
        let link_style = Style::new().fg(Color::Blue).add_modifier(Modifier::UNDERLINED);
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
                            images.push(InlineImage { line: line_idx, col: col as u16, ..entry });
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
                    let mods = if bold { Modifier::BOLD } else { Modifier::ITALIC };
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
            && row.iter().all(|c| !c.is_empty() && c.chars().all(|ch| matches!(ch, '-' | ':')))
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
                let rule = widths.iter().map(|w| "─".repeat(w + 2)).collect::<Vec<_>>().join("┼");
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
