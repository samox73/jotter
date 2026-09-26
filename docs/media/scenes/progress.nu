# Outputs: a tqdm bar and a display handle, both updating in place.
send "jotter progress.ipynb"
key enter
wait-for "○ idle"
rec start
pause 1sec
key X
settle
pause 2sec
rec stop
