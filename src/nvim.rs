//! Embedded `nvim --embed --headless` as the cell editor's modal engine
//! (experiment; `editor = "nvim"` in config.toml). jotter keeps rendering
//! everything — nvim owns buffer text, cursor, mode, registers, and macros.
//!
//! Keys go in via `nvim_input`; state comes back with one deferred
//! `nvim_eval` that doubles as a flush barrier (it is answered only after the
//! queued input has been processed). Two round-trips per key on a local pipe
//! (~100 µs each), so the RPC is plain blocking calls from the key handler —
//! no async plumbing.
//! ponytail: full-buffer readback per key instead of nvim_buf_attach diffs —
//! cells are tens of lines; switch to buf_attach if cells ever get huge.

use crate::editor::{Editor, Mode as EdMode, Outcome};
use anyhow::{Context, Result, anyhow, bail};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use rmpv::Value;
use std::collections::HashMap;
use std::io::Write;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

/// Per-key RPC budget. A local pipe answers in ~100 µs; hitting this means
/// nvim sits in a blocking prompt (hit-enter) or is replaying a long macro —
/// we keep the last known state and resync on the next key.
const KEY: Duration = Duration::from_millis(250);
/// Startup budget: a user config (plugin manager, ...) loads before the
/// first request is answered.
const SETUP: Duration = Duration::from_secs(10);

/// Editor mode, coarse enough for rendering (chip, cursor shape, selection).
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum ModeKind {
    Normal,
    Insert,
    Visual,
    VisualLine,
    VisualBlock,
    Replace,
    Pending,
    Cmdline,
}

/// The cell editor: the builtin vi subset, or a real nvim behind it.
pub enum Backend {
    Builtin(Editor),
    Nvim(NvimCell),
}

impl Backend {
    pub fn source(&self) -> String {
        match self {
            Backend::Builtin(e) => e.source(),
            Backend::Nvim(c) => c.lines.join("\n"),
        }
    }

    pub fn line_count(&self) -> usize {
        match self {
            Backend::Builtin(e) => e.lines.len(),
            Backend::Nvim(c) => c.lines.len(),
        }
    }

    /// (row, col) in characters.
    pub fn cursor(&self) -> (usize, usize) {
        match self {
            Backend::Builtin(e) => e.cursor,
            Backend::Nvim(c) => c.cursor,
        }
    }

    pub fn mode(&self) -> ModeKind {
        match self {
            Backend::Builtin(e) => match e.mode {
                EdMode::Normal => ModeKind::Normal,
                EdMode::Insert => ModeKind::Insert,
            },
            Backend::Nvim(c) => c.mode_kind(),
        }
    }

    pub fn is_builtin(&self) -> bool {
        matches!(self, Backend::Builtin(_))
    }

    /// Err means the nvim child is gone — the caller falls back to the
    /// builtin editor with `source()` (still served from the cache).
    pub fn input(&mut self, key: KeyEvent) -> Result<Outcome> {
        match self {
            Backend::Builtin(e) => Ok(e.input(key)),
            Backend::Nvim(c) => {
                // jotter-level keys, checked against the last-known mode
                if key.code == KeyCode::Enter
                    && key
                        .modifiers
                        .intersects(KeyModifiers::SHIFT | KeyModifiers::CONTROL)
                {
                    return Ok(Outcome::Run {
                        advance: key.modifiers.contains(KeyModifiers::SHIFT),
                    });
                }
                // exactly "n": Esc must still reach nvim in "no" (cancel
                // operator), "niI" (back to insert), visual, cmdline, ...
                if key.code == KeyCode::Esc && c.mode == "n" {
                    return Ok(Outcome::Exit);
                }
                if let Some(keys) = key_to_nvim(key) {
                    c.feed(&keys)?;
                }
                Ok(Outcome::Continue)
            }
        }
    }

    pub fn click(&mut self, row: usize, col: usize, insert: bool) {
        match self {
            Backend::Builtin(e) => e.click(row, col, insert),
            Backend::Nvim(c) => c.click(row, col, insert),
        }
    }

    /// `A` from cell-selection mode: cursor to last line's end, insert mode.
    pub fn append_end(&mut self) {
        match self {
            Backend::Builtin(e) => e.append_end(),
            Backend::Nvim(c) => {
                let _ = c.feed("GA");
            }
        }
    }

    /// Replace chars [start, end) of the source with `text` and put the
    /// cursor after it (kernel completions).
    pub fn replace_range(&mut self, start: usize, end: usize, text: &str) -> Result<()> {
        match self {
            Backend::Builtin(e) => {
                e.replace_chars(start, end, text);
                Ok(())
            }
            Backend::Nvim(c) => c.replace_range(start, end, text),
        }
    }

