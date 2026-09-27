# Data safety: jotter is killed with unsaved work; reopening offers the
# autosave back.
config {autosave_secs: 1}
send "jotter tour.ipynb"
key enter
wait-for "○ idle"
key G a i
send "print(\"unsaved work\")"
key escape escape
pause 2500ms # autosave
crash-jotter
pause 500ms
send "reset"
key enter
pause 1500ms
rec start
pause 800ms
send "jotter tour.ipynb" --delay 60ms
key enter
wait-for "r: restore"
pause 2500ms
key r
pause 1500ms
key G
pause 2500ms
rec stop
