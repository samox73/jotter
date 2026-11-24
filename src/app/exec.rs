//! Kernel lifecycle and executions: starting, restarting, running cells, applying kernel events.

use super::*;

impl App {
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

    /// A launch came to nothing: say why and drop the runs waiting for it.
    fn no_kernel(&mut self, reason: String) {
        self.kernel_starting = false;
        let dropped = std::mem::take(&mut self.deferred).len();
        self.message = Some(if dropped > 0 {
            format!("{reason} — {dropped} queued run(s) dropped")
        } else {
            reason
        });
    }

    /// Kill the kernel and start a fresh one; `run_all` queues every code
    /// cell to run once it is ready.
    pub(super) fn restart_kernel(&mut self, run_all: bool) {
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

    /// Keys while the kernel waits on `input()`: typed into the status line.
    pub(super) fn on_key_kernel_pick(&mut self, key: KeyEvent) {
        let Some(pick) = &mut self.kernel_pick else {
            return;
        };
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let last = pick.shown().len().saturating_sub(1);
        match key.code {
            KeyCode::Esc => {
                self.kernel_pick = None;
                self.no_kernel("no kernel — R to pick one".into());
            }
            KeyCode::Enter => {
                let Some(name) = pick.shown().get(pick.selected).map(|k| k.to_string()) else {
                    return; // nothing matches the filter
                };
                self.kernel_pick = None;
                // as in jupyterlab, the choice replaces the notebook's
                // kernelspec: Ready records the new one for the next w
                if let Some(meta) = self.notebook.extra.get_mut("metadata")
                    && let Some(meta) = meta.as_object_mut()
                {
                    meta.remove("kernelspec");
                }
                self.kernel_name = name;
                self.spawn_kernel();
            }
            KeyCode::Down | KeyCode::Tab => pick.selected = (pick.selected + 1).min(last),
            KeyCode::Char('n') if ctrl => pick.selected = (pick.selected + 1).min(last),
            KeyCode::Up | KeyCode::BackTab => pick.selected = pick.selected.saturating_sub(1),
            KeyCode::Char('p') if ctrl => pick.selected = pick.selected.saturating_sub(1),
            KeyCode::Backspace => {
                pick.filter.pop();
                pick.selected = 0;
            }
            KeyCode::Char(c) if !ctrl => {
                pick.filter.push(c);
                pick.selected = 0;
            }
            _ => {}
        }
    }

    pub(super) fn on_key_stdin(&mut self, key: KeyEvent) {
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
                    let value = if key.code == KeyCode::Enter { req.buf } else { String::new() };
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

    pub(super) fn run_range(&mut self, range: std::ops::Range<usize>, rendered: &mut Rendered) {
        self.follow = range.len() > 1;
        for i in range {
            self.run_cell(i, rendered);
        }
    }

    /// Drop all in-flight execution bookkeeping (restart, death, reload).
    pub(super) fn forget_executions(&mut self) {
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

    pub(super) fn remap_running(&mut self, f: impl Fn(usize) -> Option<usize>) {
        self.running = self.running.drain().filter_map(|(k, v)| f(v).map(|v| (k, v))).collect();
    }

    pub(super) fn run_cell(&mut self, idx: usize, rendered: &mut Rendered) {
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
                self.message =
                    Some(format!("kernel starting — {} cell(s) queued", self.deferred.len()));
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
                self.message = Some(format!("kernel '{}' ready ({origin})", kernel.name));
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
            Event::LaunchFailed { reason, .. } => self.no_kernel(reason),
            Event::NotFound { launch, .. } if launch != self.launch => {}
            Event::NotFound { wanted, mut kernels, .. } => {
                self.kernel_starting = false; // runs queued so far wait for the pick
                if kernels.is_empty() {
                    self.no_kernel(format!(
                        "kernel '{wanted}' not found, and no other is installed — pip install ipykernel in your venv"
                    ));
                } else {
                    // only kernels for the notebook's language, if there are any
                    let lang = crate::ui::notebook_language(&self.notebook);
                    if kernels.iter().any(|(_, l)| l.eq_ignore_ascii_case(&lang)) {
                        kernels.retain(|(_, l)| l.eq_ignore_ascii_case(&lang));
                    }
                    self.message = Some(format!("kernel '{wanted}' not found"));
                    self.kernel_pick = Some(KernelPick {
                        wanted,
                        kernels: kernels.into_iter().map(|(n, _)| n).collect(),
                        filter: String::new(),
                        selected: 0,
                    });
                }
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
            Event::Complete { parent, matches, start, types, .. } => {
                self.on_complete_reply(parent, matches, start, types)
            }
            Event::Inspect { parent, text } => self.on_inspect_reply(parent, text),
            Event::LanguageInfo(info) => {
                // as jupyter writes it; saved with the next w
                if let Some(name) = info["name"].as_str() {
                    rendered.set_language(name);
                }
                if let Some(meta) = self
                    .notebook
                    .extra
                    .entry("metadata")
                    .or_insert_with(|| Value::Object(Map::new()))
                    .as_object_mut()
                {
                    meta.insert("language_info".into(), info);
                }
            }
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
                    self.notebook.cells[idx].extra.insert("execution_count".into(), count.into());
                    self.touch_outputs();
                    self.started.insert(parent, Instant::now()); // no longer queued
                    if self.follow {
                        self.selected = idx;
                        self.manual_scroll = false;
                    }
                }
            }
            Event::Input { request, prompt, password } => {
                // overlays take keys first: close them so typing reaches the box
                if self.zoom.take().is_some() {
                    self.message = None; // "zoom — any key closes"
                }
                self.help = None;
                self.debug = None;
                self.logs = None;
                self.pager = None;
                self.stdin_req = Some(StdinReq { prompt, buf: String::new(), password, request });
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
