
#![allow(deprecated)] // Picker::from_fontsize is the only headless constructor
use base64::Engine;
use ratatui_image::{FontSize, picker::Picker};
use serde_json::Value;
use syntect::highlighting::ThemeSet;

use super::{block::*, cache::*, overlays::*, *};
use crate::notebook::Notebook;

// 1x1 transparent png
const PNG: &str =
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAAC0lEQVR4nGNgAAIAAAUAAXpeqz8AAAAASUVORK5CYII=";

/// A dropped image's delete is held back for the flush after the next
/// frame, so its replacement is on screen first.
#[test]
fn kitty_deletes_wait_for_the_flush() {
    drop(KittyImage { id: 7_000_001, tmux: false });
    drop(KittyImage { id: 7_000_002, tmux: true });
    let mut out = Vec::new();
    flush_kitty_deletes(&mut out).unwrap();
    let out = String::from_utf8(out).unwrap();
    assert!(out.contains("\x1b_Ga=d,d=I,i=7000001,q=2\x1b\\"), "{out:?}");
    assert!(out.contains("\x1bPtmux;\x1b\x1b_Ga=d,d=I,i=7000002,q=2\x1b\x1b\\\x1b\\"), "{out:?}");
}

#[test]
fn stderr_is_styled_apart_from_stdout() {
    let style = |name| {
        let out = serde_json::json!({"output_type": "stream", "name": name, "text": "x\n"});
        output_lines(&out)[0].style
    };
    assert_eq!(style("stdout"), OUTPUT_STYLE);
    assert_eq!(style("stderr"), STDERR_STYLE);
}

/// IPython < 9.14 highlights the error range as syntax colours on yellow.
#[test]
fn traceback_highlight_is_black_on_yellow() {
    let out = serde_json::json!({"output_type": "error",
            "traceback": ["x = \x1b[38;5;255;43mf\x1b[38;5;70;43m(a)\x1b[39;49m"]});
    let line = &output_lines(&out)[0];
    let hl: Vec<_> = line.spans.iter().filter(|s| s.style.bg.is_some()).collect();
    assert!(!hl.is_empty());
    assert!(hl.iter().all(|s| s.style.fg == Some(Color::Black)), "{line:?}");
}

/// Invalid code (`f"{}"`, `np.(`) gets the theme's `invalid` style: text in
/// the background colour on a coloured background. It must stay visible.
#[test]
fn invalid_code_stays_visible_in_every_theme() {
    let nb = Notebook { cells: Vec::new(), extra: serde_json::Map::new(), header: None };
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
        .map(|s| (s.content.to_string(), s.style.add_modifier.contains(Modifier::REVERSED)))
        .collect();
    assert_eq!(
        flags,
        [("ab".into(), false), ("c".into(), true), ("de".into(), true), ("f".into(), false)]
    );
    // empty line inside a linewise selection still shows a mark
    let mut empty = Line::raw("");
    overlay_reversed(&mut empty, 0, 0);
    assert_eq!(line_chars(&empty), 1);
}

#[test]
fn try_image_steps() {
    let b64: String = PNG.split_whitespace().collect();
    let bytes = base64::engine::general_purpose::STANDARD.decode(b64).expect("b64 decode");
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
    let nb = Notebook { cells: vec![cell], extra: serde_json::Map::new(), header: None };
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
    let nb = Notebook { cells: vec![], extra: serde_json::Map::new(), header: None };
    let picker = Picker::from_fontsize(FontSize::new(8, 16));
    let r = Rendered::build(&nb, Some(picker), Default::default());
    // 640x480 > 18*16=288 px tall -> prescaled to exactly the cap
    let entry = r.image_entry(image::DynamicImage::new_rgba8(640, 480), false).unwrap();
    assert_eq!(entry.rows, max_image_rows());
    assert_eq!(entry.cols, 48); // 640 * 288/480 = 384 px / 8 px per col

    // small image: untouched, ceil to cells
    let entry = r.image_entry(image::DynamicImage::new_rgba8(100, 100), false).unwrap();
    assert_eq!((entry.cols, entry.rows), (13, 7));

    // native-size toggle: no prescale, full 480 px = 30 rows
    let entry = r.image_entry(image::DynamicImage::new_rgba8(640, 480), true).unwrap();
    assert_eq!((entry.cols, entry.rows), (80, 30));
}

