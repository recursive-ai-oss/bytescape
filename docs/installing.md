# Installing Bytescape

Bytescape has not had a release yet, so nothing here is available to download today. This page describes what each release will contain and how to install it, so you know what to expect.

Every release file is named `bytescape-<version>-<os>-<arch>.<ext>`, with `x64` or `arm64` for the architecture. Each release also has a `SHA256SUMS` file. To check a download, run `sha256sum -c SHA256SUMS --ignore-missing` on Linux, or compare `Get-FileHash <file>` with the matching line on Windows.

## Windows

| File | What it is |
|---|---|
| `bytescape-<version>-windows-<arch>-setup.exe` | The installer. Installs for all users into `C:\Program Files\Bytescape`, adds a Start menu entry and "Analyze with Bytescape" to the right-click menu of folders and drives, and removes all of it again when you uninstall. |
| `bytescape-<version>-windows-<arch>-portable.zip` | Both programs, no installer, nothing written to the registry. |
| `bytescape-<version>-windows-<arch>-symbols.zip` | Debug symbols, for reading a crash report's backtrace. Not needed to run Bytescape. |

**Silent install** (for scripts and package managers): run the installer with `/VERYSILENT /SUPPRESSMSGBOXES /NORESTART`. It needs administrator rights, because it installs into Program Files.

**Why the installer goes into Program Files.** Bytescape has two programs. The interface runs as you. The helper, `bytescape-helper.exe`, runs with administrator rights, and only when you ask for a full scan or a delete that needs them. Windows only lets administrators write to Program Files, so another program running as you cannot swap the helper for a different one.

**The portable zip.** Keep both `.exe` files in one folder. If you unzip it into a folder your own account can write to, the helper sits where any program running as you could replace it before you approve the administrator prompt. For full scans, use the installer, or unzip into a folder that only administrators can write to. The helper loads Windows' own libraries from the system folder only, so a DLL placed beside it is not picked up, but that does not protect the helper file itself: a program running as you can still replace it in a folder you can write to.

**Windows 11 and the right-click entry.** Windows 11 shows classic entries under "Show more options".

**Signing.** Release files are signed when the project's signing account is set up. An unsigned file makes Windows SmartScreen ask you to confirm. The release notes say which state a release is in.

Windows ARM64 files are built on an x64 machine. They are marked as such in the release notes until they have been run on ARM64 Windows.

## Linux

| File | What it is |
|---|---|
| `bytescape-<version>-linux-<arch>.deb` | For Debian, Ubuntu and relatives. Built on Ubuntu 22.04, so it needs that release's C library or newer. |
| `bytescape-<version>-linux-<arch>.AppImage` | One file, no installation: `chmod +x` it and run it. |
| `bytescape-<version>-linux-<arch>-strict.snap` | The snap from the store, in its confined edition. |

**The `.deb`.** `sudo apt install ./bytescape-<version>-linux-x64.deb`. It installs `/usr/bin/bytescape`, the helper at `/usr/libexec/bytescape/bytescape-helper`, a polkit policy, a menu entry, icons, and "Analyze with Bytescape" for Dolphin. For Nautilus, install `python3-nautilus` as well (the package suggests it). Full scans and deletes that need administrator rights ask for your password through `pkexec`: scans may keep the authorization for your session, deletes ask every time.

**The AppImage.** It contains the interface only. An AppImage is mounted for your account alone, so a program started with administrator rights could not read it. If the `.deb` is installed too, the interface finds its helper at the path above.

**The snap.** The strict edition reads your home folder, removable media and the mount table, and has no administrator access, so it has no "Full scan" button. A classic edition, with whole-disk reach and the helper, is planned once the snap store approves classic confinement.

Linux ARM64 files are built on an ARM64 machine when the project has one for the release. The release notes say so.

## Package managers

These channels are planned: `winget install RecursiveAI.Bytescape`, `choco install bytescape`, the Microsoft Store, and the snap store. They install the same installer and files described above.

## Uninstalling

- Windows: Settings, Apps, Installed apps, Bytescape, Uninstall. Crash reports you may have in your user profile are left in place.
- Debian: `sudo apt remove bytescape`.
- AppImage and portable zip: delete the file or folder.
- Snap: `sudo snap remove bytescape`.

## Building it yourself

See [building.md](building.md).
