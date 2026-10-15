Bytescape, portable build
=========================

Run bytescape.exe. It needs no installation and writes nothing to the
registry.

Keep bytescape.exe and bytescape-helper.exe in the same folder. The helper is
the separate program that Windows asks you to approve when you choose a full
scan or a delete that needs administrator rights.

A note on where to unzip: the helper runs with administrator rights, so the
folder it sits in should be one that only administrators can write to (for
example a folder under C:\Program Files). In a folder your own account can
write to, any program running as you could replace the helper before you
approve the prompt. If you want full scans, the installer is the better choice:
it puts both programs under Program Files.

The same goes for other files in that folder. The helper is built to load
Windows' own libraries (DLLs) from the system folder only, so a DLL that lands
next to it is not picked up by the helper. That protects the helper's libraries,
not the helper file itself, and not the rest of the folder: a program running as
you can still replace bytescape-helper.exe. Do not unzip into a folder you share
with programs you do not trust, such as Downloads, if you plan to approve the
administrator prompt; put the folder somewhere only administrators can write.

Bytescape never connects to the internet.
https://github.com/recursive-ai-oss/bytescape
