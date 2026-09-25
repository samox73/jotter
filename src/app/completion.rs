//! Kernel completion and docs inside the cell editor: the popup's
//! candidates, filtering as you type, auto-open after `.`, the jedi retry,
//! the notebook's own identifiers, lazy kind/signature lookups, and the
//! Shift+Tab docs pager.

use super::*;

/// One completion candidate.
#[derive(Clone, Debug, PartialEq)]
pub struct Candidate {
    /// Replacement text (the live completer returns attributes as `.name`).
    pub text: String,
    /// IPython's kind (`function`, `module`, `instance`, `keyword`, ...),
    /// `word` for notebook identifiers; refined when resolved.
    pub kind: String,
    /// Signature (callables) or type name; empty until known.
    pub detail: String,
    /// A detail lookup was sent (or none is needed).
    pub(super) resolved: bool,
}

impl Candidate {
    pub(super) fn new(text: String, kind: &str, detail: &str) -> Self {
        let resolved = !detail.is_empty() || kind == "word" || kind == "keyword";
        Candidate {
            text,
            kind: kind.into(),
            detail: detail.into(),
            resolved,
        }
    }

    /// What the popup shows: the name without the live completer's `.`.
    pub fn label(&self) -> &str {
        self.text.trim_start_matches('.')
    }
}

/// Tab completion: requested, then (with several matches) an open popup.
pub struct Completion {
    /// complete_request msg_id; replies for other ids are stale.
    pub(super) request: String,
    /// Editor text at request time: the text before `start` must still
    /// match when the reply lands (typing after it is fine: it filters).
    pub(super) source: String,
    /// Every match; `items` is the part matching what was typed since
    /// `start`.
    pub(super) all: Vec<Candidate>,
    pub items: Vec<Candidate>,
    pub sel: usize,
    /// First visible popup row (moves only when the selection leaves the
    /// window; kept by the draw).
    pub top: usize,
    /// In-flight detail lookups (inspect msg_id -> candidate text).
    pub(super) detail_reqs: HashMap<String, String>,
    /// Char range of the source the chosen item replaces (`end` tracks the
    /// cursor while typing filters).
    pub(super) start: usize,
    pub(super) end: usize,
    /// Tab (a single match applies directly) vs auto-open on `.` (never
    /// inserts on its own).
    pub(super) explicit: bool,
    /// Request time, for the latency in the debug log.
    pub(super) asked: Instant,
    /// Cursor offset of the request (a jedi retry re-asks at the same spot).
    pub(super) at: usize,
    /// A reply arrived (an empty `all` then means "nothing matched").
    pub(super) replied: bool,
    /// Already retried with jedi after the live completer found nothing.
    pub(super) jedi_retry: bool,
}

impl Completion {
    #[cfg(test)]
    pub fn for_test(start: usize, end: usize) -> Self {
        Completion {
            request: String::new(),
            source: String::new(),
            all: Vec::new(),
            items: Vec::new(),
            sel: 0,
            top: 0,
            detail_reqs: HashMap::new(),
            start,
            end,
            explicit: false,
            asked: Instant::now(),
            at: end,
            replied: true,
            jedi_retry: true,
        }
    }

    #[cfg(test)]
    pub fn push_for_test(&mut self, text: &str, kind: &str, detail: &str) {
        self.items.push(Candidate::new(text.into(), kind, detail));
    }

    /// Char range the chosen candidate replaces.
    pub fn span(&self) -> (usize, usize) {
        (self.start, self.end)
    }
}

/// `source` with `text` replacing chars [start, end), and the char offset
/// right after it — where to inspect a candidate as if it were accepted.
pub(super) fn splice(source: &str, start: usize, end: usize, text: &str) -> (String, usize) {
    let head: String = source.chars().take(start).collect();
    let tail: String = source.chars().skip(end.max(start)).collect();
    (format!("{head}{text}{tail}"), start + text.chars().count())
}

