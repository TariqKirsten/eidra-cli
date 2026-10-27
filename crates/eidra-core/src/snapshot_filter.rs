//! git's filter for encrypted snapshots (docs/DESIGN.md §18). In an encrypted eidra directory,
//! the snapshot repository's attributes send every file through a filter named `eidra`, and git
//! runs eidra itself as that filter's long-running process (gitattributes(5), "Long Running
//! Filter Process"): "clean" seals what goes into a snapshot, "smudge" opens what `/undo` puts
//! back. `/diff` reads snapshots through `textconv`. Both get the snapshot key alone, in the
//! environment of the git command eidra runs.

use std::fs;
use std::io::{self, Read, Write};
use std::path::Path;

use crate::text::fnv1a;
use crate::vault::SnapshotCipher;

/// The environment variable the key comes in, as hex.
pub const KEY_ENV: &str = "EIDRA_SNAPSHOT_KEY";
/// The most data one pkt-line carries.
const LARGEST_PACKET: usize = 65516;

/// The cipher for the key in the environment.
pub fn cipher_from_env() -> io::Result<SnapshotCipher> {
    let hex = std::env::var(KEY_ENV)
        .map_err(|_| io::Error::other(format!("{KEY_ENV} is not set: eidra runs this for git")))?;
    let mut key = zeroize::Zeroizing::new([0u8; 32]);
    if hex.len() != 64 {
        return Err(io::Error::other(format!("{KEY_ENV} is not 32 bytes of hex")));
    }
    for (i, byte) in key.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[2 * i..2 * i + 2], 16)
            .map_err(|_| io::Error::other(format!("{KEY_ENV} is not hex")))?;
    }
    Ok(SnapshotCipher::new(&key))
}

/// The key as the environment carries it.
pub fn key_hex(key: &[u8; 32]) -> zeroize::Zeroizing<String> {
    zeroize::Zeroizing::new(key.iter().map(|b| format!("{b:02x}")).collect())
}

/// Serves git's filter protocol on `input` and `output` until git closes it.
pub fn serve(cipher: &SnapshotCipher, input: impl Read, output: impl Write) -> io::Result<()> {
    let mut input = io::BufReader::new(input);
    let mut output = io::BufWriter::new(output);
    let hello = read_list(&mut input)?.ok_or_else(|| eof("the handshake"))?;
    if !hello.iter().any(|l| l == "git-filter-client") || !hello.iter().any(|l| l == "version=2") {
        return Err(io::Error::other("git did not offer version 2 of the filter protocol"));
    }
    write_list(&mut output, &["git-filter-server", "version=2"])?;
    output.flush()?;
    let offered = read_list(&mut input)?.ok_or_else(|| eof("the capabilities"))?;
    let both: Vec<&str> = ["capability=clean", "capability=smudge"]
        .into_iter()
        .filter(|c| offered.iter().any(|o| o == c))
        .collect();
    write_list(&mut output, &both)?;
    output.flush()?;
    // One file at a time, until git is done and closes the pipe.
    while let Some(request) = read_list(&mut input)? {
        let command = request.iter().find_map(|l| l.strip_prefix("command=")).unwrap_or_default();
        let content = read_content(&mut input)?;
        let result = match command {
            "clean" => Ok(cipher.seal(&content)),
            "smudge" => cipher.open(&content).map_err(|e| e.to_string()),
            other => Err(format!("unknown command {other}")),
        };
        match result {
            Ok(filtered) => {
                write_list(&mut output, &["status=success"])?;
                for chunk in filtered.chunks(LARGEST_PACKET) {
                    write_packet(&mut output, chunk)?;
                }
                flush_packet(&mut output)?;
                // An empty list: the status stays "success".
                flush_packet(&mut output)?;
            }
            Err(_) => write_list(&mut output, &["status=error"])?,
        }
        output.flush()?;
    }
    Ok(())
}

/// A snapshotted file git wrote out at `path`, opened, to `output`: what `/diff` compares. A
/// binary one is its size and a fingerprint, as git would show it changed rather than its bytes.
pub fn textconv(cipher: &SnapshotCipher, path: &Path, mut output: impl Write) -> io::Result<()> {
    let data = fs::read(path)?;
    let plain = cipher.open(&data).map_err(|e| io::Error::other(e.to_string()))?;
    // git's own test for a binary file: a NUL in its first 8000 bytes.
    if plain.iter().take(8000).any(|&b| b == 0) {
        writeln!(output, "(binary, {} bytes, {:016x})", plain.len(), fnv1a(&plain))?;
    } else {
        output.write_all(&plain)?;
    }
    output.flush()
}

fn eof(during: &str) -> io::Error {
    io::Error::new(io::ErrorKind::UnexpectedEof, format!("git closed the pipe during {during}"))
}

