# Theme gallery: the same notebook in every syntax theme, each on a terminal
# background that suits it. Plots follow the terminal's colours on their own.
send "jotter gallery.ipynb"
key enter
wait-for "○ idle"
key X
settle
key w # keep the outputs for the other themes
pause 500ms
key q
let themes = [
    [theme, slug, bg, fg];
    ["base16-ocean.dark", ocean-dark, "#2b303b", "#c0c5ce"]
    ["base16-eighties.dark", eighties-dark, "#2d2d2d", "#d3d0c8"]
    ["base16-mocha.dark", mocha-dark, "#3b3228", "#d0c8c6"]
    ["Solarized (dark)", solarized-dark, "#002b36", "#839496"]
    ["base16-ocean.light", ocean-light, "#eff1f5", "#4f5b66"]
    ["InspiredGitHub", inspired-github, "#ffffff", "#323232"]
    ["Solarized (light)", solarized-light, "#fdf6e3", "#657b83"]
]
for t in $themes {
    config {theme: $t.theme}
    colors $t.bg $t.fg
    send "clear"
    key enter
    send "jotter gallery.ipynb" --delay 0ms
    key enter
    wait-for "cell 1/3"
    pause 1500ms # images drawn
    still $"theme-($t.slug)"
    key q
    pause 500ms
}
