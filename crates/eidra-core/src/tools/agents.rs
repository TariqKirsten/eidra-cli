//! Nested AGENTS.md files (docs/DESIGN.md §11). The workspace's own goes into the system prompt;
//! one in a subdirectory joins the first tool result that touches that directory. The prompt only
//! grows at its end (R5), and only for the parts of the project the agent works in.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

/// Characters of one file given; past this it is cut, with a note.
const MOST: usize = 8_000;

pub struct Nested {
    workspace: PathBuf,
    /// Directories whose AGENTS.md has been given.
    given: HashSet<PathBuf>,
}

impl Nested {
    pub fn new(workspace: &Path) -> Self {
        Nested { workspace: workspace.to_path_buf(), given: HashSet::new() }
    }

    /// The AGENTS.md files between the workspace and `path` (a file or a directory) not given
    /// yet, outermost first, as text to add to a tool's result.
    pub fn for_path(&mut self, path: &Path) -> String {
        let dir = if path.is_dir() { path } else { path.parent().unwrap_or(path) };
        let Ok(inside) = dir.strip_prefix(&self.workspace) else {
            return String::new();
        };
        let mut current = self.workspace.clone();
        let mut text = String::new();
        for part in inside.components() {
            current.push(part);
            if self.given.contains(&current) {
                continue;
            }
            let Ok(instructions) = fs::read_to_string(current.join("AGENTS.md")) else {
                continue;
            };
            self.given.insert(current.clone());
            let instructions = instructions.trim();
            if instructions.is_empty() {
                continue;
            }
            let shown = current.strip_prefix(&self.workspace).unwrap_or(&current).display();
            text.push_str(&format!("\n\n# Instructions from {shown}/AGENTS.md, for {shown}/\n"));
            if instructions.len() > MOST {
                let cut = crate::text::char_floor(instructions, MOST);
                text.push_str(&instructions[..cut]);
                text.push_str(&format!("\n(cut short; the whole file is {shown}/AGENTS.md)"));
            } else {
                text.push_str(instructions);
            }
        }
        text
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gives_each_nested_file_once_outermost_first() {
        let dir = std::env::temp_dir().join(format!("eidra-agents-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("api/v2")).unwrap();
        fs::create_dir_all(dir.join("web")).unwrap();
        fs::write(dir.join("AGENTS.md"), "root: in the system prompt already").unwrap();
        fs::write(dir.join("api/AGENTS.md"), "Run the API tests with make api-test.").unwrap();
        fs::write(dir.join("api/v2/AGENTS.md"), "v2 is frozen: no new endpoints.").unwrap();
        fs::write(dir.join("api/v2/routes.py"), "").unwrap();
        let mut nested = Nested::new(&dir);

        let first = nested.for_path(&dir.join("api/v2/routes.py"));
        assert_eq!(
            first,
            "\n\n# Instructions from api/AGENTS.md, for api/\nRun the API tests with make api-test.\
             \n\n# Instructions from api/v2/AGENTS.md, for api/v2/\nv2 is frozen: no new endpoints."
        );
        assert_eq!(nested.for_path(&dir.join("api/v2")), "");
        assert_eq!(nested.for_path(&dir.join("web/index.html")), "");
        assert_eq!(nested.for_path(&dir), "");
        assert_eq!(nested.for_path(Path::new("/etc/passwd")), "");
        let _ = fs::remove_dir_all(&dir);
    }
}
