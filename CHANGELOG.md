# Changelog

## [Unreleased]

### Changed
- Export: filling the gaps around the photo edges (`fill_holes`) now uses all CPU cores. The picture is identical pixel by pixel, a 13459x10541 px export took 227 s instead of 263 s.

### Added
- Export with little free memory: when not even the smallest strip fits, the photo cache gives up memory first and then the number of blend levels goes down (not below 3). The export dialog says so, the log names the reason. With enough memory nothing changes. `PW_DEMO_TIMEOUT=<seconds>` raises the time limit of the demo runs.
- The export plans with 1.4 times the strip memory it counted before (`StripConfig::memory_factor`): measured on two runs, the program used 1.40 and 1.38 times the planned strip memory (no fixed part), so the old estimate missed temporary copies. This holds on any computer; the budget itself still follows the free memory. The log line of an export now shows the program's memory at the start and its highest value during the export.

### Fixed
- Crop frame (step 4): dragging a white edge moved the picture instead of the edge about half of the time (the edge was looked for where the pointer was after the drag threshold, not where it was pressed).

## [0.1.1] - 2026-11-05

### Fixed
- Sporadic crash on Windows (about every fourth start of the export/frame step): wgpu panicked with "Failed to create staging buffer" when Vulkan, DirectX 12 and OpenGL were started side by side. Windows now starts DirectX 12 only; `WGPU_BACKEND` still overrides it. 24 of 24 test runs finished (before: 7 of 27 crashed).

### Added
- Crash report: a panic is written to `%APPDATA%\PanoWeave\crash.log` and the window program shows a message with the file location. The graphics adapter in use is noted in `gpu.log` next to it.

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
