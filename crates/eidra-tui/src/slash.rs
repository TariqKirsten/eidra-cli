//! The list that opens under the input box while it holds a `/name` being typed: eidra's own
//! commands and the skills the user can invoke, narrowed to those that fit what has been typed,
//! the best first (docs/DESIGN.md §12). Up and Down choose, Tab completes, Enter runs.

use eidra_core::display::clip;

use crate::style::{self, Line, Style, theme};

/// A command or a skill in the list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub name: String,
    /// What may follow the name, such as `[what]`.
    pub hint: String,
    pub description: String,
}

/// The entries that fit `query`, the letters typed after the slash, the best first:
/// - the name itself, then the names that start with it, then those with a later word (after a
///   hyphen) that does, then those whose words' initials do, shorter names first in each;
/// - then the names that hold it, the earlier the better, then those that hold its letters in
///   order, closest together first;
/// - last, from three letters on, the entries with a word in their description that starts with
///   it.
///
/// Case does not matter, and ties go in alphabetical order. With nothing typed, all of them fit.
pub fn search<'a>(entries: &'a [Entry], query: &str) -> Vec<&'a Entry> {
    let query = query.to_lowercase();
    let mut found: Vec<((u8, usize), &Entry)> =
        entries.iter().filter_map(|entry| Some((rank(entry, &query)?, entry))).collect();
    found.sort_by(|(a, x), (b, y)| a.cmp(b).then_with(|| x.name.cmp(&y.name)));
    found.into_iter().map(|(_, entry)| entry).collect()
}

/// How well `entry` fits `query`, lowest best: the kind of fit, then how far from the name
/// itself it is.
fn rank(entry: &Entry, query: &str) -> Option<(u8, usize)> {
    let name = entry.name.to_lowercase();
    if query.is_empty() || name == query {
        return Some((0, 0));
    }
    let words: Vec<&str> = name.split('-').filter(|word| !word.is_empty()).collect();
    let initials: String = words.iter().filter_map(|word| word.chars().next()).collect();
    let described = || {
        let description = entry.description.to_lowercase();
        let mut words = description.split(|c: char| !c.is_alphanumeric());
        query.chars().count() >= 3 && words.any(|word| word.starts_with(query))
    };
    if name.starts_with(query) {
        Some((1, name.len()))
    } else if words.iter().skip(1).any(|word| word.starts_with(query)) {
        Some((2, name.len()))
    } else if query.len() > 1 && initials.starts_with(query) {
        Some((3, name.len()))
    } else if let Some(at) = name.find(query) {
        Some((4, at))
    } else if let Some(span) = spread(&name, query) {
        Some((5, span))
    } else if described() {
        Some((6, 0))
    } else {
        None
    }
}

/// The fewest characters of `name` that hold the letters of `query` in order, if it holds them.
fn spread(name: &str, query: &str) -> Option<usize> {
    let name: Vec<char> = name.chars().collect();
    let query: Vec<char> = query.chars().collect();
    let first = *query.first()?;
    let mut best: Option<usize> = None;
    for start in (0..name.len()).filter(|&i| name[i] == first) {
        let mut matched = 0;
        for (i, &c) in name[start..].iter().enumerate() {
            if c == query[matched] {
                matched += 1;
                if matched == query.len() {
                    best = Some(best.map_or(i + 1, |span| span.min(i + 1)));
                    break;
                }
            }
        }
    }
    best
}