    /// Visual selection for rendering: (kind, anchor, cursor) in char coords.
    #[allow(clippy::type_complexity)] // two (row, col) pairs, not worth a type
    pub fn visual(&self) -> Option<(ModeKind, (usize, usize), (usize, usize))> {
        match self {
            Backend::Builtin(_) => None,
            Backend::Nvim(c) => match c.mode_kind() {
                k @ (ModeKind::Visual | ModeKind::VisualLine | ModeKind::VisualBlock) => {
                    Some((k, c.anchor, c.cursor))
                }
                _ => None,
            },
        }
    }

    /// Command line being typed inside nvim (`:`/`/`/`?` + content), if any.
    pub fn cmdline(&self) -> Option<&str> {
        match self {
            Backend::Builtin(_) => None,
            Backend::Nvim(c) => (!c.cmdline.is_empty()).then_some(c.cmdline.as_str()),
        }
    }

    /// Status message from the backend (nvim closed a foreign window).
    pub fn take_notice(&mut self) -> Option<String> {
        match self {
            Backend::Builtin(_) => None,
            Backend::Nvim(c) => c.take_notice(),
        }
    }

    /// `:w` inside the cell fired (BufWriteCmd): commit without exiting.
    pub fn take_write_request(&mut self) -> bool {
        match self {
            Backend::Builtin(_) => false,
            Backend::Nvim(c) => c.session.rpc.write_requested.swap(false, Ordering::Relaxed),
        }
    }

    /// Quit intent from inside nvim (`:q`, `:wq`, `ZZ`, ...): leave the cell.
    /// `Some(true)` = discard the edit (`:q!`, `ZQ`) instead of committing.
    pub fn take_quit_request(&mut self) -> Option<bool> {
        match self {
            Backend::Builtin(_) => None,
            Backend::Nvim(c) => match c.session.rpc.quit_requested.swap(0, Ordering::Relaxed) {
                1 => Some(false),
                2 => Some(true),
                _ => None,
            },
        }
    }

    /// Reclaim the nvim session on commit so per-cell buffers (undo history,
    /// marks) survive leaving and re-entering cells.
    pub fn into_session(self) -> Option<NvimSession> {
        match self {
            Backend::Builtin(_) => None,
            Backend::Nvim(c) => Some(c.session),
        }
    }
}

/// crossterm `KeyEvent` -> nvim key notation for `nvim_input()`.
/// None: a key nvim has no notation for (media keys, caps lock, ...).
fn key_to_nvim(key: KeyEvent) -> Option<String> {
    use KeyCode::*;
    // Shift is already baked into Char payloads; it only matters for specials.
    let mut shift = key.modifiers.contains(KeyModifiers::SHIFT);
    let (name, special) = match key.code {
        Char(' ') => ("Space".into(), true),
        Char('<') => ("lt".into(), true),
        Char(c) => {
            shift = false;
            (c.to_string(), false)
        }
        Enter => ("CR".into(), true),
        Esc => ("Esc".into(), true),
        Backspace => ("BS".into(), true),
        Tab => ("Tab".into(), true),
        BackTab => {
            shift = true;
            ("Tab".into(), true)
        }
        Left => ("Left".into(), true),
        Right => ("Right".into(), true),
        Up => ("Up".into(), true),
        Down => ("Down".into(), true),
        Home => ("Home".into(), true),
        End => ("End".into(), true),
        PageUp => ("PageUp".into(), true),
        PageDown => ("PageDown".into(), true),
        Delete => ("Del".into(), true),
        Insert => ("Insert".into(), true),
        F(n) => (format!("F{n}"), true),
        _ => return None,
    };
    let mut mods = String::new();
    if key.modifiers.contains(KeyModifiers::CONTROL) {
        mods.push_str("C-");
    }
    if key.modifiers.contains(KeyModifiers::ALT) {
        mods.push_str("M-");
    }
    if special && shift {
        mods.push_str("S-");
    }
    Some(if special || !mods.is_empty() {
        format!("<{mods}{name}>")
    } else {
        name
    })
}

/// Blocking msgpack-RPC over the child's pipes. A reader thread parses frames;
/// responses land on a channel, the lone notification we register flips a flag.
struct Rpc {
    child: Child,
    stdin: ChildStdin,
    responses: mpsc::Receiver<(u64, Result<Value, String>)>,
    write_requested: Arc<AtomicBool>,
    /// Quit intent from inside nvim (0 none, 1 commit+exit, 2 discard+exit).
    quit_requested: Arc<AtomicU8>,
    next_id: u64,
}

fn rpc_error(err: &Value) -> String {
    // errors arrive as [type, message]
    match err {
        Value::Array(a) => a
            .iter()
            .rev()
            .find_map(|v| v.as_str())
            .unwrap_or("unknown error")
            .into(),
        v => v.to_string(),
    }
}

