//! Notebook-level cell edits (insert/delete/move/split/merge/clear/type)
//! as self-inverting `CellOp`s: `App::apply` performs one and returns its
//! inverse, so the same ops drive edits, undo, and redo.

use super::*;

/// A notebook-level cell edit. `App::apply` performs one and returns its
/// inverse, so the same ops drive edits, undo, and redo (both stacks hold
/// ops that undo whatever was last done).
pub(super) enum CellOp {
    Remove(usize),
    Insert(usize, Box<Cell>),
    /// Swap two cells; the selection follows the selected cell.
    Swap(usize, usize),
    /// Source only, so outputs that arrived since (a re-run) survive undo.
    SetSource(usize, String),
    /// Outputs + execution_count (clear outputs).
    SetOutputs(usize, Option<Vec<Value>>, Option<Value>),
    SetCell(usize, Box<Cell>),
    /// Applied in order as one undo step (split, merge, clear all).
    Batch(Vec<CellOp>),
}

/// Undo/redo depth.
pub(super) const UNDO_CAP: usize = 100;

/// Push onto an undo/redo stack, dropping the oldest beyond UNDO_CAP.
pub(super) fn push_capped(stack: &mut Vec<CellOp>, op: CellOp) {
    stack.push(op);
    if stack.len() > UNDO_CAP {
        stack.remove(0); // ponytail: O(n) at n=100, irrelevant
    }
}

impl CellOp {
    /// Touches outputs and counts only (clear outputs, and its undo): no
    /// change to a file that doesn't store them.
    fn outputs_only(&self) -> bool {
        match self {
            CellOp::SetOutputs(..) => true,
            CellOp::Batch(ops) => ops.iter().all(CellOp::outputs_only),
            _ => false,
        }
    }
}

impl App {
    /// Perform `op` as a new undo step (clears redo).
    pub(super) fn record(&mut self, op: CellOp, rendered: &mut Rendered) {
        let outputs_only = op.outputs_only();
        let inverse = self.apply(op, rendered);
        push_capped(&mut self.undo_stack, inverse);
        self.redo_stack.clear();
        self.touch_for(outputs_only);
    }

    pub(super) fn undo(&mut self, rendered: &mut Rendered) {
        let Some(op) = self.undo_stack.pop() else {
            self.message = Some("nothing to undo".into());
            return;
        };
        let outputs_only = op.outputs_only();
        let inverse = self.apply(op, rendered);
        push_capped(&mut self.redo_stack, inverse);
        self.touch_for(outputs_only);
        self.message = Some("undone (Ctrl+r redoes)".into());
    }

    pub(super) fn redo(&mut self, rendered: &mut Rendered) {
        let Some(op) = self.redo_stack.pop() else {
            self.message = Some("nothing to redo".into());
            return;
        };
        let outputs_only = op.outputs_only();
        let inverse = self.apply(op, rendered);
        push_capped(&mut self.undo_stack, inverse);
        self.touch_for(outputs_only);
        self.message = Some("redone".into());
    }

    fn touch_for(&mut self, outputs_only: bool) {
        if outputs_only {
            self.touch_outputs();
        } else {
            self.touch();
        }
    }

