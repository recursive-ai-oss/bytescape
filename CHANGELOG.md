# Changelog

All notable changes to Bytescape are recorded here. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project follows [Semantic Versioning](https://semver.org/spec/v2.0.0.html) once it has a release.

## [Unreleased]

### Added

- The project skeleton: a Cargo workspace with a crate for each part of the program (interface, elevated helper, size tree, scanners, layout, renderer, knowledge tables, platform services and the helper protocol), all still empty.
- Continuous integration on Linux and Windows: formatting, lints, tests, dependency licenses and advisories, and checks that no networking dependency, no off-list helper dependency, no banned wording and no private references enter the repository.
- The documentation set: README, contributing guide, security policy, code of conduct and licenses (`MIT OR Apache-2.0`).