#[test]
fn all_inline_math_becomes_images() {
    let cell: Cell = serde_json::from_value(serde_json::json!({
            "cell_type": "markdown",
            "metadata": {},
            "source": "on the $\\tau$ grid, and $ \\int_0^\\infty \\frac{1}{x^2} $ too\n## Read off $\\tau_\\mathrm{int}$"
        }))
        .unwrap();
    let nb = Notebook { cells: vec![cell], extra: serde_json::Map::new(), header: None };
    let picker = Picker::from_fontsize(FontSize::new(8, 16));
    let mut r = Rendered::build(&nb, Some(picker), Default::default());
    r.ensure(0, &nb.cells[0]);
    let block = &r.blocks[0];
    assert_eq!(block.images.len(), 3, "heading math renders too");
    assert_eq!((block.images[2].line, block.images[2].col), (1, "Read off ".len() as u16));
    // ALL inline math is exactly one row; too small => author uses $$
    assert!(block.images.iter().all(|i| i.rows == 1));
    assert_eq!(block.images[0].col, "on the ".len() as u16);
    assert_eq!(block.lines.len(), 2);
}

#[test]
fn long_output_windows_scrolls_and_collapses() {
    let text: String = (0..40).map(|i| format!("l{i}")).collect::<Vec<_>>().join("\n");
    let cell: Cell = serde_json::from_value(serde_json::json!({
        "cell_type": "code", "source": "x", "metadata": {},
        "outputs": [{"output_type": "stream", "name": "stdout", "text": text}]
    }))
    .unwrap();
    let nb = Notebook { cells: vec![cell], extra: serde_json::Map::new(), header: None };
    let mut r = Rendered::build(&nb, None, Default::default());
    r.ensure(0, &nb.cells[0]);
    let b = &mut r.blocks[0];
    let len = b.out_len();
    assert!(len > max_output_rows());
    // default follows the tail
    assert_eq!(b.window(), (len - max_output_rows(), max_output_rows(), true));
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
    let nb = Notebook { cells: vec![], extra: serde_json::Map::new(), header: None };
    let picker = Picker::from_fontsize(FontSize::new(8, 16));
    let r = Rendered::build(&nb, Some(picker), Default::default());
    // a full-size plot: 18 reserved rows, above the 15-row text budget
    let entry = r.image_entry(image::DynamicImage::new_rgba8(640, 480), false).unwrap();
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
fn julia_r_and_rust_cells_have_a_syntax_and_an_editor_suffix() {
    for (lang, ext) in [("julia", "jl"), ("r", "R"), ("rust", "rs")] {
        let cell: Cell = serde_json::from_value(serde_json::json!({
            "cell_type": "code", "metadata": {}, "source": "x = 1", "outputs": []
        }))
        .unwrap();
        let mut extra = serde_json::Map::new();
        extra.insert("metadata".into(), serde_json::json!({"kernelspec": {"language": lang}}));
        let nb = Notebook { cells: vec![cell], extra, header: None };
        let r = Rendered::build(&nb, None, Default::default());
        assert!(r.ps.find_syntax_by_token(lang).is_some(), "{lang}");
        assert_eq!(r.edit_suffix(&nb.cells[0]), ext, "{lang}");
    }
}

#[test]
fn html_capable_outputs_show_their_plain_text() {
    // IRkernel's bundle for `c(1, 4, 9)`; without text/html (IPython's
    // Markdown()) the markdown is what the user asked for
    let text = |data: Value| -> String {
        let cell: Cell = serde_json::from_value(serde_json::json!({
            "cell_type": "code", "metadata": {}, "source": "x",
            "outputs": [{"output_type": "execute_result", "metadata": {}, "data": data}]
        }))
        .unwrap();
        let nb = Notebook { cells: vec![cell], extra: serde_json::Map::new(), header: None };
        let mut r = Rendered::build(&nb, None, Default::default());
        r.ensure(0, &nb.cells[0]);
        let lines = r.blocks[0].lines.iter();
        lines
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    };
    let r_vector = text(serde_json::json!({
        "text/html": "<ol><li>1</li><li>4</li><li>9</li></ol>",
        "text/markdown": "1. 1\n2. 4\n3. 9",
        "text/latex": "\\begin{enumerate}\\item 1\\end{enumerate}",
        "text/plain": "[1] 1 4 9"
    }));
    assert!(r_vector.contains("[1] 1 4 9"), "{r_vector}");
    let markdown = text(serde_json::json!({"text/markdown": "**b**", "text/plain": "<Markdown>"}));
    assert!(!markdown.contains("<Markdown>"), "{markdown}");
}

#[test]
fn markdown_click_map_points_at_source_lines() {
    let src = "# title\n\ntext\n| a |\n|---|\n| 1 |\nafter";
    let (_, _, map) = {
        let nb = Notebook { cells: vec![], extra: serde_json::Map::new(), header: None };
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
    let nb = Notebook { cells: vec![cell], extra: serde_json::Map::new(), header: None };
    let mut r = Rendered::build(&nb, None, Default::default());
    r.ensure(0, &nb.cells[0]);
    let text: Vec<String> = r.blocks[0]
        .lines
        .iter()
        .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
        .collect();
    assert!(text[0].contains("docs") && text[0].contains("⟨https://x.y⟩"), "{}", text[0]);
    assert!(text[0].contains("bold") && text[0].contains("it") && !text[0].contains('*'));
    assert!(text[1].contains(" a ") && text[1].contains('│'));
    assert!(text[2].contains('┼'));
    assert!(text[3].contains(" 1 ") && text[3].contains(" 22 "));
}

fn buffer_rows(term: &ratatui::Terminal<ratatui::backend::TestBackend>) -> Vec<String> {
    let buf = term.backend().buffer();
    (0..buf.area.height)
        .map(|y| (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect::<String>())
        .collect()
}

#[test]
fn wrap_breaks_at_spaces_hard_breaks_long_words_and_keeps_nbsp() {
    let text = |w: &Wrapped| -> Vec<String> {
        w.rows.iter().map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect()).collect()
    };
    let red = Style::new().fg(Color::Red);
    let line = Line::from(vec![Span::raw("aaa bbb "), Span::styled("ccc", red)]);
    let w = wrap_lines(std::slice::from_ref(&line), 8);
    assert_eq!(text(&w), ["aaa bbb ", "ccc"]);
    assert_eq!(w.rows[1].spans[0].style, red, "styles survive the split");
    assert_eq!(w.origin[1], RowOrigin { line: 0, col: 8, x: 8 });
    // unwrapped when it fits, or with width 0
    assert_eq!(wrap_lines(std::slice::from_ref(&line), 11).rows.len(), 1);
    assert_eq!(wrap_lines(&[line], 0).rows.len(), 1);
    // a word longer than the row hard-breaks
    assert_eq!(text(&wrap_lines(&[Line::raw("abcdefghij")], 4)), ["abcd", "efgh", "ij"]);
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
            if i % 2 == 0 { "(start, stop, num=50)" } else { "int" },
        );
    }
    c.sel = 13; // scrolled past the first window
    app.completion = Some(c);
    let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(70, 20)).unwrap();
    terminal.draw(|f| draw(f, &mut app, &mut rendered)).unwrap();
    let rows = buffer_rows(&terminal);
    if std::env::var_os("JOTTER_SHOW").is_some() {
        println!("{}", rows.join("\n"));
    }
    let pop: Vec<&String> =
        rows.iter().filter(|r| r.contains("Function") || r.contains("Variable")).collect();
    assert_eq!(pop.len(), 12, "12-row window");
    // text column starts under the token after `np.` ("▌ x = np." = 9 cols)
    let col = |r: &str, pat: &str| r.find(pat).map(|b| r[..b].chars().count());
    assert!(pop.iter().all(|r| col(r, "li").is_some_and(|c| c == 9)), "{pop:#?}");
    let kind_col = col(pop[0], "Variable").or(col(pop[0], "Function"));
    assert!(pop.iter().all(|r| col(r, "Variable").or(col(r, "Function")) == kind_col));
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
    let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(60, 10)).unwrap();
    terminal.draw(|f| draw(f, &mut app, &mut rendered)).unwrap();
    let rows = buffer_rows(&terminal);
    let code = rows.iter().position(|r| r.contains("x = np.std(a,")).unwrap();
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
    assert_eq!(expand_tabs("a\tb\n\x1b[31mab\tc"), "a       b\n\x1b[31mab      c");
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
    let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(40, 12)).unwrap();
    terminal.draw(|f| draw(f, &mut app, &mut rendered)).unwrap();
    let words: usize = buffer_rows(&terminal).iter().map(|r| r.matches("word").count()).sum();
    assert_eq!(words, 30, "every word visible across wrapped rows");
    // clicks on a continuation row map back to the one source line
    assert!(matches!(app.hit[2].kind, crate::app::HitKind::Source { line: 0, .. }));
}

