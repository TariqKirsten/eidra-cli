//! Markdown to styled lines, for the model's replies: headings, emphasis, code, lists, task lists,
//! quotes, rules and tables, wrapped to a width. A line break stays a line break: a model breaks a
//! line where it means one. Code is shown without fences or a margin, so that it can be copied from
//! the terminal as it is.

use pulldown_cmark::{Alignment, Event, HeadingLevel, Options, Parser, Tag, TagEnd};

use crate::style::{Line, Style, expand_tabs, theme};
use crate::wrap::wrap;

/// `markdown` rendered as lines of at most `width` columns.
pub fn render(markdown: &str, width: usize) -> Vec<Line> {
    let options =
        Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS;
    let mut renderer = Renderer {
        width: width.max(8),
        out: Vec::new(),
        inline: Vec::new(),
        styles: Vec::new(),
        containers: Vec::new(),
        blank: false,
        code: None,
        links: Vec::new(),
        table: None,
    };
    for event in Parser::new_ext(markdown, options) {
        renderer.event(event);
    }
    renderer.flush();
    renderer.out
}

/// What the lines being written are inside of.
enum Container {
    Quote,
    List {
        /// The next item's number, in an ordered list.
        next: Option<u64>,
    },
    Item {
        marker: String,
        /// Whether the marker has been written: lines after the first are indented instead.
        written: bool,
        /// A checked task: its text is dimmed.
        done: bool,
    },
}

struct Table {
    alignments: Vec<Alignment>,
    rows: Vec<Vec<Line>>,
    /// Rows in the head, which are bold and ruled off.
    head: usize,
}

struct Renderer {
    width: usize,
    out: Vec<Line>,
    /// The block being written, split at its line breaks.
    inline: Vec<Line>,
    /// Inline styles, innermost last.
    styles: Vec<Style>,
    containers: Vec<Container>,
    /// A blank line goes before the next block.
    blank: bool,
    /// The text of the code block being read.
    code: Option<String>,
    /// The links being written: each one's destination and text so far.
    links: Vec<(String, String)>,
    table: Option<Table>,
}

impl Renderer {
    fn event(&mut self, event: Event) {
        match event {
            Event::Start(tag) => self.start(tag),
            Event::End(tag) => self.end(tag),
            Event::Text(text) => match &mut self.code {
                Some(code) => code.push_str(&text),
                None => self.text(&text, self.style()),
            },
            Event::Code(text) => self.text(&text, self.style().patch(theme::CODE)),
            Event::Html(html) | Event::InlineHtml(html) => self.text(&html, self.style()),
            Event::InlineMath(math) | Event::DisplayMath(math) => self.text(&math, self.style()),
            Event::FootnoteReference(name) => self.text(&format!("[^{name}]"), self.style()),
            Event::SoftBreak | Event::HardBreak => self.inline.push(Line::new()),
            Event::Rule => {
                self.block();
                let width = self.width.saturating_sub(self.indent()).max(1);
                self.emit(Line::styled("─".repeat(width), theme::DIM));
                self.blank = true;
            }
            Event::TaskListMarker(done) => {
                if let Some(Container::Item { marker, written: false, done: d }) =
                    self.containers.last_mut()
                {
                    *marker = if done { "☒ " } else { "☐ " }.into();
                    *d = done;
                }
            }
        }
    }