    /// Perform one cell op (model, render cache, running map, selection) and
    /// return the op that reverts it. Stacks are LIFO, so stored indices are
    /// valid by construction.
    pub(super) fn apply(&mut self, op: CellOp, rendered: &mut Rendered) -> CellOp {
        let cells = &mut self.notebook.cells;
        match op {
            CellOp::Remove(at) => {
                let cell = cells.remove(at);
                rendered.remove_cell(at);
                self.remap_running(|i| match i.cmp(&at) {
                    std::cmp::Ordering::Less => Some(i),
                    std::cmp::Ordering::Equal => None,
                    std::cmp::Ordering::Greater => Some(i - 1),
                });
                self.selected = at.min(self.notebook.cells.len().saturating_sub(1));
                CellOp::Insert(at, Box::new(cell))
            }
            CellOp::Insert(at, cell) => {
                rendered.insert_cell(at);
                cells.insert(at, *cell);
                self.remap_running(|i| Some(if i >= at { i + 1 } else { i }));
                self.selected = at;
                CellOp::Remove(at)
            }
            CellOp::Swap(a, b) => {
                cells.swap(a, b);
                rendered.swap_cells(a, b);
                let swap = |i: usize| {
                    if i == a {
                        b
                    } else if i == b {
                        a
                    } else {
                        i
                    }
                };
                self.remap_running(|i| Some(swap(i)));
                self.selected = swap(self.selected);
                CellOp::Swap(a, b)
            }
            CellOp::SetSource(at, source) => {
                let cell = &mut cells[at];
                let old = std::mem::replace(&mut cell.source, source);
                rendered.rebuild_cell(at, cell);
                self.selected = at;
                CellOp::SetSource(at, old)
            }
            CellOp::SetOutputs(at, outputs, count) => {
                let cell = &mut cells[at];
                let old_out = std::mem::replace(&mut cell.outputs, outputs);
                let old_count = match count {
                    Some(c) => cell.extra.insert("execution_count".into(), c),
                    None => cell.extra.remove("execution_count"),
                };
                rendered.rebuild_cell(at, cell);
                if let Some(b) = rendered.blocks.get_mut(at) {
                    b.elapsed = None;
                }
                self.selected = at;
                CellOp::SetOutputs(at, old_out, old_count)
            }
            CellOp::SetCell(at, cell) => {
                let old = std::mem::replace(&mut cells[at], *cell);
                rendered.rebuild_cell(at, &cells[at]);
                self.selected = at;
                CellOp::SetCell(at, Box::new(old))
            }
            CellOp::Batch(ops) => {
                let mut inverse: Vec<CellOp> =
                    ops.into_iter().map(|op| self.apply(op, rendered)).collect();
                inverse.reverse();
                CellOp::Batch(inverse)
            }
        }
    }

    pub(super) fn insert_cell(&mut self, at: usize, rendered: &mut Rendered) {
        self.insert_cell_value(at, Cell::new_code(), rendered);
    }

    pub(super) fn insert_cell_value(&mut self, at: usize, cell: Cell, rendered: &mut Rendered) {
        let at = at.min(self.notebook.cells.len());
        self.record(CellOp::Insert(at, Box::new(cell)), rendered);
    }

    /// `dd`, or `d` on a `V` selection: delete the cells, keeping them for `p`.
    pub(super) fn delete_cells(&mut self, r: std::ops::Range<usize>, rendered: &mut Rendered) {
        if r.is_empty() || r.end > self.notebook.cells.len() {
            return;
        }
        self.yanked = self.notebook.cells[r.clone()].to_vec();
        let n = r.len();
        self.record(
            CellOp::Batch((0..n).map(|_| CellOp::Remove(r.start)).collect()),
            rendered,
        );
        self.message = Some(match n {
            1 => "cell deleted (p pastes it back)".into(),
            n => format!("{n} cells deleted (p pastes them back)"),
        });
    }

    /// `p` / `P`: the last deleted or yanked cells as new cells (fresh ids),
    /// inserted at `at`: after (`selected + 1`) or before the selected cell.
    pub(super) fn paste(&mut self, at: usize, rendered: &mut Rendered) {
        let at = at.min(self.notebook.cells.len());
        let ops: Vec<CellOp> = (self.yanked.iter().cloned().enumerate())
            .map(|(k, mut cell)| {
                cell.extra.insert("id".into(), new_cell_id().into());
                CellOp::Insert(at + k, Box::new(cell))
            })
            .collect();
        if !ops.is_empty() {
            self.record(CellOp::Batch(ops), rendered);
        }
    }

    /// Move the cells in `r` one place down (`delta` 1) or up (-1) as a
    /// block: the neighbour bubbles across to the other side. The selection
    /// (and a `V` anchor) moves along. False at the notebook's edge.
    pub(super) fn move_cells(
        &mut self,
        r: std::ops::Range<usize>,
        delta: isize,
        rendered: &mut Rendered,
    ) -> bool {
        let swaps: Vec<CellOp> = if delta > 0 {
            if r.end >= self.notebook.cells.len() {
                return false;
            }
            r.clone().rev().map(|i| CellOp::Swap(i, i + 1)).collect()
        } else {
            if r.start == 0 {
                return false;
            }
            r.clone().map(|i| CellOp::Swap(i - 1, i)).collect()
        };
        let selected = self.selected;
        self.record(CellOp::Batch(swaps), rendered);
        self.selected = selected.saturating_add_signed(delta);
        self.visual = self.visual.map(|v| v.saturating_add_signed(delta));
        true
    }