#[test]
fn docs_pager_is_only_as_large_as_its_content() {
    let draw = |text: &str, top: usize| {
        let mut t = ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 12)).unwrap();
        let mut pager = crate::app::Pager { title: "docs".into(), text: text.into(), top };
        t.draw(|f| draw_pager(f, &mut pager)).unwrap();
        (buffer_rows(&t), pager.top)
    };
    let (rows, _) = draw("Type: int\nDocstring: x", 0);
    let boxed: Vec<&String> = rows.iter().filter(|r| r.contains(['│', '╭', '╰'])).collect();
    assert_eq!(boxed.len(), 4, "two lines + borders:\n{}", rows.join("\n"));
    assert!(boxed[0].trim().chars().count() < 30, "narrow: {}", boxed[0]);
    assert!(boxed[3].contains("any key closes") && !boxed[3].contains("scroll"));
    // more lines than fit: full height, scroll hint, `top` clamped
    let long = (0..30).map(|i| format!("line {i}")).collect::<Vec<_>>().join("\n");
    let (rows, top) = draw(&long, usize::MAX);
    // (and the whole screen: it would cover over 70% of it anyway)
    assert_eq!(top, 30 - 10);
    assert!(rows[0].contains("21–30 of 30") && rows[10].contains("line 29"));
    assert!(rows[11].contains("scroll"), "{}", rows.join("\n"));
    // a line wider than the screen wraps instead of losing its end
    let (rows, _) = draw(&format!("/nix/store/{}/numpy/__init__.py", "x".repeat(90)), 0);
    assert!(rows.join("\n").contains("__init__.py"), "{}", rows.join("\n"));
}

