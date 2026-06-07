/// A run of text and its bounding box in raster-pixel space (top-left origin).
#[derive(Clone)]
pub(crate) struct TextGlyph {
    pub(crate) text: String,
    pub(crate) x: f32,
    pub(crate) y: f32,
    pub(crate) w: f32,
    pub(crate) h: f32,
}

/// Order glyphs for selection/copy. PDF content streams draw glyphs in arbitrary
/// order, so we recover reading order. Two-column pages are handled region-aware:
/// rows that span the gutter (titles, author blocks, full-width captions) stay
/// row-major in place, while the genuinely two-column bands between them are
/// ordered column-major (left column top-to-bottom, then right). This handles the
/// common paper layout of a full-width title above a 2-column body. Single-column
/// pages fall back to plain top-to-bottom, left-to-right order.
pub(crate) fn sort_reading_order(glyphs: Vec<TextGlyph>) -> Vec<TextGlyph> {
    if glyphs.is_empty() {
        return glyphs;
    }
    let page_width = glyphs.iter().map(|g| g.x + g.w).fold(0.0_f32, f32::max);
    let Some(split) = detect_column_split(&glyphs, page_width) else {
        return insert_word_spaces(cluster_rows(glyphs).into_iter().flatten().collect());
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
    insert_word_spaces(out)
}

/// PDFs commonly position inter-word spacing instead of drawing a space glyph, so
/// the captured glyphs run words together ("socialgraph"). Walk the reading-order
/// glyphs and re-insert a space wherever two consecutive glyphs sit on the same
/// line (similar baseline, x advancing) with a horizontal gap wide enough to be a
/// space — roughly a fifth of the em (glyph cell height ≈ em). Line and column
/// breaks reset x or y, so no space is inserted across them.
fn insert_word_spaces(glyphs: Vec<TextGlyph>) -> Vec<TextGlyph> {
    if glyphs.len() < 2 {
        return glyphs;
    }
    let cmp = |a: f32, b: f32| a.partial_cmp(&b).unwrap_or(std::cmp::Ordering::Equal);
    let mut heights: Vec<f32> = glyphs.iter().map(|g| g.h).filter(|h| *h > 0.0).collect();
    heights.sort_by(|a, b| cmp(*a, *b));
    let em = heights.get(heights.len() / 2).copied().unwrap_or(10.0);
    let tol = em * 0.6; // same-line baseline tolerance (matches selected_text)

    let mut out: Vec<TextGlyph> = Vec::with_capacity(glyphs.len());
    for g in glyphs {
        if let Some(prev) = out.last() {
            let same_line = (g.y - prev.y).abs() <= tol;
            let gap = g.x - (prev.x + prev.w);
            let prev_space = prev.text.chars().all(char::is_whitespace);
            let g_space = g.text.chars().all(char::is_whitespace);
            if same_line && gap > 0.2 * em && !prev_space && !g_space {
                out.push(TextGlyph {
                    text: " ".to_string(),
                    x: prev.x + prev.w,
                    y: prev.y,
                    w: gap,
                    h: prev.h.max(g.h),
                });
            }
        }
        out.push(g);
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

/// Merge selected glyphs into one rectangle per visual line, so the highlight
/// reads as a continuous selection bar rather than per-glyph boxes. Lines are
/// detected by the x coordinate resetting leftward in reading order. Vertical
/// extent comes from glyphs that have real height (spaces only widen the run).
/// Returns raster-space rects `(x, y, w, h)`.
pub(crate) fn selection_runs(glyphs: &[TextGlyph]) -> Vec<(f32, f32, f32, f32)> {
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
pub(crate) fn nearest_glyph(glyphs: &[TextGlyph], rx: f32, ry: f32) -> usize {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn tg(text: &str) -> TextGlyph {
        TextGlyph { text: text.to_string(), x: 0.0, y: 0.0, w: 1.0, h: 1.0 }
    }

    fn tgp(text: &str, x: f32, y: f32, w: f32, h: f32) -> TextGlyph {
        TextGlyph { text: text.to_string(), x, y, w, h }
    }

    fn joined(glyphs: &[TextGlyph]) -> String {
        glyphs.iter().map(|g| g.text.as_str()).collect()
    }

    #[test]
    fn test_insert_word_spaces_fills_gaps() {
        // Two words on one line (em=10) separated by a 5px gap (> 0.2*em) get a
        // space; the letters within a word abut (gap 0) and stay joined.
        let glyphs = vec![
            tgp("In", 0.0, 0.0, 10.0, 10.0),
            tgp("Fig", 15.0, 0.0, 12.0, 10.0),
        ];
        let out = insert_word_spaces(glyphs);
        assert_eq!(joined(&out), "In Fig");
    }

    #[test]
    fn test_insert_word_spaces_no_space_when_tight() {
        // Cells abut (gap 0) — same word, no space inserted.
        let glyphs = vec![
            tgp("so", 0.0, 0.0, 10.0, 10.0),
            tgp("cial", 10.0, 0.0, 20.0, 10.0),
        ];
        let out = insert_word_spaces(glyphs);
        assert_eq!(joined(&out), "social");
    }

    #[test]
    fn test_insert_word_spaces_not_across_line_break() {
        // Next glyph drops to a new baseline (y jump > tol) — a line break, not a
        // word gap, so no space is inserted (the copy path adds the newline).
        let glyphs = vec![
            tgp("end", 50.0, 0.0, 10.0, 10.0),
            tgp("next", 0.0, 20.0, 10.0, 10.0),
        ];
        let out = insert_word_spaces(glyphs);
        assert_eq!(joined(&out), "endnext");
        assert_eq!(out.len(), 2, "no space glyph inserted across the line break");
    }
}
