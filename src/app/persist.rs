//! Saving, autosave sidecars, reload and the changed-on-disk check.

use super::*;

pub(super) fn mtime(path: &std::path::Path) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

/// Autosave location for `path`: `<state dir>/autosave/<absolute path with
/// '/' as '%'>` (vim undodir style), so sidecars never litter the notebook's
/// directory or a git repo. Falls back to `.name.autosave` beside the file.
pub(super) fn autosave_path_for(path: &std::path::Path) -> PathBuf {
    let abs = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    let name = abs.to_string_lossy().replace('%', "%%").replace('/', "%");
    // ponytail: filenames cap at 255 bytes; very deep paths keep their tail
    // (collisions need two notebooks sharing the last 200 bytes of path)
    let name = match name.char_indices().rev().nth(199) {
        Some((i, _)) => name[i..].to_string(),
        None => name,
    };
    match crate::config::state_dir() {
        Some(dir) => dir.join("autosave").join(name),
        None => {
            let file = path.file_name().map(|s| s.to_string_lossy()).unwrap_or_default();
            path.with_file_name(format!(".{file}.autosave"))
        }
    }
}

/// "3m", "2h", "5d": age of a file, for the recovery prompt.
fn age(t: SystemTime) -> String {
    let s = t.elapsed().map_or(0, |d| d.as_secs());
    match s {
        0..60 => format!("{s}s"),
        60..3600 => format!("{}m", s / 60),
        3600..86400 => format!("{}h", s / 3600),
        _ => format!("{}d", s / 86400),
    }
}

/// Whether the cells' type/source/outputs match (ids are ignored: a pre-4.5
/// file gets fresh random ids on every load).
pub(super) fn same_cells(a: &Notebook, b: &Notebook) -> bool {
    let key = |c| {
        let c: &crate::notebook::Cell = c;
        (&c.cell_type, &c.source, &c.outputs)
    };
    a.cells.iter().map(key).eq(b.cells.iter().map(key))
}

/// `~/x` -> `$HOME/x`; anything else as typed (relative to the cwd).
pub(super) fn expand_tilde(p: &str) -> PathBuf {
    match (p.strip_prefix("~/"), std::env::var_os("HOME")) {
        (Some(rest), Some(home)) => PathBuf::from(home).join(rest),
        _ => PathBuf::from(p),
    }
}

impl App {
    /// Startup: an autosave that differs from the file means a session died
    /// with unsaved work — ask. An identical one is just stale: remove it.
    pub(super) fn check_autosave(&mut self) {
        let side = self.autosave_path();
        let Some(when) = mtime(&side) else { return };
        match Notebook::read_ipynb(&side) {
            Ok(saved) if same_cells(&saved, &self.notebook) => {
                let _ = std::fs::remove_file(&side);
            }
            Ok(saved) => {
                let summary = format!("autosave from {} ago", age(when));
                self.confirm = Some(Confirm::Recover(Box::new(saved), summary));
            }
            Err(e) => self.message = Some(format!("unreadable autosave {}: {e:#}", side.display())),
        }
    }

    /// Where the autosave timer writes this notebook's recovery copy.
    pub fn autosave_path(&self) -> PathBuf {
        // ponytail: one scratchpad sidecar per working directory, so two
        // scratchpads open in the same directory share (and clobber) it
        if self.is_scratch() {
            autosave_path_for(std::path::Path::new("[scratch]"))
        } else {
            autosave_path_for(&self.path)
        }
    }

    pub fn is_scratch(&self) -> bool {
        self.path.as_os_str().is_empty()
    }

    /// SIGTERM/SIGHUP/SIGINT: flush unsaved work to the autosave so the
    /// next start offers it back; the caller then exits normally.
    pub fn on_signal(&mut self, name: &str) {
        log::warn!("{name}: autosaving and exiting");
        self.maybe_autosave();
        self.should_quit = true;
    }

    /// Called from the main loop's timer; writes the sidecar if anything changed.
    pub fn maybe_autosave(&mut self) {
        if !self.autosave_pending {
            return;
        }
        let side = self.autosave_path();
        if let Some(dir) = side.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        // always JSON: it keeps the outputs a `.py` can't, and its name ends
        // like the notebook's
        match self.notebook.write_ipynb(&side) {
            Ok(()) => self.autosave_pending = false,
            Err(e) => self.message = Some(format!("autosave failed: {e:#}")),
        }
    }

    pub(super) fn touch(&mut self) {
        self.dirty = true;
        self.autosave_pending = true;
    }

    /// Outputs or execution counts changed: worth an autosave (the sidecar
    /// keeps them for a crash), but only a change to the file when the
    /// format stores them — a `.py` never does, so nothing to save.
    pub(crate) fn touch_outputs(&mut self) {
        self.autosave_pending = true;
        if self.stores_outputs() {
            self.dirty = true;
        }
    }

