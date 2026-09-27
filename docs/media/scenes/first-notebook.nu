# Your first notebook: the getting-started tour, start to finish.
config {complete_on_dot: false}
rec start
pause 800ms
send "jotter first.ipynb" --delay 60ms
key enter
wait-for "○ idle"
pause 1200ms
key i
send "import numpy as np
x = np.linspace(0, 2 * np.pi, 200)
print(\"points:\", len(x))"
pause 500ms
key shift+enter
settle
pause 1sec
key a i --gap 400ms
send "import matplotlib.pyplot as plt
plt.plot(x, np.sin(x))
plt.show()"
pause 500ms
key shift+enter
settle
pause 1500ms
key b m i --gap 500ms
send "## A sine wave

We plot $y = \\sin x$ for $x \\in [0, 2\\pi]$:

$$\\int_0^{2\\pi} \\sin x \\, dx = 0$$"
pause 500ms
key escape escape --gap 400ms
pause 2sec
key w
pause 2500ms
rec stop
