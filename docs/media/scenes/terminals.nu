# Terminal gallery: the same notebook in other terminals (and tmux), one still
# each. jotter logs the graphics and keyboard protocol it detected in each; the
# scene prints them, and the compatibility table is checked against that.
send "jotter gallery.ipynb"
key enter
wait-for "○ idle"
key X
settle
key w
pause 500ms
still term-kitty
key q
pause 500ms

# the "terminal:" line jotter logs at startup
def detected [log: string] {
    open --raw $log | lines | where $it =~ "terminal:" | first | str replace -r '^.*terminal: ' ''
}

# the same font everywhere, so the stills compare like for like
let terminals = [
    [slug, command];
    [foot, "foot --font='JetBrains Mono:size=11.5'"]
    [alacritty, "alacritty -o 'font.normal.family=\"JetBrains Mono\"' -o font.size=11.5 -e"]
    [wezterm, "wezterm --config 'font=wezterm.font(\"JetBrains Mono\")' --config font_size=11.5 --config enable_tab_bar=false --config 'window_close_confirmation=\"NeverPrompt\"' --config enable_wayland=true start --always-new-process --cwd . --"]
    [ghostty, "ghostty --font-family='JetBrains Mono' --font-size=11.5 --confirm-close-surface=false -e"]
]
for t in $terminals {
    let log = $env.JREC_RUN | path join $"($t.slug).log"
    let jlog = $env.JREC_RUN | path join $"($t.slug).jotter.log"
    # the redirect covers the whole subshell, so nothing keeps our pipe open
    let pid = ^bash -c $"\(cd '($env.JREC_WORK)' && exec ($t.command) jotter --log '($jlog)' gallery.ipynb\) > '($log)' 2>&1 & echo $!" | str trim
    pause 5sec # start, query the terminal, draw
    sway fullscreen enable
    pause 1500ms
    still $"term-($t.slug)"
    ^kill $pid | complete | ignore # it may have exited already
    pause 1sec
    let got = try { detected $jlog } catch { $"no jotter log; terminal said:\n(open --raw $log | lines | last 6 | str join "\n")" }
    print $"   ($t.slug): ($got)"
}

# tmux inside kitty, with image passthrough on
"set -g allow-passthrough on\n" | save -f ($env.JREC_WORK | path join tmux.conf)
let jlog = $env.JREC_RUN | path join tmux.jotter.log
send $"clear; tmux -f tmux.conf new-session -c ($env.JREC_WORK) jotter --log ($jlog) gallery.ipynb" --delay 0ms
key enter
pause 5sec # no still: detection only, for the compatibility table
print $"   tmux: (detected $jlog)"
