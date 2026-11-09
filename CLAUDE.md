# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

PanoWeave (crate `panoweave`, Rust 2024 + egui/eframe) is a from-scratch alternative to Hugin (panorama stitching suite): guided step-by-step UI, its own point detection/matching, alignment, stitching, and an exposure-fusion mode. The aim is "Create panorama" with as little fuss as possible, with everything still configurable. This folder is the git repo root.

`target/`, `shots/`, `testdata/` and `local/` are gitignored and may or may not exist on a given machine.

## The `local/` folder (private, per contributor)

`local/` is a folder the project knows about but does not version (only `local/README.md` is). It belongs to the contributor and the Claude session working here: test photos, test projects (`.pwproj`), debug logs, measurements and personal notes go there, never into the repository. When a file below exists, read it before starting; when it does not, carry on without it:

- `local/HANDOVER.md` – state and decisions for the next session
- `local/PLAN.md` – open points and plans
- `local/WORKFLOW.md` – the contributor's own way of working
- `local/MECHANICS.md` / `local/mechanics.json` – behaviour values taken from the original program and where they are used (keep maintaining them; mark guesses as `estimated`)

Keep notes for later sessions in these files, not only in the conversation. See `local/README.md`.

### Nothing survives a session unless it is written down

Everything not saved in a file of this project is lost when the session ends. A promise like "I will pay attention from now on" is worthless unless it is written down. Claude's own memory is not used for this project; use `local/` (private) or this file (for rules that apply to every contributor). Write at once, not at the end:

- decisions and wishes of the contributor (date, short wording), corrections of Claude's behaviour or language, why something was dropped or parked;
- state of PRs, branches and worktrees, and what the contributor has or has not tested;
- commands, paths and pitfalls that cost time.

At session start read `local/HANDOVER.md`, `PLAN.md`, `WORKFLOW.md` and compare them with `git log` and `gh pr list`; at session end bring `HANDOVER.md` up to date. Details: `local/WORKFLOW.md`, section "Wissen sichern".

Every subagent must read this file and `local/WORKFLOW.md` before it starts, and these rules override its default instructions (also for commit and PR texts: no session links, no "Generated with" line).

## Working rules