    pub fn stores_outputs(&self) -> bool {
        !crate::notebook::is_text(&self.path)
    }

    /// External-modification check, run on focus-gain and the periodic tick.
    /// Like nvim autoread: a clean buffer reloads silently; a dirty buffer
    /// (or an open editor, which may hold uncommitted text) prompts.
    pub fn check_disk(&mut self, rendered: &mut Rendered) {
        let on_disk = mtime(&self.path);
        if on_disk == self.disk_mtime || self.confirm.is_some() {
            return;
        }
        if !self.dirty && self.editor.is_none() {
            match self.reload(rendered) {
                Ok(()) => self.message = Some("file changed on disk — reloaded".into()),
                Err(e) => {
                    self.message = Some(format!("file changed on disk, reload failed: {e:#}"));
                    self.disk_mtime = on_disk; // don't retry every tick
                }
            }
        } else {
            self.confirm = Some(Confirm::Reload);
        }
    }

    /// Replace the in-memory notebook with the on-disk state.
    pub(super) fn reload(&mut self, rendered: &mut Rendered) -> Result<()> {
        self.notebook = Notebook::open(&self.path)?;
        rendered.rebuild_all(&self.notebook);
        if let Some(ed) = self.editor.take()
            && let Some(s) = ed.into_session()
        {
            self.nvim = Some(s);
        }
        self.selected = self.selected.min(self.notebook.cells.len().saturating_sub(1));
        // in-flight executions point at cells that no longer exist as indexed
        self.forget_executions();
        self.undo_stack.clear();
        self.redo_stack.clear();
        self.dirty = false;
        self.autosave_pending = false;
        self.disk_mtime = mtime(&self.path);
        Ok(())
    }

    /// `:w` from inside a cell: commit its text, then save like `w` (`:w!`
    /// like `W`), with the on-disk-change refusal phrased in the editor's terms.
    pub(super) fn write_from_editor(
        &mut self,
        source: String,
        force: bool,
        rendered: &mut Rendered,
    ) -> bool {
        self.update_cell_source(source, rendered);
        if !force && self.changed_on_disk() {
            self.message = Some("file changed on disk since open/save — :w! to overwrite".into());
            return false;
        }
        self.save_notebook(force, rendered)
    }

    fn changed_on_disk(&self) -> bool {
        self.disk_mtime.is_some() && mtime(&self.path) != self.disk_mtime
    }

    /// Save to `path`; `force` skips the external-modification check.
    /// Returns true on success.
    pub(super) fn save_notebook(&mut self, force: bool, rendered: &mut Rendered) -> bool {
        if self.is_scratch() {
            self.message = Some("scratchpad has no file — :w name.ipynb (or name.py)".into());
            return false;
        }
        if !force && self.changed_on_disk() {
            self.message = Some("file changed on disk since open/save — W to overwrite".into());
            return false;
        }
        let formatted = crate::config::get()
            .format_on_save
            .then(|| self.format_cells(0..self.notebook.cells.len(), rendered));
        match self.notebook.save(&self.path) {
            Ok(()) => {
                self.dirty = false;
                self.autosave_pending = false;
                self.disk_mtime = mtime(&self.path);
                let _ = std::fs::remove_file(self.autosave_path());
                self.message = Some(match formatted {
                    Some(f) => format!("saved {} · {f}", self.path.display()),
                    None => format!("saved {}", self.path.display()),
                });
                true
            }
            Err(e) => {
                self.message = Some(format!("save failed: {e:#}"));
                false
            }
        }
    }

    /// Save under a new path and continue editing that file. Returns false
    /// while waiting for a second Enter to overwrite an existing file.
    pub(super) fn save_as(&mut self, typed: &str, rendered: &mut Rendered) -> bool {
        let target = expand_tilde(typed.trim());
        if target.as_os_str().is_empty() {
            return true;
        }
        if target.exists() && target != self.path && self.overwrite_armed.as_ref() != Some(&target)
        {
            self.message = Some(format!("{} exists — Enter again to overwrite", target.display()));
            self.overwrite_armed = Some(target);
            return false;
        }
        self.overwrite_armed = None;
        let old_side = self.autosave_path();
        let old = std::mem::replace(&mut self.path, target);
        if self.save_notebook(true, rendered) {
            if self.autosave_path() != old_side {
                let _ = std::fs::remove_file(old_side);
            }
            self.message = Some(format!(
                "saved as {} (kernel keeps its working directory)",
                self.path.display()
            ));
        } else {
            self.path = old;
        }
        true
    }
}
