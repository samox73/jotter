//! jupytext's percent format and plain Python scripts.
//!
//! A `.py` notebook is a script with `# %%` cell markers (jupytext's
//! `py:percent`; VS Code, Spyder and PyCharm read the same markers). This is a
//! port of the rules jupytext 1.19 applies — `DoublePercentScriptCellReader`,
//! `DoublePercentCellExporter`, `magics.py`, `pep8.py`, `stringparser.py` — so a
//! file jupytext wrote round-trips byte for byte and jupytext reads what we
//! write the same way. Python only. Outputs, counts and ids never reach the
//! file; a file without markers is one code cell and stays marker-less.
//!
//! jupytext is MIT licensed, Copyright (c) 2018-2026 Marc Wouts
//! (<https://github.com/mwouts/jupytext>); the notice is in
//! `licenses/jupytext-MIT.txt`.

use crate::notebook::{Cell, Notebook, new_cell_id};
use serde_json::{Map, Value};

/// Cell metadata jupytext leaves out of the text form (`_IGNORE_CELL_METADATA`):
/// frontend state, and its own layout keys (which still steer the layout).
const IGNORED_METADATA: [&str; 11] = [
    "autoscroll",
    "collapsed",
    "scrolled",
    "trusted",
    "execution",
    "ExecuteTime",
    "skipline",
    "noskipline",
    "cell_marker",
    "lines_to_next_cell",
    "lines_to_end_of_cell_marker",
];

pub fn read(text: &str) -> Notebook {
    let lines: Vec<&str> = text.lines().collect();
    let (header, mut pos) = split_header(&lines);
    let mut metadata = Map::new();
    if let Some(ks) = header.as_deref().and_then(header_kernelspec) {
        metadata.insert("kernelspec".into(), ks);
    }
    let mut cells = Vec::new();
    while pos < lines.len() {
        let (cell, next) = read_cell(&lines[pos..]);
        cells.push(cell);
        pos += next;
    }
    if cells.is_empty() {
        cells.push(Cell::new_code());
    }
    let mut extra = Map::new();
    extra.insert("metadata".into(), Value::Object(metadata));
    extra.insert("nbformat".into(), 4.into());
    extra.insert("nbformat_minor".into(), 5.into());
    Notebook {
        cells,
        extra,
        header,
    }
}

pub fn write(nb: &Notebook) -> String {
    // a lone code cell without metadata is a plain script: no marker, so
    // `jotter script.py` + `w` leaves the script as it was
    let plain = nb.cells.len() == 1
        && nb.cells[0].cell_type == "code"
        && filtered_metadata(&nb.cells[0]).is_empty()
        && !nb.header.as_deref().is_some_and(has_yaml);
    // back to front: the blank lines after a cell depend on what follows
    let mut lines: Vec<String> = Vec::new();
    for cell in nb.cells.iter().rev() {
        let mut text = cell_to_text(cell, !plain);
        let n = cell_metadata(cell)
            .get("lines_to_next_cell")
            .and_then(Value::as_u64)
            .map_or_else(
                || pep8_lines_between(&refs(&text), &refs(&lines)),
                |n| n as usize,
            );
        text.extend(std::iter::repeat_n(String::new(), n));
        text.append(&mut lines);
        lines = text;
    }
    let mut out = nb.header.clone().unwrap_or_default();
    out.push_str(&lines.join("\n"));
    out
}

fn refs(lines: &[String]) -> Vec<&str> {
    lines.iter().map(String::as_str).collect()
}

/// The preamble: a shebang and an encoding line, and jupytext's `# ---` …
/// `# ---` YAML header, plus the blank lines following them. Returned
/// verbatim (a shebang in a cell would read as a shell magic) with the
/// index of the first line after it.
fn split_header(lines: &[&str]) -> (Option<String>, usize) {
    let mut end = 0;
    if lines.first().is_some_and(|l| l.starts_with("#!")) {
        end += 1;
    }
    if lines
        .get(end)
        .is_some_and(|l| l.starts_with('#') && l.contains("coding"))
    {
        end += 1;
    }
    if lines.get(end).is_some_and(is_rule)
        && let Some(close) = lines[end + 1..].iter().position(is_rule)
    {
        end += close + 2;
    }
    if end == 0 {
        return (None, 0);
    }
    while lines.get(end).is_some_and(|l| l.trim().is_empty()) {
        end += 1;
    }
    (Some(lines[..end].join("\n") + "\n"), end)
}

/// A `# ---` line, as jupytext's `_HEADER_RE` sees it after uncommenting.
fn is_rule(line: &&str) -> bool {
    uncomment(line).trim_end() == "---"
}

/// Does the preamble hold a YAML header (then the file is jupytext's, and
/// even a lone cell keeps its marker)?
fn has_yaml(header: &str) -> bool {
    header.lines().any(|l| is_rule(&l))
}

/// The `kernelspec:` mapping of the header, by indentation (no YAML parser:
/// jupytext writes plain `key: value` scalars, possibly quoted).
fn header_kernelspec(header: &str) -> Option<Value> {
    let lines: Vec<&str> = header.lines().map(uncomment).collect();
    let indent = |l: &str| l.len() - l.trim_start().len();
    let at = lines.iter().position(|l| l.trim() == "kernelspec:")?;
    let depth = indent(lines[at]);
    let mut spec = Map::new();
    for line in &lines[at + 1..] {
        if line.trim().is_empty() || indent(line) <= depth {
            break;
        }
        if let Some((key, value)) = line.split_once(':') {
            let value = value.trim();
            let unquoted = value
                .strip_prefix('\'')
                .and_then(|v| v.strip_suffix('\''))
                .or_else(|| value.strip_prefix('"').and_then(|v| v.strip_suffix('"')))
                .unwrap_or(value);
            spec.insert(key.trim().into(), unquoted.into());
        }
    }
    (!spec.is_empty()).then_some(Value::Object(spec))
}