/// (kind, detail) from IPython inspect text (ANSI-colored sections like
/// `Signature:`, `Type:`): the signature from its `(` for callables, else
/// the type name.
pub fn parse_inspect(text: &str) -> (String, String) {
    let mut plain = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            for n in chars.by_ref() {
                if n.is_ascii_alphabetic() {
                    break; // end of the CSI sequence
                }
            }
        } else {
            plain.push(c);
        }
    }
    let mut sections: Vec<(String, String)> = Vec::new();
    for line in plain.lines() {
        let header = line
            .split_once(':')
            .filter(|(h, _)| h.starts_with(|c: char| c.is_ascii_uppercase()))
            .filter(|(h, _)| h.chars().all(|c| c.is_ascii_alphabetic() || c == ' '));
        match (header, sections.last_mut()) {
            (Some((h, rest)), _) => sections.push((h.to_string(), rest.trim().to_string())),
            (None, Some((_, body))) => {
                body.push(' ');
                body.push_str(line.trim());
            }
            (None, None) => {}
        }
    }
    let get = |name: &str| {
        sections
            .iter()
            .find(|(h, _)| h == name)
            .map(|(_, b)| b.split_whitespace().collect::<Vec<_>>().join(" "))
    };
    let ty = get("Type").unwrap_or_default();
    let kind = if ty.contains("function") || ty.contains("method") {
        "function"
    } else if ty == "module" {
        "module"
    } else if ty == "type" || ty.ends_with("Meta") {
        "class"
    } else if ty.is_empty() {
        ""
    } else {
        "instance"
    };
    let signature = ["Signature", "Init signature", "Call signature"]
        .iter()
        .find_map(|h| get(h))
        .and_then(|s| {
            let sig = &s[s.find('(')?..];
            Some(sig.replace("( ", "(").replace(" )", ")").replace(",)", ")"))
        });
    (kind.into(), signature.unwrap_or(ty))
}

/// Identifiers in Python `text` (2+ chars, not starting with a digit),
/// skipping string literals and `#` comments: the notebook's own names,
/// offered alongside the kernel's — they exist before the defining cell ever
/// ran (`np` right after typing `import numpy as np`). ponytail: per-line
/// string tracking; words inside multi-line triple-quoted strings leak in.
pub(super) fn identifiers(text: &str) -> Vec<String> {
    let mut words = Vec::new();
    for line in text.lines() {
        let mut code = String::with_capacity(line.len());
        let mut quote: Option<char> = None;
        let mut chars = line.chars();
        while let Some(c) = chars.next() {
            match (quote, c) {
                (Some(_), '\\') => {
                    chars.next();
                }
                (Some(q), c) if c == q => quote = None,
                (Some(_), _) => {}
                (None, '\'' | '"') => {
                    quote = Some(c);
                    code.push(' ');
                }
                (None, '#') => break,
                (None, c) => code.push(c),
            }
        }
        words.extend(
            code.split(|c: char| !is_ident(c))
                .filter(|w| w.chars().count() >= 2 && !w.starts_with(|c: char| c.is_ascii_digit()))
                .map(String::from),
        );
    }
    words
}

/// Python identifier char (what keeps a completion popup filtering).
pub(super) fn is_ident(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// `line` (text before the cursor) just got a `.` that should open
/// completion: after a name, `)` or `]` (`x.`, `f().`, `a[0].`), not after a
/// number (`1.`), another dot, or inside a string/comment.
pub(super) fn wants_dot_completion(line: &str) -> bool {
    let Some(head) = line.strip_suffix('.') else {
        return false; // the key was remapped (nvim) or didn't insert
    };
    let name: String = head.chars().rev().take_while(|&c| is_ident(c)).collect();
    // `name` is reversed: its last char is the token's first
    let after_name = name.chars().last().is_some_and(|c| !c.is_ascii_digit());
    let after_call = head.ends_with(')') || head.ends_with(']');
    (after_name || after_call) && !in_string_or_comment(head)
}

/// Whether the end of `line` sits inside a string literal or a `#` comment
/// (Python rules, single-line approximation: triple-quoted strings spanning
/// lines are not tracked).
pub(super) fn in_string_or_comment(line: &str) -> bool {
    let mut quote: Option<char> = None;
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        match (quote, c) {
            (Some(_), '\\') => {
                chars.next();
            }
            (Some(q), c) if c == q => quote = None,
            (None, '\'' | '"') => quote = Some(c),
            (None, '#') => return true,
            _ => {}
        }
    }
    quote.is_some()
}