impl Rpc {
    fn spawn(user_config: bool) -> Result<Self> {
        // -i NONE -n: no shada/swap files, ever. With the user config,
        // g:jotter is set before init.lua/init.vim so they can opt out of
        // UI-only plugins (`if vim.g.jotter then ... end`).
        let init: &[&str] = if user_config {
            &["--cmd", "let g:jotter = 1"]
        } else {
            &["-u", "NONE"]
        };
        let mut child = Command::new("nvim")
            .args(["--embed", "--headless", "-i", "NONE", "-n"])
            .args(init)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .context("spawning nvim (is it installed?)")?;
        let stdin = child.stdin.take().expect("piped stdin");
        let stdout = child.stdout.take().expect("piped stdout");
        let (tx, responses) = mpsc::channel();
        let write_requested = Arc::new(AtomicBool::new(false));
        let quit_requested = Arc::new(AtomicU8::new(0));
        let (wflag, qflag) = (write_requested.clone(), quit_requested.clone());
        std::thread::spawn(move || {
            let mut r = std::io::BufReader::new(stdout);
            while let Ok(v) = rmpv::decode::read_value(&mut r) {
                let Value::Array(items) = v else { continue };
                match items.as_slice() {
                    // response: [1, msgid, error, result]
                    [t, id, err, result] if t.as_u64() == Some(1) => {
                        let Some(id) = id.as_u64() else { continue };
                        let payload = if err.is_nil() {
                            Ok(result.clone())
                        } else {
                            Err(rpc_error(err))
                        };
                        if tx.send((id, payload)).is_err() {
                            break;
                        }
                    }
                    // notification: [2, method, params] — jotter hooks only
                    [t, method, _] if t.as_u64() == Some(2) => match method.as_str() {
                        Some("jotter_write") => wflag.store(true, Ordering::Relaxed),
                        Some("jotter_quit") => qflag.store(1, Ordering::Relaxed),
                        Some("jotter_bail") => qflag.store(2, Ordering::Relaxed),
                        _ => {}
                    },
                    _ => {}
                }
            }
        });
        Ok(Self {
            child,
            stdin,
            responses,
            write_requested,
            quit_requested,
            next_id: 0,
        })
    }

    fn request(&mut self, method: &str, params: Vec<Value>, timeout: Duration) -> Result<Value> {
        self.next_id += 1;
        let id = self.next_id;
        let msg = Value::Array(vec![
            0u64.into(),
            id.into(),
            method.into(),
            Value::Array(params),
        ]);
        rmpv::encode::write_value(&mut self.stdin, &msg).context("nvim rpc write")?;
        self.stdin.flush().context("nvim rpc flush")?;
        let deadline = Instant::now() + timeout;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.responses.recv_timeout(left) {
                Ok((rid, payload)) if rid == id => {
                    return payload.map_err(|e| anyhow!("nvim: {e}"));
                }
                Ok(_) => {} // stale response of a timed-out request: drop
                Err(mpsc::RecvTimeoutError::Timeout) => bail!("nvim rpc timeout ({method})"),
                Err(mpsc::RecvTimeoutError::Disconnected) => bail!("nvim exited"),
            }
        }
    }
}

impl Drop for Rpc {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// One nvim child per jotter session, spawned lazily on first cell edit.
pub struct NvimSession {
    rpc: Rpc,
    /// cell key -> nvim buffer handle (msgpack EXT value, echoed back verbatim)
    buffers: HashMap<String, Value>,
}

impl NvimSession {
    /// `user_config`: load the user's nvim config (config.nvim_user_config).
    pub fn spawn(user_config: bool) -> Result<Self> {
        let mut rpc = Rpc::spawn(user_config)?;
        // channel id (for rpcnotify) is the first element of the api info
        let info = rpc.request("nvim_get_api_info", vec![], SETUP)?;
        let chan = info
            .as_array()
            .and_then(|a| a.first())
            .and_then(|v| v.as_u64())
            .ok_or_else(|| anyhow!("bad nvim_get_api_info reply"))?;
        // :w commits to the notebook instead of E32-ing (cell buffers are
        // buftype=acwrite, so writes route through the autocmd), and quit
        // intent (:q/:wq/:x/ZZ/ZQ) leaves the cell instead of exiting the
        // embedded nvim — quits can't be vetoed after the fact, so the common
        // spellings are rewritten via cmdline abbreviations before they run.
        let mut setup = format!(
            "autocmd BufWriteCmd jotter://* call rpcnotify({chan}, 'jotter_write') | setlocal nomodified\n\
             command! -bang JotterQ call rpcnotify({chan}, <bang>0 ? 'jotter_bail' : 'jotter_quit')\n\
             command! -bang JotterWq call rpcnotify({chan}, 'jotter_quit')\n\
             nnoremap ZZ <Cmd>JotterWq<CR>\n\
             nnoremap ZQ <Cmd>JotterQ!<CR>\n"
        );
        if !user_config {
            // no ftplugins without a config: Tab must still insert spaces
            // (a literal \t is an IndentationError waiting to happen)
            setup += "set expandtab tabstop=4 shiftwidth=4 softtabstop=4\n";
        }
        for (ab, cmd) in [
            ("q", "JotterQ"),
            ("qa", "JotterQ"),
            ("qall", "JotterQ"),
            ("quit", "JotterQ"),
            ("wq", "JotterWq"), // -bang: :wq! means "write even if readonly", never discard
            ("wqa", "JotterWq"),
            ("x", "JotterWq"),
            ("exit", "JotterWq"),
        ] {
            setup += &format!(
                "cnoreabbrev <expr> {ab} (getcmdtype() ==# ':' && getcmdline() ==# '{ab}') ? '{cmd}' : '{ab}'\n"
            );
        }
        rpc.request("nvim_exec2", vec![setup.into(), Value::Map(vec![])], SETUP)?;
        Ok(Self {
            rpc,
            buffers: HashMap::new(),
        })
    }

