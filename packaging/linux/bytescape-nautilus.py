"""Adds "Analyze with Bytescape" to the Nautilus menu for folders.

This is a nautilus-python extension (package python3-nautilus). It only runs
`bytescape PATH` for a local folder. Remote locations are left out.
"""
import subprocess

from gi.repository import GObject, Nautilus


class AnalyzeWithBytescape(GObject.GObject, Nautilus.MenuProvider):
    def _item(self, name, folder):
        path = folder.get_location().get_path()
        if path is None or not folder.is_directory():
            return []
        item = Nautilus.MenuItem(
            name=name,
            label="Analyze with Bytescape",
            tip="Show where the space in this folder is used",
        )
        item.connect("activate", lambda _item: subprocess.Popen(["bytescape", path]))
        return [item]

    # Nautilus 4 passes (files); Nautilus 3 passes (window, files).
    def get_file_items(self, *args):
        files = args[-1]
        if len(files) != 1:
            return []
        return self._item("Bytescape::AnalyzeFolder", files[0])

    def get_background_items(self, *args):
        return self._item("Bytescape::AnalyzeBackground", args[-1])
