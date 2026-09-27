# Commands for scene scripts (scenes/*.nu), which record.nu runs one by one.
# A scene drives a real jotter in a real kitty through kitty's remote control:
# `send` types text, `key` presses keys, `wait-for` waits on what's on screen.

def --wrapped rc [...args: string] {
    ^kitty @ --to $env.JREC_KITTY ...$args
}

# The terminal's visible text.
export def screen []: nothing -> string {
    rc get-text
}

# Press keys: single characters as typed (`key X`, `key ?`), everything else
# in kitty's key syntax (`key shift+enter`, `key ctrl+c`, `key escape`).
# (kitty's send-key drops the shift from `shift+x`, hence the split.)
export def key [...keys: string, --gap: duration = 120ms] {
    for k in $keys {
        if ($k | str length) == 1 {
            $k | rc send-text --stdin
        } else {
            rc send-key $k
        }
        sleep $gap
    }
}

# Type text like a person would; newlines press Enter. Two characters per
# remote-control call keeps the pace natural despite each call's overhead.
export def send [text: string, --delay: duration = 25ms] {
    let lines = $text | lines
    for line in ($lines | enumerate) {
        for chunk in ($line.item | split chars | chunks 2) {
            $chunk | str join | rc send-text --stdin
            sleep $delay
        }
        if $line.index < ($lines | length) - 1 {
            key enter --gap $delay
        }
    }
}

export def pause [d: duration] {
    sleep $d
}

# Wait until `pattern` is on screen.
export def wait-for [pattern: string, --timeout: duration = 30sec] {
    let deadline = (date now) + $timeout
    while not (screen | str contains $pattern) {
        if (date now) > $deadline {
            error make {msg: $"timed out waiting for '($pattern)' on screen:\n(screen)"}
        }
        sleep 100ms
    }
}

# Wait until the kernel is idle and no visible cell is running or queued.
export def settle [--timeout: duration = 60sec] {
    let deadline = (date now) + $timeout
    mut calm = 0
    while $calm < 5 {
        let s = screen
        let quiet = ($s | str contains "○ idle") and not ($s | str contains "In [*]") and not ($s | str contains "In [·]")
        $calm = if $quiet { $calm + 1 } else { 0 }
        if (date now) > $deadline {
            error make {msg: $"kernel never settled; screen:\n($s)"}
        }
        sleep 100ms
    }
}

# Start recording the screen; `rec stop` ends the clip.
export def "rec start" [] {
    let file = $env.JREC_RUN | path join $"($env.JREC_SCENE).mkv"
    let pid = ^bash -c $"wf-recorder -D -o HEADLESS-1 -r 30 -c libx264rgb -p crf=0 -p preset=ultrafast -f '($file)' > '($file).log' 2>&1 & echo $!"
    $pid | save -f ($env.JREC_RUN | path join rec.pid)
    sleep 500ms # wf-recorder needs a moment before the first frame
}

export def "rec stop" [] {
    let pid = open ($env.JREC_RUN | path join rec.pid) | str trim
    ^kill -INT $pid # SIGINT: wf-recorder finalises the file
    while (^bash -c $"kill -0 ($pid) 2>/dev/null" | complete).exit_code == 0 {
        sleep 100ms
    }
}

# Save a screenshot as docs/public/media/<name>.webp.
export def still [name: string] {
    let png = $env.JREC_RUN | path join $"($name).png"
    ^grim -o HEADLESS-1 $png
    ^ffmpeg -loglevel error -y -i $png -c:v libwebp -quality 90 ($env.JREC_OUT | path join $"($name).webp")
}

# Write jotter's config.toml for this scene; call it before starting jotter.
export def config [settings: record] {
    let dir = $env.XDG_CONFIG_HOME | path join jotter
    mkdir $dir
    $settings | to toml | save -f ($dir | path join config.toml)
}

# kill -9 the jotter running in this scene's terminal (a simulated crash).
# The pid comes from kitty, so no other jotter on the machine is touched.
export def crash-jotter [] {
    let pid = rc ls | from json | get 0.tabs.0.windows.0.foreground_processes
        | where {|p| ($p.cmdline | first | path basename) == "jotter" }
        | get 0.pid
    ^kill -9 $pid
}

# Make the screen `px` logical pixels tall (default 600) for scenes whose
# notebook is short, so the clip isn't mostly empty terminal. Call it first,
# before starting jotter.
export def height [px: int] {
    let sock = glob ($env.JREC_RUN | path join "sway-ipc.*.sock") | first
    ^swaymsg -s $sock output HEADLESS-1 resolution $"1920x($px * 2)" | ignore
    sleep 300ms # kitty follows the new size
}

# Set the terminal's background and foreground (for light-theme stills).
export def colors [background: string, foreground: string] {
    rc set-colors --all $"background=($background)" $"foreground=($foreground)"
}

# Run a sway command on this scene's compositor, e.g. `sway fullscreen enable`.
export def sway [...args: string] {
    let sock = glob ($env.JREC_RUN | path join "sway-ipc.*.sock") | first
    ^swaymsg -s $sock ...$args | ignore
}