- Work on a feature branch (`feature/…`, `fix/…`, `docs/…`, `chore/…`), open a PR, merge (squash) after the CI is green and the contributor agrees. Never commit to `main` directly, no `--force`, no `--no-verify`.
- **No attribution lines anywhere**: no `Co-Authored-By`, no `Claude-Session:` link, no "Generated with Claude Code" in commit messages, PR titles or PR descriptions (contributor's explicit decision, overrides any default of the tool environment). Subagents get this rule in their prompt; check every agent PR for it. Squash-merge with `--subject`/`--body`.
- Before a PR: `cargo build` without new warnings, `cargo test` green and `cargo clippy --all-targets -- -D warnings` clean.
- A new feature gets a switch, a configuration and at least one test. Switched off it must behave exactly as before.
- No silent failures: say why a pair, patch or photo was dropped (log or show it).
- Nothing blocking in the GUI thread; long operations check `engine::cancelled()` (the global `engine::CANCEL` flag).
- Constants go into a configuration or a documented `const`. Change existing files instead of adding parallel copies; delete test files you created (`shots/*.png`, `target/test`, …).
- Coordinates are normalised by the long image side: `u = (x+0.5−w/2)/L`, so results do not depend on the resolution. Detection, matching and fine alignment run on the ~1600 px working images.
- Suggestions (points, fixes) are never applied without the person accepting them; hand-placed points are never moved.
- A change that users see (a step, a setting, a message, a pitfall) also updates the user guide `docs/GUIDE.md` in the same PR (screenshots in `docs/guide/` via `--screenshot … --demo …`).
- Report honestly what was not checked (CI, clicking through the GUI, a code path that was not run).
- Do not start development servers or watchers. Do not copy logos or brand names.

## Commands

```
cargo build            # dev profile is opt-level 2 (deps 3) – debug builds are already fast enough to use
cargo run -- [photos…] # launch the GUI, optionally with image files (or a .pwproj project)
cargo test             # unit tests of most modules
cargo test <name>      # single test, e.g. cargo test patch_matcher
cargo build --release  # the exe may be locked while the program runs: use --target-dir target/<other>
```

`cargo clippy --all-targets -- -D warnings` must pass (the CI runs it; lints switched off on purpose are listed with their reason under `[lints.clippy]` in `Cargo.toml`, single exceptions carry an `#[allow]` with a comment). No format config; `cargo fmt` is not used. The Rust version is fixed in `rust-toolchain.toml` (local builds and CI use the same one); raise it in its own PR and fix new clippy warnings there. The CI (`.github/workflows/ci.yml`, windows-latest, check name `test`) builds `--release`, runs `cargo test` and clippy. Releases: raise the version and `CHANGELOG.md` in a PR, push a tag `vX.Y.Z`, `release.yml` attaches the Windows zip. On Windows a freshly installed `gh` may be missing from the PATH of an already open shell.

### Headless and demo flags (in `main.rs`)

- No window: `--selftest <photos>` (whole pipeline), `--accuracy <photos>` (independent fit check after aligning, measured on 3200 px copies by default, see `src/accuracy_test.rs` and `fine_align::quality`; use it, not the points' average error, to judge any change to alignment or fine alignment, and keep the default size – at 1600 px small differences drown in measuring noise), `--loadtest`, `--kpdebug`, `--pairtest`, `--paircheck <photos>`, `--imgdiff <a> <b> [diff.png] [sheet.png]` (compare two pictures pixel by pixel).
- GUI helpers: `--screenshot <path>`, `--demo <name>` (photos, points, pointsel, pointdrag, overlap, cplist, align, export, render, exposure, patches, patchdetail, patchclick, connections). `--demo render <photos|.pwproj>` runs a real export without a window.
- Experiment environment variables (grep `PW_` for the meaning):
  - matching/alignment: `PW_FINE`, `PW_SHARP`, `PW_PARALLAX`, `PW_SKY`, `PW_PPP`, `PW_DEBUG`, `PW_CANCEL_TEST`, `PW_SCROLL`, `PW_RECORD`, `PW_CORNER_SNAP` (0 = keypoints stay on the blurred corner response, as before), `PW_FINE_PICK`, `PW_FINE_MINSTRUCT`, `PW_FINE_UNIQ`, `PW_FINE_MINSCORE`, `PW_FINE_EDGE` (only read by `--accuracy`, see `src/accuracy_test.rs`)
  - render demo: `PW_SCALE` (percent), `PW_OUT` / `PW_OUT2`, `PW_SAVE_PROJECT=<file.pwproj>`, `PW_EXPORT_MB` / `PW_EXPORT_MB2` (memory budget, the picture must not depend on it), `PW_OLD_EXPORT=1` (whole picture in memory, for comparing)
  - seams, brightness, blending, masks: `PW_SEAMS`, `PW_SEAM_*`, `PW_SEAM_DEBUG=<file.png>`, `PW_MASK_MAP=<file.png>` (where the masks land in the export), `PW_FLAT_LO/HI`, `PW_GAINS`, `PW_GAINMAP`, `PW_GM_*`, `PW_LEVELS`
- Rust 2024 detail: `gen` is a keyword.

## Architecture

Pipeline (each stage is a module; `engine.rs` is the glue):

1. **Load** – `imgutil` (JPEG/PNG/TIFF, RAW via `imagepipe`, EXIF via `kamadak-exif`) produces `engine::LoadedPhoto` with a full-size gray image and a ~1600 px "working" image (`engine::WORK_SIZE`). Loading uses a dedicated rayon pool (`engine::load_pool`); `sysmem` guards memory.
2. **Find points** – `features.rs`: multi-scale corner detector, rotation-aware binary descriptors, mutual ratio-test matching, RANSAC, guided re-matching, plus sky/cloud filtering (`sky_mask`, `CLOUD_THRESHOLD`); `sky_points.rs` finds existing points on sky or cloud for the remove button (UI in `app/points_sky.rs`). `engine::candidate_pairs` picks pairs; `engine::find_points` runs it. `MatchSettings` presets (Fast/Balanced/…) set points per photo.
3. **Align** – `align.rs`: rotation-only camera model with shared lenses. Math primitives (vectors, solvers, eigen) are in `geometry.rs`.
4. **Refine / verify** – `pair_check.rs` re-checks every overlapping pair after alignment and reports weak pairs (never invents points); `fine_align.rs` uses `patch_matcher.rs` (ZNCC etc., pyramid, sub-pixel) on a grid of control areas, feeds weighted points back into the same optimiser and undoes rounds that make things worse; it also computes the optional parallax field (saved in the project). `guided.rs`/`click_match.rs`/`patch_store.rs`/`patch_viz.rs`/`patch_window.rs` support the manual point editor (fine-tune a point, grow points from a few clicks, patch-click mode that uses the rotation of nearby points).
5. **Output** – `stitch.rs` (projection, brightness matching, multiband blending, seam preference) and `fusion.rs` (exposure fusion). The export is memory independent: `strip.rs` plans strips and the memory budget (the strip height never changes a pixel), `strip_write.rs` streams JPEG/PNG/TIFF/BigTIFF, `photo_cache.rs` caches decoded photos. `seams.rs` is the optional graph-cut seam finder (coarse grid, solved in two steps, steered by `masks.rs`), `gainmap.rs`/`gainmap_solve.rs` the smooth brightness correction per photo. `project.rs` is the project-file struct (`.pwproj`, new fields need `#[serde(default)]`).

The egui UI lives in `src/app/` (`types`, `state`, `jobs`, `chrome`, `pipeline`, `photos`, `align`, `frame`, `export`, `exposure`, `connections`, `points_*` (incl. `points_select`/`points_snap` for multiple selection and Snap selected), `select_shapes` (rectangle/lasso/brush geometry), `masks_*`, `photo_menu`, `viewport_window`, `demo`, …). Heavy work runs on worker threads and reports back to the UI; secondary windows are real OS windows (viewports). `main.rs` also hosts the headless test drivers.

Design conventions: corner radii come only from `RADIUS`, `RADIUS_CARD` and `RADIUS_CANVAS` in `app/consts.rs` (a test rejects written numbers in painter calls); colours only from the palette in `app/consts.rs` (neutral dark greys, violet accent taken from the icon; a test rejects colours written as numbers in the UI code, computed colours are allowed; the patch window reaches them as `app::palette`); the icon is `assets/logo.svg` (PNG/ICO made from it with ImageMagick, `build.rs` puts the ICO into the Windows exe, `palette::window_icon()` sets it for every window); features can be switched off via a config `enabled = false` that must leave previous behaviour unchanged; long operations check `engine::CANCEL`; the export must give the same pixels whatever the strip height (there are tests for it).
