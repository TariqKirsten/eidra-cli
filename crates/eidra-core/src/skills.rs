//! Skills (docs/DESIGN.md §11.1), in the Agent Skills format: a folder holding a SKILL.md, whose
//! frontmatter names the skill and says when to use it, and whose body is its instructions. The
//! folder's other files, such as scripts, references and templates, are for the agent to read or
//! run when the instructions say so. eidra finds skills in `~/.agents/skills`, which agents share.
//!
//! Only names and descriptions go into the system prompt. A skill's instructions reach the
//! conversation when it is used: as the result of the `skill` tool, or after the user's message
//! when they invoke it with `/name`.

use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use crate::display::clip;

/// The longest name the format allows.
const NAME_MAX: usize = 64;
/// The longest description the format allows. A longer one is cut there, since every session's
/// system prompt carries it.
const DESCRIPTION_MAX: usize = 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skill {
    pub name: String,
    /// What it is for and when to use it, on one line.
    pub description: String,
    /// Its folder, which the paths in its instructions start from.
    pub dir: PathBuf,
    /// What may follow `/name` (`argument-hint`), or nothing.
    pub hint: String,
    /// The model may load it; not with `disable-model-invocation: true`.
    pub model: bool,
    /// The user may invoke it with `/name`; not with `user-invocable: false`.
    pub user: bool,
}

/// The skills found in a directory, and the folders left out, each with why.
#[derive(Debug, Default)]
pub struct Skills {
    pub skills: Vec<Skill>,
    pub skipped: Vec<String>,
}

/// `~/.agents/skills`, where agents share skills; none without a home.
pub fn dir() -> Option<PathBuf> {
    match std::env::var_os("HOME") {
        Some(home) if !home.is_empty() && home != "/" => {
            Some(PathBuf::from(home).join(".agents").join("skills"))
        }
        _ => None,
    }
}

/// The skills in `dir`: each folder in it with a SKILL.md, in the order of the folders' names. A
/// folder whose SKILL.md cannot be read, or lacks a usable name or a description, is left out, and
/// so is a skill of a name already taken; `skipped` says why.
pub fn find(dir: &Path) -> Skills {
    let mut found = Skills::default();
    let Ok(entries) = fs::read_dir(dir) else {
        return found;
    };
    let mut folders: Vec<(String, PathBuf)> = entries
        .flatten()
        .filter_map(|entry| Some((entry.file_name().into_string().ok()?, entry.path())))
        .filter(|(name, _)| !name.starts_with('.'))
        .collect();
    folders.sort();
    for (folder, path) in folders {
        if !path.is_dir() {
            // A link to a folder that is not there, as a container sees a link out of its mounts.
            if let Ok(target) = fs::read_link(&path)
                && !path.exists()
            {
                let target = target.display();
                found.skipped.push(format!("{folder}: a link to {target}, which is not there"));
            }
            continue;
        }
        match read(&path, &folder) {
            Ok(skill) if found.skills.iter().any(|s| s.name == skill.name) => {
                found.skipped.push(format!("{folder}: another skill is called {}", skill.name));
            }
            Ok(skill) => found.skills.push(skill),
            Err(why) => found.skipped.push(format!("{folder}: {why}")),
        }
    }
    found
}

impl Skills {
    pub fn get(&self, name: &str) -> Option<&Skill> {
        self.skills.iter().find(|skill| skill.name == name)
    }

    /// A line for each skill the model may load, its name and its description, for the system
    /// prompt; empty if there are none.
    pub fn listing(&self) -> String {
        let lines: Vec<String> = self
            .skills
            .iter()
            .filter(|skill| skill.model)
            .map(|skill| format!("- {}: {}", skill.name, skill.description))
            .collect();
        lines.join("\n")
    }
}

impl Skill {
    /// Its instructions: SKILL.md after the frontmatter.
    pub fn instructions(&self) -> Result<String, String> {
        let path = self.dir.join("SKILL.md");
        let text = fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        let (_, body) = frontmatter(&text)?;
        Ok(body.trim().to_owned())
    }

    /// Its instructions in the profile's `skill` wrapping, as the conversation carries them.
    pub fn wrapped(&self, template: &str, instructions: &str) -> String {
        let (before, after) = template.split_once("{instructions}").unwrap_or((template, ""));
        format!("{}{instructions}{}", self.fill(before), self.fill(after))
    }

    /// What comes before its instructions in the profile's `skill` wrapping: how to tell that a
    /// message carries them.
    pub fn heading(&self, template: &str) -> String {
        self.fill(template.split("{instructions}").next().unwrap_or_default())
    }

    fn fill(&self, text: &str) -> String {
        text.replace("{name}", &self.name).replace("{dir}", &self.dir.display().to_string())
    }
}

