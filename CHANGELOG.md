# Changelog

## [Unreleased]

### Added
- Initial public version: guided workflow, automatic control points, sky/cloud filter, point editor, patch-matching refinement, pair check, connection graph, multi-band export, exposure fusion.

- Export options (behind "More size and file options"): target width in pixels, 16-bit TIFF, and cancelling without leaving a partial file.
- About 90 new unit tests plus an end-to-end test with synthetic images.
- Dual license `MIT OR Apache-2.0`, README screenshots.

### Changed
- `src/app.rs` split into `src/app/` modules (no behaviour change).

### Removed
- Unused code (compiler warnings cleaned up).
