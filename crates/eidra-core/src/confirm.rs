//! Which commands need the user's go-ahead (docs/DESIGN.md §7).
//!
//! Commands run freely unless one of the user's patterns matches. A pattern is a command and its
//! leading words: `git` matches every git command, `git push` only pushes. Matching works on
//! parsed commands, not on the text: a command line is parsed with tree-sitter-bash, and every
//! command in it is checked, including those in pipelines, `&&` lists, subshells, `$(...)`,
//! `bash -c` scripts and after wrappers such as `sudo` or `env`. Anything that cannot be analysed
//! asks.
//!
//! This guards against the model's mistakes. It is not a security boundary: a script the model
//! writes and runs can do anything.

use std::collections::HashSet;

use tree_sitter::{Node, Parser};

/// Whether a command may run as it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Run,
    /// It needs approval, for these reasons.
    Ask(Vec<String>),
}

pub struct ConfirmPolicy {
    patterns: Vec<Vec<String>>,
    approved: HashSet<String>,
}

/// A word of a command line: its text when that is fixed, or `Dynamic` when it is only known at
/// run time (it contains an expansion or a substitution).
#[derive(Debug, Clone, PartialEq, Eq)]
enum Word {
    Lit(String),
    Dynamic,
}

#[derive(Debug, PartialEq, Eq)]
enum Match {
    No,
    Maybe,
    Yes,
}

/// Shells whose `-c` script is analysed in turn.
const SHELLS: [&str; 5] = ["bash", "sh", "dash", "zsh", "ksh"];
/// How deeply `bash -c` and `eval` scripts are followed.
const MAX_DEPTH: usize = 4;

impl ConfirmPolicy {
    pub fn new<S: AsRef<str>>(patterns: &[S]) -> Self {
        let patterns = patterns
            .iter()
            .map(|p| p.as_ref().split_whitespace().map(str::to_owned).collect::<Vec<_>>())
            .filter(|words| !words.is_empty())
            .collect();
        ConfirmPolicy { patterns, approved: HashSet::new() }
    }

    pub fn check(&self, command: &str) -> Verdict {
        if self.patterns.is_empty() || self.approved.contains(command) {
            return Verdict::Run;
        }
        let commands = match parse(command, 0) {
            Ok(commands) => commands,
            Err(why) => return Verdict::Ask(vec![why]),
        };
        let mut reasons: Vec<String> = Vec::new();
        for argv in &commands {
            if argv.first() == Some(&Word::Dynamic) {
                reasons.push("it runs a command whose name is only known at run time".into());
                continue;
            }
            for pattern in &self.patterns {
                let joined = pattern.join(" ");
                match match_pattern(pattern, argv) {
                    Match::Yes => reasons.push(format!("it matches \"{joined}\"")),
                    Match::Maybe => reasons.push(format!(
                        "it may match \"{joined}\": an argument is only known at run time"
                    )),
                    Match::No => {}
                }
            }
        }
        let mut seen = HashSet::new();
        reasons.retain(|r| seen.insert(r.clone()));
        if reasons.is_empty() { Verdict::Run } else { Verdict::Ask(reasons) }
    }

    /// Lets this exact command line run again without asking.
    pub fn allow_for_session(&mut self, command: &str) {
        self.approved.insert(command.to_owned());
    }
}

/// Every command in `text`, as its words after wrappers are taken off.
fn parse(text: &str, depth: usize) -> Result<Vec<Vec<Word>>, String> {
    if depth > MAX_DEPTH {
        return Err("it nests shell scripts too deeply to check".into());
    }
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_bash::LANGUAGE.into())
        .map_err(|e| format!("the bash parser failed to load ({e})"))?;
    let tree = parser.parse(text, None).ok_or("the command could not be parsed")?;
    let root = tree.root_node();
    if root.has_error() {
        return Err("the command could not be parsed, so it could not be checked".into());
    }
    let mut commands = Vec::new();
    collect(root, text.as_bytes(), depth, &mut commands)?;
    Ok(commands)
}

fn collect(node: Node, src: &[u8], depth: usize, out: &mut Vec<Vec<Word>>) -> Result<(), String> {
    if node.kind() == "command" {
        let argv = unwrap_wrappers(command_words(node, src));
        if let Some(Word::Lit(first)) = argv.first() {
            let name = basename(first);
            if SHELLS.contains(&name) {
                if let Some(script) = shell_script(&argv)? {
                    out.extend(parse(&script, depth + 1)?);
                }
            } else if name == "eval" {
                let mut script = Vec::new();
                for word in &argv[1..] {
                    match word {
                        Word::Lit(w) => script.push(w.as_str()),
                        Word::Dynamic => return Err("it runs code built at run time (eval)".into()),
                    }
                }
                out.extend(parse(&script.join(" "), depth + 1)?);
            }
        }
        out.push(argv);
    }
    // Command substitutions and the like inside this node hold commands of their own.
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect(child, src, depth, out)?;
    }
    Ok(())
}

