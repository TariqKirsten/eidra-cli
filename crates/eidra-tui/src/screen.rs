//! The renderer. eidra draws inline, below the prompt it was started from: finished output goes
//! into the terminal's own scrollback, and a live region at the bottom (the streaming tail, the
//! input box, the status line) is redrawn in place. A draw moves to the live region's top, clears
//! everything below, writes the newly finished lines (which push the region down, and older lines
//! into the scrollback), and writes the region again, inside a synchronized update so that the
//! terminal shows the result at once.
//!
//! Between draws the cursor rests at the live region's top-left, so a draw starts there without
//! knowing where the region is on screen. That also survives a terminal reflowing its contents on
//! resize, since the cursor moves with the cell it is on. Live rows are wrapped one column short of
//! the width, so the terminal never wraps them itself, and the region is kept shorter than the
//! terminal, so its top is always on screen to come back to.

use std::io::{self, Write};

use crossterm::cursor::MoveUp;
use crossterm::queue;
use crossterm::terminal::{BeginSynchronizedUpdate, Clear, ClearType, EndSynchronizedUpdate};

use crate::style::Line;
use crate::wrap::wrap_all;

pub struct Screen<W: Write> {
    out: W,
    width: usize,
    height: usize,
    /// The live region as last drawn, wrapped.
    live: Vec<Line>,
    /// Set after a resize: draw even if nothing changed.
    stale: bool,
}

impl<W: Write> Screen<W> {
    pub fn new(out: W, width: u16, height: u16) -> Self {
        let mut screen = Screen { out, width: 0, height: 0, live: Vec::new(), stale: true };
        screen.resize(width, height);
        screen
    }

    pub fn resize(&mut self, width: u16, height: u16) {
        self.width = usize::from(width.max(2));
        self.height = usize::from(height.max(2));
        self.stale = true;
    }

    /// Columns for finished lines.
    pub fn width(&self) -> usize {
        self.width
    }

    /// Columns for live rows.
    pub fn live_width(&self) -> usize {
        self.width - 1
    }

    /// Rows the live region may take.
    pub fn live_height(&self) -> usize {
        self.height - 1
    }

    /// Writes `finished` into the scrollback, above the live region, and redraws the live region
    /// as `live`. Both are wrapped here; a live region that is too tall loses its top rows.
    pub fn draw(&mut self, finished: &[Line], live: &[Line]) -> io::Result<()> {
        let finished = wrap_all(finished, self.width);
        let mut live = wrap_all(live, self.live_width());
        let excess = live.len().saturating_sub(self.live_height());
        live.drain(..excess);
        if finished.is_empty() && live == self.live && !self.stale {
            return Ok(());
        }

        // The frame is built whole, then written at once.
        let mut frame = Vec::new();
        queue!(frame, BeginSynchronizedUpdate)?;
        frame.extend_from_slice(b"\r");
        queue!(frame, Clear(ClearType::FromCursorDown))?;
        for line in &finished {
            write_line(&mut frame, line)?;
            frame.extend_from_slice(b"\r\n");
        }
        for (i, row) in live.iter().enumerate() {
            if i > 0 {
                frame.extend_from_slice(b"\r\n");
            }
            write_line(&mut frame, row)?;
        }
        // Back to the region's top. (Zero would still move one row.)
        if live.len() > 1 {
            queue!(frame, MoveUp((live.len() - 1) as u16))?;
        }
        frame.extend_from_slice(b"\r");
        queue!(frame, EndSynchronizedUpdate)?;
        self.out.write_all(&frame)?;
        self.out.flush()?;
        self.live = live;
        self.stale = false;
        Ok(())
    }
}