/// The skill in `dir`, from its SKILL.md; `folder` is the folder's name, which a skill without a
/// name goes by.
fn read(dir: &Path, folder: &str) -> Result<Skill, String> {
    let text = match fs::read_to_string(dir.join("SKILL.md")) {
        Ok(text) => text,
        Err(e) if e.kind() == ErrorKind::NotFound => return Err("there is no SKILL.md".into()),
        Err(e) => return Err(format!("SKILL.md: {e}")),
    };
    let (fields, _) = frontmatter(&text)?;
    let field = |key: &str| {
        fields.iter().find(|(k, _)| k == key).map(|(_, value)| value.trim()).unwrap_or_default()
    };
    let name = Some(field("name")).filter(|name| !name.is_empty()).unwrap_or(folder);
    if name.len() > NAME_MAX || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        return Err(format!(
            "its name, {name:?}, is not letters, digits and hyphens, at most {NAME_MAX} of them"
        ));
    }
    let description = one_line(field("description"));
    if description.is_empty() {
        return Err("its frontmatter has no description".into());
    }
    let flag = |key: &str, default: bool| match field(key) {
        value if value.eq_ignore_ascii_case("true") => true,
        value if value.eq_ignore_ascii_case("false") => false,
        _ => default,
    };
    Ok(Skill {
        name: name.to_owned(),
        description: clip(&description, DESCRIPTION_MAX - 1),
        dir: dir.to_path_buf(),
        hint: one_line(field("argument-hint")),
        model: !flag("disable-model-invocation", false),
        user: flag("user-invocable", true),
    })
}

/// `text` with every run of whitespace, line breaks among them, made one space.
fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// SKILL.md's frontmatter, as its top-level keys with the values of those that are text, and the
/// body after it. Frontmatter is YAML between two lines of `---`. This reads the YAML that it is
/// written in: plain, quoted and block (`|`, `>`) values, over several lines or one, and comments.
/// Maps and lists on the lines below a key, such as `metadata` or a list of `allowed-tools`, are
/// passed over, since eidra reads none of them; one written on its key's line is kept as written,
/// as `argument-hint: [pr-number]` is meant. It is lenient where YAML is not: a plain value may
/// hold `: `.
fn frontmatter(text: &str) -> Result<(Fields, &str), String> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut lines = text.split_inclusive('\n');
    let first = lines.next().unwrap_or_default();
    if first.trim_end() != "---" {
        return Err("SKILL.md does not start with frontmatter, a line of ---".into());
    }
    let mut yaml = Vec::new();
    let mut end = first.len();
    let mut closed = false;
    for line in lines {
        end += line.len();
        if line.trim_end() == "---" {
            closed = true;
            break;
        }
        yaml.push(line.trim_end_matches(['\n', '\r']));
    }
    if !closed {
        return Err("its frontmatter has no closing line of ---".into());
    }
    Ok((fields(&yaml), &text[end..]))
}

/// Frontmatter's top-level keys, each with its value.
type Fields = Vec<(String, String)>;

/// The top-level keys of frontmatter's lines, each with its value.
fn fields(lines: &[&str]) -> Fields {
    let indented = |line: &str| line.starts_with([' ', '\t']) || line.trim().is_empty();
    let mut fields = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        i += 1;
        if indented(line) || line.starts_with('#') {
            continue;
        }
        let Some((key, rest)) = line.split_once(':') else {
            continue;
        };
        // The lines under the key: more of its value, or a nested map or list.
        let start = i;
        while i < lines.len() && indented(lines[i]) {
            i += 1;
        }
        let key = key.trim().trim_matches(['"', '\'']).to_owned();
        fields.push((key, value(rest, &lines[start..i])));
    }
    fields
}

/// A key's value: `rest` of its line, and the lines `below` it.
fn value(rest: &str, below: &[&str]) -> String {
    let rest = rest.trim();
    match rest.chars().next() {
        Some(style @ ('|' | '>')) => block(style == '>', below),
        Some(quote @ ('"' | '\'')) => quoted(&rest[1..], below, quote),
        Some(_) => plain(Some(rest), below),
        // Nothing on the key's line: the value is below, unless that is a nested map or list.
        None => {
            let at = below.iter().position(|line| !line.trim().is_empty());
            match at.map(|i| (below[i].trim(), &below[i + 1..])) {
                Some((first, after)) if first.starts_with(['"', '\'']) => value(first, after),
                Some((first, _)) if nested(first) => String::new(),
                _ => plain(None, below),
            }
        }
    }
}

/// Whether `line` is an item of a list or a key of a map.
fn nested(line: &str) -> bool {
    let key = |(key, after): (&str, &str)| {
        !key.contains(char::is_whitespace) && (after.is_empty() || after.starts_with([' ', '\t']))
    };
    line == "-" || line.starts_with("- ") || line.split_once(':').is_some_and(key)
}

