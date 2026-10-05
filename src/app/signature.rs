//! Signature help in the cell editor: typing `(` after a name (or `,` in a
//! call whose signature isn't known yet) asks the kernel for the callee's
//! signature once; the popup then follows the cursor through the call and
//! highlights the argument being typed, positionally or by `name=`.

use super::*;
use completion::{is_ident, parse_inspect};

/// The call the cursor is in, and its signature once the kernel answered.
pub struct SigHelp {
    cell: usize,
    /// Char offset of the call's `(` in the cell source.
    open: usize,
    /// The callee as written (`np.arange`).
    name: String,
    /// inspect_request msg_id until the reply lands.
    request: Option<String>,
    /// `name(params) -> ret`; empty until the reply lands.
    pub text: String,
    /// Positional index of the argument being typed (None after a named
    /// argument: Python allows only named ones from there), and its
    /// `name=` if it has one.
    arg: Option<usize>,
    keyword: Option<String>,
}

impl SigHelp {
    /// Byte range in `text` of the parameter being typed: the `name=` one,
    /// else the `arg`-th positional (a `*args` takes all the rest), else
    /// none (past a named argument, only a typed `name=` highlights).
    pub fn active(&self) -> Option<std::ops::Range<usize>> {
        let params = params(&self.text);
        let name = |r: &std::ops::Range<usize>| {
            let p = self.text[r.clone()].trim_start_matches('*');
            p.split([':', '=']).next().unwrap_or("").trim().to_string()
        };
        if let Some(k) = &self.keyword {
            return params.into_iter().find(|r| &name(r) == k);
        }
        let arg = self.arg?;
        let mut i = 0;
        for r in params {
            let p = &self.text[r.clone()];
            if p == "/" {
                continue;
            }
            if p == "*" || p.starts_with("**") {
                return None; // keyword-only from here on
            }
            if p.starts_with('*') || i == arg {
                return Some(r);
            }
            i += 1;
        }
        None
    }

    /// Char offset in the cell source where the callee's name starts: the
    /// popup lines its text up with it.
    pub fn name_start(&self) -> usize {
        self.open.saturating_sub(self.name.chars().count())
    }

    /// Test helper: the call's `(` at char `open`, the callee `name` before it.
    #[cfg(test)]
    pub fn placed(self, open: usize, name: &str) -> Self {
        SigHelp {
            open,
            name: name.into(),
            ..self
        }
    }

    #[cfg(test)]
    pub fn for_test(text: &str, arg: usize, keyword: Option<&str>) -> Self {
        SigHelp {
            cell: 0,
            open: 0,
            name: String::new(),
            request: None,
            text: text.into(),
            arg: Some(arg),
            keyword: keyword.map(String::from),
        }
    }
}

/// Byte ranges of the parameters in `sig`'s first parenthesised group, split
/// at its top-level commas (defaults may hold strings, brackets, commas).
fn params(sig: &str) -> Vec<std::ops::Range<usize>> {
    let Some(open) = sig.find('(') else {
        return Vec::new();
    };
    let (mut out, mut depth, mut quote, mut start) = (Vec::new(), 0, None, open + 1);
    let push = |s: usize, e: usize, out: &mut Vec<std::ops::Range<usize>>| {
        let piece = &sig[s..e];
        let lead = piece.len() - piece.trim_start().len();
        if !piece.trim().is_empty() {
            out.push(s + lead..s + lead + piece.trim().len());
        }
    };
    let mut chars = sig[open + 1..].char_indices();
    while let Some((j, c)) = chars.next() {
        let i = open + 1 + j;
        match (quote, c) {
            (Some(_), '\\') => {
                chars.next();
            }
            (Some(q), c) if c == q => quote = None,
            (Some(_), _) => {}
            (None, '\'' | '"') => quote = Some(c),
            (None, '(' | '[' | '{') => depth += 1,
            (None, ')') if depth == 0 => {
                push(start, i, &mut out);
                break;
            }
            (None, ')' | ']' | '}') => depth -= 1,
            (None, ',') if depth == 0 => {
                push(start, i, &mut out);
                start = i + 1;
            }
            _ => {}
        }
    }
    out
}

