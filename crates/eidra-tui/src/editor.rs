//! The input box: multi-line text with a caret, readline-style keys, and the history of what was
//! sent. Up and Down move between rows as the box shows them, and walk the history from the first
//! or last row.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::style::{Line, Style, theme, widths};

const PROMPT: &str = "› ";
const INDENT: &str = "  ";
/// Columns the prompt and the indent take.
const MARGIN: usize = 2;

#[derive(Default)]
pub struct Editor {
    text: String,
    /// A byte offset into `text`, on a character boundary.
    caret: usize,
    /// The column Up and Down aim for, kept while they are pressed in a row.
    goal: Option<usize>,
    /// Columns of text per row, as last drawn.
    columns: usize,
    /// The first row shown, when there are more rows than room.
    scroll: usize,
    history: Vec<String>,
    /// The history entry shown, while walking the history.
    browsing: Option<usize>,
    /// What was in the box before walking the history.
    draft: String,
}

/// A row of the box: a byte range of `text`, without the line break.
struct Row {
    start: usize,
    end: usize,
    /// The last row of its line; otherwise the line wraps onto the next row.
    last: bool,
}

impl Editor {
    pub fn new() -> Self {
        Editor { columns: 80, ..Self::default() }
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    pub fn clear(&mut self) {
        self.set(String::new());
    }

    /// Replaces the text, with the caret at the end.
    pub fn set(&mut self, text: String) {
        self.text = text;
        self.caret = self.text.len();
        self.goal = None;
        self.browsing = None;
    }

    /// Takes the text, leaving the box empty, and adds it to the history.
    pub fn submit(&mut self) -> String {
        let text = std::mem::take(&mut self.text);
        self.clear();
        self.draft.clear();
        if !text.trim().is_empty() && self.history.last() != Some(&text) {
            self.history.push(text.clone());
        }
        text
    }

    /// Inserts text at the caret; pasted line endings become line breaks.
    pub fn insert(&mut self, text: &str) {
        let text = text.replace("\r\n", "\n").replace('\r', "\n");
        self.text.insert_str(self.caret, &text);
        self.caret += text.len();
        self.edited();
    }

    /// Handles an editing or movement key, and says whether it was one.
    pub fn key(&mut self, key: KeyEvent) -> bool {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let goal = self.goal.take();
        let code = match key.code {
            KeyCode::Char('p') if ctrl => KeyCode::Up,
            KeyCode::Char('n') if ctrl => KeyCode::Down,
            code => code,
        };
        match code {
            KeyCode::Char(ch) if !ctrl && !alt => self.insert(ch.encode_utf8(&mut [0; 4])),
            KeyCode::Char('a') if ctrl => self.caret = self.line_start(),
            KeyCode::Char('e') if ctrl => self.caret = self.line_end(),
            KeyCode::Char('b') if ctrl => self.caret = self.previous(self.caret),
            KeyCode::Char('f') if ctrl => self.caret = self.next(self.caret),
            KeyCode::Char('b') if alt => self.caret = self.word_start(),
            KeyCode::Char('f') if alt => self.caret = self.word_end(),
            KeyCode::Char('h') if ctrl => self.delete_to(self.previous(self.caret)),
            KeyCode::Char('w') if ctrl => self.delete_to(self.word_start()),
            KeyCode::Char('u') if ctrl => self.delete_to(self.line_start()),
            KeyCode::Char('k') if ctrl => self.delete_to(self.line_end()),
            KeyCode::Char('d') if alt => self.delete_to(self.word_end()),
            KeyCode::Backspace if ctrl || alt => self.delete_to(self.word_start()),
            KeyCode::Backspace => self.delete_to(self.previous(self.caret)),
            KeyCode::Delete => self.delete_to(self.next(self.caret)),
            KeyCode::Left if ctrl || alt => self.caret = self.word_start(),
            KeyCode::Right if ctrl || alt => self.caret = self.word_end(),
            KeyCode::Left => self.caret = self.previous(self.caret),
            KeyCode::Right => self.caret = self.next(self.caret),
            KeyCode::Home => self.caret = self.line_start(),
            KeyCode::End => self.caret = self.line_end(),
            KeyCode::Up => {
                if !self.vertical(-1, goal) {
                    self.older();
                }
            }
            KeyCode::Down => {
                if !self.vertical(1, goal) {
                    self.newer();
                }
            }
            KeyCode::Tab => self.insert("    "),
            _ => {
                self.goal = goal;
                return false;
            }
        }
        true
    }

    /// The box as rows `width` columns wide: a prompt, then the text with the caret in reverse
    /// video if `caret`, scrolled to keep it in view within `max_rows`. Empty, it shows
    /// `placeholder`.
    pub fn render(
        &mut self,
        width: usize,
        max_rows: usize,
        caret: bool,
        placeholder: &str,
    ) -> Vec<Line> {
        // Past the prompt, a column stays free for the caret after a full row.
        self.columns = width.saturating_sub(MARGIN + 1).max(1);
        if self.text.is_empty() {
            let mut line = Line::styled(PROMPT, theme::USER);
            let mut hint = placeholder.chars();
            if caret {
                line.push(&hint.next().unwrap_or(' ').to_string(), theme::CARET.dim());
            }
            let hint: String = hint.collect();
            // clip() adds an ellipsis after the characters it keeps.
            let room = width.saturating_sub(MARGIN + usize::from(caret) + 1);
            line.push(&eidra_core::display::clip(&hint, room), theme::DIM);
            return vec![line];
        }

        let rows = self.rows();
        let (at, _) = self.position(&rows);
        let max_rows = max_rows.max(1);
        self.scroll = self.scroll.min(at).max((at + 1).saturating_sub(max_rows));
        self.scroll = self.scroll.min(rows.len().saturating_sub(max_rows));
        let mut lines = Vec::new();
        for (i, row) in rows.iter().enumerate().skip(self.scroll).take(max_rows) {
            let mut line =
                if i == 0 { Line::styled(PROMPT, theme::USER) } else { Line::plain(INDENT) };
            let mut col = 0;
            for (offset, ch) in self.text[row.start..row.end].char_indices() {
                let style = if caret && i == at && row.start + offset == self.caret {
                    theme::CARET
                } else {
                    Style::PLAIN
                };
                let cell = cell_width(ch, col, &self.text[row.start..row.start + offset]);
                if ch == '\t' {
                    line.push(" ", style);
                    line.push(&" ".repeat(cell - 1), Style::PLAIN);
                } else {
                    line.push(&ch.to_string(), style);
                }
                col += cell;
            }
            if caret && i == at && self.caret == row.end {
                line.push(" ", theme::CARET);
            }
            lines.push(line);
        }
        lines
    }

    /// The box's rows at the last drawn width: one per line, wrapped at any character.
    fn rows(&self) -> Vec<Row> {
        let mut rows = Vec::new();
        let mut offset = 0;
        for line in self.text.split('\n') {
            let mut start = offset;
            let mut col = 0;
            for (i, ch) in line.char_indices() {
                let before = &self.text[start..offset + i];
                let mut cell = cell_width(ch, col, before);
                if col + cell > self.columns && col > 0 {
                    rows.push(Row { start, end: offset + i, last: false });
                    start = offset + i;
                    col = 0;
                    cell = cell_width(ch, 0, "");
                }
                col += cell;
            }
            rows.push(Row { start, end: offset + line.len(), last: true });
            offset += line.len() + 1;
        }
        rows
    }

    /// The caret's row and column. At the end of a row that wraps, it is at the start of the next.
    fn position(&self, rows: &[Row]) -> (usize, usize) {
        let at = rows
            .iter()
            .rposition(|row| row.start <= self.caret && self.caret <= row.end)
            .unwrap_or(0);
        (at, self.columns_between(rows[at].start, self.caret))
    }

    fn columns_between(&self, start: usize, end: usize) -> usize {
        let mut col = 0;
        for (offset, ch) in self.text[start..end].char_indices() {
            col += cell_width(ch, col, &self.text[start..start + offset]);
        }
        col
    }

    /// Moves the caret a row up (-1) or down (1), towards the goal column; false if there is no
    /// row there.
    fn vertical(&mut self, direction: isize, goal: Option<usize>) -> bool {
        let rows = self.rows();
        let (at, column) = self.position(&rows);
        let Some(target) = at.checked_add_signed(direction).and_then(|t| rows.get(t)) else {
            return false;
        };
        let goal = goal.unwrap_or(column);
        let mut caret = target.start;
        let mut col = 0;
        for (offset, ch) in self.text[target.start..target.end].char_indices() {
            let at = target.start + offset;
            let cell = cell_width(ch, col, &self.text[target.start..at]);
            if col + cell > goal {
                break;
            }
            col += cell;
            caret = at + ch.len_utf8();
        }
        // The end of a row that wraps is shown on the next row, so stop a character short.
        if !target.last && caret == target.end {
            caret = self.previous(caret).max(target.start);
        }
        self.caret = caret;
        self.goal = Some(goal);
        true
    }

    /// Shows the previous history entry.
    fn older(&mut self) {
        let index = self.browsing.unwrap_or(self.history.len());
        if index == 0 {
            return;
        }
        if self.browsing.is_none() {
            self.draft = std::mem::take(&mut self.text);
        }
        self.text = self.history[index - 1].clone();
        self.caret = self.text.len();
        self.browsing = Some(index - 1);
    }

    /// Shows the next history entry, or what was there before walking the history.
    fn newer(&mut self) {
        let Some(index) = self.browsing else {
            return;
        };
        if index + 1 < self.history.len() {
            self.text = self.history[index + 1].clone();
            self.browsing = Some(index + 1);
        } else {
            self.text = std::mem::take(&mut self.draft);
            self.browsing = None;
        }
        self.caret = self.text.len();
    }

    /// An edited history entry becomes the text being written.
    fn edited(&mut self) {
        self.browsing = None;
        self.draft.clear();
    }

    /// Deletes between the caret and `to`.
    fn delete_to(&mut self, to: usize) {
        let (start, end) = if to < self.caret { (to, self.caret) } else { (self.caret, to) };
        if start == end {
            return;
        }
        self.text.replace_range(start..end, "");
        self.caret = start;
        self.edited();
    }

    fn previous(&self, at: usize) -> usize {
        self.text[..at].char_indices().next_back().map_or(0, |(i, _)| i)
    }

    fn next(&self, at: usize) -> usize {
        self.text[at..].chars().next().map_or(at, |ch| at + ch.len_utf8())
    }

    fn line_start(&self) -> usize {
        self.text[..self.caret].rfind('\n').map_or(0, |i| i + 1)
    }

    fn line_end(&self) -> usize {
        self.text[self.caret..].find('\n').map_or(self.text.len(), |i| self.caret + i)
    }

    /// The start of the word before the caret.
    fn word_start(&self) -> usize {
        let before = &self.text[..self.caret];
        let end = before.trim_end_matches(|c: char| !is_word(c)).len();
        before[..end].trim_end_matches(is_word).len()
    }

    /// The end of the word after the caret.
    fn word_end(&self) -> usize {
        let after = &self.text[self.caret..];
        let skipped = after.len() - after.trim_start_matches(|c: char| !is_word(c)).len();
        let rest = &after[skipped..];
        self.caret + skipped + (rest.len() - rest.trim_start_matches(is_word).len())
    }
}

fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Columns a character takes at column `col` of a row whose text before it is `before`: a tab
/// reaches the next multiple of four.
fn cell_width(ch: char, col: usize, before: &str) -> usize {
    match ch {
        '\t' => 4 - col % 4,
        _ => {
            let previous = before.chars().next_back();
            widths(previous.into_iter().chain([ch])).last().map_or(0, |(_, w)| w)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn press(editor: &mut Editor, code: KeyCode) {
        editor.key(KeyEvent::new(code, KeyModifiers::NONE));
    }

    fn ctrl(editor: &mut Editor, ch: char) {
        editor.key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::CONTROL));
    }

    fn typed(text: &str) -> Editor {
        let mut editor = Editor::new();
        editor.insert(text);
        editor
    }

    /// The rows as text, with the caret shown as `|`.
    fn shown(editor: &mut Editor, width: usize, max_rows: usize) -> Vec<String> {
        editor
            .render(width, max_rows, true, "")
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|s| {
                        if s.style == theme::CARET {
                            format!("|{}", s.text.trim_end())
                        } else {
                            s.text.clone()
                        }
                    })
                    .collect::<String>()
            })
            .collect()
    }

    #[test]
    fn edits_at_the_caret() {
        let mut editor = typed("hello world");
        press(&mut editor, KeyCode::Left);
        press(&mut editor, KeyCode::Backspace);
        assert_eq!(editor.text(), "hello word");
        ctrl(&mut editor, 'a');
        press(&mut editor, KeyCode::Delete);
        assert_eq!(editor.text(), "ello word");
        ctrl(&mut editor, 'e');
        ctrl(&mut editor, 'w');
        assert_eq!(editor.text(), "ello ");
        editor.insert("wörld\r\nnext");
        assert_eq!(editor.text(), "ello wörld\nnext");
        ctrl(&mut editor, 'u');
        assert_eq!(editor.text(), "ello wörld\n");
        for _ in 0..4 {
            press(&mut editor, KeyCode::Left);
        }
        press(&mut editor, KeyCode::Backspace);
        assert_eq!(editor.text(), "ello wrld\n");
    }

    #[test]
    fn moves_by_words() {
        let mut editor = typed("one two_three  four");
        editor.key(KeyEvent::new(KeyCode::Left, KeyModifiers::CONTROL));
        assert_eq!(editor.caret, 15);
        editor.key(KeyEvent::new(KeyCode::Left, KeyModifiers::CONTROL));
        assert_eq!(editor.caret, 4);
        editor.key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::ALT));
        assert_eq!(editor.text(), "one   four");
        editor.key(KeyEvent::new(KeyCode::Right, KeyModifiers::ALT));
        assert_eq!(editor.caret, editor.text().len());
    }

    #[test]
    fn wraps_and_shows_the_caret() {
        let mut editor = typed("abcdefgh\nxy");
        // Eight columns: the prompt, five of text, and one for the caret.
        assert_eq!(shown(&mut editor, 8, 10), ["› abcde", "  fgh", "  xy|"]);
        ctrl(&mut editor, 'a');
        assert_eq!(shown(&mut editor, 8, 10), ["› abcde", "  fgh", "  |xy"]);
        let mut editor = typed("abcde");
        assert_eq!(shown(&mut editor, 8, 10), ["› abcde|"]);
        editor.insert("f");
        assert_eq!(shown(&mut editor, 8, 10), ["› abcde", "  f|"]);
    }

    #[test]
    fn scrolls_to_keep_the_caret_in_view() {
        let mut editor = typed("1\n2\n3\n4\n5");
        assert_eq!(shown(&mut editor, 20, 2), ["  4", "  5|"]);
        for _ in 0..4 {
            press(&mut editor, KeyCode::Up);
        }
        assert_eq!(shown(&mut editor, 20, 2), ["› 1|", "  2"]);
        press(&mut editor, KeyCode::Down);
        assert_eq!(shown(&mut editor, 20, 2), ["› 1", "  2|"]);
    }

    #[test]
    fn moves_between_rows_towards_a_goal_column() {
        let mut editor = typed("a long line\nab\nanother line");
        shown(&mut editor, 40, 10);
        press(&mut editor, KeyCode::Up);
        assert_eq!(editor.caret, "a long line\nab".len());
        press(&mut editor, KeyCode::Up);
        assert_eq!(editor.caret, "a long line".len());
        press(&mut editor, KeyCode::Down);
        press(&mut editor, KeyCode::Down);
        assert_eq!(editor.caret, editor.text().len());

        // Across wrapped rows, the end of a row that wraps is not a place to stop.
        let mut editor = typed("abcdefghij");
        assert_eq!(shown(&mut editor, 8, 10), ["› abcde", "  fghij|"]);
        press(&mut editor, KeyCode::Up);
        assert_eq!(shown(&mut editor, 8, 10), ["› abcd|e", "  fghij"]);
    }

    #[test]
    fn walks_the_history_from_the_first_and_last_rows() {
        let mut editor = Editor::new();
        for entry in ["first", "second\nentry"] {
            editor.insert(entry);
            editor.submit();
        }
        editor.insert("draft");
        press(&mut editor, KeyCode::Up);
        assert_eq!(editor.text(), "second\nentry");
        // From the entry's last row, Up first moves within it.
        press(&mut editor, KeyCode::Up);
        assert_eq!(editor.text(), "second\nentry");
        press(&mut editor, KeyCode::Up);
        assert_eq!(editor.text(), "first");
        press(&mut editor, KeyCode::Up);
        assert_eq!(editor.text(), "first");
        press(&mut editor, KeyCode::Down);
        assert_eq!(editor.text(), "second\nentry");
        press(&mut editor, KeyCode::Down);
        assert_eq!(editor.text(), "draft");
        // Ctrl+P and Ctrl+N do the same, as in a shell.
        let ctrl = |code| KeyEvent::new(code, KeyModifiers::CONTROL);
        editor.key(ctrl(KeyCode::Char('p')));
        assert_eq!(editor.text(), "second\nentry");
        editor.key(ctrl(KeyCode::Char('n')));
        assert_eq!(editor.text(), "draft");
        // Sending the same thing twice keeps one entry.
        editor.set("first".into());
        editor.submit();
        assert_eq!(editor.history, ["first", "second\nentry", "first"]);
        editor.set("first".into());
        editor.submit();
        assert_eq!(editor.history.len(), 3);
    }

    #[test]
    fn shows_a_placeholder_when_empty() {
        let mut editor = Editor::new();
        let line = &editor.render(30, 5, true, "Message eidra")[0];
        assert_eq!(line.text(), "› Message eidra");
        assert_eq!(line.spans[1].text, "M");
        assert!(line.spans[1].style.reverse);
        let line = &editor.render(10, 5, false, "Message eidra")[0];
        assert_eq!(line.text(), "› Message…");
    }

    #[test]
    fn draws_tabs_as_spaces_to_the_next_stop() {
        let mut editor = typed("a\tb");
        assert_eq!(shown(&mut editor, 20, 5), ["› a   b|"]);
    }
}
