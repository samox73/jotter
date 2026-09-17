//! Minimal modal (vi) cell editor over a Vec<String> buffer.
//! Covers the everyday subset; `E` opens the cell in $EDITOR for anything more.
//! ponytail: no operator+motion combos (dw, ci"), no visual mode, no counts —
//! the nvim escape hatch is the upgrade path.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

#[derive(PartialEq, Clone, Copy)]
pub enum Mode {
    Normal,
    Insert,
}

pub enum Outcome {
    Continue,
    /// Esc in normal mode: commit the buffer back to the cell.
    Exit,
    /// Shift+Enter / Ctrl+Enter: commit and run the cell (advance on `true`).
    Run { advance: bool },
}

pub struct Editor {
    pub lines: Vec<String>,
    /// (row, col) in characters.
    pub cursor: (usize, usize),
    pub mode: Mode,
    yank: Vec<String>,
    pending: Option<char>,
    undo: Vec<(Vec<String>, (usize, usize))>,
    redo: Vec<(Vec<String>, (usize, usize))>,
}

fn bidx(s: &str, ci: usize) -> usize {
    s.char_indices().nth(ci).map_or(s.len(), |(i, _)| i)
}
fn clen(s: &str) -> usize {
    s.chars().count()
}
fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

impl Editor {
    pub fn new(source: &str) -> Self {
        Self {
            lines: source.split('\n').map(String::from).collect(),
            cursor: (0, 0),
            mode: Mode::Normal,
            yank: Vec::new(),
            pending: None,
            undo: Vec::new(),
            redo: Vec::new(),
        }
    }

    pub fn source(&self) -> String {
        self.lines.join("\n")
    }

    /// `A` from cell-selection mode: cursor to last line's end, insert mode.
    pub fn append_end(&mut self) {
        self.snapshot();
        self.cursor.0 = self.lines.len() - 1;
        self.mode = Mode::Insert;
        self.cursor.1 = clen(self.line());
    }

    /// Mouse click: place the cursor; optionally switch to insert (double click).
    pub fn click(&mut self, row: usize, col: usize, insert: bool) {
        if insert && self.mode == Mode::Normal {
            self.snapshot();
            self.mode = Mode::Insert;
        }
        self.cursor.0 = row.min(self.lines.len() - 1);
        self.cursor.1 = col;
        self.clamp();
    }

    fn line(&self) -> &str {
        &self.lines[self.cursor.0]
    }

    /// Max cursor column on the current line for the current mode.
    fn max_col(&self) -> usize {
        let len = clen(self.line());
        match self.mode {
            Mode::Insert => len,
            Mode::Normal => len.saturating_sub(1),
        }
    }

    fn clamp(&mut self) {
        self.cursor.1 = self.cursor.1.min(self.max_col());
    }

    fn snapshot(&mut self) {
        self.undo.push((self.lines.clone(), self.cursor));
        if self.undo.len() > 100 {
            self.undo.remove(0);
        }
        self.redo.clear();
    }

    pub fn input(&mut self, key: KeyEvent) -> Outcome {
        // run bindings work in both modes
        if key.code == KeyCode::Enter
            && key
                .modifiers
                .intersects(KeyModifiers::SHIFT | KeyModifiers::CONTROL)
        {
            return Outcome::Run {
                advance: key.modifiers.contains(KeyModifiers::SHIFT),
            };
        }
        match self.mode {
            Mode::Insert => self.input_insert(key),
            Mode::Normal => return self.input_normal(key),
        }
        Outcome::Continue
    }

    fn input_insert(&mut self, key: KeyEvent) {
        let (row, col) = self.cursor;
        match key.code {
            KeyCode::Esc => {
                self.mode = Mode::Normal;
                self.cursor.1 = self.cursor.1.saturating_sub(1);
                self.clamp();
            }
            KeyCode::Enter => {
                let b = bidx(&self.lines[row], col);
                let rest = self.lines[row].split_off(b);
                self.lines.insert(row + 1, rest);
                self.cursor = (row + 1, 0);
            }
            KeyCode::Backspace => {
                if col > 0 {
                    let b = bidx(&self.lines[row], col - 1);
                    self.lines[row].remove(b);
                    self.cursor.1 -= 1;
                } else if row > 0 {
                    let cur = self.lines.remove(row);
                    let prev_len = clen(&self.lines[row - 1]);
                    self.lines[row - 1].push_str(&cur);
                    self.cursor = (row - 1, prev_len);
                }
            }
            KeyCode::Tab => {
                let b = bidx(&self.lines[row], col);
                self.lines[row].insert_str(b, "    ");
                self.cursor.1 += 4;
            }
            KeyCode::Char(c) => {
                let b = bidx(&self.lines[row], col);
                self.lines[row].insert(b, c);
                self.cursor.1 += 1;
            }
            KeyCode::Left => self.cursor.1 = col.saturating_sub(1),
            KeyCode::Right => self.cursor.1 = (col + 1).min(self.max_col()),
            KeyCode::Up | KeyCode::Down => self.move_row(key.code == KeyCode::Down),
            _ => {}
        }
    }

