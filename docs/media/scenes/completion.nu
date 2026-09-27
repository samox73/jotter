# Completion: the popup opens after a dot, filters as you type; Shift+Tab
# shows the docs for the highlighted entry.
send "jotter completion.ipynb"
key enter
wait-for "○ idle"
key " " # import numpy, off camera
settle
key j
rec start
pause 800ms
key i
send "x = np." --delay 60ms
pause 1200ms
send "lin" --delay 150ms
pause 1000ms
key down
pause 800ms
key shift+tab # docs for the highlighted entry
pause 3sec
key escape
pause 500ms
key enter # accept
pause 500ms
send "(0, 1, 5)" --delay 60ms
pause 1500ms
rec stop
