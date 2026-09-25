use crate::editor::{Editor, Outcome};
use crate::kernel::{Event, Kernel};
use crate::nvim::{Backend, ModeKind, NvimCell, NvimSession};
use crate::notebook::{Cell, Notebook};
use crate::ui::Rendered;
use anyhow::Result;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use jupyter_protocol::messaging::JupyterMessage;
use ratatui::layout::Rect;
use serde_json::{Map, Value};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::{Instant, SystemTime};
use tokio::sync::mpsc;

/// What a rendered line belongs to — built each draw, used for mouse hits.
pub struct Hit {
    pub cell: usize,
    pub kind: HitKind,
}

#[derive(Clone, Copy)]
pub enum HitKind {
    /// Source line (row within the cell's source).
    Source(usize),
    /// Output row (wheel here scrolls the cell's output viewport).
    Output,
    /// Prompt or separator line.
    Other,
}

/// A pending `input()` request from the kernel, answered from the status line.
pub struct StdinReq {
    pub prompt: String,
    pub buf: String,
    pub password: bool,
    request: Box<JupyterMessage>,
}

/// What a wheel gesture is latched onto (browser-style scroll latching).
#[derive(Clone, Copy, PartialEq)]
enum WheelTarget {
    View,
    Output(usize),
}

/// Inverse operations for notebook-level cell edits (LIFO).
enum UndoOp {
    Inserted(usize),
    Deleted(usize, Box<Cell>),
    Moved(usize, usize),
    Replaced(usize, Box<Cell>),
}

/// Things the main loop must do on our behalf (they need the terminal).
pub enum Action {
    None,
    /// Suspend the TUI and open the selected cell in $EDITOR.
    ExternalEdit,
}

pub struct App {
    pub notebook: Notebook,
    pub path: PathBuf,
    pub selected: usize,
    /// First visible rendered line; adjusted in ui::draw to keep selection visible.
    pub scroll: usize,
    pub should_quit: bool,
    /// One-shot status message.
    pub message: Option<String>,
    pub kernel: Option<Kernel>,
    pub kernel_busy: bool,
    /// In-flight executions: msg_id -> cell index (remapped on cell ops).
    pub running: HashMap<String, usize>,
    pub dirty: bool,
    /// Some(_) while a cell is being edited.
    pub editor: Option<Backend>,
    /// Embedded nvim (editor = "nvim"), spawned lazily on first cell edit and
    /// parked here between edits so per-cell buffers keep their undo history.
    nvim: Option<NvimSession>,
    /// Line -> cell mapping of the last draw (mouse support).
    pub hit: Vec<Hit>,
    /// Notebook body area of the last draw.
    pub body: Rect,
    /// View was wheel-scrolled: draw must not snap back to the selection.
    pub manual_scroll: bool,
    /// Keybinding cheatsheet overlay is open.
    pub show_help: bool,
    /// Total rendered lines of the last draw (scroll bound).
    pub content_lines: usize,
    last_click: Option<(Instant, u16, u16)>,
    confirm_quit: bool,
    /// File changed on disk while we're dirty: next key answers load/keep.
    confirm_reload: bool,
    /// First key of a two-key chord (dd).
    pending: Option<char>,
    yanked: Option<Cell>,
    kernel_name: String,
    events_tx: mpsc::UnboundedSender<Event>,
    /// clear_output(wait=True) received: clear on that parent's next output.
    pending_clear: HashSet<String>,
    /// mtime of the file as we last saw it on disk (external-change check).
    disk_mtime: Option<SystemTime>,
    /// Changes since the last autosave/save (drives the sidecar write).
    autosave_pending: bool,
    /// Executions the kernel has actually started (vs still queued), with
    /// their start time for the gutter timing display.
    pub started: HashMap<String, Instant>,
    /// Search prompt buffer while typing (`/`), and the committed query.
    pub search_input: Option<String>,
    search: Option<String>,
    /// Kernel is waiting on `input()`.
    pub stdin_req: Option<StdinReq>,
    undo_stack: Vec<UndoOp>,
    /// Numeric count prefix (`5G`, `3j`).
    count: Option<usize>,
    /// Every status message shown this session (M opens the overlay).
    pub history: Vec<String>,
    pub show_history: bool,
    /// `z`: fullscreen image overlay (any key closes).
    pub zoom: Option<ratatui_image::sliced::SlicedProtocol>,
    /// Wheel-gesture latch: ticks in one burst keep their initial target even
    /// as content moves under the pointer.
    wheel_latch: Option<(Instant, WheelTarget)>,
}

fn mtime(path: &std::path::Path) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

fn new_code_cell() -> Cell {
    let mut extra = Map::new();
    extra.insert("id".into(), uuid::Uuid::new_v4().to_string()[..8].into());
    extra.insert("metadata".into(), Value::Object(Map::new()));
    extra.insert("execution_count".into(), Value::Null);
    Cell {
        cell_type: "code".into(),
        source: String::new(),
        outputs: Some(Vec::new()),
        extra,
    }
}

