# Kernel picker smoke test: a notebook asking for a Julia kernel that isn't
# installed, with another Julia kernel and at least one other kernel that are.
# Run and read like the lang-* scenes; results in $LANGTEST_OUT/pick.
let out = $env.LANGTEST_OUT | path join pick
mkdir $out
def snap [tag: string] {
    screen | save -f ($out | path join $"($tag).txt")
    ^grim -o HEADLESS-1 ($out | path join $"($tag).png")
}
def step [name: string, body: closure] {
    let r = try { do $body; "ok" } catch {|e| $"FAIL: ($e.msg | lines | first)" }
    snap $name
    $"($name)\t($r)\n" | save -a ($out | path join results.tsv)
}
def expect [text: string] {
    if not (screen | str contains $text) { error make {msg: $"missing: ($text)"} }
}
let nb = $env.JREC_WORK | path join pick.ipynb
{
    cells: [{cell_type: code, id: c1, metadata: {}, outputs: [], execution_count: null, source: ["1 + 1"]}]
    metadata: {kernelspec: {name: "julia-1.11", display_name: "Julia 1.11", language: julia}}
    nbformat: 4, nbformat_minor: 5
} | to json | save -f $nb
height 900
send "jotter --log jotter.log pick.ipynb" --delay 0ms
key enter
step asks {
    wait-for "start which?" --timeout 60sec
    expect "  julia"
    if (screen | str contains "  python3") { error make {msg: "a Python kernel offered to a Julia notebook"} }
}
step decline { key escape; pause 1sec; expect "no kernel — R to pick one" }
step run-without { key shift+enter; pause 1sec; expect "no kernel" }
step asks-again { key R; wait-for "start which?" --timeout 60sec }
step filter {
    send "zz" --delay 100ms; pause 500ms
    if (screen | str contains "  julia") { error make {msg: "filter kept julia"} }
    key backspace backspace
    send "JUL" --delay 100ms; pause 500ms
    expect "  julia"
}
step pick { key down up enter; wait-for "○ idle" --timeout 240sec; expect "ready" }
step run { key shift+enter; settle --timeout 180sec; expect "In [1]"; expect "\n2" }
step save {
    key w; pause 2sec
    let name = open --raw $nb | from json | get metadata.kernelspec.name
    if not ($name | str starts-with "julia") or $name == "julia-1.11" { error make {msg: $"saved kernelspec is ($name)"} }
}
key q
pause 1sec
cp $nb $out
cp ($env.JREC_WORK | path join jotter.log) $out