/// Where `cursor` (a char offset) sits in a call.
#[derive(Debug, PartialEq)]
pub(super) struct CallSite {
    /// Char offset of the innermost unclosed `(` before the cursor.
    pub open: usize,
    /// Commas between it and the cursor at its own level; None once an
    /// argument before the cursor was named (`name=`, `**kwargs`).
    pub arg: Option<usize>,
    /// `name` when the argument being typed starts with `name=`.
    pub keyword: Option<String>,
}

/// The innermost call `cursor` is inside, scanning the cell from its start:
/// strings (incl. triple-quoted), comments and nested brackets are skipped,
/// so `f(x[1, 2], "a,b", ` is at argument 2 of `f`.
pub(super) fn call_site(source: &str, cursor: usize) -> Option<CallSite> {
    let chars: Vec<char> = source.chars().take(cursor).collect();
    // (bracket, offset, commas, start of the current argument, an earlier
    // argument was named)
    let mut stack: Vec<(char, usize, usize, usize, bool)> = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        match c {
            '#' => {
                while i < chars.len() && chars[i] != '\n' {
                    i += 1;
                }
            }
            '\'' | '"' => {
                let triple = chars.get(i + 1) == Some(&c) && chars.get(i + 2) == Some(&c);
                let close = if triple { 3 } else { 1 };
                i += close;
                loop {
                    match chars.get(i) {
                        None => return None, // the cursor is inside a string
                        Some('\\') => i += 2,
                        Some('\n') if !triple => break, // unterminated: give up on it
                        Some(&q) if q == c && (!triple || chars[i..].starts_with(&[c, c, c])) => {
                            i += close - 1;
                            break;
                        }
                        Some(_) => i += 1,
                    }
                }
            }
            '(' | '[' | '{' => stack.push((c, i, 0, i + 1, false)),
            ')' | ']' | '}' => {
                stack.pop();
            }
            ',' => {
                if let Some(top) = stack.last_mut() {
                    let done: String = chars[top.3..i].iter().collect();
                    top.4 |= keyword_of(&done).is_some() || done.trim_start().starts_with("**");
                    top.2 += 1;
                    top.3 = i + 1;
                }
            }
            _ => {}
        }
        i += 1;
    }
    let depth = stack.iter().rposition(|b| b.0 == '(')?;
    let (_, open, arg, start, named) = stack[depth];
    // `name=` only counts in the call's own argument, not inside a nested
    // bracket (`f(x[a=` is not a keyword of f)
    let keyword = (depth == stack.len() - 1)
        .then(|| keyword_of(&chars[start..].iter().collect::<String>()))
        .flatten();
    let arg = (!named).then_some(arg);
    Some(CallSite { open, arg, keyword })
}

/// `name` when argument text `arg` starts with `name=` (not `name ==`).
fn keyword_of(arg: &str) -> Option<String> {
    let (name, rest) = arg.trim_start().split_once('=')?;
    let name = name.trim_end();
    let ok = !rest.starts_with('=')
        && !name.is_empty()
        && name.chars().all(is_ident)
        && !name.starts_with(|c: char| c.is_ascii_digit());
    ok.then(|| name.to_string())
}

/// The callee written right before the `(` at `open` (`np.random.normal`),
/// or None when the bracket isn't a call (`(a, b)`, `if (`, `x = (`).
fn callee(source: &str, open: usize) -> Option<String> {
    let before: Vec<char> = source.chars().take(open).collect();
    let start = before
        .iter()
        .rposition(|&c| !(is_ident(c) || c == '.'))
        .map_or(0, |p| p + 1);
    let name: String = before[start..].iter().collect();
    let name = name.trim_start_matches('.');
    const KEYWORDS: &[&str] = &[
        "and", "assert", "del", "elif", "except", "for", "from", "if", "import", "in", "is",
        "lambda", "not", "or", "return", "while", "with", "yield", "await",
    ];
    let ok = !name.is_empty()
        && !name.starts_with(|c: char| c.is_ascii_digit())
        && !KEYWORDS.contains(&name);
    ok.then(|| name.to_string())
}

