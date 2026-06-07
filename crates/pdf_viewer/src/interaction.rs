use std::time::Duration;

use gpui::{
    App, ClipboardItem, Context, DismissEvent, Focusable, MouseButton, MouseDownEvent,
    MouseMoveEvent, MouseUpEvent, Pixels, Point, Window, px,
};
use ui::ContextMenu;

use crate::{
    CopySelection, FitPage, FitWidth, ZoomIn, ZoomOut,
    MIN_DISPLAY_WIDTH, MAX_DISPLAY_WIDTH, PAGE_GAP, ZOOM_STEP,
};
use crate::glyph::{TextGlyph, nearest_glyph};
use crate::view::{PdfView, Selection};

impl PdfView {
    /// Open a right-click menu (Copy) at the cursor.
    pub(crate) fn deploy_context_menu(
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

    pub(crate) fn set_display_width(&mut self, width: f32, cx: &mut Context<Self>) {
        self.display_width = width.clamp(MIN_DISPLAY_WIDTH, MAX_DISPLAY_WIDTH);
        // Zoom changes page scale; keep the current find match centered.
        if self.find.current.is_some() {
            self.scroll_current_match_into_view(cx);
        }
        cx.notify();
    }

    pub(crate) fn zoom_in(&mut self, _: &ZoomIn, _: &mut Window, cx: &mut Context<Self>) {
        self.set_display_width(self.display_width * ZOOM_STEP, cx);
    }

    pub(crate) fn zoom_out(&mut self, _: &ZoomOut, _: &mut Window, cx: &mut Context<Self>) {
        self.set_display_width(self.display_width / ZOOM_STEP, cx);
    }

    /// Scale pages so one page's width fills the viewport.
    pub(crate) fn fit_width(&mut self, _: &FitWidth, _: &mut Window, cx: &mut Context<Self>) {
        let viewport_width = f32::from(self.scroll_handle.bounds().size.width);
        if viewport_width > 0.0 {
            self.set_display_width(viewport_width - 2.0 * PAGE_GAP, cx);
        }
    }

    /// Scale pages so a whole (first) page fits within the viewport.
    pub(crate) fn fit_page(&mut self, _: &FitPage, _: &mut Window, cx: &mut Context<Self>) {
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
    pub(crate) fn page_local(&self, position: Point<Pixels>, cx: &App) -> Option<(usize, f32, f32)> {
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
    pub(crate) fn glyph_at(&self, position: Point<Pixels>, cx: &App) -> Option<usize> {
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
    pub(crate) fn link_at(&self, position: Point<Pixels>, cx: &App) -> Option<String> {
        let (page_ix, rx, ry) = self.page_local(position, cx)?;
        self.pdf_item.read(cx).pages[page_ix]
            .links
            .iter()
            .find(|l| rx >= l.x && rx <= l.x + l.w && ry >= l.y && ry <= l.y + l.h)
            .map(|l| l.uri.clone())
    }

    pub(crate) fn on_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
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

    pub(crate) fn on_mouse_move(
        &mut self,
        event: &MouseMoveEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
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

    pub(crate) fn on_mouse_up(
        &mut self,
        _: &MouseUpEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.dragging {
            self.dragging = false;
            self.autoscroll_task = None;
            cx.notify();
        }
    }

    /// Start the auto-scroll loop if not already running. It ticks ~60fps while
    /// dragging, scrolling whenever the cursor sits within `EDGE` of the viewport
    /// top/bottom and extending the selection to the cursor as content scrolls.
    pub(crate) fn ensure_autoscroll(&mut self, cx: &mut Context<Self>) {
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
    pub(crate) fn autoscroll_tick(&mut self, cx: &mut Context<Self>) -> bool {
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

    pub(crate) fn selected_text(&self, cx: &App) -> Option<String> {
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

    pub(crate) fn copy(&mut self, _: &CopySelection, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(text) = self.selected_text(cx) {
            cx.write_to_clipboard(ClipboardItem::new_string(text));
        }
    }
}
