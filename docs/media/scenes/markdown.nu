# Markdown cells: rendered, with a table, code, an image; Enter shows the
# source, Esc renders it again.
send "jotter markdown.ipynb"
key enter
wait-for "○ idle"
rec start
pause 3sec
key enter
pause 2500ms
key escape
pause 2sec
rec stop
