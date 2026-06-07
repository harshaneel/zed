//! A native PDF viewer pane for Zed.
//!
//! Pages are rasterized in-process with the pure-Rust hayro engine to BGRA
//! bitmaps, wrapped as gpui `RenderImage`s, and drawn in a scrollable column.
//! Text is recovered via a glyph-capturing hayro `Device` for drag-selection and
//! copy (`Cmd+C` / right-click). Registered as a project item so `.pdf` files
//! open in this view instead of the editor. See `init`.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, Result, anyhow};
use gpui::{
    App, ClipboardItem, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable,
    IntoElement, KeyDownEvent, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent,
    ParentElement, Pixels, Point, Render, RenderImage, ScrollHandle, SharedString, Styled,
    Subscription, Task, Window, actions, anchored, deferred, div, img, px, rgba,
};
use hayro::hayro_interpret::font::Glyph;
use hayro::hayro_interpret::hayro_cmap::BfString;
use hayro::hayro_interpret::{
    BlendMode, ClipPath, Context as PdfContext, Device, GlyphDrawMode, Image, InterpreterCache,
    InterpreterSettings, Paint, PathDrawMode, SoftMask, TransformExt, interpret_page,
};
use hayro::hayro_syntax::Pdf;
use hayro::hayro_syntax::object::{Array, Dict, Name, Rect as PdfRect, String as PdfString};
use hayro::hayro_syntax::page::Page;
use hayro::vello_cpu::color::palette::css::WHITE;
use hayro::vello_cpu::kurbo::{Affine, BezPath, Point as KurboPoint, Rect};
use hayro::{RenderCache, RenderSettings, render};
use image::{Frame, RgbaImage};
use project::{Project, ProjectEntryId, ProjectPath};
use smallvec::smallvec;
use ui::ContextMenu;
use ui::Tooltip;
use ui::prelude::*;
use workspace::{
    Pane,
    item::{Item, ProjectItem},
};

/// Width each page is rasterized to, in pixels. Higher = sharper but heavier.
const RASTER_WIDTH: f32 = 1600.0;
/// Default on-screen page width before any zoom (bitmap is scaled to this).
const DEFAULT_DISPLAY_WIDTH: f32 = 900.0;
/// Clamp range for the zoomable on-screen page width.
const MIN_DISPLAY_WIDTH: f32 = 200.0;
const MAX_DISPLAY_WIDTH: f32 = 4000.0;
/// Multiplicative zoom step.
const ZOOM_STEP: f32 = 1.2;
/// Vertical gap above the first page and between pages (matches the render's
/// `p_4`/`gap_4`, both 16px). Used to compute page positions for hit-testing.
const PAGE_GAP: f32 = 16.0;

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

/// A run of text and its bounding box in raster-pixel space (top-left origin).
#[derive(Clone)]
struct TextGlyph {
    text: String,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
}

/// A hyperlink annotation: clickable rectangle (raster-pixel space) + target URI.
#[derive(Clone)]
struct Link {
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    uri: String,
}

/// A single rasterized page plus its selectable text runs (reading order) and
/// hyperlink annotations.
struct PdfPage {
    image: Arc<RenderImage>,
    width: u32,
    height: u32,
    glyphs: Vec<TextGlyph>,
    links: Vec<Link>,
}

/// An active text selection as an inclusive range of *global* glyph indices
/// (glyphs of all pages concatenated in reading order), so a selection can span
/// pages.
#[derive(Clone, Copy)]
struct Selection {
    anchor: usize,
    head: usize,
}

impl Selection {
    fn bounds(&self) -> (usize, usize) {
        (self.anchor.min(self.head), self.anchor.max(self.head))
    }
}

/// In-document find state: the query, options, the current match list (page-local
/// glyph ranges), and which match is selected.
#[derive(Default)]
struct FindState {
    active: bool,
    query: String,
    options: FindOptions,
    matches: Vec<FindMatch>,
    current: Option<usize>,
}

/// Project-level item: holds the rasterized pages for one PDF file.
pub struct PdfItem {
    abs_path: PathBuf,
    project_path: ProjectPath,
    entry_id: Option<ProjectEntryId>,
    pages: Vec<Arc<PdfPage>>,
}

impl project::ProjectItem for PdfItem {
    fn try_open(
        project: &Entity<Project>,
        path: &ProjectPath,
        cx: &mut App,
    ) -> Option<Task<Result<Entity<Self>>>> {
        let abs_path = project.read(cx).absolute_path(path, cx)?;
        if !is_pdf_path(&abs_path) {
            return None;
        }

        let project_path = path.clone();
        let entry_id = project.read(cx).entry_for_path(path, cx).map(|entry| entry.id);

        Some(cx.spawn(async move |cx| {
            let render_path = abs_path.clone();
            let pages = cx
                .background_spawn(async move { render_pdf(&render_path) })
                .await
                .context("rasterizing PDF")?;

            let item = cx.update(|cx| {
                cx.new(|_| PdfItem {
                    abs_path,
                    project_path,
                    entry_id,
                    pages,
                })
            });
            Ok(item)
        }))
    }

    fn entry_id(&self, _: &App) -> Option<ProjectEntryId> {
        self.entry_id
    }

    fn project_path(&self, _: &App) -> Option<ProjectPath> {
        Some(self.project_path.clone())
    }

    fn is_dirty(&self) -> bool {
        false
    }
}

/// Whether `path` names a PDF file (by extension, case-insensitive).
fn is_pdf_path(path: &std::path::Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("pdf"))
}

/// Rasterize every page of the PDF to a BGRA `RenderImage` with pure-Rust hayro.
/// Blocking; run on a background thread.
fn render_pdf(path: &std::path::Path) -> Result<Vec<Arc<PdfPage>>> {
    let bytes = std::fs::read(path).context("reading PDF file")?;
    let pdf = Pdf::new(bytes).map_err(|e| anyhow!("parsing PDF: {e:?}"))?;

    let cache = RenderCache::new();
    let interpreter = InterpreterSettings::default();

    let mut pages = Vec::new();
    for page in pdf.pages().iter() {
        let (width_pts, _height_pts) = page.render_dimensions();
        let scale = RASTER_WIDTH / width_pts;

        let settings = RenderSettings {
            x_scale: scale,
            y_scale: scale,
            width: None,
            height: None,
            bg_color: WHITE,
        };
        let pixmap = render(&page, &cache, &interpreter, &settings);
        let width = pixmap.width() as u32;
        let height = pixmap.height() as u32;

        // hayro gives premultiplied RGBA; gpui's RenderImage is BGRA. Pages are
        // rendered on opaque white (alpha == 255), so premultiplied == straight
        // and we just reorder channels to BGRA.
        let mut bgra = Vec::with_capacity((width * height * 4) as usize);
        for px in pixmap.data() {
            bgra.extend_from_slice(&[px.b, px.g, px.r, px.a]);
        }
        let buffer = RgbaImage::from_raw(width, height, bgra)
            .context("pixmap byte length did not match dimensions")?;
        let render_image = RenderImage::new(smallvec![Frame::new(buffer)]);

        let glyphs = extract_glyphs(&page, scale);
        let links = extract_links(&page, scale);

        pages.push(Arc::new(PdfPage {
            image: Arc::new(render_image),
            width,
            height,
            glyphs,
            links,
        }));
    }

    Ok(pages)
}

