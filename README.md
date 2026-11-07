# PanoWeave

A guided panorama stitcher written in Rust. PanoWeave aims to be a more intuitive, faster alternative to Hugin with better automatic control-point detection. It covers the whole workflow in one window: **load photos → control points → align → crop → export**, plus an **exposure fusion** mode for bracketed shots.

> Status: early development (v0.2.0, alpha). Windows is the primary target; a ready-to-run Windows zip is attached to each [release](https://github.com/schmenn/panoweave/releases).

## Screenshots

| Photos and lenses | Control points |
|---|---|
| ![Photos step](docs/photos.png) | ![Points step](docs/points.png) |

| Alignment result and pair check | Export |
|---|---|
| ![Align step](docs/align.png) | ![Export step](docs/export.png) |

## Features

- **Guided workflow** with five steps: Photos, Points, Align, Frame, Export
- **Formats:** JPEG, PNG, TIFF and RAW (via `imagepipe`, optional `raw` feature), EXIF rotation is respected
- **Automatic control points:** multi-scale corner detection, rotation-aware descriptors, ratio-test matching, RANSAC and guided re-matching
- **No points on sky or clouds** (can be switched off)
- **Point editor:** place a few points by hand and let PanoWeave find more, with sub-pixel snapping, zoom levels and a sortable point list; the patch-click mode uses the rotation and zoom of nearby points (and of the alignment), so slightly turned photos are matched too
- **Protected points:** lock a point (key L) and no bulk action (clean up, delete all in a pair, outlier removal, duplicate removal, a new search) removes or moves it; a locked point is also refused by single delete and drag until it is unlocked.
- **Duplicate points:** points that lie on top of each other are marked in the point list and can be removed with one click (hand-placed points stay).
- **Undo and redo** (Ctrl+Z, Ctrl+Y) for every change of the control points, in the main window and in the point list, overlap check, connection view and patch window.
- **Overlap check:** lay two photos over each other (blend, difference, checkerboard) with a brightness match and a filter for photos with finer resolution.
- **Alignment:** rotation model with shared lenses and Levenberg-Marquardt bundle adjustment
- **Patch matching refinement:** ZNCC/SSD/SAD matching on a pyramid with weighted rounds that are rolled back if they make things worse, plus an optional parallax mode (local correction near the seams for close objects; saved in the project file)
- **Pair check and connection graph:** weak pairs and unconnected photo groups are reported instead of hidden
- **Export:** multi-band blending, gain compensation, and a seam preference for the sharpest photo; optional target width in pixels and 16-bit TIFF; a cancelled export never leaves a half-written file
- **Memory-independent export:** the panorama is made strip by strip and written straight to the file (JPEG, PNG, TIFF, BigTIFF above 3.5 GB). The strip height follows from the free memory, but it never changes a pixel, so the picture is the same whatever memory the computer has. Free disk space is checked before the export starts.
- **Smooth brightness correction across each photo:** besides one gain per photo, a smooth correction field per photo (a coarse grid, solved from the overlaps, robust against clouds and people) evens out sky gradients, vignetting and automatic exposure, so the footprints of single photos no longer show as rectangles. On by default, switchable in the export dialog (Blending).
- **Lens correction** (optional, off by default; Export step, Blending): a response curve (one exponent per colour channel) and a vignetting model per lens, solved from the overlaps of all photos. It helped on one test set and not on another, so it is an experiment, not a default.
- **Smart seams** (optional, off by default): a graph-cut seam finder searches, once for the whole panorama, where the photos agree, so that close objects that two photos see from different sides (poles, lamps, people) are not cut in two. Found on a coarse grid in two steps, so the result does not depend on the strips either.
- **Masks:** paint "use here" (green) and "do not use here" (red) masks on a photo with a brush or lasso (Export step, Blending, "Edit masks..."). They steer the seams, with or without smart seams, are saved in the project file, and a "Show seams" view shows which photo wins where.
- **Exposure fusion** (Mertens) for bracketed exposures
- **Cancel button** that stops long steps within about a second

## Building

Requires a current stable Rust toolchain (edition 2024).

```
cargo build --release
cargo run --release -- [photo1.jpg photo2.jpg ...]
```

The binary is written to `target/release/panoweave.exe`. Debug builds (`cargo build`) are already optimised (`opt-level = 2`, dependencies at 3).

RAW photos are read with `imagepipe` / `rawloader`, which are LGPL licensed. They are built in by default (feature `raw`); `cargo build --release --no-default-features` leaves them out (JPEG, PNG and TIFF keep working).

### Windows SmartScreen

The release `.exe` is not code-signed, so Windows SmartScreen may say "Windows protected your PC" for a new download. Click "More info", then "Run anyway". You can compare the SHA-256 checksum of the zip (`Get-FileHash <file>`) with the one in the release notes.

## Usage

Start the program and add photos in the first step, or pass image files on the command line. The steps then guide you through finding points, aligning, cropping and exporting. If photos end up in separate groups, the Connections window lists them, and you can connect them manually with the point editor.

### Command-line tools

| Flag | Purpose |
|---|---|
| `--selftest <photos>` | Run the whole pipeline without a window |
| `--loadtest` | Measure load time and memory |
| `--kpdebug`, `--pairtest` | Point detection and pair debugging |
| `--paircheck <photos>` | Pair check and triangle test |
| `--screenshot <file.png>` | Save a screenshot after about 120 frames and exit |
| `--demo <name>` | Open a demo view (`photos`, `points`, `align`, `export`, `exposure`, `patches`, `connections`, ...) |

Environment variables for experiments: `PW_FINE`, `PW_SHARP`, `PW_PARALLAX`, `PW_SKY`, `PW_PPP` (points per photo), `PW_DEBUG`, `PW_CANCEL_TEST`, `PW_SCROLL`.

Export and seam experiments (with `--screenshot ... --demo render <photos or .pwproj>`): `PW_SCALE` (percent), `PW_OUT` / `PW_OUT2` (output file of the first and of a second export), `PW_EXPORT_MB` / `PW_EXPORT_MB2` (memory budget in MB, shows that the picture does not depend on it), `PW_OLD_EXPORT=1` (render the whole picture in memory, for comparing), `PW_SAVE_PROJECT=<file.pwproj>` (keep the alignment; a `.pwproj` on the command line loads it again), `PW_SEAMS=0/1` and `PW_SEAMS2` (force smart seams off / on for the first / second export), `PW_SEAM_DATA`, `PW_SEAM_SMOOTH`, `PW_SEAM_CUT`, `PW_SEAM_POWER`, `PW_SEAM_MISSING`, `PW_SEAM_SIDE` (seam finder weights and grid), `PW_GAINS=0` (no brightness matching), `PW_GAINMAP=0/1`, `PW_GM_GRID`, `PW_GM_SMOOTH`, `PW_GM_MAXDEV` (brightness correction field), `PW_LEVELS=<n>` (blend levels), `PW_FLAT_LO` / `PW_FLAT_HI` (texture limits below which a detail advantage fades out in the seam search; `PW_FLAT_HI=0` switches it off), `PW_SEAM_DEBUG=<file.png>` (write a picture of the seams).

`panoweave --imgdiff <a.png> <b.png> [diff.png] [sheet.png]` compares two pictures pixel by pixel (maximum and mean difference, number of differing pixels), optionally writes a difference picture and a sheet of the places with the biggest differences.

Settings are stored in `%APPDATA%\PanoWeave\settings.json`.

## Project layout

| Module | Role |
|---|---|
| `imgutil`, `sysmem` | Loading (EXIF, RAW), 1600 px working images, memory checks |
| `features`, `guided` | Corner detection, descriptors, matching, RANSAC, sky/cloud filter |
| `engine` | Pipeline glue: find points, candidate pairs, align, cancel |
| `align`, `geometry` | Camera model and bundle adjustment |
| `patch_matcher`, `fine_align` | Patch matching and refinement |
| `pair_check`, `click_match` | Pair verification and patch-click mode |
| `patch_store`, `patch_viz`, `patch_window` | Patch records, heatmaps, "Patch comparison" window |
| `stitch`, `fusion` | Projection, blending, exposure fusion |
| `strip`, `strip_write`, `photo_cache` | Memory-independent export: strip planning and memory budget, streaming JPEG/PNG/TIFF writers, cache of decoded photos |
| `lensphoto` | Optional per-lens response curve and vignetting model |
| `overlap`, `duplicate_points` | Overlap check sampling; detection of points on top of each other |
| `seams`, `masks` | Seam finder (graph cut) and the masks painted on photos |
| `app` | The egui user interface, split into `types`, `state`, `jobs`, `demo`, `chrome`, `pipeline`, `points_*`, `photos`, `align`, `frame`, `export`, `exposure`, `connections` |
| `main` | Arguments and headless test drivers |

See [CONTRIBUTING.md](CONTRIBUTING.md) for development notes.

## Testing

```
cargo test
```

## Known limitations

- Photos with no overlap to the rest are not connected automatically. Use the manual point editor.
- Smart seams cannot fix every case: where a close object is seen from very different sides and no seam can go around it, a step or ghost remains (for example a pole whose bracket lies between two rows of photos). Masks are meant for those cases.
- The strip export is about twice as slow as writing the whole picture from memory, and smart seams add some seconds. Panoramas of 1 to 2 gigapixels are planned for but not yet tested (the largest test so far is about 92 megapixels).
- The brightness correction and the seam finder even out most exposure differences between photos, but a faint soft colour edge can remain in a smooth sky where two cameras (for example a wide and a tele lens) differ in colour rendering.
- The parallax mode is off by default and only corrects small shifts (up to a few pixels) near the seams.
- A few functions in `src/app/` (`ui_align`, `graph_window`, `ui_frame`) are still long.
- A running export cannot be interrupted while the encoder is writing; a cancel takes effect afterwards and the file is discarded.

## License

Licensed under either of

- [Apache License, Version 2.0](LICENSE-APACHE)
- [MIT license](LICENSE-MIT)

at your option. Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in this project shall be dual licensed as above, without any additional terms or conditions.

The licenses of all crates PanoWeave is built from, including the LGPL crates used for RAW support, are listed in [THIRD_PARTY_LICENSES.md](THIRD_PARTY_LICENSES.md).

## Acknowledgements

PanoWeave is an independent project. It is inspired by [Hugin](https://hugin.sourceforge.io/) but contains none of its code.
