# The Ratatui showcase clip: a notebook that solves the Lorenz system with
# SymPy, pre-renders an attractor + Mandelbrot dive, then plays it inline.
#
#   cargo build --release
#   cd docs/media/showcase && betamax run showcase.tape   # betamax >= 0.3.0
#
# Writes target/showcase.gif and .webp; the clip isn't committed.
Output ../../../target/showcase.gif
Output ../../../target/showcase.webp
Set Shell "bash"
Set Width 1200
Set Height 720
Set FontSize 19
Set Framerate 60
Set WebpQuality 85
Set TypingSpeed 18ms
Set Theme "Aardvark Ink"
Set Padding 8
Set Margin 0
Set WindowBar ""
Hide
Type "[ -d .venv ] || uv venv -q; uv pip install -q -r requirements.txt"
Enter
Type "export PATH=$PWD/../../../target/release:$PATH MPLCONFIGDIR=$PWD XDG_CONFIG_HOME=$PWD XDG_STATE_HOME=$(mktemp -d); clear; jotter chaos.ipynb"
Enter
Wait+Screen@120s /ready/
Type "r"
Wait+Screen@30s /\[1\]/
Show
Sleep 1.5s
Type "r"
Sleep 0.5s
Type "r"
Sleep 0.6s
Type "r"
Wait+Screen@60s /48\/48/
Sleep 1s
Type "r"
Wait+Screen@60s /5\/5.*idle/
Sleep 2s