    fn move_row(&mut self, down: bool) {
        if down {
            self.cursor.0 = (self.cursor.0 + 1).min(self.lines.len() - 1);
        } else {
            self.cursor.0 = self.cursor.0.saturating_sub(1);
        }
        self.clamp();
    }

    fn input_normal(&mut self, key: KeyEvent) -> Outcome {
        let pending = self.pending.take();
        let (row, col) = self.cursor;
        match (pending, key.code) {
            (Some('d'), KeyCode::Char('d')) => {
                self.snapshot();
                self.yank = vec![self.lines[row].clone()];
                if self.lines.len() == 1 {
                    self.lines[0].clear();
                } else {
                    self.lines.remove(row);
                }
                self.cursor.0 = row.min(self.lines.len() - 1);
                self.clamp();
            }
            (Some('y'), KeyCode::Char('y')) => self.yank = vec![self.lines[row].clone()],
            (Some('g'), KeyCode::Char('g')) => {
                self.cursor = (0, 0);
            }
            (Some(_), _) => {} // unknown combo: swallow
            (None, code) => match code {
                KeyCode::Esc => return Outcome::Exit,
                KeyCode::Char('d') => self.pending = Some('d'),
                KeyCode::Char('y') => self.pending = Some('y'),
                KeyCode::Char('g') => self.pending = Some('g'),
                KeyCode::Char('h') | KeyCode::Left => self.cursor.1 = col.saturating_sub(1),
                KeyCode::Char('l') | KeyCode::Right => self.cursor.1 = (col + 1).min(self.max_col()),
                KeyCode::Char('j') | KeyCode::Down => self.move_row(true),
                KeyCode::Char('k') | KeyCode::Up => self.move_row(false),
                KeyCode::Char('0') => self.cursor.1 = 0,
                KeyCode::Char('$') => self.cursor.1 = self.max_col(),
                KeyCode::Char('^') => {
                    self.cursor.1 = self
                        .line()
                        .chars()
                        .position(|c| !c.is_whitespace())
                        .unwrap_or(0)
                }
                KeyCode::Char('G') => {
                    self.cursor.0 = self.lines.len() - 1;
                    self.clamp();
                }
                KeyCode::Char('w') => self.word_forward(),
                KeyCode::Char('b') => self.word_back(),
                KeyCode::Char('e') => self.word_end(),
                KeyCode::Char('i') => self.enter_insert(0),
                KeyCode::Char('a') => self.enter_insert(1),
                KeyCode::Char('I') => {
                    self.cursor.1 = 0;
                    self.enter_insert(0);
                }
                KeyCode::Char('A') => {
                    self.cursor.1 = clen(self.line());
                    self.snapshot();
                    self.mode = Mode::Insert;
                }
                KeyCode::Char('o') => {
                    self.snapshot();
                    self.lines.insert(row + 1, String::new());
                    self.cursor = (row + 1, 0);
                    self.mode = Mode::Insert;
                }
                KeyCode::Char('O') => {
                    self.snapshot();
                    self.lines.insert(row, String::new());
                    self.cursor = (row, 0);
                    self.mode = Mode::Insert;
                }
                KeyCode::Char('x') => {
                    if !self.line().is_empty() {
                        self.snapshot();
                        let b = bidx(&self.lines[row], col);
                        self.lines[row].remove(b);
                        self.clamp();
                    }
                }
                KeyCode::Char('D') => {
                    self.snapshot();
                    let b = bidx(&self.lines[row], col);
                    self.lines[row].truncate(b);
                    self.clamp();
                }
                KeyCode::Char('p') => {
                    if !self.yank.is_empty() {
                        self.snapshot();
                        for (i, l) in self.yank.clone().into_iter().enumerate() {
                            self.lines.insert(row + 1 + i, l);
                        }
                        self.cursor = (row + 1, 0);
                    }
                }
                KeyCode::Char('P') => {
                    if !self.yank.is_empty() {
                        self.snapshot();
                        for (i, l) in self.yank.clone().into_iter().enumerate() {
                            self.lines.insert(row + i, l);
                        }
                        self.cursor = (row, 0);
                    }
                }
                KeyCode::Char('u') => {
                    if let Some((lines, cursor)) = self.undo.pop() {
                        self.redo.push((self.lines.clone(), self.cursor));
                        self.lines = lines;
                        self.cursor = cursor;
                    }
                }
                KeyCode::Char('r') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    if let Some((lines, cursor)) = self.redo.pop() {
                        self.undo.push((self.lines.clone(), self.cursor));
                        self.lines = lines;
                        self.cursor = cursor;
                    }
                }
                _ => {}
            },
        }
        Outcome::Continue
    }

    fn enter_insert(&mut self, offset: usize) {
        self.snapshot();
        self.cursor.1 = (self.cursor.1 + offset).min(clen(self.line()));
        self.mode = Mode::Insert;
    }

    fn word_forward(&mut self) {
        let (mut row, mut col) = self.cursor;
        let chars: Vec<char> = self.lines[row].chars().collect();
        // skip current word/punct run, then whitespace
        let mut i = col;
        if i < chars.len() {
            let w = is_word(chars[i]);
            while i < chars.len() && is_word(chars[i]) == w && !chars[i].is_whitespace() {
                i += 1;
            }
        }
        while i < chars.len() && chars[i].is_whitespace() {
            i += 1;
        }
        if i >= chars.len() && row + 1 < self.lines.len() {
            row += 1;
            col = self.lines[row]
                .chars()
                .position(|c| !c.is_whitespace())
                .unwrap_or(0);
        } else {
            col = i;
        }
        self.cursor = (row, col);
        self.clamp();
    }

    fn word_back(&mut self) {
        let (mut row, col) = self.cursor;
        if col == 0 {
            if row > 0 {
                row -= 1;
                self.cursor = (row, clen(&self.lines[row]).saturating_sub(1));
            }
            self.clamp();
            return;
        }
        let chars: Vec<char> = self.lines[row].chars().collect();
        let mut i = col - 1;
        while i > 0 && chars[i].is_whitespace() {
            i -= 1;
        }
        let w = is_word(chars[i]);
        while i > 0 && is_word(chars[i - 1]) == w && !chars[i - 1].is_whitespace() {
            i -= 1;
        }
        self.cursor = (row, i);
    }

    fn word_end(&mut self) {
        let (row, col) = self.cursor;
        let chars: Vec<char> = self.lines[row].chars().collect();
        let mut i = (col + 1).min(chars.len());
        while i < chars.len() && chars[i].is_whitespace() {
            i += 1;
        }
        if i < chars.len() {
            let w = is_word(chars[i]);
            while i + 1 < chars.len() && is_word(chars[i + 1]) == w && !chars[i + 1].is_whitespace()
            {
                i += 1;
            }
            self.cursor = (row, i);
        } else {
            self.cursor.1 = self.max_col();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyModifiers;

    fn key(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
    }
    fn code(c: KeyCode) -> KeyEvent {
        KeyEvent::new(c, KeyModifiers::NONE)
    }

    #[test]
    fn insert_and_exit() {
        let mut e = Editor::new("ab");
        e.input(key('i'));
        e.input(key('x'));
        e.input(code(KeyCode::Esc));
        assert_eq!(e.source(), "xab");
        assert!(matches!(e.input(code(KeyCode::Esc)), Outcome::Exit));
    }

    #[test]
    fn dd_p_undo() {
        let mut e = Editor::new("one\ntwo\nthree");
        e.input(key('j'));
        e.input(key('d'));
        e.input(key('d')); // delete "two"
        assert_eq!(e.source(), "one\nthree");
        e.input(key('p')); // paste below
        assert_eq!(e.source(), "one\nthree\ntwo");
        e.input(key('u'));
        e.input(key('u'));
        assert_eq!(e.source(), "one\ntwo\nthree");
    }

    #[test]
    fn open_line_and_words() {
        let mut e = Editor::new("foo bar");
        e.input(key('w'));
        assert_eq!(e.cursor, (0, 4));
        e.input(key('o'));
        e.input(key('z'));
        e.input(code(KeyCode::Esc));
        assert_eq!(e.source(), "foo bar\nz");
    }

    #[test]
    fn shift_enter_runs() {
        let mut e = Editor::new("x");
        let ev = KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT);
        assert!(matches!(e.input(ev), Outcome::Run { advance: true }));
    }
}