/// Read the page's `Link` annotations and map each clickable `/Rect` into the
/// same raster-pixel space as the rendered image.
fn extract_links(page: &Page, scale: f32) -> Vec<Link> {
    let initial = Affine::scale_non_uniform(scale as f64, scale as f64)
        * page.initial_transform(true).to_kurbo();
    let mut links = Vec::new();
    let Some(annotations) = page.raw().get::<Array>(b"Annots") else {
        return links;
    };
    for annotation in annotations.iter::<Dict>() {
        if annotation.get::<Name>(b"Subtype").map(|n| n.as_str() == "Link") != Some(true) {
            continue;
        }
        let Some(rect) = annotation.get::<PdfRect>(b"Rect") else {
            continue;
        };
        let Some(uri) = annotation
            .get::<Dict>(b"A")
            .and_then(|action| action.get::<PdfString>(b"URI"))
            .map(|s| std::string::String::from_utf8_lossy(s.as_bytes()).into_owned())
        else {
            continue;
        };

        let corners = [
            initial * KurboPoint::new(rect.x0, rect.y0),
            initial * KurboPoint::new(rect.x1, rect.y0),
            initial * KurboPoint::new(rect.x1, rect.y1),
            initial * KurboPoint::new(rect.x0, rect.y1),
        ];
        let min_x = corners.iter().map(|p| p.x).fold(f64::MAX, f64::min);
        let max_x = corners.iter().map(|p| p.x).fold(f64::MIN, f64::max);
        let min_y = corners.iter().map(|p| p.y).fold(f64::MAX, f64::min);
        let max_y = corners.iter().map(|p| p.y).fold(f64::MIN, f64::max);
        links.push(Link {
            x: min_x as f32,
            y: min_y as f32,
            w: (max_x - min_x) as f32,
            h: (max_y - min_y) as f32,
            uri,
        });
    }
    links
}

/// Re-interpret a page with a glyph-capturing [`Device`] to recover the text and
/// each run's on-page bounding box, in the same raster-pixel space as the image.
fn extract_glyphs(page: &Page, scale: f32) -> Vec<TextGlyph> {
    // Same transform the renderer uses, so glyph boxes line up with the bitmap.
    let initial = Affine::scale_non_uniform(scale as f64, scale as f64)
        * page.initial_transform(true).to_kurbo();
    let (width_pts, height_pts) = page.render_dimensions();
    let bbox = Rect::new(
        0.0,
        0.0,
        (width_pts * scale) as f64,
        (height_pts * scale) as f64,
    );

    let cache = InterpreterCache::new();
    let mut context = PdfContext::new(
        initial,
        bbox,
        &cache,
        page.xref(),
        InterpreterSettings::default(),
    );
    let mut collector = GlyphCollector { glyphs: Vec::new() };
    interpret_page(page, &mut context, &mut collector);
    sort_reading_order(collector.glyphs)
}

/// Order glyphs for selection/copy. PDF content streams draw glyphs in arbitrary
/// order, so we recover reading order. Two-column pages are handled region-aware:
/// rows that span the gutter (titles, author blocks, full-width captions) stay
/// row-major in place, while the genuinely two-column bands between them are
/// ordered column-major (left column top-to-bottom, then right). This handles the
/// common paper layout of a full-width title above a 2-column body. Single-column
/// pages fall back to plain top-to-bottom, left-to-right order.
fn sort_reading_order(glyphs: Vec<TextGlyph>) -> Vec<TextGlyph> {
    if glyphs.is_empty() {
        return glyphs;
    }
    let page_width = glyphs.iter().map(|g| g.x + g.w).fold(0.0_f32, f32::max);
    let Some(split) = detect_column_split(&glyphs, page_width) else {
        return cluster_rows(glyphs).into_iter().flatten().collect();
    };

    let rows = cluster_rows(glyphs);
    let mut out: Vec<TextGlyph> = Vec::new();
    let mut i = 0;
    while i < rows.len() {
        if row_spans_gutter(&rows[i], split) {
            // Full-width row (e.g. the title): keep it as a single row.
            out.extend(rows[i].iter().cloned());
            i += 1;
        } else {
            // A maximal run of two-column rows: emit the whole left column
            // (top-to-bottom), then the whole right column.
            let start = i;
            while i < rows.len() && !row_spans_gutter(&rows[i], split) {
                i += 1;
            }
            for row in &rows[start..i] {
                out.extend(row.iter().filter(|g| g.x + g.w / 2.0 < split).cloned());
            }
            for row in &rows[start..i] {
                out.extend(row.iter().filter(|g| g.x + g.w / 2.0 >= split).cloned());
            }
        }
    }
    out
}

/// Whether a row has text on both sides of the gutter (a glyph crossing it) — a
/// full-width line rather than two side-by-side column lines.
fn row_spans_gutter(row: &[TextGlyph], split: f32) -> bool {
    row.iter()
        .any(|g| g.w > 0.0 && g.x < split && g.x + g.w > split)
}

/// The x of a two-column gutter, if the page has one. Scans the central band for
/// the x the fewest glyphs straddle and accepts it only if that's nearly empty,
/// so single-column / figure pages return `None`.
fn detect_column_split(glyphs: &[TextGlyph], page_width: f32) -> Option<f32> {
    let total = glyphs.iter().filter(|g| g.w > 0.0).count();
    if page_width <= 0.0 || total < 300 {
        return None; // too little text to confidently call it a column layout
    }
    let (lo, hi) = (page_width * 0.35, page_width * 0.65);
    let steps = 48;
    let mut best_x = lo;
    let mut best_straddle = usize::MAX;
    for i in 0..=steps {
        let x = lo + (hi - lo) * (i as f32 / steps as f32);
        let straddle = glyphs
            .iter()
            .filter(|g| g.w > 0.0 && g.x < x && g.x + g.w > x)
            .count();
        if straddle < best_straddle {
            best_straddle = straddle;
            best_x = x;
        }
    }
    // Require: (1) the gutter is straddled by almost nothing (a few full-width
    // lines are ok), and (2) both sides hold substantial text. The balance check
    // rejects tables (a sparse label column beside a wide content column) and
    // figure/caption pages, which want row-major order instead.
    let left = glyphs
        .iter()
        .filter(|g| g.w > 0.0 && g.x + g.w / 2.0 < best_x)
        .count();
    let right = total - left;
    let balanced = left.min(right) >= total * 3 / 10;
    // A real column gutter is straddled by ~nothing (only the odd full-width
    // line); a table's "gutter" or spurious inter-word alignment straddles more.
    (best_straddle <= total / 150 && balanced).then_some(best_x)
}

/// Cluster glyphs into visual rows (top-to-bottom), each row sorted left-to-right.
fn cluster_rows(mut glyphs: Vec<TextGlyph>) -> Vec<Vec<TextGlyph>> {
    if glyphs.is_empty() {
        return Vec::new();
    }
    let cmp_f32 = |a: f32, b: f32| a.partial_cmp(&b).unwrap_or(std::cmp::Ordering::Equal);
    // Line-clustering tolerance: ~70% of the median glyph height.
    let mut heights: Vec<f32> = glyphs.iter().map(|g| g.h).filter(|h| *h > 0.0).collect();
    heights.sort_by(|a, b| cmp_f32(*a, *b));
    let tol = heights.get(heights.len() / 2).copied().unwrap_or(10.0) * 0.7;

    glyphs.sort_by(|a, b| cmp_f32(a.y, b.y).then(cmp_f32(a.x, b.x)));

    let mut rows: Vec<Vec<TextGlyph>> = Vec::new();
    let mut row: Vec<TextGlyph> = Vec::new();
    let mut line_y = glyphs[0].y;
    for g in glyphs {
        if !row.is_empty() && (g.y - line_y) > tol {
            row.sort_by(|a, b| cmp_f32(a.x, b.x));
            rows.push(std::mem::take(&mut row));
        }
        if row.is_empty() {
            line_y = g.y;
        }
        row.push(g);
    }
    if !row.is_empty() {
        row.sort_by(|a, b| cmp_f32(a.x, b.x));
        rows.push(row);
    }
    rows
}

