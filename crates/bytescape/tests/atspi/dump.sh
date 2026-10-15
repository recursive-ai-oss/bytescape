#!/bin/bash
# Starts Bytescape under a virtual display and a private session bus with the
# AT-SPI bus launcher, then prints its accessibility tree.
#
# Usage: dump.sh <path to the bytescape binary> <folder to scan>
# Needs: Xvfb, dbus-daemon, at-spi2-core, python3-dbus, gdbus.
set -u
bin=$1
folder=$2
here=$(cd "$(dirname "$0")" && pwd)

inner() {
    unset AT_SPI_BUS_ADDRESS
    gdbus_call() { gdbus call --session --timeout 5 "$@"; }
    reply=$(gdbus_call --dest org.a11y.Bus --object-path /org/a11y/bus \
        --method org.a11y.Bus.GetAddress) || { echo "FAIL: no accessibility bus"; exit 1; }
    address=$(echo "$reply" | sed -e "s/^('//" -e "s/',)\$//")
    enable() {
        # What a screen reader does when it starts.
        gdbus_call --dest org.a11y.Bus --object-path /org/a11y/bus \
            --method org.freedesktop.DBus.Properties.Set org.a11y.Status IsEnabled "<true>" >/dev/null
    }
    # ORDER=before: the screen reader is already running when the program starts.
    [ "${ORDER:-after}" = before ] && enable
    "$bin" "$folder" 2>"$XDG_RUNTIME_DIR/bytescape.err" &
    app=$!
    if [ "${ORDER:-after}" = after ]; then sleep 3; enable; fi
    /usr/bin/python3 "$here/dump.py" "$address" 30
    status=$?
    kill "$app" 2>/dev/null
    wait "$app" 2>/dev/null
    echo "--- bytescape stderr"
    cat "$XDG_RUNTIME_DIR/bytescape.err"
    exit $status
}

# A private runtime directory, so nothing here can touch the real session's
# sockets, including its accessibility bus.
XDG_RUNTIME_DIR=$(mktemp -d)
chmod 700 "$XDG_RUNTIME_DIR"
trap 'rm -rf "$XDG_RUNTIME_DIR"' EXIT
export XDG_RUNTIME_DIR bin folder here ORDER
env -u WAYLAND_DISPLAY -u DBUS_SESSION_BUS_ADDRESS -u AT_SPI_BUS_ADDRESS xvfb-run -a -s "-screen 0 1280x800x24" \
    dbus-run-session -- bash -c "$(declare -f inner); inner"
