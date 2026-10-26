//! Styled text: spans of one style, and lines of spans. Everything the TUI shows is built from
//! these, wrapped to the terminal's width (wrap.rs) and written out by the screen (screen.rs).

use std::sync::OnceLock;

pub use crossterm::style::Color;
use unicode_width::UnicodeWidthChar;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Style {
    pub fg: Option<Color>,
    pub bold: bool,
    pub dim: bool,
    pub italic: bool,
    pub underline: bool,
    pub reverse: bool,
    pub strike: bool,
}

impl Style {
    pub const PLAIN: Style = Style {
        fg: None,
        bold: false,
        dim: false,
        italic: false,
        underline: false,
        reverse: false,
        strike: false,
    };

    pub const fn fg(mut self, color: Color) -> Self {
        self.fg = Some(color);
        self
    }

    pub const fn bold(mut self) -> Self {
        self.bold = true;
        self
    }

    pub const fn dim(mut self) -> Self {
        self.dim = true;
        self
    }

    pub const fn italic(mut self) -> Self {
        self.italic = true;
        self
    }

    pub const fn underline(mut self) -> Self {
        self.underline = true;
        self
    }

    pub const fn reverse(mut self) -> Self {
        self.reverse = true;
        self
    }

    pub const fn strike(mut self) -> Self {
        self.strike = true;
        self
    }

    /// `over` laid on this style: its color if it has one, and the attributes of both.
    pub fn patch(self, over: Style) -> Style {
        Style {
            fg: over.fg.or(self.fg),
            bold: self.bold || over.bold,
            dim: self.dim || over.dim,
            italic: self.italic || over.italic,
            underline: self.underline || over.underline,
            reverse: self.reverse || over.reverse,
            strike: self.strike || over.strike,
        }
    }

    /// The SGR parameters that set this style after a reset, such as `0;1;31`. Colors are the
    /// terminal's own palette, so they follow its theme, and NO_COLOR leaves them out.
    pub fn sgr(&self) -> String {
        let mut sgr = String::from("0");
        for (on, code) in [
            (self.bold, "1"),
            (self.dim, "2"),
            (self.italic, "3"),
            (self.underline, "4"),
            (self.reverse, "7"),
            (self.strike, "9"),
        ] {
            if on {
                sgr.push(';');
                sgr.push_str(code);
            }
        }
        if let Some(color) = self.fg.filter(|_| colors_enabled()) {
            sgr.push(';');
            sgr.push_str(&foreground(color));
        }
        sgr
    }
}

fn colors_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var_os("NO_COLOR").is_none_or(|v| v.is_empty()))
}

fn foreground(color: Color) -> String {
    let code = match color {
        Color::Reset => 39,
        Color::Black => 30,
        Color::DarkRed => 31,
        Color::DarkGreen => 32,
        Color::DarkYellow => 33,
        Color::DarkBlue => 34,
        Color::DarkMagenta => 35,
        Color::DarkCyan => 36,
        Color::Grey => 37,
        Color::DarkGrey => 90,
        Color::Red => 91,
        Color::Green => 92,
        Color::Yellow => 93,
        Color::Blue => 94,
        Color::Magenta => 95,
        Color::Cyan => 96,
        Color::White => 97,
        Color::AnsiValue(n) => return format!("38;5;{n}"),
        Color::Rgb { r, g, b } => return format!("38;2;{r};{g};{b}"),
    };
    code.to_string()
}

/// The styles the TUI uses, in one place.
pub mod theme {
    use super::{Color, Style};

