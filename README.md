# PanoWeave

A guided panorama stitcher written in Rust. PanoWeave aims to be a more intuitive, faster alternative to Hugin with better automatic control-point detection. It covers the whole workflow in one window: **load photos → control points → align → crop → export**, plus an **exposure fusion** mode for bracketed shots.

> Status: early development (v0.1.0). Windows is the primary target; nothing is published as a release yet.

## Screenshots

| Photos and lenses | Control points |
|---|---|
| ![Photos step](docs/photos.png) | ![Points step](docs/points.png) |

| Alignment result and pair check | Export |
|---|---|
| ![Align step](docs/align.png) | ![Export step](docs/export.png) |

## Features

- **Guided workflow** with five steps: Photos, Points, Align, Frame, Export
- **Formats:** JPEG, PNG, TIFF and RAW (via `rawloader` / `imagepipe`), EXIF rotation is respected
- **Automatic control points:** multi-scale corner detection, rotation-aware descriptors, ratio-test matching, RANSAC and guided re-matching
- **No points on sky or clouds** (can be switched off)
- **Point editor:** place a few points by hand and let PanoWeave find more, with sub-pixel snapping, zoom levels and a sortable point list
- **Alignment:** rotation model with shared lenses and Levenberg-Marquardt bundle adjustment
- **Patch matching refinement:** ZNCC/SSD/SAD matching on a pyramid with weighted rounds that are rolled back if they make things worse, plus a parallax mode
- **Pair check and connection graph:** weak pairs and unconnected photo groups are reported instead of hidden
- **Export:** multi-band blending, gain compensation, and a seam preference for the sharpest photo; optional target width in pixels and 16-bit TIFF; a cancelled export never leaves a half-written file
- **Exposure fusion** (Mertens) for bracketed exposures
- **Cancel button** that stops long steps within about a second

## Building

Requires a current stable Rust toolchain (edition 2024).

```
cargo build --release
cargo run --release -- [photo1.jpg photo2.jpg ...]
```

The binary is written to `target/release/panoweave.exe`. Debug builds (`cargo build`) are already optimised (`opt-level = 2`, dependencies at 3).

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
| `stitch`, `fusion` | Projection, blending, export, exposure fusion |
| `app` | The egui user interface |
| `main` | Arguments and headless test drivers |

See [CONTRIBUTING.md](CONTRIBUTING.md) for development notes.

## Testing

```
cargo test
```

## Known limitations

- Photos with no overlap to the rest are not connected automatically. Use the manual point editor.
- `src/app/mod.rs` and `src/app/points.rs` are still large (about 1000 lines each) and are planned to be split further.
- A running export cannot be interrupted while the encoder is writing; a cancel takes effect afterwards and the file is discarded.

## License

Licensed under either of

- [Apache License, Version 2.0](LICENSE-APACHE)
- [MIT license](LICENSE-MIT)

at your option. Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in this project shall be dual licensed as above, without any additional terms or conditions.

## Acknowledgements

PanoWeave is an independent project. It is inspired by [Hugin](https://hugin.sourceforge.io/) but contains none of its code.
