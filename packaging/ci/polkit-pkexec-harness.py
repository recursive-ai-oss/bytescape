#!/usr/bin/env python3
"""Runs pkexec for the elevated helper the way a person would, without a person.

Run it as root inside the container (packaging/ci/polkit-pkexec-test.sh does):

    polkit-pkexec-harness.py USER PASSWORD ACTION HELPER

A pseudo-terminal is made, and inside it pkexec is started as USER with
DISPLAY and XAUTHORITY set on purpose, as in a desktop session. If pkexec
asks for a password (or which administrator to use) the script answers; a
container has no login manager, so polkit cannot complete such a prompt there
(it says "No session for cookie"), and the run then ends with pkexec's own
failure code. The script prints what pkexec showed on the terminal and exits
with pkexec's exit status, which for a program that ran is that program's.

pkexec's own codes are 126 (not authorized, or the prompt was dismissed) and
127 (it could not run the program).
"""

import os
import pty
import pwd
import re
import select
import sys
import time

user, password, action, helper = sys.argv[1:5]
nonce = os.urandom(16).hex()  # 32 hex digits, as the helper expects
pw = pwd.getpwnam(user)

pid, fd = pty.fork()
if pid == 0:
    os.initgroups(user, pw.pw_gid)
    os.setgid(pw.pw_gid)
    os.setuid(pw.pw_uid)
    env = {
        "PATH": "/usr/bin:/bin",
        "HOME": pw.pw_dir,
        "LANG": "C",
        # What a desktop session has. pkexec must not pass these on.
        "DISPLAY": ":99",
        "XAUTHORITY": pw.pw_dir + "/.Xauthority",
        "XDG_SESSION_TYPE": "x11",
    }
    os.execve("/usr/bin/pkexec", ["pkexec", helper, action, "--protocol", "1", "--nonce", nonce], env)

transcript = b""
sent = 0
chose = False
status = None
deadline = time.time() + 90

while time.time() < deadline and status is None:
    readable, _, _ = select.select([fd], [], [], 0.5)
    if readable:
        try:
            chunk = os.read(fd, 4096)
        except OSError:
            chunk = b""
        transcript += chunk
        # With more than one administrator on the machine pkexec asks which
        # identity to use; pick the test user.
        if not chose and b"Choose identity" in transcript:
            match = re.search(rb"^\s*(\d+)\.\s+.*\b" + user.encode() + rb"\b\s*$", transcript, re.M)
            if match:
                os.write(fd, match.group(1) + b"\n")
                chose = True
        # One answer per prompt, at most three prompts (a wrong password would
        # otherwise be tried again and again).
        prompts = transcript.lower().count(b"password:")
        if prompts > sent and sent < 3 and (chose or b"Choose identity" not in transcript):
            os.write(fd, (password + "\n").encode())
            sent = prompts
    done, raw = os.waitpid(pid, os.WNOHANG)
    if done:
        status = os.waitstatus_to_exitcode(raw)

if status is None:
    print("pkexec did not finish in 90 seconds; terminal output:")
    print(transcript.decode(errors="replace"))
    sys.exit(124)

print(transcript.decode(errors="replace").rstrip())
print(f"pkexec-exit={status}")
sys.exit(status)
