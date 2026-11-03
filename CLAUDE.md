# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

PanoWeave (crate `panoweave`, Rust 2024 + egui/eframe) is a from-scratch alternative to Hugin (panorama stitching suite): guided step-by-step UI, its own point detection/matching, alignment, stitching, and an exposure-fusion mode. This folder is the git repo root. `target/`, `shots/`, `testdata/` (local test photos) and `local/` (private notes) are gitignored and may or may not exist on a given machine.

**Clean-room rule:** do not open, read or disassemble the original Hugin program. Behaviour values from an earlier analysis are kept in `local/MECHANICS.md` and `local/mechanics.json` (not versioned); they are only maintained, never extended from the original. If `local/HANDOVER.md` exists, read it first. Build further from the request and the existing behaviour of PanoWeave.

## Commands

```
cargo build            # dev profile is opt-level 2 (deps 3) – debug builds are already fast enough to use
cargo run -- [photos…] # launch the GUI, optionally with image files
cargo test             # unit tests (patch_matcher, pair_check, click_match, patch_store, stitch, app, patch_window)
cargo test <name>      # single test, e.g. cargo test patch_matcher
```

No lint/format config; use plain `cargo clippy` / `cargo fmt` if needed.

Headless/dev flags handled in `main.rs` (no window): `--selftest <photos>` (whole pipeline), `--loadtest` (load time/memory, then point finding), `--kpdebug`, `--pairtest`, `--paircheck`. GUI helpers: `--screenshot <path>`, `--demo <name>`. Env vars used for experiments: `PW_CANCEL_TEST`, `PW_FINE`, `PW_SKY`, `PW_SHARP`, `PW_PARALLAX`, `PW_PPP`, `PW_DEBUG`, `PW_SCROLL` (grep `PW_` for the meaning). `shots/` holds UI screenshots.

## Architecture

Pipeline (each stage is a module; `engine.rs` is the glue):

1. **Load** – `imgutil` (JPEG/PNG/TIFF, RAW via `rawloader`/`imagepipe`, EXIF via `kamadak-exif`) produces `engine::LoadedPhoto` with a full-size gray image and a ~1600 px "working" image (`engine::WORK_SIZE`). All detection/matching/fine-alignment runs on the working images; keep that in mind for coordinate scaling. Loading uses a dedicated rayon pool (`engine::load_pool`); `sysmem` guards memory.
2. **Find points** – `features.rs`: multi-scale corner detector, rotation-aware binary descriptors, mutual ratio-test matching, RANSAC, guided re-matching, plus sky/cloud filtering (`sky_mask`, `CLOUD_THRESHOLD`). `engine::candidate_pairs` picks pairs; `engine::find_points` runs it (cancellable via the global `engine::CANCEL` atomic). `MatchSettings` presets (Fast/Balanced/…) set points per photo.
3. **Align** – `align.rs`: rotation-only camera model with shared lenses; coordinates are normalised by the long image side (u = (x+0.5−w/2)/L), so results are resolution independent. Math primitives (vectors, solvers, eigen) are in `geometry.rs`.
4. **Refine / verify** – `pair_check.rs` re-checks every overlapping pair after alignment and reports weak pairs (never invents points); `fine_align.rs` uses `patch_matcher.rs` (ZNCC etc., pyramid, sub-pixel) on a grid of control areas, feeds weighted points back into the same optimiser, and undoes rounds that make things worse. Hand-placed points are never moved. `guided.rs`/`click_match.rs`/`patch_store.rs`/`patch_viz.rs`/`patch_window.rs` support the manual point editor (fine-tune a point, grow points from a few clicks, patch-click mode). Suggestions are never applied without the person accepting them.
5. **Output** – `stitch.rs` (projection/blending/export) and `fusion.rs` (exposure fusion: contrast/saturation/exposure weights, Laplacian pyramids). `project.rs` is the tiny project-file struct.

`app.rs` (~4800 lines) is the whole egui UI: a guided step flow plus an exposure-blending mode; heavy work runs on worker threads and reports back to the UI. `main.rs` also hosts the headless test drivers.

Design conventions seen across modules: features can be switched off via a config `enabled = false` that must leave previous behavior unchanged; long operations check `engine::CANCEL`; tunable constants are documented in `local/mechanics.json` (marked `estimated` where guessed) and `local/MECHANICS.md`, when present.
