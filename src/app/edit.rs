//! The cell editing session: opening, keys while editing, committing, the nvim backend.

use super::*;

impl App {
    /// Close the cell editor: commit its text, or (`discard`) drop the edit.
    pub(super) fn leave_editor(&mut self, discard: bool, rendered: &mut Rendered) {
        if !discard {
            self.commit_editor(rendered);
            return;
        }
        // no commit; reclaim the nvim session, its buffer resyncs on reopen
        if let Some(ed) = self.editor.take()
            && let Some(s) = ed.into_session()
        {
            self.nvim = Some(s);
        }
        self.message = Some("cell edit discarded".into());
    }

    pub(super) fn on_key_edit(&mut self, key: KeyEvent, rendered: &mut Rendered) {
        if self.on_key_completion(key) {
            return;
        }
        // Ctrl+k splits the cell at the cursor.
        // DEPRECATED since 0.7.0, remove in 0.8.0: Ctrl+\, the old key, and Ctrl+4,
        // which is how terminals without the kitty keyboard protocol report it.
        if key.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(key.code, KeyCode::Char('k' | '\\' | '4'))
        {
            self.split_cell(rendered);
            if key.code != KeyCode::Char('k') {
                self.message = Some("Ctrl+\\ goes away in 0.8.0 — Ctrl+k splits the cell".into());
            }
            return;
        }
        let editor = self.editor.as_mut().unwrap();
        // `q` is unbound in the builtin editor and is almost always quit-intent
        // from someone who forgot they're inside the cell: say so. (With the
        // nvim backend q is real macro recording and forwards.)
        if editor.is_builtin()
            && editor.mode() == ModeKind::Normal
            && key.code == KeyCode::Char('q')
            && key.modifiers.is_empty()
        {
            self.message = Some("editing cell — :q or Esc leaves it, :w saves".into());
            return;
        }
        // the builtin editor has no command line of its own: open jOtter's
        if editor.is_builtin()
            && editor.mode() == ModeKind::Normal
            && key.code == KeyCode::Char(':')
        {
            self.prompt = Some(Prompt { kind: PromptKind::Command, buf: String::new() });
            return;
        }
        let outcome = match editor.input(key) {
            Ok(outcome) => outcome,
            Err(e) => {
                self.nvim_failed(e, rendered);
                return;
            }
        };
        if let Some(notice) = self.editor.as_mut().and_then(|e| e.take_notice()) {
            self.message = Some(notice);
        }
        // :w inside the cell (BufWriteCmd hook), or the write half of
        // :wq/:x/ZZ: commit, save the notebook, keep editing
        if let Some(editor) = &mut self.editor
            && let Some(force) = editor.take_write_request()
        {
            let source = editor.source();
            self.write_from_editor(source, force, rendered);
        }
        // :q/:wq/ZZ/... inside nvim: leave the cell instead of exiting nvim
        if let Some(editor) = &mut self.editor
            && let Some(discard) = editor.take_quit_request()
        {
            self.leave_editor(discard, rendered); // :q!/ZQ discard
            return;
        }
        match outcome {
            Outcome::Continue => {}
            Outcome::Exit => self.commit_editor(rendered),
            Outcome::Run { advance } => {
                self.commit_editor(rendered);
                self.run_cell(self.selected, rendered);
                if advance {
                    self.selected =
                        (self.selected + 1).min(self.notebook.cells.len().saturating_sub(1));
                }
            }
        }
    }

    /// An nvim request failed. A late reply keeps the session: the key is
    /// queued in nvim and the next key reads the state back. Anything else
    /// means the child is gone: salvage the text into the builtin editor.
    pub(super) fn nvim_failed(&mut self, e: anyhow::Error, rendered: &mut Rendered) {
        if crate::nvim::is_busy(&e) {
            self.message =
                Some("nvim is busy — keys are queued, the cell refreshes with the next one".into());
            return;
        }
        let Some(editor) = &mut self.editor else {
            return;
        };
        let source = editor.source();
        if let Some(force) = editor.take_write_request() {
            self.write_from_editor(source.clone(), force, rendered); // honor a final :wq
        }
        self.editor = Some(Backend::Builtin(Editor::new(&source)));
        self.message = Some(format!("nvim backend lost ({e:#}) — builtin editor"));
    }