/// Strip one `# ` (or `#`) like jupytext's `uncomment`.
fn uncomment(line: &str) -> &str {
    line.strip_prefix("# ")
        .or_else(|| line.strip_prefix('#'))
        .unwrap_or(line)
}

/// jupytext's `^\s*#\s*%%(%*)\s(.*)$` and `^\s*#\s*(%%|<codecell>|In\[[0-9 ]*\]:?)\s*$`:
/// `# %%`, `#%%`, `# %% [markdown] …`, `# %%% sub-cell`, nbconvert's `# In[3]:`;
/// not `# %%time`, which is a commented cell magic.
fn is_marker(line: &str) -> bool {
    let Some(rest) = line.trim_start().strip_prefix('#') else {
        return false;
    };
    let rest = rest.trim_start();
    if let Some(after) = rest.strip_prefix("%%") {
        let deeper = after.trim_start_matches('%');
        if deeper.len() < after.len() {
            return deeper.starts_with(char::is_whitespace);
        }
        return after.is_empty() || after.starts_with(char::is_whitespace);
    }
    if rest.trim_end() == "<codecell>" {
        return true;
    }
    if let Some(after) = rest.strip_prefix("In[")
        && let Some((n, tail)) = after.split_once(']')
    {
        return n.chars().all(|c| c.is_ascii_digit() || c == ' ')
            && matches!(tail.trim_end(), "" | ":");
    }
    false
}

/// One cell from the start of `lines` (a marker or an unmarked first cell):
/// the cell and the index of the next one.
fn read_cell(lines: &[&str]) -> (Cell, usize) {
    let marked = is_marker(lines[0]);
    // the next marker, outside multi-line strings
    let mut parser = StringParser::default();
    let mut next = lines.len();
    for (k, line) in lines.iter().enumerate() {
        if parser.is_quoted() {
            parser.read_line(line);
            continue;
        }
        parser.read_line(line);
        if k > 0 && is_marker(line) {
            next = k;
            break;
        }
    }
    // one empty separator line is layout; two blank ones too (pep8 spacing);
    // anything else is content
    let region = &lines[..next];
    let end = if last_two_blank(region) {
        next - 2
    } else if next > 0 && region[next - 1].is_empty() {
        next - 1
    } else {
        next
    };
    let start = usize::from(marked);
    let raw = &lines[start..end.max(start)];
    let (cell_type, mut metadata) = if marked {
        parse_options(lines[0])
    } else {
        ("code", Map::new())
    };
    // like jupytext, remember the spacing only when it isn't what pep8 gives
    let total = lines.len();
    let blank_after = if end < total {
        next - end + usize::from(next >= total)
    } else {
        1
    };
    let expected = pep8_lines_between(if raw.is_empty() { &[""] } else { raw }, &lines[next..]);
    if blank_after != expected {
        metadata.insert("lines_to_next_cell".into(), blank_after.into());
    }
    let mut content: Vec<String> = raw.iter().map(|s| s.to_string()).collect();
    if cell_type == "code" {
        uncomment_magics(&mut content);
    } else {
        for line in &mut content {
            *line = uncomment(line).to_string();
        }
    }
    let mut extra = Map::new();
    extra.insert("id".into(), new_cell_id().into());
    extra.insert("metadata".into(), Value::Object(metadata));
    if cell_type == "code" {
        extra.insert("execution_count".into(), Value::Null);
    }
    let cell = Cell {
        cell_type: cell_type.into(),
        source: content.join("\n"),
        outputs: (cell_type == "code").then(Vec::new),
        extra,
    };
    (cell, next)
}

fn last_two_blank(lines: &[&str]) -> bool {
    let blank = |l: &str| l.trim().is_empty();
    let n = lines.len();
    n >= 3 && !blank(lines[n - 3]) && blank(lines[n - 2]) && blank(lines[n - 1])
}

/// The marker's tail (jupytext's `options_to_metadata`): `[markdown]`, `[raw]`
/// or `[md]` set the type, free text is the title, `%` prefixes a sub-cell
/// depth, and `key=value` pairs or one `{…}` object are the metadata.
fn parse_options(marker: &str) -> (&'static str, Map<String, Value>) {
    let line = uncomment(marker.trim_start());
    let Some(at) = line.find("%%") else {
        return ("code", Map::new());
    };
    let (mut title, mut meta) = text_to_metadata(&line[at + 2..]);
    let mut cell_type = "code";
    for (tag, ty) in [
        ("[markdown]", "markdown"),
        ("[raw]", "raw"),
        ("[md]", "markdown"),
    ] {
        if title.contains(tag) {
            title = title.replace(tag, "").trim().to_string();
            cell_type = ty;
            if tag == "[md]" {
                meta.insert("region_name".into(), "md".into());
            }
            break;
        }
    }
    let depth = title.chars().take_while(|&c| c == '%').count();
    if depth > 0 {
        meta.insert("cell_depth".into(), depth.into());
        title = title[depth..].trim().to_string();
    }
    if !title.is_empty() {
        meta.insert("title".into(), title.into());
    }
    (cell_type, meta)
}

/// jupytext's `text_to_metadata(allow_title=True)`: the title is the words
/// before the first key, or before a `{…}` JSON object.
fn text_to_metadata(text: &str) -> (String, Map<String, Value>) {
    let text = text.trim();
    let curly = text.find('{');
    let eq = text.find('=');
    if let Some(c) = curly
        && eq.is_none_or(|e| e > c)
    {
        let meta = match serde_json::from_str::<Value>(text[c..].trim()) {
            Ok(Value::Object(m)) => m,
            _ => incorrectly_encoded(&text[c..]),
        };
        return (text[..c].trim().to_string(), meta);
    }
    let mut words: Vec<&str> = match eq {
        Some(e) => {
            let mut words: Vec<&str> = text[..e].split(' ').collect();
            while words.last().is_some_and(|w| w.is_empty()) {
                words.pop();
            }
            words.pop(); // the key
            words
        }
        None => text.split(' ').collect(),
    };
    while words
        .last()
        .is_some_and(|w| w.trim().is_empty() || w.starts_with('.'))
    {
        words.pop();
    }
    let title = words.join(" ");
    let meta = parse_key_equal_value(&text[title.len()..]);
    (title, meta)
}

