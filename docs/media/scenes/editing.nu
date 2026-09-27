# Editing cells: normal and insert mode in the builtin editor, with vim
# motions, a deleted line and undo.
height 300
send "jotter editing.ipynb"
key enter
wait-for "○ idle"
rec start
pause 1sec
key enter # edit the cell, normal mode
pause 700ms
key j j "$" --gap 350ms
pause 400ms
key a
send " if signal else 0.0" --delay 45ms
pause 500ms
key escape
pause 500ms
key k d d --gap 350ms # delete the docstring
pause 900ms
key u # and bring it back
pause 900ms
key escape # leave the cell
pause 1500ms
rec stop
