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
