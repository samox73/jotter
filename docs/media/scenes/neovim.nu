# Embedded Neovim: text objects, visual mode and undo inside a cell, with
# jotter still drawing everything.
height 300
config {editor: nvim, nvim_user_config: false}
send "jotter neovim.ipynb"
key enter
wait-for "○ idle"
rec start
pause 1sec
key enter # edit in nvim, normal mode
pause 800ms
key j j --gap 300ms
key f "(" --gap 300ms
pause 400ms
key c i "(" --gap 300ms # change inside the parentheses
send "f\"{total = }\"" --delay 60ms
key escape
pause 1200ms
key g g V j --gap 350ms # select two lines
pause 1200ms
key d
pause 1000ms
key u
pause 1000ms
key escape # leave the cell
pause 1500ms
rec stop
