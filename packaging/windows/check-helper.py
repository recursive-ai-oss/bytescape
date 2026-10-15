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

canary       Shows that the DLL-search flag works the way `loadconfig` assumes.
             Observed on a hosted Windows runner: the helper's own libraries
             (kernel32, ntdll, bcryptprimitives and an api-ms-win set) did not
             pick up a stand-in planted beside it even with the flag zeroed,
             so the helper cannot show the flag working. A probe program
             (dependent-load-probe.rs) that imports from version.dll, linked
             with the same /DEPENDENTLOADFLAG:0x800, can. A stand-in
             version.dll (canary-dll.rs) writes a file when it is loaded. Two
             controls come first. One: a process that loads the stand-in by
             full path must write the file. Two: a copy of the probe with
             DependentLoadFlags set to zero, started beside the stand-in, must
             write it too, which shows a DLL beside a program is picked up
             when nothing restricts the search. If either control writes
             nothing the check cannot tell anything and exits 2, never "ok".
             Then the probe as linked is started beside the stand-in: the file
             must not appear (exit 1 if it does, whatever the exit status of
             the probe). The helper is tied to the probe only by the
             DependentLoadFlags value they must share, so the claim for the
             helper is: its flag is 0x800 (read from the file) and a program
             with that flag ignores a DLL beside it (shown on the probe).
             Needs rustc and a Windows SDK (the linker finds it).

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