    /// Split the cell being edited at the editor cursor: text before stays,
    /// text after becomes a new cell below (same type, no outputs).
    pub(super) fn split_cell(&mut self, rendered: &mut Rendered) {
        let Some(editor) = self.editor.take() else {
            return;
        };
        let (row, col) = editor.cursor();
        let source = editor.source();
        if let Some(session) = editor.into_session() {
            self.nvim = Some(session);
        }
        let at = self.selected;
        let Some(cell) = self.notebook.cells.get(at) else {
            return;
        };
        let offset = char_offset(&source, (row, col));
        let (before, after) = source.split_at(
            source
                .char_indices()
                .nth(offset)
                .map_or(source.len(), |(b, _)| b),
        );
        let before = before.strip_suffix('\n').unwrap_or(before).to_string();
        let after = after.strip_prefix('\n').unwrap_or(after).to_string();
        let mut second = cell.clone();
        second.source = after;
        second.extra.insert("id".into(), new_cell_id().into());
        second.extra.remove("attachments");
        if second.cell_type == "code" {
            second.outputs = Some(Vec::new());
            second.extra.insert("execution_count".into(), Value::Null);
        }
        self.record(
            CellOp::Batch(vec![
                CellOp::SetSource(at, before),
                CellOp::Insert(at + 1, Box::new(second)),
            ]),
            rendered,
        );
        self.message = Some("cell split (u undoes)".into());
    }

    /// `M`: merge the cells in `r` into the first (JupyterLab's Shift+M),
    /// which keeps its type and outputs. Empty sources add no blank line.
    pub(super) fn merge_cells(&mut self, r: std::ops::Range<usize>, rendered: &mut Rendered) {
        if r.len() < 2 || r.end > self.notebook.cells.len() {
            self.message = Some("no cell below to merge".into());
            return;
        }
        let merged = self.notebook.cells[r.clone()]
            .iter()
            .map(|c| c.source.as_str())
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join("\n");
        let n = r.len();
        let mut ops = vec![CellOp::SetSource(r.start, merged)];
        ops.extend((1..n).map(|_| CellOp::Remove(r.start + 1)));
        self.record(CellOp::Batch(ops), rendered);
        self.selected = r.start;
        self.message = Some(format!("merged {n} cells (u undoes)"));
    }

    /// `c`/`C`: clear outputs + execution counts of the code cells in `range`.
    pub(super) fn clear_outputs(&mut self, range: std::ops::Range<usize>, rendered: &mut Rendered) {
        let selected = self.selected;
        let ops: Vec<CellOp> = range
            .filter(|&i| {
                self.notebook.cells.get(i).is_some_and(|c| {
                    c.cell_type == "code"
                        && (c.outputs.as_ref().is_some_and(|o| !o.is_empty())
                            || c.execution_count().is_some())
                })
            })
            .map(|i| CellOp::SetOutputs(i, Some(Vec::new()), Some(Value::Null)))
            .collect();
        if ops.is_empty() {
            self.message = Some("no outputs to clear".into());
            return;
        }
        let n = ops.len();
        self.record(CellOp::Batch(ops), rendered);
        self.selected = selected;
        self.message = Some(format!("cleared {n} cell output(s) (u undoes)"));
    }

