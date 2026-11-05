# Changelog

## [Unreleased]

### Changed
- Export: the blend pyramid (building the bands, putting the picture together) uses all CPU cores in the steps that still ran on one. Identical picture. Faster on several cores (single runs varied by about 15 % on the same machine, so no exact figure). `PW_TIMING=1` prints where the time goes (loading, writing, the steps of a strip).
- Export: filling the gaps around the photo edges (`fill_holes`) now uses all CPU cores. The picture is identical pixel by pixel. Faster on several cores (no reliable figure: single runs vary by about 15 %).

### Added
- Control point list: points that lie on top of each other are found and marked (orange rows, hover says which point). Two points of a photo pair on the same spot in both photos are duplicates; the button "Remove N duplicates" removes the worse one of each (hand-placed points stay, nothing is removed without the click). Points that agree in only one photo are marked as contradicting, not removed. Switch and tolerance (default 2 px of the working image) are in the list window, "only those" filters the list.
- Export with little free memory: when not even the smallest strip fits, the photo cache gives up memory first and then the number of blend levels goes down (not below 3). The export dialog says so, the log names the reason. With enough memory nothing changes. `PW_DEMO_TIMEOUT=<seconds>` raises the time limit of the demo runs.
- The export plans with 1.4 times the strip memory it counted before (`StripConfig::memory_factor`): measured on two runs, the program used 1.40 and 1.38 times the planned strip memory (no fixed part), so the old estimate missed temporary copies. This holds on any computer; the budget itself still follows the free memory. The log line of an export now shows the program's memory at the start and its highest value during the export.

### Fixed
- Ctrl+Z worked almost nowhere: it was only read in the point editor of the main window, so it did nothing in the point list (F3), the overlap check, the connection view or the patch window, and most changes of the control points had no undo step at all. Now one handler serves the main window and these windows (Ctrl+Z undo, Ctrl+Y or Ctrl+Shift+Z redo, a new change empties the redo list; not while a text field or number box has the keyboard focus, and not while a job is running). Every change of the points is one undo step with a name: delete (point, pair, list, outliers, "Remove N duplicates"), add, move (one step per drag), clean up, find points (all photos or one pair, also the pair re-check) and an alignment that drops outlier points. The Undo button got a Redo partner. Depth is 20 steps (each holds a copy of all points). Steps are forgotten when a photo is removed, all photos are cleared or a project is opened (photo numbers change). Not covered (no undo): crop frame, photo on/off, lens and alignment settings, the connection view settings, the rotations/lenses result of an alignment (Ctrl+Z restores the points and asks to align again). The masks keep their own history, and Ctrl+Z in the mask window only touches the masks. Not tried by clicking through the running program.
- Point editor (step 4): "Add point" and "Patch click" are now one tool choice (like radio buttons) instead of two independent switches. Before, both could light up while a click only did a patch click, and switching patch click off also switched add off. Pressing the active tool again, or Esc, returns to selecting points; "then find more automatically" only shows with "Add point".
- Overlap check: a photo with more pixels per angle than the view (e.g. the 7 mm photos next to 16 mm ones, about 5 times finer) is now averaged down to the view's step before blending/differencing, instead of being point-sampled (aliasing made fine texture show up as a difference). New checkbox "smooth fine photo" (on by default; off = old behaviour). Measured once on the test project (pair 1/2, difference mode): mean difference 0.235 without, 0.209 with the filter; a pair of equal photos gives 0.077.
- Overlap check: the remaining difference between a 7 mm and a 16 mm photo was a different brightness and contrast of the two cameras, not a geometry error. New checkbox "match brightness" (blend and difference mode, on by default; off = plain brightness as before; the checkerboard is never changed): photo B is mapped to the mean and spread of photo A over the common area first. Measured once each on the test project (difference mode, zoom 1): pair 1/2 0.209 -> 0.112, pair 1/3 0.232 -> 0.099, pair 3/4 (equal photos) 0.077 -> 0.075; runs vary by up to about 15 %. What stays red is mostly building edges, where the wide-angle photo is softer. Checks that the view matches the export: a single photo sampled by the view and by the export code (`render_final`, same plane) gave a mean difference of 0.0000 on the test project (photos 1 and 2) and in a unit test, so the view's geometry, gains, gain map and parallax field are the export's. Shifting photo B by up to 1 view pixel (about 0.06 degrees for the 7 mm photo) did not make pair 1/2 fit better, so the "stretched photo" in blend mode is not a geometry error; it is explained by the softness and different contrast of the wide-angle photo (blend now also uses the brightness match). Diagnostics: `PW_OVERLAP_STATS=1`, `PW_OV_TONE=0`.
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
