# Outputs: type a plot, run it, zoom into the figure with z.
# No completion popups while typing: they'd distract from the plot.
config {complete_on_dot: false}
send "jotter plots.ipynb"
key enter
wait-for "○ idle"
key space # style cell, off camera
settle
key j
rec start
pause 600ms
key i
send "x = np.linspace(-3, 3, 400)
for s in (0.5, 1, 2):
    plt.plot(x, np.exp(-x**2 / (2 * s**2)), label=f\"σ = {s}\")
plt.legend()
plt.show()"
pause 400ms
key shift+enter
settle
pause 1500ms
key z
pause 2sec
key escape
pause 1sec
rec stop