/// A [`Device`] that records only glyph draws (text + transformed bounds) and
/// ignores every other drawing instruction.
struct GlyphCollector {
    glyphs: Vec<TextGlyph>,
}

impl<'a> Device<'a> for GlyphCollector {
    fn draw_glyph(
        &mut self,
        glyph: &Glyph<'a>,
        transform: Affine,
        glyph_transform: Affine,
        _paint: &Paint<'a>,
        _draw_mode: &GlyphDrawMode,
    ) {
        let Some(unicode) = glyph.as_unicode() else {
            return;
        };
        let text = match unicode {
            BfString::Char(c) => c.to_string(),
            BfString::String(s) => s,
        };

        // Build a character-CELL box (advance wide, ascent..descent tall) rather
        // than glyph ink bounds: `outline()` is empty for some fonts (so ink
        // bounds collapse to zero and become unclickable), and a cell makes the
        // selection cover whole lines like a normal editor.
        // `transform * glyph_transform` maps glyph (1000-upem) space to device px.
        let to_device = transform * glyph_transform;
        let advance = match glyph {
            Glyph::Outline(outline) => outline.advance_width(),
            Glyph::Type3(_) => None,
        }
        .unwrap_or(500.0) as f64;
        const ASCENT: f64 = 760.0;
        const DESCENT: f64 = 240.0;

        let baseline = to_device * KurboPoint::new(0.0, 0.0);
        let advance_end = to_device * KurboPoint::new(advance, 0.0);
        let ascent_pt = to_device * KurboPoint::new(0.0, ASCENT);
        let descent_pt = to_device * KurboPoint::new(0.0, -DESCENT);

        let x = baseline.x.min(advance_end.x) as f32;
        let w = (baseline.x - advance_end.x).abs() as f32;
        let y = ascent_pt.y.min(descent_pt.y) as f32;
        let h = (ascent_pt.y - descent_pt.y).abs() as f32;

        self.glyphs.push(TextGlyph { text, x, y, w, h });
    }

