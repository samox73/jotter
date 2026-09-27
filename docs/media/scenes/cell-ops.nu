# Cell operations: add, delete, paste, move, merge and undo, in notebook mode.
height 380
send "jotter cells.ipynb"
key enter
wait-for "○ idle"
rec start
pause 1sec
key j j --gap 400ms
pause 500ms
key d d # delete "clean"
pause 1200ms
key p # paste it back below
pause 1200ms
key K # move it up again
pause 1200ms
key j M # merge "fit" with "plot"
pause 1500ms
key u # undo the merge
pause 1500ms
key a # a new cell below
pause 1500ms
rec stop
