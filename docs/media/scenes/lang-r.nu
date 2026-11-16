# Language smoke test (r): drives a real IRkernel kernel through launch, runs,
# errors, completion, docs, the external editor, interrupt, restart and save.
# Records nothing; each step's result goes to $LANGTEST_OUT/r/results.tsv
# with a screenshot and the screen text next to it. Needs the kernel installed:
#   LANGTEST_OUT=/tmp/langtest nix develop .#docs -c nu docs/media/record.nu lang-r -j 1
let out = $env.LANGTEST_OUT | path join r
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
send "jotter --kernel ir --log jotter.log t.ipynb" --delay 0ms
key enter
step launch { wait-for "○ idle" --timeout 240sec }
note launch_time $"((date now) - $t0)"
step basic {
    key i
    send "x <- c(1, 2, 3)\nprint(sum(x))\nx^2" --delay 0ms
    key shift+enter
    settle --timeout 180sec
    if not (screen | str contains "[1] 6") { error make {msg: "expected output missing"} }
}
step error {
    cell "stop(\"boom\")"
    key shift+enter
    settle --timeout 120sec
    if not (screen | str contains "boom") { error make {msg: "expected error text missing"} }
}
step highlight {
    cell "# a comment\nf <- function(n) {\ns <- \"total\"\nn * 2.5\n}\nf(4)"
    key shift+enter
    settle --timeout 120sec
    if not (screen | str contains "10") { error make {msg: "expected value missing"} }
}
step rich {
    cell "plot(1:10, main = \"ten points\")"
    key shift+enter
    settle --timeout 120sec
    pause 1sec
}
step complete {
    cell "data.fra"
    key tab
    pause 4sec
    snap complete-popup
    if not (screen | str contains "data.frame") { error make {msg: "no completion on screen"} }
}
step inspect {
    cell "print"
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
    cell "s <- readline(\"name: \")\nprint(paste(\"got\", s))"
    key shift+enter
    pause 4sec
    snap stdin-prompt
    send "otter" --delay 0ms
    key enter
    settle --timeout 30sec
    if not (screen | str contains "got") { error make {msg: "no reply echoed"} }
}
step interrupt {
    cell "while (TRUE) Sys.sleep(0.1)"
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