    fn set_soft_mask(&mut self, _: Option<SoftMask<'a>>) {}
    fn set_blend_mode(&mut self, _: BlendMode) {}
    fn draw_path(&mut self, _: &BezPath, _: Affine, _: &Paint<'a>, _: &PathDrawMode) {}
    fn push_clip_path(&mut self, _: &ClipPath) {}
    fn push_transparency_group(&mut self, _: f32, _: Option<SoftMask<'a>>, _: BlendMode) {}
    fn draw_image(&mut self, _: Image<'a, '_>, _: Affine) {}
    fn pop_clip_path(&mut self) {}
    fn pop_transparency_group(&mut self) {}
}

/// The pane view over a [`PdfItem`].
pub struct PdfView {
    pdf_item: Entity<PdfItem>,
    project: Entity<Project>,
    focus_handle: FocusHandle,
    /// Tracks the scroll container; gpui records each page child's window bounds.
    scroll_handle: ScrollHandle,
    selection: Option<Selection>,
    dragging: bool,
    /// Last cursor position (window space) while dragging, for auto-scroll.
    last_mouse: Option<Point<Pixels>>,
    /// Repeating task that auto-scrolls while the cursor is held at an edge.
    autoscroll_task: Option<Task<()>>,
    /// On-screen page width in px; the zoom level (pages scale to this).
    display_width: f32,
    /// Open right-click menu: (menu, click position, dismiss subscription).
    context_menu: Option<(Entity<ContextMenu>, Point<Pixels>, Subscription)>,
    /// In-document find bar state.
    find: FindState,
    /// Focus target for the find bar's text input.
    find_focus_handle: FocusHandle,
}

impl PdfView {
    pub fn new(pdf_item: Entity<PdfItem>, project: Entity<Project>, cx: &mut Context<Self>) -> Self {
        Self {
            pdf_item,
            project,
            focus_handle: cx.focus_handle(),
            scroll_handle: ScrollHandle::new(),
            selection: None,
            dragging: false,
            last_mouse: None,
            autoscroll_task: None,
            display_width: DEFAULT_DISPLAY_WIDTH,
            context_menu: None,
            find: FindState::default(),
            find_focus_handle: cx.focus_handle(),
        }
    }

    /// Open a right-click menu (Copy) at the cursor.
    fn deploy_context_menu(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let position = event.position;
        let weak = cx.entity().downgrade();
        let menu = ContextMenu::build(window, cx, move |menu, _, _| {
            let weak = weak.clone();
            menu.entry("Copy", None, move |_, cx| {
                weak.update(cx, |this, cx| {
                    if let Some(text) = this.selected_text(cx) {
                        cx.write_to_clipboard(ClipboardItem::new_string(text));
                    }
                })
                .ok();
            })
        });
        window.focus(&menu.focus_handle(cx), cx);
        let subscription = cx.subscribe(&menu, |this, _, _: &DismissEvent, cx| {
            this.context_menu.take();
            cx.notify();
        });
        self.context_menu = Some((menu, position, subscription));
        cx.notify();
    }

    fn set_display_width(&mut self, width: f32, cx: &mut Context<Self>) {
        self.display_width = width.clamp(MIN_DISPLAY_WIDTH, MAX_DISPLAY_WIDTH);
        // Zoom changes page scale; keep the current find match centered.
        if self.find.current.is_some() {
            self.scroll_current_match_into_view(cx);
        }
        cx.notify();
    }

    fn zoom_in(&mut self, _: &ZoomIn, _: &mut Window, cx: &mut Context<Self>) {
        self.set_display_width(self.display_width * ZOOM_STEP, cx);
    }

    fn zoom_out(&mut self, _: &ZoomOut, _: &mut Window, cx: &mut Context<Self>) {
        self.set_display_width(self.display_width / ZOOM_STEP, cx);
    }

    /// Scale pages so one page's width fills the viewport.
    fn fit_width(&mut self, _: &FitWidth, _: &mut Window, cx: &mut Context<Self>) {
        let viewport_width = f32::from(self.scroll_handle.bounds().size.width);
        if viewport_width > 0.0 {
            self.set_display_width(viewport_width - 2.0 * PAGE_GAP, cx);
        }
    }

    /// Scale pages so a whole (first) page fits within the viewport.
    fn fit_page(&mut self, _: &FitPage, _: &mut Window, cx: &mut Context<Self>) {
        let bounds = self.scroll_handle.bounds();
        let viewport_width = f32::from(bounds.size.width);
        let viewport_height = f32::from(bounds.size.height);
        if let Some(page) = self.pdf_item.read(cx).pages.first()
            && page.width > 0
            && viewport_height > 0.0
        {
            // height = width * (page_h / page_w); pick width so height fits, and cap to viewport width.
            let aspect = page.height as f32 / page.width as f32;
            let width_for_height = (viewport_height - 2.0 * PAGE_GAP) / aspect;
            self.set_display_width(width_for_height.min(viewport_width - 2.0 * PAGE_GAP), cx);
        }
    }

    /// Map a window point to the page it lands on plus the raster-space `(rx, ry)`
    /// within that page. Computed analytically from the live scroll offset and the
    /// known column layout (`PAGE_GAP` above/between pages, centered at
    /// `DISPLAY_WIDTH`). This is intentionally NOT `scroll_handle.bounds_for_item`,
    /// which lags one frame behind a scroll and mis-maps the cursor while
    /// auto-scrolling.
    fn page_local(&self, position: Point<Pixels>, cx: &App) -> Option<(usize, f32, f32)> {
        let pages = &self.pdf_item.read(cx).pages;
        let viewport = self.scroll_handle.bounds();
        let scroll_y = f32::from(self.scroll_handle.offset().y);
        let scroll_x = f32::from(self.scroll_handle.offset().x);
        let px_pos = f32::from(position.x);
        let py_pos = f32::from(position.y);
        // Pages are centered when narrower than the viewport, and left-anchored
        // (horizontally scrollable) when wider — so the centering term is clamped
        // to >= 0 and the live horizontal scroll offset is added.
        let page_x = f32::from(viewport.origin.x)
            + scroll_x
            + (f32::from(viewport.size.width) - self.display_width).max(0.0) / 2.0;
        let mut page_y = f32::from(viewport.origin.y) + PAGE_GAP + scroll_y;

        for (ix, page) in pages.iter().enumerate() {
            let scale = self.display_width / page.width as f32;
            let page_h = page.height as f32 * scale;
            if px_pos >= page_x
                && px_pos <= page_x + self.display_width
                && py_pos >= page_y
                && py_pos <= page_y + page_h
            {
                return Some((ix, (px_pos - page_x) / scale, (py_pos - page_y) / scale));
            }
            page_y += page_h + PAGE_GAP;
        }
        None
    }

    /// Map a window point to a *global* glyph index (all pages concatenated in
    /// reading order), if it lands on a page with text.
    fn glyph_at(&self, position: Point<Pixels>, cx: &App) -> Option<usize> {
        let (page_ix, rx, ry) = self.page_local(position, cx)?;
        let pages = &self.pdf_item.read(cx).pages;
        let page = &pages[page_ix];
        if page.glyphs.is_empty() {
            return None;
        }
        let offset: usize = pages[..page_ix].iter().map(|p| p.glyphs.len()).sum();
        Some(offset + nearest_glyph(&page.glyphs, rx, ry))
    }

    /// The target URI of the hyperlink under `position`, if any.
    fn link_at(&self, position: Point<Pixels>, cx: &App) -> Option<String> {
        let (page_ix, rx, ry) = self.page_local(position, cx)?;
        self.pdf_item.read(cx).pages[page_ix]
            .links
            .iter()
            .find(|l| rx >= l.x && rx <= l.x + l.w && ry >= l.y && ry <= l.y + l.h)
            .map(|l| l.uri.clone())
    }

    fn on_mouse_down(&mut self, event: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        if event.button != MouseButton::Left {
            return;
        }
        // Clicking a hyperlink opens it instead of starting a selection.
        if let Some(uri) = self.link_at(event.position, cx) {
            cx.open_url(&uri);
            return;
        }
        // Take focus so the `cmd-c` binding (scoped to the PdfViewer key context)
        // dispatches to this view. While the find bar is open it owns keyboard
        // focus (so typing keeps editing the query); don't steal it back on a
        // document click, or the query input would silently stop responding.
        if !self.find.active {
            window.focus(&self.focus_handle, cx);
        }
        self.last_mouse = Some(event.position);
        if let Some(g) = self.glyph_at(event.position, cx) {
            self.selection = Some(Selection { anchor: g, head: g });
            self.dragging = true;
        } else {
            self.selection = None;
            self.dragging = false;
        }
        cx.notify();
    }

    fn on_mouse_move(&mut self, event: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        if !self.dragging {
            return;
        }
        self.last_mouse = Some(event.position);
        if let Some(g) = self.glyph_at(event.position, cx)
            && let Some(selection) = self.selection
        {
            self.selection = Some(Selection { head: g, ..selection });
            cx.notify();
        }
        self.ensure_autoscroll(cx);
    }

    fn on_mouse_up(&mut self, _: &MouseUpEvent, _: &mut Window, cx: &mut Context<Self>) {
        if self.dragging {
            self.dragging = false;
            self.autoscroll_task = None;
            cx.notify();
        }
    }

    /// Start the auto-scroll loop if not already running. It ticks ~60fps while
    /// dragging, scrolling whenever the cursor sits within `EDGE` of the viewport
    /// top/bottom and extending the selection to the cursor as content scrolls.
    fn ensure_autoscroll(&mut self, cx: &mut Context<Self>) {
        if self.autoscroll_task.is_some() {
            return;
        }
        self.autoscroll_task = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(16))
                    .await;
                let keep = this
                    .update(cx, |this, cx| this.autoscroll_tick(cx))
                    .unwrap_or(false);
                if !keep {
                    break;
                }
            }
            this.update(cx, |this, _| this.autoscroll_task = None).ok();
        }));
    }

    /// One auto-scroll step; returns whether the loop should keep running.
    fn autoscroll_tick(&mut self, cx: &mut Context<Self>) -> bool {
        if !self.dragging {
            return false;
        }
        let Some(mouse) = self.last_mouse else {
            return false;
        };
        const EDGE: f32 = 64.0;
        const SPEED: f32 = 16.0;
        let viewport = self.scroll_handle.bounds();
        let top = f32::from(viewport.origin.y);
        let bottom = top + f32::from(viewport.size.height);
        let my = f32::from(mouse.y);

        let delta = if my < top + EDGE {
            -SPEED
        } else if my > bottom - EDGE {
            SPEED
        } else {
            return false; // not at an edge; pause until the cursor returns
        };

        let before = self.scroll_handle.offset();
        let mut offset = before;
        offset.y -= px(delta); // scrolling down (delta > 0) moves content up
        self.scroll_handle.set_offset(offset);
        // set_offset clamps; if we're already at the top/bottom there's no
        // progress, so stop the loop instead of spinning at 60fps.
        if (f32::from(self.scroll_handle.offset().y) - f32::from(before.y)).abs() < 0.5 {
            return false;
        }

        if let Some(g) = self.glyph_at(mouse, cx)
            && let Some(selection) = self.selection
        {
            self.selection = Some(Selection { head: g, ..selection });
        }
        cx.notify();
        true
    }

    fn selected_text(&self, cx: &App) -> Option<String> {
        let (lo, hi) = self.selection?.bounds();
        let pages = &self.pdf_item.read(cx).pages;

        // Gather the selected glyphs in reading order across pages.
        let mut selected: Vec<&TextGlyph> = Vec::new();
        let mut offset = 0;
        for page in pages {
            let (start, end) = (offset, offset + page.glyphs.len());
            let a = lo.max(start);
            let b = (hi + 1).min(end);
            if a < b {
                selected.extend(page.glyphs[a - start..b - start].iter());
            }
            offset = end;
        }
        if selected.is_empty() {
            return None;
        }

        // Insert newlines at line/column/page breaks (same heuristic as the
        // highlight bars) so copied text isn't run together at line ends.
        let cmp = |a: f32, b: f32| a.partial_cmp(&b).unwrap_or(std::cmp::Ordering::Equal);
        let mut heights: Vec<f32> = selected.iter().map(|g| g.h).filter(|h| *h > 0.0).collect();
        heights.sort_by(|a, b| cmp(*a, *b));
        let tol = heights.get(heights.len() / 2).copied().unwrap_or(10.0) * 0.6;

        let mut text = String::new();
        let mut prev_x = f32::MIN;
        let mut line_y = selected[0].y;
        for (i, g) in selected.iter().enumerate() {
            if i > 0 && (g.x < prev_x - 0.5 || (g.h > 0.0 && (g.y - line_y).abs() > tol)) {
                text.push('\n');
                line_y = g.y;
            }
            text.push_str(&g.text);
            prev_x = g.x;
        }
        (!text.trim().is_empty()).then_some(text)
    }

    fn copy(&mut self, _: &CopySelection, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(text) = self.selected_text(cx) {
            cx.write_to_clipboard(ClipboardItem::new_string(text));
        }
    }

    fn deploy_find(&mut self, _: &DeployFind, window: &mut Window, cx: &mut Context<Self>) {
        self.find.active = true;
        // Re-pressing Cmd+F here only re-focuses; it does not select-all the query
        // for retyping (the input is a rendered label, not a real text field).
        if self.find.query.is_empty()
            && let Some(text) = self.selected_text(cx)
        {
            let seed: String = text.split('\n').next().unwrap_or("").chars().take(80).collect();
            if !seed.trim().is_empty() {
                self.find.query = seed;
            }
        }
        window.focus(&self.find_focus_handle, cx);
        self.update_matches(cx);
    }

    fn dismiss_find(&mut self, _: &DismissFind, window: &mut Window, cx: &mut Context<Self>) {
        self.find.active = false;
        self.find.matches.clear();
        self.find.current = None;
        window.focus(&self.focus_handle, cx);
        cx.notify();
    }

    /// Recompute matches for the current query/options, pick a current match, and
    /// scroll it into view.
    fn update_matches(&mut self, cx: &mut Context<Self>) {
        let matches = {
            let pages = &self.pdf_item.read(cx).pages;
            find_matches(
                pages.iter().map(|p| p.glyphs.as_slice()),
                &self.find.query,
                self.find.options,
            )
        };
        self.find.current = if matches.is_empty() {
            None
        } else {
            Some(self.first_match_from_viewport(&matches, cx))
        };
        self.find.matches = matches;
        if self.find.current.is_some() {
            self.scroll_current_match_into_view(cx);
        }
        cx.notify();
    }

    /// Content-space y (relative to the page column's top) of a match's first glyph.
    fn match_content_y(&self, m: &FindMatch, cx: &App) -> f32 {
        let pages = &self.pdf_item.read(cx).pages;
        let mut y = PAGE_GAP;
        for (ix, page) in pages.iter().enumerate() {
            let scale = self.display_width / page.width as f32;
            if ix == m.page_ix {
                let gy = page.glyphs.get(m.start_glyph).map(|g| g.y).unwrap_or(0.0);
                return y + gy * scale;
            }
            y += page.height as f32 * scale + PAGE_GAP;
        }
        y
    }

    /// First match at or after the current viewport top (fallback: first match).
    fn first_match_from_viewport(&self, matches: &[FindMatch], cx: &App) -> usize {
        let top = -f32::from(self.scroll_handle.offset().y);
        matches
            .iter()
            .position(|m| self.match_content_y(m, cx) >= top)
            .unwrap_or(0)
    }

    /// Scroll so the current match is vertically centered in the viewport.
    fn scroll_current_match_into_view(&mut self, cx: &mut Context<Self>) {
        let Some(ci) = self.find.current else { return };
        let Some(m) = self.find.matches.get(ci).copied() else {
            return;
        };
        let content_y = self.match_content_y(&m, cx);
        let viewport_h = f32::from(self.scroll_handle.bounds().size.height);
        let mut offset = self.scroll_handle.offset();
        offset.y = px(viewport_h / 2.0 - content_y);
        self.scroll_handle.set_offset(offset);
    }

    fn select_next_match(&mut self, _: &SelectNextMatch, _: &mut Window, cx: &mut Context<Self>) {
        self.step_match(1, cx);
    }

    fn select_prev_match(&mut self, _: &SelectPrevMatch, _: &mut Window, cx: &mut Context<Self>) {
        self.step_match(-1, cx);
    }

    /// Move the current match by `dir` (+1 next, -1 prev) with wraparound and
    /// scroll it into view.
    fn step_match(&mut self, dir: isize, cx: &mut Context<Self>) {
        let n = self.find.matches.len();
        if n == 0 {
            return;
        }
        let cur = self.find.current.unwrap_or(0) as isize;
        let next = (cur + dir).rem_euclid(n as isize) as usize;
        self.find.current = Some(next);
        self.scroll_current_match_into_view(cx);
        cx.notify();
    }

    fn toggle_case_sensitive(
        &mut self,
        _: &ToggleCaseSensitive,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.find.options.case_sensitive = !self.find.options.case_sensitive;
        self.update_matches(cx);
    }

    fn toggle_whole_word(&mut self, _: &ToggleWholeWord, _: &mut Window, cx: &mut Context<Self>) {
        self.find.options.whole_word = !self.find.options.whole_word;
        self.update_matches(cx);
    }

    /// Minimal text entry for the find bar: printable chars and backspace edit the
    /// query; Cmd+V pastes (first line). Enter/Escape/Cmd-G are keymap actions and
    /// are intentionally not handled here.
    fn on_find_key(&mut self, event: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        let ks = &event.keystroke;
        let mods = ks.modifiers;

        // Plain Backspace deletes one char. Modified variants (Cmd/Option/Ctrl +
        // Backspace = delete-word/line) are not implemented; ignore them rather
        // than treating them as a single delete.
        if ks.key == "backspace" {
            if !mods.platform && !mods.alt && !mods.control {
                self.find.query.pop();
                self.update_matches(cx);
            }
            return;
        }
        if mods.platform && ks.key == "v" {
            if let Some(text) = cx.read_from_clipboard().and_then(|c| c.text()) {
                self.find.query.push_str(text.split('\n').next().unwrap_or(""));
                self.update_matches(cx);
            }
            return;
        }
        if mods.platform || mods.control || mods.function {
            return;
        }
        if let Some(ch) = ks.key_char.as_ref()
            && ch.chars().all(|c| !c.is_control())
        {
            self.find.query.push_str(ch);
            self.update_matches(cx);
        }
    }
}

