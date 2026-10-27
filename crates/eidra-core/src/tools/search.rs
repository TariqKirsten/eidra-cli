//! `glob` and `grep`, in process with ripgrep's own crates. Files ignored by .gitignore (in a
//! repository or not) and hidden files are skipped.

use std::path::{Path, PathBuf};

use globset::{GlobBuilder, GlobMatcher};
use grep_regex::RegexMatcher;
use grep_searcher::sinks::UTF8;
use grep_searcher::{BinaryDetection, SearcherBuilder};
use ignore::WalkBuilder;

use super::resolve;

const MAX_PATHS: usize = 500;
const MAX_MATCHES: usize = 200;
const MAX_MATCH_CHARS: usize = 300;

/// Matches paths: a pattern with a slash against the path below the search root, one without
/// against the file name, so `*.py` finds Python files in every directory.
struct PathPattern {
    glob: GlobMatcher,
    whole_path: bool,
}

impl PathPattern {
    fn new(pattern: &str) -> Result<Self, String> {
        let glob = GlobBuilder::new(pattern)
            .literal_separator(true)
            .build()
            .map_err(|e| format!("bad glob pattern {pattern:?}: {e}"))?
            .compile_matcher();
        Ok(PathPattern { glob, whole_path: pattern.contains('/') })
    }

    fn matches(&self, relative: &Path) -> bool {
        if self.whole_path {
            self.glob.is_match(relative)
        } else {
            relative.file_name().is_some_and(|name| self.glob.is_match(name))
        }
    }
}

/// Every file below `root`, in name order.
fn files_under(root: &Path) -> impl Iterator<Item = PathBuf> {
    WalkBuilder::new(root)
        .require_git(false)
        .sort_by_file_name(|a, b| a.cmp(b))
        .build()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_some_and(|t| t.is_file()))
        .map(|entry| entry.into_path())
}

/// A path as the model should see it: relative to the workspace when it is inside it.
fn display(workspace: &Path, path: &Path) -> String {
    path.strip_prefix(workspace).unwrap_or(path).display().to_string()
}

pub fn glob(workspace: &Path, pattern: &str, path: Option<&str>) -> Result<String, String> {
    let root = resolve(workspace, path.unwrap_or("."));
    if !root.is_dir() {
        return Err(format!("{} is not a directory", path.unwrap_or(".")));
    }
    let pattern_matcher = PathPattern::new(pattern)?;
    let mut hits = Vec::new();
    let mut total = 0;
    for file in files_under(&root) {
        if pattern_matcher.matches(file.strip_prefix(&root).unwrap_or(&file)) {
            total += 1;
            if hits.len() < MAX_PATHS {
                hits.push(display(workspace, &file));
            }
        }
    }
    if hits.is_empty() {
        return Ok(format!("No files match {pattern}."));
    }
    let mut out = hits.join("\n");
    if total > hits.len() {
        out.push_str(&format!("\n({total} files match; the first {} are shown)", hits.len()));
    }
    Ok(out)
}

pub fn grep(
    workspace: &Path,
    pattern: &str,
    path: Option<&str>,
    file_glob: Option<&str>,
) -> Result<String, String> {
    let matcher = RegexMatcher::new_line_matcher(pattern)
        .map_err(|e| format!("bad regular expression: {e}"))?;
    let root = resolve(workspace, path.unwrap_or("."));
    let files: Vec<PathBuf> = if root.is_file() {
        vec![root.clone()]
    } else if root.is_dir() {
        let filter = file_glob.map(PathPattern::new).transpose()?;
        files_under(&root)
            .filter(|file| {
                filter.as_ref().is_none_or(|f| f.matches(file.strip_prefix(&root).unwrap_or(file)))
            })
            .collect()
    } else {
        return Err(format!("{} does not exist", path.unwrap_or(".")));
    };

    let mut searcher =
        SearcherBuilder::new().binary_detection(BinaryDetection::quit(0)).line_number(true).build();
    let mut lines = Vec::new();
    let mut total = 0usize;
    for file in &files {
        let shown = display(workspace, file);
        let _ = searcher.search_path(
            &matcher,
            file,
            UTF8(|number, line| {
                total += 1;
                if lines.len() < MAX_MATCHES {
                    let line = line.trim_end_matches(['\n', '\r']);
                    let line: String = line.chars().take(MAX_MATCH_CHARS).collect();
                    lines.push(format!("{shown}:{number}:{line}"));
                }
                Ok(true)
            }),
        );
    }
    if lines.is_empty() {
        return Ok(format!("No matches for {pattern}."));
    }
    let mut out = lines.join("\n");
    if total > lines.len() {
        out.push_str(&format!(
            "\n({total} matching lines; the first {} are shown, so narrow the pattern or the path)",
            lines.len()
        ));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// A small project of the test's own: the tests run side by side.
    fn tree(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("eidra-search-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        for (path, text) in [
            ("src/a.py", "def alpha():\n    return 1\n"),
            ("src/pkg/b.py", "def beta():\n    return alpha()\n"),
            ("README.md", "alpha and beta\n"),
            ("build/out.py", "def alpha(): pass\n"),
            (".gitignore", "build/\n"),
        ] {
            let file = dir.join(path);
            fs::create_dir_all(file.parent().unwrap()).unwrap();
            fs::write(file, text).unwrap();
        }
        dir
    }

    #[test]
    fn globs_by_name_or_path_and_skips_ignored_files() {
        let dir = tree("glob");
        assert_eq!(glob(&dir, "*.py", None).unwrap(), "src/a.py\nsrc/pkg/b.py");
        assert_eq!(glob(&dir, "src/*.py", None).unwrap(), "src/a.py");
        assert_eq!(glob(&dir, "src/**/*.py", None).unwrap(), "src/a.py\nsrc/pkg/b.py");
        assert_eq!(glob(&dir, "*.py", Some("src/pkg")).unwrap(), "src/pkg/b.py");
        assert_eq!(glob(&dir, "*.rs", None).unwrap(), "No files match *.rs.");
    }

    #[test]
    fn greps_lines_with_numbers() {
        let dir = tree("grep");
        assert_eq!(
            grep(&dir, r"alpha", None, None).unwrap(),
            "README.md:1:alpha and beta\nsrc/a.py:1:def alpha():\nsrc/pkg/b.py:2:    return alpha()"
        );
        assert_eq!(
            grep(&dir, r"def \w+", None, Some("*.py")).unwrap(),
            "src/a.py:1:def alpha():\nsrc/pkg/b.py:1:def beta():"
        );
        assert_eq!(
            grep(&dir, "beta", Some("README.md"), None).unwrap(),
            "README.md:1:alpha and beta"
        );
        assert!(grep(&dir, "(", None, None).unwrap_err().contains("bad regular expression"));
    }
}
