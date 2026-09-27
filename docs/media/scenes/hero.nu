# Landing page: open a notebook from the shell and run it top to bottom.
# Markdown with math, code, output, and a plot, in one take. Paced for a
# first-time viewer: time to read the notebook, and to look at the result.
rec start
pause 800ms
send "jotter tour.ipynb" --delay 70ms
pause 1000ms
key enter
wait-for "cell 1/4"
pause 2500ms # read the markdown and the equation
key X # run all
settle
pause 3500ms # the finished plot
rec stop
