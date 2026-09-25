use crate::editor::{Editor, Outcome};
use crate::kernel::{Event, Kernel};
use crate::nvim::{Backend, ModeKind, NvimCell, NvimSession};
use crate::notebook::{Cell, Notebook, new_cell_id};
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

mod cellops;
mod completion;

use cellops::CellOp;
pub use completion::Completion;

/// What a rendered line belongs to — built each draw, used for mouse hits.
pub struct Hit {
    pub cell: usize,
    pub kind: HitKind,
}

#[derive(Clone, Copy)]
pub enum HitKind {
    /// Source text: logical source line, and the char column the screen row
    /// starts at (soft-wrapped lines span several rows).
    Source { line: usize, col: usize },
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

/// A yes/no-style question on the status line; the next key answers it.
#[derive(Debug, PartialEq)]
pub enum Confirm {
    Quit,
    Reload,
    Restart,
    /// An autosave differs from the file (crashed session?): restore it?
    Recover(Box<Notebook>, String),
}

impl Confirm {
    pub fn question(&self) -> String {
        match self {
            Confirm::Quit => "unsaved changes — s: save & quit · q: discard · else cancel".into(),
            Confirm::Reload => {
                "file changed on disk — l: load it (discard mine) · any other key: keep mine".into()
            }
            Confirm::Restart => {
                "restart kernel? R: restart · a: restart & run all · else cancel".into()
            }
            Confirm::Recover(_, summary) => {
                format!("{summary} — r: restore it · d: delete it · else keep for now")
            }
        }
    }
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum PromptKind {
    Search,
    SaveAs,
}

/// Scrollable read-only text overlay (Shift+Tab docs).
pub struct Pager {
    pub title: String,
    pub text: String,
    /// First visible line; clamped by the draw.
    pub top: usize,
}

/// (row, col) in chars -> char offset into `source` (rows split on '\n').
pub fn char_offset(source: &str, (row, col): (usize, usize)) -> usize {
    let mut off = 0;
    for (i, line) in source.split('\n').enumerate() {
        let len = line.chars().count();
        if i == row {
            return off + col.min(len);
        }
        off += len + 1;
    }
    source.chars().count()
}

/// One-line text input on the status line (`/` search, `S` save as).
pub struct Prompt {
    pub kind: PromptKind,
    pub buf: String,
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
    /// Pending question; the next key answers it (shown on the status line).
    pub confirm: Option<Confirm>,
    /// First key of a two-key chord (dd).
    pending: Option<char>,
    yanked: Option<Cell>,
    kernel_name: String,
    events_tx: mpsc::UnboundedSender<Event>,
    /// Current kernel launch (Ready/LaunchFailed of older launches are stale).
    launch: u64,
    kernel_starting: bool,
    /// Cells (by id) run while the kernel was starting; run on Ready.
    deferred: Vec<String>,
    /// Multi-cell run in progress: the selection jumps to each cell as it
    /// starts, until the user moves the selection or scrolls.
    follow: bool,
    /// clear_output(wait=True) received: clear on that parent's next output.
    pending_clear: HashSet<String>,
    /// Executions that got one of their two end signals (shell execute_reply,
    /// iopub idle); the second one finishes them. iopub and shell are separate
    /// sockets, so outputs may still arrive after the reply — only the idle
    /// status (ordered on iopub after all outputs) proves the stream is done.
    half_done: HashSet<String>,
    /// mtime of the file as we last saw it on disk (external-change check).
    disk_mtime: Option<SystemTime>,
    /// Changes since the last autosave/save (drives the sidecar write).
    autosave_pending: bool,
    /// Executions the kernel has actually started (vs still queued), with
    /// their start time for the gutter timing display.
    pub started: HashMap<String, Instant>,
    /// Status-line text input, and the committed search query.
    pub prompt: Option<Prompt>,
    search: Option<String>,
    /// Save-as target that exists: a second Enter on it overwrites.
    overwrite_armed: Option<PathBuf>,
    /// Kernel is waiting on `input()`.
    pub stdin_req: Option<StdinReq>,
    pub completion: Option<Completion>,
    /// inspect_request msg_id awaiting its reply.
    pending_inspect: Option<String>,
    pub pager: Option<Pager>,
    undo_stack: Vec<CellOp>,
    redo_stack: Vec<CellOp>,
    /// Numeric count prefix (`5G`, `3j`).
    count: Option<usize>,
    /// Last status message sent to the log (ui::draw logs each one once).
    pub logged_status: Option<String>,
    /// `L` log viewer: Some(rows scrolled up from the newest entry).
    pub logs: Option<usize>,
    /// `D` debug overlay text (also copied to the clipboard).
    pub debug: Option<String>,
    /// `z`: fullscreen image overlay (any key closes).
    pub zoom: Option<ratatui_image::sliced::SlicedProtocol>,
    /// Wheel-gesture latch: ticks in one burst keep their initial target even
    /// as content moves under the pointer.
    wheel_latch: Option<(Instant, WheelTarget)>,
}

fn mtime(path: &std::path::Path) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

/// Copy `text` to the system clipboard via OSC 52 (works over ssh/tmux).
fn osc52(text: &str) {
    if cfg!(test) {
        return; // keep escape sequences out of test output
    }
    use base64::Engine;
    use std::io::Write;
    let b64 = base64::engine::general_purpose::STANDARD.encode(text.as_bytes());
    let mut out = std::io::stdout();
    let _ = write!(out, "\x1b]52;c;{b64}\x07");
    let _ = out.flush();
}

/// Autosave location for `path`: `<state dir>/autosave/<absolute path with
/// '/' as '%'>` (vim undodir style), so sidecars never litter the notebook's
/// directory or a git repo. Falls back to `.name.autosave` beside the file.
fn autosave_path_for(path: &std::path::Path) -> PathBuf {
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

/// Cells whose type/source/outputs differ (ids are ignored: a pre-4.5 file
/// gets fresh random ids on every load).
fn differing_cells(a: &Notebook, b: &Notebook) -> usize {
    let n = a.cells.len().max(b.cells.len());
    (0..n)
        .filter(|&i| match (a.cells.get(i), b.cells.get(i)) {
            (Some(x), Some(y)) => {
                (&x.cell_type, &x.source, &x.outputs) != (&y.cell_type, &y.source, &y.outputs)
            }
            _ => true,
        })
        .count()
}

/// `~/x` -> `$HOME/x`; anything else as typed (relative to the cwd).
fn expand_tilde(p: &str) -> PathBuf {
    match (p.strip_prefix("~/"), std::env::var_os("HOME")) {
        (Some(rest), Some(home)) => PathBuf::from(home).join(rest),
        _ => PathBuf::from(p),
    }
}

impl App {
    pub fn open(
        path: PathBuf,
        kernel_override: Option<String>,
        events_tx: mpsc::UnboundedSender<Event>,
    ) -> Result<Self> {
        let is_new = !path.exists();
        let notebook = if is_new {
            Notebook::new_empty()
        } else {
            Notebook::open(&path)?
        };
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
            confirm: None,
            pending: None,
            yanked: None,
            kernel_name,
            events_tx,
            launch: 0,
            kernel_starting: false,
            deferred: Vec::new(),
            follow: false,
            pending_clear: HashSet::new(),
            half_done: HashSet::new(),
            disk_mtime: None,
            autosave_pending: false,
            started: HashMap::new(),
            prompt: None,
            search: None,
            overwrite_armed: None,
            stdin_req: None,
            completion: None,
            pending_inspect: None,
            pager: None,
            undo_stack: Vec::new(),
            redo_stack: Vec::new(),
            count: None,
            logged_status: None,
            logs: None,
            debug: None,
            zoom: None,
            wheel_latch: None,
        };
        app.disk_mtime = mtime(&app.path);
        if is_new {
            app.message = Some(format!("new notebook — w creates {}", app.path.display()));
        }
        app.check_autosave();
        Ok(app)
    }

    /// Startup: an autosave that differs from the file means a session died
    /// with unsaved work — ask. An identical one is just stale: remove it.
    fn check_autosave(&mut self) {
        let side = self.autosave_path();
        let Some(when) = mtime(&side) else { return };
        match Notebook::open(&side) {
            Ok(saved) => match differing_cells(&saved, &self.notebook) {
                0 => {
                    let _ = std::fs::remove_file(&side);
                }
                n => {
                    let summary = format!(
                        "unsaved autosave from {} ago: {n} cell(s) differ ({} vs {} cells)",
                        age(when),
                        saved.cells.len(),
                        self.notebook.cells.len()
                    );
                    self.confirm = Some(Confirm::Recover(Box::new(saved), summary));
                }
            },
            Err(e) => self.message = Some(format!("unreadable autosave {}: {e:#}", side.display())),
        }
    }

    /// Where the autosave timer writes this notebook's recovery copy.
    pub fn autosave_path(&self) -> PathBuf {
        autosave_path_for(&self.path)
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
        match self.notebook.save(&side) {
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
        self.forget_executions();
        self.undo_stack.clear();
        self.redo_stack.clear();
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
        self.launch += 1;
        self.kernel_starting = true;
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
            self.launch,
        ));
    }