/// jupytext's `parse_key_equal_value`: pairs from the right, a bare
/// identifier is a key with no value, and what won't parse is kept whole.
fn parse_key_equal_value(text: &str) -> Map<String, Value> {
    let text = text.trim();
    if text.is_empty() {
        return Map::new();
    }
    let last_space = text.rfind(' ');
    let last = &text[last_space.map_or(0, |p| p + 1)..];
    if !text.starts_with("--") && is_identifier(last) {
        let mut meta = match last_space {
            Some(p) if p > 0 => parse_key_equal_value(&text[..p]),
            _ => Map::new(),
        };
        meta.insert(last.into(), Value::Null);
        return meta;
    }
    let mut end = text.len();
    while let Some(e) = text[..end].rfind('=') {
        end = e;
        let prev_space = text[..e].trim_end().rfind(' ');
        let key = text[prev_space.map_or(0, |p| p + 1)..e].trim();
        if !is_valid_key(key) {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(text[e + 1..].trim()) else {
            continue; // try a longer expression
        };
        let mut meta = match prev_space {
            Some(p) if p > 0 => parse_key_equal_value(&text[..p]),
            _ => Map::new(),
        };
        meta.insert(key.into(), value);
        return meta;
    }
    incorrectly_encoded(text)
}

fn incorrectly_encoded(text: &str) -> Map<String, Value> {
    let mut meta = Map::new();
    meta.insert("incorrectly_encoded_metadata".into(), text.into());
    meta
}

/// `^[a-zA-Z_\.]+[a-zA-Z0-9_\.]*$`
fn is_identifier(s: &str) -> bool {
    s.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_' || c == '.')
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.')
}

/// `^[a-zA-Z0-9_\.@/-]+$`
fn is_valid_key(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "_.@/-".contains(c))
}

fn cell_metadata(cell: &Cell) -> Map<String, Value> {
    cell.extra
        .get("metadata")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default()
}

/// Cell metadata as the marker line carries it.
fn filtered_metadata(cell: &Cell) -> Map<String, Value> {
    let mut meta = cell_metadata(cell);
    for key in IGNORED_METADATA {
        meta.remove(key);
    }
    meta
}

/// A cell's lines in the file: the marker, then the commented-magics code or
/// the `# `-prefixed markdown (jupytext's `DoublePercentCellExporter`).
fn cell_to_text(cell: &Cell, marker: bool) -> Vec<String> {
    let source: Vec<&str> = cell.source.split('\n').collect();
    let code = cell.cell_type == "code";
    let mut lines = Vec::new();
    if marker {
        let mut meta = filtered_metadata(cell);
        let mut parts: Vec<String> = Vec::new();
        if let Some(title) = meta.remove("title") {
            parts.push(title.as_str().map_or_else(|| py_json(&title), String::from));
        }
        if let Some(depth) = meta.remove("cell_depth").and_then(|v| v.as_u64()) {
            parts.insert(0, "%".repeat(depth as usize));
        }
        if !code {
            let name = meta
                .remove("region_name")
                .and_then(|v| v.as_str().map(String::from))
                .unwrap_or_else(|| cell.cell_type.clone());
            parts.push(format!("[{name}]"));
        }
        let mut words = Vec::new();
        if !parts.is_empty() {
            words.push(parts.join(" "));
        }
        // bare keys (null values) last: the reader takes a bare word before
        // the first `key=` for part of the title
        let mut bare = Vec::new();
        for (key, value) in &meta {
            match value {
                _ if key == "incorrectly_encoded_metadata" => {
                    words.push(value.as_str().unwrap_or_default().to_string());
                }
                Value::Null => bare.push(key.clone()),
                _ => words.push(format!("{key}={}", py_json(value))),
            }
        }
        words.append(&mut bare);
        let options = words.join(" ");
        // a code cell whose first line is indented gets an indented marker
        let indent = match source.first() {
            Some(first) if code && !first.trim().is_empty() => {
                &first[..first.len() - first.trim_start().len()]
            }
            _ => "",
        };
        lines.push(if options.is_empty() || options.starts_with('%') {
            format!("{indent}# %%{options}")
        } else {
            format!("{indent}# %% {options}")
        });
    }
    if code {
        let mut src: Vec<String> = source.iter().map(|s| s.to_string()).collect();
        comment_magics(&mut src);
        if src == [""] {
            return lines;
        }
        lines.extend(src);
    } else {
        lines.extend(source.iter().map(|l| {
            if l.is_empty() {
                "#".to_string()
            } else {
                format!("# {l}")
            }
        }));
    }
    lines
}

/// `json.dumps` with Python's defaults: `, ` and `: ` separators, non-ASCII
/// escaped — what jupytext writes on the marker line.
fn py_json(value: &Value) -> String {
    let mut buf = Vec::new();
    let mut ser = serde_json::Serializer::with_formatter(&mut buf, PyFormatter);
    serde::Serialize::serialize(value, &mut ser).expect("json value");
    String::from_utf8(buf).expect("ascii")
}

struct PyFormatter;

impl serde_json::ser::Formatter for PyFormatter {
    fn begin_array_value<W: ?Sized + std::io::Write>(
        &mut self,
        w: &mut W,
        first: bool,
    ) -> std::io::Result<()> {
        if first { Ok(()) } else { w.write_all(b", ") }
    }

    fn begin_object_key<W: ?Sized + std::io::Write>(
        &mut self,
        w: &mut W,
        first: bool,
    ) -> std::io::Result<()> {
        if first { Ok(()) } else { w.write_all(b", ") }
    }

