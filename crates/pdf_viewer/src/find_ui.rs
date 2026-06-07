use gpui::{App, Context, KeyDownEvent, Window, px};

use crate::{
    DeployFind, DismissFind, PAGE_GAP, SelectNextMatch, SelectPrevMatch, ToggleCaseSensitive,
    ToggleWholeWord,
};
use crate::find::{FindMatch, find_matches};
use crate::view::PdfView;

impl PdfView {
    pub(crate) fn deploy_find(
        &mut self,
        _: &DeployFind,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
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

    pub(crate) fn dismiss_find(
        &mut self,
        _: &DismissFind,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.find.active = false;
        self.find.matches.clear();
        self.find.current = None;
        window.focus(&self.focus_handle, cx);
        cx.notify();
    }

    /// Recompute matches for the current query/options, pick a current match, and
    /// scroll it into view.
    pub(crate) fn update_matches(&mut self, cx: &mut Context<Self>) {
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
    pub(crate) fn match_content_y(&self, m: &FindMatch, cx: &App) -> f32 {
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
    pub(crate) fn first_match_from_viewport(&self, matches: &[FindMatch], cx: &App) -> usize {
        let top = -f32::from(self.scroll_handle.offset().y);
        matches
            .iter()
            .position(|m| self.match_content_y(m, cx) >= top)
            .unwrap_or(0)
    }

    /// Scroll so the current match is vertically centered in the viewport.
    pub(crate) fn scroll_current_match_into_view(&mut self, cx: &mut Context<Self>) {
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

    pub(crate) fn select_next_match(
        &mut self,
        _: &SelectNextMatch,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.step_match(1, cx);
    }

    pub(crate) fn select_prev_match(
        &mut self,
        _: &SelectPrevMatch,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.step_match(-1, cx);
    }

    /// Move the current match by `dir` (+1 next, -1 prev) with wraparound and
    /// scroll it into view.
    pub(crate) fn step_match(&mut self, dir: isize, cx: &mut Context<Self>) {
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

    pub(crate) fn toggle_case_sensitive(
        &mut self,
        _: &ToggleCaseSensitive,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.find.options.case_sensitive = !self.find.options.case_sensitive;
        self.update_matches(cx);
    }

    pub(crate) fn toggle_whole_word(
        &mut self,
        _: &ToggleWholeWord,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.find.options.whole_word = !self.find.options.whole_word;
        self.update_matches(cx);
    }

    /// Minimal text entry for the find bar: printable chars and backspace edit the
    /// query; Cmd+V pastes (first line). Enter/Escape/Cmd-G are keymap actions and
    /// are intentionally not handled here.
    pub(crate) fn on_find_key(
        &mut self,
        event: &KeyDownEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
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
