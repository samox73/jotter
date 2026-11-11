# jOtter macOS test playbook

jOtter 0.6.0 ships a macOS build for Apple Silicon only, and nobody on the project has a Mac. This playbook covers the parts only a Mac can test: Gatekeeper, the install, macOS terminals, a kernel found through uv, and the filesystem. Plan on about 45 minutes. Everything happens inside your clone of the repository, in a folder called `jotter-test` that git ignores, and your results go into `testing/macos/REPORT.md` on this branch.

## Before you start

You need:

- An M-series Mac on macOS 13 or newer, with zsh as your shell (the default).
- [Homebrew](https://brew.sh) and uv: `brew install uv`.
- Your usual terminal, plus Terminal.app, which every Mac has. kitty or Ghostty is the best-supported case and worth installing for step 4: `brew install --cask kitty` or `brew install --cask ghostty`.

Each step says what to run, what should happen, and ends with a checklist. For every item, note pass or fail in `REPORT.md` with one line on what you saw; "image was garbled at the bottom" is plenty. When something misbehaves, run it again with a log, reproduce it, quit, and the log is already where it belongs:

```sh
jotter --log ../testing/macos/results/kitty.log test.ipynb    # name the log after the terminal
```

Screenshot anything visual into the same folder. Inside jOtter, `?` shows every key, `:q` quits, `:q!` quits without saving, and `Ctrl+c` interrupts the kernel.

## Step 1: Clone and set up

1. Download `jotter-aarch64-macos.tar.gz` from the [latest release](https://github.com/samox73/jotter/releases/latest) with your browser and leave it in `~/Downloads`. The browser matters: it marks the download with the quarantine flag that triggers Gatekeeper, and curl doesn't.
2. Clone the repository on the `macos-test` branch and run the setup script:

```sh
git clone -b macos-test https://github.com/samox73/jotter.git
cd jotter
bash testing/macos/setup.sh
```

The script refuses to run on anything but an Apple Silicon Mac. It creates `jotter-test/` with the unpacked release in `release/`, a virtual environment in `.venv` with ipykernel, numpy, matplotlib, sympy and pandas, the test notebook as `test.ipynb`, a copy of it under an awkward path for step 5, and `env.zsh`, which puts the release binary and its zsh completions on your PATH for one terminal session.

Check:

- [ ] The script ends with a line starting with `ready`.
- [ ] `ls -a jotter-test` shows `release`, `.venv`, `test.ipynb` and `env.zsh`.

## Step 2: Gatekeeper and install

The binary is neither signed nor notarized, so the first run of a browser download is where macOS gets in the way. What macOS says, and how hard it is to get past, is the main thing to record here.

1. Run the unpacked binary:

```sh
cd jotter-test
./release/jotter --version
```

2. macOS should block it. Write down the exact wording of the dialog. Then get past it one of two ways, and note which:
    - System Settings, Privacy & Security, scroll down to "jotter was blocked", Open Anyway, then run the command again.
    - Or remove the flag: `xattr -d com.apple.quarantine release/jotter`.
3. Load the test environment and check the completions and the man page. Repeat the `source` line in every new terminal window you open for this playbook.

```sh
source env.zsh
jotter --version
man release/man/jotter.1
```

Then type `jotter --` and press Tab.

4. The PyPI route, which is how people without the tarball install it:

```sh
uvx --from jotter-tui jotter --version
```

Check:

- [ ] `./release/jotter --version` was blocked on the first try and prints `jotter 0.6.0` after the bypass. The wording and the bypass you used are in the report.
- [ ] After `source env.zsh`, `jotter --version` works and Tab after `jotter --` offers `--kernel`, `--log` and `--no-images`.
- [ ] The man page opens.
- [ ] The uvx line prints `jotter 0.6.0` with no Gatekeeper prompt.

## Step 3: The test notebook

`test.ipynb` runs in a few seconds and produces every kind of output jOtter renders. It also checks that jOtter finds the kernel in the `.venv` next to the notebook without you activating anything.

Run `jotter test.ipynb`, then press `X` to run every cell. The status line briefly says `starting kernel`. Cell 13 asks for input: type anything and press Enter. Cell 14 raises an error on purpose, and run-all stops there. Afterwards walk through the cells with `j` and `k` and compare with the table. `[` and `]` scroll inside a long output; `zi` zooms an image and any key returns.

| Cell | What it does | Expected |
| --- | --- | --- |
| 1 | Markdown with inline and display math | Drawn equations, not raw `$` source; a bulleted list |
| 2 | Prints the Python path and the macOS version | A path inside `jotter-test/.venv`. Copy both lines into the report |
| 3 | A matplotlib line plot | Inside the terminal, never a separate window. Sharp in kitty, Ghostty, iTerm2 and WezTerm; blocky in Terminal.app and Alacritty |
| 4 | A colour image of the Mandelbrot set | A smooth, complete viridis gradient |
| 5 | SymPy returns LaTeX | A drawn equation |
| 6 | A pandas DataFrame | An aligned table |
| 7 | Eight lines, streamed | They appear one at a time; the last line with `ü ✓ 🦦` doesn't break the layout |
| 8 | A spinner through `clear_output` | Replaced in place, ending on one line, never twelve stacked ones |
| 9 | An SVG | Rendered as an image reading "SVG ok" |
| 10 | A markdown output | Bold text, a link, drawn math |
| 11 | A hundred lines | Shown as a viewport; `[` and `]` scroll it |
| 12 | stdout and stderr | Both lines show, stderr marked differently |
| 13 | `input()` | The prompt appears in the status line, your answer is echoed back |
| 14 | An exception | A coloured traceback, and no cell after it runs |

Check:

- [ ] Cell 2 prints a path inside `jotter-test/.venv`.
- [ ] Every row matches, or the deviation is in the report with its cell number.
- [ ] No plot opened a separate macOS window.

## Step 4: Terminals

Run the same five-minute script in every terminal you can. Terminal.app is the floor everyone has; kitty or Ghostty is the ceiling. The table says what each should do, so you know whether a blocky image is a bug or the expected fallback. Remember `source env.zsh` in each new window.

| Terminal | Install | Images | Shift+Enter and Ctrl+Enter | `yy` to clipboard |
| --- | --- | --- | --- | --- |
| Terminal.app | built in | coarse half-block image | no: use `r` and `Space` | no, Terminal.app has no OSC 52 |
| kitty | `brew install --cask kitty` | sharp | yes | yes, may ask once |
| Ghostty | `brew install --cask ghostty` | sharp | yes | yes, asks the first time |
| iTerm2 | `brew install --cask iterm2` | sharp | 3.5 and newer: should work; older: `r` and `Space` | after enabling Settings, General, Selection, "Applications in terminal may access clipboard" |
| WezTerm | `brew install --cask wezterm` | sharp | only with `enable_kitty_keyboard = true` in `~/.wezterm.lua` | yes |
| Alacritty | `brew install --cask alacritty` | coarse half-block image | yes | yes |
| Warp | `brew install --cask warp` | unknown: note what you see | unknown | unknown |

The script, in each terminal:

1. `cd` into `jotter-test`, `source env.zsh`, then `jotter test.ipynb` and `X` to run every cell (answer the input prompt).
2. On a cell, press `Shift+Enter`: does it run and move down, or does nothing happen? Then `Ctrl+Enter` on another cell.
3. Look at the plots in cells 3 and 4: sharp, blocky, or missing. On cell 4 press `zi` to zoom it to the whole screen, then any key to return. Check nothing is left behind.
4. Drag the window to a different size, go full screen with the green button or `Ctrl+Cmd+F`, then hold `j` down to the end and `k` back. Watch for flicker, images lagging behind the text, or old images left on screen.
5. Press `i` on a code cell, move the cursor into the middle of a line, press `Ctrl+\`. The cell should split in two. `Esc`, then `:q!`.
6. Reopen, put the cursor on a code cell and press `yy`, then `:q`. Paste with `Cmd+V` at the shell prompt: the cell's source should appear.
7. After each quit: a normal prompt, no leftover images, mouse selection works.

tmux, if you use it: `brew install tmux`, add to `~/.tmux.conf`:

```
set -g allow-passthrough on
set -g set-clipboard on
```

restart tmux and repeat 1 to 3 inside it, in kitty or Ghostty. Expect sharp images but no `Shift+Enter`, since tmux drops it; `r` runs the cell. `Ctrl+\` may arrive as `Ctrl+4`, and both split.

Check: one row per terminal in the report's terminal table, with the terminal's version (`kitty --version`, or the About box) and anything odd you saw.

## Step 5: Files on a Mac

What differs on macOS: the filesystem folds case and stores accented names in a different Unicode form, and iCloud Drive syncs files underneath you. The kernel lookup only sees a `.venv` next to the notebook, so for this step activate it: `source .venv/bin/activate` inside `jotter-test`.

1. Spaces and accents. The setup script put a copy at `my notes/ünïcode/tést.ipynb`:

```sh
jotter "my notes/ünïcode/tést.ipynb"
```

Run cell 2 with `Space`, press `w`, then `:q`. `ls -l "my notes/ünïcode/"` should show the file modified just now.

2. Case folding. Open `jotter test.ipynb` and type `:w Test.ipynb`. On APFS that is the same file, so jOtter should treat it as overwriting an existing file and ask for a second Enter. Then `:q` and `ls`: still one test notebook, whichever spelling.
3. iCloud Drive, only if you use it:

```sh
cp test.ipynb "$HOME/Library/Mobile Documents/com~apple~CloudDocs/jotter-icloud.ipynb"
jotter "$HOME/Library/Mobile Documents/com~apple~CloudDocs/jotter-icloud.ipynb"
```

Run cell 2, `w`, wait a minute while iCloud syncs, run it again, `w`. Neither save should trip a warning about the file having changed on disk.

Check:

- [ ] The spaced, accented path opens, runs and saves.
- [ ] `:w Test.ipynb` asks before overwriting and leaves one file behind.
- [ ] iCloud: both saves stick, no spurious warning.

## Step 6: Submit your results

1. Fill in `testing/macos/REPORT.md`.
2. Put logs and screenshots in `testing/macos/results/`.
3. Commit and push to this branch:

```sh
cd ..    # back to the repository root
git add testing/macos/REPORT.md testing/macos/results
git commit -m "macOS test results"
git push
```

If you can't push, fork the repository, push there, and open a pull request against the `macos-test` branch.
