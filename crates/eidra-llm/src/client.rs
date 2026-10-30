//! The HTTP client.

use std::path::Path;
use std::time::Duration;

use serde_json::{Value, json};

use crate::sse::SseDecoder;
use crate::stream::{Chunk, StreamEvent, TurnAccumulator};
use crate::tls;
use crate::types::{AssistantTurn, ChatRequest};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("request to {url} failed: {source}")]
    Http { url: String, source: reqwest::Error },
    #[error("{url} answered {status}: {body}")]
    Status { url: String, status: u16, body: String },
    #[error("{url} sent something unreadable ({source}): {data}")]
    Decode { url: String, source: serde_json::Error, data: String },
    #[error("{url} reported an error mid-reply: {detail}")]
    Server { url: String, detail: String },
    #[error("{url} ended the stream before the reply finished")]
    Incomplete { url: String },
    #[error("{url}: {detail}")]
    Unexpected { url: String, detail: String },
}

/// A client for one OpenAI-compatible server.
#[derive(Debug, Clone)]
pub struct Client {
    http: reqwest::Client,
    base_url: String,
    api_key: Option<String>,
}

impl Client {
    /// `base_url` is the API root, such as `http://127.0.0.1:8000/v1` or an `https:` one.
    ///
    /// With `socket`, every connection goes through that Unix socket instead, and the URL's host
    /// only names the server. That is how an agent with no network reaches the model: a relay
    /// outside its container forwards the socket to the server (docs/DESIGN.md §13).
    ///
    /// With `pin`, the SHA-256 of an `https:` server's own certificate (what `eidra relay
    /// --listen` prints), that certificate is the one trusted, whatever name or address the URL
    /// gives. Without one, an `https:` server's certificate must be vouched for, for its name, by
    /// an authority the system trusts. A pin for a plain `http:` URL is refused: what it was to
    /// protect would cross the network unencrypted.
    pub fn new(
        base_url: &str,
        api_key: Option<String>,
        socket: Option<&Path>,
        pin: Option<&str>,
    ) -> Result<Self, Error> {
        let base_url = base_url.trim_end_matches('/').to_owned();
        let mut builder = reqwest::Client::builder().connect_timeout(Duration::from_secs(10));
        if let Some(socket) = socket {
            builder = builder.unix_socket(socket);
        }
        tls::use_ring();
        if let Some(pin) = pin {
            let unexpected = |detail| Error::Unexpected { url: base_url.clone(), detail };
            if !base_url.starts_with("https://") {
                let why = "a TLS pin is set, but the URL is not https://, so nothing would be \
                           encrypted";
                return Err(unexpected(why.to_owned()));
            }
            let pin = tls::parse_pin(pin).map_err(unexpected)?;
            builder = builder.tls_backend_preconfigured(tls::pinned(pin));
        }
        let http =
            builder.build().map_err(|source| Error::Http { url: base_url.clone(), source })?;
        Ok(Client { http, base_url, api_key })
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// The server root: the API root without its `/v1`.
    pub fn root_url(&self) -> &str {
        self.base_url.strip_suffix("/v1").unwrap_or(&self.base_url)
    }

    /// Streams a chat completion, calling `on_event` as reasoning, text and tool calls arrive,
    /// and returns the finished turn.
    ///
    /// There is no overall timeout: radiance sends nothing until the first token, and reading a
    /// long uncached prompt takes minutes. Drop the future to cancel.
    pub async fn chat(
        &self,
        req: &ChatRequest,
        mut on_event: impl FnMut(StreamEvent),
    ) -> Result<AssistantTurn, Error> {
        let url = format!("{}/chat/completions", self.base_url);
        let mut resp = self.send(self.http.post(&url).json(&streaming_body(req)), &url).await?;
        let mut sse = SseDecoder::default();
        let mut turn = TurnAccumulator::default();
        loop {
            let bytes =
                resp.chunk().await.map_err(|source| Error::Http { url: url.clone(), source })?;
            let ended = bytes.is_none();
            let payloads = match bytes {
                Some(bytes) => sse.push(&bytes),
                None => sse.finish().into_iter().collect(),
            };
            for data in payloads {
                if data == "[DONE]" {
                    return Ok(turn.finish());
                }
                let chunk: Chunk = match serde_json::from_str(&data) {
                    Ok(chunk) => chunk,
                    Err(source) => return Err(Error::Decode { url, source, data }),
                };
                if let Some(error) = &chunk.error {
                    return Err(Error::Server { url, detail: error.to_string() });
                }
                turn.push(chunk).into_iter().for_each(&mut on_event);
            }
            if ended {
                // Some servers close without `[DONE]`; a reply with no finish reason was cut off.
                return if turn.has_finished() {
                    Ok(turn.finish())
                } else {
                    Err(Error::Incomplete { url })
                };
            }
        }
    }

    /// The prompt `req` renders to, from `/apply-template` (radiance, llama-server).
    ///
    /// `root` is the server that answers it: [`Client::root_url`], or behind llama-swap
    /// `<root>/upstream/<model>`.
    pub async fn apply_template(&self, root: &str, req: &ChatRequest) -> Result<String, Error> {
        let url = format!("{}/apply-template", root.trim_end_matches('/'));
        let resp = self.send(self.http.post(&url).json(&streaming_body(req)), &url).await?;
        let value = read_json(&url, resp).await?;
        match value.get("prompt").and_then(Value::as_str) {
            Some(prompt) => Ok(prompt.to_owned()),
            None => Err(Error::Unexpected { url, detail: format!("no \"prompt\" in {value}") }),
        }
    }

    /// Empties the server's prefix cache (`/reset_prefix_cache`: radiance, vLLM), for everyone
    /// using it. `root` is as for [`Client::apply_template`].
    pub async fn reset_prefix_cache(&self, root: &str) -> Result<(), Error> {
        let url = format!("{}/reset_prefix_cache", root.trim_end_matches('/'));
        self.send(self.http.post(&url).json(&json!({})), &url).await?;
        Ok(())
    }

    /// The ids of the models the server lists.
    pub async fn models(&self) -> Result<Vec<String>, Error> {
        let url = format!("{}/models", self.base_url);
        let resp = self.send(self.http.get(&url), &url).await?;
        let value = read_json(&url, resp).await?;
        let ids = value.get("data").and_then(Value::as_array).map(|models| {
            models
                .iter()
                .filter_map(|m| m.get("id").and_then(Value::as_str))
                .map(str::to_owned)
                .collect()
        });
        ids.ok_or_else(|| Error::Unexpected { url, detail: format!("no model list in {value}") })
    }

    /// Sends with the API key, and turns a non-success status into [`Error::Status`].
    async fn send(
        &self,
        req: reqwest::RequestBuilder,
        url: &str,
    ) -> Result<reqwest::Response, Error> {
        let req = match &self.api_key {
            Some(key) => req.bearer_auth(key),
            None => req,
        };
        let resp =
            req.send().await.map_err(|source| Error::Http { url: url.to_owned(), source })?;
        let status = resp.status();
        if status.is_success() {
            return Ok(resp);
        }
        let body = resp.text().await.unwrap_or_default();
        Err(Error::Status { url: url.to_owned(), status: status.as_u16(), body })
    }
}

async fn read_json(url: &str, resp: reqwest::Response) -> Result<Value, Error> {
    let text = resp.text().await.map_err(|source| Error::Http { url: url.to_owned(), source })?;
    serde_json::from_str(&text).map_err(|source| Error::Decode {
        url: url.to_owned(),
        source,
        data: text,
    })
}

/// `req` as a streaming request body, with usage reported at the end.
fn streaming_body(req: &ChatRequest) -> Value {
    // Strings, numbers, sequences and string-keyed maps: this cannot fail.
    let mut body = serde_json::to_value(req).expect("a ChatRequest serialises to JSON");
    body["stream"] = Value::Bool(true);
    body["stream_options"] = json!({"include_usage": true});
    body
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Message;

    #[test]
    fn streaming_body_adds_the_stream_fields() {
        let req = ChatRequest {
            model: "m".into(),
            messages: vec![Message::user("hi")],
            ..Default::default()
        };
        let body = streaming_body(&req);
        assert_eq!(body["stream"], json!(true));
        assert_eq!(body["stream_options"], json!({"include_usage": true}));
        assert_eq!(body["model"], json!("m"));
    }

    #[test]
    fn root_url_drops_v1() {
        let client = Client::new("http://127.0.0.1:8000/v1/", None, None, None).unwrap();
        assert_eq!(client.base_url(), "http://127.0.0.1:8000/v1");
        assert_eq!(client.root_url(), "http://127.0.0.1:8000");
    }

    #[test]
    fn a_pin_needs_an_https_url_and_a_sha256() {
        let pin = tls::pin_of(b"a certificate");
        let plain = Client::new("http://192.168.1.210:8443/v1", None, None, Some(&pin));
        assert!(plain.unwrap_err().to_string().contains("not https://"));
        let bad = Client::new("https://192.168.1.210:8443/v1", None, None, Some("nope"));
        assert!(bad.unwrap_err().to_string().contains("64 hex digits"));
        assert!(Client::new("https://192.168.1.210:8443/v1", None, None, Some(&pin)).is_ok());
        // Without a pin, https is checked against the authorities the system trusts.
        assert!(Client::new("https://example.com/v1", None, None, None).is_ok());
    }
}