/// A plain value, from `first` on the key's line and the lines below, which fold into it with
/// spaces; a blank line among them is a line break. A ` #` starts a comment.
fn plain(first: Option<&str>, below: &[&str]) -> String {
    fn uncomment(line: &str) -> &str {
        let line = line.trim();
        if line.starts_with('#') {
            return "";
        }
        line.find(" #").or_else(|| line.find("\t#")).map_or(line, |i| line[..i].trim_end())
    }
    let mut out = first.map(uncomment).unwrap_or_default().to_owned();
    for line in below {
        if line.trim().is_empty() {
            out.push('\n');
        } else if !uncomment(line).is_empty() {
            if !out.is_empty() && !out.ends_with('\n') {
                out.push(' ');
            }
            out.push_str(uncomment(line));
        }
    }
    out.trim().to_owned()
}

/// A block value, the lines below the key: `|` keeps their line breaks, `>` folds them into
/// spaces, and a blank line is a line break either way.
fn block(folded: bool, below: &[&str]) -> String {
    let indent = below
        .iter()
        .find(|line| !line.trim().is_empty())
        .map_or(0, |line| line.len() - line.trim_start().len());
    let mut out = String::new();
    for line in below {
        if line.trim().is_empty() {
            out.push('\n');
            continue;
        }
        let start = (line.len() - line.trim_start().len()).min(indent);
        if !out.is_empty() && !out.ends_with('\n') {
            out.push(if folded { ' ' } else { '\n' });
        }
        out.push_str(line[start..].trim_end());
    }
    out.trim().to_owned()
}

