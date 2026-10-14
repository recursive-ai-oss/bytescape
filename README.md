# Bytescape

[![CI](https://github.com/recursive-ai-oss/bytescape/actions/workflows/ci.yml/badge.svg)](https://github.com/recursive-ai-oss/bytescape/actions/workflows/ci.yml)
[![License: MIT OR Apache-2.0](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue)](#license)

**A disk-space visualizer for Windows and Linux: every folder and file drawn as a box sized by the space it takes on disk, so you can see where your storage went and act on it.** Written in Rust, drawn by its own small renderer, and it never connects to the internet.

> **Status: not released yet.** Bytescape is under construction. There is nothing to download today, and the table below lists the channels we plan to ship through, not ones that exist.

<!-- A screenshot goes here once the interface exists. -->

## Install

No release exists yet, so none of these channels is live. The table will gain links as each one ships.

| Channel | Platforms | Status |
|---|---|---|
| GitHub Releases: installer | Windows x64, Windows ARM64 | planned |
| GitHub Releases: portable `.zip` (both programs) | Windows x64, Windows ARM64 | planned |
| GitHub Releases: `.deb` | Linux x64, Linux ARM64 | planned |
| GitHub Releases: AppImage | Linux x64, Linux ARM64 | planned |
| winget | Windows | planned |
| Chocolatey | Windows | planned |
| Microsoft Store | Windows | planned |
| Snap | Linux | planned |

To build it yourself today, see [docs/building.md](docs/building.md).

## Privacy

**Bytescape never connects to the internet.** There is no networking code in either program, no update check, no telemetry and no account. Two independent checks in the build reject the networking libraries that we know of. One of them also rejects library features (including the Windows networking APIs) and system calls, so adding any of these fails the build. They are a backstop for code review, not a proof: a networking library they do not know about would not be caught.

What stays on your machine:

- **Crash reports.** If Bytescape crashes, the report is written to a file on your computer. On the next start you can read it in full. File and folder paths are hidden by default, with a switch to show them. Nothing is sent anywhere. "Report on GitHub" only opens your browser with a pre-filled issue that you can edit or close. (Planned for the first release.)
- **Links.** Links, such as the one to RecursiveAI.org or to the releases page, open in your own browser. Bytescape itself makes no request.
- **Your files.** Bytescape reads file names, sizes and the file system's own records to draw the map. It does not read what is inside your documents, and it copies nothing off your disk.

## Why use it, and why not

**Why:**

- It shows structure. Folders are boxes with title bars and their contents nested inside, so you can see which folder holds what.
- It tells the truth about bytes. Sizes shown while a scan runs are marked as lower bounds. Space that cannot be read is drawn as its own block rather than hidden. Moving something to the Recycle Bin or Trash is reported as exactly that, because the space only returns once the bin is emptied.
- It is careful with deletion. Operating system folders are blocked, protected locations need a deliberate hold-to-confirm, and permanent deletes are confirmed.
- It never changes your system by itself. It reads, and it deletes only what you pick.

**Why not:**

- **It is a visualizer, not a cleanup advisor.** It describes what a folder is and which program manages it. It does not tell you what is worth removing, offers no cleanup lists and has no one-click cleaning.
- **No macOS.** Windows and Linux only.
- **No rename or move.** It shows and it deletes. File management stays in your file manager.
- **No themes.** It follows your system light or dark setting.
- **No cloud features.** No sync, no remote scans, no updates over the network.

## Inspired by SpaceMonger 1.4

Bytescape is inspired by SpaceMonger 1.4, the small Windows tool whose nested boxes made a full disk readable at a glance. Bytescape is a new program written from scratch. It is not affiliated with SpaceMonger or its authors.

## Trademark

The licenses below cover the source code. They do not grant any right to use the name "Bytescape" or its logo. You are welcome to fork the code under the license terms, but a modified version must not be distributed under the Bytescape name or logo in a way that suggests it is the original or endorsed by Recursive AI, LLC.

## Contributing and security

- Build, test and contribute: [CONTRIBUTING.md](CONTRIBUTING.md)
- Report a vulnerability privately: [SECURITY.md](SECURITY.md)
- Community standards: [CODE_OF_CONDUCT.md](CODE_OF_CONDUCT.md)
- What changed: [CHANGELOG.md](CHANGELOG.md)

## More from Recursive AI

Find more free software at [RecursiveAI.org](https://recursiveai.org).

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option.

Copyright (c) 2026 Recursive AI, LLC.

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in Bytescape by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or conditions.