/// Merge selected glyphs into one rectangle per visual line, so the highlight
/// reads as a continuous selection bar rather than per-glyph boxes. Lines are
/// detected by the x coordinate resetting leftward in reading order. Vertical
/// extent comes from glyphs that have real height (spaces only widen the run).
/// Returns raster-space rects `(x, y, w, h)`.
fn selection_runs(glyphs: &[TextGlyph]) -> Vec<(f32, f32, f32, f32)> {
    let cmp = |a: f32, b: f32| a.partial_cmp(&b).unwrap_or(std::cmp::Ordering::Equal);
    let mut heights: Vec<f32> = glyphs.iter().map(|g| g.h).filter(|h| *h > 0.0).collect();
    heights.sort_by(|a, b| cmp(*a, *b));
    let tol = heights.get(heights.len() / 2).copied().unwrap_or(10.0) * 0.6;

    let mut runs = Vec::new();
    let (mut x0, mut y0, mut x1, mut y1) = (0.0f32, f32::MAX, 0.0f32, f32::MIN);
    let mut active = false;
    let mut has_height = false;
    let mut prev_x = f32::MIN;
    let mut line_y = 0.0f32;

    for g in glyphs {
        // New line when x resets leftward OR the row's y shifts — the latter
        // catches wrapping and the jump to the top of the next column.
        let new_line =
            active && (g.x < prev_x - 0.5 || (g.h > 0.0 && (g.y - line_y).abs() > tol));
        if new_line {
            if has_height {
                runs.push((x0, y0, x1 - x0, y1 - y0));
            }
            active = false;
        }
        if !active {
            (x0, x1, y0, y1, has_height, active) = (g.x, g.x + g.w, f32::MAX, f32::MIN, false, true);
            line_y = g.y;
        } else {
            x0 = x0.min(g.x);
            x1 = x1.max(g.x + g.w);
        }
        if g.h > 0.0 {
            y0 = y0.min(g.y);
            y1 = y1.max(g.y + g.h);
            has_height = true;
        }
        prev_x = g.x;
    }
    if active && has_height {
        runs.push((x0, y0, x1 - x0, y1 - y0));
    }
    // Pad each bar vertically so consecutive lines' highlights touch (fills the
    // line leading), matching a normal editor's contiguous selection.
    runs.into_iter()
        .map(|(x, y, w, h)| {
            let pad = h * 0.12;
            (x, y - pad, w, h + 2.0 * pad)
        })
        .collect()
}