    fn begin_object_value<W: ?Sized + std::io::Write>(&mut self, w: &mut W) -> std::io::Result<()> {
        w.write_all(b": ")
    }

    fn write_string_fragment<W: ?Sized + std::io::Write>(
        &mut self,
        w: &mut W,
        fragment: &str,
    ) -> std::io::Result<()> {
        for c in fragment.chars() {
            if c.is_ascii() {
                w.write_all(&[c as u8])?;
            } else {
                for unit in c.encode_utf16(&mut [0; 2]) {
                    write!(w, "\\u{unit:04x}")?;
                }
            }
        }
        Ok(())
    }
}

// --- magics (jupytext/magics.py, Python rules) ---

/// jupytext's `(# |#)*` prefix.
fn strip_hashes(mut s: &str) -> &str {
    loop {
        if let Some(r) = s.strip_prefix("# ") {
            s = r;
        } else if let Some(r) = s.strip_prefix('#') {
            s = r;
        } else {
            return s;
        }
    }
}

/// `(%|%%|%%%)[a-zA-Z]`: what follows the percents, when it's a magic name.
fn after_percents(s: &str) -> Option<&str> {
    let name = s.trim_start_matches('%');
    let n = s.len() - name.len();
    ((1..=3).contains(&n) && name.starts_with(|c: char| c.is_ascii_alphabetic())).then_some(name)
}

/// `(.*)#\s*flag` after the magic: a `# escape` / `# noescape` override.
fn has_flag(rest: &str, flag: &str) -> bool {
    rest.match_indices('#')
        .any(|(i, _)| rest[i + 1..].trim_start().starts_with(flag))
}

/// Is this line (possibly already commented) a Jupyter magic, to be
/// commented out in the script and uncommented on the way back?
fn is_magic(line: &str) -> bool {
    if let Some(rest) = after_percents(strip_hashes(line.trim_start())) {
        if has_flag(rest, "escape") {
            return true;
        }
        return !has_flag(rest, "noescape");
    }
    help_or_bash(line) || magic_assign(line) || ipython_help(line) || magic_cmd(line)
}

/// `^\s*(?:(?:# |#)+\s*)?(\?|!)\s*[A-Za-z\.\~\$\\\/\{\}]`
fn help_or_bash(line: &str) -> bool {
    let s = line.trim_start();
    let t = strip_hashes(s);
    let t = if t.len() < s.len() { t.trim_start() } else { t };
    let Some(r) = t.strip_prefix(['?', '!']) else {
        return false;
    };
    r.trim_start()
        .starts_with(|c: char| c.is_ascii_alphabetic() || ".~$\\/{}".contains(c))
}

/// `^(# |#)*\s*([a-zA-Z_][a-zA-Z_$0-9]*)\s*=\s*(%|%%|%%%|!)[a-zA-Z]`
fn magic_assign(line: &str) -> bool {
    let s = strip_hashes(line).trim_start();
    if !s.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_') {
        return false;
    }
    let ident = s
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '$')
        .count();
    let Some(r) = s[ident..].trim_start().strip_prefix('=') else {
        return false;
    };
    let r = r.trim_start();
    after_percents(r).is_some()
        || r.strip_prefix('!')
            .is_some_and(|x| x.starts_with(|c: char| c.is_ascii_alphabetic()))
}

/// `^\s*(# )*[^\s]*\?\s*$`: IPython help, `obj?`.
fn ipython_help(line: &str) -> bool {
    let mut s = line.trim_start();
    while let Some(r) = s.strip_prefix("# ") {
        s = r;
    }
    let s = s.trim_end();
    s.ends_with('?') && !s.contains(char::is_whitespace)
}

/// `^(# |#)*(cat|cd|…)($|\s$|\s[^=,])`: a shell command at column 0.
fn magic_cmd(line: &str) -> bool {
    const CMDS: [&str; 13] = [
        "cat", "cd", "cp", "mv", "rm", "rmdir", "mkdir", "copy", "ddir", "echo", "ls", "ldir",
        "ren",
    ];
    let s = strip_hashes(line);
    CMDS.iter().any(|cmd| {
        s.strip_prefix(cmd).is_some_and(|rest| {
            let mut chars = rest.chars();
            match chars.next() {
                None => true,
                Some(c) if c.is_whitespace() => !matches!(chars.next(), Some('=' | ',')),
                _ => false,
            }
        })
    })
}

fn continues(line: &str) -> bool {
    line.trim_end().ends_with('\\')
}

/// Comment magics out with `# ` (a real comment that looks like one gets a
/// second `# `, so both survive the round trip).
fn comment_magics(source: &mut [String]) {
    let mut parser = StringParser::default();
    let mut next_is_magic = false;
    for line in source.iter_mut() {
        if !parser.is_quoted() && (next_is_magic || is_magic(line)) {
            let new = if next_is_magic {
                format!("# {line}")
            } else {
                let body = line.trim_start();
                format!("{}# {body}", &line[..line.len() - body.len()])
            };
            next_is_magic = continues(line);
            parser.read_line(line);
            *line = new;
        } else {
            parser.read_line(line);
        }
    }
}

fn uncomment_magics(source: &mut [String]) {
    let mut parser = StringParser::default();
    let mut next_is_magic = false;
    for line in source.iter_mut() {
        if !parser.is_quoted() && (next_is_magic || is_magic(line)) {
            next_is_magic = continues(line);
            let body = line.trim_start();
            let indent = &line[..line.len() - body.len()];
            let new = match body.strip_prefix("# ").or_else(|| body.strip_prefix('#')) {
                Some(rest) => format!("{indent}{rest}"),
                None => line.clone(),
            };
            parser.read_line(line);
            *line = new;
        } else {
            parser.read_line(line);
        }
    }
}

/// jupytext's `StringParser`: is the start of the next line inside a
/// multi-line string? (Then it is neither a magic nor a cell marker.)
#[derive(Default)]
struct StringParser {
    single: Option<char>,
    triple: Option<char>,
    triple_start: isize,
}

