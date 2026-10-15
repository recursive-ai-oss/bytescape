#!/usr/bin/python3
"""Prints the accessibility tree of a running Bytescape window as a screen
reader sees it, over AT-SPI, and checks that the map is in it.

Usage: dump.py <accessibility bus address> [timeout seconds]

Needs the system python3 with dbus-python (python3-dbus).
"""
import sys
import time

import dbus

ACC = "org.a11y.atspi.Accessible"
PROPS = "org.freedesktop.DBus.Properties"


def node(bus, name, path):
    return bus.get_object(name, path)


def text(bus, name, path):
    obj = node(bus, name, path)
    label = str(obj.Get(ACC, "Name", dbus_interface=PROPS))
    role = int(obj.GetRole(dbus_interface=ACC))
    return role, label


def children(bus, name, path):
    obj = node(bus, name, path)
    return [(str(n), str(p)) for n, p in obj.GetChildren(dbus_interface=ACC)]


def walk(bus, name, path, depth, lines, limit=400):
    if len(lines) >= limit:
        return
    try:
        role, label = text(bus, name, path)
    except dbus.DBusException as error:
        lines.append("  " * depth + f"<{error.get_dbus_name()}>")
        return
    lines.append("  " * depth + f"[{role}] {label}")
    for child_name, child_path in children(bus, name, path):
        walk(bus, child_name, child_path, depth + 1, lines, limit)


def find_app(bus, wanted, seen):
    for name, path in children(bus, "org.a11y.atspi.Registry", "/org/a11y/atspi/accessible/root"):
        try:
            _, label = text(bus, name, path)
        except dbus.DBusException:
            continue
        seen.add(label)
        if wanted in label.lower():
            return name, path
    return None


def main():
    address = sys.argv[1]
    timeout = float(sys.argv[2]) if len(sys.argv) > 2 else 20.0
    bus = dbus.bus.BusConnection(address)
    deadline = time.time() + timeout
    app = None
    seen = set()
    while time.time() < deadline and app is None:
        app = find_app(bus, "bytescape", seen)
        if app is None:
            time.sleep(0.25)
    if app is None:
        print("FAIL: no application named bytescape on the accessibility bus; saw", sorted(seen))
        return 1
    lines = []
    walk(bus, app[0], app[1], 0, lines)
    print("\n".join(lines))
    joined = "\n".join(lines)
    missing = [w for w in ("Map of", "Back", "RecursiveAI.org") if w not in joined]
    if missing:
        print(f"FAIL: not in the tree: {missing}")
        return 1
    print(f"OK: {len(lines)} nodes")
    return 0


sys.exit(main())
