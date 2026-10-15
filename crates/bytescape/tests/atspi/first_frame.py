#!/usr/bin/python3
"""Times the first presented frame of Bytescape with and without the
screen-reader adapter, from just before the process is spawned.

Usage: first_frame.py <bytescape binary> <runs> <results.json>

Run it through first_frame.sh, which provides the display and the buses.
The three series are interleaved so that drift in the machine's load hits
them equally:
  none     the adapter is left out (`--bench-no-access`)
  adapter  the adapter is made; no screen reader is running
  reader   the adapter is made and a screen reader has switched
           accessibility on
"""
import json
import os
import platform
import subprocess
import sys
import time

bin_path, runs, out_path = sys.argv[1], int(sys.argv[2]), sys.argv[3]
WARMUPS = 5


def set_enabled(value):
    subprocess.run(
        ["gdbus", "call", "--session", "--timeout", "5", "--dest", "org.a11y.Bus",
         "--object-path", "/org/a11y/bus", "--method", "org.freedesktop.DBus.Properties.Set",
         "org.a11y.Status", "IsEnabled", "<%s>" % value],
        stdout=subprocess.DEVNULL, check=True)


def once(extra):
    start = time.monotonic()
    child = subprocess.Popen([bin_path, "--bench-first-frame", *extra],
                             stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
    presented = None
    for line in child.stdout:
        if line.strip() == "presented":
            presented = (time.monotonic() - start) * 1000.0
            break
    child.stdout.read()
    child.wait()
    err = child.stderr.read()
    if presented is None:
        raise SystemExit("no frame presented: " + err)
    return presented, err


series = {"none": [], "adapter": [], "reader": []}
refusals = 0
for i in range(WARMUPS + runs):
    for name in series:
        set_enabled("true" if name == "reader" else "false")
        ms, err = once(["--bench-no-access"] if name == "none" else [])
        if "screen-reader support is off" in err:
            refusals += 1
        if i >= WARMUPS:
            series[name].append(ms)


def dist(values):
    s = sorted(values)
    pick = lambda q: s[min(len(s) - 1, int(round(q * (len(s) - 1))))]
    return {"n": len(s), "min_ms": round(s[0], 3), "median_ms": round(pick(0.5), 3),
            "p95_ms": round(pick(0.95), 3), "max_ms": round(s[-1], 3)}


cpu = next((l.split(":", 1)[1].strip() for l in open("/proc/cpuinfo") if l.startswith("model name")), "?")
result = {
    "series": {k: dist(v) for k, v in series.items()},
    "adapter_refusals": refusals,
    "env": {
        "cpu": cpu,
        "cpus_allowed": next((l.split(":", 1)[1].strip() for l in open("/proc/self/status")
                              if l.startswith("Cpus_allowed_list")), "?"),
        "kernel": platform.release(),
        "load_average": open("/proc/loadavg").read().split()[:3],
        "display": os.environ.get("DISPLAY"),
        "warmups": WARMUPS,
    },
}
json.dump(result, open(out_path, "w"), indent=2)
print(json.dumps(result, indent=2))
