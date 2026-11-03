# Contributing

Thanks for your interest in PanoWeave.

## Setup

```
cargo build
cargo test
```

## Rules for changes

- **Every new feature gets a switch.** With the switch off, behaviour must be identical to before.
- **Constants** live in a config struct or as a `const`, not as scattered numbers.
- **No silent failures.** Log or show the reason when a pair or patch is rejected.
- **Nothing blocking in the GUI thread.** Heavy work belongs on worker threads, and long steps check `engine::cancelled()`.
- **Tests:** a new feature needs at least one test. Run `cargo build` and `cargo test` before opening a pull request.
- Edit existing files; do not add parallel copies. Do not commit screenshots or test output.
- The UI is in English.
- Do not copy logos or brand names.

## Windows tips

- A running `target\debug\panoweave.exe` is locked. Rename it before rebuilding.
- `gen` is a reserved keyword in Rust 2024.

## Pull requests

Describe what changed and why, and mention how you tested it.
