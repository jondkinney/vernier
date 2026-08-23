# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.5.0](https://github.com/jondkinney/vernier/compare/v0.4.5...v0.5.0) - 2026-08-23

### Added

- *(figma)* add the official zoom bridge plugin
- *(app)* add the shell companion control protocol
- *(platform)* support exact synchronous Wayland captures

### Fixed

- *(linux)* recognize Chromium Figma window titles
- *(figma)* keep the focus gate blind to Vernier itself
- *(figma)* latch the correction per measurement session
- *(figma)* clear the menu bar with the corner indicator
- *(figma)* make accepted bridge sockets blocking
- *(wayland)* derive scale correctly for transformed outputs

### Other

- *(app)* alias the figma correction tuple
- *(linux)* consume the newest queued PipeWire frame
- *(figma)* point install surfaces at the live listing

## [0.4.5](https://github.com/jondkinney/vernier/compare/v0.4.4...v0.4.5) - 2026-08-21

### Fixed

- *(measure)* prioritize guide-snapped bounds
- *(ui)* show live shortcuts in context menu
- *(core)* preserve full text envelope when hugging

### Other

- *(linux)* smooth pointer and guide rendering

## [0.4.4](https://github.com/jondkinney/vernier/compare/v0.4.3...v0.4.4) - 2026-08-21

### Fixed

- *(linux)* register toggle hotkey via hyprctl eval on Hyprland Lua configs
- *(ui)* type stroke widths for current Rust

## [0.4.3](https://github.com/jondkinney/vernier/compare/v0.4.2...v0.4.3) - 2026-08-19

### Fixed

- *(linux)* use a oneshot pointer confinement for stuck-pill drags

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