/// Index of the glyph containing (rx, ry) in raster space, else nearest by center.
fn nearest_glyph(glyphs: &[TextGlyph], rx: f32, ry: f32) -> usize {
    if let Some(i) = glyphs
        .iter()
        .position(|g| rx >= g.x && rx <= g.x + g.w && ry >= g.y && ry <= g.y + g.h)
    {
        return i;
    }
    let mut best = 0;
    let mut best_dist = f32::MAX;
    for (i, g) in glyphs.iter().enumerate() {
        let dx = g.x + g.w / 2.0 - rx;
        let dy = g.y + g.h / 2.0 - ry;
        let dist = dx * dx + dy * dy;
        if dist < best_dist {
            best_dist = dist;
            best = i;
        }
    }
    best
}

/// Options for in-document find. `case_sensitive`/`whole_word` are user toggles.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct FindOptions {
    case_sensitive: bool,
    whole_word: bool,
}

/// A match within one page, as a page-local glyph range `[start_glyph, end_glyph)`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct FindMatch {
    page_ix: usize,
    start_glyph: usize,
    end_glyph: usize,
}

/// Concatenate a page's glyph texts in reading order, recording the byte offset
/// at which each glyph's text begins, so a match byte-range can be mapped back to
/// the glyphs that produced it.
fn page_search_text(glyphs: &[TextGlyph]) -> (String, Vec<usize>) {
    let mut text = String::new();
    let mut starts = Vec::with_capacity(glyphs.len());
    for g in glyphs {
        starts.push(text.len());
        text.push_str(&g.text);
    }
    (text, starts)
}

/// Whether two equal-length char windows are equal (case-folded unless
/// `case_sensitive`).
fn window_eq(a: &[char], b: &[char], case_sensitive: bool) -> bool {
    a.len() == b.len()
        && a.iter().zip(b).all(|(&x, &y)| {
            if case_sensitive {
                x == y
            } else {
                x == y || x.to_lowercase().eq(y.to_lowercase())
            }
        })
}

/// Map a match byte range `[b0, b1)` in the page text to a glyph range
/// `[start_glyph, end_glyph)` (end exclusive, always non-empty).
fn byte_range_to_glyphs(starts: &[usize], b0: usize, b1: usize) -> (usize, usize) {
    let start_glyph = starts.partition_point(|&s| s <= b0).saturating_sub(1);
    let end_glyph = starts.partition_point(|&s| s < b1).max(start_glyph + 1);
    (start_glyph, end_glyph)
}

/// Whether the `len`-char window starting at `start` is bounded by word
/// separators (non-alphanumeric chars or the string edges) on both sides.
fn is_word_bounded(chars: &[char], start: usize, len: usize) -> bool {
    let before_ok = start == 0 || !chars[start - 1].is_alphanumeric();
    let after = start + len;
    let after_ok = after >= chars.len() || !chars[after].is_alphanumeric();
    before_ok && after_ok
}

/// Find all non-overlapping matches of `query` within one page's glyphs.
/// Returns page-local glyph ranges `(start_glyph, end_glyph)` (end exclusive).
/// Matching works in char space so case folding can't desync byte offsets.
fn find_in_glyphs(glyphs: &[TextGlyph], query: &str, opts: FindOptions) -> Vec<(usize, usize)> {
    if query.is_empty() || glyphs.is_empty() {
        return Vec::new();
    }
    let (text, starts) = page_search_text(glyphs);
    let chars: Vec<char> = text.chars().collect();
    let char_bytes: Vec<usize> = text.char_indices().map(|(b, _)| b).collect();
    let q: Vec<char> = query.chars().collect();
    let (n, m) = (chars.len(), q.len());
    let mut out = Vec::new();
    let mut i = 0;
    while i + m <= n {
        if window_eq(&chars[i..i + m], &q, opts.case_sensitive)
            && (!opts.whole_word || is_word_bounded(&chars, i, m))
        {
            let b0 = char_bytes[i];
            let b1 = if i + m < n { char_bytes[i + m] } else { text.len() };
            out.push(byte_range_to_glyphs(&starts, b0, b1));
            i += m;
        } else {
            i += 1;
        }
    }
    out
}

/// Find matches across all pages. `pages` yields each page's glyph slice in page
/// order; matches are returned ordered by `(page_ix, start_glyph)`.
fn find_matches<'a>(
    pages: impl IntoIterator<Item = &'a [TextGlyph]>,
    query: &str,
    opts: FindOptions,
) -> Vec<FindMatch> {
    let mut out = Vec::new();
    for (page_ix, glyphs) in pages.into_iter().enumerate() {
        for (start_glyph, end_glyph) in find_in_glyphs(glyphs, query, opts) {
            out.push(FindMatch { page_ix, start_glyph, end_glyph });
        }
    }
    out
}

impl EventEmitter<()> for PdfView {}

impl Focusable for PdfView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Item for PdfView {
    type Event = ();

    fn to_item_events(_: &Self::Event, _: &mut dyn FnMut(workspace::item::ItemEvent)) {}

    fn tab_content_text(&self, _detail: usize, cx: &App) -> SharedString {
        self.pdf_item
            .read(cx)
            .abs_path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "PDF".to_owned())
            .into()
    }

    fn for_each_project_item(
        &self,
        cx: &App,
        f: &mut dyn FnMut(gpui::EntityId, &dyn project::ProjectItem),
    ) {
        f(self.pdf_item.entity_id(), self.pdf_item.read(cx))
    }

    fn clone_on_split(
        &self,
        _workspace_id: Option<workspace::WorkspaceId>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Option<Entity<Self>>> {
        let pdf_item = self.pdf_item.clone();
        let project = self.project.clone();
        Task::ready(Some(cx.new(|cx| Self::new(pdf_item, project, cx))))
    }

    fn buffer_kind(&self, _: &App) -> workspace::item::ItemBufferKind {
        workspace::item::ItemBufferKind::Singleton
    }
}

impl ProjectItem for PdfView {
    type Item = PdfItem;

    fn for_project_item(
        project: Entity<Project>,
        _pane: Option<&Pane>,
        item: Entity<Self::Item>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        Self::new(item, project, cx)
    }
}