    pub(super) fn commit_editor(&mut self, rendered: &mut Rendered) {
        self.signature = None;
        let Some(editor) = self.editor.take() else {
            return;
        };
        let source = editor.source();
        if let Some(session) = editor.into_session() {
            self.nvim = Some(session); // per-cell buffers survive across edits
        }
        self.update_cell_source(source, rendered);
    }

    /// Write `source` into the selected cell (undo-tracked, re-rendered);
    /// editor state untouched.
    pub(super) fn update_cell_source(&mut self, source: String, rendered: &mut Rendered) {
        let Some(cell) = self.notebook.cells.get_mut(self.selected) else {
            return;
        };
        if source != cell.source {
            self.record(CellOp::SetSource(self.selected, source), rendered);
        }
    }

    #[cfg(test)]
    pub fn open_editor_for_test(&mut self) {
        self.open_editor(false);
        if let Some(e) = &mut self.editor {
            e.append_end();
        }
    }

    pub(super) fn open_editor(&mut self, insert: bool) {
        let Some(cell) = self.notebook.cells.get(self.selected) else {
            return;
        };
        let source = cell.source.clone();
        let mut editor = self.make_backend(&source);
        if insert {
            let _ = editor.input(KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE));
        }
        self.editor = Some(editor);
    }

    /// Editor backend per config; any nvim failure falls back to the builtin
    /// with a status message, so editing always works.
    pub(super) fn make_backend(&mut self, source: &str) -> Backend {
        if crate::config::get().editor != "nvim" {
            return Backend::Builtin(Editor::new(source));
        }
        let key = self.cell_key();
        let filetype = self.cell_filetype();
        let user_config = crate::config::get().nvim_user_config;
        let existing = self.nvim.take();
        let had_existing = existing.is_some();
        // first edit: take the background spawn (waits if it's still loading)
        let started = self.nvim_spawn.take().map(|h| {
            h.join().unwrap_or_else(|_| Err(anyhow::anyhow!("nvim spawn thread panicked")))
        });
        let opened = existing
            .map(Ok)
            .or(started)
            .unwrap_or_else(|| NvimSession::spawn(user_config))
            .and_then(|s| NvimCell::open(s, &key, source, &filetype));
        let opened = match opened {
            // parked session died in the background: one respawn attempt
            Err(_) if had_existing => NvimSession::spawn(user_config)
                .and_then(|s| NvimCell::open(s, &key, source, &filetype)),
            other => other,
        };
        match opened {
            Ok(cell) => Backend::Nvim(cell),
            Err(e) => {
                self.message = Some(format!("nvim unavailable ({e:#}) — builtin editor"));
                Backend::Builtin(Editor::new(source))
            }
        }
    }

    /// nvim `filetype` for the selected cell: the kernel language for code,
    /// `markdown`, `tex` for latex raw cells.
    fn cell_filetype(&self) -> String {
        match self.notebook.cells.get(self.selected) {
            Some(c) if c.cell_type == "code" => crate::ui::notebook_language(&self.notebook),
            Some(c) if c.cell_type == "markdown" => "markdown".into(),
            Some(c) if crate::ui::is_latex_raw(c) => "tex".into(),
            _ => String::new(),
        }
    }

    /// Per-cell nvim buffer key: the nbformat cell id (every cell has one
    /// since `Notebook::open` upgrades to 4.5, and new cells get one).
    fn cell_key(&self) -> String {
        let id = self
            .notebook
            .cells
            .get(self.selected)
            .and_then(|c| c.extra.get("id"))
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        format!("id-{id}")
    }

    /// Called by main after $EDITOR closed; applies the edited source.
    pub fn apply_external_edit(&mut self, source: String, rendered: &mut Rendered) {
        // editors write a trailing newline; cell sources don't carry one
        let source = source.strip_suffix('\n').unwrap_or(&source).to_string();
        self.update_cell_source(source, rendered);
    }
}
