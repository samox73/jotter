"""Timing loops for scenes/bench-editors.nu, over kitty's remote-control socket.

`kitty @` starts a new process per call (~45 ms), which is coarser than what
the benchmark measures; this talks to the socket directly (well under a
millisecond per call). Every subcommand prints one JSON value (milliseconds).

  kitty_probe.py SOCK WIN shown PATTERN T0_NS      launch -> PATTERN on screen
  kitty_probe.py SOCK WIN settled T0_NS            T0 -> last change (1.5 s still)
  kitty_probe.py SOCK WIN per-key N KEY...         N samples: key -> first change
  kitty_probe.py SOCK WIN burst TEXT               send TEXT -> last change
"""
import json
import socket
import sys
import time

sock_path, win = sys.argv[1].removeprefix("unix:"), sys.argv[2]
VERSION = [0, 49, 0]


def rc(cmd, payload):
    msg = {"cmd": cmd, "version": VERSION, "no_response": False, "payload": payload}
    data = b"\x1bP@kitty-cmd" + json.dumps(msg).encode() + b"\x1b\\"
    with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as s:
        s.connect(sock_path)
        s.sendall(data)
        buf = b""
        while not buf.endswith(b"\x1b\\"):
            chunk = s.recv(65536)
            if not chunk:
                break
            buf += chunk
    reply = json.loads(buf[len(b"\x1bP@kitty-cmd") : -2])
    if not reply.get("ok"):
        sys.exit(f"kitty {cmd}: {reply.get('error')}")
    return reply.get("data")


def screen():
    return rc("get-text", {"match": f"id:{win}", "extent": "screen"})


def send(text):
    rc("send-text", {"match": f"id:{win}", "data": "text:" + text})


def ms(seconds):
    return round(seconds * 1000, 2)


def settled(t0, quiet=1.5):
    last, changed = screen(), time.perf_counter()
    while True:
        s, now = screen(), time.perf_counter()
        if s != last:
            last, changed = s, now
        elif now - changed > quiet:
            return changed - t0


def first_change(key, timeout=3.0):
    before = screen()
    t0 = time.perf_counter()
    send(key)
    while screen() == before:
        if time.perf_counter() - t0 > timeout:
            sys.exit(f"no screen change after {key!r}")
    return time.perf_counter() - t0


mode = sys.argv[3]
if mode == "shown":
    # T0 is the launch time from the scene (time.time_ns clock)
    pattern, t0_ns = sys.argv[4], int(sys.argv[5])
    while pattern not in screen():
        if time.time_ns() - t0_ns > 60e9:
            sys.exit(f"never showed {pattern!r}:\n{screen()}")
    print(json.dumps(ms((time.time_ns() - t0_ns) / 1e9)))
elif mode == "settled":
    t0_ns = int(sys.argv[4])
    # perf_counter-relative: convert the wall-clock launch time once
    offset = time.time_ns() / 1e9 - time.perf_counter()
    print(json.dumps(ms(settled(t0_ns / 1e9 - offset))))
elif mode == "per-key":
    n, keys = int(sys.argv[4]), sys.argv[5:]
    samples = []
    for i in range(n):
        time.sleep(0.12)  # let the previous key's redraws finish
        samples.append(ms(first_change(keys[i % len(keys)])))
    print(json.dumps(samples))
elif mode == "burst":
    t0 = time.perf_counter()
    send(sys.argv[4])
    print(json.dumps(ms(settled(t0))))