    fn req(&mut self, method: &str, params: Vec<Value>) -> Result<Value> {
        self.rpc.request(method, params, KEY)
    }

    fn cmd(&mut self, command: &str) -> Result<()> {
        self.req("nvim_command", vec![command.into()]).map(|_| ())
    }
}

/// An open editing session on one cell. Owns the session while editing;
/// `Backend::into_session` hands it back on commit.
pub struct NvimCell {
    session: NvimSession,
    /// Cached buffer state, refreshed after every key.
    pub lines: Vec<String>,
    /// (row, col) in characters.
    cursor: (usize, usize),
    /// Raw `mode(1)` string, e.g. "n", "i", "no", "v", "V", "\x16".
    mode: String,
    /// Visual anchor (`getpos('v')`); equals cursor outside visual mode.
    anchor: (usize, usize),
    /// `getcmdtype() .. getcmdline()`; empty outside cmdline mode.
    cmdline: String,
    /// nvim buffer number of this cell (`bufnr()`), to notice when a plugin
    /// switched away from it.
    bufnr: u64,
    /// One-shot status message for jotter (a foreign window was closed).
    notice: Option<String>,
}

/// Put nvim back into the cell: leave insert mode, close every floating
/// window (pickers, hovers, popups) and every other split, and show the cell
/// buffer again. `...` = the cell's buffer number.
const RECLAIM: &str = r#"
local buf = ...
pcall(vim.cmd, 'stopinsert')
-- closing one float can close its companions (telescope: prompt + results
-- + preview), so re-check validity for every window
for _, w in ipairs(vim.api.nvim_list_wins()) do
  if vim.api.nvim_win_is_valid(w) and vim.api.nvim_win_get_config(w).relative ~= '' then
    pcall(vim.api.nvim_win_close, w, true)
  end
end
pcall(vim.cmd, 'silent! only!')
if vim.api.nvim_get_current_buf() ~= buf then pcall(vim.api.nvim_set_current_buf, buf) end
"#;

impl NvimCell {
    /// Switch the session to this cell's buffer, creating it on first edit —
    /// per-cell buffers keep undo history and marks across cell switches.
    /// `filetype`: cell language ("python", "markdown", ...) so ftplugins,
    /// indent rules, and filetype-specific user settings apply.
    pub fn open(
        mut session: NvimSession,
        cell_key: &str,
        source: &str,
        filetype: &str,
    ) -> Result<Self> {
        let src_lines: Vec<Value> = source.split('\n').map(Value::from).collect();
        let existing = match session.buffers.get(cell_key).cloned() {
            Some(b) => session
                .req("nvim_buf_is_valid", vec![b.clone()])?
                .as_bool()
                .unwrap_or(false)
                .then_some(b),
            None => None,
        };
        match existing {
            Some(buf) => {
                session.req("nvim_set_current_buf", vec![buf.clone()])?;
                // buffer tracks the cell (external edit, reload); skip when
                // equal so cursor/marks stay exactly put
                let cur = session.req(
                    "nvim_buf_get_lines",
                    vec![buf.clone(), 0.into(), (-1).into(), false.into()],
                )?;
                let differs = cur.as_array().is_none_or(|a| {
                    a.len() != src_lines.len() || a.iter().zip(&src_lines).any(|(x, y)| x != y)
                });
                if differs {
                    session.req(
                        "nvim_buf_set_lines",
                        vec![
                            buf,
                            0.into(),
                            (-1).into(),
                            false.into(),
                            Value::Array(src_lines),
                        ],
                    )?;
                    session.cmd("setlocal nomodified")?;
                }
            }
            None => {
                let buf = session.req("nvim_create_buf", vec![false.into(), false.into()])?;
                session.req("nvim_set_current_buf", vec![buf.clone()])?;
                session.req(
                    "nvim_buf_set_name",
                    vec![buf.clone(), format!("jotter://{cell_key}").into()],
                )?;
                // acwrite: :w fires our BufWriteCmd hook instead of hitting
                // disk; undolevels=-1 keeps the initial fill out of undo
                // history (u must not empty a freshly opened cell)
                session.cmd("setlocal buftype=acwrite undolevels=-1")?;
                // filetype fires FileType autocmds (ftplugins, user config);
                // alphanumerics only, it goes through an Ex command
                let ft: String = filetype
                    .chars()
                    .filter(|c| c.is_ascii_alphanumeric())
                    .collect();
                if !ft.is_empty() {
                    session.cmd(&format!("setlocal filetype={ft}"))?;
                }
                session.req(
                    "nvim_buf_set_lines",
                    vec![
                        buf.clone(),
                        0.into(),
                        (-1).into(),
                        false.into(),
                        Value::Array(src_lines),
                    ],
                )?;
                session.cmd("setlocal undolevels< nomodified")?;
                session.buffers.insert(cell_key.into(), buf);
            }
        }
        let bufnr = session
            .req("nvim_eval", vec!["bufnr('%')".into()])?
            .as_u64()
            .ok_or_else(|| anyhow!("bad bufnr reply"))?;
        let mut cell = Self {
            session,
            lines: vec![String::new()],
            cursor: (0, 0),
            mode: "n".into(),
            anchor: (0, 0),
            cmdline: String::new(),
            bufnr,
            notice: None,
        };
        // normalize whatever mode the previous cell edit left behind
        cell.feed("<Esc>")?;
        Ok(cell)
    }