impl App {
    /// After an editor key: follow the cursor through the call it is in,
    /// asking the kernel when `(` or `,` enters a call with no signature yet.
    pub(super) fn update_signature(&mut self, key: KeyEvent) {
        let wanted = crate::config::get().signature_help
            && self
                .notebook
                .cells
                .get(self.selected)
                .is_some_and(|c| c.cell_type == "code");
        let Some(editor) = self
            .editor
            .as_ref()
            .filter(|e| wanted && e.mode() == ModeKind::Insert)
        else {
            self.signature = None;
            return;
        };
        let source = editor.source();
        let Some(site) = call_site(&source, char_offset(&source, editor.cursor())) else {
            self.signature = None;
            return;
        };
        if let Some(s) = &mut self.signature
            && s.cell == self.selected
            && s.open == site.open
        {
            (s.arg, s.keyword) = (site.arg, site.keyword);
            return;
        }
        self.signature = None;
        if !matches!(key.code, KeyCode::Char('(' | ',')) {
            return;
        }
        let (Some(name), Some(kernel)) = (callee(&source, site.open), &self.kernel) else {
            return;
        };
        // inspect with the cursor right after the name, as Shift+Tab does
        let request = kernel.inspect(source, site.open);
        self.signature = Some(SigHelp {
            cell: self.selected,
            open: site.open,
            name,
            request: Some(request),
            text: String::new(),
            arg: site.arg,
            keyword: site.keyword,
        });
    }