impl StringParser {
    fn is_quoted(&self) -> bool {
        self.single.is_some() || self.triple.is_some()
    }

    fn read_line(&mut self, line: &str) {
        if !self.is_quoted() && line.trim_start().starts_with('#') {
            return;
        }
        self.triple_start = -1;
        let chars: Vec<char> = line.chars().collect();
        for (i, &c) in chars.iter().enumerate() {
            if self.single.is_none() && self.triple.is_none() && c == '#' {
                break;
            }
            if c != '"' && c != '\'' {
                continue;
            }
            if i > 0 && chars[i - 1] == '\\' {
                continue;
            }
            if self.single == Some(c) {
                self.single = None;
                continue;
            }
            if self.single.is_some() {
                continue;
            }
            if i >= 2
                && chars[i - 2] == c
                && chars[i - 1] == c
                && i as isize >= self.triple_start + 3
            {
                if self.triple == Some(c) {
                    self.triple = None;
                    self.triple_start = i as isize;
                    continue;
                }
                if self.triple.is_some() {
                    continue;
                }
                self.triple = Some(c);
                self.triple_start = i as isize;
                continue;
            }
            if self.triple.is_some() {
                continue;
            }
            self.single = Some(c);
        }
        self.single = None;
    }
}

// --- blank lines between cells (jupytext/pep8.py) ---

fn is_def(line: &str) -> bool {
    line.starts_with("def ") || line.starts_with("async ") || line.starts_with("class ")
}

/// Is the first instruction of the cell a function or class?
fn next_is_def(lines: &[&str]) -> bool {
    let mut parser = StringParser::default();
    for (i, line) in lines.iter().enumerate() {
        if parser.is_quoted() {
            parser.read_line(line);
            continue;
        }
        parser.read_line(line);
        if line.trim().is_empty() {
            if i > 0 && lines[i - 1].trim().is_empty() {
                return false;
            }
            continue;
        }
        if is_def(line) {
            return true;
        }
        if line.starts_with(['#', '@', ' ', ')']) {
            continue;
        }
        return false;
    }
    false
}

/// Does the cell end inside a function or class body?
fn ends_with_def(lines: &[&str]) -> bool {
    let mut parser = StringParser::default();
    let mut code = Vec::new();
    for line in lines {
        if !parser.is_quoted() {
            code.push(*line);
        }
        parser.read_line(line);
    }
    code.reverse();
    for (i, line) in code.iter().enumerate() {
        if line.trim().is_empty() {
            if i > 0 && code[i - 1].trim().is_empty() {
                return false;
            }
            continue;
        }
        if line.starts_with(['#', ' ', ')']) {
            continue;
        }
        return is_def(line);
    }
    false
}

fn ends_with_code(lines: &[&str]) -> bool {
    lines
        .last()
        .is_some_and(|l| !l.trim().is_empty() && !l.starts_with('#'))
}

fn has_code(lines: &[&str]) -> bool {
    for (i, line) in lines.iter().enumerate() {
        let s = line.trim();
        if s.starts_with('#') {
            continue;
        }
        if s.is_empty() {
            if i > 0 && lines[i - 1].trim().is_empty() {
                return false;
            }
            continue;
        }
        return true;
    }
    false
}

