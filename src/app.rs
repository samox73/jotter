use crate::kernel::{Event, Kernel};
use crate::notebook::Notebook;
use crate::ui::Rendered;
use anyhow::Result;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use std::collections::HashMap;
use std::path::PathBuf;
use tokio::sync::mpsc;

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
    /// In-flight executions: msg_id -> cell index.
    /// ponytail: index-keyed; M2 cell reordering must remap or key by cell id
    pub running: HashMap<String, usize>,
    pub dirty: bool,
    confirm_quit: bool,
    kernel_name: String,
    events_tx: mpsc::UnboundedSender<Event>,
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
            confirm_quit: false,
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

    pub fn on_key(&mut self, key: KeyEvent, rendered: &mut Rendered) {
        self.message = None;
        if key.code != KeyCode::Char('q') {
            self.confirm_quit = false;
        }
        let last = self.notebook.cells.len().saturating_sub(1);
        match key.code {
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
            // Enter/Shift+Enter: run + advance; Ctrl+Enter: run in place.
            // (Enter becomes "edit cell" in M2.)
            KeyCode::Enter => {
                self.run_cell(self.selected, rendered);
                if !key.modifiers.contains(KeyModifiers::CONTROL) {
                    self.selected = (self.selected + 1).min(last);
                }
            }
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
        }
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