    fn mode_kind(&self) -> ModeKind {
        match self.mode.as_bytes().first() {
            _ if self.mode.starts_with("no") => ModeKind::Pending,
            Some(b'i') => ModeKind::Insert,
            Some(b'R') => ModeKind::Replace,
            Some(b'v' | b's') => ModeKind::Visual,
            Some(b'V' | b'S') => ModeKind::VisualLine,
            Some(0x16) => ModeKind::VisualBlock,
            Some(b'c') => ModeKind::Cmdline,
            _ => ModeKind::Normal,
        }
    }

    /// Queue keys, then refresh the cached state.
    fn feed(&mut self, keys: &str) -> Result<()> {
        self.session.req("nvim_input", vec![keys.into()])?;
        self.readback()
    }

    /// One deferred eval doubles as the flush barrier: nvim answers it only
    /// after the queued input has been processed, so the state is post-key.
    ///
    /// jotter draws only the cell buffer: nvim has no UI attached, so a
    /// plugin window (telescope picker, float, split) would be invisible and
    /// its buffer would be read back *as the cell*. When the current buffer
    /// or window isn't the cell's, reclaim nvim and say so instead.
    fn readback(&mut self) -> Result<()> {
        match self
            .session
            .req("nvim_eval", vec![self.state_expr().into()])
        {
            Ok(v) if Self::is_foreign(&v, self.bufnr) => {
                self.session.req(
                    "nvim_exec_lua",
                    vec![RECLAIM.into(), Value::Array(vec![self.bufnr.into()])],
                )?;
                self.notice = Some(
                    "nvim opened a window/picker — jotter can't draw nvim UI, so it was closed"
                        .into(),
                );
                let v = self
                    .session
                    .req("nvim_eval", vec![self.state_expr().into()])?;
                self.apply_state(v)
            }
            Ok(v) => self.apply_state(v),
            Err(e) => {
                // Deferred requests queue while nvim sits in a blocking prompt
                // (hit-enter after an error, ...). nvim_get_mode is answered
                // even then — it doubles as the liveness probe; a dead child
                // propagates the original error to trigger the fallback.
                let m = self.session.req("nvim_get_mode", vec![]).map_err(|_| e)?;
                if let Some(mode) = m
                    .as_map()
                    .and_then(|m| m.iter().find(|(k, _)| k.as_str() == Some("mode")))
                    .and_then(|(_, v)| v.as_str())
                {
                    self.mode = mode.into();
                }
                Ok(())
            }
        }
    }

    /// The per-key state query: the cell buffer's text (by number, never
    /// "whatever is current"), cursor, mode, cmdline, visual anchor, and
    /// where nvim actually is (for `is_foreign`).
    fn state_expr(&self) -> String {
        format!(
            "[getbufline({}, 1, '$'), getpos('.'), mode(1), getcmdtype() .. getcmdline(), \
             getpos('v'), bufnr('%'), win_gettype()]",
            self.bufnr
        )
    }