impl App {
    pub fn open(
        path: PathBuf,
        kernel_override: Option<String>,
        events_tx: mpsc::UnboundedSender<Event>,
    ) -> Result<Self> {
        let notebook = Notebook::open(&path)?;
        let kernel_name = kernel_override
            .or_else(|| {
                notebook
                    .extra
                    .get("metadata")?
                    .get("kernelspec")?
                    .get("name")?
                    .as_str()
                    .map(String::from)
            })
            .unwrap_or_else(|| "python3".into());
        let mut app = Self {
            notebook,
            path,
            selected: 0,
            scroll: 0,
            should_quit: false,
            message: None,
            kernel: None,
            kernel_busy: false,
            running: HashMap::new(),
            dirty: false,
            editor: None,
            nvim: None,
            hit: Vec::new(),
            body: Rect::default(),
            manual_scroll: false,
            show_help: false,
            content_lines: 0,
            last_click: None,
            confirm_quit: false,
            confirm_reload: false,
            pending: None,
            yanked: None,
            kernel_name,
            events_tx,
            pending_clear: HashSet::new(),
            disk_mtime: None,
            autosave_pending: false,
            started: HashMap::new(),
            search_input: None,
            search: None,
            stdin_req: None,
            undo_stack: Vec::new(),
            count: None,
            history: Vec::new(),
            show_history: false,
            zoom: None,
            wheel_latch: None,
        };
        app.disk_mtime = mtime(&app.path);
        if app.autosave_path().exists() {
            app.message = Some(format!(
                "autosave found (crashed session?): {} — kept until the next save",
                app.autosave_path().display()
            ));
        }
        Ok(app)
    }

    /// Sidecar written by the autosave timer, e.g. `.analysis.ipynb.autosave`.
    pub fn autosave_path(&self) -> PathBuf {
        let name = self
            .path
            .file_name()
            .map(|s| s.to_string_lossy())
            .unwrap_or_default();
        self.path.with_file_name(format!(".{name}.autosave"))
    }

    /// Called from the main loop's timer; writes the sidecar if anything changed.
    pub fn maybe_autosave(&mut self) {
        if !self.autosave_pending {
            return;
        }
        match self.notebook.save(&self.autosave_path()) {
            Ok(()) => self.autosave_pending = false,
            Err(e) => self.message = Some(format!("autosave failed: {e:#}")),
        }
    }

    fn touch(&mut self) {
        self.dirty = true;
        self.autosave_pending = true;
    }

    /// External-modification check, run on focus-gain and the periodic tick.
    /// Like nvim autoread: a clean buffer reloads silently; a dirty buffer
    /// (or an open editor, which may hold uncommitted text) prompts.
    pub fn check_disk(&mut self, rendered: &mut Rendered) {
        let on_disk = mtime(&self.path);
        if on_disk == self.disk_mtime || self.confirm_reload {
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
            self.confirm_reload = true;
            self.message = Some(
                "file changed on disk — l: load it (discard mine) · any other key: keep mine"
                    .into(),
            );
        }
    }

    /// Replace the in-memory notebook with the on-disk state.
    fn reload(&mut self, rendered: &mut Rendered) -> Result<()> {
        self.notebook = Notebook::open(&self.path)?;
        rendered.rebuild_all(&self.notebook);
        if let Some(ed) = self.editor.take()
            && let Some(s) = ed.into_session()
        {
            self.nvim = Some(s);
        }
        self.selected = self
            .selected
            .min(self.notebook.cells.len().saturating_sub(1));
        // in-flight executions point at cells that no longer exist as indexed
        self.running.clear();
        self.started.clear();
        self.pending_clear.clear();
        self.undo_stack.clear();
        self.dirty = false;
        self.autosave_pending = false;
        self.disk_mtime = mtime(&self.path);
        Ok(())
    }

    /// Save to `path`; `force` skips the external-modification check.
    /// Returns true on success.
    fn save_notebook(&mut self, force: bool) -> bool {
        if !force && self.disk_mtime.is_some() && mtime(&self.path) != self.disk_mtime {
            self.message = Some("file changed on disk since open/save — W to overwrite".into());
            return false;
        }
        match self.notebook.save(&self.path) {
            Ok(()) => {
                self.dirty = false;
                self.autosave_pending = false;
                self.disk_mtime = mtime(&self.path);
                let _ = std::fs::remove_file(self.autosave_path());
                self.message = Some(format!("saved {}", self.path.display()));
                true
            }
            Err(e) => {
                self.message = Some(format!("save failed: {e:#}"));
                false
            }
        }
    }

    pub fn spawn_kernel(&mut self) {
        self.message = Some(format!("starting kernel '{}'...", self.kernel_name));
        let dir = self
            .path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."));
        tokio::spawn(Kernel::launch(
            self.kernel_name.clone(),
            dir,
            self.events_tx.clone(),
        ));
    }