#[test]
fn docs_are_regrouped_and_long_lines_wrap() {
    use ansi_to_tui::IntoText;
    let plain = |t: Text| t.lines.iter().map(Line::to_string).collect::<Vec<_>>();
    // real IPython output for a class
    let class = "\x1b[31mInit signature:\x1b[39m A(x, y=\x1b[32m2\x1b[39m)\n\x1b[31mDocstring:\x1b[39m      Class doc.\n\nType: not a field\n\x1b[31mInit docstring:\x1b[39m init doc\n\x1b[31mFile:\x1b[39m           ~/a.py\n\x1b[31mType:\x1b[39m           type\n\x1b[31mSubclasses:\x1b[39m     ";
    let out = docs_layout(class.into_text().unwrap());
    assert_eq!(
        plain(out.clone()),
        [
            "A(x, y=2)",
            "",
            "Class doc.",
            "",
            "Type: not a field",
            "",
            "Init docstring:",
            "init doc",
            "",
            "File: ~/a.py",
            "Type: type",
        ]
    );
    assert_eq!(out.lines[0].spans[1].style.fg, Some(Color::Green), "colors survive");
    assert_eq!(out.lines[10].spans[0].style.fg, Some(Color::DarkGray));
    // no colored labels (another kernel): untouched
    let other = "Type: int\n  Docstring: x";
    assert_eq!(plain(docs_layout(other.into_text().unwrap())), ["Type: int", "  Docstring: x"]);
}