/// A command's name and arguments, without its variable assignments and redirections.
fn command_words(node: Node, src: &[u8]) -> Vec<Word> {
    let mut words = Vec::new();
    if let Some(name) = node.child_by_field_name("name") {
        words.push(word(name, src));
    }
    let mut cursor = node.walk();
    words.extend(node.children_by_field_name("argument", &mut cursor).map(|arg| word(arg, src)));
    words
}

fn word(node: Node, src: &[u8]) -> Word {
    let text = node.utf8_text(src).unwrap_or_default();
    match node.kind() {
        "command_name" => {
            let mut cursor = node.walk();
            let inner = node.named_children(&mut cursor).next();
            inner.map_or(Word::Lit(unescape(text)), |inner| word(inner, src))
        }
        "word" | "number" => Word::Lit(unescape(text)),
        "raw_string" => Word::Lit(text.trim_start_matches('\'').trim_end_matches('\'').to_owned()),
        "string" => {
            let mut value = String::new();
            let mut cursor = node.walk();
            for part in node.named_children(&mut cursor) {
                match part.kind() {
                    "string_content" => value
                        .push_str(&unescape_double_quoted(part.utf8_text(src).unwrap_or_default())),
                    _ => return Word::Dynamic,
                }
            }
            Word::Lit(value)
        }
        "concatenation" => {
            let mut value = String::new();
            let mut cursor = node.walk();
            for part in node.named_children(&mut cursor) {
                match word(part, src) {
                    Word::Lit(s) => value.push_str(&s),
                    Word::Dynamic => return Word::Dynamic,
                }
            }
            Word::Lit(value)
        }
        // Expansions, substitutions, arithmetic, brace expressions, $'...'.
        _ => Word::Dynamic,
    }
}

