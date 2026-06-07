use crate::glyph::TextGlyph;

/// Options for in-document find. `case_sensitive`/`whole_word` are user toggles.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct FindOptions {
    pub(crate) case_sensitive: bool,
    pub(crate) whole_word: bool,
}

/// A match within one page, as a page-local glyph range `[start_glyph, end_glyph)`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct FindMatch {
    pub(crate) page_ix: usize,
    pub(crate) start_glyph: usize,
    pub(crate) end_glyph: usize,
}

/// Concatenate a page's glyph texts in reading order, recording the byte offset
/// at which each glyph's text begins, so a match byte-range can be mapped back to
/// the glyphs that produced it.
pub(crate) fn page_search_text(glyphs: &[TextGlyph]) -> (String, Vec<usize>) {
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
pub(crate) fn window_eq(a: &[char], b: &[char], case_sensitive: bool) -> bool {
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
pub(crate) fn byte_range_to_glyphs(starts: &[usize], b0: usize, b1: usize) -> (usize, usize) {
    let start_glyph = starts.partition_point(|&s| s <= b0).saturating_sub(1);
    let end_glyph = starts.partition_point(|&s| s < b1).max(start_glyph + 1);
    (start_glyph, end_glyph)
}

/// Whether the `len`-char window starting at `start` is bounded by word
/// separators (non-alphanumeric chars or the string edges) on both sides.
pub(crate) fn is_word_bounded(chars: &[char], start: usize, len: usize) -> bool {
    let before_ok = start == 0 || !chars[start - 1].is_alphanumeric();
    let after = start + len;
    let after_ok = after >= chars.len() || !chars[after].is_alphanumeric();
    before_ok && after_ok
}

/// Find all non-overlapping matches of `query` within one page's glyphs.
/// Returns page-local glyph ranges `(start_glyph, end_glyph)` (end exclusive).
/// Matching works in char space so case folding can't desync byte offsets.
pub(crate) fn find_in_glyphs(
    glyphs: &[TextGlyph],
    query: &str,
    opts: FindOptions,
) -> Vec<(usize, usize)> {
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
pub(crate) fn find_matches<'a>(
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