#[test]
fn overlays_are_padded_and_help_is_grouped() {
    let render = |w: u16, h: u16, f: &dyn Fn(&mut Frame)| {
        let mut t = ratatui::Terminal::new(ratatui::backend::TestBackend::new(w, h)).unwrap();
        t.draw(|fr| f(fr)).unwrap();
        buffer_rows(&t)
    };
    let help = |w, h, filter: &str, top| {
        let state =
            std::cell::RefCell::new(crate::app::Help { filter: filter.into(), typing: false, top });
        let rows = render(w, h, &|f| draw_help(f, &mut state.borrow_mut()));
        (rows, state.into_inner().top)
    };
    let (rows, _) = help(120, 45, "", 0);
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
    // over 70% of the height: the whole screen
    assert!(rows[0].starts_with('╭') && rows[44].starts_with('╰'), "{all}");
    // a filter keeps the headings of what matches and fits one column
    let (rows, _) = help(120, 45, "KERNEL", 0);
    let all = rows.join("\n");
    assert!(all.contains("Run") && all.contains("interrupt kernel"), "{all}");
    assert!(!all.contains("Navigate") && !all.contains("run in place"), "{all}");
    // the panel doesn't shrink with the matches
    assert!(rows[0].starts_with('╭') && rows[44].starts_with('╰'), "{all}");
    assert!(help(120, 45, "no such key", 0).0.join("\n").contains("no match"));
    // a short screen scrolls: the end is reachable, `top` is clamped
    let (rows, top) = help(120, 20, "", usize::MAX);
    let all = rows.join("\n");
    assert!(top > 0 && all.contains("Shift+drag") && all.contains("scroll"), "{all}");
    assert!(!all.contains("Navigate"), "{all}");
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
    // wide enough for the status line to show the whole temp path (macOS
    // puts it under /var/folders/<random>/T) and still the cell counter
    let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(160, 30)).unwrap();
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
    assert!(all.contains(" markdown  ─") && all.contains("Big Title"), "{all}");
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
    let nb = Notebook { cells, extra: serde_json::Map::new(), header: None };
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
    let math =
        "$$\\sum_{n=0}^{\\infty} \\frac{x^n}{n!} = \\int_0^1 \\frac{\\mathrm{d}t}{\\sqrt{1-t^2}}$$";
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
    let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(60, 20)).unwrap();
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
    let above = (0..299).rev().find(|&i| i % 2 == 0 && !rendered.blocks[i].rendered).unwrap();
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
    let nb = Notebook { cells: vec![cell], extra: serde_json::Map::new(), header: None };
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
    let src =
        std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/showcase.ipynb")).unwrap();
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
    assert!(renders < 2 * n, "{renders} renders for {n} cells in 3 sweeps");
}

/// Over the budget, the least recently seen blocks go first, and only
/// until the rest fits; the render window is never evicted.
#[test]
fn eviction_drops_least_recently_seen_until_within_budget() {
    let nb = Notebook { cells: Vec::new(), extra: serde_json::Map::new(), header: None };
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

#[test]
fn command_line_draws_as_a_titled_popup() {
    let nb_json = serde_json::json!({
        "cells": [{"cell_type": "code", "metadata": {}, "source": ["x"], "outputs": []}],
        "metadata": {}, "nbformat": 4, "nbformat_minor": 5
    });
    let path = std::env::temp_dir().join(format!("jotter-cmdline-{}.ipynb", std::process::id()));
    std::fs::write(&path, nb_json.to_string()).unwrap();
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = crate::app::App::open(path.clone(), None, tx).unwrap();
    std::fs::remove_file(&path).ok();
    app.prompt =
        Some(crate::app::Prompt { kind: crate::app::PromptKind::Command, buf: "wq".into() });
    let mut rendered = Rendered::build(&app.notebook, None, Default::default());
    let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(60, 20)).unwrap();
    terminal.draw(|f| draw(f, &mut app, &mut rendered)).unwrap();
    let rows = buffer_rows(&terminal);
    let top = rows.iter().position(|r| r.contains("─ Cmdline ─")).expect("title");
    assert!(rows[top].trim_start().starts_with('╭'));
    assert!(rows[top + 1].contains("│ › wq"), "{}", rows[top + 1]);
    assert!(!rows.last().unwrap().contains("wq"), "not echoed in the status line");
}

#[test]
fn visual_selection_marks_its_cells_in_the_gutter() {
    let nb_json = serde_json::json!({
        "cells": (["a", "b", "c"].map(|s| serde_json::json!(
            {"cell_type": "code", "metadata": {}, "source": [s], "outputs": []}))),
        "metadata": {}, "nbformat": 4, "nbformat_minor": 5
    });
    let path = std::env::temp_dir().join(format!("jotter-vis-{}.ipynb", std::process::id()));
    std::fs::write(&path, nb_json.to_string()).unwrap();
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = crate::app::App::open(path.clone(), None, tx).unwrap();
    std::fs::remove_file(&path).ok();
    app.visual = Some(0);
    app.selected = 1;
    let mut rendered = Rendered::build(&app.notebook, None, Default::default());
    let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(40, 12)).unwrap();
    terminal.draw(|f| draw(f, &mut app, &mut rendered)).unwrap();
    let rows = buffer_rows(&terminal);
    if std::env::var_os("JOTTER_SHOW").is_some() {
        println!("{}", rows.join("\n"));
    }
    // the bar keeps its shape; only its colour marks the selection
    let fg = |src: &str| {
        let y = rows.iter().position(|r| r.trim_end() == format!("▌ {src}"));
        terminal.backend().buffer()[(0, y.unwrap() as u16)].fg
    };
    assert_eq!((fg("a"), fg("b")), (VISUAL_COLOR, VISUAL_COLOR));
    assert_eq!(fg("c"), Color::Cyan, "outside the selection");
    assert!(rows.last().unwrap().contains("V-LINE"));
}

