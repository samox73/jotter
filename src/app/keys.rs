//! Key dispatch: overlays, prompts, ex commands and the notebook keymap.

use super::*;

impl App {
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
        if let Some(help) = &mut self.help {
            let page = (self.body.height as usize / 2).max(1);
            let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
            match key.code {
                // `/`: type a filter; Enter keeps it, Esc drops it
                KeyCode::Enter if help.typing => help.typing = false,
                KeyCode::Esc if help.typing || !help.filter.is_empty() => {
                    *help = Help::default();
                }
                KeyCode::Backspace if help.typing => {
                    help.filter.pop();
                }
                KeyCode::Char(c) if help.typing && !ctrl => {
                    help.filter.push(c);
                    help.top = 0;
                }
                KeyCode::Char('/') => help.typing = true,
                KeyCode::Char('j') | KeyCode::Down => help.top += 1,
                KeyCode::Char('k') | KeyCode::Up => help.top = help.top.saturating_sub(1),
                KeyCode::Char('d') if ctrl => help.top += page,
                KeyCode::Char('u') if ctrl => help.top = help.top.saturating_sub(page),
                KeyCode::PageDown | KeyCode::Char(' ') => help.top += page,
                KeyCode::PageUp => help.top = help.top.saturating_sub(page),
                KeyCode::Char('g') | KeyCode::Home => help.top = 0,
                KeyCode::Char('G') | KeyCode::End => help.top = usize::MAX,
                _ => self.help = None,
            }
            return Action::None;
        }
        if self.debug.is_some() || self.zoom.is_some() {
            self.debug = None;
            self.zoom = None;
            return Action::None;
        }
        if self.stdin_req.is_some() {
            self.on_key_stdin(key);
            return Action::None;
        }
        if self.kernel_pick.is_some() {
            self.on_key_kernel_pick(key);
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
    pub(super) fn ex_command(&mut self, cmd: &str, rendered: &mut Rendered) {
        let cmd = cmd.trim();
        let (name, arg) =
            cmd.split_once(char::is_whitespace).map_or((cmd, ""), |(n, a)| (n, a.trim()));
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

    /// `q`: quit, asking first when there are unsaved changes.
    pub(super) fn quit(&mut self) {
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
            if self.notebook.cells.get(i).is_some_and(|c| c.source.to_lowercase().contains(&q)) {
                self.mark_jump();
                self.selected = i;
                return;
            }
        }
        self.message = Some(format!("no match: {q}"));
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
                    self.prompt = Some(Prompt { kind: PromptKind::Command, buf: String::new() })
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
                    self.prompt = Some(Prompt { kind: PromptKind::Search, buf: String::new() })
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
                KeyCode::Char('?') => self.help = Some(Help::default()),
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
}
