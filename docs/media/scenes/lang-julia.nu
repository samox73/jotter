# Language smoke test (julia): drives a real IJulia kernel through launch, runs,
# errors, completion, docs, the external editor, interrupt, restart and save.
# Records nothing; each step's result goes to $LANGTEST_OUT/julia/results.tsv
# with a screenshot and the screen text next to it. Needs the kernel installed:
#   LANGTEST_OUT=/tmp/langtest nix develop .#docs -c nu docs/media/record.nu lang-julia -j 1
# The kernelspec is named after the Julia version: set LANGTEST_JULIA if it isn't julia-1.12.
let out = $env.LANGTEST_OUT | path join julia
mkdir $out
def snap [tag: string] {
    screen | save -f ($out | path join $"($tag).txt")
    ^grim -o HEADLESS-1 ($out | path join $"($tag).png")
}
def note [step: string, result: string] {
    $"($step)\t($result)\n" | save -a ($out | path join results.tsv)
}
def step [name: string, body: closure] {
    let r = try { do $body; "ok" } catch {|e| $"FAIL: ($e.msg | lines | first)" }
    snap $name
    note $name $r
    # back to notebook mode with an idle kernel, whatever happened
    key escape escape escape
    if not (screen | str contains "○ idle") { key ctrl+c; pause 2sec }
}
def cell [src: string] {
    key a i --gap 300ms
    send $src --delay 0ms
    pause 300ms
}
height 900
let t0 = date now
send $"jotter --kernel ($env.LANGTEST_JULIA? | default julia-1.12) --log jotter.log t.ipynb" --delay 0ms
key enter
step launch { wait-for "○ idle" --timeout 240sec }
note launch_time $"((date now) - $t0)"
step basic {
    key i
    send "x = [1, 2, 3]\nprintln(\"sum: \", sum(x))\nx .^ 2" --delay 0ms
    key shift+enter
    settle --timeout 180sec
    if not (screen | str contains "sum: 6") { error make {msg: "expected output missing"} }
}
step error {
    cell "error(\"boom\")"
    key shift+enter
    settle --timeout 120sec
    if not (screen | str contains "boom") { error make {msg: "expected error text missing"} }
}
step highlight {
    cell "# a comment\nfunction f(n::Int)\ns = \"total\"\nreturn n * 2.5\nend\nf(4)"
    key shift+enter
    settle --timeout 120sec
    if not (screen | str contains "10.0") { error make {msg: "expected value missing"} }
}
step rich {
    cell "display(\"text/html\", \"<b>bold</b> html\")"
    key shift+enter
    settle --timeout 120sec
    pause 1sec
}
step complete {
    cell "uppercasef"
    key tab
    pause 4sec
    snap complete-popup
    if not (screen | str contains "uppercasefirst") { error make {msg: "no completion on screen"} }
}
step inspect {
    cell "uppercasefirst"
    key shift+tab
    pause 4sec
}
step editor {
    key ctrl+g
    pause 2sec
    send ":echo expand('%:t') . ' ft=' . &filetype" --delay 0ms
    key enter
    pause 500ms
    snap editor-open
    send ":q!" --delay 0ms
    key enter
    pause 1sec
}
step stdin {
    cell "s = readline()\nprintln(\"got \", s)"
    key shift+enter
    pause 4sec
    snap stdin-prompt
    send "otter" --delay 0ms
    key enter
    settle --timeout 30sec
    if not (screen | str contains "got") { error make {msg: "no reply echoed"} }
}
step interrupt {
    cell "while true; sleep(0.1); end"
    key shift+enter
    pause 5sec
    snap interrupt-running
    if not (screen | str contains "In [*]") { error make {msg: "loop is not running"} }
    key ctrl+c
    settle --timeout 30sec
}
step restart {
    key R
    pause 500ms
    key R
    wait-for "○ idle" --timeout 240sec
    key g g
    key shift+enter
    settle --timeout 180sec
}
step save {
    key w
    pause 1500ms
}
key q
pause 1sec
cp ($env.JREC_WORK | path join t.ipynb) $out
cp ($env.JREC_WORK | path join jotter.log) $out
