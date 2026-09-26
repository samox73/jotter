# Long outputs: a viewport pinned to the live tail, scrolled with [ and ],
# collapsed with o.
send "jotter stream.ipynb"
key enter
wait-for "○ idle"
rec start
pause 1sec
key " "
settle
pause 1sec
key "[" "[" "[" "[" "[" "[" "[" "[" --gap 150ms
pause 1sec
key "]" "]" "]" "]" --gap 150ms
pause 1sec
key o
pause 1200ms
key o
pause 1500ms
rec stop
