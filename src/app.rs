use crate::editor::{Editor, Outcome};
use crate::kernel::{Event, Kernel};
use crate::notebook::{Cell, Notebook, new_cell_id};
use crate::nvim::{Backend, ModeKind, NvimCell, NvimSession};
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
mod signature;

use cellops::CellOp;
pub use completion::Completion;
pub use signature::SigHelp;

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
                format!("{summary} — r: restore · d: delete · else keep")
            }
        }
    }
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum PromptKind {
    Search,
    SaveAs,
    /// `:` command line (vim ex commands, see `App::ex_command`).
    Command,
}

/// Screen position for the selected cell (vim's zt / zz / zb).
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Align {
    Top,
    Middle,
    Bottom,
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
    /// Empty for a scratchpad (`jotter` without a file); `:w name` names it.
    pub path: PathBuf,
    pub selected: usize,
    /// First visible rendered line; adjusted in ui::draw to keep selection visible.
    pub scroll: usize,
    pub should_quit: bool,
    /// One-shot status message.
    pub message: Option<String>,
    /// The terminal speaks the kitty keyboard protocol (Shift+Enter works).
    pub keyboard_protocol: bool,
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
    /// The first session, started in the background when the notebook opens
    /// so the first edit doesn't wait for the user's nvim config to load.
    nvim_spawn: Option<std::thread::JoinHandle<Result<NvimSession>>>,
    /// Line -> cell mapping of the last draw (mouse support).
    pub hit: Vec<Hit>,
    /// Notebook body area of the last draw.
    pub body: Rect,
    /// View was wheel-scrolled: draw must not snap back to the selection.
    pub manual_scroll: bool,
    /// `zz`/`zt`/`zb`: where the next draw puts the selected cell.
    pub align: Option<Align>,
    /// Keybinding cheatsheet overlay is open.
    pub show_help: bool,
    /// Total rendered lines of the last draw (scroll bound).
    pub content_lines: usize,
    last_click: Option<(Instant, u16, u16)>,
    /// Pending question; the next key answers it (shown on the status line).
    pub confirm: Option<Confirm>,
    /// First key of a two-key chord (dd, yy, gg).
    pending: Option<char>,
    /// Vim-style jump list of cell ids (ids survive inserts and moves);
    /// `jump_at` == len means "at the newest position, not in the list".
    jumps: Vec<String>,
    jump_at: usize,
    /// `ma`: mark letter -> cell id. Session only; a mark on a deleted cell
    /// is gone until undo brings the cell (and its id) back.
    pub marks: std::collections::BTreeMap<char, String>,
    /// Cells deleted or yanked last (`p` pastes them all).
    yanked: Vec<Cell>,
    /// `V`: anchor cell of the visual selection, which runs to `selected`.
    pub visual: Option<usize>,
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
    /// Signature help for the call the cursor is in.
    pub signature: Option<SigHelp>,
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

/// Jump list depth (vim's is 100).
const JUMPS_CAP: usize = 100;

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
            let file = path
                .file_name()
                .map(|s| s.to_string_lossy())
                .unwrap_or_default();
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
            keyboard_protocol: false,
            kernel: None,
            kernel_busy: false,
            running: HashMap::new(),
            dirty: false,
            editor: None,
            nvim: None,
            nvim_spawn: (crate::config::get().editor == "nvim").then(|| {
                let user_config = crate::config::get().nvim_user_config;
                std::thread::spawn(move || NvimSession::spawn(user_config))
            }),
            hit: Vec::new(),
            body: Rect::default(),
            manual_scroll: false,
            align: None,
            show_help: false,
            content_lines: 0,
            last_click: None,
            confirm: None,
            pending: None,
            jumps: Vec::new(),
            jump_at: 0,
            marks: Default::default(),
            yanked: Vec::new(),
            visual: None,
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
            signature: None,
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
        if is_new && !app.is_scratch() {
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
        match Notebook::read_ipynb(&side) {
            Ok(saved) => match differing_cells(&saved, &self.notebook) {
                0 => {
                    let _ = std::fs::remove_file(&side);
                }
                n => {
                    let summary = format!("autosave from {} ago, {n} cell(s) differ", age(when));
                    self.confirm = Some(Confirm::Recover(Box::new(saved), summary));
                }
            },
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

    fn touch(&mut self) {
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

    /// `:w` from inside a cell: commit its text, then save like `w` (`:w!`
    /// like `W`), with the on-disk-change refusal phrased in the editor's terms.
    fn write_from_editor(&mut self, source: String, force: bool, rendered: &mut Rendered) -> bool {
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
    fn save_notebook(&mut self, force: bool, rendered: &mut Rendered) -> bool {
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
        if self.zoom.is_some() || self.show_help || self.debug.is_some() || self.confirm.is_some() {
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
        self.visual = None;
        self.selected = cell;
        self.manual_scroll = true; // a click never scrolls the view
        match kind {
            HitKind::Source {
                line: row,
                col: col0,
            } => {
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

    /// Bracketed paste, whole: a key per character would run each one as a
    /// command outside the editor, and cost an nvim round trip each inside.
    pub fn on_paste(&mut self, text: String, rendered: &mut Rendered) {
        let text = text.replace("\r\n", "\n").replace('\r', "\n");
        self.message = None;
        if let Some(req) = &mut self.stdin_req {
            req.buf.push_str(text.lines().next().unwrap_or(""));
        } else if let Some(p) = &mut self.prompt {
            p.buf.push_str(text.lines().next().unwrap_or(""));
        } else if let Some(editor) = &mut self.editor {
            self.completion = None;
            if let Err(e) = editor.paste(&text) {
                self.nvim_failed(e, rendered);
            }
        } else {
            self.message = Some("paste ignored outside a cell (i or A edits it)".into());
        }
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
            self.on_key_prompt(key, rendered);
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
                self.update_signature(key);
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
                KeyCode::Char('q') => self.discard_quit(),
                KeyCode::Char('s') | KeyCode::Char('w') => {
                    if self.save_notebook(false, rendered) {
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
                    self.message = Some(
                        "autosave kept — the next autosave (after any change) replaces it".into(),
                    )
                }
            },
        }
    }

    /// Keys while a status-line prompt (`/` search, `S` save as) is open.
    fn on_key_prompt(&mut self, key: KeyEvent, rendered: &mut Rendered) {
        let Some(prompt) = &mut self.prompt else {
            return;
        };
        match key.code {
            KeyCode::Esc => self.prompt = None,
            // like vim: backspace on an empty line closes it
            KeyCode::Backspace if prompt.buf.is_empty() => self.prompt = None,
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
                        if self.save_as(&buf, rendered) {
                            self.prompt = None;
                        }
                    }
                    PromptKind::Command => {
                        self.prompt = None;
                        self.ex_command(&buf, rendered);
                    }
                }
            }
            _ => {}
        }
    }

    /// A `:` command, vim spellings. Inside a cell they act like the nvim
    /// backend's: `:w` saves the notebook and keeps editing, `:q` leaves
    /// the cell, `:wq`/`:x` both. In notebook mode `:q` quits and `:5`
    /// selects cell 5.
    fn ex_command(&mut self, cmd: &str, rendered: &mut Rendered) {
        let cmd = cmd.trim();
        let (name, arg) = cmd
            .split_once(char::is_whitespace)
            .map_or((cmd, ""), |(n, a)| (n, a.trim()));
        let force = name.ends_with('!');
        let editing = self.editor.is_some();
        match (name, arg) {
            ("", _) => {}
            ("w" | "w!", "") => {
                self.ex_write(force, rendered);
            }
            ("w" | "w!" | "sav" | "saveas", path) => {
                if let Some(src) = self.editor.as_ref().map(|e| e.source()) {
                    self.update_cell_source(src, rendered);
                }
                if force {
                    self.overwrite_armed = Some(expand_tilde(path));
                }
                if !self.save_as(path, rendered) {
                    self.overwrite_armed = None;
                    self.message = Some(format!("{path} exists — :w! {path} overwrites"));
                }
            }
            ("delm!", "") => {
                self.marks.clear();
                self.message = Some("all marks deleted".into());
            }
            // a deliberate crash, to check that the terminal comes back and
            // the autosave is offered at the next open (tests/crash.rs)
            ("panic", "") => panic!(":panic"),
            ("delm" | "delmarks", letters) if !letters.is_empty() => {
                self.marks.retain(|c, _| !letters.contains(*c));
            }
            ("q" | "q!", "") if editing => self.leave_editor(force, rendered),
            ("q", "") => self.quit(),
            ("q!" | "qa!", "") => self.discard_quit(),
            ("qa", "") => {
                self.leave_editor(false, rendered);
                self.quit();
            }
            ("wq" | "wq!" | "x" | "x!", "") => {
                // leave first, so format_on_save also formats this cell
                self.leave_editor(false, rendered);
                if self.save_notebook(force, rendered) && !editing {
                    self.should_quit = true;
                }
            }
            (n, "") if !editing && n.parse::<usize>().is_ok() => {
                let last = self.notebook.cells.len().saturating_sub(1);
                self.mark_jump();
                self.selected = n.parse::<usize>().unwrap_or(1).saturating_sub(1).min(last);
            }
            _ => self.message = Some(format!("not a command: :{cmd}")),
        }
    }

    /// `:w`: commit the open cell's text (if any), then save.
    fn ex_write(&mut self, force: bool, rendered: &mut Rendered) -> bool {
        match self.editor.as_ref().map(|e| e.source()) {
            Some(src) => self.write_from_editor(src, force, rendered),
            None => self.save_notebook(force, rendered),
        }
    }

    /// Close the cell editor: commit its text, or (`discard`) drop the edit.
    fn leave_editor(&mut self, discard: bool, rendered: &mut Rendered) {
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

    /// `q`: quit, asking first when there are unsaved changes.
    fn quit(&mut self) {
        if self.dirty {
            self.confirm = Some(Confirm::Quit);
        } else {
            // a clean buffer's sidecar holds at most outputs a `.py` can't
            // store; left behind, every later open would offer them back
            let _ = std::fs::remove_file(self.autosave_path());
            self.should_quit = true;
        }
    }

    /// Quit without saving. The autosave stays behind as a safety net, but
    /// a scratchpad's goes, like vim's `:q!` dropping a no-name swap file
    /// (else every later `jotter` would offer to restore the throwaway).
    fn discard_quit(&mut self) {
        if self.is_scratch() {
            let _ = std::fs::remove_file(self.autosave_path());
        }
        self.should_quit = true;
    }

    /// Save under a new path and continue editing that file. Returns false
    /// while waiting for a second Enter to overwrite an existing file.
    fn save_as(&mut self, typed: &str, rendered: &mut Rendered) -> bool {
        let target = expand_tilde(typed.trim());
        if target.as_os_str().is_empty() {
            return true;
        }
        if target.exists() && target != self.path && self.overwrite_armed.as_ref() != Some(&target)
        {
            self.message = Some(format!(
                "{} exists — Enter again to overwrite",
                target.display()
            ));
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

    /// Before a jump (gg, G, :5, search): remember the selected cell. Like
    /// vim, an entry for the same cell moves to the newest position.
    fn mark_jump(&mut self) {
        let Some(id) = self.selected_id() else { return };
        self.jumps.retain(|j| *j != id);
        self.jumps.push(id);
        if self.jumps.len() > JUMPS_CAP {
            self.jumps.remove(0);
        }
        self.jump_at = self.jumps.len();
    }

    /// Ctrl+o. Leaving the newest position records it first, so Ctrl+i
    /// comes back to it.
    fn jump_older(&mut self) {
        if self.jump_at >= self.jumps.len() {
            self.mark_jump();
            self.jump_at = self.jumps.len().saturating_sub(1);
        }
        self.step_jump(-1);
    }

    /// Move `dir` entries through the jump list, skipping deleted cells and
    /// the cell already selected.
    fn step_jump(&mut self, dir: isize) {
        let mut at = self.jump_at as isize;
        loop {
            at += dir;
            let Some(id) = usize::try_from(at).ok().and_then(|a| self.jumps.get(a)) else {
                self.message = Some("end of jump list".into());
                return;
            };
            let Some(i) = self.cell_index(id) else {
                continue;
            };
            if i != self.selected {
                self.jump_at = at as usize;
                self.selected = i;
                return;
            }
        }
    }

    /// Index of the cell mark `c` is on, if it's set and the cell exists.
    pub fn mark_cell(&self, c: char) -> Option<usize> {
        self.cell_index(self.marks.get(&c)?)
    }

    fn selected_id(&self) -> Option<String> {
        let cell = self.notebook.cells.get(self.selected)?;
        Some(cell.extra.get("id")?.as_str()?.to_string())
    }

    fn cell_index(&self, id: &str) -> Option<usize> {
        self.notebook
            .cells
            .iter()
            .position(|c| c.extra.get("id").and_then(Value::as_str) == Some(id))
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
                self.mark_jump();
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
            self.message = Some("editing cell — :q or Esc leaves it, :w saves".into());
            return;
        }
        // the builtin editor has no command line of its own: open jOtter's
        if editor.is_builtin()
            && editor.mode() == ModeKind::Normal
            && key.code == KeyCode::Char(':')
        {
            self.prompt = Some(Prompt {
                kind: PromptKind::Command,
                buf: String::new(),
            });
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
    fn nvim_failed(&mut self, e: anyhow::Error, rendered: &mut Rendered) {
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

    fn commit_editor(&mut self, rendered: &mut Rendered) {
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
    fn update_cell_source(&mut self, source: String, rendered: &mut Rendered) {
        let Some(cell) = self.notebook.cells.get_mut(self.selected) else {
            return;
        };
        if source != cell.source {
            self.record(CellOp::SetSource(self.selected, source), rendered);
        }
    }

    fn on_key_notebook(&mut self, key: KeyEvent, rendered: &mut Rendered) -> Action {
        if self.visual.is_some()
            && self.pending.is_none()
            && self.count.is_none()
            && self.on_key_visual(key, rendered)
        {
            return Action::None;
        }
        let action = self.on_key_normal(key, rendered);
        // motions extend a visual selection; anything else ends it
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let motion = match key.code {
            KeyCode::Char('o' | 'i') => ctrl,
            KeyCode::Char('V' | 'j' | 'k' | 'g' | 'G' | 'n' | 'N' | '0'..='9')
            | KeyCode::Up
            | KeyCode::Down
            | KeyCode::Tab => true,
            _ => false,
        };
        if !motion {
            self.visual = None;
        }
        action
    }

    /// The cells an operation acts on: the `V` selection, else the selected one.
    pub fn sel_range(&self) -> std::ops::Range<usize> {
        let a = self.visual.unwrap_or(self.selected);
        a.min(self.selected)..a.max(self.selected) + 1
    }

    /// Operators on a `V` selection. False: not one, handle the key normally.
    fn on_key_visual(&mut self, key: KeyEvent, rendered: &mut Rendered) -> bool {
        let r = self.sel_range();
        match key.code {
            KeyCode::Esc | KeyCode::Char('V') => {}
            KeyCode::Char('d' | 'x') => self.delete_cells(r, rendered),
            KeyCode::Char('y') => self.yank_cells(r),
            // the selection moves along, so J/K repeat
            KeyCode::Char('J') => {
                self.move_cells(r, 1, rendered);
                return true;
            }
            KeyCode::Char('K') => {
                self.move_cells(r, -1, rendered);
                return true;
            }
            KeyCode::Char('r' | ' ') | KeyCode::Enter => self.run_range(r, rendered),
            KeyCode::Char('c') if key.modifiers.is_empty() => self.clear_outputs(r, rendered),
            KeyCode::Char('f') => self.message = Some(self.format_cells(r, rendered)),
            KeyCode::Char('M') => self.merge_cells(r, rendered),
            _ => return false,
        }
        self.visual = None;
        true
    }

    fn on_key_normal(&mut self, key: KeyEvent, rendered: &mut Rendered) -> Action {
        let pending = self.pending.take();
        let count = self.count.take();
        let step = count.unwrap_or(1).max(1);
        let last = self.notebook.cells.len().saturating_sub(1);
        match (pending, key.code) {
            (Some('d'), KeyCode::Char('d')) => {
                self.delete_cells(self.selected..self.selected + 1, rendered)
            }
            (Some('y'), KeyCode::Char('y')) => self.yank_cells(self.selected..self.selected + 1),
            (Some('m'), KeyCode::Char(c)) if c.is_ascii_lowercase() => {
                if let Some(id) = self.selected_id() {
                    self.marks.insert(c, id);
                    self.message = Some(format!("mark {c} set ('{c} jumps here)"));
                }
            }
            (Some('\'' | '`'), KeyCode::Char(c)) => match self.mark_cell(c) {
                Some(i) => {
                    self.mark_jump();
                    self.selected = i;
                }
                None => self.message = Some(format!("mark {c} not set")),
            },
            (Some('z'), KeyCode::Char('t')) => self.align = Some(Align::Top),
            (Some('z'), KeyCode::Char('z')) => self.align = Some(Align::Middle),
            (Some('z'), KeyCode::Char('b')) => self.align = Some(Align::Bottom),
            (Some('z'), KeyCode::Char('i')) => {
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
            (Some('g'), KeyCode::Char('g')) => {
                self.mark_jump();
                self.selected = count.map_or(0, |c| c.saturating_sub(1).min(last)) // 5gg like 5G
            }
            (Some(_), _) => {}
            (None, KeyCode::Char(c)) if c.is_ascii_digit() => {
                self.count = Some(count.unwrap_or(0) * 10 + (c as usize - '0' as usize));
            }
            (None, code) => match code {
                KeyCode::Char('q') => self.quit(),
                KeyCode::Char(':') => {
                    self.prompt = Some(Prompt {
                        kind: PromptKind::Command,
                        buf: String::new(),
                    })
                }
                KeyCode::Char('w') => {
                    self.save_notebook(false, rendered);
                }
                KeyCode::Char('W') => {
                    self.save_notebook(true, rendered);
                }
                KeyCode::Char('f') => {
                    self.message =
                        Some(self.format_cells(self.selected..self.selected + 1, rendered))
                }
                KeyCode::Char('F') => {
                    self.message = Some(self.format_cells(0..self.notebook.cells.len(), rendered))
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
                // the cell in $EDITOR (Ctrl+g, as in Claude Code and Codex)
                KeyCode::Char('g') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    return Action::ExternalEdit;
                }
                KeyCode::Char('g') => {
                    self.pending = Some('g');
                    self.count = count; // keep it for 5gg
                }
                // `G` = last cell, `5G` = cell 5 (1-based, like vim)
                KeyCode::Char('G') => {
                    self.mark_jump();
                    self.selected = count.map_or(last, |c| c.saturating_sub(1).min(last))
                }
                // jump list; terminals without the kitty protocol send Ctrl+i as Tab
                KeyCode::Char('o') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    self.jump_older()
                }
                KeyCode::Char('i') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    self.step_jump(1)
                }
                KeyCode::Tab => self.step_jump(1),
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
                KeyCode::Char('M') => self.merge_cells(self.selected..self.selected + 2, rendered),
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
                KeyCode::Char('p') => self.paste(self.selected + 1, rendered),
                KeyCode::Char('P') => self.paste(self.selected, rendered),
                KeyCode::Char('V') => self.visual = Some(self.selected),
                KeyCode::Char('J') => {
                    self.move_cells(self.selected..self.selected + 1, 1, rendered);
                }
                KeyCode::Char('K') => {
                    self.move_cells(self.selected..self.selected + 1, -1, rendered);
                }
                KeyCode::Char('t') => self.toggle_type(rendered),
                KeyCode::Char('m') => self.pending = Some('m'),
                KeyCode::Char('\'') => self.pending = Some('\''),
                KeyCode::Char('`') => self.pending = Some('`'),
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
                KeyCode::Char('z') => self.pending = Some('z'),
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
        // first edit: take the background spawn (waits if it's still loading)
        let started = self.nvim_spawn.take().map(|h| {
            h.join()
                .unwrap_or_else(|_| Err(anyhow::anyhow!("nvim spawn thread panicked")))
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
                        let half = if self.half_done.contains(m) {
                            ", half-done"
                        } else {
                            ""
                        };
                        format!("{state}{half} msg {m}")
                    })
                    .collect();
                out += &format!(
                    "exec: {}\n",
                    if runs.is_empty() {
                        "idle".into()
                    } else {
                        runs.join(" · ")
                    }
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
            "editor: {} · graphics: {} · keys: {}\n",
            match &self.editor {
                None => "closed".to_string(),
                Some(e) => format!(
                    "{} {:?} cursor {:?}",
                    if e.is_builtin() { "builtin" } else { "nvim" },
                    e.mode(),
                    e.cursor()
                ),
            },
            rendered.graphics_summary(),
            if self.keyboard_protocol {
                "kitty protocol"
            } else {
                "legacy (no Shift+Enter)"
            }
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
        self.touch_outputs();
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
                let mut any = false;
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
                        any = true;
                    }
                }
                if any {
                    self.touch_outputs();
                }
            }
            Event::Complete {
                parent,
                matches,
                start,
                types,
                ..
            } => self.on_complete_reply(parent, matches, start, types),
            Event::Inspect { parent, text } => self.on_inspect_reply(parent, text),
            Event::Output { parent, output } => {
                if let Some(idx) = self.target_cell(&parent) {
                    let cell = &mut self.notebook.cells[idx];
                    if self.pending_clear.remove(&parent) {
                        cell.outputs = Some(Vec::new());
                    }
                    cell.push_output(output);
                    rendered.rebuild_cell(idx, cell);
                    self.touch_outputs();
                }
            }
            Event::Clear { parent, wait } => {
                if wait {
                    self.pending_clear.insert(parent);
                } else if let Some(idx) = self.target_cell(&parent) {
                    let cell = &mut self.notebook.cells[idx];
                    cell.outputs = Some(Vec::new());
                    rendered.rebuild_cell(idx, cell);
                    self.touch_outputs();
                }
            }
            Event::ExecutionCount { parent, count } => {
                if let Some(idx) = self.target_cell(&parent) {
                    self.notebook.cells[idx]
                        .extra
                        .insert("execution_count".into(), count.into());
                    self.touch_outputs();
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
                // overlays take keys first: close them so typing reaches the box
                if self.zoom.take().is_some() {
                    self.message = None; // "zoom — any key closes"
                }
                self.show_help = false;
                self.debug = None;
                self.logs = None;
                self.pager = None;
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
        app.completion
            .as_ref()
            .unwrap()
            .items
            .iter()
            .map(|c| c.label())
            .collect()
    }

    pub(super) fn sources(app: &App) -> Vec<&str> {
        app.notebook
            .cells
            .iter()
            .map(|c| c.source.as_str())
            .collect()
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
        app.apply_kernel_event(
            Event::LaunchFailed {
                launch: 2,
                reason: "old".into(),
            },
            &mut r,
        );
        assert_eq!(app.deferred.len(), 1);
        app.apply_kernel_event(
            Event::LaunchFailed {
                launch: 3,
                reason: "boom".into(),
            },
            &mut r,
        );
        assert!(app.deferred.is_empty() && !app.kernel_starting);
        assert!(
            app.message
                .as_deref()
                .unwrap()
                .contains("1 queued run(s) dropped")
        );
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
        let disp = |v: &str| {
            serde_json::json!({"output_type": "display_data",
            "data": {"text/plain": v}, "metadata": {}, "transient": {"display_id": "d1"}})
        };
        app.notebook.cells[0].push_output(disp("old"));
        app.notebook.cells.push(app.notebook.cells[0].clone());
        r.rebuild_all(&app.notebook);
        app.apply_kernel_event(
            Event::UpdateDisplay {
                display_id: "d1".into(),
                output: disp("new"),
            },
            &mut r,
        );
        for c in &app.notebook.cells {
            assert_eq!(c.outputs.as_ref().unwrap()[0]["data"]["text/plain"], "new");
        }
        assert!(app.dirty);
    }

    #[test]
    fn input_request_closes_overlays_so_keys_reach_the_box() {
        let (mut app, mut r) = one_cell_app("input-overlay");
        app.show_help = true;
        app.logs = Some(0);
        let request = jupyter_protocol::messaging::JupyterMessage::new(
            jupyter_protocol::messaging::InputRequest::default(),
            None,
        );
        app.apply_kernel_event(
            Event::Input {
                request: Box::new(request),
                prompt: "name: ".into(),
                password: false,
            },
            &mut r,
        );
        assert!(!app.show_help && app.logs.is_none());
        app.on_key(
            KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE),
            &mut r,
        );
        assert_eq!(app.stdin_req.as_ref().unwrap().buf, "a");
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
            app.apply_kernel_event(
                Event::ExecutionCount {
                    parent: id.into(),
                    count: 1,
                },
                r,
            );
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
        assert_eq!(
            app.message.as_deref(),
            Some("no kernel — R to (re)start one")
        );
        app.on_key(key('a'), &mut r);
        app.on_key(key('u'), &mut r);
        assert_eq!(app.notebook.cells.len(), 1);
        app.on_key(
            KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL),
            &mut r,
        );
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

    /// A `.py` notebook: outputs live in memory only. They never dirty the
    /// buffer (there is nothing to save), the sidecar still keeps them for a
    /// crash, `w` writes percent without them, and `:w x.ipynb` has them.
    #[test]
    fn py_notebooks_keep_outputs_in_memory_only() {
        let dir = std::env::temp_dir().join(format!("jotter-py-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("nb.py");
        std::fs::write(&path, "# %%\nx = 1\n\n# %% [markdown]\n# hi\n").unwrap();
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut app = App::open(path.clone(), None, tx).unwrap();
        let mut r = Rendered::build(&app.notebook, None, Default::default());
        assert_eq!(sources(&app), ["x = 1", "hi"]);
        assert!(!app.stores_outputs());
        app.running.insert("m".into(), 0);
        app.apply_kernel_event(
            Event::Output {
                parent: "m".into(),
                output: stream("out\n"),
            },
            &mut r,
        );
        app.apply_kernel_event(
            Event::ExecutionCount {
                parent: "m".into(),
                count: 3,
            },
            &mut r,
        );
        assert!(!app.dirty, "outputs can't be saved to a .py");
        assert!(app.autosave_pending, "but the sidecar keeps them");
        app.maybe_autosave();
        let side = Notebook::read_ipynb(&app.autosave_path()).unwrap();
        assert_eq!(side.cells[0].outputs.as_ref().unwrap().len(), 1);
        // a clean quit drops the sidecar instead of offering the outputs back
        app.quit();
        assert!(app.should_quit && !app.autosave_path().exists());
        // clearing outputs isn't a change to the file either; a source edit is
        app.clear_outputs(0..1, &mut r);
        assert!(!app.dirty);
        app.undo(&mut r);
        assert!(!app.dirty);
        app.update_cell_source("x = 2".into(), &mut r);
        assert!(app.dirty);
        assert!(app.save_notebook(false, &mut r));
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "# %%\nx = 2\n\n# %% [markdown]\n# hi\n"
        );
        // save-as converts: the .ipynb gets the in-memory outputs
        let ipynb = dir.join("nb.ipynb");
        assert!(app.save_as(ipynb.to_str().unwrap(), &mut r));
        let saved = Notebook::read_ipynb(&ipynb).unwrap();
        assert_eq!(saved.cells[0].outputs.as_ref().unwrap().len(), 1);
        assert_eq!(saved.cells[0].execution_count(), Some(3));
        assert!(app.stores_outputs());
        // a new .py is a plain script until it has more than one cell
        let fresh = dir.join("new.py");
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut app = App::open(fresh.clone(), None, tx).unwrap();
        let mut r = Rendered::build(&app.notebook, None, Default::default());
        app.update_cell_source("print(1)".into(), &mut r);
        assert!(app.save_notebook(false, &mut r));
        assert_eq!(std::fs::read_to_string(&fresh).unwrap(), "print(1)\n");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn autosave_paths_live_in_the_state_dir_keyed_by_absolute_path() {
        let p = autosave_path_for(std::path::Path::new("/data/nb/a.ipynb"));
        assert!(
            p.ends_with("jotter/autosave/%data%nb%a.ipynb"),
            "{}",
            p.display()
        );
        let long = format!("/{}/a.ipynb", "d".repeat(400));
        let name = autosave_path_for(std::path::Path::new(&long));
        assert!(name.file_name().unwrap().len() <= 200);
    }

    #[test]
    fn execution_ends_on_reply_and_idle_in_either_order() {
        let (mut app, mut r) = one_cell_app("end");
        for (first, second) in [
            (
                Event::Done { parent: "m".into() },
                Event::Status {
                    parent: "m".into(),
                    busy: false,
                },
            ),
            (
                Event::Status {
                    parent: "m".into(),
                    busy: false,
                },
                Event::Done { parent: "m".into() },
            ),
        ] {
            app.notebook.cells[0].outputs = Some(Vec::new());
            app.running.insert("m".into(), 0);
            app.apply_kernel_event(first, &mut r);
            // output racing the reply (separate sockets) still lands
            app.apply_kernel_event(
                Event::Output {
                    parent: "m".into(),
                    output: stream("late\n"),
                },
                &mut r,
            );
            assert_eq!(app.notebook.cells[0].outputs.as_ref().unwrap().len(), 1);
            app.apply_kernel_event(second, &mut r);
            assert!(app.running.is_empty() && app.half_done.is_empty());
        }
        // idle of a request we never sent (kernel_info) is ignored
        app.apply_kernel_event(
            Event::Status {
                parent: "other".into(),
                busy: false,
            },
            &mut r,
        );
        assert!(app.half_done.is_empty());
    }

    #[test]
    fn non_code_cells_never_receive_kernel_output() {
        let (mut app, mut r) = one_cell_app("toggle");
        app.running.insert("m".into(), 0);
        app.toggle_type(&mut r); // code -> markdown mid-run
        app.apply_kernel_event(
            Event::ExecutionCount {
                parent: "m".into(),
                count: 3,
            },
            &mut r,
        );
        app.apply_kernel_event(
            Event::Output {
                parent: "m".into(),
                output: stream("x\n"),
            },
            &mut r,
        );
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
        rendered.relayout(80, &app.notebook);
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

    /// `:w` in the cell editor commits the cell and saves the file; after an
    /// external change it refuses with the `:w!` hint, and `:w!` overwrites.
    #[test]
    fn write_from_editor_saves_and_respects_disk_changes() {
        let path = std::env::temp_dir().join(format!("jotter-w-{}.ipynb", std::process::id()));
        let nb = serde_json::json!({
            "cells": [{"cell_type": "code", "metadata": {}, "execution_count": null,
                       "outputs": [], "source": ["a = 1"]}],
            "metadata": {}, "nbformat": 4, "nbformat_minor": 5
        });
        std::fs::write(&path, nb.to_string()).unwrap();
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut app = App::open(path.clone(), None, tx).unwrap();
        let mut rendered = Rendered::build(&app.notebook, None, Default::default());
        let on_disk = || Notebook::open(&path).unwrap().cells[0].source.clone();

        app.write_from_editor("a = 2".into(), false, &mut rendered);
        assert_eq!(on_disk(), "a = 2");
        assert!(!app.dirty);

        std::thread::sleep(std::time::Duration::from_millis(20)); // distinct mtime
        std::fs::write(&path, nb.to_string()).unwrap(); // someone else saved "a = 1"
        app.write_from_editor("a = 3".into(), false, &mut rendered);
        assert_eq!(on_disk(), "a = 1", "refused");
        assert!(app.message.as_deref().unwrap().contains(":w!"));
        assert_eq!(
            app.notebook.cells[0].source, "a = 3",
            "edit kept in the notebook"
        );

        app.write_from_editor("a = 3".into(), true, &mut rendered);
        assert_eq!(on_disk(), "a = 3");
        std::fs::remove_file(&path).ok();
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
            app.save_notebook(false, &mut rendered),
            "plain w overwrites after keep-mine"
        );
        std::fs::remove_file(&path).ok();
    }

    /// A paste lands in the cell being edited and nowhere else.
    #[test]
    fn paste_goes_into_the_editor_only() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut app = App::open(PathBuf::new(), None, tx).unwrap();
        let mut r = Rendered::build(&app.notebook, None, Default::default());
        let before = app.notebook.cells.len();
        app.on_paste("arst\n".into(), &mut r); // would insert and retype cells as keys
        assert_eq!(app.notebook.cells.len(), before);
        assert!(
            app.message
                .as_deref()
                .unwrap_or("")
                .contains("paste ignored")
        );

        app.on_key(
            KeyEvent::new(KeyCode::Char('A'), KeyModifiers::NONE),
            &mut r,
        );
        app.on_paste("x = 1\r\ny = 2".into(), &mut r);
        let esc = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        app.on_key(esc, &mut r);
        app.on_key(esc, &mut r);
        assert!(app.editor.is_none());
        assert_eq!(app.notebook.cells[app.selected].source, "x = 1\ny = 2");
    }

    /// `jotter` without a file: w has nowhere to write, `:w name` names it.
    #[test]
    fn scratchpad_saves_only_under_a_name() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut app = App::open(PathBuf::new(), None, tx).unwrap();
        let mut r = Rendered::build(&app.notebook, None, Default::default());
        assert!(app.is_scratch());
        assert!(!app.save_notebook(false, &mut r));
        let path =
            std::env::temp_dir().join(format!("jotter-scratch-{}.ipynb", std::process::id()));
        app.ex_command(&format!("w {}", path.display()), &mut r);
        assert_eq!(app.path, path, "{:?}", app.message);
        assert!(!app.dirty && path.exists());
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn command_line_saves_leaves_and_jumps() {
        let (mut app, mut r) = one_cell_app("cmdline");
        let path = app.path.clone();
        app.disk_mtime = None; // the helper deleted the file: not a disk change
        let type_cmd = |app: &mut App, r: &mut Rendered, cmd: &str| {
            app.on_key(key(':'), r);
            for c in cmd.chars() {
                app.on_key(key(c), r);
            }
            app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), r);
        };
        // builtin editor: `:` opens jOtter's command line; :w commits + saves
        app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &mut r);
        app.on_key(key('A'), &mut r);
        app.on_key(key('y'), &mut r);
        app.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &mut r);
        type_cmd(&mut app, &mut r, "w");
        assert!(app.editor.is_some(), ":w keeps editing");
        assert!(!app.dirty, "{:?}", app.message);
        assert!(std::fs::read_to_string(&path).unwrap().contains("xy"));
        type_cmd(&mut app, &mut r, "q");
        assert!(app.editor.is_none(), ":q leaves the cell");
        assert!(!app.should_quit);
        type_cmd(&mut app, &mut r, "bogus");
        assert!(app.message.as_deref().unwrap().contains("not a command"));
        type_cmd(&mut app, &mut r, "q");
        assert!(app.should_quit, ":q in notebook mode quits");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn gg_g_and_jump_list() {
        let (mut app, mut r) = one_cell_app("jumps");
        for _ in 0..4 {
            app.on_key(key('a'), &mut r); // 5 cells, selection follows the new one
        }
        app.selected = 2;
        let ctrl = |c| KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL);
        app.on_key(key('g'), &mut r);
        assert_eq!(app.selected, 2, "a lone g waits for its pair");
        app.on_key(key('g'), &mut r);
        assert_eq!(app.selected, 0);
        app.on_key(key('G'), &mut r);
        assert_eq!(app.selected, 4);
        app.on_key(ctrl('o'), &mut r);
        assert_eq!(app.selected, 0);
        app.on_key(ctrl('o'), &mut r);
        assert_eq!(app.selected, 2);
        app.on_key(ctrl('o'), &mut r);
        assert_eq!(app.selected, 2, "oldest entry: stays");
        app.on_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE), &mut r);
        assert_eq!(app.selected, 0, "Tab is Ctrl+i");
        app.on_key(ctrl('i'), &mut r);
        assert_eq!(app.selected, 4);
        // the list follows cells: delete cell 0 and it is skipped
        app.selected = 0;
        app.on_key(key('d'), &mut r);
        app.on_key(key('d'), &mut r);
        app.on_key(key('3'), &mut r);
        app.on_key(key('G'), &mut r); // from the (old) cell 1, now at 0
        assert_eq!(app.selected, 2);
        app.on_key(ctrl('o'), &mut r);
        assert_eq!(app.selected, 0);
    }

    #[test]
    fn visual_selection_acts_on_every_cell() {
        let (mut app, mut r) = one_cell_app("visual");
        for i in 1..5 {
            app.on_key(key('a'), &mut r);
            app.notebook.cells[i].source = format!("c{i}");
        }
        app.notebook.cells[0].source = "c0".into();
        let srcs = |app: &App| sources(app).join(",");
        app.selected = 1;
        for c in ['V', 'j', 'j'] {
            app.on_key(key(c), &mut r);
        }
        assert_eq!(app.sel_range(), 1..4);
        app.on_key(key('K'), &mut r); // block up, stays selected
        assert_eq!(srcs(&app), "c1,c2,c3,c0,c4");
        assert_eq!(app.sel_range(), 0..3);
        app.on_key(key('d'), &mut r);
        assert_eq!(srcs(&app), "c0,c4");
        assert!(app.visual.is_none(), "an operator ends the selection");
        app.on_key(key('p'), &mut r);
        assert_eq!(srcs(&app), "c0,c1,c2,c3,c4");
        assert_eq!(app.selected, 3, "on the last pasted cell");
        app.on_key(key('u'), &mut r);
        app.selected = 0;
        app.on_key(key('P'), &mut r); // before c0
        assert_eq!(srcs(&app), "c1,c2,c3,c0,c4");
        app.on_key(key('u'), &mut r);
        app.on_key(key('p'), &mut r);
        app.on_key(key('u'), &mut r);
        app.on_key(key('u'), &mut r);
        app.on_key(key('u'), &mut r);
        assert_eq!(srcs(&app), "c0,c1,c2,c3,c4", "one undo step each");
        app.selected = 3;
        for c in ['V', 'k', 'k', 'M'] {
            app.on_key(key(c), &mut r);
        }
        assert_eq!(srcs(&app), "c0,c1\nc2\nc3,c4");
        // any other key leaves visual mode without acting
        for c in ['V', 'j', 'o'] {
            app.on_key(key(c), &mut r);
        }
        assert!(app.visual.is_none());
    }

    #[test]
    fn marks_set_jump_and_follow_their_cell() {
        let (mut app, mut r) = one_cell_app("marks");
        for _ in 0..3 {
            app.on_key(key('a'), &mut r);
        }
        app.selected = 1;
        app.on_key(key('m'), &mut r);
        app.on_key(key('a'), &mut r);
        assert_eq!(
            app.notebook.cells.len(),
            4,
            "ma sets a mark, not a new cell"
        );
        app.selected = 3;
        app.on_key(key('\''), &mut r);
        app.on_key(key('a'), &mut r);
        assert_eq!(app.selected, 1);
        app.on_key(
            KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL),
            &mut r,
        );
        assert_eq!(app.selected, 3, "a mark jump is a jump");
        // the mark follows its cell; deleting the cell drops it, undo restores
        app.selected = 0;
        app.on_key(key('J'), &mut r);
        assert_eq!(app.mark_cell('a'), Some(0));
        app.selected = 0;
        for c in ['d', 'd'] {
            app.on_key(key(c), &mut r);
        }
        assert_eq!(app.mark_cell('a'), None);
        app.on_key(key('u'), &mut r);
        assert_eq!(app.mark_cell('a'), Some(0));
        app.on_key(key('`'), &mut r);
        app.on_key(key('b'), &mut r);
        assert!(app.message.as_deref().unwrap().contains("not set"));
        // t (not m) cycles the cell type now
        app.on_key(key('t'), &mut r);
        assert_eq!(app.notebook.cells[app.selected].cell_type, "markdown");
        app.on_key(key(':'), &mut r);
        for c in "delm a".chars() {
            app.on_key(key(c), &mut r);
        }
        app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &mut r);
        assert!(app.marks.is_empty());
    }
}
