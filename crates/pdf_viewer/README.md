# pdf_viewer

A native PDF viewer pane for Zed. Opens `.pdf` files in a scrollable view of
rendered pages with text selection, copy, and clickable hyperlinks.

## Features

- **Render** — pages rasterized in-process with the pure-Rust [hayro](https://crates.io/crates/hayro)
  engine (no native libraries, no Chromium).
- **Text selection** — click-drag to select, with auto-scroll at the viewport
  edges and selection spanning across pages.
- **Copy** — `Cmd+C` or right-click → Copy.
- **Hyperlinks** — click a link annotation to open its URL; pointer cursor on hover.

## How it works

- Each page is rendered to a `vello_cpu` pixmap, converted to a gpui `RenderImage`
  (RGBA → BGRA), and drawn in a scrollable, centered column.
- Selectable text is recovered by re-interpreting the page with a glyph-capturing
  hayro `Device` (`GlyphCollector`), which records each glyph's text and a
  character-cell box, then sorts them into reading order.
- Hit-testing maps cursor positions to glyphs/links analytically from the live
  scroll offset (not `bounds_for_item`, which lags a frame behind a scroll).
- `PdfView` is registered as a project item (`workspace::register_project_item`)
  after the other item types, so it wins for `.pdf` files.

Key files: `src/pdf_viewer.rs` (everything). Copy keybinding: a `PdfViewer`
context entry in `assets/keymaps/default-macos.json`.

## Building

Part of the Zed workspace — you build the whole app. See the repo's `BUILD.md`
for the full macOS prerequisites (Rust, cmake, Xcode + Metal Toolchain, the
sudo-free `DEVELOPER_DIR` build). Then:

```bash
export DEVELOPER_DIR=/Applications/Xcode.app/Contents/Developer
cargo run -p zed
```

Open any `.pdf` from the project panel.

## Tests

```bash
cargo test -p pdf_viewer
```

- `test_is_pdf_path` — the `.pdf` routing predicate.
- `test_render_pdf_rasterizes_all_pages` — renders `test_data/sample.pdf` and
  asserts page count + dimensions.
- `test_try_open_routes_only_pdfs` — integration: against a real `Project`,
  `try_open` claims `.pdf` and declines other paths.

## Limitations (prototype)

- Selection is glyph-cell granular (no sub-glyph caret); copy joins glyph text.
- No find-in-page, no internal (GoTo-page) link navigation — only external URIs.
- Zoom is fixed (rasterized at `RASTER_WIDTH`); no re-rasterization on zoom.
- No persistence — reopening a PDF re-rasterizes.
- Layout/hit-testing assumes the single-column, no-rotation page layout.