/// An unquoted word's text: a backslash keeps the next character as it is.
fn unescape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            if let Some(next) = chars.next() {
                out.push(next);
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Inside double quotes a backslash only escapes `$`, `` ` ``, `"`, `\` and a newline.
fn unescape_double_quoted(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' && matches!(chars.peek(), Some('$' | '`' | '"' | '\\' | '\n')) {
            let next = chars.next().unwrap_or_default();
            if next != '\n' {
                out.push(next);
            }
        } else {
            out.push(c);
        }
    }
    out
}

fn basename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// Takes off commands that run another command: `sudo -u me git push` is `git push`.
fn unwrap_wrappers(mut argv: Vec<Word>) -> Vec<Word> {
    loop {
        let Some(Word::Lit(first)) = argv.first() else {
            return argv;
        };
        let args = &argv[1..];
        let skip = match basename(first) {
            "sudo" | "doas" => {
                skip_options(args, &["-u", "-g", "-p", "-C", "-D", "-h", "-r", "-t", "-T", "-U"])
            }
            "env" => {
                let mut n = skip_options(args, &["-u", "-C", "-S", "--unset", "--chdir"]);
                while matches!(args.get(n), Some(Word::Lit(w)) if w.contains('=') && !w.starts_with('='))
                {
                    n += 1;
                }
                n
            }
            "command" | "builtin" | "exec" | "nohup" | "time" | "noglob" => skip_options(args, &[]),
            "nice" | "ionice" => skip_options(args, &["-n", "-c"]),
            "stdbuf" => skip_options(args, &["-i", "-o", "-e"]),
            "timeout" => skip_options(args, &["-s", "-k", "--signal", "--kill-after"]) + 1,
            "xargs" => skip_options(
                args,
                &["-n", "-I", "-P", "-L", "-d", "-a", "-E", "-s", "--max-args", "--max-procs"],
            ),
            _ => return argv,
        };
        argv = argv.split_off((1 + skip).min(argv.len()));
    }
}

/// How many leading words of `args` are options, counting the values of `with_value` options.
fn skip_options(args: &[Word], with_value: &[&str]) -> usize {
    let mut n = 0;
    while let Some(Word::Lit(w)) = args.get(n) {
        if w == "--" {
            return n + 1;
        }
        if !w.starts_with('-') || w == "-" {
            break;
        }
        n += 1;
        if with_value.contains(&w.as_str()) {
            n += 1;
        }
    }
    n
}

/// The script a shell runs with `-c`. `None` for a script file, which is not followed;
/// a shell reading its commands from input cannot be checked.
fn shell_script(argv: &[Word]) -> Result<Option<String>, String> {
    let mut takes_script = false;
    for word in &argv[1..] {
        match word {
            Word::Lit(w) if w.starts_with('-') && w.len() > 1 && !w.starts_with("--") => {
                takes_script |= w.contains('c');
            }
            Word::Lit(w) if w.starts_with("--") => {}
            Word::Lit(w) if takes_script => return Ok(Some(w.clone())),
            Word::Dynamic if takes_script => {
                return Err("it runs a shell script built at run time".into());
            }
            _ => return Ok(None),
        }
    }
    if takes_script {
        return Err("it runs a shell without a script".into());
    }
    Err("it runs a shell that reads its commands from its input".into())
}

fn match_pattern(pattern: &[String], argv: &[Word]) -> Match {
    let Some(Word::Lit(first)) = argv.first() else {
        return Match::No;
    };
    let name = basename(first);
    if name != pattern[0] {
        return Match::No;
    }
    let rest = if name == "git" { &argv[git_subcommand(argv)..] } else { &argv[1..] };
    let mut result = Match::Yes;
    for (i, expected) in pattern[1..].iter().enumerate() {
        match rest.get(i) {
            None => return Match::No,
            Some(Word::Lit(w)) if w == expected => {}
            Some(Word::Lit(_)) => return Match::No,
            Some(Word::Dynamic) => result = Match::Maybe,
        }
    }
    result
}

/// Where a git command's subcommand is, after git's own options (`git -C dir push`).
fn git_subcommand(argv: &[Word]) -> usize {
    let mut i = 1;
    while let Some(Word::Lit(w)) = argv.get(i) {
        if !w.starts_with('-') {
            break;
        }
        i += 1;
        let takes_value = ["-C", "-c", "--git-dir", "--work-tree", "--namespace", "--config-env"];
        if takes_value.contains(&w.as_str()) {
            i += 1;
        }
    }
    i.min(argv.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn asks(patterns: &[&str], command: &str) -> bool {
        matches!(ConfirmPolicy::new(patterns).check(command), Verdict::Ask(_))
    }

    #[test]
    fn a_bare_name_matches_every_use() {
        assert!(asks(&["git"], "git status"));
        assert!(asks(&["git"], "/usr/bin/git log"));
        assert!(!asks(&["git"], "ls -la"));
        assert!(!asks(&[], "git push --force"));
    }

    #[test]
    fn longer_patterns_match_the_leading_words() {
        assert!(asks(&["git push"], "git push origin main"));
        assert!(!asks(&["git push"], "git pull"));
        assert!(!asks(&["git push"], "git"));
        assert!(asks(&["git push"], "git -C sub --no-pager push"));
        assert!(asks(&["rm -rf"], "rm -rf build"));
        assert!(!asks(&["rm -rf"], "rm build"));
    }

    #[test]
    fn every_command_in_a_line_is_checked() {
        assert!(asks(&["git push"], "cd repo && git add . && git push"));
        assert!(asks(&["git push"], "make test; git push"));
        assert!(asks(&["git push"], "(cd sub && git push)"));
        assert!(asks(&["git push"], "echo $(git push)"));
        assert!(asks(&["git push"], "for b in a b; do git push origin $b; done"));
        assert!(!asks(&["git push"], "git log | head -5"));
    }

    #[test]
    fn quoting_and_prefixes_do_not_hide_a_command() {
        assert!(asks(&["git push"], "FOO=1 git push"));
        assert!(asks(&["git push"], "\"git\" 'push'"));
        assert!(asks(&["git push"], "g\\it push"));
        assert!(asks(&["git push"], "sudo -u me git push"));
        assert!(asks(&["git push"], "env A=1 B=2 git push"));
        assert!(asks(&["git push"], "timeout 30 git push"));
        assert!(asks(&["git push"], "xargs -n1 git push"));
    }

    #[test]
    fn shell_scripts_and_eval_are_followed() {
        assert!(asks(&["git push"], "bash -c 'cd x && git push'"));
        assert!(asks(&["git push"], "sh -lc \"git push\""));
        assert!(asks(&["git push"], "eval git push"));
        assert!(!asks(&["git push"], "bash -c 'git status'"));
        assert!(!asks(&["git push"], "bash build.sh"));
    }

    #[test]
    fn text_that_only_mentions_a_command_runs() {
        assert!(!asks(&["git push"], "echo \"git push\""));
        assert!(!asks(&["git push"], "grep -r 'git push' docs"));
        assert!(!asks(&["git"], "python3 -c 'print(1)'"));
    }

    #[test]
    fn what_cannot_be_analysed_asks() {
        assert!(asks(&["git push"], "$GIT push"));
        assert!(asks(&["git push"], "git $SUB"));
        assert!(asks(&["git push"], "eval \"$CMD\""));
        assert!(asks(&["git push"], "echo git push | bash"));
        assert!(asks(&["git push"], "git push \"unterminated"));
        assert!(!asks(&["git push"], "git log $REF"));
    }

    #[test]
    fn reasons_name_the_pattern_and_session_approval_sticks() {
        let mut policy = ConfirmPolicy::new(&["git push", "rm -rf"]);
        let command = "git push && rm -rf dist";
        let Verdict::Ask(reasons) = policy.check(command) else { panic!("should ask") };
        assert_eq!(reasons, ["it matches \"git push\"", "it matches \"rm -rf\""]);
        policy.allow_for_session(command);
        assert_eq!(policy.check(command), Verdict::Run);
        assert!(matches!(policy.check("git push --force"), Verdict::Ask(_)));
    }
}
