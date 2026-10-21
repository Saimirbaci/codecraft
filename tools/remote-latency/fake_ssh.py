#!/usr/bin/env python3
"""Test-only SSH model: latency, rate limit and a contended multiplexed link.

No network, SSH configuration or real host is used. A shared channel's lock models
head-of-line queueing, not OpenSSH's TCP implementation. Environment is fixture-local.
"""
import fcntl
import os
import sys
import time
import tty

delay = float(os.environ.get("FAKE_RTT_MS", "60")) / 1000
rate = int(os.environ.get("FAKE_BYTES_PER_SEC", "65536"))
link = open(os.environ["FAKE_LINK"], "a+")
shared = "ControlPath=none" not in sys.argv
if "--busy" in sys.argv:
    fcntl.flock(link, fcntl.LOCK_EX)
    open(os.environ["FAKE_BUSY_READY"], "w").close()
    # 32 KiB behind a 64 KiB/s bandwidth cap.
    for _ in range(32):
        time.sleep(1024 / rate)
    sys.exit(0)
tty.setraw(0)  # no local line-discipline echo: measure the simulated host response
os.write(1, b"READY")
if "--burst" in sys.argv:
    frame = b"\x1b[H\x1b[2J" + (b"redrawing a TUI " * 256)
    for _ in range(128):
        os.write(1, frame)
        time.sleep(len(frame) / rate)
    os.write(1, b"BURST-DONE")
while True:
    data = os.read(0, 1)
    if not data:
        break
    if shared:
        fcntl.flock(link, fcntl.LOCK_EX)
    time.sleep(delay + len(data) / rate)
    os.write(1, data)
    if shared:
        fcntl.flock(link, fcntl.LOCK_UN)
