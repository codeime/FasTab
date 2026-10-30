#!/usr/bin/env python3
"""Compare the resident footprint of fterm builds.

    scripts/fterm-footprint-ab.py shipped=target/dist/fterm candidate=/tmp/fterm-candidate
    scripts/fterm-footprint-ab.py --rounds 5 --size 80x300 a=./fterm-a b=./fterm-b

Each build runs in a fresh pty with `/bin/sh` as its shell (no desktop app is
needed). After 3 s idle, and again after a burst of output plus some typing,
`footprint` is sampled. Numbers are phys_footprint in KiB, the figure Activity
Monitor shows. `heap-tagged` is the dirty memory under VM tag 100: mimalloc
uses that tag on macOS, which footprint/vmmap print as "IOAccelerator". It is
zero for the system allocator, which fterm uses.
"""

import argparse
import fcntl
import os
import pty
import re
import select
import signal
import statistics
import struct
import subprocess
import termios
import time

BURST = b"ls -la /usr/lib /usr/bin /System/Library/Frameworks | head -3000\r"
TYPED = b"git checkout ma"


def footprint(pid):
    out = subprocess.run(["footprint", str(pid)], capture_output=True, text=True).stdout
    total = re.search(r"Footprint: (\d+) KB", out)
    tagged = re.search(r"^\s*(\d+) KB.*IOAccelerator\s*$", out, re.M)
    return (int(total.group(1)) if total else -1, int(tagged.group(1)) if tagged else 0)


def pump(fd, seconds):
    end = time.time() + seconds
    while time.time() < end:
        ready, _, _ = select.select([fd], [], [], 0.05)
        if ready:
            try:
                if not os.read(fd, 65536):
                    return
            except OSError:
                return


def run_once(binary, rows, cols):
    env = dict(os.environ, Q_SHELL="/bin/sh")
    pid, fd = pty.fork()
    if pid == 0:
        fcntl.ioctl(0, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))
        os.execve(binary, ["fterm"], env)
    try:
        pump(fd, 3)
        idle = footprint(pid)
        os.write(fd, BURST)
        pump(fd, 1)
        for byte in TYPED:
            os.write(fd, bytes([byte]))
            pump(fd, 0.03)
        os.write(fd, b"\x03")
        pump(fd, 3)
        busy = footprint(pid)
        os.write(fd, b"exit\r")
        pump(fd, 1.5)
    finally:
        try:
            os.kill(pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        try:
            os.waitpid(pid, 0)
        except ChildProcessError:
            pass
        os.close(fd)
    return idle, busy


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("builds", nargs="+", metavar="NAME=PATH")
    parser.add_argument("--rounds", type=int, default=3)
    parser.add_argument("--size", default="40x120", help="ROWSxCOLS of the pty")
    args = parser.parse_args()
    rows, cols = (int(part) for part in args.size.split("x"))

    builds = []
    for spec in args.builds:
        name, sep, path = spec.partition("=")
        if not sep or not os.access(path, os.X_OK):
            parser.error(f"not NAME=EXECUTABLE: {spec}")
        builds.append((name, os.path.abspath(path)))

    print(f"{'build':12} {'idle total':>11} {'idle heap-tagged':>17} {'busy total':>11} {'busy heap-tagged':>17}  (KiB, median of {args.rounds}, {args.size})")
    for name, path in builds:
        samples = [run_once(path, rows, cols) for _ in range(args.rounds)]
        median = lambda pick: int(statistics.median(pick(sample) for sample in samples))
        print(
            f"{name:12} {median(lambda s: s[0][0]):11} {median(lambda s: s[0][1]):17}"
            f" {median(lambda s: s[1][0]):11} {median(lambda s: s[1][1]):17}"
        )


if __name__ == "__main__":
    main()
