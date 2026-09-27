# External editor: E opens the cell in $EDITOR (here nvim); the edit comes back.
send "jotter cells.ipynb"
key enter
wait-for "○ idle"
rec start
pause 1sec
key j j --gap 400ms
pause 600ms
key E
wait-for "clean(data)"
pause 1200ms
key A
send "  # drop NaNs and outliers" --delay 45ms
key escape
pause 800ms
send ":wq" --delay 120ms
pause 400ms
key enter
wait-for "VIEW"
pause 2sec
rec stop
