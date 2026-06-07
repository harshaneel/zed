
use gpui::{
    App, Context, Entity, EventEmitter, FocusHandle, Focusable,
    IntoElement, MouseButton,
    ParentElement, Pixels, Point, Render, ScrollHandle, SharedString, Styled,
    Subscription, Task, Window, anchored, deferred, div, img, px, rgba,
};
use project::Project;
use ui::ContextMenu;
use ui::Tooltip;
use ui::prelude::*;
use workspace::{
    Pane,
    item::{Item, ProjectItem},
};

use crate::{
    DEFAULT_DISPLAY_WIDTH, DismissFind, FitPage, FitWidth,
    SelectNextMatch, SelectPrevMatch, ToggleCaseSensitive, ToggleWholeWord, ZoomIn, ZoomOut,
};
use crate::document::PdfItem;
use crate::find::{FindMatch, FindOptions};
use crate::glyph::selection_runs;

/// An active text selection as an inclusive range of *global* glyph indices
/// (glyphs of all pages concatenated in reading order), so a selection can span
/// pages.
#[derive(Clone, Copy)]
pub(crate) struct Selection {
    pub(crate) anchor: usize,
    pub(crate) head: usize,
}

impl Selection {
    pub(crate) fn bounds(&self) -> (usize, usize) {
        (self.anchor.min(self.head), self.anchor.max(self.head))
    }
}

/// In-document find state: the query, options, the current match list (page-local
/// glyph ranges), and which match is selected.
#[derive(Default)]
pub(crate) struct FindState {
    pub(crate) active: bool,
    pub(crate) query: String,
    pub(crate) options: FindOptions,
    pub(crate) matches: Vec<FindMatch>,
    pub(crate) current: Option<usize>,
}

/// The pane view over a [`PdfItem`].
pub struct PdfView {
    pub(crate) pdf_item: Entity<PdfItem>,
    pub(crate) project: Entity<Project>,
    pub(crate) focus_handle: FocusHandle,
    /// Tracks the scroll container; gpui records each page child's window bounds.
    pub(crate) scroll_handle: ScrollHandle,
    pub(crate) selection: Option<Selection>,
    pub(crate) dragging: bool,
    /// Last cursor position (window space) while dragging, for auto-scroll.
    pub(crate) last_mouse: Option<Point<Pixels>>,
    /// Repeating task that auto-scrolls while the cursor is held at an edge.
    pub(crate) autoscroll_task: Option<Task<()>>,
    /// On-screen page width in px; the zoom level (pages scale to this).
    pub(crate) display_width: f32,
    /// Open right-click menu: (menu, click position, dismiss subscription).
    pub(crate) context_menu: Option<(Entity<ContextMenu>, Point<Pixels>, Subscription)>,
    /// In-document find bar state.
    pub(crate) find: FindState,
    /// Focus target for the find bar's text input.
    pub(crate) find_focus_handle: FocusHandle,
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
