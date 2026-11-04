# Changelog

## [Unreleased]

## [0.1.0] - 2026-11-05

First public version.

### Added
- Guided workflow (photos, control points, align, frame and crop, save) with an exposure fusion mode for bracketed shots.
- Automatic control points with a sky/cloud filter, a manual point editor (grow points from a few clicks, patch-click mode that uses the rotation and zoom of nearby points), patch-matching refinement, pair check and a connection graph.
- Memory-independent export: the picture is made strip by strip and streamed to JPEG, PNG, TIFF or BigTIFF; the result does not depend on the available memory. Free disk space is checked first. Target width in pixels, 16-bit TIFF, and cancelling without leaving a partial file.
- Multiband blending with a seam preference for the sharpest photo.
- Smart seams (on by default, can be switched off): a graph-cut seam finder that steers the seams around close objects such as poles and lamps, and that lets a zoomed-in photo win only where there is detail to gain (no coloured patches in the sky).
- Masks: positive/negative masks per photo (brush, lasso, eraser, undo/redo, "Show seams" view, right-click menu on photos) that steer the seams, saved in the project file.
- Smooth brightness correction across each photo (gain maps): evens out sky gradients, vignetting and automatic exposure between photos; option in the export dialog.
- Optional parallax correction near the seams, saved in the project file.
- RAW support as the optional Cargo feature `raw` (on by default; its crates are LGPL licensed). `THIRD_PARTY_LICENSES.md` lists all dependency licenses.
- Secondary windows (patch comparison, mask editor) are real OS windows.
- `--imgdiff` for comparing two pictures and experiment variables for the export and the seams (see the README).
- About 195 unit tests plus end-to-end tests with synthetic images; dual license `MIT OR Apache-2.0`; README screenshots.

### Changed
- `src/app.rs` split into `src/app/` modules (no behaviour change).
- The export always runs strip by strip; `PW_OLD_EXPORT=1` renders the whole picture in memory for comparing.
- Blending is wide ("Automatic") by default.

### Fixed
- With a parallax field, up to 25 visible rows at a photo's top or bottom edge could be cut off in the export.
