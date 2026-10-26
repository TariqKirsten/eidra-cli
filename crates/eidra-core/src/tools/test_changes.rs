//! A reminder when a change alters a test the project already had (docs/DESIGN.md §9). Shown a
//! failing test, the model tends to make the test fit its change, where the project's tests say
//! what the code must go on doing: in the July SWE-rebench baseline, three of six failures were
//! the agent editing existing tests until its change passed them. Some tasks do ask for a tested
//! behaviour to change, so the result only says so; nothing is refused.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use tokio::process::Command;
use tree_sitter::Node;

/// How long `git show` may take.
const GIT_TIMEOUT: Duration = Duration::from_secs(5);

/// Whether `path` holds Python tests, by pytest's conventions.
pub fn is_test_file(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    let in_tests =
        path.components().any(|c| matches!(c.as_os_str().to_str(), Some("tests" | "test")));
    name.ends_with(".py") && (name.starts_with("test_") || name.ends_with("_test.py") || in_tests)
}

/// The file as the project's last commit has it, if it is in one.
pub async fn at_head(path: &Path) -> Option<String> {
    let (dir, name) = (path.parent()?, path.file_name()?.to_str()?);
    let mut git = Command::new("git");
    git.arg("-C")
        .arg(dir)
        .args(["show", &format!("HEAD:./{name}")])
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let output = tokio::time::timeout(GIT_TIMEOUT, git.output()).await.ok()?.ok()?;
    output.status.success().then(|| String::from_utf8_lossy(&output.stdout).into_owned())
}

/// The tests of `head`, the file as committed, that a change from `before` to `after` altered or
/// removed, by name: `test_x`, or `TestY.test_x` in a class. Tests the change added, or that were
/// not in the commit, do not count.
pub fn changed(head: &str, before: &str, after: &str) -> Vec<String> {
    let (head, after) = (tests(head), tests(after));
    tests(before)
        .into_iter()
        .filter(|(name, _)| head.iter().any(|(n, _)| n == name))
        .filter(|(name, text)| after.iter().find(|(n, _)| n == name).is_none_or(|(_, t)| t != text))
        .map(|(name, _)| name)
        .collect()
}

/// Each test function's name and its text, decorators included, without blank lines and
/// trailing spaces, so that only real changes count.
fn tests(source: &str) -> Vec<(String, String)> {
    let mut parser = tree_sitter::Parser::new();
    if parser.set_language(&tree_sitter_python::LANGUAGE.into()).is_err() {
        return Vec::new();
    }
    let Some(tree) = parser.parse(source, None) else {
        return Vec::new();
    };
    let mut found = Vec::new();
    collect(tree.root_node(), source, None, &mut found);
    found
}

fn collect(node: Node, source: &str, class: Option<&str>, found: &mut Vec<(String, String)>) {
    let text = |node: Node| node.utf8_text(source.as_bytes()).unwrap_or_default().to_owned();
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        // A decorated definition holds what it decorates.
        let definition = match child.kind() {
            "decorated_definition" => match child.child_by_field_name("definition") {
                Some(definition) => definition,
                None => continue,
            },
            _ => child,
        };
        let name = definition.child_by_field_name("name").map(text).unwrap_or_default();
        match (definition.kind(), class) {
            ("function_definition", _) if name.starts_with("test") => {
                let name = class.map_or(name.clone(), |class| format!("{class}.{name}"));
                let body: Vec<String> = text(child)
                    .lines()
                    .map(str::trim_end)
                    .filter(|line| !line.is_empty())
                    .map(str::to_owned)
                    .collect();
                found.push((name, body.join("\n")));
            }
            ("class_definition", None) => {
                if let Some(body) = definition.child_by_field_name("body") {
                    collect(body, source, Some(&name), found);
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HEAD: &str = r#"
import pytest

def helper():
    return 1

def test_adds():
    assert add(1, 2) == 3

@pytest.mark.parametrize("x", [1, 2])
def test_positive(x):
    assert x > 0

class TestCache:
    def setup_method(self):
        self.cache = {}

    async def test_refetches(self):
        assert fetch() == "2.0.0"
"#;

    #[test]
    fn names_the_committed_tests_a_change_alters_or_removes() {
        // Reformatting, a new test and a changed helper are not changes to the project's tests.
        let reformatted = HEAD.replace("def test_adds():\n", "def test_adds():\n\n");
        let more = format!("{HEAD}\ndef test_new():\n    assert True\n");
        let helper = HEAD.replace("return 1", "return 2");
        for after in [reformatted.as_str(), &more, &helper] {
            assert!(changed(HEAD, HEAD, after).is_empty());
        }
        // An assertion, a decorator's cases, a method in a class, and a removed test do.
        let edited =
            HEAD.replace("== 3", "== 4").replace("[1, 2]", "[1]").replace("\"2.0.0\"", "\"1.0.0\"");
        assert_eq!(
            changed(HEAD, HEAD, &edited),
            ["test_adds", "test_positive", "TestCache.test_refetches"]
        );
        let removed = HEAD.replace("def test_adds():\n    assert add(1, 2) == 3\n", "");
        assert_eq!(changed(HEAD, HEAD, &removed), ["test_adds"]);
        // A test the session added and then changed is its own business.
        let added = format!("{HEAD}\ndef test_new():\n    assert True\n");
        let changed_added = added.replace("assert True", "assert False");
        assert!(changed(HEAD, &added, &changed_added).is_empty());
    }

    #[test]
    fn knows_test_files() {
        for path in
            ["tests/unit/test_cache.py", "pkg/cache_test.py", "test_x.py", "tests/conftest.py"]
        {
            assert!(is_test_file(Path::new(path)), "{path}");
        }
        for path in ["pkg/cache.py", "tests/data.json", "testing.py"] {
            assert!(!is_test_file(Path::new(path)), "{path}");
        }
    }
}