impl Render for PdfView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let pages = self.pdf_item.read(cx).pages.clone();
        let selection = self.selection;
        let find_matches = if self.find.active {
            self.find.matches.clone()
        } else {
            Vec::new()
        };
        let find_current = self.find.current;
        let display_width = self.display_width;
        // Column width = max(viewport, page) so it grows past the viewport when
        // zoomed (→ horizontal scroll); vertical-only padding keeps hit-test x math
        // exact. viewport width comes from the prior frame's scroll bounds.
        let viewport_width = f32::from(self.scroll_handle.bounds().size.width);
        let content_width = display_width.max(viewport_width);
        // Global glyph offset of each page (for mapping the cross-page selection
        // range onto each page's local glyph slice).
        let mut page_offsets = Vec::with_capacity(pages.len());
        let mut acc = 0;
        for page in &pages {
            page_offsets.push(acc);
            acc += page.glyphs.len();
        }

        // The page column is the scroll container's direct child (no wrapper) so
        // it can grow wider than the viewport when zoomed → horizontal scroll.
        let pages_column = div()
            .flex()
            .flex_col()
            .items_center()
            .flex_shrink_0()
            .w(px(content_width))
            .gap_4()
            .py_4()
            .children(pages.iter().enumerate().map(|(ix, page)| {
                        let scale = display_width / page.width as f32;
                        let start = page_offsets[ix];
                        let end = start + page.glyphs.len();
                        let highlights = selection
                            .and_then(|s| {
                                let (lo, hi) = s.bounds();
                                let a = lo.max(start);
                                let b = (hi + 1).min(end);
                                (a < b).then(|| &page.glyphs[a - start..b - start])
                            })
                            .map(|slice| {
                                selection_runs(slice)
                                    .into_iter()
                                    .map(|(x, y, w, h)| {
                                        div()
                                            .absolute()
                                            .left(px(x * scale))
                                            .top(px(y * scale))
                                            .w(px(w * scale))
                                            .h(px(h * scale))
                                            .rounded_sm()
                                            .bg(rgba(0x3b82f659))
                                    })
                                    .collect::<Vec<_>>()
                            })
                            .unwrap_or_default();

                        // Transparent overlays over link rects, just to show the
                        // pointer cursor on hover. Clicks are handled by the
                        // scroll container's mouse-down (link_at), which these
                        // don't intercept.
                        let link_overlays = page.links.iter().map(|link| {
                            div()
                                .absolute()
                                .left(px(link.x * scale))
                                .top(px(link.y * scale))
                                .w(px(link.w * scale))
                                .h(px(link.h * scale))
                                .cursor_pointer()
                        });

                        let find_matches = &find_matches;
                        let match_highlights = find_matches
                            .iter()
                            .enumerate()
                            .filter(|(_, m)| m.page_ix == ix)
                            .flat_map(|(mi, m)| {
                                let end = m.end_glyph.min(page.glyphs.len());
                                let slice = &page.glyphs[m.start_glyph.min(end)..end];
                                let strong = find_current == Some(mi);
                                selection_runs(slice).into_iter().map(move |(x, y, w, h)| {
                                    div()
                                        .absolute()
                                        .left(px(x * scale))
                                        .top(px(y * scale))
                                        .w(px(w * scale))
                                        .h(px(h * scale))
                                        .rounded_sm()
                                        .bg(if strong { rgba(0xf59e0bcc) } else { rgba(0xfde68a80) })
                                })
                            })
                            .collect::<Vec<_>>();

                        div()
                            .id(("pdf-page", ix))
                            .relative()
                            .w(px(display_width))
                            .h(px(page.height as f32 * scale))
                            .shadow_md()
                            .child(img(page.image.clone()).size_full())
                            .children(highlights)
                            .children(link_overlays)
                            .children(match_highlights)
            }));

        let controls = h_flex()
            .occlude() // don't let toolbar clicks fall through to the document
            .absolute()
            .top_2()
            .right_2()
            .gap_1()
            .p_1()
            .rounded_md()
            .border_1()
            .border_color(cx.theme().colors().border)
            .bg(cx.theme().colors().elevated_surface_background)
            .shadow_md()
            .child(
                IconButton::new("pdf-zoom-out", IconName::Dash)
                    .on_click(cx.listener(|this, _, window, cx| this.zoom_out(&ZoomOut, window, cx))),
            )
            .child(
                IconButton::new("pdf-zoom-in", IconName::Plus)
                    .on_click(cx.listener(|this, _, window, cx| this.zoom_in(&ZoomIn, window, cx))),
            )
            .child(
                Button::new("pdf-fit-width", "Fit width").on_click(
                    cx.listener(|this, _, window, cx| this.fit_width(&FitWidth, window, cx)),
                ),
            )
            .child(
                Button::new("pdf-fit-page", "Fit page")
                    .on_click(cx.listener(|this, _, window, cx| this.fit_page(&FitPage, window, cx))),
            );

        let find_bar = self.find.active.then(|| {
            let counter = if self.find.query.is_empty() {
                String::new()
            } else if self.find.matches.is_empty() {
                "No results".to_owned()
            } else {
                let pos = self.find.current.map(|c| c + 1).unwrap_or(0);
                format!("{} of {}", pos, self.find.matches.len())
            };
            let query_label = if self.find.query.is_empty() {
                "Find".to_owned()
            } else {
                self.find.query.clone()
            };
            let case_on = self.find.options.case_sensitive;
            let word_on = self.find.options.whole_word;

            h_flex()
                .occlude()
                .track_focus(&self.find_focus_handle)
                .key_context("PdfFind")
                .on_action(cx.listener(Self::dismiss_find))
                .on_action(cx.listener(Self::select_next_match))
                .on_action(cx.listener(Self::select_prev_match))
                .on_key_down(cx.listener(Self::on_find_key))
                .absolute()
                .top_2()
                .left_2()
                .gap_1()
                .p_1()
                .rounded_md()
                .border_1()
                .border_color(cx.theme().colors().border)
                .bg(cx.theme().colors().elevated_surface_background)
                .shadow_md()
                .child(
                    div()
                        .min_w(px(160.0))
                        .px_2()
                        .rounded_sm()
                        .bg(cx.theme().colors().editor_background)
                        .child(Label::new(query_label).color(if self.find.query.is_empty() {
                            Color::Muted
                        } else {
                            Color::Default
                        })),
                )
                .child(
                    IconButton::new("pdf-find-case", IconName::CaseSensitive)
                        .toggle_state(case_on)
                        .tooltip(Tooltip::text("Match case"))
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.toggle_case_sensitive(&ToggleCaseSensitive, window, cx)
                        })),
                )
                .child(
                    IconButton::new("pdf-find-word", IconName::WholeWord)
                        .toggle_state(word_on)
                        .tooltip(Tooltip::text("Whole word"))
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.toggle_whole_word(&ToggleWholeWord, window, cx)
                        })),
                )
                .child(Label::new(counter).color(Color::Muted))
                .child(
                    IconButton::new("pdf-find-prev", IconName::ChevronUp)
                        .tooltip(Tooltip::text("Previous match"))
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.select_prev_match(&SelectPrevMatch, window, cx)
                        })),
                )
                .child(
                    IconButton::new("pdf-find-next", IconName::ChevronDown)
                        .tooltip(Tooltip::text("Next match"))
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.select_next_match(&SelectNextMatch, window, cx)
                        })),
                )
                .child(
                    IconButton::new("pdf-find-close", IconName::Close)
                        .tooltip(Tooltip::text("Close"))
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.dismiss_find(&DismissFind, window, cx)
                        })),
                )
        });

        div()
            .track_focus(&self.focus_handle)
            .key_context("PdfViewer")
            .relative()
            .on_action(cx.listener(Self::copy))
            .on_action(cx.listener(Self::zoom_in))
            .on_action(cx.listener(Self::zoom_out))
            .on_action(cx.listener(Self::fit_width))
            .on_action(cx.listener(Self::fit_page))
            .on_action(cx.listener(Self::deploy_find))
            .size_full()
            .bg(cx.theme().colors().editor_background)
            .child(
                div()
                    .id("pdf-scroll")
                    .track_scroll(&self.scroll_handle)
                    .size_full()
                    .overflow_scroll()
                    .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
                    .on_mouse_down(MouseButton::Right, cx.listener(Self::deploy_context_menu))
                    .on_mouse_move(cx.listener(Self::on_mouse_move))
                    .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
                    .child(pages_column),
            )
            .child(controls)
            .children(find_bar)
            .children(self.context_menu.as_ref().map(|(menu, position, _)| {
                deferred(
                    anchored()
                        .position(*position)
                        .anchor(gpui::Anchor::TopLeft)
                        .child(menu.clone()),
                )
                .with_priority(1)
            }))
    }
}