    fn start(&mut self, tag: Tag) {
        match tag {
            Tag::Paragraph | Tag::HtmlBlock => self.block(),
            Tag::Heading { level, .. } => {
                self.block();
                let style = match level {
                    HeadingLevel::H1 => theme::BOLD.underline(),
                    _ => theme::BOLD,
                };
                self.styles.push(style);
            }
            Tag::BlockQuote(_) => {
                self.block();
                self.containers.push(Container::Quote);
            }
            Tag::CodeBlock(_) => {
                self.block();
                self.code = Some(String::new());
            }
            Tag::List(first) => {
                // A list inside an item follows the item's text without a gap.
                if self.in_item() {
                    self.flush();
                } else {
                    self.block();
                }
                self.containers.push(Container::List { next: first });
            }
            Tag::Item => {
                self.flush();
                let depth =
                    self.containers.iter().filter(|c| matches!(c, Container::List { .. })).count();
                let marker = match self.containers.last_mut() {
                    Some(Container::List { next: Some(n) }) => {
                        *n += 1;
                        format!("{}. ", *n - 1)
                    }
                    _ => ["• ", "◦ ", "▪ "][(depth.max(1) - 1).min(2)].into(),
                };
                self.containers.push(Container::Item { marker, written: false, done: false });
            }
            Tag::Emphasis => self.styles.push(Style::PLAIN.italic()),
            Tag::Strong => self.styles.push(theme::BOLD),
            Tag::Strikethrough => self.styles.push(Style::PLAIN.strike()),
            Tag::Link { dest_url, .. } => {
                self.styles.push(Style::PLAIN.underline());
                self.links.push((dest_url.into_string(), String::new()));
            }
            Tag::Image { .. } => self.text("[image: ", theme::DIM),
            Tag::Table(alignments) => {
                self.block();
                self.table = Some(Table { alignments, rows: Vec::new(), head: 0 });
            }
            Tag::TableHead | Tag::TableRow => {
                if let Some(table) = &mut self.table {
                    table.rows.push(Vec::new());
                }
            }
            Tag::TableCell => self.inline = vec![Line::new()],
            Tag::FootnoteDefinition(_)
            | Tag::DefinitionList
            | Tag::DefinitionListTitle
            | Tag::DefinitionListDefinition
            | Tag::MetadataBlock(_) => self.block(),
            Tag::Superscript | Tag::Subscript => self.styles.push(Style::PLAIN),
        }
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph
            | TagEnd::HtmlBlock
            | TagEnd::FootnoteDefinition
            | TagEnd::DefinitionList
            | TagEnd::DefinitionListTitle
            | TagEnd::DefinitionListDefinition
            | TagEnd::MetadataBlock(_) => self.end_block(),
            TagEnd::Heading(_) => {
                self.styles.pop();
                self.end_block();
            }
            TagEnd::BlockQuote(_) => {
                self.flush();
                self.containers.pop();
                self.blank = true;
            }
            TagEnd::CodeBlock => {
                let code = self.code.take().unwrap_or_default();
                let width = self.width.saturating_sub(self.indent()).max(1);
                for line in code.strip_suffix('\n').unwrap_or(&code).split('\n') {
                    for row in wrap(&Line::styled(expand_tabs(line), theme::CODE), width) {
                        self.emit(row);
                    }
                }
                self.blank = true;
            }
            TagEnd::List(_) => {
                self.flush();
                self.containers.pop();
                if !self.in_item() {
                    self.blank = true;
                }
            }
            TagEnd::Item => {
                self.flush();
                self.containers.pop();
            }
            TagEnd::Emphasis
            | TagEnd::Strong
            | TagEnd::Strikethrough
            | TagEnd::Superscript
            | TagEnd::Subscript => {
                self.styles.pop();
            }
            TagEnd::Link => {
                self.styles.pop();
                if let Some((url, text)) = self.links.pop()
                    && !url.is_empty()
                    && url != text
                    && url.strip_prefix("mailto:") != Some(&text)
                {
                    self.text(&format!(" ({url})"), theme::DIM);
                }
            }
            TagEnd::Image => self.text("]", theme::DIM),
            TagEnd::TableHead => {
                if let Some(table) = &mut self.table {
                    table.head = table.rows.len();
                }
            }
            TagEnd::TableRow => {}
            TagEnd::TableCell => {
                let mut cell = Line::new();
                for (i, line) in std::mem::take(&mut self.inline).into_iter().enumerate() {
                    if i > 0 {
                        cell.push(" ", Style::PLAIN);
                    }
                    cell.append(line);
                }
                if let Some(row) = self.table.as_mut().and_then(|t| t.rows.last_mut()) {
                    row.push(cell);
                }
            }
            TagEnd::Table => {
                if let Some(table) = self.table.take() {
                    self.table(table);
                }
                self.blank = true;
            }
        }
    }

    /// Adds inline text in `style`; line breaks in it start new lines.
    fn text(&mut self, text: &str, style: Style) {
        for (_, link) in &mut self.links {
            link.push_str(text);
        }
        for (i, part) in text.split('\n').enumerate() {
            if i > 0 || self.inline.is_empty() {
                self.inline.push(Line::new());
            }
            if let Some(line) = self.inline.last_mut() {
                line.push(&expand_tabs(part), style);
            }
        }
    }

    fn style(&self) -> Style {
        self.styles.iter().fold(Style::PLAIN, |style, &over| style.patch(over))
    }

    fn in_item(&self) -> bool {
        self.containers.iter().any(|c| matches!(c, Container::Item { .. }))
    }

    /// A block starts: what came before it is written, then a blank line if one is due.
    fn block(&mut self) {
        self.flush();
        if self.blank {
            let mut line = Line::new();
            for container in &self.containers {
                if let Container::Quote = container {
                    line.push("│ ", theme::DIM);
                }
            }
            if let Some(last) = line.spans.last_mut() {
                last.text.truncate(last.text.trim_end().len());
            }
            self.out.push(line);
            self.blank = false;
        }
    }

    fn end_block(&mut self) {
        self.flush();
        self.blank = true;
    }

    /// Writes the inline text so far, wrapped within the containers' margins.
    fn flush(&mut self) {
        let lines = std::mem::take(&mut self.inline);
        if lines.iter().all(Line::is_empty) {
            return;
        }
        let done = self.containers.iter().any(|c| matches!(c, Container::Item { done: true, .. }));
        let width = self.width.saturating_sub(self.indent()).max(1);
        for line in lines {
            for row in wrap(&line, width) {
                self.emit(if done { row.patched(theme::DIM) } else { row });
            }
        }
    }

    /// Columns the containers' margins take.
    fn indent(&self) -> usize {
        self.containers
            .iter()
            .map(|c| match c {
                Container::Quote => 2,
                Container::List { .. } => 0,
                Container::Item { marker, .. } => crate::style::width(marker),
            })
            .sum()
    }

    /// Writes one row with the containers' margins: quote bars, and an item's marker on its first
    /// row and spaces after that.
    fn emit(&mut self, row: Line) {
        let mut line = Line::new();
        for container in &mut self.containers {
            match container {
                Container::Quote => line.push("│ ", theme::DIM),
                Container::List { .. } => {}
                Container::Item { marker, written, .. } => {
                    if *written {
                        line.push(&" ".repeat(crate::style::width(marker)), Style::PLAIN);
                    } else {
                        // Bullets and numbers recede; a task's box is the point.
                        let task = marker.starts_with(['☐', '☒']);
                        line.push(marker, if task { Style::PLAIN } else { theme::DIM });
                        *written = true;
                    }
                }
            }
        }
        line.append(row);
        self.out.push(line);
    }

    /// A table: columns padded to line up when they fit, else each row's cells joined.
    fn table(&mut self, table: Table) {
        let columns = table.rows.iter().map(Vec::len).max().unwrap_or(0);
        if columns == 0 {
            return;
        }
        let widths: Vec<usize> = (0..columns)
            .map(|c| table.rows.iter().filter_map(|r| r.get(c)).map(Line::width).max().unwrap_or(0))
            .collect();
        let room = self.width.saturating_sub(self.indent());
        let fits = widths.iter().sum::<usize>() + 3 * (columns - 1) <= room;
        for (i, row) in table.rows.iter().enumerate() {
            let mut line = Line::new();
            for (c, &width) in widths.iter().enumerate() {
                if c > 0 {
                    line.push(" │ ", theme::DIM);
                }
                let mut cell = row.get(c).cloned().unwrap_or_default();
                if i < table.head {
                    cell = cell.patched(theme::BOLD);
                }
                if !fits {
                    line.append(cell);
                    continue;
                }
                let pad = width.saturating_sub(cell.width());
                let (left, right) = match table.alignments.get(c) {
                    Some(Alignment::Right) => (pad, 0),
                    Some(Alignment::Center) => (pad / 2, pad - pad / 2),
                    _ => (0, pad),
                };
                line.push(&" ".repeat(left), Style::PLAIN);
                line.append(cell);
                // The last column needs no padding after it.
                if c + 1 < columns {
                    line.push(&" ".repeat(right), Style::PLAIN);
                }
            }
            for row in wrap(&line, room.max(1)) {
                self.emit(row);
            }
            if fits && i + 1 == table.head {
                let rule: Vec<String> = widths.iter().map(|&w| "─".repeat(w)).collect();
                self.emit(Line::styled(rule.join("─┼─"), theme::DIM));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(markdown: &str, width: usize) -> Vec<String> {
        render(markdown, width).iter().map(Line::text).collect()
    }

    #[test]
    fn paragraphs_wrap_and_keep_their_line_breaks() {
        assert_eq!(
            text("One two three four five.\nSix.\n\nNext paragraph.", 12),
            ["One two", "three four", "five.", "Six.", "", "Next", "paragraph."]
        );
        assert_eq!(text("line one  \nline two", 40), ["line one", "line two"]);
        assert_eq!(text("1\n2\n3", 40), ["1", "2", "3"]);
    }

    #[test]
    fn styles_inline_text() {
        let lines = render("Some **bold**, *italic* and `code` here.", 80);
        assert_eq!(lines.len(), 1);
        let styled: Vec<(&str, Style)> =
            lines[0].spans.iter().map(|s| (s.text.as_str(), s.style)).collect();
        assert_eq!(
            styled,
            [
                ("Some ", Style::PLAIN),
                ("bold", theme::BOLD),
                (", ", Style::PLAIN),
                ("italic", Style::PLAIN.italic()),
                (" and ", Style::PLAIN),
                ("code", theme::CODE),
                (" here.", Style::PLAIN),
            ]
        );
    }

    #[test]
    fn headings_and_rules() {
        let lines = render("# Title\n\ntext\n\n---\n\n## Sub", 10);
        let texts: Vec<String> = lines.iter().map(Line::text).collect();
        assert_eq!(texts, ["Title", "", "text", "", "──────────", "", "Sub"]);
        assert!(lines[0].spans[0].style.bold && lines[0].spans[0].style.underline);
        assert_eq!(lines[6].spans[0].style, theme::BOLD);
    }

    #[test]
    fn lists_hang_their_text_under_the_marker() {
        assert_eq!(
            text("- first item is long\n- second\n  - nested item\n\n3. three\n4. four", 14),
            [
                "• first item",
                "  is long",
                "• second",
                "  ◦ nested",
                "    item",
                "",
                "3. three",
                "4. four"
            ]
        );
        assert_eq!(text("- a\n\n- b", 20), ["• a", "", "• b"]);
    }

    #[test]
    fn task_lists_dim_what_is_done() {
        let lines = render("- [x] read the spec\n- [ ] write tests", 40);
        let texts: Vec<String> = lines.iter().map(Line::text).collect();
        assert_eq!(texts, ["☒ read the spec", "☐ write tests"]);
        assert!(!lines[0].spans[0].style.dim);
        assert!(lines[0].spans.iter().skip(1).all(|s| s.style.dim));
        assert!(lines[1].spans.iter().all(|s| !s.style.dim));
    }

    #[test]
    fn code_blocks_keep_their_text() {
        let lines = render("Run:\n\n```sh\ncargo test\n\tindented\n\n```\nDone.", 40);
        let texts: Vec<String> = lines.iter().map(Line::text).collect();
        assert_eq!(texts, ["Run:", "", "cargo test", "    indented", "", "", "Done."]);
        assert_eq!(lines[2].spans[0].style, theme::CODE);
    }

    #[test]
    fn quotes_and_links() {
        assert_eq!(
            text("> quoted text that wraps\n>\n> more", 14),
            ["│ quoted text", "│ that wraps", "│", "│ more"]
        );
        assert_eq!(
            text("See [the docs](https://example.com) or <https://example.com>.", 80),
            ["See the docs (https://example.com) or https://example.com."]
        );
    }

    #[test]
    fn tables_line_up_when_they_fit() {
        let table = "| name | n |\n|------|--:|\n| a | 1 |\n| longer | 22 |";
        assert_eq!(text(table, 40), ["name   │  n", "───────┼───", "a      │  1", "longer │ 22"]);
        assert_eq!(text(table, 8), ["name │ n", "a │ 1", "longer │", "22"]);
    }

    #[test]
    fn unfinished_markdown_renders_as_far_as_it_goes() {
        assert_eq!(text("Text\n\n```py\nprint(1", 40), ["Text", "", "print(1"]);
        assert_eq!(text("a **bold", 40), ["a **bold"]);
    }
}