/// The list's rows, `width` columns wide: at most `rows` of `found`, scrolled to keep `selected`
/// in view. Each is the entry's name and hint, then its description, dimmed and cut to fit; the
/// selected one stands out.
pub fn render(found: &[&Entry], selected: usize, rows: usize, width: usize) -> Vec<Line> {
    let rows = rows.min(found.len());
    if rows == 0 {
        return Vec::new();
    }
    let start = (selected + 1).saturating_sub(rows).min(found.len() - rows);
    let shown = &found[start..start + rows];
    let label = |entry: &Entry| match entry.hint.as_str() {
        "" => format!("/{}", entry.name),
        hint => format!("/{} {hint}", entry.name),
    };
    let column = shown.iter().map(|entry| style::width(&label(entry))).max().unwrap_or(0);
    let column = column.min(32).min(width / 2);
    shown
        .iter()
        .enumerate()
        .map(|(i, entry)| {
            let chosen = start + i == selected;
            let mut label = label(entry);
            if style::width(&label) > column {
                label = clip(&label, column.saturating_sub(1));
            }
            let pad = " ".repeat(column.saturating_sub(style::width(&label)));
            let room = width.saturating_sub(column + 5);
            let (name, description) =
                if chosen { (theme::USER, Style::PLAIN) } else { (Style::PLAIN, theme::DIM) };
            Line::plain("  ")
                .with(&label, name)
                .with(&format!("{pad}  "), Style::PLAIN)
                .with(&clip(&entry.description, room), description)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entries(names: &[&str]) -> Vec<Entry> {
        names
            .iter()
            .map(|name| Entry {
                name: (*name).into(),
                hint: String::new(),
                description: format!("what {name} does"),
            })
            .collect()
    }

    fn names(found: Vec<&Entry>) -> Vec<&str> {
        found.into_iter().map(|entry| entry.name.as_str()).collect()
    }

    #[test]
    fn the_best_fits_come_first() {
        let all = entries(&[
            "compact",
            "diff",
            "diagnose-crash",
            "hf-cli",
            "help",
            "new",
            "omarchy",
            "reasoning",
            "stop",
            "tasks",
            "undo",
        ]);
        assert_eq!(names(search(&all, "")).len(), all.len());
        assert_eq!(names(search(&all, ""))[..3], ["compact", "diagnose-crash", "diff"]);
        // The name, then names starting with it, then a later word starting with it.
        assert_eq!(names(search(&all, "diff")), ["diff"]);
        assert_eq!(names(search(&all, "di")), ["diff", "diagnose-crash"]);
        assert_eq!(names(search(&all, "cr")), ["diagnose-crash"]);
        assert_eq!(names(search(&all, "cli")), ["hf-cli"]);
        // Initials, then letters in order, closest together first.
        assert_eq!(names(search(&all, "dc")), ["diagnose-crash"]);
        assert_eq!(names(search(&all, "hcl")), ["hf-cli"]);
        assert_eq!(names(search(&all, "ask")), ["tasks"]);
        assert_eq!(names(search(&all, "as")), ["tasks", "reasoning", "diagnose-crash"]);
        assert_eq!(names(search(&all, "an")), ["diagnose-crash", "reasoning"]);
        assert_eq!(names(search(&all, "HE")), ["help"]);
        assert!(search(&all, "zzz").is_empty());
    }

    #[test]
    fn descriptions_count_last_and_from_three_letters() {
        let mut all = entries(&["compact", "summary-of-it"]);
        all[0].description = "go on from a summary of the conversation".into();
        assert_eq!(names(search(&all, "summ")), ["summary-of-it", "compact"]);
        assert_eq!(names(search(&all, "su")), ["summary-of-it"]);
    }

    #[test]
    fn rows_keep_the_choice_in_view_and_fit_the_width() {
        let mut all = entries(&["alpha", "beta", "gamma", "delta", "epsilon"]);
        all[1].hint = "[what]".into();
        all[2].description = "a description far too long to fit in the room the list has".into();
        let found: Vec<&Entry> = all.iter().collect();
        let text = |lines: Vec<Line>| lines.iter().map(Line::text).collect::<Vec<_>>();
        let rows = text(render(&found, 0, 3, 40));
        assert_eq!(
            rows,
            [
                "  /alpha        what alpha does",
                "  /beta [what]  what beta does",
                "  /gamma        a description far too l…",
            ]
        );
        assert!(rows.iter().all(|row| style::width(row) <= 40));
        // Choosing past the last row shown scrolls the list.
        let rows = text(render(&found, 4, 3, 40));
        assert!(rows[0].starts_with("  /gamma ") && rows[2].starts_with("  /epsilon "), "{rows:?}");
        let chosen = render(&found, 1, 3, 40);
        let stands_out = |line: &Line| line.spans.iter().any(|span| span.style == theme::USER);
        assert!(stands_out(&chosen[1]) && !stands_out(&chosen[0]) && !stands_out(&chosen[2]));
        assert!(render(&[], 0, 3, 40).is_empty());
    }
}
