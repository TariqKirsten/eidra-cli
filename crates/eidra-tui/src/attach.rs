//! Images attached to a message (docs/DESIGN.md §10): `@path` mentions, and paths pasted or dropped
//! into the input box, which terminals give quoted, with escaped spaces, or as `file://` URLs. A
//! plain word such as `photo.png` in a sentence is left alone.

use std::path::{Path, PathBuf};

use eidra_core::media::{self, Image};

/// An image a message points to, or why it could not be attached.
pub struct Attachment {
    /// The path as the message gave it.
    pub name: String,
    pub image: Result<Image, String>,
}

/// The images `text` points to, each once, in the order it names them.
pub fn attachments(text: &str, workspace: &Path) -> Vec<Attachment> {
    let mut seen: Vec<PathBuf> = Vec::new();
    let mut found = Vec::new();
    for word in words(text) {
        let (mention, rest) = match word.strip_prefix('@') {
            Some(rest) => (true, rest.to_owned()),
            None => (false, word.clone()),
        };
        let rest = match rest.strip_prefix("file://") {
            Some(url) => percent_decoded(url),
            None => rest,
        };
        // Without an @, only what looks pasted: an absolute or home-relative path.
        if !mention && !rest.starts_with('/') && !rest.starts_with("~/") {
            continue;
        }
        let path = resolve(&rest, workspace);
        if !media::is_image(&path) || !path.is_file() || seen.contains(&path) {
            continue;
        }
        let image = std::fs::read(&path).map_err(|e| e.to_string()).and_then(|b| media::image(&b));
        seen.push(path);
        found.push(Attachment { name: rest, image });
    }
    found
}

/// `text` split at whitespace, except inside quotes or after a backslash, which are taken out.
fn words(text: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut quote = None;
    let mut chars = text.chars();
    while let Some(ch) = chars.next() {
        match (ch, quote) {
            ('\\', _) => word.extend(chars.next()),
            ('\'' | '"', None) => quote = Some(ch),
            (c, Some(q)) if c == q => quote = None,
            (c, None) if c.is_whitespace() => {
                if !word.is_empty() {
                    words.push(std::mem::take(&mut word));
                }
            }
            (c, _) => word.push(c),
        }
    }
    if !word.is_empty() {
        words.push(word);
    }
    // A mention ends a sentence as often as not.
    words
        .into_iter()
        .map(|w| w.trim_end_matches([',', '.', ';', ':', '!', '?', ')']).to_owned())
        .collect()
}

fn resolve(path: &str, workspace: &Path) -> PathBuf {
    match path.strip_prefix("~/") {
        Some(rest) => std::env::var_os("HOME")
            .map_or_else(|| PathBuf::from(path), |h| PathBuf::from(h).join(rest)),
        None => workspace.join(path),
    }
}

/// `%20` and its kind decoded, as a `file://` URL writes them.
fn percent_decoded(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = bytes.get(i + 1..i + 3).and_then(|h| std::str::from_utf8(h).ok());
        match (bytes[i], hex.and_then(|h| u8::from_str_radix(h, 16).ok())) {
            (b'%', Some(byte)) => {
                out.push(byte);
                i += 3;
            }
            (byte, _) => {
                out.push(byte);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_words_as_a_shell_would() {
        assert_eq!(
            words(r#"look at '/tmp/My Shots/a.png' and /tmp/b\ c.png, then @d.png."#),
            ["look", "at", "/tmp/My Shots/a.png", "and", "/tmp/b c.png", "then", "@d.png"]
        );
        assert_eq!(percent_decoded("/tmp/My%20Shots/a.png"), "/tmp/My Shots/a.png");
    }

    #[test]
    fn attaches_mentions_and_pasted_paths_but_not_words() {
        let dir = std::env::temp_dir().join(format!("eidra-attach-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("shots")).unwrap();
        let mut png = Vec::new();
        image::RgbImage::new(4, 3)
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        std::fs::write(dir.join("chart.png"), &png).unwrap();
        std::fs::write(dir.join("shots/dropped one.png"), &png).unwrap();
        let dropped = dir.join("shots/dropped one.png").display().to_string();
        let text = format!(
            "compare @chart.png with '{dropped}', not chart.png, and @chart.png again; @missing.png"
        );
        let found = attachments(&text, &dir);
        let names: Vec<&str> = found.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(names, ["chart.png", dropped.as_str()]);
        assert!(
            found.iter().all(|a| a.image.as_ref().is_ok_and(|i| (i.width, i.height) == (4, 3)))
        );
        let url = format!("file://{}", dir.join("shots/dropped%20one.png").display());
        assert_eq!(attachments(&url, &dir).len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
