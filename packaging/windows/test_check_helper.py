#!/usr/bin/env python3
"""Tests for check-helper.py: run them with `python3 packaging/windows/test_check_helper.py`.

They build small PE files by hand, so they run on Linux and Windows alike and
need no compiler.
"""

import importlib.util
import re
import struct
import sys
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
ROOT = HERE.parent.parent

spec = importlib.util.spec_from_file_location("check_helper", HERE / "check-helper.py")
check_helper = importlib.util.module_from_spec(spec)
spec.loader.exec_module(check_helper)


def make_pe(plus: bool, flags: int, with_config: bool = True) -> bytes:
    """A minimal PE file with one section that holds a load configuration
    directory whose DependentLoadFlags is `flags`."""
    section_rva, section_raw, section_size = 0x1000, 0x200, 0x200
    dos = bytearray(0x40)
    dos[0:2] = b"MZ"
    struct.pack_into("<I", dos, 0x3C, 0x40)
    size_optional = 240 if plus else 224
    coff = struct.pack("<HHIIIHH", 0x8664 if plus else 0x14C, 1, 0, 0, 0, size_optional, 0x22)
    optional = bytearray(size_optional)
    struct.pack_into("<H", optional, 0, 0x20B if plus else 0x10B)
    count_at, dirs_at = (108, 112) if plus else (92, 96)
    struct.pack_into("<I", optional, count_at, 16)
    if with_config:
        struct.pack_into("<II", optional, dirs_at + 8 * 10, section_rva, 0x100)
    section = struct.pack("<8sIIIIIIHHI", b".rdata", section_size, section_rva, section_size,
                          section_raw, 0, 0, 0, 0, 0x40000040)
    head = bytes(dos) + b"PE\0\0" + coff + bytes(optional) + section
    head += bytes(section_raw - len(head))
    body = bytearray(section_size)
    size = 0xA0 if plus else 0x5C
    struct.pack_into("<I", body, 0, size)
    # DependentLoadFlags, then a marker in EditList, which a wrong offset reads.
    flags_at = 78 if plus else 54
    struct.pack_into("<H", body, flags_at, flags)
    struct.pack_into("<H", body, flags_at + 2, 0x5555)
    return head + bytes(body)


class LoadFlags(unittest.TestCase):
    def test_reads_the_field_in_both_widths(self):
        for plus in (True, False):
            for flags in (0x800, 0, 0x1234):
                pe = check_helper.Pe(make_pe(plus, flags))
                self.assertEqual(pe.dependent_load_flags(), flags, f"plus={plus}")

    def test_the_32_bit_field_is_at_54_and_not_the_edit_list_at_56(self):
        data = make_pe(False, 0x800)
        pe = check_helper.Pe(data)
        at = pe.dependent_load_flags_at()
        self.assertEqual(at, pe.offset(0x1000) + 54)
        self.assertNotEqual(pe.dependent_load_flags(), 0x5555)

    def test_zeroing_changes_only_the_two_bytes(self):
        for plus in (True, False):
            data = make_pe(plus, 0x800)
            zeroed = check_helper.Pe(data).with_zeroed_load_flags()
            self.assertEqual(check_helper.Pe(zeroed).dependent_load_flags(), 0)
            changed = [i for i, (a, b) in enumerate(zip(data, zeroed)) if a != b]
            self.assertEqual(len(changed), 1)  # 0x0800 to 0x0000 changes one byte
            self.assertEqual(len(data), len(zeroed))

    def test_a_file_without_the_field_gives_none(self):
        pe = check_helper.Pe(make_pe(True, 0x800, with_config=False))
        self.assertIsNone(pe.dependent_load_flags())
        self.assertIsNone(pe.with_zeroed_load_flags())

    def test_loadconfig_check_passes_only_for_0x800(self):
        import tempfile

        for flags, expected in ((0x800, True), (0, False), (0x1000, False)):
            with tempfile.TemporaryDirectory() as folder:
                exe = Path(folder) / "h.exe"
                exe.write_bytes(make_pe(True, flags))
                self.assertEqual(check_helper.check_loadconfig(exe), expected, hex(flags))


def no_spawn_lists():
    """The three name lists of the helper's no-spawn test, read from source."""
    text = (ROOT / "crates/bytescape-helper/tests/no_spawn.rs").read_text()

    def names(const):
        block = re.search(rf"const {const}: &\[&str\] = &\[(.*?)\];", text, re.S)
        assert block, f"{const} not found in no_spawn.rs"
        return re.findall(r'"([^"]+)"', block.group(1))

    return names("FORBIDDEN"), names("FORBIDDEN_IN_LIBC"), names("FORBIDDEN_LIBC_PREFIXES")


# Names in the no-spawn test that cannot be imports of a Windows program: Rust
# and Unix names. Anything else in that test's lists must be in the import
# check too, so adding a Windows name there without adding it here fails.
NOT_WINDOWS_IMPORTS = {
    "Command", "CommandExt", "execv", "execve", "execvp", "execvpe", "execl", "execle",
    "execlp", "fexecve", "posix_spawn", "posix_spawnp", "SYS_execve", "SYS_execveat",
    "SYS_fork", "SYS_vfork", "SYS_clone", "SYS_clone3", "popen", "fork", "vfork", "clone",
    "clone3", "exec", "posix_spawn",
}


class ImportList(unittest.TestCase):
    def test_every_windows_name_the_source_test_forbids_is_forbidden_here(self):
        forbidden, in_libc, prefixes = no_spawn_lists()
        for name in sorted(set(forbidden + in_libc + prefixes) - NOT_WINDOWS_IMPORTS):
            # A prefix stands for its whole family.
            sample = name + "x" if name in prefixes else name
            self.assertTrue(check_helper.FORBIDDEN.match(sample), f"{sample} is not caught")

    def test_the_import_check_has_nothing_the_source_test_lacks(self):
        forbidden, in_libc, prefixes = no_spawn_lists()
        known = set(forbidden + in_libc + prefixes)
        alternatives = check_helper.FORBIDDEN.pattern.strip("^$()").split("|")
        for alternative in alternatives:
            stem = alternative.removesuffix(r"\w*")
            if alternative.endswith(r"\w*"):
                self.assertTrue(any(k.startswith(stem) for k in known), f"{alternative} is extra")
            else:
                self.assertIn(stem, known, f"{alternative} is extra")

    def test_ordinary_imports_pass(self):
        for name in ("CreateFileW", "CloseHandle", "ProcessPrng", "NtCreateFile", "systemx"):
            self.assertFalse(check_helper.FORBIDDEN.match(name), name)

    def test_the_names_the_review_asked_for_are_caught(self):
        for name in ("_popen", "_wpopen", "NtCreateProcess", "NtCreateProcessEx"):
            self.assertTrue(check_helper.FORBIDDEN.match(name), name)


if __name__ == "__main__":
    sys.exit(unittest.main())
