# Changelog

## [Unreleased]

### Added
- Initial public version: guided workflow, automatic control points, sky/cloud filter, point editor, patch-matching refinement, pair check, connection graph, multi-band export, exposure fusion.

- Export options (behind "More size and file options"): target width in pixels, 16-bit TIFF, and cancelling without leaving a partial file.
- About 90 new unit tests plus an end-to-end test with synthetic images.
- Dual license `MIT OR Apache-2.0`, README screenshots.

- Memory-independent export: the picture is made strip by strip and streamed to JPEG, PNG, TIFF or BigTIFF; the result does not depend on the available memory. Free disk space is checked first.
- Smart seams (optional): graph-cut seam finder that steers the seams around close objects such as poles and lamps.
- Masks: positive/negative masks per photo (brush, lasso, eraser, undo/redo, "Show seams" view) that steer the seams, saved in the project file.
- The parallax correction and its settings are saved in the project file.
- Patch-click mode uses the rotation and zoom of the existing points near the click and of the alignment.
- `--imgdiff` for comparing two pictures, and experiment variables for the export and the seams (see the README).

### Fixed
- With a parallax field, up to 25 visible rows at a photo's top or bottom edge could be cut off in the export.

### Changed
- `src/app.rs` split into `src/app/` modules, and `app/mod.rs` and `app/points.rs` split further (no behaviour change).
- The export always runs strip by strip; `PW_OLD_EXPORT=1` renders the whole picture in memory for comparing.

### Removed
- Unused code (compiler warnings cleaned up).
