#!/usr/bin/env python3
"""Checks on a finished Windows helper program (bytescape-helper.exe).

    check-helper.py imports  HELPER.exe     no way to start another program
    check-helper.py loadconfig HELPER.exe   restricted DLL search is linked in
    check-helper.py canary   HELPER.exe     a planted DLL is not loaded (Windows only)
    check-helper.py all      HELPER.exe     the three above (canary on Windows only)

imports      Reads the program's import tables (normal and delay-load) and
             fails if it imports any function that starts a process:
             CreateProcess*, ShellExecute*, WinExec and the like. The helper
             never launches other programs, and a program that does not import
             them cannot. This is the same list `dumpbin /imports` shows, read
             here without Visual Studio so it also runs on Linux.

loadconfig   Reads DependentLoadFlags from the load configuration and fails
             unless it is 0x800 (LOAD_LIBRARY_SEARCH_SYSTEM32): the helper then
             looks for its libraries in the system folder only, never beside
             itself.

canary       Builds a stand-in bcryptprimitives.dll whose DllMain writes a file
             (it also exports ProcessPrng, the function Rust's standard library
             asks that DLL for, so the loader accepts it). First, a control: a
             process that loads it by full path must write the file. Then the
             helper is copied next to the DLL in a temporary folder and started
             with no arguments; the file must not appear. Needs rustc, and an
             elevated shell, because the helper's manifest asks for
             administrator rights.

Exit status: 0 good, 1 a check failed, 2 could not run the check.
"""

import os
import re
import shutil
import struct
import subprocess
import sys
import tempfile
from pathlib import Path

FORBIDDEN = re.compile(
    r"^(CreateProcess\w*|CreateProcessAsUser\w*|CreateProcessWithLogonW|CreateProcessWithTokenW"
    r"|ShellExecute\w*|WinExec|NtCreateUserProcess|RtlCreateUserProcess|CreateProcessInternal\w*"
    r"|system|_wsystem|_spawn\w*|_wspawn\w*|_exec\w*|_wexec\w*)$"
)


class Pe:
    """Just enough of the PE format to list imports and read the load config."""

    def __init__(self, data: bytes):
        self.data = data
        if data[:2] != b"MZ":
            raise ValueError("not a PE file (no MZ header)")
        self.pe = struct.unpack_from("<I", data, 0x3C)[0]
        if data[self.pe : self.pe + 4] != b"PE\0\0":
            raise ValueError("not a PE file (no PE signature)")
        coff = self.pe + 4
        self.machine, self.nsections = struct.unpack_from("<HH", data, coff)
        size_optional = struct.unpack_from("<H", data, coff + 16)[0]
        self.opt = coff + 20
        magic = struct.unpack_from("<H", data, self.opt)[0]
        if magic == 0x20B:
            self.plus = True
            dirs = self.opt + 112
            count = struct.unpack_from("<I", data, self.opt + 108)[0]
        elif magic == 0x10B:
            self.plus = False
            dirs = self.opt + 96
            count = struct.unpack_from("<I", data, self.opt + 92)[0]
        else:
            raise ValueError(f"unknown optional header magic {magic:#x}")
        self.dirs = [struct.unpack_from("<II", data, dirs + 8 * i) for i in range(min(count, 16))]
        table = self.opt + size_optional
        self.sections = []
        for i in range(self.nsections):
            _, vsize, vaddr, rawsize, rawptr = struct.unpack_from("<8sIIII", data, table + 40 * i)
            self.sections.append((vaddr, max(vsize, rawsize), rawptr))

    def offset(self, rva: int) -> int:
        for vaddr, size, rawptr in self.sections:
            if vaddr <= rva < vaddr + size:
                return rawptr + (rva - vaddr)
        raise ValueError(f"address {rva:#x} is in no section")

    def cstring(self, rva: int) -> str:
        start = self.offset(rva)
        end = self.data.index(b"\0", start)
        return self.data[start:end].decode("ascii", "replace")

    def directory(self, index: int):
        if index < len(self.dirs) and self.dirs[index][0]:
            return self.dirs[index]
        return None

    def imports(self):
        """(dll, function) for every import by name, normal and delay-load."""
        found = []
        step = 8 if self.plus else 4
        high = 1 << (63 if self.plus else 31)
        fmt = "<Q" if self.plus else "<I"

        def thunks(rva, dll):
            at = self.offset(rva)
            while True:
                value = struct.unpack_from(fmt, self.data, at)[0]
                if value == 0:
                    return
                if not value & high:
                    found.append((dll, self.cstring((value & 0x7FFFFFFF) + 2)))
                at += step

        normal = self.directory(1)
        if normal:
            at = self.offset(normal[0])
            while True:
                original, _, _, name, first = struct.unpack_from("<IIIII", self.data, at)
                if not name:
                    break
                thunks(original or first, self.cstring(name))
                at += 20
        delay = self.directory(13)
        if delay:
            at = self.offset(delay[0])
            while True:
                attributes, name, _, _, names = struct.unpack_from("<IIIII", self.data, at)
                if not name:
                    break
                if attributes & 1:
                    thunks(names, self.cstring(name))
                at += 32
        return found

    def dependent_load_flags(self):
        """DependentLoadFlags from the load configuration, or None if absent."""
        config = self.directory(10)
        if not config:
            return None
        at = self.offset(config[0])
        size = struct.unpack_from("<I", self.data, at)[0]
        field = 78 if self.plus else 56
        if size < field + 2:
            return None
        return struct.unpack_from("<H", self.data, at + field)[0]


