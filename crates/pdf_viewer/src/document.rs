use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context as _, Result, anyhow};
use gpui::{App, AppContext, RenderImage, Task};
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

use crate::RASTER_WIDTH;
use crate::glyph::{TextGlyph, sort_reading_order};

/// A hyperlink annotation: clickable rectangle (raster-pixel space) + target URI.
#[derive(Clone)]
pub(crate) struct Link {
    pub(crate) x: f32,
    pub(crate) y: f32,
    pub(crate) w: f32,
    pub(crate) h: f32,
    pub(crate) uri: String,
}

/// A single rasterized page plus its selectable text runs (reading order) and
/// hyperlink annotations.
pub(crate) struct PdfPage {
    pub(crate) image: Arc<RenderImage>,
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) glyphs: Vec<TextGlyph>,
    pub(crate) links: Vec<Link>,
}

/// Project-level item: holds the rasterized pages for one PDF file.
pub struct PdfItem {
    pub(crate) abs_path: PathBuf,
    pub(crate) project_path: ProjectPath,
    pub(crate) entry_id: Option<ProjectEntryId>,
    pub(crate) pages: Vec<Arc<PdfPage>>,
}

impl project::ProjectItem for PdfItem {
    fn try_open(
        project: &gpui::Entity<Project>,
        path: &ProjectPath,
        cx: &mut App,
    ) -> Option<Task<Result<gpui::Entity<Self>>>> {
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
pub(crate) fn is_pdf_path(path: &std::path::Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("pdf"))
}

/// Rasterize every page of the PDF to a BGRA `RenderImage` with pure-Rust hayro.
/// Blocking; run on a background thread.
pub(crate) fn render_pdf(path: &std::path::Path) -> Result<Vec<Arc<PdfPage>>> {
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
pub(crate) fn extract_links(page: &Page, scale: f32) -> Vec<Link> {
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
pub(crate) fn extract_glyphs(page: &Page, scale: f32) -> Vec<TextGlyph> {
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