    /// Kill the kernel and start a fresh one; `run_all` queues every code
    /// cell to run once it is ready.
    fn restart_kernel(&mut self, run_all: bool) {
        self.kernel = None; // drop kills the old process
        self.kernel_busy = false;
        self.forget_executions();
        self.stdin_req = None;
        self.deferred.clear();
        self.spawn_kernel();
        if run_all {
            self.deferred = self
                .notebook
                .cells
                .iter()
                .filter(|c| c.cell_type == "code")
                .filter_map(|c| c.extra.get("id")?.as_str().map(String::from))
                .collect();
            self.follow = true;
            self.message = Some("restarting kernel — then run all".into());
        }
    }

    pub fn on_mouse(&mut self, mouse: MouseEvent, rendered: &mut Rendered) {
        if let Some(p) = &mut self.pager {
            match mouse.kind {
                MouseEventKind::ScrollUp => p.top = p.top.saturating_sub(3),
                MouseEventKind::ScrollDown => p.top += 3,
                _ => {}
            }
            return;
        }
        if let Some(up) = &mut self.logs {
            match mouse.kind {
                MouseEventKind::ScrollUp => *up += 3,
                MouseEventKind::ScrollDown => *up = up.saturating_sub(3),
                _ => {}
            }
            return;
        }
        if self.zoom.is_some() || self.show_help || self.debug.is_some() || self.confirm.is_some()
        {
            return; // overlay/question owns the screen; keys close it
        }
        if matches!(
            mouse.kind,
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown | MouseEventKind::Down(_)
        ) {
            self.follow = false; // manual movement
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
            HitKind::Source { line: row, col: col0 } => {
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
                let col = col0 + (x - body.x).saturating_sub(crate::ui::GUTTER) as usize;
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
        let before = self.selected;
        let action = self.on_key_inner(key, rendered);
        if self.selected != before {
            self.follow = false; // manual movement ends run-all following
        }
        action
    }

    fn on_key_inner(&mut self, key: KeyEvent, rendered: &mut Rendered) -> Action {
        self.message = None;
        self.manual_scroll = false; // keyboard nav re-follows the selection
        if let Some(up) = &mut self.logs {
            // scroll keys move through the log; anything else closes it
            // (draw clamps `up` to the entry count)
            let page = (self.body.height as usize / 2).max(1);
            let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
            match key.code {
                KeyCode::Char('k') | KeyCode::Up => *up += 1,
                KeyCode::Char('j') | KeyCode::Down => *up = up.saturating_sub(1),
                KeyCode::Char('u') if ctrl => *up += page,
                KeyCode::Char('d') if ctrl => *up = up.saturating_sub(page),
                KeyCode::PageUp => *up += page,
                KeyCode::PageDown => *up = up.saturating_sub(page),
                KeyCode::Char('g') | KeyCode::Home => *up = usize::MAX,
                KeyCode::Char('G') | KeyCode::End => *up = 0,
                _ => self.logs = None,
            }
            return Action::None;
        }
        if let Some(pager) = &mut self.pager {
            let page = (self.body.height as usize / 2).max(1);
            let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
            match key.code {
                KeyCode::Char('j') | KeyCode::Down => pager.top += 1,
                KeyCode::Char('k') | KeyCode::Up => pager.top = pager.top.saturating_sub(1),
                KeyCode::Char('d') if ctrl => pager.top += page,
                KeyCode::Char('u') if ctrl => pager.top = pager.top.saturating_sub(page),
                KeyCode::PageDown | KeyCode::Char(' ') => pager.top += page,
                KeyCode::PageUp => pager.top = pager.top.saturating_sub(page),
                KeyCode::Char('g') | KeyCode::Home => pager.top = 0,
                KeyCode::Char('G') | KeyCode::End => pager.top = usize::MAX,
                _ => self.pager = None,
            }
            return Action::None;
        }
        if self.show_help || self.debug.is_some() || self.zoom.is_some() {
            self.show_help = false;
            self.debug = None;
            self.zoom = None;
            return Action::None;
        }
        if self.stdin_req.is_some() {
            self.on_key_stdin(key);
            return Action::None;
        }
        if self.prompt.is_some() {
            self.on_key_prompt(key);
            return Action::None;
        }
        if let Some(confirm) = self.confirm.take() {
            self.answer(confirm, key, rendered);
            return Action::None;
        }
        if self.editor.is_some() {
            self.on_key_edit(key, rendered);
            if self.editor.is_some() {
                self.after_edit_key(key);
            }
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

    /// The key answering a pending `Confirm` question.
    fn answer(&mut self, confirm: Confirm, key: KeyEvent, rendered: &mut Rendered) {
        let k = key.code;
        match confirm {
            Confirm::Quit => match k {
                // discard; the autosave stays behind as a safety net
                KeyCode::Char('q') => self.should_quit = true,
                KeyCode::Char('s') | KeyCode::Char('w') => {
                    if self.save_notebook(false) {
                        self.should_quit = true;
                    }
                }
                _ => self.message = Some("quit cancelled".into()),
            },
            Confirm::Reload => {
                if k == KeyCode::Char('l') {
                    match self.reload(rendered) {
                        Ok(()) => self.message = Some("reloaded from disk".into()),
                        Err(e) => self.message = Some(format!("reload failed: {e:#}")),
                    }
                } else {
                    // keep mine: accept the divergence; a plain w now overwrites
                    self.disk_mtime = mtime(&self.path);
                    self.message = Some("keeping this buffer — w will overwrite the file".into());
                }
            }
            Confirm::Restart => match k {
                KeyCode::Char('R') => self.restart_kernel(false),
                KeyCode::Char('a') => self.restart_kernel(true),
                _ => self.message = Some("restart cancelled".into()),
            },
            Confirm::Recover(saved, _) => match k {
                KeyCode::Char('r') => {
                    self.notebook = *saved;
                    rendered.rebuild_all(&self.notebook);
                    self.selected = 0;
                    self.undo_stack.clear();
                    self.redo_stack.clear();
                    self.dirty = true; // the sidecar stays until the next save
                    self.message = Some("autosave restored — w writes it to the file".into());
                }
                KeyCode::Char('d') => {
                    let _ = std::fs::remove_file(self.autosave_path());
                    self.message = Some("autosave deleted".into());
                }
                _ => {
                    self.message =
                        Some("autosave kept — the next autosave (after any change) replaces it".into())
                }
            },
        }
    }

    /// Keys while a status-line prompt (`/` search, `S` save as) is open.
    fn on_key_prompt(&mut self, key: KeyEvent) {
        let Some(prompt) = &mut self.prompt else { return };
        match key.code {
            KeyCode::Esc => self.prompt = None,
            KeyCode::Backspace => {
                prompt.buf.pop();
            }
            KeyCode::Char(c) => prompt.buf.push(c),
            KeyCode::Enter => {
                let (kind, buf) = (prompt.kind, prompt.buf.clone());
                match kind {
                    PromptKind::Search => {
                        self.prompt = None;
                        if !buf.is_empty() {
                            self.search = Some(buf);
                            self.find(1);
                        }
                    }
                    PromptKind::SaveAs => {
                        if self.save_as(&buf) {
                            self.prompt = None;
                        }
                    }
                }
            }
            _ => {}
        }
    }

    /// Save under a new path and continue editing that file. Returns false
    /// while waiting for a second Enter to overwrite an existing file.
    fn save_as(&mut self, typed: &str) -> bool {
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
        if self.save_notebook(true) {
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
        if self.on_key_completion(key) {
            return;
        }
        // Ctrl+\ splits the cell at the cursor (plain terminals report it as Ctrl+4)
        if key.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(key.code, KeyCode::Char('\\') | KeyCode::Char('4'))
        {
            self.split_cell(rendered);
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
        if let Some(notice) = self.editor.as_mut().and_then(|e| e.take_notice()) {
            self.message = Some(notice);
        }
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
            self.record(CellOp::SetSource(self.selected, source), rendered);
        }
    }

    fn on_key_notebook(&mut self, key: KeyEvent, rendered: &mut Rendered) -> Action {
        let pending = self.pending.take();
        let count = self.count.take();
        let step = count.unwrap_or(1).max(1);
        let last = self.notebook.cells.len().saturating_sub(1);
        match (pending, key.code) {
            (Some('d'), KeyCode::Char('d')) => self.delete_cell(rendered),
            (Some('y'), KeyCode::Char('y')) => self.yank_cell(),
            (Some(_), _) => {}
            (None, KeyCode::Char(c)) if c.is_ascii_digit() => {
                self.count = Some(count.unwrap_or(0) * 10 + (c as usize - '0' as usize));
            }
            (None, code) => match code {
                KeyCode::Char('q') => {
                    if self.dirty {
                        self.confirm = Some(Confirm::Quit);
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
                KeyCode::Char('S') => {
                    self.overwrite_armed = None;
                    self.prompt = Some(Prompt {
                        kind: PromptKind::SaveAs,
                        buf: self.path.display().to_string(),
                    });
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
                KeyCode::Char('/') => {
                    self.prompt = Some(Prompt {
                        kind: PromptKind::Search,
                        buf: String::new(),
                    })
                }
                KeyCode::Char('n') => self.find(1),
                KeyCode::Char('N') => self.find(-1),
                KeyCode::Char('u') => self.undo(rendered),
                KeyCode::Char('r') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    self.redo(rendered)
                }
                KeyCode::Char('c') if key.modifiers.is_empty() => {
                    self.clear_outputs(self.selected..self.selected + 1, rendered)
                }
                KeyCode::Char('C') => self.clear_outputs(0..self.notebook.cells.len(), rendered),
                KeyCode::Char('M') => self.merge_below(rendered),
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
                KeyCode::Char('X') => self.run_range(0..self.notebook.cells.len(), rendered),
                // run in place (Ctrl+Enter is the same, where terminals report it)
                KeyCode::Char(' ') => self.run_cell(self.selected, rendered),
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
                        cell.extra.insert("id".into(), new_cell_id().into());
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
                // a live kernel holds state worth a confirmation; a dead one not
                KeyCode::Char('R') if self.kernel.is_none() && !self.kernel_starting => {
                    self.restart_kernel(false)
                }
                KeyCode::Char('R') => self.confirm = Some(Confirm::Restart),
                KeyCode::Char('?') => self.show_help = true,
                KeyCode::Char('L') => self.logs = Some(0),
                KeyCode::Char('D') => {
                    let text = self.debug_report(rendered);
                    log::info!(target: "debug", "{}", text.replace('\n', " | "));
                    osc52(&text);
                    self.debug = Some(text);
                    self.message = Some("debug info copied to clipboard".into());
                }
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

    #[cfg(test)]
    pub fn open_editor_for_test(&mut self) {
        self.open_editor(false);
        if let Some(e) = &mut self.editor {
            e.append_end();
        }
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
        let filetype = self.cell_filetype();
        let user_config = crate::config::get().nvim_user_config;
        let existing = self.nvim.take();
        let had_existing = existing.is_some();
        let opened = existing
            .map(Ok)
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

    fn run_range(&mut self, range: std::ops::Range<usize>, rendered: &mut Rendered) {
        self.follow = range.len() > 1;
        for i in range {
            self.run_cell(i, rendered);
        }
    }

    /// `D`: everything about the selected cell that helps pin down a bug
    /// report — identity, model, render/viewport state, execution, kernel.
    fn debug_report(&self, rendered: &Rendered) -> String {
        let mut out = format!("jotter {}\n", env!("CARGO_PKG_VERSION"));
        match self.notebook.cells.get(self.selected) {
            Some(cell) => {
                let id = cell.extra.get("id").and_then(Value::as_str).unwrap_or("-");
                let count = cell
                    .execution_count()
                    .map_or("-".to_string(), |n| n.to_string());
                out += &format!(
                    "cell {}/{} · id {id} · {} · execution_count {count}\n",
                    self.selected + 1,
                    self.notebook.cells.len(),
                    cell.cell_type,
                );
                out += &format!(
                    "source: {} lines, {} chars\n",
                    cell.source.split('\n').count(),
                    cell.source.chars().count()
                );
                let outputs: Vec<String> = cell
                    .outputs
                    .iter()
                    .flatten()
                    .map(|o| {
                        let ty = o["output_type"].as_str().unwrap_or("?");
                        match ty {
                            "stream" => format!(
                                "stream:{} ({} lines)",
                                o["name"].as_str().unwrap_or("?"),
                                crate::notebook::join_multiline(&o["text"]).lines().count()
                            ),
                            "error" => format!("error:{}", o["ename"].as_str().unwrap_or("?")),
                            _ => {
                                let mimes = o["data"]
                                    .as_object()
                                    .map(|d| d.keys().cloned().collect::<Vec<_>>().join(","));
                                format!("{ty}[{}]", mimes.unwrap_or_default())
                            }
                        }
                    })
                    .collect();
                out += &format!("outputs ({}): {}\n", outputs.len(), outputs.join(" · "));
                if let Some(b) = rendered.blocks.get(self.selected) {
                    out += &format!("render: {}\n", b.debug_summary());
                }
                let runs: Vec<String> = self
                    .running
                    .iter()
                    .filter(|&(_, &c)| c == self.selected)
                    .map(|(m, _)| {
                        let state = if self.started.contains_key(m) {
                            "running"
                        } else {
                            "queued"
                        };
                        let half = if self.half_done.contains(m) { ", half-done" } else { "" };
                        format!("{state}{half} msg {m}")
                    })
                    .collect();
                out += &format!(
                    "exec: {}\n",
                    if runs.is_empty() { "idle".into() } else { runs.join(" · ") }
                );
            }
            None => out += "no cell selected (empty notebook)\n",
        }
        out += &format!(
            "view: scroll {} of {} lines{} · body {}x{}\n",
            self.scroll,
            self.content_lines,
            if self.manual_scroll { " (manual)" } else { "" },
            self.body.width,
            self.body.height
        );
        out += &match &self.kernel {
            Some(k) => format!(
                "kernel: {} ({}) · {} · {} in flight\n",
                k.name,
                k.spec_dir.display(),
                if self.kernel_busy { "busy" } else { "idle" },
                self.running.len()
            ),
            None => format!("kernel: none (requested '{}')\n", self.kernel_name),
        };
        if let Some(c) = &self.completion {
            out += &format!(
                "completion: {} · {} of {} matches · chars {}..{}{}\n",
                if c.all.is_empty() { "pending" } else { "open" },
                c.items.len(),
                c.all.len(),
                c.start,
                c.end,
                if c.explicit { " · Tab" } else { " · auto" },
            );
        }
        out += &format!(
            "editor: {} · graphics: {}\n",
            match &self.editor {
                None => "closed".to_string(),
                Some(e) => format!(
                    "{} {:?} cursor {:?}",
                    if e.is_builtin() { "builtin" } else { "nvim" },
                    e.mode(),
                    e.cursor()
                ),
            },
            rendered.graphics_summary()
        );
        out.trim_end().to_string()
    }

    /// Drop all in-flight execution bookkeeping (restart, death, reload).
    fn forget_executions(&mut self) {
        self.running.clear();
        self.started.clear();
        self.pending_clear.clear();
        self.half_done.clear();
        self.follow = false;
    }

    /// The code cell an execution's messages route to. Anything else is
    /// dropped: a cell turned markdown mid-run must not gain outputs.
    fn target_cell(&self, parent: &str) -> Option<usize> {
        let &idx = self.running.get(parent)?;
        (self.notebook.cells.get(idx)?.cell_type == "code").then_some(idx)
    }

    /// One of an execution's two end signals arrived; finish on the second.
    fn end_signal(&mut self, parent: String, rendered: &mut Rendered) {
        if !self.running.contains_key(&parent) {
            return; // idle of kernel_info etc., or a forgotten execution
        }
        if self.half_done.insert(parent.clone()) {
            return;
        }
        self.half_done.remove(&parent);
        // gutter timing: wall time from execute_input to the end
        if let (Some(idx), Some(t)) = (self.target_cell(&parent), self.started.get(&parent))
            && let Some(b) = rendered.blocks.get_mut(idx)
        {
            b.elapsed = Some(t.elapsed());
        }
        self.running.remove(&parent);
        self.started.remove(&parent);
        self.pending_clear.remove(&parent);
        if self.running.is_empty() {
            self.follow = false;
        }
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
            if self.kernel_starting
                && let Some(id) = cell.extra.get("id").and_then(Value::as_str)
            {
                if !self.deferred.iter().any(|d| d == id) {
                    self.deferred.push(id.to_string());
                }
                self.message = Some(format!(
                    "kernel starting — {} cell(s) queued",
                    self.deferred.len()
                ));
            } else {
                self.message = Some("no kernel — R to (re)start one".into());
            }
            return;
        };
        cell.outputs = Some(Vec::new());
        // a fresh run follows its tail, even if the previous output had been
        // scrolled up (rebuild_cell preserves the viewport across rebuilds)
        if let Some(b) = rendered.blocks.get_mut(idx) {
            b.out_scroll = usize::MAX;
        }
        let msg_id = kernel.execute(cell.source.clone());
        rendered.rebuild_cell(idx, cell);
        self.running.insert(msg_id, idx);
        self.touch();
    }

    pub fn apply_kernel_event(&mut self, event: Event, rendered: &mut Rendered) {
        match event {
            Event::Ready(kernel) if kernel.launch != self.launch => {
                log::info!("dropping stale kernel from launch {}", kernel.launch);
            }
            Event::Ready(kernel) => {
                let origin = kernel.spec_dir.display();
                self.message = Some(if kernel.name == self.kernel_name {
                    format!("kernel '{}' ready ({origin})", kernel.name)
                } else {
                    format!("'{}' not found — using {origin}", self.kernel_name)
                });
                // new notebooks (or ones that never had one) learn their
                // kernelspec, like jupyter writes it; saved with the next w
                if let Some(meta) = self
                    .notebook
                    .extra
                    .entry("metadata")
                    .or_insert_with(|| Value::Object(Map::new()))
                    .as_object_mut()
                    && !meta.contains_key("kernelspec")
                {
                    meta.insert(
                        "kernelspec".into(),
                        serde_json::json!({"name": kernel.name,
                            "display_name": kernel.display_name, "language": kernel.language}),
                    );
                    rendered.set_language(&kernel.language);
                }
                // IPython completes with jedi (static analysis, slow on big
                // modules) unless told otherwise; the live-object completer
                // answers in milliseconds. Silent: no output, count, history.
                if kernel.language.eq_ignore_ascii_case("python") && !crate::config::get().jedi {
                    kernel.execute_silent("%config Completer.use_jedi = False");
                }
                self.kernel = Some(*kernel);
                self.kernel_starting = false;
                for id in std::mem::take(&mut self.deferred) {
                    let idx = self
                        .notebook
                        .cells
                        .iter()
                        .position(|c| c.extra.get("id").and_then(Value::as_str) == Some(&id));
                    if let Some(idx) = idx {
                        self.run_cell(idx, rendered);
                    }
                }
            }
            Event::LaunchFailed { launch, .. } if launch != self.launch => {}
            Event::LaunchFailed { reason, .. } => {
                self.kernel_starting = false;
                let dropped = std::mem::take(&mut self.deferred).len();
                self.message = Some(if dropped > 0 {
                    format!("{reason} — {dropped} queued run(s) dropped")
                } else {
                    reason
                });
            }
            Event::UpdateDisplay { display_id, output } => {
                // ponytail: scans every output per update; fine for thousands
                // of outputs, index display_ids if it ever shows in profiles
                for (idx, cell) in self.notebook.cells.iter_mut().enumerate() {
                    let mut hit = false;
                    for o in cell.outputs.iter_mut().flatten() {
                        if o["transient"]["display_id"].as_str() == Some(&display_id) {
                            o["data"] = output["data"].clone();
                            o["metadata"] = output["metadata"].clone();
                            hit = true;
                        }
                    }
                    if hit {
                        rendered.rebuild_cell(idx, cell);
                        self.dirty = true;
                        self.autosave_pending = true;
                    }
                }
            }
            Event::Complete { parent, matches, start, types, .. } => {
                self.on_complete_reply(parent, matches, start, types)
            }
            Event::Inspect { parent, text } => self.on_inspect_reply(parent, text),
            Event::Output { parent, output } => {
                if let Some(idx) = self.target_cell(&parent) {
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
                } else if let Some(idx) = self.target_cell(&parent) {
                    let cell = &mut self.notebook.cells[idx];
                    cell.outputs = Some(Vec::new());
                    rendered.rebuild_cell(idx, cell);
                    self.touch();
                }
            }
            Event::ExecutionCount { parent, count } => {
                if let Some(idx) = self.target_cell(&parent) {
                    self.notebook.cells[idx]
                        .extra
                        .insert("execution_count".into(), count.into());
                    self.touch();
                    self.started.insert(parent, Instant::now()); // no longer queued
                    if self.follow {
                        self.selected = idx;
                        self.manual_scroll = false;
                    }
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
            Event::Status { parent, busy } => {
                self.kernel_busy = busy;
                if !busy {
                    self.end_signal(parent, rendered);
                }
            }
            Event::Done { parent } => self.end_signal(parent, rendered),
            Event::Dead(reason) => {
                self.kernel = None;
                self.kernel_busy = false;
                self.forget_executions();
                self.stdin_req = None;
                self.message = Some(format!("{reason} — R to restart"));
            }
            Event::Info(s) => self.message = Some(s),
        }
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;

    /// App over a one-code-cell notebook, plus its render cache.
    pub(super) fn one_cell_app(tag: &str) -> (App, Rendered) {
        let path = std::env::temp_dir().join(format!("jotter-{tag}-{}.ipynb", std::process::id()));
        let nb = serde_json::json!({
            "cells": [{"cell_type": "code", "id": "c1", "metadata": {}, "execution_count": null,
                       "outputs": [], "source": ["x"]}],
            "metadata": {}, "nbformat": 4, "nbformat_minor": 5
        });
        std::fs::write(&path, nb.to_string()).unwrap();
        let (tx, _rx) = mpsc::unbounded_channel();
        let app = App::open(path.clone(), None, tx).unwrap();
        std::fs::remove_file(&path).ok();
        let rendered = Rendered::build(&app.notebook, None, Default::default());
        (app, rendered)
    }

    pub(super) fn stream(text: &str) -> Value {
        serde_json::json!({"output_type": "stream", "name": "stdout", "text": text})
    }

    pub(super) fn labels(app: &App) -> Vec<&str> {
        app.completion.as_ref().unwrap().items.iter().map(|c| c.label()).collect()
    }

    pub(super) fn sources(app: &App) -> Vec<&str> {
        app.notebook.cells.iter().map(|c| c.source.as_str()).collect()
    }

    pub(super) fn key(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
    }

    #[test]
    fn runs_during_startup_are_queued_and_dropped_on_launch_failure() {
        let (mut app, mut r) = one_cell_app("defer");
        app.kernel_starting = true;
        app.launch = 3;
        app.run_cell(0, &mut r);
        app.run_cell(0, &mut r);
        assert_eq!(app.deferred, ["c1"], "queued once, by cell id");
        // a stale launch's failure is ignored
        app.apply_kernel_event(Event::LaunchFailed { launch: 2, reason: "old".into() }, &mut r);
        assert_eq!(app.deferred.len(), 1);
        app.apply_kernel_event(Event::LaunchFailed { launch: 3, reason: "boom".into() }, &mut r);
        assert!(app.deferred.is_empty() && !app.kernel_starting);
        assert!(app.message.as_deref().unwrap().contains("1 queued run(s) dropped"));
    }

    #[test]
    fn restart_asks_first_when_a_kernel_could_be_alive() {
        let (mut app, mut r) = one_cell_app("restart");
        app.kernel_starting = true; // as if a kernel were coming up
        app.on_key(key('R'), &mut r);
        assert_eq!(app.confirm, Some(Confirm::Restart));
        app.on_key(key('x'), &mut r);
        assert!(app.confirm.is_none());
        assert_eq!(app.message.as_deref(), Some("restart cancelled"));
    }

    #[test]
    fn update_display_data_replaces_matching_outputs_everywhere() {
        let (mut app, mut r) = one_cell_app("upd");
        let disp = |v: &str| serde_json::json!({"output_type": "display_data",
            "data": {"text/plain": v}, "metadata": {}, "transient": {"display_id": "d1"}});
        app.notebook.cells[0].push_output(disp("old"));
        app.notebook.cells.push(app.notebook.cells[0].clone());
        r.rebuild_all(&app.notebook);
        app.apply_kernel_event(
            Event::UpdateDisplay { display_id: "d1".into(), output: disp("new") },
            &mut r,
        );
        for c in &app.notebook.cells {
            assert_eq!(c.outputs.as_ref().unwrap()[0]["data"]["text/plain"], "new");
        }
        assert!(app.dirty);
    }

    #[test]
    fn run_all_follows_the_running_cell_until_manual_movement() {
        let (mut app, mut r) = one_cell_app("follow");
        for _ in 0..2 {
            app.notebook.cells.push(app.notebook.cells[0].clone());
        }
        r.rebuild_all(&app.notebook);
        let start = |app: &mut App, r: &mut Rendered, id: &str, idx: usize| {
            app.running.insert(id.into(), idx);
            app.apply_kernel_event(Event::ExecutionCount { parent: id.into(), count: 1 }, r);
        };
        app.follow = true; // as run_range(0..3) sets it
        start(&mut app, &mut r, "m1", 1);
        assert_eq!(app.selected, 1, "selection jumps to the running cell");
        app.on_key(key('k'), &mut r); // manual movement
        assert!(!app.follow);
        start(&mut app, &mut r, "m2", 2);
        assert_eq!(app.selected, 0, "no more jumping");
        // non-movement keys keep following
        app.follow = true;
        app.on_key(key('o'), &mut r);
        assert!(app.follow);
    }

    #[test]
    fn space_runs_x_runs_all_ctrl_r_redoes() {
        let (mut app, mut r) = one_cell_app("keys");
        app.on_key(key(' '), &mut r);
        assert_eq!(app.message.as_deref(), Some("no kernel — R to (re)start one"));
        app.on_key(key('a'), &mut r);
        app.on_key(key('u'), &mut r);
        assert_eq!(app.notebook.cells.len(), 1);
        app.on_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL), &mut r);
        assert_eq!(app.notebook.cells.len(), 2, "Ctrl+r redoes");
        app.on_key(key('X'), &mut r);
        assert!(app.follow, "run all follows");
    }

    #[test]
    fn missing_path_opens_a_new_notebook_and_recovery_restores() {
        let path = std::env::temp_dir().join(format!("jotter-new-{}.ipynb", std::process::id()));
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut app = App::open(path.clone(), None, tx).unwrap();
        assert!(!path.exists(), "nothing written until w");
        assert_eq!(sources(&app), [""]);
        assert!(app.message.as_deref().unwrap().starts_with("new notebook"));
        let mut r = Rendered::build(&app.notebook, None, Default::default());
        // recovery prompt: r swaps in the autosaved notebook as a dirty buffer
        let mut saved = app.notebook.clone();
        saved.cells[0].source = "unsaved work".into();
        assert_eq!(differing_cells(&saved, &app.notebook), 1);
        app.confirm = Some(Confirm::Recover(Box::new(saved), "x".into()));
        app.on_key(key('r'), &mut r);
        assert_eq!(sources(&app), ["unsaved work"]);
        assert!(app.dirty);
    }

    #[test]
    fn autosave_paths_live_in_the_state_dir_keyed_by_absolute_path() {
        let p = autosave_path_for(std::path::Path::new("/data/nb/a.ipynb"));
        assert!(p.ends_with("jotter/autosave/%data%nb%a.ipynb"), "{}", p.display());
        let long = format!("/{}/a.ipynb", "d".repeat(400));
        let name = autosave_path_for(std::path::Path::new(&long));
        assert!(name.file_name().unwrap().len() <= 200);
    }

    #[test]
    fn execution_ends_on_reply_and_idle_in_either_order() {
        let (mut app, mut r) = one_cell_app("end");
        for (first, second) in [
            (Event::Done { parent: "m".into() }, Event::Status { parent: "m".into(), busy: false }),
            (Event::Status { parent: "m".into(), busy: false }, Event::Done { parent: "m".into() }),
        ] {
            app.notebook.cells[0].outputs = Some(Vec::new());
            app.running.insert("m".into(), 0);
            app.apply_kernel_event(first, &mut r);
            // output racing the reply (separate sockets) still lands
            app.apply_kernel_event(Event::Output { parent: "m".into(), output: stream("late\n") }, &mut r);
            assert_eq!(app.notebook.cells[0].outputs.as_ref().unwrap().len(), 1);
            app.apply_kernel_event(second, &mut r);
            assert!(app.running.is_empty() && app.half_done.is_empty());
        }
        // idle of a request we never sent (kernel_info) is ignored
        app.apply_kernel_event(Event::Status { parent: "other".into(), busy: false }, &mut r);
        assert!(app.half_done.is_empty());
    }

    #[test]
    fn non_code_cells_never_receive_kernel_output() {
        let (mut app, mut r) = one_cell_app("toggle");
        app.running.insert("m".into(), 0);
        app.toggle_type(&mut r); // code -> markdown mid-run
        app.apply_kernel_event(Event::ExecutionCount { parent: "m".into(), count: 3 }, &mut r);
        app.apply_kernel_event(Event::Output { parent: "m".into(), output: stream("x\n") }, &mut r);
        let cell = &app.notebook.cells[0];
        assert!(cell.outputs.is_none());
        assert!(!cell.extra.contains_key("execution_count"));
    }

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
        assert!(app.confirm.is_none());

        // dirty buffer: prompt instead of clobbering local state
        app.notebook.cells[0].source = "local".into();
        app.touch();
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(&path, nb("a = 3")).unwrap();
        app.check_disk(&mut rendered);
        assert_eq!(app.confirm, Some(Confirm::Reload));
        assert_eq!(app.notebook.cells[0].source, "local");

        // "keep mine" (any key but l) accepts divergence: no re-prompt, w saves
        let key = KeyEvent::new(KeyCode::Char('k'), KeyModifiers::NONE);
        app.on_key(key, &mut rendered);
        assert!(app.confirm.is_none());
        app.check_disk(&mut rendered);
        assert!(app.confirm.is_none(), "mtime accepted, no re-prompt");
        assert!(
            app.save_notebook(false),
            "plain w overwrites after keep-mine"
        );
        std::fs::remove_file(&path).ok();
    }
}