/// A quoted value: `text` after the opening quote, then the lines below, which fold into it with
/// spaces, up to the closing quote. In double quotes a backslash escapes; in single quotes `''`
/// is a quote.
fn quoted(text: &str, below: &[&str], quote: char) -> String {
    let mut text = text.to_owned();
    for line in below {
        text.push(' ');
        text.push_str(line.trim());
    }
    let mut out = String::new();
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' if quote == '"' => match chars.next() {
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some('u') => {
                    let hex: String = chars.by_ref().take(4).collect();
                    out.extend(u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32));
                }
                Some(other) => out.push(other),
                None => {}
            },
            '\'' if quote == '\'' && chars.peek() == Some(&'\'') => {
                chars.next();
                out.push('\'');
            }
            c if c == quote => break,
            c => out.push(c),
        }
    }
    out.trim().to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fields_of(yaml: &str) -> Vec<(String, String)> {
        let text = format!("---\n{yaml}\n---\nbody\n");
        frontmatter(&text).unwrap().0
    }

    fn field(yaml: &str, key: &str) -> String {
        let fields = fields_of(yaml);
        fields.into_iter().find(|(k, _)| k == key).map(|(_, v)| v).unwrap_or_default()
    }

    #[test]
    fn reads_the_ways_frontmatter_writes_a_value() {
        assert_eq!(field("name: pdf", "name"), "pdf");
        assert_eq!(
            field("description: Use when: the user asks", "description"),
            "Use when: the user asks"
        );
        assert_eq!(
            field("description: \"Say \\\"hi\\\"\\tnow \\u00e9\"", "description"),
            "Say \"hi\"\tnow é"
        );
        assert_eq!(field("description: 'It''s here' # a comment", "description"), "It's here");
        assert_eq!(
            field("disable-model-invocation: true  # for user-only", "disable-model-invocation"),
            "true"
        );
        // Folded and literal blocks, and plain and quoted values over several lines.
        let folded = "description: >\n  REQUIRED for this.\n  Use when that.\n\n  Then more.";
        assert_eq!(field(folded, "description"), "REQUIRED for this. Use when that.\nThen more.");
        let literal = "description: |-\n  one\n    two\nname: x";
        assert_eq!(field(literal, "description"), "one\n  two");
        assert_eq!(field(literal, "name"), "x");
        assert_eq!(
            field("description: starts here\n  and goes on", "description"),
            "starts here and goes on"
        );
        assert_eq!(
            field("description: \"starts here\n  and goes on\"", "description"),
            "starts here and goes on"
        );
        assert_eq!(
            field("description:\n  Use this when: asked", "description"),
            "Use this when: asked"
        );
        let below = "description:\n  \"Solve it, and\n  check it.\"\nname: x";
        assert_eq!(field(below, "description"), "Solve it, and check it.");
    }

    #[test]
    fn passes_over_nested_maps_and_lists() {
        let yaml = "name: x\nmetadata:\n  author: me\n  version: \"1\"\nallowed-tools:\n  - Read\n  - Bash\ntools: [Read, Grep]\n# a comment\ndescription: d";
        let fields = fields_of(yaml);
        let keys: Vec<&str> = fields.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(keys, ["name", "metadata", "allowed-tools", "tools", "description"]);
        assert_eq!(field(yaml, "metadata"), "");
        assert_eq!(field(yaml, "allowed-tools"), "");
        assert_eq!(field(yaml, "tools"), "[Read, Grep]");
        assert_eq!(field(yaml, "description"), "d");
    }

    #[test]
    fn finds_the_body_after_the_frontmatter() {
        let text = "\u{feff}---\r\nname: x\r\ndescription: d\r\n---\r\n\r\n# X\r\nDo it.\r\n";
        let (fields, body) = frontmatter(text).unwrap();
        assert_eq!(fields[1], ("description".to_owned(), "d".to_owned()));
        assert_eq!(body, "\r\n# X\r\nDo it.\r\n");
        assert!(frontmatter("# no frontmatter\n").unwrap_err().contains("does not start"));
        assert!(frontmatter("---\nname: x\n").unwrap_err().contains("no closing"));
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("eidra-skills-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn skill(dir: &Path, folder: &str, text: &str) {
        fs::create_dir_all(dir.join(folder)).unwrap();
        fs::write(dir.join(folder).join("SKILL.md"), text).unwrap();
    }

    #[test]
    fn finds_skills_in_order_and_says_why_it_left_any_out() {
        let dir = scratch("find");
        skill(
            &dir,
            "pdf",
            "---\nname: pdf\ndescription: >\n  PDF files:\n  read and fill them.\n---\n# PDF\n\nRun scripts/fill.py.\n",
        );
        skill(
            &dir,
            "review",
            "---\ndescription: Review a PR.\nargument-hint: [pr-number]\ndisable-model-invocation: true\n---\nReview $ARGUMENTS.\n",
        );
        skill(
            &dir,
            "helper",
            "---\nname: helper\ndescription: For the model.\nuser-invocable: false\n---\nHelp.\n",
        );
        skill(&dir, "pdf-copy", "---\nname: pdf\ndescription: Another.\n---\n");
        skill(&dir, "bad name", "---\ndescription: d\n---\n");
        skill(&dir, "nothing", "---\nname: nothing\n---\n");
        skill(&dir, ".hidden", "---\nname: hidden\ndescription: d\n---\n");
        fs::create_dir_all(dir.join("empty")).unwrap();
        fs::write(dir.join("README.md"), "not a skill").unwrap();
        std::os::unix::fs::symlink("/nowhere/at/all", dir.join("gone")).unwrap();

        let found = find(&dir);
        let names: Vec<&str> = found.skills.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["helper", "pdf", "review"]);
        assert_eq!(
            found.skipped,
            [
                "bad name: its name, \"bad name\", is not letters, digits and hyphens, at most 64 of them",
                "empty: there is no SKILL.md",
                "gone: a link to /nowhere/at/all, which is not there",
                "nothing: its frontmatter has no description",
                "pdf-copy: another skill is called pdf",
            ]
        );
        let pdf = found.get("pdf").unwrap();
        assert_eq!(pdf.description, "PDF files: read and fill them.");
        assert_eq!(pdf.dir, dir.join("pdf"));
        assert!(pdf.model && pdf.user);
        assert_eq!(pdf.instructions().unwrap(), "# PDF\n\nRun scripts/fill.py.");
        let review = found.get("review").unwrap();
        assert_eq!((review.hint.as_str(), review.model, review.user), ("[pr-number]", false, true));
        assert!(!found.get("helper").unwrap().user);
        // The model is told only of the skills it may load.
        assert_eq!(
            found.listing(),
            "- helper: For the model.\n- pdf: PDF files: read and fill them."
        );
        assert!(find(&dir.join("missing")).skills.is_empty());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_long_description_is_cut_to_the_formats_limit() {
        let dir = scratch("long");
        skill(
            &dir,
            "long",
            &format!("---\nname: long\ndescription: {}\n---\n", "word ".repeat(400)),
        );
        let found = find(&dir);
        let description = &found.skills[0].description;
        assert_eq!(description.chars().count(), DESCRIPTION_MAX);
        assert!(description.ends_with('…'));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn wraps_instructions_in_the_profiles_template() {
        let skill = Skill {
            name: "pdf".into(),
            description: "d".into(),
            dir: PathBuf::from("/skills/pdf"),
            hint: String::new(),
            model: true,
            user: true,
        };
        let template = "[The skill {name}, from {dir}.]\n\n{instructions}\n[end]";
        assert_eq!(
            skill.wrapped(template, "Do {name}."),
            "[The skill pdf, from /skills/pdf.]\n\nDo {name}.\n[end]"
        );
        assert_eq!(skill.heading(template), "[The skill pdf, from /skills/pdf.]\n\n");
    }
}