    pub fn on_mouse(&mut self, mouse: MouseEvent, rendered: &mut Rendered) {
        if self.zoom.is_some() {
            return; // overlay owns the screen; keys close it
        }
        match mouse.kind {
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                let delta: isize = if mouse.kind == MouseEventKind::ScrollUp {
                    -3
                } else {
                    3
                };
                // Scroll latching: ticks less than a pause apart form one
                // gesture, and the gesture keeps its initial target — a view
                // scroll is never captured by an output that merely slides
                // under the pointer. A fresh gesture re-decides from hover.
                const GESTURE_GAP: std::time::Duration = std::time::Duration::from_millis(400);
                let mut target = self
                    .wheel_latch
                    .filter(|(t, _)| t.elapsed() < GESTURE_GAP)
                    .map(|(_, tgt)| tgt)
                    .unwrap_or_else(|| {
                        match self
                            .body
                            .contains((mouse.column, mouse.row).into())
                            .then(|| self.hit.get((mouse.row - self.body.y) as usize))
                            .flatten()
                        {
                            Some(&Hit {
                                cell,
                                kind: HitKind::Output,
                            }) if rendered
                                .blocks
                                .get(cell)
                                .is_some_and(|b| b.output_scrollable()) =>
                            {
                                WheelTarget::Output(cell)
                            }
                            _ => WheelTarget::View,
                        }
                    });
                if let WheelTarget::Output(cell) = target {
                    match rendered.blocks.get_mut(cell) {
                        Some(b) if b.output_scrollable() => {
                            let before = b.win_start();
                            b.scroll_output(delta);
                            if b.win_start() == before {
                                target = WheelTarget::View; // edge: hand off
                            }
                        }
                        _ => target = WheelTarget::View,
                    }
                }
                if target == WheelTarget::View {
                    self.scroll = self
                        .scroll
                        .saturating_add_signed(delta)
                        .min(self.content_lines.saturating_sub(1));
                    self.manual_scroll = true;
                }
                self.wheel_latch = Some((Instant::now(), target));
            }
            MouseEventKind::Down(MouseButton::Left) => {
                self.click(mouse.column, mouse.row, rendered)
            }
            _ => {}
        }
    }

    fn click(&mut self, x: u16, y: u16, rendered: &mut Rendered) {
        let body = self.body;
        if y < body.y || y >= body.y + body.height || x < body.x {
            return;
        }
        // hit map is window-relative (only visible rows are materialized)
        let Some(&Hit { cell, kind }) = self.hit.get((y - body.y) as usize) else {
            return;
        };
        let double = self
            .last_click
            .take()
            .is_some_and(|(t, px, py)| t.elapsed().as_millis() < 400 && px == x && py == y);
        self.last_click = Some((Instant::now(), x, y));

        if self.editor.is_some() && cell != self.selected {
            self.commit_editor(rendered);
        }
        self.selected = cell;
        self.manual_scroll = true; // a click never scrolls the view
        match kind {
            HitKind::Source(row) => {
                // markdown renders rich when not edited: a single click only
                // selects; a double click opens the raw source at the line
                let is_md = self
                    .notebook
                    .cells
                    .get(cell)
                    .is_some_and(|c| c.cell_type == "markdown");
                if is_md && !double && self.editor.is_none() {
                    return;
                }
                let col = (x - body.x) as usize;
                match &mut self.editor {
                    Some(editor) => editor.click(row, col, double),
                    None => {
                        let Some(cell) = self.notebook.cells.get(self.selected) else {
                            return;
                        };
                        let source = cell.source.clone();
                        let mut editor = self.make_backend(&source);
                        editor.click(row, col, double);
                        self.editor = Some(editor);
                    }
                }
            }
            // double click on rendered markdown / prompt / output: edit raw
            HitKind::Other | HitKind::Output if double && self.editor.is_none() => {
                self.open_editor(false)
            }
            HitKind::Other | HitKind::Output => {}
        }
    }

    pub fn on_key(&mut self, key: KeyEvent, rendered: &mut Rendered) -> Action {
        self.message = None;
        self.manual_scroll = false; // keyboard nav re-follows the selection
        if self.show_help || self.show_history || self.zoom.is_some() {
            self.show_help = false;
            self.show_history = false;
            self.zoom = None;
            return Action::None;
        }
        if self.stdin_req.is_some() {
            self.on_key_stdin(key);
            return Action::None;
        }
        if self.search_input.is_some() {
            self.on_key_search(key);
            return Action::None;
        }
        if self.confirm_reload {
            self.confirm_reload = false;
            if key.code == KeyCode::Char('l') {
                match self.reload(rendered) {
                    Ok(()) => self.message = Some("reloaded from disk".into()),
                    Err(e) => self.message = Some(format!("reload failed: {e:#}")),
                }
            } else {
                // keep mine: accept the divergence; a plain w now overwrites
                self.disk_mtime = mtime(&self.path);
                self.message = Some("keeping this buffer — w will overwrite the file".into());
            }
            return Action::None;
        }
        if self.editor.is_some() {
            self.on_key_edit(key, rendered);
            return Action::None;
        }
        self.on_key_notebook(key, rendered)
    }

    /// Keys while the kernel waits on `input()`: typed into the status line.
    fn on_key_stdin(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.stdin_req = None;
                if let Some(k) = &self.kernel {
                    k.interrupt();
                    self.message = Some("interrupt sent".into());
                }
            }
            KeyCode::Enter | KeyCode::Esc => {
                if let Some(req) = self.stdin_req.take()
                    && let Some(k) = &self.kernel
                {
                    let value = if key.code == KeyCode::Enter {
                        req.buf
                    } else {
                        String::new()
                    };
                    k.reply_input(&req.request, value);
                }
            }
            KeyCode::Backspace => {
                if let Some(r) = &mut self.stdin_req {
                    r.buf.pop();
                }
            }
            KeyCode::Char(c) => {
                if let Some(r) = &mut self.stdin_req {
                    r.buf.push(c);
                }
            }
            _ => {}
        }
    }

    fn on_key_search(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => self.search_input = None,
            KeyCode::Enter => {
                let q = self.search_input.take().unwrap_or_default();
                if !q.is_empty() {
                    self.search = Some(q);
                    self.find(1);
                }
            }
            KeyCode::Backspace => {
                if let Some(b) = &mut self.search_input {
                    b.pop();
                }
            }
            KeyCode::Char(c) => {
                if let Some(b) = &mut self.search_input {
                    b.push(c);
                }
            }
            _ => {}
        }
    }

    /// Select the next cell (dir = ±1, wrapping) whose source contains the
    /// query, case-insensitively.
    fn find(&mut self, dir: isize) {
        let Some(q) = self.search.clone() else {
            self.message = Some("no search — / to start one".into());
            return;
        };
        let q = q.to_lowercase();
        let n = self.notebook.cells.len();
        for step in 1..=n.max(1) {
            let i = (self.selected as isize + dir * step as isize).rem_euclid(n.max(1) as isize)
                as usize;
            if self
                .notebook
                .cells
                .get(i)
                .is_some_and(|c| c.source.to_lowercase().contains(&q))
            {
                self.selected = i;
                return;
            }
        }
        self.message = Some(format!("no match: {q}"));
    }

    fn on_key_edit(&mut self, key: KeyEvent, rendered: &mut Rendered) {
        let editor = self.editor.as_mut().unwrap();
        // `q` is unbound in the builtin editor and is almost always quit-intent
        // from someone who forgot they're inside the cell: say so. (With the
        // nvim backend q is real macro recording and forwards.)
        if editor.is_builtin()
            && editor.mode() == ModeKind::Normal
            && key.code == KeyCode::Char('q')
            && key.modifiers.is_empty()
        {
            self.message =
                Some("editing cell — Esc exits the editor; then q quits, w saves".into());
            return;
        }
        let outcome = match editor.input(key) {
            Ok(outcome) => outcome,
            Err(e) => {
                // nvim died mid-edit: salvage the text into the builtin editor
                let source = editor.source();
                if editor.take_write_request() {
                    self.update_cell_source(source.clone(), rendered); // honor a final :wq
                }
                self.editor = Some(Backend::Builtin(Editor::new(&source)));
                self.message = Some(format!("nvim backend lost ({e:#}) — builtin editor"));
                return;
            }
        };
        // :w inside the cell (BufWriteCmd hook): commit but keep editing
        if let Some(editor) = &mut self.editor
            && editor.take_write_request()
        {
            let source = editor.source();
            self.update_cell_source(source, rendered);
        }
        // :q/:wq/ZZ/... inside nvim: leave the cell instead of exiting nvim
        if let Some(editor) = &mut self.editor
            && let Some(discard) = editor.take_quit_request()
        {
            if discard {
                // :q!/ZQ — no commit; reclaim the session, buffer resyncs on reopen
                if let Some(ed) = self.editor.take()
                    && let Some(s) = ed.into_session()
                {
                    self.nvim = Some(s);
                }
                self.message = Some("cell edit discarded".into());
            } else {
                self.commit_editor(rendered);
            }
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

    fn commit_editor(&mut self, rendered: &mut Rendered) {
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
    fn update_cell_source(&mut self, source: String, rendered: &mut Rendered) {
        let Some(cell) = self.notebook.cells.get_mut(self.selected) else {
            return;
        };
        if source != cell.source {
            let prior = cell.clone();
            cell.source = source;
            rendered.rebuild_cell(self.selected, cell);
            self.push_undo(UndoOp::Replaced(self.selected, Box::new(prior)));
            self.touch();
        }
    }

    fn on_key_notebook(&mut self, key: KeyEvent, rendered: &mut Rendered) -> Action {
        if self.confirm_quit {
            self.confirm_quit = false;
            match key.code {
                // discard; the autosave sidecar stays behind as a safety net
                KeyCode::Char('q') => self.should_quit = true,
                KeyCode::Char('s') | KeyCode::Char('w') => {
                    if self.save_notebook(false) {
                        self.should_quit = true;
                    }
                }
                _ => self.message = Some("quit cancelled".into()),
            }
            return Action::None;
        }
        let pending = self.pending.take();
        let count = self.count.take();
        let step = count.unwrap_or(1).max(1);
        let last = self.notebook.cells.len().saturating_sub(1);
        match (pending, key.code) {
            (Some('d'), KeyCode::Char('d')) => self.delete_cell(rendered),
            (Some('y'), KeyCode::Char('y')) => self.yank_to_clipboard(),
            (Some(_), _) => {}
            (None, KeyCode::Char(c)) if c.is_ascii_digit() => {
                self.count = Some(count.unwrap_or(0) * 10 + (c as usize - '0' as usize));
            }
            (None, code) => match code {
                KeyCode::Char('q') => {
                    if self.dirty {
                        self.confirm_quit = true;
                        self.message = Some(
                            "unsaved changes — s: save & quit · q: discard · else cancel".into(),
                        );
                    } else {
                        self.should_quit = true;
                    }
                }
                KeyCode::Char('w') => {
                    self.save_notebook(false);
                }
                KeyCode::Char('W') => {
                    self.save_notebook(true);
                }
                KeyCode::Char('j') | KeyCode::Down => {
                    self.selected = (self.selected + step).min(last)
                }
                KeyCode::Char('k') | KeyCode::Up => {
                    self.selected = self.selected.saturating_sub(step)
                }
                KeyCode::Char('g') => self.selected = 0,
                // `G` = last cell, `5G` = cell 5 (1-based, like vim)
                KeyCode::Char('G') => {
                    self.selected = count.map_or(last, |c| c.saturating_sub(1).min(last))
                }
                KeyCode::Char('d') => self.pending = Some('d'),
                KeyCode::Char('y') => self.pending = Some('y'),
                KeyCode::Char('/') => self.search_input = Some(String::new()),
                KeyCode::Char('n') => self.find(1),
                KeyCode::Char('N') => self.find(-1),
                KeyCode::Char('u') => self.undo(rendered),
                KeyCode::Char('o') => {
                    if let Some(b) = rendered.blocks.get_mut(self.selected) {
                        b.collapsed = !b.collapsed;
                    }
                }
                KeyCode::Char('[') => {
                    if let Some(b) = rendered.blocks.get_mut(self.selected) {
                        b.scroll_output(-1);
                    }
                }
                KeyCode::Char(']') => {
                    if let Some(b) = rendered.blocks.get_mut(self.selected) {
                        b.scroll_output(1);
                    }
                }
                // run all / all above / selected + below
                KeyCode::Char('r') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    self.run_range(0..self.notebook.cells.len(), rendered);
                }
                KeyCode::Char('<') => self.run_range(0..self.selected, rendered),
                KeyCode::Char('>') => {
                    self.run_range(self.selected..self.notebook.cells.len(), rendered)
                }
                // edit
                KeyCode::Enter if key.modifiers.is_empty() => self.open_editor(false),
                KeyCode::Char('i') => self.open_editor(true),
                KeyCode::Char('A') => {
                    self.open_editor(false);
                    if let Some(editor) = &mut self.editor {
                        editor.append_end();
                    }
                }
                KeyCode::Char('E') => return Action::ExternalEdit,
                // run (Shift+Enter / Ctrl+Enter need the kitty keyboard protocol)
                KeyCode::Enter | KeyCode::Char('r') => {
                    self.run_cell(self.selected, rendered);
                    if key.code == KeyCode::Char('r') || key.modifiers.contains(KeyModifiers::SHIFT)
                    {
                        self.selected = (self.selected + 1).min(last);
                    }
                }
                // cell ops
                KeyCode::Char('a') => self.insert_cell(self.selected + 1, rendered),
                KeyCode::Char('b') => self.insert_cell(self.selected, rendered),
                KeyCode::Char('p') => {
                    if let Some(mut cell) = self.yanked.clone() {
                        cell.extra
                            .insert("id".into(), uuid::Uuid::new_v4().to_string()[..8].into());
                        self.insert_cell_value(self.selected + 1, cell, rendered);
                    }
                }
                KeyCode::Char('J') => self.move_cell(1, rendered),
                KeyCode::Char('K') => self.move_cell(-1, rendered),
                KeyCode::Char('m') => self.toggle_type(rendered),
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    if let Some(k) = &self.kernel {
                        k.interrupt();
                        self.message = Some("interrupt sent".into());
                    }
                }
                KeyCode::Char('R') => {
                    self.kernel = None; // drop kills the old process
                    self.kernel_busy = false;
                    self.running.clear();
                    self.started.clear();
                    self.pending_clear.clear();
                    self.stdin_req = None;
                    self.spawn_kernel();
                }
                KeyCode::Char('?') => self.show_help = true,
                KeyCode::Char('M') => self.show_history = true,
                KeyCode::Char('z') => {
                    self.zoom = self
                        .notebook
                        .cells
                        .get(self.selected)
                        .and_then(|c| rendered.zoom_image(c, self.body));
                    self.message = Some(match self.zoom {
                        Some(_) => "zoom — any key closes".into(),
                        None => "no image output in this cell".into(),
                    });
                }
                KeyCode::Char('Z') => {
                    if let Some(cell) = self.notebook.cells.get(self.selected) {
                        let native = rendered.toggle_full_images(self.selected, cell);
                        self.message = Some(if native {
                            "images at native size — Z restores the cap".into()
                        } else {
                            "images back to capped size".into()
                        });
                    }
                }
                _ => {}
            },
        }
        Action::None
    }

    fn open_editor(&mut self, insert: bool) {
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
    fn make_backend(&mut self, source: &str) -> Backend {
        if crate::config::get().editor != "nvim" {
            return Backend::Builtin(Editor::new(source));
        }
        let key = self.cell_key();
        let existing = self.nvim.take();
        let had_existing = existing.is_some();
        let opened = existing
            .map(Ok)
            .unwrap_or_else(NvimSession::spawn)
            .and_then(|s| NvimCell::open(s, &key, source));
        let opened = match opened {
            // parked session died in the background: one respawn attempt
            Err(_) if had_existing => {
                NvimSession::spawn().and_then(|s| NvimCell::open(s, &key, source))
            }
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

    /// Per-cell nvim buffer key: the nbformat cell id. ponytail: cells without
    /// one (pre-4.5 files) get a positional key — wrong after moves, but that
    /// only costs undo continuity, never text.
    fn cell_key(&self) -> String {
        self.notebook
            .cells
            .get(self.selected)
            .and_then(|c| c.extra.get("id"))
            .and_then(|v| v.as_str())
            .map(|id| format!("id-{id}"))
            .unwrap_or_else(|| format!("pos-{}", self.selected))
    }

    /// Called by main after $EDITOR closed; applies the edited source.
    pub fn apply_external_edit(&mut self, source: String, rendered: &mut Rendered) {
        // editors write a trailing newline; cell sources don't carry one
        let source = source.strip_suffix('\n').unwrap_or(&source).to_string();
        self.update_cell_source(source, rendered);
    }

    fn push_undo(&mut self, op: UndoOp) {
        self.undo_stack.push(op);
        if self.undo_stack.len() > 100 {
            self.undo_stack.remove(0); // ponytail: O(n) at n=100, irrelevant
        }
    }

    /// Undo the last notebook-level cell operation (LIFO, so stored indices
    /// are valid by construction).
    fn undo(&mut self, rendered: &mut Rendered) {
        let Some(op) = self.undo_stack.pop() else {
            self.message = Some("nothing to undo".into());
            return;
        };
        match op {
            UndoOp::Inserted(at) => {
                self.notebook.cells.remove(at);
                rendered.remove_cell(at);
                self.remap_running(|i| match i.cmp(&at) {
                    std::cmp::Ordering::Less => Some(i),
                    std::cmp::Ordering::Equal => None,
                    std::cmp::Ordering::Greater => Some(i - 1),
                });
                self.selected = at.min(self.notebook.cells.len().saturating_sub(1));
            }
            UndoOp::Deleted(at, cell) => {
                rendered.insert_cell(at, &cell);
                self.notebook.cells.insert(at, *cell);
                self.remap_running(|i| Some(if i >= at { i + 1 } else { i }));
                self.selected = at;
            }
            UndoOp::Moved(a, b) => {
                self.notebook.cells.swap(a, b);
                rendered.swap_cells(a, b);
                self.remap_running(|i| {
                    Some(if i == a {
                        b
                    } else if i == b {
                        a
                    } else {
                        i
                    })
                });
                self.selected = a;
            }
            UndoOp::Replaced(at, cell) => {
                rendered.rebuild_cell(at, &cell);
                self.notebook.cells[at] = *cell;
                self.selected = at;
            }
        }
        self.touch();
        self.message = Some("undone".into());
    }

    fn run_range(&mut self, range: std::ops::Range<usize>, rendered: &mut Rendered) {
        for i in range {
            self.run_cell(i, rendered);
        }
    }

    fn insert_cell(&mut self, at: usize, rendered: &mut Rendered) {
        self.insert_cell_value(at, new_code_cell(), rendered);
    }

    fn insert_cell_value(&mut self, at: usize, cell: Cell, rendered: &mut Rendered) {
        let at = at.min(self.notebook.cells.len());
        rendered.insert_cell(at, &cell);
        self.notebook.cells.insert(at, cell);
        self.remap_running(|i| Some(if i >= at { i + 1 } else { i }));
        self.selected = at;
        self.push_undo(UndoOp::Inserted(at));
        self.touch();
    }

    fn delete_cell(&mut self, rendered: &mut Rendered) {
        if self.notebook.cells.is_empty() {
            return;
        }
        let at = self.selected;
        let cell = self.notebook.cells.remove(at);
        self.yanked = Some(cell.clone());
        self.push_undo(UndoOp::Deleted(at, Box::new(cell)));
        rendered.remove_cell(at);
        self.remap_running(|i| match i.cmp(&at) {
            std::cmp::Ordering::Less => Some(i),
            std::cmp::Ordering::Equal => None,
            std::cmp::Ordering::Greater => Some(i - 1),
        });
        self.selected = at.min(self.notebook.cells.len().saturating_sub(1));
        self.touch();
        self.message = Some("cell deleted (p pastes it back)".into());
    }

    fn move_cell(&mut self, delta: isize, rendered: &mut Rendered) {
        let a = self.selected;
        let b = a as isize + delta;
        if b < 0 || b as usize >= self.notebook.cells.len() {
            return;
        }
        let b = b as usize;
        self.notebook.cells.swap(a, b);
        rendered.swap_cells(a, b);
        self.push_undo(UndoOp::Moved(a, b));
        self.remap_running(|i| {
            Some(if i == a {
                b
            } else if i == b {
                a
            } else {
                i
            })
        });
        self.selected = b;
        self.touch();
    }

    /// Cycle cell type: code -> markdown -> raw (rendered as latex) -> code.
    fn toggle_type(&mut self, rendered: &mut Rendered) {
        let prior = match self.notebook.cells.get(self.selected) {
            Some(c) => c.clone(),
            None => return,
        };
        self.push_undo(UndoOp::Replaced(self.selected, Box::new(prior)));
        let Some(cell) = self.notebook.cells.get_mut(self.selected) else {
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
                metadata(cell).insert("format".into(), "text/latex".into());
                self.message = Some("raw latex cell".into());
            }
            _ => {
                cell.cell_type = "code".into();
                metadata(cell).remove("format");
                cell.outputs = Some(Vec::new());
                cell.extra.insert("execution_count".into(), Value::Null);
            }
        }
        rendered.rebuild_cell(self.selected, cell);
        self.touch();
    }

    /// Copy the selected cell's source to the system clipboard via OSC 52.
    fn yank_to_clipboard(&mut self) {
        use base64::Engine;
        use std::io::Write;
        let Some(cell) = self.notebook.cells.get(self.selected) else {
            return;
        };
        let b64 = base64::engine::general_purpose::STANDARD.encode(cell.source.as_bytes());
        let mut out = std::io::stdout();
        let _ = write!(out, "\x1b]52;c;{b64}\x07");
        let _ = out.flush();
        self.message = Some("cell source copied to clipboard".into());
    }

    fn remap_running(&mut self, f: impl Fn(usize) -> Option<usize>) {
        self.running = self
            .running
            .drain()
            .filter_map(|(k, v)| f(v).map(|v| (k, v)))
            .collect();
    }

    fn run_cell(&mut self, idx: usize, rendered: &mut Rendered) {
        let Some(cell) = self.notebook.cells.get_mut(idx) else {
            return;
        };
        if cell.cell_type != "code" {
            return;
        }
        let Some(kernel) = &self.kernel else {
            self.message = Some("no kernel — R to (re)start one".into());
            return;
        };
        cell.outputs = Some(Vec::new());
        let msg_id = kernel.execute(cell.source.clone());
        rendered.rebuild_cell(idx, cell);
        self.running.insert(msg_id, idx);
        self.touch();
    }

    pub fn apply_kernel_event(&mut self, event: Event, rendered: &mut Rendered) {
        match event {
            Event::Ready(kernel) => {
                let origin = kernel.spec_dir.display();
                self.message = Some(if kernel.name == self.kernel_name {
                    format!("kernel '{}' ready ({origin})", kernel.name)
                } else {
                    format!("'{}' not found — using {origin}", self.kernel_name)
                });
                self.kernel = Some(*kernel);
            }
            Event::Output { parent, output } => {
                if let Some(&idx) = self.running.get(&parent) {
                    let cell = &mut self.notebook.cells[idx];
                    if self.pending_clear.remove(&parent) {
                        cell.outputs = Some(Vec::new());
                    }
                    cell.push_output(output);
                    rendered.rebuild_cell(idx, cell);
                    self.touch();
                }
            }
            Event::Clear { parent, wait } => {
                if wait {
                    self.pending_clear.insert(parent);
                } else if let Some(&idx) = self.running.get(&parent) {
                    let cell = &mut self.notebook.cells[idx];
                    cell.outputs = Some(Vec::new());
                    rendered.rebuild_cell(idx, cell);
                    self.touch();
                }
            }
            Event::ExecutionCount { parent, count } => {
                if let Some(&idx) = self.running.get(&parent) {
                    self.notebook.cells[idx]
                        .extra
                        .insert("execution_count".into(), count.into());
                    self.touch();
                    self.started.insert(parent, Instant::now()); // no longer queued
                }
            }
            Event::Input {
                request,
                prompt,
                password,
            } => {
                self.stdin_req = Some(StdinReq {
                    prompt,
                    buf: String::new(),
                    password,
                    request,
                });
            }
            Event::Busy(b) => self.kernel_busy = b,
            Event::Done { parent } => {
                // gutter timing: wall time from execute_input to the reply
                if let (Some(&idx), Some(t)) =
                    (self.running.get(&parent), self.started.get(&parent))
                    && let Some(b) = rendered.blocks.get_mut(idx)
                {
                    b.elapsed = Some(t.elapsed());
                }
                self.running.remove(&parent);
                self.started.remove(&parent);
                self.pending_clear.remove(&parent);
            }
            Event::Dead(reason) => {
                self.kernel = None;
                self.kernel_busy = false;
                self.running.clear();
                self.started.clear();
                self.stdin_req = None;
                self.message = Some(format!("{reason} — R to restart"));
            }
            Event::Info(s) => self.message = Some(s),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wheel_gesture_latches_to_its_initial_target() {
        let path = std::env::temp_dir().join(format!("jotter-wheel-{}.ipynb", std::process::id()));
        let text: String = (0..40)
            .map(|i| format!("l{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let nb = serde_json::json!({
            "cells": [{"cell_type": "code", "metadata": {}, "execution_count": null,
                       "source": ["x"],
                       "outputs": [{"output_type": "stream", "name": "stdout", "text": text}]}],
            "metadata": {}, "nbformat": 4, "nbformat_minor": 5
        });
        std::fs::write(&path, nb.to_string()).unwrap();
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut app = App::open(path.clone(), None, tx).unwrap();
        std::fs::remove_file(&path).ok();
        let mut rendered = Rendered::build(&app.notebook, None, Default::default());
        // fake a draw: row 0 is the prompt, rows 1.. are the output viewport
        app.body = Rect::new(0, 0, 80, 30);
        app.content_lines = 100;
        app.hit = (0..30)
            .map(|r| Hit {
                cell: 0,
                kind: if r == 0 {
                    HitKind::Other
                } else {
                    HitKind::Output
                },
            })
            .collect();
        assert!(rendered.blocks[0].output_scrollable());
        let wheel = |row: u16, up: bool| MouseEvent {
            kind: if up {
                MouseEventKind::ScrollUp
            } else {
                MouseEventKind::ScrollDown
            },
            column: 5,
            row,
            modifiers: KeyModifiers::NONE,
        };

        // gesture starts over the prompt -> view; output sliding under the
        // pointer mid-gesture must NOT capture it
        app.on_mouse(wheel(0, false), &mut rendered);
        let after_first = app.scroll;
        assert!(after_first > 0, "view scrolled");
        let out_pos = rendered.blocks[0].win_start();
        app.on_mouse(wheel(5, false), &mut rendered); // now over the output
        assert!(app.scroll > after_first, "gesture stays with the view");
        assert_eq!(rendered.blocks[0].win_start(), out_pos, "output untouched");

        // after a pause, a gesture starting over the output scrolls the output
        app.wheel_latch = None; // simulate the pause
        let view_pos = app.scroll;
        app.on_mouse(wheel(5, true), &mut rendered);
        assert!(rendered.blocks[0].win_start() < out_pos, "output scrolled");
        assert_eq!(app.scroll, view_pos, "view untouched");
    }

    #[test]
    fn external_change_reloads_when_clean_and_prompts_when_dirty() {
        let path = std::env::temp_dir().join(format!("jotter-reload-{}.ipynb", std::process::id()));
        let nb = |src: &str| {
            serde_json::json!({
                "cells": [{"cell_type": "code", "metadata": {}, "execution_count": null,
                           "outputs": [], "source": [src]}],
                "metadata": {}, "nbformat": 4, "nbformat_minor": 5
            })
            .to_string()
        };
        std::fs::write(&path, nb("a = 1")).unwrap();
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut app = App::open(path.clone(), None, tx).unwrap();
        let mut rendered = Rendered::build(&app.notebook, None, Default::default());

        // clean buffer: silent reload (nvim autoread)
        std::thread::sleep(std::time::Duration::from_millis(20)); // distinct mtime
        std::fs::write(&path, nb("a = 2")).unwrap();
        app.check_disk(&mut rendered);
        assert_eq!(app.notebook.cells[0].source, "a = 2");
        assert!(!app.confirm_reload);

        // dirty buffer: prompt instead of clobbering local state
        app.notebook.cells[0].source = "local".into();
        app.touch();
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(&path, nb("a = 3")).unwrap();
        app.check_disk(&mut rendered);
        assert!(app.confirm_reload);
        assert_eq!(app.notebook.cells[0].source, "local");

        // "keep mine" (any key but l) accepts divergence: no re-prompt, w saves
        let key = KeyEvent::new(KeyCode::Char('k'), KeyModifiers::NONE);
        app.on_key(key, &mut rendered);
        assert!(!app.confirm_reload);
        app.check_disk(&mut rendered);
        assert!(!app.confirm_reload, "mtime accepted, no re-prompt");
        assert!(
            app.save_notebook(false),
            "plain w overwrites after keep-mine"
        );
        std::fs::remove_file(&path).ok();
    }
}
