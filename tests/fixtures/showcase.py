# ---
# jupyter:
#   jupytext:
#     text_representation:
#       extension: .py
#       format_name: percent
#       format_version: '1.3'
#       jupytext_version: 1.19.6
#   kernelspec:
#     display_name: Python 3
#     language: python
#     name: python3
# ---

# %% [markdown]
# # jOtter feature showcase 🦦
#
# Run this top to bottom with **Ctrl+r** (run all — cells queue as `In [·]` and flip to `In [*]` when they start), or step through with **Shift+Enter**. Press **?** for the key cheatsheet.
#
# Some features live outside cells — try them while you're here:
#
# | feature | how to try it |
# |---------|---------------|
# | search | `/` then type `spinner`, Enter; `n` / `N` cycle matches |
# | goto | `5G` jumps to cell 5, `3j` moves down 3 |
# | undo cell ops | `dd` a cell, then `u`; also works for moves (`J`/`K`) and `m` |
# | quit safety | edit something, press `q`: save / discard / cancel prompt |
# | external changes | touch the file in another shell, `w` refuses, `W` forces |
# | autosave | edit, wait 30 s, see `.showcase.ipynb.autosave` appear |

# %% [markdown]
# ## Markdown rendering
#
# Inline: **bold**, *italic*, `code`, math $e^{i\pi} + 1 = 0$, and a [link to the repo](https://github.com/samox/jotter).
#
# - bullets
# - with a nested dollar $\tau$ mid-sentence
#
# | stage | feature | status |
# |-------|---------|--------|
# | 1 | stream semantics | done |
# | 2 | tables, links, emphasis | done |
#
# Display math:
#
# $$G(\mathbf{k}, \tau) = -\langle T_\tau\, c_{\mathbf{k}}(\tau)\, c^\dagger_{\mathbf{k}}(0) \rangle$$
#
# A local image (path relative to the notebook):
#
# ![jotter screenshot](assets/showcase.png)

# %% [raw] format="text/latex"
# $\Sigma(\mathbf{k}, \tau) = \int \frac{d^3q}{(2\pi)^3} \, |g(\mathbf{q})|^2 \, G(\mathbf{k}-\mathbf{q}, \tau) \, D(\mathbf{q}, \tau)$

# %%
# Long outputs: chunks coalesce into ONE output, display becomes a viewport
# pinned to the tail. Wheel over it or use [ ] to scroll, o to collapse.
import time
for i in range(1, 41):
    print(f"line {i:3d} — the viewport follows the tail while this streams")
    time.sleep(0.1)

# %%
# \r overwrites in place: one line on screen, in memory, and in the saved file
import time
for i in range(1, 41):
    print(f"line {i:3d}")
    time.sleep(0.1)
#for i in range(101):
#    print(f"progress [{'#' * (i // 5):<20}] {i:3d}%", end="\r", flush=True)
#    time.sleep(0.02)
#print()
#print("done — a single line survived")

# %%
# clear_output(wait=True): flicker-free replacement instead of endless append
import time
from IPython.display import clear_output
for frame in ["◐", "◓", "◑", "◒"] * 5:
    clear_output(wait=True)
    print(f"spinner {frame}")
    time.sleep(0.1)
clear_output(wait=True)
print("spinner done — replaced, never appended")

# %%
# tqdm combines \r and stderr streams — stays a single tidy line
try:
    import time
    from tqdm import trange
    for _ in trange(60, desc="tqdm"):
        time.sleep(0.02)
except ImportError:
    print("tqdm not installed — skipped")

# %%
# image/svg+xml outputs render via resvg
from IPython.display import SVG
SVG('''<svg xmlns="http://www.w3.org/2000/svg" width="360" height="120">
  <rect width="360" height="120" rx="14" fill="#2b3339"/>
  <circle cx="70" cy="60" r="38" fill="#7fbbb3"/>
  <rect x="140" y="30" width="60" height="60" fill="#dbbc7f" transform="rotate(12 170 60)"/>
  <text x="230" y="72" font-size="28" fill="#d3c6aa" font-family="sans-serif">SVG ok</text>
</svg>''')

# %%
# text/markdown outputs go through the markdown renderer
from IPython.display import Markdown
Markdown(
    "**markdown output** with a [link](https://jupyter.org) and math $E = \\hbar\\omega$\n\n"
    "| mime | rendered |\n|------|----------|\n| text/markdown | yes |"
)

# %%
# an output with no renderable rep names what it carries instead of vanishing
from IPython.display import display
display({"text/html": "<b>html only</b>"}, raw=True)

# %%
# plots: one Lanczos prescale to <=18 rows, transmitted once, cropped (never
# rescaled) at the viewport edges — scroll this across the top and bottom
import numpy as np
import os
import matplotlib.pyplot as plt
x = np.linspace(0, 4 * np.pi, 400)
plt.subplots(figsize=(10, 10))
for k in range(1, 4):
    plt.plot(x, np.sin(k * x) / k, label=f"$\\sin({k}x)/{k}$")
plt.legend(); plt.title("sine or no sine")
plt.tight_layout(); plt.show()

# %%
# input() prompts in the status line (Esc sends empty, Ctrl+C interrupts)
name = input("your name: ")
print(f"hello, {name or 'stranger'}!")


# %%
# ANSI tracebacks render colored; run-all stops queued cells here (aborted
# cells drop back from In [·] to idle)
def boom(depth=3):
    if depth == 0:
        raise ValueError("ANSI traceback demo — everything above still ran")
    return boom(depth - 1)
boom()

# %%
# with stop_on_error, run-all aborts here — run it by hand to prove the
# kernel survived the traceback above
print("still alive")