    /// State reply shows nvim outside the cell: another buffer is current,
    /// or the current window is a float/preview/cmdline window.
    fn is_foreign(v: &Value, bufnr: u64) -> bool {
        let Some([.., cur, wintype]) = v.as_array().map(|a| a.as_slice()) else {
            return false;
        };
        cur.as_u64() != Some(bufnr) || wintype.as_str().is_some_and(|t| !t.is_empty())
    }

    /// Status message for jotter, once.
    pub fn take_notice(&mut self) -> Option<String> {
        self.notice.take()
    }

    fn apply_state(&mut self, v: Value) -> Result<()> {
        let arr = v.as_array().ok_or_else(|| anyhow!("bad state reply"))?;
        let [lines, curpos, mode, cmdline, vpos, ..] = arr.as_slice() else {
            bail!("bad state reply shape");
        };
        self.lines = lines
            .as_array()
            .ok_or_else(|| anyhow!("bad getline reply"))?
            .iter()
            .map(|l| l.as_str().unwrap_or_default().to_string())
            .collect();
        if self.lines.is_empty() {
            self.lines.push(String::new());
        }
        self.mode = mode.as_str().unwrap_or("n").into();
        self.cmdline = cmdline.as_str().unwrap_or_default().into();
        self.cursor = self.pos_to_rc(curpos);
        self.anchor = self.pos_to_rc(vpos);
        Ok(())
    }

    /// `getpos()` -> [bufnum, lnum, col, off]; lnum/col 1-based, col in bytes.
    fn pos_to_rc(&self, pos: &Value) -> (usize, usize) {
        let a = pos.as_array();
        let get = |i: usize| {
            a.and_then(|a| a.get(i))
                .and_then(|v| v.as_u64())
                .unwrap_or(1) as usize
        };
        let row = get(1).saturating_sub(1).min(self.lines.len() - 1);
        let bcol = get(2);
        let col = self.lines[row]
            .char_indices()
            .take_while(|(i, _)| i + 1 < bcol)
            .count();
        (row, col)
    }

    /// Char offset into the joined source -> (0-based row, byte col).
    fn byte_pos(&self, offset: usize) -> (usize, usize) {
        let mut left = offset;
        for (row, line) in self.lines.iter().enumerate() {
            let len = line.chars().count();
            if left <= len || row + 1 == self.lines.len() {
                let byte = line.char_indices().nth(left).map_or(line.len(), |(b, _)| b);
                return (row, byte);
            }
            left -= len + 1;
        }
        (0, 0)
    }

    fn replace_range(&mut self, start: usize, end: usize, text: &str) -> Result<()> {
        let (sr, sc) = self.byte_pos(start);
        let (er, ec) = self.byte_pos(end.max(start));
        let replacement: Vec<Value> = text.split('\n').map(Value::from).collect();
        self.session.req(
            "nvim_buf_set_text",
            vec![
                0.into(),
                (sr as u64).into(),
                (sc as u64).into(),
                (er as u64).into(),
                (ec as u64).into(),
                Value::Array(replacement),
            ],
        )?;
        // cursor right after the inserted text (still in insert mode)
        let last = text.rsplit('\n').next().unwrap_or("");
        let rows = text.matches('\n').count();
        let (row, col) = if rows == 0 {
            (sr, sc + last.len())
        } else {
            (sr + rows, last.len())
        };
        self.session.req(
            "nvim_win_set_cursor",
            vec![
                0.into(),
                Value::Array(vec![((row + 1) as u64).into(), (col as u64).into()]),
            ],
        )?;
        self.readback()
    }

    /// Mouse click: place the cursor; optionally enter insert (double click).
    /// Best-effort — a dead child surfaces on the next key instead.
    fn click(&mut self, row: usize, col: usize, insert: bool) {
        let row = row.min(self.lines.len().saturating_sub(1));
        let line = &self.lines[row];
        let byte = line.char_indices().nth(col).map_or(line.len(), |(i, _)| i);
        let _ = self.session.req(
            "nvim_win_set_cursor",
            vec![
                0.into(),
                Value::Array(vec![((row + 1) as u64).into(), (byte as u64).into()]),
            ],
        );
        if insert && self.mode == "n" {
            let _ = self.feed("i");
        } else {
            let _ = self.readback();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, mods)
    }

    #[test]
    fn key_notation_table() {
        let k = |code, m| key_to_nvim(key(code, m)).unwrap();
        use KeyCode::*;
        use KeyModifiers as M;
        assert_eq!(k(Char('a'), M::NONE), "a");
        assert_eq!(k(Char('A'), M::SHIFT), "A"); // shift baked into the char
        assert_eq!(k(Char('<'), M::NONE), "<lt>");
        assert_eq!(k(Char(' '), M::NONE), "<Space>");
        assert_eq!(k(Char('r'), M::CONTROL), "<C-r>");
        assert_eq!(k(Char('x'), M::ALT), "<M-x>");
        assert_eq!(k(Esc, M::NONE), "<Esc>");
        assert_eq!(k(Enter, M::NONE), "<CR>");
        assert_eq!(k(Backspace, M::NONE), "<BS>");
        assert_eq!(k(BackTab, M::NONE), "<S-Tab>");
        assert_eq!(k(F(5), M::NONE), "<F5>");
        assert_eq!(k(Left, M::CONTROL | M::SHIFT), "<C-S-Left>");
        assert_eq!(key_to_nvim(key(CapsLock, M::NONE)), None);
    }