# The same names as the helper's no-spawn test (crates/bytescape-helper/tests/
# no_spawn.rs) looks for in source, as far as they can be imports of a Windows
# program. test_check_helper.py fails when the two lists differ.
FORBIDDEN = re.compile(
    r"^(CreateProcess\w*|CreateProcessAsUser\w*|CreateProcessWithLogonW|CreateProcessWithTokenW"
    r"|ShellExecute\w*|WinExec|NtCreateUserProcess|RtlCreateUserProcess|CreateProcessInternal\w*"
    r"|NtCreateProcess|NtCreateProcessEx|system|_wsystem|_popen|_wpopen"
    r"|_spawn\w*|_wspawn\w*|_exec\w*|_wexec\w*)$"
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

    def dependent_load_flags_at(self):
        """File offset of DependentLoadFlags in the load configuration, or None.

        In IMAGE_LOAD_CONFIG_DIRECTORY the field is at byte 78 for a 64-bit
        file and byte 54 for a 32-bit one (EditList follows it).
        """
        config = self.directory(10)
        if not config:
            return None
        at = self.offset(config[0])
        size = struct.unpack_from("<I", self.data, at)[0]
        field = 78 if self.plus else 54
        if size < field + 2:
            return None
        return at + field

    def dependent_load_flags(self):
        """DependentLoadFlags from the load configuration, or None if absent."""
        at = self.dependent_load_flags_at()
        if at is None:
            return None
        return struct.unpack_from("<H", self.data, at)[0]

    def with_zeroed_load_flags(self):
        """A copy of the file with DependentLoadFlags set to zero, or None."""
        at = self.dependent_load_flags_at()
        if at is None:
            return None
        data = bytearray(self.data)
        data[at : at + 2] = b"\0\0"
        return bytes(data)


def check_imports(path: Path) -> bool:
    pe = Pe(path.read_bytes())
    found = pe.imports()
    bad = sorted({f"{dll}!{name}" for dll, name in found if FORBIDDEN.match(name)})
    dlls = sorted({dll.lower() for dll, _ in found})
    print(f"{path.name}: {len(found)} imports from {len(dlls)} libraries: {', '.join(dlls)}")
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


CANARY_SOURCE = Path(__file__).with_name("canary-dll.rs")
PROBE_SOURCE = Path(__file__).with_name("dependent-load-probe.rs")
CANARY_FILE = "canary-loaded.txt"


def run_beside_canary(exe: Path, folder: Path, name: str, marker_decides: bool = False):
    """Start `exe` (copied to `folder/name`, which holds the stand-in DLL) with
    no arguments. Exits with status 2 if it cannot be started or fails, unless
    `marker_decides` is set and the stand-in wrote its marker file: then the
    caller judges by the file."""
    copy = folder / name
    shutil.copy2(exe, copy)
    try:
        ran = subprocess.run([str(copy)], cwd=folder, stdin=subprocess.DEVNULL,
                             capture_output=True, timeout=60)
    except OSError as error:
        print(f"could not start {name}:", error)
        sys.exit(2)
    if marker_decides and (folder / CANARY_FILE).exists():
        return
    if ran.returncode != 0:
        print(f"{name} did not run cleanly: status {ran.returncode & 0xFFFFFFFF:#x}")
        sys.exit(2)
    print(f"{name} ran and exited with status 0")


def rustc(source: Path, out: Path, *args: str):
    built = subprocess.run(["rustc", "--edition", "2024", "-O", *args, "-o", str(out), str(source)],
                           capture_output=True, text=True)
    if built.returncode != 0 or not out.exists():
        print(f"could not build {source.name}:", built.stderr)
        sys.exit(2)


def check_canary(path: Path) -> bool:
    if os.name != "nt":
        print("canary: skipped (Windows only)")
        return True
    helper_flags = Pe(path.read_bytes()).dependent_load_flags()
    work = Path(tempfile.mkdtemp(prefix="bytescape-canary-"))
    try:
        dll = work / "version.dll"
        rustc(CANARY_SOURCE, dll, "--crate-type", "cdylib", "-C", "panic=abort")
        probe = work / "probe.exe"
        rustc(PROBE_SOURCE, probe, "-C", "link-arg=/DEPENDENTLOADFLAG:0x800")
        pe = Pe(probe.read_bytes())
        flags = pe.dependent_load_flags()
        print(f"probe: DependentLoadFlags = {flags if flags is None else hex(flags)}")
        if flags != 0x800:
            print("could not run the canary: the linker did not record the flag in the probe")
            sys.exit(2)
        if helper_flags != flags:
            print(f"FAILED: the helper's flags ({helper_flags}) are not the probe's ({flags:#x})")
            return False
        zeroed = pe.with_zeroed_load_flags()
        if zeroed is None:
            print("could not run the canary: nothing to zero in the probe")
            sys.exit(2)
        # Control one: a process that loads the stand-in by full path writes
        # the file.
        control = work / "control"
        control.mkdir()
        loaded = subprocess.run(
            [sys.executable, "-c", f"import ctypes; ctypes.WinDLL(r'{dll}')"],
            cwd=control, capture_output=True, text=True, timeout=60,
        )
        if not (control / CANARY_FILE).exists():
            print("could not run the canary: loading it by full path wrote nothing", loaded.stderr)
            sys.exit(2)
        print("control ok: the stand-in writes its file when it is loaded")
        # Control two: the probe with the restriction removed, beside the
        # stand-in, must load it. If it does not, a DLL beside a program is
        # not picked up here at all and the real run below would prove nothing.
        loose = work / "loose"
        loose.mkdir()
        shutil.copy2(dll, loose / dll.name)
        zeroed_exe = work / "zeroed.exe"
        zeroed_exe.write_bytes(zeroed)
        run_beside_canary(zeroed_exe, loose, "probe.exe")
        if not (loose / CANARY_FILE).exists():
            print("inconclusive: the probe with DependentLoadFlags zeroed did not load a DLL "
                  "planted beside it, so a clean result below would prove nothing")
            sys.exit(2)
        print("control ok: without the restriction a DLL planted beside the program is loaded")
        # The test: the probe as linked, beside the stand-in.
        tight = work / "tight"
        tight.mkdir()
        shutil.copy2(dll, tight / dll.name)
        run_beside_canary(probe, tight, "probe.exe", marker_decides=True)
        if (tight / CANARY_FILE).exists():
            print("FAILED: a program linked with /DEPENDENTLOADFLAG:0x800 loaded a DLL planted beside it")
            return False
        print("ok: with /DEPENDENTLOADFLAG:0x800 a version.dll beside the program was not loaded")
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
