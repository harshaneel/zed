# Resource benchmark: this fork vs an Electron editor

One reason to render PDFs with the pure-Rust [hayro](https://crates.io/crates/hayro)
engine inside GPUI — rather than embedding a Chromium/`pdfium` viewer — is that it
keeps the editor's footprint native. This page measures that footprint against a
popular Chromium/Electron-based editor (VS Code derivative) on the same workload.

## Workload

Both editors had the **same large project open**: a production Ruby on Rails 8
monorepo (Ruby 3.4) with thousands of source files. A project of this shape spins
up several language servers at once, so the comparison reflects real day-to-day
memory pressure, not an empty window:

- Ruby LSP (+ a Rails LSP add-on server)
- Sorbet (static type checker, LSP mode)
- RuboCop (LSP mode)
- A Node-based language server
- File watchers (`watchman`, `fsevent_watch`)

## Method

- Measured **after indexing settled**, not at startup. Each editor's full process
  **tree** was sampled every 20 s; "settled" = the busiest process in either tree
  stayed under 6% CPU across repeated samples. (Sorbet's first indexing pass briefly
  spikes one worker to ~20%+ and transiently allocates several hundred MB per worker;
  that scratch memory is freed once indexing completes, so steady state is what is
  reported here.)
- **Fair attribution, both sides.** Each editor is credited with its entire process
  tree — the editor process **plus every language server / type checker / watcher it
  spawns**. RSS is summed with `ps`; CPU is the instantaneous second sample from `top`.
  Counting only the main process would unfairly favor whichever editor pushes more work
  into child processes.

## Results

Idle, settled, identical project loaded in both:

| Metric            | This fork (Zed + all LSPs) | Electron editor (all procs) | Result            |
| ----------------- | -------------------------- | --------------------------- | ----------------- |
| **RAM (RSS)**     | **249 MB**                 | 4,439 MB                    | **~17.8× lighter** |
| **Processes**     | 10                         | 137                         | ~13.7× fewer      |
| **Idle CPU**      | ~0%                        | ~12%                        | idle vs not idle  |

### This fork's process tree (RSS)

| Process                  | RSS    |
| ------------------------ | ------ |
| `zed` (editor)           | 132 MB |
| Node language server     | 33 MB  |
| Ruby LSP Rails server    | 20 MB  |
| `watchman`               | 17 MB  |
| Sorbet (settled)         | 14 MB  |
| Ruby LSP                 | 10 MB  |
| RuboCop (LSP)            | 9 MB   |
| 3× `fsevent_watch`       | 15 MB  |
| **Total**                | **249 MB** |

## Why the gap

- **No bundled browser engine.** The Electron editor ships a full Chromium runtime
  and runs a separate renderer process per window/tab plus an extension host; that is
  most of the 4.4 GB and the 137 processes.
- **The PDF viewer adds no native runtime.** hayro is pure Rust compiled into the
  binary — there is no `pdfium`/Chromium dependency, no bundled `dylib`, and no extra
  process to view a PDF. A `.pdf` renders inside the same GPUI pane the editor already
  uses.
- **Genuinely idle when idle.** Once language servers finish indexing, this fork sits
  at ~0% CPU; the Electron editor holds ~12% at rest (renderers + extension host).

## Reproducing

`ps`/`top`-based; no extra tooling. Open the same large project in both editors, wait
for indexing to finish, then for each editor sum RSS and CPU across its **full process
tree** (resolve children recursively from the editor's PID; for the Electron editor,
also union in any process whose name matches the app). Sample `top -l 2` and read the
**second** sample for instantaneous CPU. Hardware/project size will shift absolute
numbers; the ratio is the point.