/// Blank lines between two cells' text, as pep8 wants them.
fn pep8_lines_between(prev: &[&str], next: &[&str]) -> usize {
    if next.is_empty() {
        return 1;
    }
    if prev.is_empty() {
        return 0;
    }
    if ends_with_def(prev) {
        return if has_code(next) { 2 } else { 1 };
    }
    if ends_with_code(prev) && next_is_def(next) {
        return 2;
    }
    1
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> String {
        let path = format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
        std::fs::read_to_string(path).unwrap()
    }

    /// The text after jupytext's header (and the blank line after it).
    fn body(fixture: &str) -> &str {
        let at = fixture.find("\n# ---\n\n").unwrap();
        &fixture[at + "\n# ---\n\n".len()..]
    }

    fn sources(nb: &Notebook) -> Vec<(&str, &str)> {
        nb.cells
            .iter()
            .map(|c| (c.cell_type.as_str(), c.source.as_str()))
            .collect()
    }

    // Fixtures generated by jupytext itself (see the header for its version):
    //   uvx jupytext --to py:percent test.ipynb -o tests/fixtures/test.py
    //   uvx jupytext --to py:percent showcase.ipynb -o tests/fixtures/showcase.py
    //   uvx jupytext --to py:percent tests/fixtures/magics.ipynb -o tests/fixtures/magics.py

    #[test]
    fn jupytext_files_round_trip_byte_for_byte() {
        for name in ["test.py", "showcase.py", "magics.py"] {
            let text = fixture(name);
            assert_eq!(write(&read(&text)), text, "{name}");
        }
    }

    #[test]
    fn reading_matches_the_notebook_jupytext_wrote_it_from() {
        for (py, ipynb) in [
            ("test.py", "/test.ipynb"),
            ("showcase.py", "/showcase.ipynb"),
            ("magics.py", "/tests/fixtures/magics.ipynb"),
        ] {
            let nb: Notebook = serde_json::from_str(
                &std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR")).to_string() + ipynb)
                    .unwrap(),
            )
            .unwrap();
            let got = read(&fixture(py));
            assert_eq!(sources(&got), sources(&nb), "{py}");
            assert_eq!(
                got.extra["metadata"]["kernelspec"],
                nb.extra["metadata"]["kernelspec"]
            );
        }
    }

    #[test]
    fn converting_a_notebook_matches_jupytext() {
        for (ipynb, py) in [
            ("/test.ipynb", "test.py"),
            ("/showcase.ipynb", "showcase.py"),
            ("/tests/fixtures/magics.ipynb", "magics.py"),
        ] {
            let path = concat!(env!("CARGO_MANIFEST_DIR")).to_string() + ipynb;
            let nb = Notebook::read_ipynb(std::path::Path::new(&path)).unwrap();
            assert_eq!(write(&nb), body(&fixture(py)), "{py}");
        }
    }

    #[test]
    fn markers() {
        for m in [
            "# %%",
            "#%%",
            "# %% [markdown]",
            "  # %% x=1",
            "# %%% deeper",
            "# In[3]:",
            "# In[ ]:",
            "# <codecell>",
            "# %%\t",
        ] {
            assert!(is_marker(m), "{m:?}");
        }
        for m in [
            "# %%time", "# %%%", "x = 1", "# % not", "#%%bash", "# In[a]:",
        ] {
            assert!(!is_marker(m), "{m:?}");
        }
    }

    #[test]
    fn marker_options() {
        let (ty, meta) = parse_options("# %% My title [markdown] tags=[\"a\", \"b\"] x=1 flag");
        assert_eq!(ty, "markdown");
        assert_eq!(meta["title"], "My title");
        assert_eq!(meta["tags"], serde_json::json!(["a", "b"]));
        assert_eq!(meta["x"], 1);
        assert_eq!(meta["flag"], Value::Null);
        let (ty, meta) = parse_options("# %% [raw] {\"format\": \"text/latex\"}");
        assert_eq!((ty, &meta["format"]), ("raw", &"text/latex".into()));
        let (ty, meta) = parse_options("# %%% [md] sub");
        assert_eq!(
            (
                ty,
                &meta["cell_depth"],
                &meta["region_name"],
                &meta["title"]
            ),
            ("markdown", &1.into(), &"md".into(), &"sub".into())
        );
        // an unparseable tail is kept whole, so nothing is lost
        let (_, meta) = parse_options("# %% a=b=c");
        assert_eq!(meta["incorrectly_encoded_metadata"], "a=b=c");
        // and all of it writes back the same
        for line in [
            "# %% My title [markdown] tags=[\"a\", \"b\"] x=1 flag",
            "# %% [raw] format=\"text/latex\"",
            "# %%% sub [md]",
            "# %% a=b=c",
            "# %% s={\"k\": [1, \"\\u00e9\"]}",
        ] {
            let nb = read(&format!("{line}\nx\n\n# %%\ny\n"));
            assert_eq!(write(&nb).lines().next().unwrap(), line);
        }
    }

    #[test]
    fn magics_comment_and_uncomment() {
        let magic = [
            "%matplotlib inline",
            "%%time",
            "!ls -l",
            "?np.sum",
            "x?",
            "    %cd ..",
            "x = !ls",
            "cd foo",
            "ls",
            "echo hi",
        ];
        let not = [
            "x = 1",
            "# comment",
            "ls = [1]",
            "cd(x)",
            "a ? b",
            "%",
            "%%",
            "cat, dog",
            "  # %%",
        ];
        for m in magic {
            assert!(is_magic(m), "{m:?}");
        }
        for m in not {
            assert!(!is_magic(m), "{m:?}");
        }
        let mut src: Vec<String> = [
            "%matplotlib inline",
            "# %x is a comment",
            "s = \"\"\"",
            "%not code",
            "\"\"\"",
            "%x \\",
            "  continued",
            "%y # noescape",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let orig = src.clone();
        comment_magics(&mut src);
        assert_eq!(
            src,
            [
                "# %matplotlib inline",
                "# # %x is a comment",
                "s = \"\"\"",
                "%not code",
                "\"\"\"",
                "# %x \\",
                "#   continued",
                "%y # noescape"
            ]
        );
        uncomment_magics(&mut src);
        assert_eq!(src, orig);
    }

    #[test]
    fn plain_scripts_stay_plain() {
        for text in [
            "x = 1\nprint(x)\n",
            "",
            "#!/usr/bin/env python\nimport sys\n\n\n",
            "# %matplotlib inline\nimport os\n",
        ] {
            let nb = read(text);
            assert_eq!(nb.cells.len(), 1);
            assert_eq!(write(&nb), text, "{text:?}");
        }
        // a magic in a script is commented, as jupytext does, and read back
        assert_eq!(
            write(&read("%matplotlib inline\n")),
            "# %matplotlib inline\n"
        );
        assert_eq!(
            read("# %matplotlib inline\n").cells[0].source,
            "%matplotlib inline"
        );
        // the whole script is one cell, magics and all
        assert_eq!(read("x = 1\nprint(x)\n").cells[0].source, "x = 1\nprint(x)");
        // a second cell brings the markers
        let mut nb = read("x = 1\n");
        nb.cells.push(Cell::new_code());
        assert_eq!(write(&nb), "# %%\nx = 1\n\n# %%\n");
        // and so does a markdown cell
        let mut nb = read("text\n");
        nb.cells[0].cell_type = "markdown".into();
        assert_eq!(write(&nb), "# %% [markdown]\n# text\n");
    }

    #[test]
    fn header_is_verbatim_and_yields_the_kernelspec() {
        let text = "#!/usr/bin/env python\n# ---\n# jupyter:\n#   kernelspec:\n#     display_name: 'My Env'\n#     language: python\n#     name: \"my-env\"\n#   other: 1\n# ---\n\n# %%\nx\n";
        let nb = read(text);
        assert_eq!(
            nb.extra["metadata"]["kernelspec"],
            serde_json::json!({"display_name": "My Env", "language": "python", "name": "my-env"})
        );
        assert_eq!(write(&nb), text);
        // a lone marked cell under a header keeps its marker
        assert_eq!(nb.cells.len(), 1);
        // content before the first marker is a cell of its own
        let nb = read("import os\n\n# %%\nx\n");
        assert_eq!(sources(&nb), [("code", "import os"), ("code", "x")]);
        assert_eq!(write(&nb), "# %%\nimport os\n\n# %%\nx\n");
    }

    #[test]
    fn markdown_and_raw_lines() {
        let nb = read(
            "# %% [markdown]\n# # Title\n#\n# ![img](x.png)\n#   indented\n\n# %% [raw]\n# raw\n",
        );
        assert_eq!(
            sources(&nb),
            [
                ("markdown", "# Title\n\n![img](x.png)\n  indented"),
                ("raw", "raw")
            ]
        );
        assert!(nb.cells[0].outputs.is_none());
        assert_eq!(nb.cells[1].outputs, None);
    }

    #[test]
    fn spacing_follows_pep8_and_is_remembered_otherwise() {
        // two blank lines before a def, one otherwise; a cell marker inside
        // a string is content
        let text = "# %%\nx = 1\n\n\n# %%\ndef f():\n    return 1\n\n\n# %%\ns = \"\"\"\n# %%\n\"\"\"\n\n# %% [markdown]\n# end\n";
        let nb = read(text);
        assert_eq!(nb.cells.len(), 4);
        assert_eq!(nb.cells[2].source, "s = \"\"\"\n# %%\n\"\"\"");
        assert!(
            !nb.cells[0].extra["metadata"]
                .as_object()
                .unwrap()
                .contains_key("lines_to_next_cell")
        );
        assert_eq!(write(&nb), text);
        // non-pep8 spacing is kept through the metadata
        let text = "# %%\nx = 1\n# %%\ny = 2\n\n\n\n# %%\nz\n";
        let nb = read(text);
        assert_eq!(nb.cells[0].extra["metadata"]["lines_to_next_cell"], 0);
        assert_eq!(write(&nb), text);
        // a fresh cell has no marker-line metadata
        assert!(filtered_metadata(&Cell::new_code()).is_empty());
    }

    #[test]
    fn indented_first_line_indents_the_marker() {
        let text = "# %%\nif True:\n    pass\n\n    # %%\n    x = 1\n";
        let nb = read(text);
        assert_eq!(nb.cells[1].source, "    x = 1");
        assert_eq!(write(&nb), text);
    }

    #[test]
    fn python_json_formatting() {
        assert_eq!(
            py_json(&serde_json::json!({"a": [1, 2.5, "é", null, true], "b": {"c": "x"}})),
            r#"{"a": [1, 2.5, "\u00e9", null, true], "b": {"c": "x"}}"#
        );
        assert_eq!(py_json(&serde_json::json!("🦦")), r#""\ud83e\udda6""#);
    }

    // --- differential checks against the jupytext on PATH ---
    // cargo test jupytext -- --ignored --nocapture

    /// `JUPYTEXT="uvx jupytext@1.19.6"` picks another jupytext.
    fn jupytext(args: &[&str]) -> String {
        let bin = std::env::var("JUPYTEXT").unwrap_or_else(|_| "jupytext".into());
        let mut words = bin.split_whitespace();
        let out = std::process::Command::new(words.next().unwrap())
            .args(words)
            .args(args)
            .output()
            .expect("jupytext on PATH");
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap()
    }

    /// Everything after the closing `# ---` and the blank lines following it.
    fn strip_header(text: &str) -> &str {
        let Some(open) = text.find("# ---\n") else {
            return text;
        };
        let rest = &text[open + "# ---\n".len()..];
        match rest.find("\n# ---\n") {
            Some(at) => rest[at + "\n# ---\n".len()..].trim_start_matches('\n'),
            None => text,
        }
    }

    fn cells_of(nb: &Notebook) -> Vec<(String, String, Value)> {
        nb.cells
            .iter()
            .map(|c| {
                (
                    c.cell_type.clone(),
                    c.source.clone(),
                    c.extra["metadata"].clone(),
                )
            })
            .collect()
    }

    const CORPUS: [(&str, &str); 20] = [
        (
            "options",
            "# %% My title [markdown] tags=[\"a\", \"b\"] x=1 flag\n# hi\n\n# %%\nx\n",
        ),
        ("plain", "x = 1\nprint(x)\n"),
        (
            "magics",
            "# %%\n# %matplotlib inline\nimport os\n\n# %%\n# %%time\nx = 1\n\n# %%\n# !ls -la\n# x?\n# ?x\n# y = !pwd\n# cd ..\n# ls\n# echo hi\n# %x \\\n#   continued\n%y # noescape\n# %z # escape\n# # %w\n# # !v\n",
        ),
        (
            "strings",
            "# %%\ns = \"\"\"\n# %%\n%not magic\n\"\"\"\nt = '''# %%'''\n\n# %%\ny\n",
        ),
        (
            "spacing",
            "# %%\nx = 1\n\n\n# %%\ndef f():\n    return 1\n\n\n# %%\nclass A:\n    pass\n\n# %% [markdown]\n# after class\n\n# %%\ny = 2\n# %%\nz = 3\n\n\n\n# %%\nw\n",
        ),
        (
            "indent",
            "# %%\nif True:\n    pass\n\n    # %%\n    x = 1\n",
        ),
        (
            "markdown",
            "# %% [markdown]\n# # Title\n#\n# # %x\n# ![i](x.png)\n#   indented\n\n# %%\nz\n",
        ),
        (
            "rawjson",
            "# %% [raw] {\"format\": \"text/latex\", \"k\": [1, 2]}\n# E\n\n# %%\nx\n",
        ),
        ("empty", "# %%\n\n# %%\n\n# %% [markdown]\n\n# %%\nx\n"),
        ("depth", "# %%% [md] sub\n# text\n\n# %%%% deeper\nx\n"),
        (
            "nbconvert",
            "# In[1]:\n\n\nimport os\n\n\n# In[ ]:\n\n\nx = 1\n\n",
        ),
        (
            "shebang",
            "#!/usr/bin/env python\n# ---\n# jupyter:\n#   kernelspec:\n#     display_name: Python 3\n#     language: python\n#     name: python3\n# ---\n\n# %%\nx\n",
        ),
        ("leading", "import os\n\n# %%\nx\n"),
        (
            "unicode",
            "# %% [markdown] t=\"é\"\n# héllo 🦦\n\n# %%\nprint(\"ü\")\n",
        ),
        ("whitespace", "# %%\nx = 1\n   \n# %%\ny\n  \n"),
        ("titles", "# %% Setup\nx = 1\n\n# %% Another one\ny\n"),
        ("active", "# %% active=\"\"\nx\n\n# %%\ny\n"),
        ("trailing", "# %%\nx\n\n\n\n"),
        ("nomarkerblank", "\n\nx = 1\n"),
        ("hashmarker", "# %%\n# # %% not a marker\nx\n"),
    ];

    /// How the installed jupytext reads and rewrites each case, against us.
    /// Deviations are listed with their reason; anything else fails.
    #[test]
    #[ignore = "needs jupytext on PATH"]
    fn installed_jupytext_agrees() {
        let dir = std::env::temp_dir().join(format!("jotter-jt-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let fixtures = ["test.py", "showcase.py", "magics.py"].map(|f| (f, fixture(f)));
        let mut cases: Vec<(&str, String)> =
            CORPUS.iter().map(|(n, t)| (*n, t.to_string())).collect();
        cases.extend(fixtures.iter().map(|(n, t)| (*n, t.clone())));
        let mut deviations = Vec::new();
        for (name, text) in &cases {
            let path = dir.join(format!("{name}.py"));
            std::fs::write(&path, text).unwrap();
            let path = path.to_str().unwrap();
            // reading
            // `--from`: left to guess, jupytext takes a file whose only
            // markers are `# %%%` sub-cells (or none) for the light format
            let theirs: Notebook = serde_json::from_str(&jupytext(&[
                "--from",
                "py:percent",
                "--to",
                "ipynb",
                "-o",
                "-",
                path,
            ]))
            .unwrap();
            let ours = read(text);
            if cells_of(&theirs) != cells_of(&ours) {
                deviations.push((*name, "read"));
                eprintln!(
                    "[{name}] READ differs\n jupytext: {:?}\n jotter:   {:?}",
                    cells_of(&theirs),
                    cells_of(&ours)
                );
            }
            // rewriting
            let theirs = jupytext(&[
                "--from",
                "py:percent",
                "--to",
                "py:percent",
                "-o",
                "-",
                path,
            ]);
            let ours = write(&ours);
            let (theirs, ours) = (strip_header(&theirs), strip_header(&ours));
            if theirs != ours {
                deviations.push((*name, "write"));
                eprintln!("[{name}] WRITE differs\n jupytext: {theirs:?}\n jotter:   {ours:?}");
            }
        }
        std::fs::remove_dir_all(&dir).unwrap();
        // a whitespace-only line before a marker: jupytext < 1.19 drops it,
        // 1.19 keeps it as content (#1599), as we do
        deviations.retain(|d| d.0 != "whitespace");
        let mut expected = vec![
            // bare keys: we write them last so they read back as keys; jupytext
            // writes them in dict order, and then reads them as title words
            ("options", "write"),
            // a lone code cell without a header is a plain script and gets no
            // marker; jupytext always writes one
            ("plain", "write"),
            ("trailing", "write"),
            ("nomarkerblank", "write"),
            ("hashmarker", "write"),
            // a markdown line `# %x`: jupytext uncomments it as a magic and
            // loses the `#`, so its own round trip isn't stable there
            ("markdown", "read"),
            ("markdown", "write"),
            // jupytext remembers the `{…}` style in notebook metadata
            // (`jupytext.cell_metadata_json`) and writes it back as JSON
            ("rawjson", "write"),
            // jupytext's active cells (`active=""`, `active-*` tags) become
            // raw cells in the notebook; we keep the metadata and the code type
            ("active", "read"),
            ("active", "write"),
        ];
        expected.sort_unstable();
        deviations.sort_unstable();
        assert_eq!(deviations, expected, "unexpected deviations from jupytext");
    }

    /// The reporter's workflow: a paired notebook, the `.py` edited by jotter,
    /// `jupytext --sync` carries the edit into the `.ipynb` and leaves the
    /// `.py` as jotter wrote it.
    #[test]
    #[ignore = "needs jupytext on PATH"]
    fn paired_notebook_syncs_a_jotter_edit() {
        let dir = std::env::temp_dir().join(format!("jotter-pair-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let ipynb = dir.join("nb.ipynb");
        let py = dir.join("nb.py");
        std::fs::copy(
            concat!(env!("CARGO_MANIFEST_DIR"), "/showcase.ipynb"),
            &ipynb,
        )
        .unwrap();
        jupytext(&["--set-formats", "ipynb,py:percent", ipynb.to_str().unwrap()]);
        let paired = std::fs::read_to_string(&py).unwrap();
        // jotter rewrites the paired file byte for byte, pairing header included
        let mut nb = Notebook::open(&py).unwrap();
        assert_eq!(write(&nb), paired);
        // an edit in jotter
        let idx = nb.cells.iter().position(|c| c.cell_type == "code").unwrap();
        nb.cells[idx].source = "edited = 'in jotter'".into();
        nb.save(&py).unwrap();
        let edited = std::fs::read_to_string(&py).unwrap();
        assert_ne!(edited, paired);
        jupytext(&["--sync", py.to_str().unwrap()]);
        // the .ipynb took the edit and kept its outputs elsewhere
        let synced = Notebook::read_ipynb(&ipynb).unwrap();
        assert_eq!(synced.cells[idx].source, "edited = 'in jotter'");
        let outputs = synced
            .cells
            .iter()
            .filter(|c| c.outputs.as_ref().is_some_and(|o| !o.is_empty()))
            .count();
        assert!(outputs > 0, "outputs survive the sync");
        // and the .py is still what jotter wrote
        assert_eq!(std::fs::read_to_string(&py).unwrap(), edited);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
