//! A native PDF viewer pane for Zed.
//!
//! Pages are rasterized in-process with the pure-Rust hayro engine to BGRA
//! bitmaps, wrapped as gpui `RenderImage`s, and drawn in a scrollable column.
//! Text is recovered via a glyph-capturing hayro `Device` for drag-selection and
//! copy (`Cmd+C` / right-click). Registered as a project item so `.pdf` files
//! open in this view instead of the editor. See `init`.

mod document;
mod find;
mod find_ui;
mod glyph;
mod interaction;
mod view;

use gpui::{App, actions};

/// Width each page is rasterized to, in pixels. Higher = sharper but heavier.
pub(crate) const RASTER_WIDTH: f32 = 1600.0;
/// Default on-screen page width before any zoom (bitmap is scaled to this).
pub(crate) const DEFAULT_DISPLAY_WIDTH: f32 = 900.0;
/// Clamp range for the zoomable on-screen page width.
pub(crate) const MIN_DISPLAY_WIDTH: f32 = 200.0;
pub(crate) const MAX_DISPLAY_WIDTH: f32 = 4000.0;
/// Multiplicative zoom step.
pub(crate) const ZOOM_STEP: f32 = 1.2;
/// Vertical gap above the first page and between pages (matches the render's
/// `p_4`/`gap_4`, both 16px). Used to compute page positions for hit-testing.
pub(crate) const PAGE_GAP: f32 = 16.0;

actions!(
    pdf_viewer,
    [
        /// Copy the selected text to the clipboard.
        CopySelection,
        /// Zoom in.
        ZoomIn,
        /// Zoom out.
        ZoomOut,
        /// Fit the page width to the viewport.
        FitWidth,
        /// Fit the whole page within the viewport.
        FitPage,
        /// Open the in-document find bar.
        DeployFind,
        /// Close the find bar and clear match highlights.
        DismissFind,
        /// Select the next match.
        SelectNextMatch,
        /// Select the previous match.
        SelectPrevMatch,
        /// Toggle case-sensitive matching.
        ToggleCaseSensitive,
        /// Toggle whole-word matching.
        ToggleWholeWord
    ]
);

pub use document::PdfItem;
pub use view::PdfView;

/// Register the PDF viewer so `.pdf` files open in it. Call after the other
/// project-item registrations so this opener wins for PDFs.
pub fn init(cx: &mut App) {
    workspace::register_project_item::<PdfView>(cx);
}