/// One pkt-line: Some(data), or None for a flush packet.
fn read_packet(input: &mut impl Read) -> io::Result<Option<Vec<u8>>> {
    let mut length = [0u8; 4];
    input.read_exact(&mut length)?;
    let length = std::str::from_utf8(&length)
        .ok()
        .and_then(|hex| usize::from_str_radix(hex, 16).ok())
        .ok_or_else(|| io::Error::other("a pkt-line with a length that is not hex"))?;
    match length {
        0 => Ok(None),
        1..=4 => Err(io::Error::other("a pkt-line shorter than its header")),
        _ => {
            let mut data = vec![0u8; length - 4];
            input.read_exact(&mut data)?;
            Ok(Some(data))
        }
    }
}

/// Text lines up to a flush packet, without their newlines; None if git closed the pipe before
/// the list began.
fn read_list(input: &mut impl Read) -> io::Result<Option<Vec<String>>> {
    let mut lines = Vec::new();
    loop {
        match read_packet(input) {
            Ok(Some(data)) => {
                let line = String::from_utf8_lossy(&data);
                lines.push(line.strip_suffix('\n').unwrap_or(&line).to_owned());
            }
            Ok(None) => return Ok(Some(lines)),
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof && lines.is_empty() => {
                return Ok(None);
            }
            Err(e) => return Err(e),
        }
    }
}

/// Binary packets up to a flush packet, joined.
fn read_content(input: &mut impl Read) -> io::Result<Vec<u8>> {
    let mut content = Vec::new();
    while let Some(data) = read_packet(input)? {
        content.extend(data);
    }
    Ok(content)
}

fn write_packet(output: &mut impl Write, data: &[u8]) -> io::Result<()> {
    write!(output, "{:04x}", data.len() + 4)?;
    output.write_all(data)
}

fn flush_packet(output: &mut impl Write) -> io::Result<()> {
    output.write_all(b"0000")
}

fn write_list(output: &mut impl Write, lines: &[&str]) -> io::Result<()> {
    for line in lines {
        write_packet(output, format!("{line}\n").as_bytes())?;
    }
    flush_packet(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn packets(out: &mut Vec<u8>, lines: &[&str]) {
        write_list(out, lines).unwrap();
    }

    #[test]
    fn speaks_gits_filter_protocol() {
        let cipher = SnapshotCipher::new(&[7u8; 32]);
        let big: Vec<u8> = (0..150_000u32).map(|i| (i % 251) as u8).collect();
        let sealed = cipher.seal(&big);
        let mut request = Vec::new();
        packets(&mut request, &["git-filter-client", "version=2"]);
        packets(&mut request, &["capability=clean", "capability=smudge", "capability=delay"]);
        for (command, content) in [("clean", &big[..]), ("smudge", &sealed[..])] {
            packets(&mut request, &[&format!("command={command}"), "pathname=a/b.py"]);
            for chunk in content.chunks(LARGEST_PACKET) {
                write_packet(&mut request, chunk).unwrap();
            }
            flush_packet(&mut request).unwrap();
        }
        // Something that is not sealed by this key, and is not plain either.
        packets(&mut request, &["command=smudge", "pathname=c"]);
        write_packet(&mut request, &cipher.seal(b"x")[..30]).unwrap();
        flush_packet(&mut request).unwrap();

        let mut reply = Vec::new();
        serve(&cipher, &request[..], &mut reply).unwrap();
        let mut reply = &reply[..];
        let list = |r: &mut &[u8]| read_list(r).unwrap().unwrap();
        assert_eq!(list(&mut reply), ["git-filter-server", "version=2"]);
        assert_eq!(list(&mut reply), ["capability=clean", "capability=smudge"]);
        assert_eq!(list(&mut reply), ["status=success"]);
        assert_eq!(read_content(&mut reply).unwrap(), sealed);
        assert!(list(&mut reply).is_empty());
        assert_eq!(list(&mut reply), ["status=success"]);
        assert_eq!(read_content(&mut reply).unwrap(), big);
        assert!(list(&mut reply).is_empty());
        assert_eq!(list(&mut reply), ["status=error"]);
        assert!(reply.is_empty());
    }

    #[test]
    fn textconv_opens_text_and_stands_in_for_binary() {
        let cipher = SnapshotCipher::new(&[7u8; 32]);
        let dir = std::env::temp_dir().join(format!("eidra-textconv-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let read = |plain: &[u8]| {
            fs::write(dir.join("blob"), cipher.seal(plain)).unwrap();
            let mut out = Vec::new();
            textconv(&cipher, &dir.join("blob"), &mut out).unwrap();
            String::from_utf8(out).unwrap()
        };
        assert_eq!(read(b"x = 1\n"), "x = 1\n");
        let binary = read(b"\x89PNG\0\0\x01");
        assert!(binary.starts_with("(binary, 7 bytes, "), "{binary}");
        assert_ne!(binary, read(b"\x89PNG\0\0\x02"), "a change shows");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_key_goes_through_the_environment_as_hex() {
        let key = [0xabu8; 32];
        assert_eq!(key_hex(&key).as_str(), "ab".repeat(32));
    }
}
