# Demo GIF script — regenerate with: vhs assets/demo.tape
# (https://github.com/charmbracelet/vhs; graphics fall back to halfblocks
# inside vhs's terminal — record in kitty + asciinema for pixel plots)
Output assets/demo.gif
Set FontSize 15
Set Width 1200
Set Height 720
Set TypingSpeed 60ms

Type "jotter showcase.ipynb"
Enter
Sleep 3s

# key cheatsheet
Type "?"
Sleep 2.5s
Type " "
Sleep 500ms

# run all, watch the queue drain and outputs stream
Ctrl+R
Sleep 8s

# scroll a long output's viewport
Type "3G"
Sleep 1s
Type "[["
Sleep 1.5s
Type "o"
Sleep 1.5s
Type "o"

# search
Type "/spinner"
Enter
Sleep 1.5s

# quit (dirty -> discard)
Type "q"
Sleep 800ms
Type "q"
Sleep 500ms
