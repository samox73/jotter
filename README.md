<div align="center">

# jOtter 🦦

**A fast Jupyter notebook TUI. The Jupyter otter.**

<img src="assets/banner.png" alt="jOtter mascot" width="400"/>

</div>

Open, edit, and run real `.ipynb` notebooks in your terminal with vim keys, inline plots, and rendered LaTeX — with keystroke latency so low that holding a key is a non-event.

<div align="center">
<img src="assets/showcase.png" alt="jOtter showing markdown with rendered LaTeX, syntax-highlighted code cells, and an inline matplotlib plot" width="700"/>
</div>

## Features

- **Real nbformat**: opens and saves `.ipynb` losslessly — cell ids, metadata, and fields written by other tools survive round-trips untouched.
- **Fast**: single-digit-microsecond redraws (Rust + [ratatui](https://ratatui.rs)); cell bodies are cached and rebuilt per cell, never per keystroke.
- **Vi editing**: modal editor inside cells (`hjkl`, `w b e`, `dd yy p`, `u`/`Ctrl+r`, ...), with modal cursor shapes, and `E` to open the cell in `$EDITOR` for anything heavier.
- **Kernel execution**: launches any Jupyter kernelspec over ZMQ, streams outputs live, interrupt with `Ctrl+C`, restart with `R`. Kernel cwd is the notebook's directory, like jupyterlab.
- **Kernel resolution ladder**: activated env (`$VIRTUAL_ENV`, `$CONDA_PREFIX`) → `.venv`/`venv` beside the notebook → the notebook's kernelspec → global kernels → `python3` fallback. Works with uv, poetry, conda, pixi — anything that installs `ipykernel`.
- **Inline graphics**: matplotlib PNGs render in-terminal via the kitty graphics protocol (sixel/iTerm2/halfblocks fallback through [ratatui-image](https://github.com/benjajaja/ratatui-image)).
- **LaTeX**: `$$display$$` and `$inline$` math render as real equations (via [ratex](https://crates.io/crates/ratex-svg)); raw cells with `metadata.format: text/latex` render entirely as math. No TeX installation needed.
- **Markdown cells** render rich (headings, bullets, fenced code with syntax highlighting, math) when not being edited.
- **Mouse**: click to select or place the cursor, double-click to insert, wheel to scroll. `Shift+drag` for native text selection, `yy` copies a cell to the system clipboard (OSC 52).

## Install

```sh
cargo install --path .
```

Rust 1.85+ (edition 2024). No native dependencies — the ZMQ stack is pure Rust.

## Usage

```sh
jotter notebook.ipynb              # kernel from notebook metadata
jotter --kernel phy notebook.ipynb # explicit kernelspec
jotter --no-images notebook.ipynb  # text-only outputs
```

Press `?` inside for the full key reference.

|  |  |
| --- | --- |
| `j/k` `g/G` | select / first / last cell |
| `Enter` `i` `A` | edit cell (vi bindings inside, `Esc` exits) |
| `Shift+Enter` / `Ctrl+Enter` / `r` | run cell (+advance / in place / +advance) |
| `a`/`b` `dd`/`p` `J`/`K` | insert, delete/paste, move cells |
| `m` | cycle cell type: code → markdown → latex |
| `yy` | copy cell source to clipboard |
| `E` | edit cell in `$EDITOR` |
| `w` / `q` | save / quit |
| `Ctrl+C` / `R` | interrupt / restart kernel |

## Terminal support

Best in [kitty](https://sw.kovidgoyal.net/kitty/) (graphics + keyboard protocol → `Shift+Enter`, distinct modifier keys). Any terminal works: graphics fall back to sixel/iTerm2/unicode halfblocks, and `r` runs cells where `Shift+Enter` can't be distinguished.
