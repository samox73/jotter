//! A crash must hand the terminal back and keep the unsaved work: run the
//! binary in a pty, edit, `:panic`, then check what it left behind.

use std::{
    io::{Read, Write},
    os::{fd::FromRawFd, unix::process::CommandExt},
    process::{Command, Stdio},
    time::{Duration, Instant},
};

/// Read until `quiet` passes with nothing new (or `max` overall). Answers
/// the keyboard-protocol query so startup doesn't wait for its timeout.
fn read_until_quiet(pty: &mut std::fs::File, out: &mut Vec<u8>, quiet: Duration, max: Duration) {
    let start = Instant::now();
    let mut last = Instant::now();
    let mut buf = [0u8; 65536];
    loop {
        match pty.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                if buf[..n].windows(4).any(|w| w == b"\x1b[?u") {
                    let _ = pty.write_all(b"\x1b[?0u\x1b[?62c");
                }
                out.extend_from_slice(&buf[..n]);
                last = Instant::now();
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                if last.elapsed() > quiet || start.elapsed() > max {
                    break;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(_) => break, // EIO: the child is gone
        }
    }
}

#[test]
fn a_panic_restores_the_terminal_and_autosaves() {
    let dir = std::env::temp_dir().join(format!("jotter-crash-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("state")).unwrap();
    std::fs::create_dir_all(dir.join("config")).unwrap();
    let nb = dir.join("nb.ipynb");
    std::fs::write(
        &nb,
        r#"{"cells":[{"cell_type":"code","execution_count":null,"id":"a1","metadata":{},"outputs":[],"source":["x = 1"]}],"metadata":{},"nbformat":4,"nbformat_minor":5}"#,
    )
    .unwrap();

    // a pty, with the child as its session leader so raw mode works
    let (mut master, mut slave) = (0, 0);
    let ws = libc::winsize { ws_row: 30, ws_col: 100, ws_xpixel: 0, ws_ypixel: 0 };
    // SAFETY: openpty fills two fds; the size goes in by ioctl afterwards
    // (openpty's pointer parameters differ in mutability between Linux and
    // macOS; `as _` on the ioctl requests covers their differing int types)
    let rc = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    assert_eq!(rc, 0, "openpty");
    // SAFETY: a valid fd and a valid winsize
    unsafe { libc::ioctl(master, libc::TIOCSWINSZ as _, &ws) };
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_jotter"));
    cmd.arg(&nb)
        .arg("--no-images")
        .env("XDG_STATE_HOME", dir.join("state"))
        .env("XDG_CONFIG_HOME", dir.join("config"))
        .env("TERM", "xterm-256color");
    // SAFETY: the slave fd is valid; each dup is owned by one Stdio
    unsafe {
        cmd.stdin(Stdio::from_raw_fd(libc::dup(slave)))
            .stdout(Stdio::from_raw_fd(libc::dup(slave)))
            .stderr(Stdio::from_raw_fd(libc::dup(slave)));
        cmd.pre_exec(move || {
            libc::setsid();
            libc::ioctl(slave, libc::TIOCSCTTY as _, 0);
            Ok(())
        });
    }
    let mut child = cmd.spawn().unwrap();
    // SAFETY: openpty gave us the master fd and nothing else owns it
    let mut pty = unsafe { std::fs::File::from_raw_fd(master) };
    // SAFETY: closing our copy of the slave; the child holds its own
    unsafe { libc::close(slave) };
    // SAFETY: a valid fd; O_NONBLOCK lets the reads time out
    unsafe {
        let flags = libc::fcntl(master, libc::F_GETFL);
        libc::fcntl(master, libc::F_SETFL, flags | libc::O_NONBLOCK);
    }

    let mut out = Vec::new();
    read_until_quiet(&mut pty, &mut out, Duration::from_millis(800), Duration::from_secs(15));
    let screen = String::from_utf8_lossy(&out).to_string();
    assert!(screen.contains("nb.ipynb"), "jotter never drew:\n{screen}");
    assert!(screen.contains("VIEW"), "status line missing:\n{screen}"); // highlighted source is split by colours

    // an edit (new cell) makes the notebook dirty, then the crash
    pty.write_all(b"a").unwrap();
    read_until_quiet(&mut pty, &mut out, Duration::from_millis(300), Duration::from_secs(5));
    pty.write_all(b":panic\r").unwrap();
    read_until_quiet(&mut pty, &mut out, Duration::from_millis(500), Duration::from_secs(10));
    let status = child.wait().unwrap();
    assert!(!status.success(), "the crash must not look like a clean exit");

    let all = String::from_utf8_lossy(&out);
    let tail = &all[all.rfind("\x1b[?1049h").unwrap_or(0)..]; // the session: after entering the alt screen
    for (seq, what) in [
        ("\x1b[?1049l", "leave the alternate screen"),
        ("\x1b[?1000l", "mouse reporting off"),
        ("\x1b[?2004l", "bracketed paste off"),
        ("\x1b[<1u", "keyboard protocol popped"),
    ] {
        assert!(tail.contains(seq), "missing after the panic: {what}\n{tail:?}");
    }
    let autosaves: Vec<_> = std::fs::read_dir(dir.join("state/jotter/autosave"))
        .map(|d| d.flatten().collect())
        .unwrap_or_default();
    assert_eq!(autosaves.len(), 1, "one recovery copy after the crash");
    let saved = std::fs::read_to_string(autosaves[0].path()).unwrap();
    assert_eq!(saved.matches("\"cell_type\"").count(), 2, "the new cell is in it");
    let _ = std::fs::remove_dir_all(&dir);
}