    pub const DIM: Style = Style::PLAIN.dim();
    pub const BOLD: Style = Style::PLAIN.bold();
    pub const USER: Style = Style::PLAIN.fg(Color::DarkCyan).bold();
    pub const TOOL: Style = Style::PLAIN.fg(Color::DarkGreen);
    pub const ERROR: Style = Style::PLAIN.fg(Color::DarkRed);
    pub const NOTICE: Style = Style::PLAIN.fg(Color::DarkYellow);
    pub const CODE: Style = Style::PLAIN.fg(Color::DarkCyan);
    pub const REASONING: Style = Style::PLAIN.dim().italic();
    pub const SPINNER: Style = Style::PLAIN.fg(Color::DarkMagenta);
    pub const CARET: Style = Style::PLAIN.reverse();
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Span {
    pub text: String,
    pub style: Style,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Line {
    pub spans: Vec<Span>,
}

impl Line {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn styled(text: impl Into<String>, style: Style) -> Self {
        let mut line = Line::new();
        line.push(&text.into(), style);
        line
    }

    pub fn plain(text: impl Into<String>) -> Self {
        Self::styled(text, Style::PLAIN)
    }

    /// The line with `text` added at the end.
    pub fn with(mut self, text: &str, style: Style) -> Self {
        self.push(text, style);
        self
    }

    /// Adds `text` at the end, to the last span if it has the same style.
    pub fn push(&mut self, text: &str, style: Style) {
        if text.is_empty() {
            return;
        }
        match self.spans.last_mut() {
            Some(last) if last.style == style => last.text.push_str(text),
            _ => self.spans.push(Span { text: text.to_owned(), style }),
        }
    }

    pub fn append(&mut self, other: Line) {
        for span in other.spans {
            self.push(&span.text, span.style);
        }
    }

    /// The line with `other` added at the end.
    pub fn with_line(mut self, other: Line) -> Self {
        self.append(other);
        self
    }

    /// Columns the line takes.
    pub fn width(&self) -> usize {
        widths(self.spans.iter().flat_map(|s| s.text.chars())).map(|(_, w)| w).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.spans.iter().all(|s| s.text.is_empty())
    }

    /// The text without its styles.
    #[cfg(test)]
    pub fn text(&self) -> String {
        self.spans.iter().map(|s| s.text.as_str()).collect()
    }

    /// The line with `over` laid on every span's style.
    pub fn patched(mut self, over: Style) -> Self {
        for span in &mut self.spans {
            span.style = span.style.patch(over);
        }
        self
    }
}

/// Columns `text` takes on screen.
pub fn width(text: &str) -> usize {
    widths(text.chars()).map(|(_, w)| w).sum()
}

/// Each character with the columns it takes. Control characters take none, since the screen never
/// writes them. An emoji presentation selector widens the character before it to two columns, as
/// terminals that honour it draw the pair: counting too few would let a row overflow.
pub fn widths(chars: impl Iterator<Item = char>) -> impl Iterator<Item = (char, usize)> {
    let mut previous = 0;
    chars.map(move |ch| {
        let width = match ch {
            '\u{fe0f}' if previous == 1 => 1,
            ch if ch.is_control() => 0,
            ch => ch.width().unwrap_or(0),
        };
        previous = width;
        (ch, width)
    })
}

/// `text` as lines of one style, one per line break, with tabs expanded.
pub fn lines(text: &str, style: Style) -> Vec<Line> {
    text.split('\n')
        .map(|line| Line::styled(expand_tabs(line.strip_suffix('\r').unwrap_or(line)), style))
        .collect()
}

/// `line` with each tab replaced by spaces up to the next multiple of four columns.
pub fn expand_tabs(line: &str) -> String {
    if !line.contains('\t') {
        return line.to_owned();
    }
    let mut out = String::with_capacity(line.len() + 8);
    let mut column = 0;
    for (ch, w) in widths(line.chars()) {
        if ch == '\t' {
            let spaces = 4 - column % 4;
            out.extend(std::iter::repeat_n(' ', spaces));
            column += spaces;
        } else {
            out.push(ch);
            column += w;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn measures_wide_and_control_characters() {
        assert_eq!(width("abc"), 3);
        assert_eq!(width("日本"), 4);
        assert_eq!(width("a\x1b[31mb"), 6);
        assert_eq!(width("e\u{301}"), 1);
        assert_eq!(width("\u{2764}\u{fe0f}"), 2);
        assert_eq!(width("\u{1f600}\u{fe0f}"), 2);
    }

    #[test]
    fn joins_spans_of_one_style() {
        let line = Line::plain("a").with("b", Style::PLAIN).with("c", theme::BOLD);
        assert_eq!(line.spans.len(), 2);
        assert_eq!(line.text(), "abc");
        assert_eq!(line.width(), 3);
    }

    #[test]
    fn expands_tabs_to_stops() {
        assert_eq!(expand_tabs("\tx"), "    x");
        assert_eq!(expand_tabs("ab\tx"), "ab  x");
        assert_eq!(expand_tabs("abcd\tx"), "abcd    x");
        let split = lines("one\r\ntwo\tz", Style::PLAIN);
        assert_eq!(split.iter().map(Line::text).collect::<Vec<_>>(), ["one", "two z"]);
    }

    #[test]
    fn styles_become_one_sgr_sequence() {
        assert_eq!(Style::PLAIN.sgr(), "0");
        assert_eq!(theme::REASONING.sgr(), "0;2;3");
        if colors_enabled() {
            assert_eq!(Style::PLAIN.fg(Color::DarkRed).bold().sgr(), "0;1;31");
        }
        assert_eq!(theme::DIM.patch(theme::ERROR), Style::PLAIN.dim().fg(Color::DarkRed));
    }
}
