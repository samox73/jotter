"""Helpers for chaos.ipynb: a Lorenz attractor and a Mandelbrot dive, one PNG per frame."""
import io
import numpy as np, matplotlib.pyplot as plt
from matplotlib.colors import LinearSegmentedColormap

gold = LinearSegmentedColormap.from_list("gold", ["#000764", "#206bcb", "#edffff", "#ffaa00", "#000200", "#000764"])

def escape(w, cx=-0.743643, cy=0.131825, W=360, H=216, n=300):
    c = np.add.outer(1j*np.linspace(cy-w*.6, cy+w*.6, H), np.linspace(cx-w, cx+w, W)).ravel()
    k = np.full(c.size, np.nan); idx = np.arange(c.size); z = np.zeros_like(c); cc = c.copy()
    for i in range(n):
        z = z*z + cc; out = np.abs(z) > 2
        k[idx[out]] = i; keep = ~out; z, cc, idx = z[keep], cc[keep], idx[keep]
    return k.reshape(H, W)

def frame(path, f, n):
    fig = plt.figure(figsize=(13, 3.6)); fig.subplots_adjust(0, 0, 1, 1, wspace=0)
    a, b = fig.add_subplot(121, projection="3d"), fig.add_subplot(122)
    a.set_axis_off(); a.view_init(25, -60 + 35*np.sin(2*np.pi*f/n)); a.set_box_aspect(None, zoom=1.4)
    for seg, c in zip(np.array_split(path[:, :300 + 55*f], 24, axis=1), plt.cm.cool(np.linspace(0, 1, 24))):
        a.plot(*seg, color=c, lw=0.6)
    b.set_axis_off(); b.imshow((np.sqrt(escape(1.5*0.8**f, n=150+8*f))*0.6) % 1, cmap=gold, aspect="auto")
    buf = io.BytesIO(); fig.savefig(buf, format="png"); plt.close(fig)
    return buf.getvalue()