#[test]
fn zt_zz_zb_place_the_selected_cell() {
    let cells: Vec<_> = (0..30)
            .map(|i| serde_json::json!({"cell_type": "code", "metadata": {}, "source": [format!("c{i}")], "outputs": []}))
            .collect();
    let nb_json =
        serde_json::json!({"cells": cells, "metadata": {}, "nbformat": 4, "nbformat_minor": 5});
    let path = std::env::temp_dir().join(format!("jotter-zt-{}.ipynb", std::process::id()));
    std::fs::write(&path, nb_json.to_string()).unwrap();
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = crate::app::App::open(path.clone(), None, tx).unwrap();
    std::fs::remove_file(&path).ok();
    let mut rendered = Rendered::build(&app.notebook, None, Default::default());
    let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(30, 13)).unwrap();
    app.selected = 15;
    let mut row_of_c15 = |app: &mut crate::app::App, keys: &str| {
        for c in keys.chars() {
            let k = crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Char(c),
                crossterm::event::KeyModifiers::NONE,
            );
            app.on_key(k, &mut rendered);
        }
        terminal.draw(|f| draw(f, app, &mut rendered)).unwrap();
        let rows = buffer_rows(&terminal);
        rows.iter().position(|r| r.trim_end() == "▌ c15").unwrap()
    };
    // 12 body rows; each cell is header + source + separator
    assert_eq!(row_of_c15(&mut app, "zt"), 1, "header on the top row");
    assert_eq!(row_of_c15(&mut app, "zb"), 11, "source on the bottom row");
    assert_eq!(row_of_c15(&mut app, "zz"), 6, "middle");
    assert_eq!(row_of_c15(&mut app, "j"), 6, "the view stays until it must move");
}

#[test]
fn marks_show_in_headers_and_a_sidebar() {
    let cells: Vec<_> = ["import numpy as np", "x = 1", "plot(x)"]
            .iter()
            .map(|s| serde_json::json!({"cell_type": "code", "metadata": {}, "source": [s], "outputs": []}))
            .collect();
    let nb_json =
        serde_json::json!({"cells": cells, "metadata": {}, "nbformat": 4, "nbformat_minor": 5});
    let path = std::env::temp_dir().join(format!("jotter-marks-{}.ipynb", std::process::id()));
    std::fs::write(&path, nb_json.to_string()).unwrap();
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = crate::app::App::open(path.clone(), None, tx).unwrap();
    std::fs::remove_file(&path).ok();
    let id = |i: usize| app.notebook.cells[i].extra["id"].as_str().unwrap().to_string();
    let (a, b) = (id(2), id(0));
    app.marks.insert('a', a);
    app.marks.insert('b', b);
    let mut rendered = Rendered::build(&app.notebook, None, Default::default());
    let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(90, 14)).unwrap();
    terminal.draw(|f| draw(f, &mut app, &mut rendered)).unwrap();
    let rows = buffer_rows(&terminal);
    if std::env::var_os("JOTTER_SHOW").is_some() {
        println!("{}", rows.join("\n"));
    }
    assert!(rows[0].contains("In [ ]  'b") && rows[0].contains("Marks"), "{}", rows[0]);
    // sidebar in notebook order: b (cell 1) before a (cell 3)
    let b_row = rows.iter().position(|r| r.contains(" 'b 1 import numpy")).unwrap();
    let a_row = rows.iter().position(|r| r.contains(" 'a 3 plot(x)")).unwrap();
    assert!(b_row < a_row);
    // too narrow: no sidebar, headers still carry the letters
    let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(60, 14)).unwrap();
    terminal.draw(|f| draw(f, &mut app, &mut rendered)).unwrap();
    let rows = buffer_rows(&terminal);
    assert!(!rows.join("").contains("Marks"));
    assert!(rows[0].contains("'b"));
}
