# Search: find cells by their source with /, then n and N.
send "jotter search.ipynb"
key enter
wait-for "○ idle"
rec start
pause 1sec
key "/"
send "fit" --delay 80ms
pause 600ms
key enter
pause 1300ms
key n
pause 1300ms
key n
pause 1300ms
key N
pause 1500ms
rec stop
