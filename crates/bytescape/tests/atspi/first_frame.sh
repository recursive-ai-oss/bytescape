#!/bin/bash
# Times the first frame with and without the screen-reader adapter, under a
# virtual display and a private session bus with the AT-SPI bus launcher.
# Never touches the desktop session.
#
# Usage: first_frame.sh <bytescape binary> <runs> <results.json> [taskset cpu list]
# Needs: Xvfb, dbus-daemon, at-spi2-core, python3-dbus, gdbus.
set -u
here=$(cd "$(dirname "$0")" && pwd)
XDG_RUNTIME_DIR=$(mktemp -d)
chmod 700 "$XDG_RUNTIME_DIR"
trap 'rm -rf "$XDG_RUNTIME_DIR"' EXIT
export XDG_RUNTIME_DIR
pin=()
[ -n "${4:-}" ] && pin=(taskset -c "$4")
inner() {
    # Start the accessibility bus once, as a desktop session would have.
    gdbus call --session --timeout 10 --dest org.a11y.Bus --object-path /org/a11y/bus \
        --method org.a11y.Bus.GetAddress >/dev/null || exit 1
    exec "${pin[@]}" /usr/bin/python3 "$here/first_frame.py" "$1" "$2" "$3"
}
export here
export -f inner
env -u WAYLAND_DISPLAY -u DBUS_SESSION_BUS_ADDRESS -u AT_SPI_BUS_ADDRESS \
    xvfb-run -a -s "-screen 0 3840x2160x24 -noreset" \
    dbus-run-session -- bash -c 'pin=('"${pin[*]:-}"'); '"$(declare -f inner)"'; inner "$@"' _ "$1" "$2" "$3"
