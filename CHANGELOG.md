# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.4.3](https://github.com/jondkinney/vernier/compare/v0.4.2...v0.4.3) - 2026-05-31

### Other

- update Cargo.lock dependencies
- canonical modifier order CTRL/SHIFT/ALT/SUPER

## [0.4.2](https://github.com/jondkinney/vernier/compare/v0.4.1...v0.4.2) - 2026-05-31

### Other

- update Cargo.lock dependencies
- canonical modifier order CTRL/SHIFT/ALT/SUPER

## [0.4.1](https://github.com/jondkinney/vernier/compare/v0.4.0...v0.4.1) - 2026-05-30

### Other

- canonical modifier order CTRL/SHIFT/ALT/SUPER

## [0.4.0](https://github.com/jondkinney/vernier/compare/v0.3.0...v0.4.0) - 2026-05-26

### Added

- *(prefs)* record chords through the daemon + bundle Adwaita Sans
- *(core)* soft-edge localization with edge-bias control
- *(linux)* render the overlay at the true fractional display scale

### Fixed

- pixel-perfect measurement — exact physical-pixel pipeline
- scale HUD strokes to the display + track monitor changes live

### Other

- fix cargo fmt + macOS clippy on the chord-recording PR

## [0.3.0](https://github.com/jondkinney/vernier/compare/v0.2.6...v0.3.0) - 2026-05-21

### Added

- *(linux)* paint the frozen screenshot as the overlay background

### Fixed

- *(clippy)* drop needless return in macOS handoff cfg block

### Other

- group args into context structs (clippy::too_many_arguments)
- get clippy and fmt clean on workspace (CI was failing)
- clear CI-gate lint debt — fmt matrix, ChipSeg cfg, Cmd boxing
- *(macos)* modernize objc2 usage, fix CGImage api, pin toolchain ([#15](https://github.com/jondkinney/vernier/pull/15))

## [0.2.6](https://github.com/jondkinney/vernier/compare/v0.2.5...v0.2.6) - 2026-05-20

### Added

- one-shot first-launch desktop install
