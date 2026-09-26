# Running code: run all, with queued cells draining one by one and the
# selection following the running cell.
send "jotter pipeline.ipynb"
key enter
wait-for "○ idle"
rec start
pause 800ms
key X
settle
pause 1200ms
rec stop