    /// Pipe each code cell in `range` through the configured `formatter`, as
    /// one undo step. Skips the cell open in the editor (its buffer would go
    /// stale). Returns the status line.
    pub(super) fn format_cells(
        &mut self,
        range: std::ops::Range<usize>,
        rendered: &mut Rendered,
    ) -> String {
        let cmd = &crate::config::get().formatter;
        let dir = self.path.parent().filter(|p| !p.as_os_str().is_empty());
        let lang = crate::ui::notebook_language(&self.notebook);
        let editing = self.editor.is_some().then_some(self.selected);
        let selected = self.selected;
        let (mut ops, mut failed, mut first_err) = (Vec::new(), 0, None);
        // ponytail: one process per cell, sequential; batch if big notebooks lag
        for i in range {
            let Some(cell) = self.notebook.cells.get(i) else {
                break;
            };
            if cell.cell_type != "code" || cell.source.trim().is_empty() || editing == Some(i) {
                continue;
            }
            match run_formatter(cmd, &cell.source, dir, &lang) {
                Ok(out) if out != cell.source => ops.push(CellOp::SetSource(i, out)),
                Ok(_) => {}
                Err(e) => {
                    failed += 1;
                    first_err.get_or_insert(format!("cell {}: {e}", i + 1));
                }
            }
        }
        let n = ops.len();
        if n > 0 {
            self.record(CellOp::Batch(ops), rendered);
            self.selected = selected;
        }
        match first_err {
            Some(e) => format!("formatted {n}, {failed} failed — {e}"),
            None if n > 0 => format!("formatted {n} cell(s) (u undoes)"),
            None => "already formatted".into(),
        }
    }

    /// Cycle cell type: code -> markdown -> raw (rendered as latex) -> code.
    pub(super) fn toggle_type(&mut self, rendered: &mut Rendered) {
        let Some(mut cell) = self.notebook.cells.get(self.selected).cloned() else {
            return;
        };
        fn metadata(c: &mut Cell) -> &mut Map<String, Value> {
            c.extra
                .entry("metadata")
                .or_insert_with(|| Value::Object(Map::new()))
                .as_object_mut()
                .expect("metadata is an object")
        }
        match cell.cell_type.as_str() {
            "code" => {
                cell.cell_type = "markdown".into();
                cell.outputs = None;
                cell.extra.remove("execution_count");
            }
            "markdown" => {
                cell.cell_type = "raw".into();
                metadata(&mut cell).insert("format".into(), "text/latex".into());
                self.message = Some("raw latex cell".into());
            }
            _ => {
                cell.cell_type = "code".into();
                metadata(&mut cell).remove("format");
                cell.outputs = Some(Vec::new());
                cell.extra.insert("execution_count".into(), Value::Null);
            }
        }
        self.record(CellOp::SetCell(self.selected, Box::new(cell)), rendered);
    }

    /// `yy`, or `y` on a `V` selection: the cells into the paste register
    /// (`p`) and their sources onto the system clipboard via OSC 52.
    pub(super) fn yank_cells(&mut self, r: std::ops::Range<usize>) {
        let Some(cells) = self.notebook.cells.get(r) else {
            return;
        };
        osc52(
            &cells
                .iter()
                .map(|c| c.source.as_str())
                .collect::<Vec<_>>()
                .join("\n\n"),
        );
        self.yanked = cells.to_vec();
        self.message = Some(match cells.len() {
            1 => "cell yanked (p pastes) · source on the clipboard".into(),
            n => format!("{n} cells yanked (p pastes) · sources on the clipboard"),
        });
    }
}

/// Run `cmd` through `sh -c` (so pipelines work) with `source` on stdin
/// and the kernel language in `$JOTTER_LANGUAGE`;
/// the formatted text without the trailing newline cells don't carry. Err is
/// a one-line reason.
fn run_formatter(
    cmd: &str,
    source: &str,
    dir: Option<&std::path::Path>,
    lang: &str,
) -> Result<String, String> {
    use std::io::Write;
    use std::process::{Command, Stdio};
    if cmd.trim().is_empty() {
        return Err("formatter is empty".into());
    }
    let mut c = Command::new("sh");
    c.args(["-c", cmd])
        .env("JOTTER_LANGUAGE", lang)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(d) = dir {
        c.current_dir(d);
    }
    let mut child = c.spawn().map_err(|e| format!("sh: {e}"))?;
    // write in a thread so a formatter that streams output can't deadlock us
    let mut stdin = child.stdin.take().expect("piped");
    let input = source.to_string();
    let writer = std::thread::spawn(move || stdin.write_all(input.as_bytes()));
    let out = child.wait_with_output().map_err(|e| format!("sh: {e}"))?;
    let _ = writer.join();
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        let line = err
            .lines()
            .find(|l| !l.trim().is_empty())
            .unwrap_or("failed");
        return Err(line.trim().to_string());
    }
    let text = String::from_utf8(out.stdout).map_err(|_| "formatter printed invalid UTF-8")?;
    let text = text.trim_end_matches('\n');
    // a failed first stage of a pipeline still exits 0: never blank a cell
    if text.trim().is_empty() && !source.trim().is_empty() {
        return Err("formatter printed nothing".into());
    }
    Ok(text.to_string())
}

