use crate::editor::{Editor, Outcome};
use crate::kernel::{Event, Kernel};
use crate::notebook::{Cell, Notebook};
use crate::ui::Rendered;
use anyhow::Result;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::Rect;
use serde_json::{Map, Value};
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Instant;
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
    /// Prompt, output, or separator line.
    Other,
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
    pub editor: Option<Editor>,
    /// Line -> cell mapping of the last draw (mouse support).
    pub hit: Vec<Hit>,
    /// Notebook body area of the last draw.
    pub body: Rect,
    /// View was wheel-scrolled: draw must not snap back to the selection.
    pub manual_scroll: bool,
    last_click: Option<(Instant, u16, u16)>,
    confirm_quit: bool,
    /// First key of a two-key chord (dd).
    pending: Option<char>,
    yanked: Option<Cell>,
    kernel_name: String,
    events_tx: mpsc::UnboundedSender<Event>,
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
        Ok(Self {
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
            hit: Vec::new(),
            body: Rect::default(),
            manual_scroll: false,
            last_click: None,
            confirm_quit: false,
            pending: None,
            yanked: None,
            kernel_name,
            events_tx,
        })
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
        match mouse.kind {
            MouseEventKind::ScrollUp => {
                self.scroll = self.scroll.saturating_sub(3);
                self.manual_scroll = true;
            }
            MouseEventKind::ScrollDown => {
                self.scroll = (self.scroll + 3).min(self.hit.len().saturating_sub(1));
                self.manual_scroll = true;
            }
            MouseEventKind::Down(MouseButton::Left) => self.click(mouse.column, mouse.row, rendered),
            _ => {}
        }
    }

    fn click(&mut self, x: u16, y: u16, rendered: &mut Rendered) {
        let body = self.body;
        if y < body.y || y >= body.y + body.height || x < body.x {
            return;
        }
        let line = self.scroll + (y - body.y) as usize;
        let Some(&Hit { cell, kind }) = self.hit.get(line) else { return };
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
        if let HitKind::Source(row) = kind {
            let col = (x - body.x) as usize;
            match &mut self.editor {
                Some(editor) => editor.click(row, col, double),
                None => {
                    let Some(cell) = self.notebook.cells.get(self.selected) else { return };
                    let mut editor = Editor::new(&cell.source);
                    editor.click(row, col, double);
                    self.editor = Some(editor);
                }
            }
        }
    }

    pub fn on_key(&mut self, key: KeyEvent, rendered: &mut Rendered) -> Action {
        self.message = None;
        self.manual_scroll = false; // keyboard nav re-follows the selection
        if self.editor.is_some() {
            self.on_key_edit(key, rendered);
            return Action::None;
        }
        self.on_key_notebook(key, rendered)
    }

    fn on_key_edit(&mut self, key: KeyEvent, rendered: &mut Rendered) {
        let outcome = self.editor.as_mut().unwrap().input(key);
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
        let Some(editor) = self.editor.take() else { return };
        let cell = &mut self.notebook.cells[self.selected];
        let source = editor.source();
        if source != cell.source {
            cell.source = source;
            rendered.rebuild_cell(self.selected, cell);
            self.dirty = true;
        }
    }

    fn on_key_notebook(&mut self, key: KeyEvent, rendered: &mut Rendered) -> Action {
        if key.code != KeyCode::Char('q') {
            self.confirm_quit = false;
        }
        let pending = self.pending.take();
        let last = self.notebook.cells.len().saturating_sub(1);
        match (pending, key.code) {
            (Some('d'), KeyCode::Char('d')) => self.delete_cell(rendered),
            (Some(_), _) => {}
            (None, code) => match code {
                KeyCode::Char('q') => {
                    if self.dirty && !self.confirm_quit {
                        self.confirm_quit = true;
                        self.message = Some("unsaved changes — q again to quit".into());
                    } else {
                        self.should_quit = true;
                    }
                }
                KeyCode::Char('w') => {
                    self.message = Some(match self.notebook.save(&self.path) {
                        Ok(()) => {
                            self.dirty = false;
                            format!("saved {}", self.path.display())
                        }
                        Err(e) => format!("save failed: {e:#}"),
                    });
                }
                KeyCode::Char('j') | KeyCode::Down => self.selected = (self.selected + 1).min(last),
                KeyCode::Char('k') | KeyCode::Up => self.selected = self.selected.saturating_sub(1),
                KeyCode::Char('g') => self.selected = 0,
                KeyCode::Char('G') => self.selected = last,
                KeyCode::Char('d') => self.pending = Some('d'),
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
                    if key.code == KeyCode::Char('r')
                        || key.modifiers.contains(KeyModifiers::SHIFT)
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
                    self.spawn_kernel();
                }
                _ => {}
            },
        }
        Action::None
    }

    fn open_editor(&mut self, insert: bool) {
        let Some(cell) = self.notebook.cells.get(self.selected) else { return };
        let mut editor = Editor::new(&cell.source);
        if insert {
            let _ = editor.input(KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE));
        }
        self.editor = Some(editor);
    }

    /// Called by main after $EDITOR closed; applies the edited source.
    pub fn apply_external_edit(&mut self, source: String, rendered: &mut Rendered) {
        let Some(cell) = self.notebook.cells.get_mut(self.selected) else { return };
        // editors write a trailing newline; cell sources don't carry one
        let source = source.strip_suffix('\n').unwrap_or(&source).to_string();
        if source != cell.source {
            cell.source = source;
            rendered.rebuild_cell(self.selected, cell);
            self.dirty = true;
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
        self.dirty = true;
    }

    fn delete_cell(&mut self, rendered: &mut Rendered) {
        if self.notebook.cells.is_empty() {
            return;
        }
        let at = self.selected;
        self.yanked = Some(self.notebook.cells.remove(at));
        rendered.remove_cell(at);
        self.remap_running(|i| match i.cmp(&at) {
            std::cmp::Ordering::Less => Some(i),
            std::cmp::Ordering::Equal => None,
            std::cmp::Ordering::Greater => Some(i - 1),
        });
        self.selected = at.min(self.notebook.cells.len().saturating_sub(1));
        self.dirty = true;
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
        self.dirty = true;
    }

    fn toggle_type(&mut self, rendered: &mut Rendered) {
        let Some(cell) = self.notebook.cells.get_mut(self.selected) else { return };
        if cell.cell_type == "code" {
            cell.cell_type = "markdown".into();
            cell.outputs = None;
            cell.extra.remove("execution_count");
        } else {
            cell.cell_type = "code".into();
            cell.outputs = Some(Vec::new());
            cell.extra.insert("execution_count".into(), Value::Null);
        }
        rendered.rebuild_cell(self.selected, cell);
        self.dirty = true;
    }

    fn remap_running(&mut self, f: impl Fn(usize) -> Option<usize>) {
        self.running = self
            .running
            .drain()
            .filter_map(|(k, v)| f(v).map(|v| (k, v)))
            .collect();
    }

    fn run_cell(&mut self, idx: usize, rendered: &mut Rendered) {
        let Some(cell) = self.notebook.cells.get_mut(idx) else { return };
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
        self.dirty = true;
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
                    cell.outputs.get_or_insert_with(Vec::new).push(output);
                    rendered.rebuild_cell(idx, cell);
                    self.dirty = true;
                }
            }
            Event::ExecutionCount { parent, count } => {
                if let Some(&idx) = self.running.get(&parent) {
                    self.notebook.cells[idx]
                        .extra
                        .insert("execution_count".into(), count.into());
                    self.dirty = true;
                }
            }
            Event::Busy(b) => self.kernel_busy = b,
            Event::Done { parent } => {
                self.running.remove(&parent);
            }
            Event::Dead(reason) => {
                self.kernel = None;
                self.kernel_busy = false;
                self.running.clear();
                self.message = Some(format!("{reason} — R to restart"));
            }
            Event::Info(s) => self.message = Some(s),
        }
    }
}
