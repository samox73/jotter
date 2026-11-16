# Still: a notebook whose kernel isn't installed, and the popup asking which
# one to start instead. The Julia kernelspecs are dummies (never
# started); XDG_DATA_HOME hides whatever the recording machine has installed.
let specs = $env.JREC_WORK | path join jupyter
for k in [[name, display, language]; [julia-1.12, "Julia 1.12", julia] [julia-1.10, "Julia 1.10", julia]] {
    let dir = $specs | path join kernels $k.name
    mkdir $dir
    {argv: ["true"], display_name: $k.display, language: $k.language} | to json | save ($dir | path join kernel.json)
}
{
    cells: [{cell_type: code, id: c1, metadata: {}, outputs: [], execution_count: null, source: ["using LinearAlgebra\n", "norm([3, 4])"]}]
    metadata: {kernelspec: {name: "julia-1.11", display_name: "Julia 1.11", language: julia}}
    nbformat: 4, nbformat_minor: 5
} | to json | save ($env.JREC_WORK | path join orbits.ipynb)
height 400
send $"JUPYTER_PATH=($specs) XDG_DATA_HOME=($env.JREC_WORK)/data jotter orbits.ipynb" --delay 0ms
key enter
wait-for "start which?"
pause 500ms
still kernel-picker
key escape q