impl App {
    /// Completion/inspection keys inside the editor; true = consumed.
    /// With the popup open: Tab/↑↓/Ctrl+n/p pick, Enter accepts, Esc closes;
    /// name chars and Backspace go to the editor and the popup filters on
    /// (see `after_edit_key`); anything else closes it and is handled
    /// normally. Tab after a name char asks the kernel; Shift+Tab: docs.
    pub(super) fn on_key_completion(&mut self, key: KeyEvent) -> bool {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if let Some(c) = &mut self.completion {
            let n = c.items.len();
            match key.code {
                KeyCode::Tab | KeyCode::Down if n > 0 => c.sel = (c.sel + 1) % n,
                KeyCode::Char('n') if ctrl && n > 0 => c.sel = (c.sel + 1) % n,
                KeyCode::Up if n > 0 => c.sel = (c.sel + n - 1) % n,
                KeyCode::Char('p') if ctrl && n > 0 => c.sel = (c.sel + n - 1) % n,
                // Shift+Tab is always docs: here, for the highlighted candidate
                KeyCode::BackTab if n > 0 => {
                    if let Some((code, pos)) = self.accepted_code(self.completion.as_ref().unwrap().sel) {
                        self.request_docs(code, pos);
                    }
                    return true;
                }
                KeyCode::Enter if key.modifiers.is_empty() && n > 0 => {
                    let c = self.completion.take().unwrap();
                    self.accept_completion(c.start, c.end, &c.items[c.sel].text);
                    return true;
                }
                KeyCode::Esc if n > 0 => self.completion = None,
                KeyCode::Char(ch) if is_ident(ch) && !ctrl => return false, // filters on
                KeyCode::Backspace => return false,
                KeyCode::Tab => {} // pending request: ask again below
                _ => {
                    self.completion = None;
                    return false;
                }
            }
            if !(key.code == KeyCode::Tab && n == 0) {
                self.resolve_visible();
                return true;
            }
        }
        let Some(editor) = &self.editor else { return false };
        let source = editor.source();
        let offset = char_offset(&source, editor.cursor());
        match key.code {
            KeyCode::Tab if key.modifiers.is_empty() && editor.mode() == ModeKind::Insert => {
                let before = source.chars().nth(offset.wrapping_sub(1));
                if !before.is_some_and(|c| is_ident(c) || ".[\"'/".contains(c)) {
                    return false; // indentation
                }
                self.request_completion(source, offset, true);
                true
            }
            KeyCode::BackTab => {
                self.request_docs(source, offset);
                true
            }
            _ => false,
        }
    }

    /// Shift+Tab: kernel docs for the name at `pos` of `code`, into the pager.
    pub(super) fn request_docs(&mut self, code: String, pos: usize) {
        match &self.kernel {
            Some(kernel) => self.pending_inspect = Some(kernel.inspect(code, pos)),
            None => self.message = Some("no kernel — docs need one".into()),
        }
    }

    /// The cell's *current* text with candidate `i` spliced in over the
    /// typed token, and the offset right after it: what the kernel inspects
    /// for that candidate. (The request-time snapshot would be wrong once
    /// the user typed on: `start..end` are live coordinates.)
    pub(super) fn accepted_code(&self, i: usize) -> Option<(String, usize)> {
        let (c, editor) = (self.completion.as_ref()?, self.editor.as_ref()?);
        Some(splice(&editor.source(), c.start, c.end, &c.items.get(i)?.text))
    }

    /// Popup rows (items indices) that are or will be on screen: the draw's
    /// window, computed the same way.
    pub(super) fn visible_rows(&self) -> std::ops::Range<usize> {
        let Some(c) = &self.completion else { return 0..0 };
        let shown = c.items.len().min(crate::ui::COMPLETION_ROWS);
        let top = crate::ui::scroll_window(c.top, c.sel, shown, c.items.len());
        top..top + shown
    }

    /// Candidates on screen without a kind/signature yet: look each up with
    /// a (fast, live-object) inspect, as if it were accepted. Each is asked
    /// once; replies fill in as they land.
    pub(super) fn resolve_visible(&mut self) {
        if self.kernel.is_none() {
            return;
        }
        let rows = self.visible_rows();
        let asks: Vec<(String, String, usize)> = rows
            .filter(|&i| self.completion.as_ref().is_some_and(|c| !c.items[i].resolved))
            .filter_map(|i| {
                let (code, pos) = self.accepted_code(i)?;
                Some((self.completion.as_ref()?.items[i].text.clone(), code, pos))
            })
            .collect();
        if !asks.is_empty() {
            log::debug!("completion: resolving {} visible candidate(s)", asks.len());
        }
        let (Some(c), Some(kernel)) = (&mut self.completion, &self.kernel) else { return };
        for (text, code, pos) in asks {
            for cand in c.all.iter_mut().chain(c.items.iter_mut()).filter(|x| x.text == text) {
                cand.resolved = true;
            }
            c.detail_reqs.insert(kernel.inspect(code, pos), text);
        }
    }