/// Register the PDF viewer so `.pdf` files open in it. Call after the other
/// project-item registrations so this opener wins for PDFs.
pub fn init(cx: &mut App) {
    workspace::register_project_item::<PdfView>(cx);
}

#[cfg(test)]
mod tests {
    use super::*;
    use fs::FakeFs;
    use gpui::TestAppContext;
    use serde_json::json;
    use settings::SettingsStore;
    use std::path::{Path, PathBuf};
    use util::rel_path::rel_path;

    fn test_data(filename: &str) -> PathBuf {
        let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        path.push("test_data");
        path.push(filename);
        path
    }

    fn init_test(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
        });
    }

    #[test]
    fn test_is_pdf_path() {
        assert!(is_pdf_path(Path::new("/x/report.pdf")));
        assert!(is_pdf_path(Path::new("/x/REPORT.PDF")));
        assert!(is_pdf_path(Path::new("/x/a.b.Pdf")));
        assert!(!is_pdf_path(Path::new("/x/main.rs")));
        assert!(!is_pdf_path(Path::new("/x/noext")));
        assert!(!is_pdf_path(Path::new("/x/pdf"))); // bare name, no extension
    }

    #[test]
    fn test_render_pdf_rasterizes_all_pages() {
        let pages = render_pdf(&test_data("sample.pdf")).expect("sample PDF should rasterize");
        assert_eq!(pages.len(), 2, "fixture has two pages");
        for page in &pages {
            assert!(page.width > 0 && page.height > 0, "page has real dimensions");
        }
    }

    /// Build a `TextGlyph` with the given text; geometry is irrelevant to the
    /// search engine (only glyph counts/order matter for index mapping).
    fn tg(text: &str) -> TextGlyph {
        TextGlyph { text: text.to_string(), x: 0.0, y: 0.0, w: 1.0, h: 1.0 }
    }

    #[test]
    fn test_page_search_text_offsets() {
        let glyphs = vec![tg("He"), tg("llo"), tg(" "), tg("wörld")];
        let (text, starts) = page_search_text(&glyphs);
        assert_eq!(text, "Hello wörld");
        assert_eq!(starts, vec![0, 2, 5, 6]);
    }

    #[test]
    fn test_find_case_insensitive_multiple() {
        let glyphs = vec![tg("Hello "), tg("hello "), tg("HELLO")];
        let m = find_in_glyphs(&glyphs, "hello", FindOptions::default());
        assert_eq!(m.len(), 3, "case-insensitive default matches all three");
    }

    #[test]
    fn test_find_maps_match_across_glyph_boundary() {
        let glyphs = vec![tg("ab"), tg("cd"), tg("ef")];
        let m = find_in_glyphs(&glyphs, "bcd", FindOptions::default());
        assert_eq!(m, vec![(0, 2)], "covers glyph 0 and 1, exclusive end");
    }

    #[test]
    fn test_find_empty_query_and_no_match() {
        let glyphs = vec![tg("abc")];
        assert!(find_in_glyphs(&glyphs, "", FindOptions::default()).is_empty());
        assert!(find_in_glyphs(&glyphs, "zzz", FindOptions::default()).is_empty());
    }

    #[test]
    fn test_find_case_sensitive() {
        let glyphs = vec![tg("Hello "), tg("hello")];
        let opts = FindOptions { case_sensitive: true, whole_word: false };
        let m = find_in_glyphs(&glyphs, "hello", opts);
        assert_eq!(m.len(), 1, "only the lowercase occurrence");
    }

    #[test]
    fn test_find_whole_word() {
        let glyphs = vec![tg("cat "), tg("category "), tg("cat")];
        let opts = FindOptions { case_sensitive: false, whole_word: true };
        let m = find_in_glyphs(&glyphs, "cat", opts);
        assert_eq!(m.len(), 2, "standalone 'cat' x2, not the 'cat' inside 'category'");

        let loose = find_in_glyphs(&glyphs, "cat", FindOptions::default());
        assert_eq!(loose.len(), 3);
    }

    #[test]
    fn test_find_whole_word_boundary_in_next_glyph() {
        // The space that forms the trailing word boundary lives in a *different*
        // glyph than the word, confirming whole-word works over concatenated page
        // text rather than per-glyph.
        let glyphs = vec![tg("cat"), tg(" end")];
        let opts = FindOptions { case_sensitive: false, whole_word: true };
        let m = find_in_glyphs(&glyphs, "cat", opts);
        assert_eq!(m.len(), 1, "'cat' is whole-word bounded by the space in glyph 1");
    }

    #[test]
    fn test_find_adjacent_matches_are_non_overlapping() {
        // "aa" over "aaaa" yields non-overlapping matches at 0-1 and 2-3, not three
        // overlapping ones (matches standard find behavior).
        let glyphs = vec![tg("a"), tg("a"), tg("a"), tg("a")];
        let m = find_in_glyphs(&glyphs, "aa", FindOptions::default());
        assert_eq!(m, vec![(0, 2), (2, 4)]);
    }

    #[test]
    fn test_find_matches_across_pages() {
        let page0 = vec![tg("alpha "), tg("beta")];
        let page1 = vec![tg("beta "), tg("beta")];
        let pages = [page0.as_slice(), page1.as_slice()];
        let matches = find_matches(pages.iter().copied(), "beta", FindOptions::default());
        assert_eq!(matches.len(), 3);
        assert_eq!(matches[0].page_ix, 0);
        assert_eq!(matches[1].page_ix, 1);
        assert_eq!(matches[2].page_ix, 1);
        assert!(matches[1].start_glyph <= matches[2].start_glyph);
    }

    /// Integration: against a real `Project`, `try_open` claims `.pdf` paths and
    /// declines others. Checks only the (synchronous) routing decision, so it
    /// needs neither a real file nor a rasterizer — the rasterization task is dropped.
    #[gpui::test]
    async fn test_try_open_routes_only_pdfs(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree("/root", json!({ "doc.pdf": "", "notes.txt": "" }))
            .await;
        let project = Project::test(fs, ["/root".as_ref()], cx).await;
        let worktree_id =
            cx.update(|cx| project.read(cx).worktrees(cx).next().unwrap().read(cx).id());

        let pdf_path = ProjectPath {
            worktree_id,
            path: rel_path("doc.pdf").into(),
        };
        let txt_path = ProjectPath {
            worktree_id,
            path: rel_path("notes.txt").into(),
        };

        cx.update(|cx| {
            assert!(
                <PdfItem as project::ProjectItem>::try_open(&project, &pdf_path, cx).is_some(),
                "a .pdf path should be claimed by the PDF viewer"
            );
            assert!(
                <PdfItem as project::ProjectItem>::try_open(&project, &txt_path, cx).is_none(),
                "a non-pdf path should be declined"
            );
        });
    }
}
