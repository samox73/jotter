# Landing page: the showcase notebook. SymPy finds the Lorenz system's fixed
# points, then 48 pre-rendered frames of the attractor and a Mandelbrot dive
# play inline. Paced for a first-time viewer. The setup cell (imports,
# integration, rendering the frames) runs off camera: ten seconds of a
# progress bar, which has its own clip.
# the Mandelbrot palette stays, and the fixed-point matrix renders at its natural size
config {recolor_plots: false, max_image_rows: 16, max_math_rows: 6}
send "jotter chaos.ipynb" --delay 0ms
key enter
wait-for "○ idle"
key r # setup
wait-for "[1]" --timeout 180sec
key z t # the equations at the top: the setup cell stays off camera
rec start
pause 1000ms
key r # the markdown cell: the equations
pause 700ms
key r # the fixed points
settle
pause 1800ms # read them
key r # play
settle
pause 1500ms
rec stop