    /// An inspect reply for the pending signature: keep it if it is a
    /// callable's, else close. False when the reply is someone else's.
    pub(super) fn on_signature_reply(&mut self, parent: &str, text: Option<&str>) -> bool {
        let Some(s) = &mut self.signature else {
            return false;
        };
        if s.request.as_deref() != Some(parent) {
            return false;
        }
        s.request = None;
        let (_, detail) = text.map(parse_inspect).unwrap_or_default();
        if detail.starts_with('(') {
            s.text = format!("{}{detail}", s.name);
        } else {
            self.signature = None; // not callable, or unknown to the kernel
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::tests::one_cell_app;
    use crate::kernel::Event;

    fn site(src_with_cursor: &str) -> Option<CallSite> {
        let cursor = src_with_cursor.find('^').expect("^ marks the cursor");
        let src = src_with_cursor.replace('^', "");
        call_site(&src, src[..cursor].chars().count())
    }

    #[test]
    fn finds_the_call_and_argument_under_the_cursor() {
        let at = |open, arg, kw: Option<&str>| {
            Some(CallSite {
                open,
                arg: Some(arg),
                keyword: kw.map(String::from),
            })
        };
        let named = |open, kw: Option<&str>| {
            Some(CallSite {
                open,
                arg: None,
                keyword: kw.map(String::from),
            })
        };
        // after a named argument only named ones may follow: no position
        assert_eq!(site("np.var(x, ddof=1, ^"), named(6, None));
        assert_eq!(site("np.var(x, ddof=1, axis=^"), named(6, Some("axis")));
        assert_eq!(site("f(*a, ^"), at(1, 1, None)); // *args keeps positions
        assert_eq!(site("f(**kw, ^"), named(1, None));
        assert_eq!(site("f(a == 1, ^"), at(1, 1, None)); // a comparison, not named
        assert_eq!(site("f(g(k=1), ^"), at(1, 1, None)); // named in the inner call only
        assert_eq!(site("np.arange(^"), at(9, 0, None));
        assert_eq!(site("np.std(x, ^"), at(6, 1, None));
        assert_eq!(site("np.std(x, ddof=^"), at(6, 1, Some("ddof")));
        assert_eq!(site("np.std(x, ddof = ^"), at(6, 1, Some("ddof")));
        assert_eq!(site("f(a == ^"), at(1, 0, None)); // comparison, not a keyword
        // nested brackets, strings and comments don't count
        assert_eq!(site("f(x[1, 2], \"a,b)\", ^"), at(1, 2, None));
        assert_eq!(site("f(g(1, 2), ^"), at(1, 1, None));
        assert_eq!(site("f(1, # a, b (\n  ^"), at(1, 1, None));
        assert_eq!(site("f('''a,\n)''', ^"), at(1, 1, None));
        // inside a subscript within a call: still f's argument 0
        assert_eq!(site("f(x[a=^"), at(1, 0, None));
        assert_eq!(site("f(x)^"), None);
        assert_eq!(site("f(\"abc^"), None); // inside a string
    }

    #[test]
    fn callee_names_skip_non_calls() {
        assert_eq!(
            callee("np.random.normal(", 16).as_deref(),
            Some("np.random.normal")
        );
        assert_eq!(callee("x = (", 4), None);
        assert_eq!(callee("if (", 3), None);
        assert_eq!(callee("df.groupby('a').agg(", 19).as_deref(), Some("agg"));
    }

    #[test]
    fn highlights_positional_keyword_and_star_params() {
        let sig = "np.arange(start_or_stop, /, stop=None, step=None, *, dtype=None, like=None)";
        let active = |arg, kw| {
            let s = SigHelp::for_test(sig, arg, kw);
            s.active().map(|r| s.text[r].to_string())
        };
        assert_eq!(active(0, None).as_deref(), Some("start_or_stop"));
        assert_eq!(active(1, None).as_deref(), Some("stop=None")); // `/` skipped
        assert_eq!(active(3, None), None); // only keywords after `*`
        assert_eq!(active(1, Some("dtype")).as_deref(), Some("dtype=None"));
        assert_eq!(active(0, Some("nope")), None);
        // past a named argument: nothing, unless a `name=` is being typed
        let named = |kw: Option<&str>| {
            let s = SigHelp {
                arg: None,
                ..SigHelp::for_test(sig, 0, kw)
            };
            s.active().map(|r| s.text[r].to_string())
        };
        assert_eq!(named(None), None);
        assert_eq!(named(Some("step")).as_deref(), Some("step=None"));

        let p = "print(*args, sep=' ', end='\\n', file=None, flush=False)";
        let s = SigHelp::for_test(p, 5, None);
        assert_eq!(&s.text[s.active().unwrap()], "*args"); // takes all positionals
        let s = SigHelp::for_test(p, 0, Some("sep"));
        assert_eq!(&s.text[s.active().unwrap()], "sep=' '");
        let s = SigHelp::for_test("f(a, b=',', c=(1, 2)) -> int", 2, None);
        assert_eq!(&s.text[s.active().unwrap()], "c=(1, 2)");
    }

    #[test]
    fn reply_fills_the_popup_or_closes_it() {
        let (mut app, mut r) = one_cell_app("sig");
        let pending = |app: &mut App, id: &str| {
            app.signature = Some(SigHelp {
                request: Some(id.into()),
                name: "mine".into(),
                ..SigHelp::for_test("", 0, None)
            })
        };
        pending(&mut app, "q1");
        let doc = "\x1b[31mSignature:\x1b[39m mine(path, kernel=None, *, tx=1)\n\x1b[31mType:\x1b[39m      function";
        app.apply_kernel_event(
            Event::Inspect {
                parent: "q1".into(),
                text: Some(doc.into()),
            },
            &mut r,
        );
        assert_eq!(
            app.signature.as_ref().unwrap().text,
            "mine(path, kernel=None, *, tx=1)"
        );

        // not a callable (or unknown to the kernel): no popup
        pending(&mut app, "q2");
        app.apply_kernel_event(
            Event::Inspect {
                parent: "q2".into(),
                text: Some("Type:      int".into()),
            },
            &mut r,
        );
        assert!(app.signature.is_none());
    }
}