    pub(super) fn request_completion(&mut self, source: String, offset: usize, explicit: bool) {
        let Some(kernel) = &self.kernel else {
            if explicit {
                self.message = Some("no kernel — completion needs one".into());
            }
            return;
        };
        let request = kernel.complete(source.clone(), offset);
        log::debug!("completion: request {request} at char {offset} ({})", if explicit { "Tab" } else { "auto" });
        if explicit {
            self.message = Some("completing…".into());
        }
        self.completion = Some(Completion {
            request,
            source,
            all: Vec::new(),
            items: Vec::new(),
            sel: 0,
            start: offset,
            end: offset,
            explicit,
            asked: Instant::now(),
            at: offset,
            replied: false,
            jedi_retry: false,
            top: 0,
            detail_reqs: HashMap::new(),
        });
    }

    /// After the editor handled a key: an open (or pending) completion
    /// filters on what was typed; a `.` after a name auto-opens one.
    pub(super) fn after_edit_key(&mut self, key: KeyEvent) {
        if self.completion.is_some() {
            self.refilter();
            return;
        }
        let cfg = crate::config::get();
        let Some(editor) = &self.editor else { return };
        if !(cfg.complete_on_dot
            && key.code == KeyCode::Char('.')
            && editor.mode() == ModeKind::Insert
            && crate::ui::notebook_language(&self.notebook) == "python"
            && self.notebook.cells.get(self.selected).is_some_and(|c| c.cell_type == "code"))
        {
            return;
        }
        let source = editor.source();
        let (row, col) = editor.cursor();
        let offset = char_offset(&source, (row, col));
        let line: String = source.split('\n').nth(row).unwrap_or("").chars().take(col).collect();
        if wants_dot_completion(&line) {
            self.request_completion(source, offset, false);
        }
    }

    /// Re-derive the visible items from what was typed since `start`; close
    /// when the cursor left the token, the text before it changed, or
    /// nothing matches any more.
    pub(super) fn refilter(&mut self) {
        let (Some(c), Some(editor)) = (&mut self.completion, &self.editor) else { return };
        let source = editor.source();
        let cur = char_offset(&source, editor.cursor());
        let prefix_same = source.chars().take(c.start).eq(c.source.chars().take(c.start));
        if cur < c.start || !prefix_same || editor.mode() != ModeKind::Insert {
            self.completion = None;
            return;
        }
        c.end = cur;
        if !c.replied {
            return;
        }
        let typed: String = source.chars().skip(c.start).take(cur - c.start).collect();
        c.items = c.all.iter().filter(|m| m.text.starts_with(&typed)).cloned().collect();
        if c.items.is_empty() {
            self.completion = None;
        } else {
            c.sel = c.sel.min(c.items.len() - 1);
            self.resolve_visible();
        }
    }