#[cfg(test)]
mod tests {
    use crate::app::tests::{key, one_cell_app, sources, stream};

    #[test]
    fn split_merge_clear_undo_redo_roundtrip() {
        let (mut app, mut r) = one_cell_app("ops");
        app.notebook.cells[0].source = "a = 1\nb = 2".into();
        app.notebook.cells[0].push_output(stream("out\n"));
        r.rebuild_all(&app.notebook);
        // split at the start of row 1 (editor open, cursor there)
        app.open_editor(false);
        app.editor.as_mut().unwrap().click(1, 0, false);
        app.split_cell(&mut r);
        assert_eq!(sources(&app), ["a = 1", "b = 2"]);
        assert_eq!(app.selected, 1);
        assert!(app.notebook.cells[1].outputs.as_ref().unwrap().is_empty());
        assert_ne!(
            app.notebook.cells[0].extra["id"],
            app.notebook.cells[1].extra["id"]
        );
        app.undo(&mut r);
        assert_eq!(sources(&app), ["a = 1\nb = 2"]);
        app.redo(&mut r);
        assert_eq!(sources(&app), ["a = 1", "b = 2"]);
        // merge back
        app.selected = 0;
        app.merge_cells(app.selected..app.selected + 2, &mut r);
        assert_eq!(sources(&app), ["a = 1\nb = 2"]);
        assert_eq!(r.blocks.len(), 1);
        // clear outputs, undo restores them, redo clears again
        app.clear_outputs(0..1, &mut r);
        assert!(app.notebook.cells[0].outputs.as_ref().unwrap().is_empty());
        app.undo(&mut r);
        assert_eq!(app.notebook.cells[0].outputs.as_ref().unwrap().len(), 1);
        app.redo(&mut r);
        assert!(app.notebook.cells[0].outputs.as_ref().unwrap().is_empty());
        // a new op clears redo
        app.undo(&mut r);
        app.on_key(key('a'), &mut r);
        app.redo(&mut r);
        assert_eq!(app.message.as_deref(), Some("nothing to redo"));
    }

    #[test]
    fn yy_fills_the_paste_register_and_moves_undo() {
        let (mut app, mut r) = one_cell_app("yy");
        app.on_key(key('y'), &mut r);
        app.on_key(key('y'), &mut r);
        app.on_key(key('p'), &mut r);
        assert_eq!(sources(&app), ["x", "x"]);
        assert_ne!(
            app.notebook.cells[0].extra["id"],
            app.notebook.cells[1].extra["id"]
        );
        app.on_key(key('K'), &mut r); // move the copy up
        assert_eq!(app.selected, 0);
        app.undo(&mut r);
        assert_eq!(app.selected, 1, "undoing a move follows the cell back");
    }

    #[test]
    fn undoing_a_source_edit_keeps_newer_outputs() {
        let (mut app, mut r) = one_cell_app("undo");
        app.update_cell_source("y".into(), &mut r);
        app.notebook.cells[0].push_output(stream("ran\n")); // re-run after the edit
        app.undo(&mut r);
        let cell = &app.notebook.cells[0];
        assert_eq!(cell.source, "x");
        assert_eq!(cell.outputs.as_ref().unwrap().len(), 1);
    }

    #[test]
    fn formatter_pipes_stdin_and_reports_failure() {
        assert_eq!(
            super::run_formatter("tr a b", "aa\n", None, "python").unwrap(),
            "bb"
        );
        assert!(super::run_formatter("false", "x", None, "python").is_err());
        assert_eq!(
            super::run_formatter("echo $JOTTER_LANGUAGE", "x", None, "python").unwrap(),
            "python"
        );
        assert!(super::run_formatter("no-such-formatter-x", "x", None, "python").is_err());
        assert_eq!(
            super::run_formatter("tr a b | tr b c", "a", None, "python").unwrap(),
            "c"
        );
        assert!(
            super::run_formatter("false | cat", "x", None, "python").is_err(),
            "blank output"
        );
    }
}
