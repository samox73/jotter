# macOS test report

Tester: czyber (driven by Claude Code on czyber's Mac; see [How this was tested](#how-this-was-tested))
Date: 2026-11-11

| Field | Value |
| --- | --- |
| macOS version and chip | macOS 26.5.2 Tahoe (25F84), Apple M5, APFS (case-insensitive), zsh |
| `jotter --version` | `jotter 0.6.0`, both from the release tarball and from `uvx --from jotter-tui` |
| Gatekeeper wording on first run | **"jotter" Not Opened.** *Apple could not verify "jotter" is free of malware that may harm your Mac or compromise your privacy.* Buttons: **Move to Trash** (blue, the default) and **Done**. The shell prints `zsh: killed ./release/jotter --version` (exit 137). Screenshot: `results/01-gatekeeper-not-opened.png` |
| Bypass used (Open Anyway or xattr) | Open Anyway. Done → System Settings › Privacy & Security had the "jotter was blocked" entry → Open Anyway → Touch ID/password. The next run shows a second dialog, **Open "jotter"?** (*…Don't open this unless you are certain it is from a trustworthy source.*), with **Move to Trash** (default) / **Open Anyway** / **Done** (`results/02-gatekeeper-open-anyway.png`). After Open Anyway the blocked process continued and printed `jotter 0.6.0`, no rerun needed. The quarantine flag went from `0281` to `02c1`. |
| Cell 2 output (Python path, macOS version) | `/Users/czyber/.t3/scratch/2026-11-11-check-out-this-branch-of-3b014a3f/jotter/jotter-test/.venv/bin/python`<br>`macOS 26.5.2 arm64` |

## Steps

Pass means it behaved as the playbook expected; a fallback the playbook called expected for your terminal, such as blocky images in Terminal.app, is a pass.

| Step | Result (pass / fail / skipped) | Notes |
| --- | --- | --- |
| 1 Clone and set up | pass | Tarball downloaded with Chrome (quarantine `0281;…;Chrome;…`), SHA256 matches `SHA256SUMS`. `setup.sh` ended with `ready` in 2.5 s; `jotter-test` has `release`, `.venv` (uv picked Python 3.14.10), `test.ipynb`, `env.zsh`, `my notes/`. macOS `tar` carried the browser's quarantine flag over to the extracted binary by itself (same UUID), so the script's fallback never ran. |
| 2 Gatekeeper and install | pass | Blocked on first try, wording and bypass above. **On my first attempt a click on the default button sent the binary to the Trash**, see finding 1. After `source env.zsh`: `jotter --version` works, man page opens, Tab after `jotter --` offers `--kernel`, `--log`, `--no-images`, but also the hidden `--generate` (finding 12). `uvx --from jotter-tui jotter --version` prints `jotter 0.6.0` with no Gatekeeper prompt. |
| 3 Test notebook | pass, with deviations | Run in kitty. Kernel found in `.venv` without activating it and was ready about 1.5 s after start. No separate matplotlib window, no stray Python app. Deviations in cells 12 and 13 below. |
| 4 Terminals | pass, with findings | kitty and Ghostty: everything works. Terminal.app: the expected fallbacks, plus a few things worth fixing or documenting (findings 3–6). |
| 5 Files on a Mac | pass, with findings | 5.1: spaced, accented path opens, runs and saves, typed (NFC) and as Finder drag-and-drop would give it (NFD); still one file, name stays NFC. 5.2: `:w Test.ipynb` refuses with `Test.ipynb exists — :w! Test.ipynb overwrites`; a plain second Enter doesn't confirm (it opens the cell editor), `:w!` does; one file left, still spelled `test.ipynb`. 5.3 iCloud: both saves stuck, no "changed on disk" warning, **but the kernel hung on a macOS privacy dialog and timed out** (finding 2). Upload was blocked by a full iCloud quota (`brctl`: *Error uploading asset: Quota exceeded*), so iCloud never rewrote the file underneath jotter. |

## Test notebook

Only the cells that didn't match the table in the playbook:

| Cell | What you saw |
| --- | --- |
| 12 | Both lines show, but **stderr isn't marked differently**: both are drawn with the same style (`ESC[37m`, read back from kitty). Same in every terminal. `src/ui.rs:1494` renders every `stream` output with `OUTPUT_STYLE` and never looks at `name`. |
| 13 | Works (`hello, macOS tester!`), but the prompt is a **centred popup** (`your name:` · *Enter sends · Esc sends empty*), not in the status line as the playbook says. |
| 4 | Complete, smooth viridis gradient, but in a **white frame** on dark themes (the recolouring skips it because the white margin is small). Cosmetic. |
| 11 | Viewport and `[` `]` work, but one line per press: 85 presses from the bottom to the top of a 100-line output. |

## Terminals

| Terminal | Version | Images | Shift+Enter | `yy` paste | Resize, zoom, scroll | Notes |
| --- | --- | --- | --- | --- | --- | --- |
| Terminal.app | 2.15 (470.2), profile "Clear Dark" | half-blocks as expected (log: `graphics Halfblocks, font 7x15 px, keyboard protocol legacy`). **Math (cells 1, 5, 10) is unreadable**: smudges on black boxes (`results/05-…`). SVG's transparent corners and padding come out dark (`06-…`). | no, as expected: Shift+Enter arrives as Enter and **opens the cell editor**. **Ctrl+Enter never reaches jOtter: it opens Terminal's own "New Tab with Profile" menu**, which then eats the next keys (`07-…`). `r` and `Space` work. | no, as expected (no OSC 52), **but jOtter still says "source on the clipboard"** | Resize and fullscreen fine. `zi` shows the image only a little larger (native size). **Holding j/k: screenshots caught torn frames** (`08-…`, `09-…`), which settle within a second. Terminal.app doesn't do synchronized output (no reply to `DECRQM ?2026`). | Ctrl+\ splits (arrives as Ctrl+4). After quit: normal prompt, nothing left behind, mouse selection works. |
| kitty | 0.50.0 (Homebrew cask, default config) | sharp (log: `graphics Kitty, font 14x25 px, keyboard protocol kitty`), math sharp (`03-…`) | yes (runs and moves down); Ctrl+Enter runs in place | yes, no prompt | Resize, fullscreen, holding j/k: clean, images track the text, no leftovers. **`zi` doesn't enlarge** (`04-…`, finding 5). | Ctrl+\ splits. After quit: clean prompt, no images left, mouse selection works. |
| Ghostty | 1.3.1 (default config) | sharp (log: `graphics Kitty, font 16x34 px, keyboard protocol kitty`) (`12-…`) | yes; Ctrl+Enter runs in place | yes, **no prompt** (the playbook expects one the first time) | Resize, fullscreen, holding j/k: clean. `zi` doesn't enlarge, same as kitty. | Ctrl+\ splits. After quit: clean, mouse selection works. |
| iTerm2 | not tested | | | | | |
| WezTerm | not tested | | | | | |
| Alacritty | not tested | | | | | |
| Warp | not tested | | | | | |
| tmux, inside which terminal? | not tested (not installed) | | | | | |

## Anything else

Logs and screenshots are in `results/`: `kitty.log`, `terminal-app.log`, `ghostty.log`, `files.log` (5.1/5.2), `icloud.log` (5.3), and screenshots numbered as referenced above.

### Findings, most important first

1. **Gatekeeper: "Move to Trash" is the default button** in both dialogs. On my first run a click on it deleted the binary; Enter would do the same. The install docs should say, step by step: click **Done** (not Move to Trash), then System Settings › Privacy & Security › Open Anyway (needs Touch ID/password), run again, click **Open Anyway** in the second dialog. Or tell Mac users to skip the tarball: `uv tool install jotter-tui` / `uvx` never triggers Gatekeeper (checked).
2. **Notebook in iCloud Drive: the kernel hangs on a privacy dialog.** Starting the kernel with its working directory in iCloud Drive makes macOS ask *"kitty" wants to access files managed by "iCloud Drive"* (`11-…`). jOtter's own read and save didn't trigger it; the kernel did. Until someone answers, jOtter shows only `kernel starting — 1 cell(s) queued`; after 30 s it gives up with `kernel 'python3' failed: iopub connect: Connect timed out after 30s — 1 queued run(s) dropped`, and nothing points to the dialog. After clicking Allow, `R` restarted the kernel fine. A hint after a few seconds of "starting" ("macOS may be asking for permission in a dialog") would save people a confusing first run. The same may apply to other protected folders (Documents, Desktop, Downloads) in a terminal that hasn't been granted access yet; I didn't test those.
3. **Terminal.app: math is unreadable.** At 1×2 pixels per cell, inline and display equations and SymPy output turn into dark smudges, which is worse than the raw LaTeX. In half-block mode I'd show math as text (the LaTeX source, or a Unicode approximation like `∫₀^∞ e^{-x²} dx = √π/2`). The black boxes behind them also clash with non-black themes.
4. **Terminal.app: Ctrl+Enter opens Terminal's "New Tab with Profile" menu.** The key never reaches jOtter, the menu swallows the following keys, and an Enter after it opens a new tab. jOtter can't intercept it, so it belongs in the docs and playbook ("Terminal.app: use `r` / `Space`; Ctrl+Enter is taken by Terminal"). Related: Shift+Enter silently opens the cell editor. A one-time hint in legacy keyboard mode ("Shift+Enter can't be told apart here: `r` runs the cell") would help Jupyter habits.
5. **`zi` never enlarges an image.** `zoom_image` (`src/ui.rs:885`) only shrinks images that are bigger than the screen. A default matplotlib figure (100 dpi) is smaller, so in kitty and Ghostty the zoom view shows it at exactly its inline size in the middle of a black screen. On Retina, kitty reports cells in device pixels (14×25), so a 600×300 px figure covers 300×150 points: plots look half the size they would in browser Jupyter, and `zi` can't help. Suggestions: let `zi` scale up to fit, and/or render figures at 2× on HiDPI terminals (`InlineBackend.figure_format = 'retina'` or a higher `figure.dpi` when the reported cell size is large). In half-block mode, scaling up to the full screen would even add real detail.
6. **Terminal.app: torn frames while holding j/k.** Terminal.app ignores synchronized output (mode 2026), and with half-block images each frame is large, so a half-painted frame stays visible for a moment. 4 of 14 mid-scroll screenshots (windowed and fullscreen) caught one: stale lines from the previous frame under the new ones, a plot squashed to half height, a Mandelbrot stitched from two frames (`08-…`, `09-…`). It's transient: stepping one cell at a time and waiting 1.2 s before each screenshot gave 26 of 26 correct frames, including images clipped at the top and bottom edges. kitty and Ghostty showed nothing like it in their mid-scroll frames. Ideas: coalesce frames while a key repeats, or draw images as placeholders while scrolling fast on terminals without mode 2026.
7. **`yy` in Terminal.app says "source on the clipboard", but nothing arrives** (no OSC 52). On macOS, when `TERM_PROGRAM=Apple_Terminal` and the session is local (no `SSH_TTY`), falling back to `pbcopy` would make `yy` just work there.
8. **German/Austrian Mac keyboard layouts: Ctrl+\ doesn't split in kitty, and it inserts a `7` into the cell.** On these layouts `\` is Shift+Option+7, so Ctrl+\ is Ctrl+Shift+Option+7. kitty reports that as Ctrl+Alt+Shift+`7`, which the binding doesn't match, and the editor then inserts the bare `7`. Ctrl+4 does split in kitty, and in Terminal.app Ctrl+Shift+Option+7 splits fine. Suggestions: document Ctrl+4 as the alternative everywhere (today it's only mentioned for tmux), and don't insert a character for key events that carry Ctrl/Alt. Ghostty wasn't tested with this layout.
9. **Playbook wording that's out of date** (behaviour is fine): cell 13's prompt is a popup, not in the status line; `:w Test.ipynb` wants `:w!`, not a second Enter; Ghostty 1.3.1 doesn't ask before the first OSC 52 copy; `:q` while a cell editor is open only closes the editor (vim-style), so after the "second Enter" in 5.2 the playbook's `:q` doesn't quit jOtter, which tripped me up.
10. **Cell 12**: stderr isn't styled differently (see the table above).
11. **`:q!` keeps the autosave on purpose**, so the next open shows `autosave from 3s ago, 14 cell(s) differ` after I had edited one cell. The count compares cells by index, so one inserted or split cell makes every later cell "differ". Recovery itself works well on macOS: a sidecar written under `Test.ipynb` was found when opening `test.ipynb`.
12. **The shell completions advertise the hidden `--generate`**. All four scripts in the tarball (`_jotter`, `jotter.bash`, `jotter.fish`, `jotter.nu`) list it, while the man page correctly leaves it out.
13. `testing/macos/test.ipynb` isn't in canonical nbformat layout (each `source` is one string). The first save rewrites every cell into Jupyter's list-of-lines form, the same as Jupyter would, so anyone who saves the test notebook gets a large diff. Committing it in saved form avoids that.
14. FYI: Terminal.app answers `CSI 14t` in points (1280×857 for a 180×56 window on Retina) and doesn't answer `CSI 16t`, which is where `font 7x15 px` comes from. Harmless for half-blocks.

### Things that worked well

- Kernel discovery from `.venv` next to the notebook, with nothing activated.
- Unicode paths in both NFC and NFD; the atomic save keeps the existing spelling of the name on APFS.
- No keystrokes lost while starting up (tested by sending `j` 0.2 s after launch).
- Clean exit everywhere: prompt back, no leftover images, mouse selection works.

### Feature suggestions

- A Homebrew tap (`brew install samox73/tap/jotter`): formula installs aren't quarantined, so Mac users skip Gatekeeper entirely. Or Developer ID signing plus notarization, if that's ever worth the $99/year.
- `pbcopy` fallback for `yy` in Terminal.app (finding 7).
- Retina-aware figure size and an upscaling `zi` (finding 5).
- Math as text in half-block mode (finding 3).
- A kernel-start hint about macOS permission dialogs (finding 2).
- Half-page scrolling in long outputs (Ctrl+d/Ctrl+u, or a count before `[`/`]`).
- Ctrl+4 documented as the layout-independent split key (finding 8).
- No need for a Nix flake for Mac testers: Homebrew plus uv was enough, and `setup.sh` ran in 2.5 s.

### How this was tested

Claude Code drove the terminals on czyber's Mac. czyber clicked through the Gatekeeper and macOS privacy dialogs and granted kitty Accessibility and Screen Recording for the run. Keystrokes were real macOS key events sent via System Events, so each terminal did its own key encoding, exactly as for typed keys; mouse drags were real CGEvent drags. Each observation is a screenshot of that terminal's window (`screencapture -l`). The screen text came from kitty's remote control for kitty and from Apple's Vision OCR for Terminal.app and Ghostty. Terminal capabilities were queried directly (DECRQM, XTVERSION, CSI 14t/16t, OSC 11). "Holding" j/k was simulated as about 30 keystrokes per second, roughly macOS key-repeat speed, with screenshots taken during the scroll. Flicker a human would notice between those frames can't be ruled out; the torn frames above are what the screenshots caught.