    pub(super) fn on_complete_reply(
        &mut self,
        parent: String,
        matches: Vec<String>,
        start: usize,
        types: HashMap<String, (String, String)>,
    ) {
        let Some(c) = &mut self.completion else { return };
        if c.request != parent {
            return; // stale: a newer request replaced it
        }
        log::debug!(
            "completion: {} matches in {:?} ({}{})",
            matches.len(),
            c.asked.elapsed(),
            if c.explicit { "Tab" } else { "auto" },
            if c.jedi_retry { ", jedi" } else { "" }
        );
        // The live completer only knows executed names: `np.` before the
        // import cell ran is empty. Retry once with jedi (static analysis
        // of the code). Shell requests run in order, so on → complete → off
        // switches jedi for exactly this one request.
        if matches.is_empty()
            && !c.jedi_retry
            && !crate::config::get().jedi
            && let Some(kernel) = &self.kernel
            && kernel.language.eq_ignore_ascii_case("python")
        {
            kernel.execute_silent("get_ipython().Completer.use_jedi = True");
            c.request = kernel.complete(c.source.clone(), c.at);
            kernel.execute_silent("get_ipython().Completer.use_jedi = False");
            c.jedi_retry = true;
            c.asked = Instant::now();
            return;
        }
        // plain names (not `obj.attr`): this cell's identifiers first (nearest
        // the cursor first), then the kernel's, then other cells' — what you
        // just named beats a builtin
        let token_start = c.source.chars().take(c.at).collect::<Vec<_>>();
        let name_len = token_start.iter().rev().take_while(|&&ch| is_ident(ch)).count();
        let name_start = c.at - name_len;
        let attribute = name_start > 0 && token_start.get(name_start - 1) == Some(&'.');
        let kernel_empty = matches.is_empty();
        let mut all: Vec<Candidate> = Vec::new();
        let mut seen = HashSet::new();
        let words = !attribute && (kernel_empty || start == name_start);
        let typed: String = token_start[name_start..].iter().collect();
        let fits = |w: &String| w.starts_with(&typed) && *w != typed;
        if words {
            let before: String = token_start[..name_start].iter().collect();
            for w in identifiers(&before).into_iter().rev().filter(fits) {
                if seen.insert(w.clone()) {
                    all.push(Candidate::new(w, "word", ""));
                }
            }
        }
        for m in matches {
            if seen.insert(m.clone()) {
                let (kind, sig) = types.get(&m).cloned().unwrap_or_default();
                all.push(Candidate::new(m, &kind, &sig));
            }
        }
        if words {
            let others = self
                .notebook
                .cells
                .iter()
                .enumerate()
                .filter(|(i, cell)| *i != self.selected && cell.cell_type == "code")
                .flat_map(|(_, cell)| identifiers(&cell.source));
            for w in others.filter(fits) {
                if seen.insert(w.clone()) {
                    all.push(Candidate::new(w, "word", ""));
                }
            }
        }
        // replacement starts at the kernel's token start, or at the name
        // when only notebook words matched
        c.start = if kernel_empty { name_start } else { start };
        c.all = all;
        c.replied = true;
        let explicit = c.explicit;
        self.refilter();
        let Some(c) = &self.completion else {
            if explicit {
                self.message = Some("no completions".into());
            }
            return;
        };
        if self.message.as_deref() == Some("completing…") {
            self.message = None;
        }
        if explicit && c.items.len() == 1 {
            let (start, end, text) = (c.start, c.end, c.items[0].text.clone());
            self.completion = None;
            self.accept_completion(start, end, &text);
        }
    }

    pub(super) fn accept_completion(&mut self, start: usize, end: usize, text: &str) {
        if let Some(editor) = &mut self.editor
            && let Err(e) = editor.replace_range(start, end, text)
        {
            self.message = Some(format!("completion failed: {e:#}"));
        }
    }

