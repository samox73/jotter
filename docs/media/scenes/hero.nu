# Landing page: open a notebook from the shell and run it top to bottom.
# Markdown with math, code, output, and a plot, in one take.
rec start
pause 600ms
send "jotter tour.ipynb"
key enter
wait-for "cell 1/4"
pause 1sec
key X # run all; queued until the kernel is up
settle
pause 2sec
rec stop
