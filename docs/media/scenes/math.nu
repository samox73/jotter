# Math: inline and display math in markdown, a latex cell, and a SymPy result.
send "jotter math.ipynb"
key enter
wait-for "○ idle"
rec start
pause 2500ms
key G
pause 500ms
key " " # the SymPy integral
settle
pause 3sec
rec stop
