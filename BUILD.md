# Building this fork (Zed + native PDF viewer)

This is an **unofficial fork** of [zed-industries/zed](https://github.com/zed-industries/zed)
that adds a native, pure-Rust PDF viewer (`crates/pdf_viewer`): rendering, text
selection + copy, and clickable hyperlinks. Not affiliated with Zed Industries.

These instructions are for **macOS (Apple Silicon)** and require **no `sudo`**.

## Prerequisites

1. **Rust**
   ```bash
   rustup default stable
   ```

2. **cmake** (a transitive build dep, e.g. wasmtime)
   ```bash
   brew install cmake
   ```

3. **Xcode** (full Xcode, not just Command Line Tools — Zed's Metal renderer
   needs the `metal` shader compiler, which ships only with Xcode).
   - Install **Xcode** from the App Store.
   - On first launch you only need the **macOS** platform component; you can
     uncheck iOS/watchOS/tvOS/visionOS (saves ~30 GB).
   - Launch Xcode once and accept the license via the GUI (avoids `sudo`).

4. **Metal Toolchain** (Xcode 16.3+ ships it as a separate download):
   ```bash
   export DEVELOPER_DIR=/Applications/Xcode.app/Contents/Developer
   xcodebuild -downloadComponent MetalToolchain
   ```

## Build & run

Point the toolchain at Xcode with an env var (no `sudo xcode-select` needed):

```bash
export DEVELOPER_DIR=/Applications/Xcode.app/Contents/Developer
cargo run -p zed
```

First build compiles the whole workspace and takes a while. Then open any `.pdf`
from the project panel.

## Optional: build a local .dmg

```bash
export DEVELOPER_DIR=/Applications/Xcode.app/Contents/Developer
script/bundle-mac
```

The resulting app/DMG is **unsigned** — macOS Gatekeeper will warn on first open.
Right-click the app → Open, or strip the quarantine attribute:
`xattr -dr com.apple.quarantine /path/to/Zed.app`.

## Using the PDF viewer

- Open a `.pdf` — it renders in a scrollable pane.
- Click-drag to select text (auto-scrolls at the edges, spans pages).
- `Cmd+C` or right-click → **Copy**.
- Click a hyperlink to open it in your browser.

See `crates/pdf_viewer/README.md` for design details and limitations.

## Staying current with upstream

This fork keeps the PDF viewer on the `pdf-viewer` branch. To update:

```bash
git remote add upstream https://github.com/zed-industries/zed.git   # once
git fetch upstream
git rebase upstream/main pdf-viewer
```
