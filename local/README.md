# local/ – your private working folder

This folder is **not versioned** (see `.gitignore`); only this file is. Everything in `local/` belongs to
you and to the Claude session you run in this repository. Nobody else sees it, so put things here that
should not go into the repository:

- test photos and test projects (`.pwproj`), if you do not want them under `testdata/`
- debug logs, measurement notes, comparison pictures, scratch scripts
- your own notes for Claude and for later sessions

`CLAUDE.md` tells Claude to read the files below **if they exist**. None of them is required; create the
ones you find useful.

| File | Purpose |
|---|---|
| `PLAN.md` | open points and plans (what is done, what is next, what was tried and dropped) |
| `HANDOVER.md` | state for the next session: environment, decisions, how to run things on your machine |
| `WORKFLOW.md` | your personal way of working (git identity, shell quirks, tools you prefer) |
| `MECHANICS.md` | behaviour values taken from the original program and where they are used (optional) |
| `PROVENANCE.md` | where values and behaviour come from (optional) |

Keep large files (photos, renders) out of git: `local/`, `shots/`, `testdata/` and `target/` are all ignored.
