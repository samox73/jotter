#!/usr/bin/env nu
# Record the docs site's feature clips into docs/public/media/.
#
# Each scene in scenes/ runs a real jotter in a real kitty on its own headless
# sway, with kitty's remote control typing keys, and wf-recorder capturing the
# screen. Scenes are independent, so several record at once. Needs the tools
# pinned in `nix develop .#docs`:
#
#   nix develop .#docs -c nu docs/media/record.nu            # every scene
#   nix develop .#docs -c nu docs/media/record.nu hero plots # just these
#   nix develop .#docs -c nu docs/media/record.nu -j 1       # one at a time

const media = path self .

# Start `cmd` in the background with its output in `log`; returns its pid.
def bg [cmd: string, log: string]: nothing -> int {
    ^bash -c $"($cmd) > '($log)' 2>&1 & echo $!" | into int
}

def wait-until [what: string, check: closure, --timeout: duration = 20sec] {
    let deadline = (date now) + $timeout
    while not (do $check) {
        if (date now) > $deadline {
            error make {msg: $"($what) didn't come up"}
        }
        sleep 100ms
    }
}

def stop [pid: int] {
    ^bash -c $"kill ($pid) 2>/dev/null; while kill -0 ($pid) 2>/dev/null; do sleep 0.1; done"
}

# Lossless capture -> VP9 webm, H.264 mp4 (Safari) and a webp poster.
def encode [raw: string, out: string, name: string] {
    let base = $out | path join $name
    ^ffmpeg -loglevel error -y -i $raw -c:v libvpx-vp9 -crf 38 -b:v 0 -row-mt 1 -pix_fmt yuv420p -an $"($base).webm"
    ^ffmpeg -loglevel error -y -i $raw -c:v libx264 -crf 26 -preset slow -pix_fmt yuv420p -movflags +faststart -an $"($base).mp4"
    ^ffmpeg -loglevel error -y -sseof -0.1 -i $raw -frames:v 1 -c:v libwebp -quality 90 $"($base).webp"
}

# Record one scene on its own sway. Returns null, or the scene's name if it
# failed (its logs are kept and their path printed).
def record-scene [name: string, repo: string, out: string, python_prefix: string]: nothing -> any {
    print $"── ($name)"
    # sway's socket path must fit in 108 bytes, so keep the runtime dir short
    let run = ($env.XDG_RUNTIME_DIR? | default "/tmp") | path join $"jrec-(random chars --length 6)"
    mkdir $run
    ^chmod 700 $run
    let sway = with-env {
        XDG_RUNTIME_DIR: $run
        WLR_BACKENDS: headless
        WLR_RENDERER: pixman
        WLR_LIBINPUT_NO_DEVICES: "1"
    } {
        hide-env -i WAYLAND_DISPLAY DISPLAY SWAYSOCK
        bg $"sway -c '($media)/sway.conf'" ($run | path join sway.log)
    }

    # a short, project-like path: it shows in the status line
    let work = $"/tmp/jotter-demo/($name)"
    rm -rf $work
    mkdir $work
    glob ($media | path join notebooks *) | each {|f| cp $f $work } | ignore
    # a project venv next to the notebooks, as the docs recommend: jotter
    # finds its kernel there (the pinned Python, linked in)
    let venv = $work | path join .venv
    mkdir ($venv | path join bin) ($venv | path join share)
    ^ln -s ($python_prefix | path join bin python) ($venv | path join bin python)
    cp -r ($python_prefix | path join share jupyter) ($venv | path join share)

    let sock = $"unix:($run)/kitty.sock"
    let session = {
        XDG_RUNTIME_DIR: $run
        WAYLAND_DISPLAY: wayland-1
        XDG_CONFIG_HOME: ($work | path join config)
        XDG_STATE_HOME: ($work | path join state)
        MPLCONFIGDIR: $media # its matplotlibrc: figure size and HiDPI
        EDITOR: nvim # XDG_CONFIG_HOME is empty, so a clean nvim
        PATH: ($env.PATH | prepend ($repo | path join target release))
        # kitty renders with software OpenGL; cap its threads so scenes
        # recording side by side don't starve each other
        LP_NUM_THREADS: "2"
        JREC_KITTY: $sock
        JREC_RUN: $run
        JREC_OUT: $out
        JREC_SCENE: $name
        JREC_WORK: $work
    }
    let result = try {
        wait-until "sway" { $run | path join wayland-1 | path exists }
        let kitty = with-env $session {
            hide-env -i DISPLAY
            bg $"kitty --config '($media)/kitty.conf' --listen-on ($sock) --directory '($work)' bash --rcfile '($media)/bashrc'" ($run | path join kitty.log)
        }
        let scene_result = try {
            wait-until "kitty" { (^kitty @ --to $sock ls | complete).exit_code == 0 }
            let scene = $media | path join scenes $"($name).nu"
            with-env $session { ^nu -c $"use '($media)/lib.nu' *; source '($scene)'" }
            let raw = $run | path join $"($name).mkv"
            if ($raw | path exists) {
                encode $raw $out $name
                let size = ls $"($out)/($name).webm" | get size | first
                print $"   ($name).webm ($size)"
            } else if (open $scene | str contains "rec start") {
                error make {msg: $"no recording; see ($raw).log"}
            }
            null
        } catch {|e|
            $e.msg
        }
        stop $kitty
        if $scene_result != null {
            error make {msg: $scene_result}
        }
        null
    } catch {|e|
        print -e $"   ($name) failed: ($e.msg)\n   logs in ($run)"
        $name
    }
    stop $sway
    rm -rf $work
    if $result == null {
        rm -rf $run
    }
    $result
}

def main [
    ...scenes: string
    --jobs (-j): int # scenes recorded at once (default: a third of the CPU cores)
] {
    let repo = $media | path dirname | path dirname
    let out = $repo | path join docs public media
    mkdir $out
    let names = if ($scenes | is-empty) {
        ls ($media | path join scenes) | get name | path parse | get stem | sort
    } else {
        $scenes
    }
    let jobs = $jobs | default ([1 ((sys cpu | length) // 3)] | math max)
    # nothing a scene starts may reach the real desktop: no X11 display, and
    # the only Wayland display is each scene's headless sway
    hide-env -i DISPLAY WAYLAND_DISPLAY WAYLAND_SOCKET SWAYSOCK

    ^cargo build --release --locked --manifest-path ($repo | path join Cargo.toml)
    # the Python environment with ipykernel (the one that ships `jupyter`)
    let python_prefix = which jupyter | get path.0 | path dirname | path dirname

    print $"recording ($names | length) scene\(s), ($jobs) at a time"
    let failed = $names | par-each --threads $jobs {|name|
        record-scene $name $repo $out $python_prefix
    } | compact
    # a scene that failed on a busy machine gets one more try, alone
    let failed = $failed | each {|name|
        print $"── retrying ($name)"
        record-scene $name $repo $out $python_prefix
    } | compact
    rm -rf /tmp/jotter-demo
    if not ($failed | is-empty) {
        error make {msg: $"scenes failed: ($failed | str join ', ')"}
    }
}