/// One line, its styles as SGR sequences. Control characters are never written: model and tool
/// output must not be able to move the cursor or change the terminal's state.
fn write_line(out: &mut impl Write, line: &Line) -> io::Result<()> {
    for span in &line.spans {
        let text: String = span.text.chars().filter(|ch| !ch.is_control()).collect();
        write!(out, "\x1b[{}m{text}", span.style.sgr())?;
    }
    out.write_all(b"\x1b[0m")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::style::{theme, widths};

    /// Just enough of a terminal to check what the screen draws: printing, with a deferred wrap at
    /// the right margin; carriage return; line feed, scrolling the top row into the scrollback at
    /// the bottom; cursor up; erase below. Styles and modes are ignored.
    struct Terminal {
        width: usize,
        height: usize,
        rows: Vec<Vec<char>>,
        scrollback: Vec<String>,
        row: usize,
        col: usize,
        wrap_pending: bool,
        /// Rows the terminal had to wrap itself, which the screen should never cause.
        wrapped: usize,
    }

    impl Terminal {
        fn new(width: usize, height: usize) -> Self {
            Terminal {
                width,
                height,
                rows: vec![vec![' '; width]; height],
                scrollback: Vec::new(),
                row: 0,
                col: 0,
                wrap_pending: false,
                wrapped: 0,
            }
        }

        fn feed(&mut self, bytes: &[u8]) {
            let text = std::str::from_utf8(bytes).unwrap();
            let mut chars = widths(text.chars());
            while let Some((ch, width)) = chars.next() {
                match ch {
                    '\x1b' => {
                        assert_eq!(chars.next().map(|(c, _)| c), Some('['));
                        let mut params = String::new();
                        let end = loop {
                            let (c, _) = chars.next().unwrap();
                            if ('\x40'..='\x7e').contains(&c) {
                                break c;
                            }
                            params.push(c);
                        };
                        self.csi(&params, end);
                    }
                    '\r' => {
                        self.col = 0;
                        self.wrap_pending = false;
                    }
                    '\n' => {
                        self.line_feed();
                        self.wrap_pending = false;
                    }
                    ch => self.print(ch, width),
                }
            }
        }

        fn csi(&mut self, params: &str, end: char) {
            match end {
                'A' => {
                    let n: usize = params.parse().unwrap_or(1);
                    assert!(n > 0, "CSI 0 A moves a row in a real terminal");
                    self.row = self.row.saturating_sub(n);
                    self.wrap_pending = false;
                }
                'J' => {
                    assert!(params.is_empty(), "only erase-below is expected");
                    self.rows[self.row][self.col..].fill(' ');
                    for row in &mut self.rows[self.row + 1..] {
                        row.fill(' ');
                    }
                }
                'm' => {}
                'h' | 'l' => assert!(params.starts_with('?')),
                _ => panic!("unexpected sequence CSI {params}{end}"),
            }
        }

        fn line_feed(&mut self) {
            if self.row + 1 < self.height {
                self.row += 1;
            } else {
                let top = self.rows.remove(0);
                self.scrollback.push(text(&top));
                self.rows.push(vec![' '; self.width]);
            }
        }

        fn print(&mut self, ch: char, width: usize) {
            if width == 0 {
                return;
            }
            if self.wrap_pending || self.col + width > self.width {
                self.col = 0;
                self.line_feed();
                self.wrap_pending = false;
                self.wrapped += 1;
            }
            self.rows[self.row][self.col] = ch;
            if width == 2 {
                self.rows[self.row][self.col + 1] = '\0';
            }
            self.col += width;
            if self.col == self.width {
                self.col -= 1;
                self.wrap_pending = true;
            }
        }

        fn screen(&self) -> Vec<String> {
            self.rows.iter().map(|row| text(row)).collect()
        }
    }

    fn text(row: &[char]) -> String {
        row.iter().filter(|&&c| c != '\0').collect::<String>().trim_end().to_owned()
    }

    fn lines(texts: &[&str]) -> Vec<Line> {
        texts.iter().map(|t| Line::plain(*t)).collect()
    }

    /// A screen and terminal of one size, with a shell prompt on the first row.
    fn setup(width: u16, height: u16) -> (Screen<Vec<u8>>, Terminal) {
        let mut terminal = Terminal::new(width.into(), height.into());
        terminal.feed(b"$ eidra\r\n");
        (Screen::new(Vec::new(), width, height), terminal)
    }

    fn draw(screen: &mut Screen<Vec<u8>>, terminal: &mut Terminal, done: &[&str], live: &[&str]) {
        screen.draw(&lines(done), &lines(live)).unwrap();
        terminal.feed(&std::mem::take(&mut screen.out));
    }

    #[test]
    fn draws_below_the_prompt_and_rests_at_the_region_top() {
        let (mut screen, mut terminal) = setup(20, 6);
        draw(&mut screen, &mut terminal, &["header"], &["live one", "live two"]);
        assert_eq!(terminal.screen(), ["$ eidra", "header", "live one", "live two", "", ""]);
        assert_eq!((terminal.row, terminal.col), (2, 0));
    }

    #[test]
    fn redraws_in_place_as_the_region_grows_and_shrinks() {
        let (mut screen, mut terminal) = setup(20, 6);
        draw(&mut screen, &mut terminal, &[], &["a"]);
        draw(&mut screen, &mut terminal, &[], &["a", "b", "c"]);
        assert_eq!(terminal.screen(), ["$ eidra", "a", "b", "c", "", ""]);
        draw(&mut screen, &mut terminal, &[], &["x"]);
        assert_eq!(terminal.screen(), ["$ eidra", "x", "", "", "", ""]);
        assert_eq!(terminal.row, 1);
        assert!(terminal.scrollback.is_empty());
    }

    #[test]
    fn finished_lines_scroll_away_and_the_live_region_never_does() {
        let (mut screen, mut terminal) = setup(20, 5);
        draw(&mut screen, &mut terminal, &[], &["input", "status"]);
        for i in 0..8 {
            draw(&mut screen, &mut terminal, &[&format!("line {i}")], &["input", "status"]);
        }
        assert_eq!(
            terminal.scrollback,
            ["$ eidra", "line 0", "line 1", "line 2", "line 3", "line 4"]
        );
        assert_eq!(terminal.screen(), ["line 5", "line 6", "line 7", "input", "status"]);
        assert_eq!(terminal.row, 3);

        // A taller region at the bottom pushes finished lines up, not itself.
        draw(&mut screen, &mut terminal, &[], &["tail", "input", "status"]);
        assert_eq!(terminal.screen(), ["line 6", "line 7", "tail", "input", "status"]);
        assert_eq!(terminal.scrollback.last().unwrap(), "line 5");
        assert_eq!(terminal.row, 2);
    }

    #[test]
    fn keeps_the_region_shorter_than_the_terminal() {
        let (mut screen, mut terminal) = setup(20, 5);
        let rows: Vec<String> = (0..10).map(|i| format!("row {i}")).collect();
        let rows: Vec<&str> = rows.iter().map(String::as_str).collect();
        draw(&mut screen, &mut terminal, &[], &rows);
        assert_eq!(terminal.screen(), ["$ eidra", "row 6", "row 7", "row 8", "row 9"]);
        assert_eq!(terminal.row, 1);
        draw(&mut screen, &mut terminal, &["done"], &["row 9"]);
        assert_eq!(terminal.screen(), ["$ eidra", "done", "row 9", "", ""]);
    }

    #[test]
    fn wraps_live_rows_short_of_the_margin_and_finished_lines_to_it() {
        let (mut screen, mut terminal) = setup(10, 6);
        draw(&mut screen, &mut terminal, &["0123456789"], &["abcdefghijkl"]);
        assert_eq!(terminal.screen(), ["$ eidra", "0123456789", "abcdefghi", "jkl", "", ""]);
        assert_eq!(terminal.row, 2);
    }

    #[test]
    fn redraws_at_a_new_size() {
        let (mut screen, mut terminal) = setup(20, 6);
        draw(&mut screen, &mut terminal, &[], &["one two three four", "status"]);
        // A terminal that cuts rows rather than reflowing them.
        terminal.width = 10;
        for row in &mut terminal.rows {
            row.truncate(10);
        }
        screen.resize(10, 6);
        draw(&mut screen, &mut terminal, &[], &["one two three four", "status"]);
        assert_eq!(terminal.screen(), ["$ eidra", "one two", "three", "four", "status", ""]);
    }

    #[test]
    fn skips_frames_that_change_nothing() {
        let (mut screen, _) = setup(20, 6);
        screen.draw(&[], &lines(&["same"])).unwrap();
        screen.out.clear();
        screen.draw(&[], &lines(&["same"])).unwrap();
        assert!(screen.out.is_empty());
        screen.draw(&lines(&["new"]), &lines(&["same"])).unwrap();
        assert!(!screen.out.is_empty());
    }

    #[test]
    fn the_terminal_holds_every_finished_line_then_the_live_region() {
        // A fixed xorshift, so that a failure repeats.
        let mut state = 0x2545_f491_4f6c_dd1d_u64;
        let mut random = move |n: usize| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state % n as u64) as usize
        };
        let words =
            ["a", "word", "longer-word", "日本語", "—", "😀", "x", "", "unbreakable_identifier"];
        let line = |random: &mut dyn FnMut(usize) -> usize| {
            let count = random(14);
            let text: Vec<&str> = (0..count).map(|_| words[random(words.len())]).collect();
            Line::styled(text.join(" "), if random(2) == 0 { theme::DIM } else { theme::BOLD })
        };
        for (width, height) in [(12, 5), (20, 8), (33, 12), (80, 24)] {
            let (mut screen, mut terminal) = setup(width, height);
            let mut expected = vec!["$ eidra".to_owned()];
            for _ in 0..300 {
                let finished: Vec<Line> = (0..random(4)).map(|_| line(&mut random)).collect();
                let rows = random(usize::from(height) + 4);
                let live: Vec<Line> = (0..rows).map(|_| line(&mut random)).collect();
                screen.draw(&finished, &live).unwrap();
                terminal.feed(&std::mem::take(&mut screen.out));

                expected.extend(wrap_all(&finished, screen.width()).iter().map(trimmed));
                let region: Vec<String> = screen.live.iter().map(trimmed).collect();
                let shown: Vec<String> =
                    terminal.scrollback.iter().cloned().chain(terminal.screen()).collect();
                let (before, after) = shown.split_at(expected.len());
                assert_eq!(before, expected);
                assert_eq!(after[..region.len()], region);
                assert!(after[region.len()..].iter().all(String::is_empty), "{shown:#?}");
                let top = terminal.scrollback.len() + terminal.row;
                assert_eq!((top, terminal.col), (expected.len(), 0));
                assert_eq!(terminal.wrapped, 0);
            }
        }
    }

    fn trimmed(line: &Line) -> String {
        line.text().trim_end().to_owned()
    }

    #[test]
    fn writes_styles_and_never_control_characters() {
        let mut screen = Screen::new(Vec::new(), 40, 6);
        let line = Line::styled("bold", theme::BOLD).with(" \x1b[2Jx\x07", theme::DIM);
        screen.draw(&[line], &[]).unwrap();
        let out = String::from_utf8(screen.out).unwrap();
        assert!(out.starts_with("\x1b[?2026h\r\x1b[J"), "{out:?}");
        assert!(out.contains("\x1b[0;1mbold\x1b[0;2m [2Jx\x1b[0m\r\n"), "{out:?}");
        assert!(out.ends_with("\r\x1b[?2026l"), "{out:?}");
    }
}