    pub(super) fn on_inspect_reply(&mut self, parent: String, text: Option<String>) {
        // a completion candidate's kind/signature
        if let Some(c) = &mut self.completion
            && let Some(item) = c.detail_reqs.remove(&parent)
        {
            let (kind, detail) = text.as_deref().map(parse_inspect).unwrap_or_default();
            for cand in c.all.iter_mut().chain(c.items.iter_mut()).filter(|x| x.text == item) {
                if !kind.is_empty() {
                    cand.kind = kind.clone();
                }
                cand.detail = detail.clone();
            }
            return;
        }
        if self.pending_inspect.as_ref() != Some(&parent) {
            return;
        }
        self.pending_inspect = None;
        match text {
            Some(text) if !text.trim().is_empty() => {
                self.pager = Some(Pager {
                    title: "docs".into(),
                    text,
                    top: 0,
                })
            }
            _ => {
                self.message = Some(
                    "no documentation found — the kernel only knows names from cells that ran"
                        .into(),
                )
            }
        }
    }

}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::tests::{key, labels, one_cell_app};
    use crate::kernel::Event;

    #[test]
    fn completion_applies_single_match_filters_while_typing_and_goes_stale() {
        let (mut app, mut r) = one_cell_app("comp");
        app.notebook.cells[0].source = "pr".into();
        app.open_editor(false);
        app.editor.as_mut().unwrap().append_end();
        let pending = |app: &mut App, id: &str| {
            app.completion = Some(Completion {
                request: id.into(),
                source: app.editor.as_ref().unwrap().source(),
                all: Vec::new(),
                items: Vec::new(),
                sel: 0,
                start: 2,
                end: 2,
                explicit: true,
                asked: Instant::now(),
                at: 2,
                replied: false,
                jedi_retry: false,
                top: 0,
                detail_reqs: HashMap::new(),
            });
        };
        let reply = |app: &mut App, r: &mut Rendered, id: &str, m: &[&str]| {
            let matches = m.iter().map(|s| s.to_string()).collect();
            app.apply_kernel_event(
                Event::Complete { parent: id.into(), matches, start: 0, types: HashMap::new() },
                r,
            );
        };
        let src = |app: &App| app.editor.as_ref().unwrap().source();
        // several (deduplicated) matches open the popup
        pending(&mut app, "q1");
        reply(&mut app, &mut r, "q1", &["print", "property", "print", "pow"]);
        assert_eq!(labels(&app), ["print", "property"]);
        // typing keeps it open and filters; Enter accepts over what was typed
        app.on_key(key('o'), &mut r);
        assert_eq!(src(&app), "pro");
        assert_eq!(labels(&app), ["property"]);
        app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &mut r);
        assert_eq!(src(&app), "property");
        assert!(app.completion.is_none());
        // typing past every match closes it; a non-name key closes it too
        pending(&mut app, "q2");
        reply(&mut app, &mut r, "q2", &["property", "props"]);
        app.on_key(key('('), &mut r);
        assert!(app.completion.is_none());
        // an explicit request with one match applies directly
        app.editor = None;
        app.notebook.cells[0].source = "pri".into();
        app.open_editor(false);
        app.editor.as_mut().unwrap().append_end();
        pending(&mut app, "q3");
        let c = app.completion.as_mut().unwrap();
        (c.start, c.at) = (3, 3);
        reply(&mut app, &mut r, "q3", &["print"]);
        assert_eq!(src(&app), "print");
        // a reply whose text before the token changed is dropped
        pending(&mut app, "q4");
        app.completion.as_mut().unwrap().source = "zzzzz".into();
        reply(&mut app, &mut r, "q4", &["pxxxx"]);
        assert_eq!(src(&app), "print");
        assert!(app.completion.is_none());
    }

    #[test]
    fn empty_replies_close_and_notebook_names_complete_before_they_run() {
        let (mut app, mut r) = one_cell_app("words");
        let open = |app: &mut App, src: &str| {
            app.editor = None;
            app.notebook.cells[0].source = src.into();
            app.open_editor(false);
            app.editor.as_mut().unwrap().append_end();
            let at = src.chars().count();
            app.completion = Some(Completion {
                request: "q".into(),
                source: src.into(),
                all: Vec::new(),
                items: Vec::new(),
                sel: 0,
                start: at,
                end: at,
                explicit: true,
                asked: Instant::now(),
                at,
                replied: false,
                jedi_retry: false,
                top: 0,
                detail_reqs: HashMap::new(),
            });
            at
        };
        let reply = |app: &mut App, r: &mut Rendered, m: &[&str], start: usize| {
            let matches = m.iter().map(|s| s.to_string()).collect();
            app.apply_kernel_event(Event::Complete { parent: "q".into(), matches, start, types: HashMap::new() }, r);
        };
        // nothing anywhere: closes with a message (was: "completing…" forever)
        let at = open(&mut app, "zq.");
        app.message = Some("completing…".into());
        reply(&mut app, &mut r, &[], at);
        assert!(app.completion.is_none());
        assert_eq!(app.message.as_deref(), Some("no completions"));
        // unexecuted import: notebook names first, then the kernel's
        let at = open(&mut app, "import numpy as np\nn");
        reply(&mut app, &mut r, &["next", "not"], at - 1);
        assert_eq!(labels(&app), ["np", "numpy", "next", "not"], "nearest name first");
        // words in strings and comments are not names
        assert_eq!(identifiers("x = 'never mind'  # nope\nyes_1 = \"a\\\"b\""), ["yes_1"]);
        // even when the kernel knows nothing, notebook names still come
        let at = open(&mut app, "value_1 = 3\nval");
        reply(&mut app, &mut r, &[], at);
        assert_eq!(app.editor.as_ref().unwrap().source(), "value_1 = 3\nvalue_1", "single match applied");
        // attribute access: no notebook words mixed in
        let at = open(&mut app, "np.n");
        reply(&mut app, &mut r, &[], at);
        assert!(app.completion.is_none());
    }

    #[test]
    fn inspect_text_yields_kind_and_signature() {
        let linspace = "\x1b[31mSignature:\x1b[39m\nnp.linspace(\n    start,\n    stop,\n    num=50,\n)\n\x1b[31mDocstring:\x1b[39m\nReturn evenly spaced numbers.\n\x1b[31mType:\x1b[39m      function";
        assert_eq!(
            parse_inspect(linspace),
            ("function".to_string(), "(start, stop, num=50)".to_string())
        );
        let int = "\x1b[31mType:\x1b[39m        int\n\x1b[31mString form:\x1b[39m 3\n\x1b[31mDocstring:\x1b[39m  int([x]) -> integer";
        assert_eq!(parse_inspect(int), ("instance".to_string(), "int".to_string()));
        let module = "\x1b[31mType:\x1b[39m        module\n\x1b[31mString form:\x1b[39m <module 'os'>";
        assert_eq!(parse_inspect(module).0, "module");
        assert_eq!(
            splice("np.lin", 2, 6, ".linspace"),
            ("np.linspace".to_string(), 11)
        );
    }

    #[test]
    fn shift_tab_in_the_popup_asks_for_docs_and_details_fill_in_lazily() {
        let (mut app, mut r) = one_cell_app("docs");
        app.notebook.cells[0].source = "np.".into();
        app.open_editor(false);
        app.editor.as_mut().unwrap().append_end();
        app.completion = Some(Completion {
            request: "q".into(),
            source: "np.".into(),
            all: Vec::new(),
            items: Vec::new(),
            sel: 0,
            start: 2,
            end: 3,
            explicit: false,
            asked: Instant::now(),
            at: 3,
            replied: false,
            jedi_retry: false,
            top: 0,
            detail_reqs: HashMap::new(),
        });
        let types = HashMap::from([(".linspace".to_string(), ("attribute".to_string(), String::new()))]);
        app.apply_kernel_event(
            Event::Complete {
                parent: "q".into(),
                matches: vec![".linalg".into(), ".linspace".into()],
                start: 2,
                types,
            },
            &mut r,
        );
        assert_eq!(labels(&app), ["linalg", "linspace"], "shown without the dot");
        app.on_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE), &mut r);
        // Shift+Tab: docs (here: no kernel), selection untouched, popup stays
        app.on_key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT), &mut r);
        assert_eq!(app.message.as_deref(), Some("no kernel — docs need one"));
        assert_eq!(app.completion.as_ref().unwrap().sel, 1);
        // a detail reply fills kind + signature of that candidate
        app.completion.as_mut().unwrap().detail_reqs.insert("i1".into(), ".linspace".into());
        let doc = "Signature: np.linspace(start, stop)\nType:      function";
        app.apply_kernel_event(Event::Inspect { parent: "i1".into(), text: Some(doc.into()) }, &mut r);
        let item = &app.completion.as_ref().unwrap().items[1];
        assert_eq!((item.kind.as_str(), item.detail.as_str()), ("function", "(start, stop)"));
    }

    #[test]
    fn candidate_code_splices_into_the_live_text_and_visible_rows_follow_the_window() {
        let (mut app, _r) = one_cell_app("splice");
        // request made at "np." — then the user typed "lin" before Shift+Tab
        app.notebook.cells[0].source = "np.lin\nplt.subplots(figsize=(10, 10))".into();
        app.open_editor(false);
        app.editor.as_mut().unwrap().click(0, 6, true);
        let mut c = Completion::for_test(2, 6);
        c.source = "np.\nplt.subplots(figsize=(10, 10))".into(); // request-time snapshot
        c.push_for_test(".linspace", "attribute", "");
        app.completion = Some(c);
        assert_eq!(
            app.accepted_code(0),
            Some(("np.linspace\nplt.subplots(figsize=(10, 10))".to_string(), 11)),
            "the rest of the cell must survive intact"
        );
        // the resolve window is the draw's window
        let c = app.completion.as_mut().unwrap();
        for i in 0..29 {
            c.push_for_test(&format!(".x{i}"), "attribute", "");
        }
        c.sel = 13;
        assert_eq!(app.visible_rows(), 2..14);
    }

    #[test]
    fn dot_opens_completion_only_after_names_outside_strings() {
        for yes in ["np.", "  x.", "f().", "a[0].", "obj.attr."] {
            assert!(wants_dot_completion(yes), "{yes}");
        }
        for no in ["1.", "x..", ".", "'np.", "s = \"a.", "# np.", "x = 3."] {
            assert!(!wants_dot_completion(no), "{no}");
        }
        assert!(wants_dot_completion("s = 'a'; np."), "string closed before");
    }

}
