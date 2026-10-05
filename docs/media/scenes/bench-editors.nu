# Speed comparison: jOtter and euporie-notebook on the same notebooks, in the
# same kitty, driven by the same keystrokes, with `cat` as the floor: what
# the terminal and this harness take on their own (kitty echoing typed keys).
# Not a recorded clip: run it on its own (record.nu skips bench-* scenes
# unless named), on an idle machine:
#
#   make bench   # = nix develop .#docs -c nu docs/media/record.nu -j 1 bench-editors
#
# Prints a table and writes docs/media/bench-results.json. BENCH_RUNS sets
# the runs per tool and notebook (default 5); BENCH_TOOLS picks the tools
# (default "floor jotter euporie").
#
# Measures, per run (a fresh copy of the notebook in a new kitty tab, closed
# afterwards, so quit dialogs and autosave recovery never interfere):
#   startup_first_ms   launch until the notebook's title is on screen
#   kernel_ready_ms    launch until the screen stops changing; in practice the
#                      last change is the kernel status turning ready
#   key_ms / key_p95   one keypress (j or k, next/previous cell) until the
#                      screen changes, 40 times a run: median and 95th percentile
#   char_ms / char_p95 the same for one character typed into a cell
#   burst_nav_ms       a burst of j/k (to the end and back, 3 times) until the
#                      screen has been still for 1.5 s
#   burst_type_ms      the same for 200 typed characters
#   rss_mib            the tool's resident memory at the end
# The timing loops run in kitty_probe.py, which talks to kitty's remote-control
# socket directly (well under a millisecond per screen read; `kitty @` would
# start a process per read, ~45 ms). The floor row shows what the terminal
# and the harness take on their own.

const repo = path self ../../..
let runs = $env.BENCH_RUNS? | default "5" | into int
let tools = $env.BENCH_TOOLS? | default "floor jotter euporie" | split row " "
let work = $env.JREC_WORK

# same logical size as the clips (960x600, so the same rows and columns),
# but at scale 1: a quarter of the pixels for kitty's software renderer
sway output HEADLESS-1 resolution 960x600 scale 1
sleep 500ms

# the notebooks: the showcase, and five showcases in a row (70 cells)
# (Python's json: nushell's `to json` leaves the ANSI escapes in saved
# tracebacks unescaped, which isn't valid JSON)
cp ($repo | path join showcase.ipynb) ($work | path join bench-14.ipynb)
^python3 -c "import json,sys; nb=json.load(open(sys.argv[1])); nb['cells']*=5; json.dump(nb, open(sys.argv[2],'w'), indent=1)" ($repo | path join showcase.ipynb) ($work | path join bench-70.ipynb)

def --wrapped rc [...args: string] { ^kitty @ --to $env.JREC_KITTY ...$args }
# timing loops over kitty's socket: shown, settled, per-key, burst
def --wrapped probe [win: int, ...args: string] {
    ^python3 ($repo | path join docs media kitty_probe.py) $env.JREC_KITTY ($win | into string) ...$args | from json
}

def p95 [] { let xs = $in | sort; $xs | get (((($xs | length) - 1) * 0.95) | math floor) }

def rss-mib [pattern: string] {
    let pid = ^pgrep -n -f $pattern | str trim
    (^ps -o rss= -p $pid | str trim | into int) / 1024 | math round --precision 1
}

# One run of `tool` on `nb`. The floor is `cat`: kitty echoes what's typed,
# so it has no startup, no cells and no kernel, only the input path.
def run-once [tool: string, nb: string, i: int, title_text: string] {
    let file = $"($nb | str replace '.ipynb' '')-($tool)-($i).ipynb"
    cp ($env.JREC_WORK | path join $nb) ($env.JREC_WORK | path join $file)
    let cmd = match $tool {
        "jotter" => [jotter $file]
        "euporie" => [$env.EUPORIE $file]
        _ => [cat]
    }
    let floor = $tool == "floor"
    # euporie finds the work dir's kernelspec through JUPYTER_PATH, and runs
    # on its own Python: the docs shell's PYTHONPATH (Python 3.14) must not
    # leak into it
    let env_args = [--env $"JUPYTER_PATH=($env.JREC_WORK)/.venv/share/jupyter" --env PYTHONPATH=]
    let t0 = date now | into int
    let win = rc launch --type=tab --hold --cwd $env.JREC_WORK ...$env_args ...$cmd | str trim | into int
    let first = if $floor { null } else { probe $win shown $title_text ($t0 | into string) }
    let ready = if $floor { sleep 500ms; null } else { probe $win settled ($t0 | into string) }
    # navigation: single keys (down, up, ...), then a burst to the end and
    # back, three times
    let keys = probe $win per-key "40" j k
    let cells = if $nb == bench-14.ipynb { 14 } else { 70 }
    let burst = (1..3 | each { ("j" | fill -c "j" -w ($cells - 1)) + ("k" | fill -c "k" -w ($cells - 1)) } | str join)
    let nav = probe $win burst $burst
    # typing into the selected cell: single characters, then a burst
    match $tool {
        "jotter" => { "A" | rc send-text --match $"id:($win)" --stdin }
        "euporie" => { rc send-key --match $"id:($win)" enter }
        _ => {}
    }
    sleep 500ms
    let chars = probe $win per-key "40" a
    let typed = "x = 1 + 2  # the quick brown fox jumps over the lazy dog " | fill -c "y" -w 200
    let type = probe $win burst $typed
    rc send-key --match $"id:($win)" escape
    let mem = match $tool {
        "jotter" => (rss-mib $"jotter ($file)")
        "euporie" => (rss-mib $"euporie.*($file)")
        _ => null
    }
    rc close-window --match $"id:($win)"
    sleep 1sec
    {
        startup_first_ms: $first, kernel_ready_ms: $ready, keys: $keys, chars: $chars,
        burst_nav_ms: $nav, burst_keys: ($burst | str length), burst_type_ms: $type, rss_mib: $mem
    }
}

let title_text = "feature showcase"
let median_or_null = {|xs| let v = $xs | compact; if ($v | is-empty) { null } else { $v | math median } }
let results = $tools | each {|tool|
    [bench-14.ipynb bench-70.ipynb] | each {|nb|
        let rs = 1..$runs | each {|i| run-once $tool $nb $i $title_text }
        print $"($tool) ($nb): ($runs) runs"
        # single-key samples from all runs pooled (runs x 40)
        let keys = $rs.keys | flatten
        let chars = $rs.chars | flatten
        {
            tool: $tool
            notebook: $nb
            startup_first_ms: (do $median_or_null $rs.startup_first_ms)
            kernel_ready_ms: (do $median_or_null $rs.kernel_ready_ms)
            key_ms: ($keys | math median)
            key_p95_ms: ($keys | p95)
            char_ms: ($chars | math median)
            char_p95_ms: ($chars | p95)
            burst_nav_ms: ($rs.burst_nav_ms | math median)
            burst_keys: ($rs.burst_keys | first)
            burst_type_ms: ($rs.burst_type_ms | math median)
            rss_mib: (do $median_or_null $rs.rss_mib)
        }
    }
} | flatten

let meta = {
    date: (date now | format date "%Y-%m-%d")
    runs: $runs
    jotter: (^jotter --version | str trim)
    euporie: (^$env.EUPORIE --version | str trim)
    kitty: (^kitty --version | str trim)
    cpu: (sys cpu | first | get brand)
}
{meta: $meta, results: $results} | to json | save -f ($repo | path join docs media bench-results.json)
print ($results | table --expand)