    /// M0 spike + M1/M2 exit criteria against a real nvim:
    /// `cargo test -- --ignored nvim_spike` (needs nvim on PATH).
    #[test]
    #[ignore = "spawns a real nvim"]
    fn nvim_spike() {
        let session = NvimSession::spawn(false).expect("spawn nvim");
        let mut c =
            NvimCell::open(session, "spike", "one two\nthree four\nfive", "").expect("open");
        assert_eq!(c.lines, ["one two", "three four", "five"]);
        assert_eq!((c.mode.as_str(), c.cursor), ("n", (0, 0)));

        // operator-pending is visible through the flush barrier (no race)
        c.feed("d").unwrap();
        assert_eq!(c.mode, "no");
        c.feed("w").unwrap();
        assert_eq!((c.lines[0].as_str(), c.mode.as_str()), ("two", "n"));

        // ciw + insert-mode typing
        c.feed("ciw").unwrap();
        assert_eq!((c.lines[0].as_str(), c.mode.as_str()), ("", "i"));
        c.feed("hello").unwrap();
        assert_eq!(c.lines[0], "hello");
        c.feed("<Esc>").unwrap();
        assert_eq!((c.mode.as_str(), c.cursor), ("n", (0, 4)));

        // x, undo
        c.feed("x").unwrap();
        assert_eq!(c.lines[0], "hell");
        c.feed("u").unwrap();
        assert_eq!(c.lines[0], "hello");
        // walk history to the start: the initial fill is not undoable (a
        // fourth u must not empty the buffer), then redo back to "hello"
        c.feed("uuu").unwrap();
        assert_eq!(c.lines, ["one two", "three four", "five"]);
        c.feed("<C-r><C-r>").unwrap();
        assert_eq!(c.lines[0], "hello");

        // counts
        c.feed("gg3j").unwrap();
        assert_eq!(c.cursor.0, 2);

        // named register yank/paste
        c.feed("gg\"ayy").unwrap();
        c.feed("j\"ap").unwrap();
        assert_eq!(c.lines, ["hello", "three four", "hello", "five"]);

        // `.` repeat
        c.feed("ggx").unwrap();
        c.feed("j.").unwrap();
        assert_eq!(
            (c.lines[0].as_str(), c.lines[1].as_str()),
            ("ello", "hree four")
        );
        c.feed("uu").unwrap();

        // macro: qa A!<Esc> q, replay with @a
        c.feed("gg").unwrap();
        c.feed("qa").unwrap();
        c.feed("A!").unwrap();
        c.feed("<Esc>").unwrap();
        c.feed("q").unwrap();
        c.feed("j@a").unwrap();
        assert_eq!(
            (c.lines[0].as_str(), c.lines[1].as_str()),
            ("hello!", "three four!")
        );

        // visual line selection readback
        c.feed("ggVj").unwrap();
        assert_eq!(c.mode_kind(), ModeKind::VisualLine);
        assert_eq!((c.anchor.0, c.cursor.0), (0, 1));
        c.feed("<Esc>").unwrap();

        // completion splice: replace "hel" in "hello!" mid-insert, cursor after
        c.feed("gg0i").unwrap();
        let mut b = Backend::Nvim(c);
        b.replace_range(0, 3, "HEL").unwrap();
        assert_eq!(b.source().lines().next(), Some("HELlo!"));
        assert_eq!((b.cursor(), b.mode()), ((0, 3), ModeKind::Insert));
        let Backend::Nvim(mut c) = b else {
            unreachable!()
        };
        c.feed("<Esc>").unwrap();

        // Tab inserts spaces, never a literal tab (clean-config expandtab)
        c.feed("o<Tab>t<Esc>").unwrap();
        assert!(c.lines.iter().any(|l| l == "    t"), "{:?}", c.lines);
        c.feed("u").unwrap();

        // a plugin-style float stealing focus is closed; the cell survives
        // (two floats where closing the first closes the second, like telescope)
        c.feed(":lua local function f(t) local b = vim.api.nvim_create_buf(false, true); vim.api.nvim_buf_set_lines(b, 0, -1, false, {t}); return vim.api.nvim_open_win(b, true, {relative='editor', row=1, col=1, width=10, height=1}) end; local r = f('RESULTS'); local p = f('PICKER'); vim.api.nvim_create_autocmd('WinClosed', {pattern = tostring(p), once = true, callback = function() pcall(vim.api.nvim_win_close, r, true) end})<CR>").unwrap();
        assert!(c.lines.iter().all(|l| l != "PICKER"), "{:?}", c.lines);
        assert!(c.take_notice().is_some());
        assert_eq!(c.mode_kind(), ModeKind::Normal);
        // ...and so is a split onto another buffer (e.g. a file opened from it)
        c.feed(":new<CR>").unwrap();
        assert!(
            c.lines.len() > 1,
            "cell text still read back: {:?}",
            c.lines
        );
        assert!(c.take_notice().is_some());

        // cmdline echo (blind-cmdline mitigation)
        c.feed(":").unwrap();
        c.feed("wq").unwrap();
        assert_eq!(
            (c.mode_kind(), c.cmdline.as_str()),
            (ModeKind::Cmdline, ":wq")
        );
        c.feed("<Esc>").unwrap();

        // latency: the plan's gate is < 1 ms per key (2 RPC round-trips)
        let t0 = Instant::now();
        for _ in 0..50 {
            c.feed("j").unwrap();
            c.feed("k").unwrap();
        }
        let per_key = t0.elapsed() / 100;
        println!("per-key round-trip: {per_key:?}");
        assert!(
            per_key < Duration::from_millis(1),
            "per-key {per_key:?} >= 1 ms"
        );
    }

