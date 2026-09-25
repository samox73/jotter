<div align="center">

# jOtter 🦦

**A fast Jupyter notebook TUI. The Jupyter otter.**

<img src="assets/banner.png" alt="jOtter mascot" width="400"/>

</div>

Open, edit, and run real `.ipynb` notebooks in your terminal with vim keys, inline plots, and rendered LaTeX — with keystroke latency so low that holding a key is a non-event.

<div align="center">
<img src="assets/showcase.png" alt="jOtter showing markdown with rendered LaTeX, syntax-highlighted code cells, and an inline matplotlib plot" width="490"/>
</div>

## Features

- **Real nbformat**: opens and saves `.ipynb` losslessly — cell ids, metadata, and fields written by other tools survive round-trips untouched.
- **Fast**: single-digit-microsecond redraws (Rust + [ratatui](https://ratatui.rs)); cell bodies are cached and rebuilt per cell, never per keystroke.
- **Vi editing**: modal editor inside cells (`hjkl`, `w b e`, `dd yy p`, `u`/`Ctrl+r`, ...), with modal cursor shapes, and `E` to open the cell in `$EDITOR` for anything heavier. Or set `editor = "nvim"` to let an embedded nvim drive cell editing — the full modal engine (operators, visual mode, registers, macros) with jotter still rendering.
- **Kernel execution**: launches any Jupyter kernelspec over ZMQ, streams outputs live, interrupt with `Ctrl+C`, restart with `R` (asks first; `a` restarts and runs all). Cells run while the kernel is still starting are queued, not refused. Kernel cwd is the notebook's directory, like jupyterlab.
- **Completion and docs**: `Tab` in insert mode completes from the kernel, and typing `.` after a name opens completion by itself. The popup (name · kind · signature, with a scrollbar) filters as you keep typing; `Tab`/`↓`/`Ctrl+n` and `↑`/`Ctrl+p` move, `Enter` accepts, `Esc` closes. Kinds and signatures the kernel doesn't send up front are looked up for the highlighted entry as you move. `Shift+Tab` shows the docs for the name under the cursor — or, with the popup open, for the highlighted entry (docs need the defining cell to have run). Works with both the builtin and the nvim editor. Completion asks IPython's fast live-object completer first (milliseconds, but it only knows code that has run); when that finds nothing — `np.` before the import cell ran — it retries once with jedi's static analysis (slower: the first jedi call in a session takes about a second). Plain names also offer the notebook's own identifiers (this cell's nearest first, strings and comments skipped), so `np` completes right after typing `import numpy as np`. `jedi = true` uses jedi for every request.
- **Soft wrapping**: long code lines, markdown prose, and outputs word-wrap to the terminal width instead of being cut off; inline math never splits across rows.
- **Kernel resolution ladder**: activated env (`$VIRTUAL_ENV`, `$CONDA_PREFIX`) → `.venv`/`venv` beside the notebook → the notebook's kernelspec → global kernels → `python3` fallback. Works with uv, poetry, conda, pixi — anything that installs `ipykernel`.
- **Inline graphics**: matplotlib PNGs render in-terminal via the kitty graphics protocol (sixel/iTerm2/halfblocks fallback through [ratatui-image](https://github.com/benjajaja/ratatui-image)).
- **LaTeX**: `$$display$$` and `$inline$` math render as real equations (via [ratex](https://crates.io/crates/ratex-svg)); raw cells with `metadata.format: text/latex` render entirely as math. No TeX installation needed.
- **Language-aware**: highlighting and the `E` temp-file type follow the notebook's kernel language (`language_info` / kernelspec), not just Python.
- **Markdown cells** render rich (headings, bullets, fenced code with syntax highlighting, math) when not being edited.
- **Mouse**: click to select or place the cursor, double-click to insert, wheel to scroll. `Shift+drag` for native text selection, `yy` copies a cell to the system clipboard (OSC 52).
- **Jupyter stream semantics**: consecutive stream chunks coalesce, `\r` progress bars (tqdm) overwrite in place, `clear_output` and display handles (`display(..., display_id=True).update()`, `tqdm.notebook`) work, and per-cell output is capped at the last 10k lines.
- **Long outputs** display as a scrollable viewport pinned to the live tail — wheel over it or `[`/`]` to scroll, `o` to collapse.
- **Data safety**: atomic fsync'd saves, autosave to `~/.local/state/jotter/autosave/` (never beside the notebook, so no git noise) with a restore prompt after a crash, save/discard/cancel prompt on quit, and a warning instead of a silent overwrite when the file changed on disk.
- **New notebooks and save-as**: `jotter new.ipynb` starts an empty notebook (written on the first `w`, with the kernelspec filled in); `S` saves under a new name.
- **input() support**, run-all/above/below with a queued/running gutter, `/` search across cells, cell-op undo/redo, split/merge cells, clear outputs, per-cell execution timing, a log viewer (`L`), and a per-cell debug report (`D`, copied to the clipboard) for bug reports.

## Install

```sh
cargo install --path .   # or: make install
nix run github:samox/jotter -- notebook.ipynb   # or build via the bundled flake
```

Rust 1.85+ (edition 2024). No native dependencies — the ZMQ stack is pure Rust.

## Usage

```sh
jotter notebook.ipynb              # kernel from notebook metadata (new file if missing)
jotter --kernel phy notebook.ipynb # explicit kernelspec
jotter --no-images notebook.ipynb  # text-only outputs
jotter --log jotter.log nb.ipynb   # debug log (jotter + kernel wire)
```

Press `?` inside for the full key reference.

|  |  |
| --- | --- |
| `j/k` `g/G` `5G` | select / first / last / numbered cell |
| `Enter` `i` `A` | edit cell (vi bindings inside, `Esc` exits) |
| `Shift+Enter` / `r` | run cell and advance |
| `Space` / `Ctrl+Enter` | run cell in place |
| `X` `<` `>` | run all / all above / cell and below (the selection follows the running cell until you move) |
| `a`/`b` `dd`/`yy`/`p` `J`/`K` | insert, delete/copy/paste, move cells |
| `u` / `Ctrl+r` | undo / redo cell operations |
| `M` / `Ctrl+\` (editing) | merge with the cell below / split at the cursor |
| `Tab` / `Shift+Tab` (editing) | complete (also opens by itself after `.`) / show docs |
| `/` `n` `N` | search cell sources / next / previous |
| `o` `[` `]` | collapse / scroll long outputs (or wheel over them) |
| `c` / `C` | clear cell output / all outputs |
| `z` / `Z` | zoom image fullscreen / toggle native-size images |
| `m` | cycle cell type: code → markdown → latex |
| `E` | edit cell in `$EDITOR` |
| `w` (`W` force) / `S` / `q` | save / save as / quit |
| `Ctrl+C` / `R` | interrupt / restart kernel |
| `L` / `D` | view logs / debug info for the cell (copied to clipboard) |

## Config

Optional, at `~/.config/jotter/config.toml` — every key has a default:

```toml
max_image_rows = 18     # images taller than this are downscaled once (Lanczos)
max_output_rows = 15    # output rows shown before the viewport scrolls
autosave_secs = 30      # autosave interval (~/.local/state/jotter/autosave/)
theme = "base16-ocean.dark"  # try base16-ocean.light on light terminals
editor = "builtin"      # "nvim": embedded nvim drives cell editing (experimental)
nvim_user_config = true # nvim backend loads your init.lua/plugins (false: clean -u NONE)
jedi = false            # true: jedi for every completion (slow); false: live objects, jedi only as fallback
complete_on_dot = true  # open completion after `.` following a name (Python)
```

With `editor = "nvim"`, a hidden `nvim --embed` owns the cell buffer while jotter keeps rendering: full modal editing (`dw`, `ciw`, visual mode, counts, registers, macros, `.`), per-cell undo history, `:`/`/` echoed in the status bar, `:w` commits the cell, and `:q`/`:wq`/`ZZ` leave it (`:q!`/`ZQ` discard the edit). Falls back to the builtin editor if nvim is missing or dies. Your nvim config and plugins load, with `g:jotter = 1` set before `init.lua`/`init.vim` runs — guard UI-only plugins (dashboards, statuslines, completion popups jotter can't draw) with `if vim.g.jotter then ... end`. Cell buffers get their `filetype` (`python`, `markdown`, ...), so ftplugins and filetype settings apply.

**nvim UI is not drawn.** jotter renders only the cell's text; nvim runs without a UI attached. Anything that opens a window — pickers (telescope `<leader>sg`, fzf-lua), floating hovers/diagnostics, file explorers, splits, the cmdline window — can't be shown, and before this guard its buffer would have been read back *as the cell*. jotter now checks after every key that nvim is still in the cell buffer and a normal window; if not, it closes the foreign windows, leaves insert mode, returns to the cell, and says so on the status line. The cell's text is never touched, but the plugin action is lost. Guard such mappings with `if not vim.g.jotter then ... end`.

## Known limits

- Images wider than the terminal crop at the right edge instead of scaling (keeps the one-time high-quality downscale; no render-time rescaling, no aliasing).
- A font-size change (terminal zoom) re-renders every cell: output-viewport positions reset and images are re-transmitted.
- The help page (`?`) needs about 33 rows; shorter terminals clip it.
- nvim backend: plugin windows (pickers, floats, explorers, splits) are not supported — they are closed as soon as they open (see above). Plugin popups that don't take focus (e.g. nvim-cmp's menu) are simply invisible.
- Non-goals: ipywidgets (placeholder only), multiple tabs, remote/existing kernels, Windows.

## Terminal support

Best in [kitty](https://sw.kovidgoyal.net/kitty/) (graphics + keyboard protocol → `Shift+Enter`, distinct modifier keys). Any terminal works: graphics fall back to sixel/iTerm2/unicode halfblocks, and `r` runs cells where `Shift+Enter` can't be distinguished.
