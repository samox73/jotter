# input(): the prompt opens a box; the answer goes back to the kernel.
height 320
send "jotter input.ipynb"
key enter
wait-for "○ idle"
rec start
pause 1sec
key " "
wait-for "your name"
pause 1200ms
send "Ada" --delay 150ms
pause 800ms
key enter
settle
pause 2sec
rec stop
