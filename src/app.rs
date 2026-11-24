use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    time::{Instant, SystemTime},
};

use anyhow::Result;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use jupyter_protocol::messaging::JupyterMessage;
use ratatui::layout::Rect;
use serde_json::{Map, Value};
use tokio::sync::mpsc;

use crate::{
    editor::{Editor, Outcome},
    kernel::{Event, Kernel},
    notebook::{Cell, Notebook, new_cell_id},
    nvim::{Backend, ModeKind, NvimCell, NvimSession},
    ui::Rendered,
};

mod cellops;
mod completion;
mod edit;
mod exec;
mod keys;
mod mouse;
mod persist;
mod signature;
#[cfg(test)]
pub(super) mod tests;

use cellops::CellOp;
pub use completion::Completion;
use persist::{expand_tilde, mtime};
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
/// The notebook's kernel isn't installed: which one to start instead?
/// Typing filters `kernels`; Enter starts the selected match.
#[derive(Debug, PartialEq)]
pub struct KernelPick {
    pub wanted: String,
    kernels: Vec<String>,
    pub filter: String,
    pub selected: usize,
}

impl KernelPick {
    /// Kernels whose name contains the filter, ignoring case.
    pub fn shown(&self) -> Vec<&String> {
        let f = self.filter.to_lowercase();
        self.kernels.iter().filter(|k| k.to_lowercase().contains(&f)).collect()
    }
}

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

/// `?`: the key reference, scrollable and filterable.
#[derive(Default)]
pub struct Help {
    /// Entries must contain this (case-insensitive); empty shows all.
    pub filter: String,
    /// `/` was pressed: keys edit the filter.
    pub typing: bool,
    /// First visible row; clamped by the draw.
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
    pub help: Option<Help>,
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
    pub kernel_pick: Option<KernelPick>,
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

/// Copy `text` to the system clipboard via OSC 52 (works over ssh/tmux).
fn osc52(text: &str) {
    if cfg!(test) {
        return; // keep escape sequences out of test output
    }
    use std::io::Write;

    use base64::Engine;
    let b64 = base64::engine::general_purpose::STANDARD.encode(text.as_bytes());
    let mut out = std::io::stdout();
    let _ = write!(out, "\x1b]52;c;{b64}\x07");
    let _ = out.flush();
}

/// Jump list depth (vim's is 100).
const JUMPS_CAP: usize = 100;

impl App {
    pub fn open(
        path: PathBuf,
        kernel_override: Option<String>,
        events_tx: mpsc::UnboundedSender<Event>,
    ) -> Result<Self> {
        let is_new = !path.exists();
        let notebook = if is_new { Notebook::new_empty() } else { Notebook::open(&path)? };
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
            help: None,
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
            kernel_pick: None,
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

    /// The cells an operation acts on: the `V` selection, else the selected one.
    pub fn sel_range(&self) -> std::ops::Range<usize> {
        let a = self.visual.unwrap_or(self.selected);
        a.min(self.selected)..a.max(self.selected) + 1
    }

    /// `D`: everything about the selected cell that helps pin down a bug
    /// report — identity, model, render/viewport state, execution, kernel.
    fn debug_report(&self, rendered: &Rendered) -> String {
        let mut out = format!("jotter {}\n", env!("CARGO_PKG_VERSION"));
        match self.notebook.cells.get(self.selected) {
            Some(cell) => {
                let id = cell.extra.get("id").and_then(Value::as_str).unwrap_or("-");
                let count = cell.execution_count().map_or("-".to_string(), |n| n.to_string());
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
                        let state = if self.started.contains_key(m) { "running" } else { "queued" };
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
            if self.keyboard_protocol { "kitty protocol" } else { "legacy (no Shift+Enter)" }
        );
        out.trim_end().to_string()
    }
}
