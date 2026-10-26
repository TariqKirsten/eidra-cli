//! Server-sent events: splits a byte stream into the `data` payloads of its events.

#[derive(Debug, Default)]
pub(crate) struct SseDecoder {
    buf: Vec<u8>,
}

impl SseDecoder {
    /// Adds bytes and returns the data of every event they complete.
    pub(crate) fn push(&mut self, bytes: &[u8]) -> Vec<String> {
        // Lines may end in CRLF. The payloads are JSON, which never holds a raw CR, so CRs can go.
        self.buf.extend(bytes.iter().filter(|&&b| b != b'\r'));
        let mut events = Vec::new();
        while let Some(end) = self.buf.windows(2).position(|w| w == b"\n\n") {
            let block: Vec<u8> = self.buf.drain(..end + 2).collect();
            if let Some(data) = data_of(&block[..end]) {
                events.push(data);
            }
        }
        events
    }

    /// The data of a last event that the stream ended without terminating.
    pub(crate) fn finish(&mut self) -> Option<String> {
        data_of(&std::mem::take(&mut self.buf))
    }
}

/// Joins an event's `data:` lines; `None` when it has none (comments, keep-alives).
fn data_of(block: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(block);
    let mut data: Option<String> = None;
    for line in text.split('\n') {
        let Some(value) = line.strip_prefix("data:") else {
            continue;
        };
        let value = value.strip_prefix(' ').unwrap_or(value);
        match &mut data {
            Some(data) => {
                data.push('\n');
                data.push_str(value);
            }
            None => data = Some(value.to_owned()),
        }
    }
    data
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_events_across_chunks() {
        let mut d = SseDecoder::default();
        assert!(d.push(b"data: {\"a\"").is_empty());
        assert_eq!(d.push(b":1}\n\ndata: [DONE]\n\n"), ["{\"a\":1}", "[DONE]"]);
    }

    #[test]
    fn handles_crlf_comments_and_multiline_data() {
        let mut d = SseDecoder::default();
        let events = d.push(b": keep-alive\r\n\r\nevent: x\r\ndata: one\r\ndata: two\r\n\r\n");
        assert_eq!(events, ["one\ntwo"]);
    }

    #[test]
    fn keeps_utf8_split_across_chunks() {
        let mut d = SseDecoder::default();
        let bytes = "data: héllo\n\n".as_bytes();
        let (a, b) = bytes.split_at(8); // inside the two bytes of 'é'
        assert!(d.push(a).is_empty());
        assert_eq!(d.push(b), ["héllo"]);
    }

    #[test]
    fn flushes_an_unterminated_last_event() {
        let mut d = SseDecoder::default();
        assert!(d.push(b"data: last").is_empty());
        assert_eq!(d.finish().as_deref(), Some("last"));
    }
}
