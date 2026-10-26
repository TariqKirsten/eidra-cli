//! Word wrapping, measured with unicode-width. The TUI wraps everything itself, so the screen knows
//! exactly how many rows each line takes and the terminal never wraps anything on its own.

use crate::style::{Line, Style, widths};

/// `line` broken into rows of at most `width` columns: at spaces where it can, inside a word where
/// it must. The spaces at a break are dropped; a line's leading spaces are kept.
pub fn wrap(line: &Line, width: usize) -> Vec<Line> {
    let width = width.max(1);
    if line.width() <= width {
        return vec![line.clone()];
    }
    let styles = line.spans.iter().flat_map(|span| span.text.chars().map(move |_| span.style));
    let cells: Vec<Cell> = widths(line.spans.iter().flat_map(|span| span.text.chars()))
        .zip(styles)
        .map(|((ch, width), style)| Cell { ch, style, width })
        .collect();

    let mut rows = Vec::new();
    let mut start = 0;
    while start < cells.len() {
        let mut used = 0;
        let mut end = start;
        // Where the row can break: a space that follows something else.
        let mut space = None;
        while end < cells.len() && used + cells[end].width <= width {
            if cells[end].ch == ' ' && end > start && cells[end - 1].ch != ' ' {
                space = Some(end);
            }
            used += cells[end].width;
            end += 1;
        }
        if end == cells.len() {
            rows.push(row(&cells[start..]));
            break;
        }
        let cut = match space {
            _ if cells[end].ch == ' ' && end > start => end,
            Some(space) => space,
            // No space to break at, so the word is split. A character wider than the row gets one
            // to itself.
            None => end.max(start + 1),
        };
        let mut kept = &cells[start..cut];
        while let [rest @ .., last] = kept
            && last.ch == ' '
        {
            kept = rest;
        }
        rows.push(row(kept));
        start = cut;
        while start < cells.len() && cells[start].ch == ' ' {
            start += 1;
        }
    }
    rows
}

/// Every line of `lines` wrapped to `width`.
pub fn wrap_all(lines: &[Line], width: usize) -> Vec<Line> {
    lines.iter().flat_map(|line| wrap(line, width)).collect()
}

struct Cell {
    ch: char,
    style: Style,
    width: usize,
}

fn row(cells: &[Cell]) -> Line {
    let mut line = Line::new();
    let mut text = String::new();
    let mut style = cells.first().map(|c| c.style).unwrap_or_default();
    for cell in cells {
        if cell.style != style {
            line.push(&text, style);
            text.clear();
            style = cell.style;
        }
        text.push(cell.ch);
    }
    line.push(&text, style);
    line
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::style::theme;

    fn texts(line: &Line, width: usize) -> Vec<String> {
        wrap(line, width).iter().map(Line::text).collect()
    }

    #[test]
    fn breaks_at_spaces() {
        let line = Line::plain("the quick brown fox jumps");
        assert_eq!(texts(&line, 10), ["the quick", "brown fox", "jumps"]);
        assert_eq!(texts(&line, 9), ["the quick", "brown fox", "jumps"]);
        assert_eq!(texts(&line, 25), ["the quick brown fox jumps"]);
        assert_eq!(texts(&Line::plain("aaa   bbb"), 4), ["aaa", "bbb"]);
    }

    #[test]
    fn splits_words_that_do_not_fit() {
        assert_eq!(texts(&Line::plain("abcdefghij"), 4), ["abcd", "efgh", "ij"]);
        assert_eq!(texts(&Line::plain("ab abcdefgh"), 4), ["ab", "abcd", "efgh"]);
    }

    #[test]
    fn keeps_indentation_but_not_spaces_at_breaks() {
        assert_eq!(texts(&Line::plain("    let x = 1;"), 10), ["    let x", "= 1;"]);
        assert_eq!(texts(&Line::plain("      abcdefgh"), 8), ["      ab", "cdefgh"]);
        assert_eq!(texts(&Line::new(), 5), [""]);
    }

    #[test]
    fn counts_wide_characters() {
        assert_eq!(texts(&Line::plain("日本語のテキスト"), 5), ["日本", "語の", "テキ", "スト"]);
        assert_eq!(texts(&Line::plain("日本"), 1), ["日", "本"]);
        for row in wrap(&Line::plain("a日本語 です ok"), 4) {
            assert!(row.width() <= 4, "{row:?}");
        }
    }

    #[test]
    fn keeps_styles_across_breaks() {
        let line = Line::plain("plain ").with("bold words here", theme::BOLD);
        let rows = wrap(&line, 10);
        assert_eq!(rows.iter().map(Line::text).collect::<Vec<_>>(), ["plain bold", "words here"]);
        assert_eq!(rows[0].spans.len(), 2);
        assert_eq!(rows[0].spans[1].style, theme::BOLD);
        assert_eq!(rows[1].spans.len(), 1);
        assert_eq!(rows[1].spans[0].style, theme::BOLD);
    }

    #[test]
    fn wraps_text_that_fits_unchanged() {
        let line = Line::plain("fits  ");
        assert_eq!(wrap(&line, 6), [line]);
    }
}
