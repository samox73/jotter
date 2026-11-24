//! Mouse: wheel scrolling and clicks.

use super::*;

impl App {
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
        if let Some(h) = &mut self.help {
            match mouse.kind {
                MouseEventKind::ScrollUp => h.top = h.top.saturating_sub(3),
                MouseEventKind::ScrollDown => h.top += 3,
                _ => {}
            }
            return;
        }
        if self.zoom.is_some() || self.debug.is_some() || self.confirm.is_some() {
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
                let delta: isize = if mouse.kind == MouseEventKind::ScrollUp { -3 } else { 3 };
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
                            Some(&Hit { cell, kind: HitKind::Output })
                                if rendered
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
            HitKind::Source { line: row, col: col0 } => {
                // markdown renders rich when not edited: a single click only
                // selects; a double click opens the raw source at the line
                let is_md =
                    self.notebook.cells.get(cell).is_some_and(|c| c.cell_type == "markdown");
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
}
