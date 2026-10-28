<div align="center">

# jOtter 🦦

**A fast Jupyter notebook TUI. The Jupyter otter.**

<img src="https://raw.githubusercontent.com/samox73/jotter/main/assets/banner.png" alt="jOtter mascot" width="400"/>

[Documentation](https://samox73.github.io/jotter/) · [Install](https://samox73.github.io/jotter/getting-started/installation/) · [Keybindings](https://samox73.github.io/jotter/reference/keybindings/) · [Changelog](https://github.com/samox73/jotter/blob/main/CHANGELOG.md)

</div>

Open, edit and run real `.ipynb` notebooks in your terminal, with vim keys, inline plots and rendered LaTeX, and keystroke latency so low that holding a key is a non-event.

<div align="center">
<img src="https://raw.githubusercontent.com/samox73/jotter/main/docs/public/media/hero.gif" alt="jOtter running a notebook: SymPy finds the fixed points of the Lorenz system, then the Lorenz attractor and a Mandelbrot zoom play as an inline animation" width="720"/>
</div>

- **Your notebooks, untouched:** opens and saves `.ipynb` losslessly, byte for byte like Jupyter, so collaborators on JupyterLab never notice.
- **Plots and math in the terminal:** matplotlib figures inline (kitty graphics, sixel, iTerm2), drawn in your terminal's colours, and LaTeX rendered without a TeX install.
- **Vim keys everywhere:** a modal cell editor, or an embedded Neovim with your own config.
- **Any kernel:** finds your project's venv, uv, conda or pixi environment on its own.
- **Hard to lose work:** atomic saves, autosave and crash recovery, and no silent overwrites.

## Install

```sh
cargo install jotter-tui --locked                 # crates.io (the binary is `jotter`)
nix run github:samox73/jotter -- notebook.ipynb  # Nix
uv tool install jotter-tui                        # PyPI
```

Every [release](https://github.com/samox73/jotter/releases) has `.deb`, `.rpm` and `.apk` packages and a static binary for any Linux distribution; the [installation guide](https://samox73.github.io/jotter/getting-started/installation/) covers them all. To run code you need a Jupyter kernel, such as `pip install ipykernel` in your project's environment.

## Use

```sh
jotter notebook.ipynb
```

Press `?` inside for the keys. [Your first notebook](https://samox73.github.io/jotter/getting-started/first-notebook/) is a five-minute tour.

## Contributing

Bug reports and pull requests are welcome; see [Contributing](https://samox73.github.io/jotter/project/contributing/).

## AI use

jOtter is built with [Claude Code](https://claude.com/claude-code). I direct the work and review the changes; the tests run on every change in CI.

## License

Licensed under either of [Apache License 2.0](https://github.com/samox73/jotter/blob/main/LICENSE-APACHE) or [MIT](https://github.com/samox73/jotter/blob/main/LICENSE-MIT), at your option.