    /// The jotter-level key intercepts on the Backend surface (what app.rs
    /// relies on): `cargo test -- --ignored backend_intercepts`.
    #[test]
    #[ignore = "spawns a real nvim"]
    fn backend_intercepts() {
        let session = NvimSession::spawn(false).expect("spawn nvim");
        let cell = NvimCell::open(session, "intercepts", "abc", "python").expect("open");
        let mut b = Backend::Nvim(cell);

        // Shift+Enter runs in any mode; Esc in normal mode exits the cell
        let run = b.input(key(KeyCode::Enter, KeyModifiers::SHIFT)).unwrap();
        assert!(matches!(run, Outcome::Run { advance: true }));
        assert!(matches!(
            b.input(key(KeyCode::Esc, KeyModifiers::NONE)).unwrap(),
            Outcome::Exit
        ));

        // Esc in insert mode forwards to nvim instead of exiting
        b.input(key(KeyCode::Char('i'), KeyModifiers::NONE))
            .unwrap();
        assert_eq!(b.mode(), ModeKind::Insert);
        assert!(matches!(
            b.input(key(KeyCode::Esc, KeyModifiers::NONE)).unwrap(),
            Outcome::Continue
        ));
        assert_eq!(b.mode(), ModeKind::Normal);

        // visual mode reaches the render surface; Esc cancels it (no exit)
        b.input(key(KeyCode::Char('v'), KeyModifiers::NONE))
            .unwrap();
        assert!(b.visual().is_some());
        assert!(matches!(
            b.input(key(KeyCode::Esc, KeyModifiers::NONE)).unwrap(),
            Outcome::Continue
        ));
        assert!(b.visual().is_none());

        // :w routes through the BufWriteCmd hook into the commit flag
        for c in [':', 'w'] {
            b.input(key(KeyCode::Char(c), KeyModifiers::NONE)).unwrap();
        }
        assert_eq!(b.cmdline(), Some(":w"));
        b.input(key(KeyCode::Enter, KeyModifiers::NONE)).unwrap();
        assert!(b.take_write_request());
        assert!(!b.take_write_request());
        assert_eq!(b.source(), "abc");

        // quit intent leaves the cell instead of killing nvim: cmdline
        // spellings and the ZZ/ZQ normal-mode keys, bang = discard
        let mut cmdline_quit = |keys: &str| {
            for c in keys.chars() {
                b.input(key(KeyCode::Char(c), KeyModifiers::NONE)).unwrap();
            }
            b.input(key(KeyCode::Enter, KeyModifiers::NONE)).unwrap();
            b.take_quit_request()
        };
        assert_eq!(cmdline_quit(":q"), Some(false));
        assert_eq!(cmdline_quit(":q!"), Some(true));
        assert_eq!(cmdline_quit(":wq"), Some(false));
        assert_eq!(cmdline_quit(":wq!"), Some(false)); // write-forced, not discard
        assert_eq!(cmdline_quit(":qa"), Some(false));
        assert_eq!(cmdline_quit("ZZ"), Some(false)); // no ':' — plain keys + noop Enter
        assert_eq!(cmdline_quit("ZQ"), Some(true));
        assert_eq!(b.take_quit_request(), None);
        // nvim survived all of it: a real edit still round-trips
        b.input(key(KeyCode::Char('x'), KeyModifiers::NONE))
            .unwrap();
        assert_eq!(b.source(), "bc");
    }
}