def check_imports(path: Path) -> bool:
    pe = Pe(path.read_bytes())
    found = pe.imports()
    bad = sorted({f"{dll}!{name}" for dll, name in found if FORBIDDEN.match(name)})
    dlls = sorted({dll.lower() for dll, _ in found})
    print(f"{path.name}: {len(found)} imports from {len(dlls)} libraries")
    if bad:
        print("FAILED: the helper imports functions that start programs:", ", ".join(bad))
        return False
    print("ok: no import starts a program")
    return True


def check_loadconfig(path: Path) -> bool:
    flags = Pe(path.read_bytes()).dependent_load_flags()
    print(f"{path.name}: DependentLoadFlags = {flags if flags is None else hex(flags)}")
    if flags != 0x800:
        print("FAILED: the helper is not linked with a system-folder-only DLL search (want 0x800)")
        return False
    print("ok: DLL search is restricted to the system folder")
    return True


CANARY_SOURCE = Path(__file__).with_name("bcryptprimitives-canary.rs")
CANARY_FILE = "canary-loaded.txt"


def check_canary(path: Path) -> bool:
    if os.name != "nt":
        print("canary: skipped (Windows only)")
        return True
    work = Path(tempfile.mkdtemp(prefix="bytescape-canary-"))
    try:
        dll = work / "bcryptprimitives.dll"
        built = subprocess.run(
            ["rustc", "--edition", "2024", "--crate-type", "cdylib", "-C", "panic=abort",
             "-O", "-o", str(dll), str(CANARY_SOURCE)],
            capture_output=True, text=True,
        )
        if built.returncode != 0 or not dll.exists():
            print("could not build the canary DLL:", built.stderr)
            sys.exit(2)
        # Control: a process that loads the DLL by full path writes the file.
        control = work / "control"
        control.mkdir()
        loaded = subprocess.run(
            [sys.executable, "-c", f"import ctypes; ctypes.WinDLL(r'{dll}')"],
            cwd=control, capture_output=True, text=True, timeout=60,
        )
        if not (control / CANARY_FILE).exists():
            print("could not run the canary: loading it by full path wrote nothing", loaded.stderr)
            sys.exit(2)
        print("control ok: the canary writes its file when it is loaded")
        # The test: the helper, started with no arguments, beside the DLL.
        helper = work / "bytescape-helper.exe"
        shutil.copy2(path, helper)
        try:
            ran = subprocess.run([str(helper)], cwd=work, stdin=subprocess.DEVNULL,
                                 capture_output=True, timeout=60)
        except OSError as error:
            print("could not start the helper (is this shell elevated?):", error)
            sys.exit(2)
        code = ran.returncode & 0xFFFFFFFF
        if code >= 0xC0000000:
            print(f"the helper did not start: loader status {code:#x}")
            sys.exit(2)
        print(f"the helper ran and exited with status {ran.returncode}")
        if (work / CANARY_FILE).exists():
            print("FAILED: the helper loaded a DLL planted beside it")
            return False
        print("ok: a bcryptprimitives.dll beside the helper was not loaded")
        return True
    finally:
        shutil.rmtree(work, ignore_errors=True)


def main() -> int:
    if len(sys.argv) != 3 or sys.argv[1] not in ("imports", "loadconfig", "canary", "all"):
        print(__doc__)
        return 2
    command, path = sys.argv[1], Path(sys.argv[2])
    steps = {"imports": check_imports, "loadconfig": check_loadconfig, "canary": check_canary}
    chosen = list(steps) if command == "all" else [command]
    ok = True
    for name in chosen:
        ok = steps[name](path) and ok
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
