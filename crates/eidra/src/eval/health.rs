//! Whether the model server still answers, watched for the length of a run.
//!
//! A server can stop answering without refusing anything. On 2026-10-28 one of radiance's two cards
//! dropped off the PCIe bus an hour into a run, and radiance went on taking requests it never
//! answered: each attempt then waited out its task's 50 minutes, and the run would have gone on
//! like that until morning. So a watchdog asks the server for one token every few minutes. When
//! two probes in a row go unanswered the server is down: the attempts running are stopped and
//! recorded as lost (`Attempt::lost`), and no other starts until it answers again.

use std::time::Duration;

use eidra_llm::{ChatRequest, Client, Message};
use tokio::sync::watch;

/// How the watchdog paces itself.
#[derive(Debug, Clone, Copy)]
pub struct Timing {
    /// How long a probe waits for its token. A well server answers a short prompt in a second,
    /// and llama-swap brings back a model it swapped out in about 35 s.
    pub timeout: Duration,
    /// Between probes while the server answers.
    pub interval: Duration,
    /// After a probe went unanswered, and while the server is down.
    pub retry: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Timing {
            timeout: Duration::from_secs(150),
            interval: Duration::from_secs(120),
            retry: Duration::from_secs(30),
        }
    }
}

/// Probes in a row that go unanswered before the server counts as down. One alone could be a
/// server with every slot busy.
const MISSES: u32 = 2;

/// What the watchdog last found.
#[derive(Clone)]
pub struct Health {
    up: watch::Receiver<bool>,
}

impl Health {
    /// Starts watching the server `client` reaches, which is taken to answer until it is found
    /// not to. The watchdog stops once every handle is dropped.
    pub fn watch(client: Client, model: String, timing: Timing) -> Health {
        let (state, up) = watch::channel(true);
        tokio::spawn(async move {
            let mut missed = 0;
            loop {
                let pause = if missed == 0 { timing.interval } else { timing.retry };
                tokio::select! {
                    () = tokio::time::sleep(pause) => {}
                    () = state.closed() => return,
                }
                if probe(&client, &model, timing.timeout).await.is_ok() {
                    if missed >= MISSES {
                        eprintln!("{}: the model server answers again", clock());
                    }
                    missed = 0;
                    state.send_replace(true);
                } else {
                    missed += 1;
                    if missed == MISSES {
                        eprintln!(
                            "{}: the model server has not answered {MISSES} requests in a row; \
                             attempts running are stopped and none start until it answers",
                            clock()
                        );
                        state.send_replace(false);
                    }
                }
            }
        });
        Health { up }
    }

    pub fn is_up(&self) -> bool {
        *self.up.borrow()
    }

    /// Returns once the server is found down, at once if it is now.
    pub async fn down(&self) {
        let mut up = self.up.clone();
        let _ = up.wait_for(|up| !up).await;
    }

    /// Whether the server answers again within `limit`.
    pub async fn up_within(&self, limit: Duration) -> bool {
        let mut up = self.up.clone();
        matches!(tokio::time::timeout(limit, up.wait_for(|up| *up)).await, Ok(Ok(_)))
    }
}

/// Whether the server answers a request for one token within `timeout`, and if not, why.
pub async fn probe(client: &Client, model: &str, timeout: Duration) -> Result<(), String> {
    let request = ChatRequest {
        model: model.to_owned(),
        messages: vec![Message::user("Say OK.")],
        max_tokens: Some(1),
        ..ChatRequest::default()
    };
    match tokio::time::timeout(timeout, client.chat(&request, |_| {})).await {
        Ok(Ok(_)) => Ok(()),
        Ok(Err(error)) => Err(error.to_string()),
        Err(_) => Err(format!("no reply in {} s", timeout.as_secs())),
    }
}

/// The time of day, UTC, for the run's log.
fn clock() -> String {
    let (_, stamp) = super::now();
    stamp[11..19].to_owned()
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    use super::*;

    /// A server that answers each request with one token while `answering` is set, and otherwise
    /// takes it and says nothing, as radiance did with a card gone.
    async fn server(answering: Arc<AtomicBool>) -> Client {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/v1", listener.local_addr().unwrap());
        tokio::spawn(async move {
            loop {
                let (mut conn, _) = listener.accept().await.unwrap();
                let answering = answering.load(Ordering::SeqCst);
                tokio::spawn(async move {
                    let mut buf = [0u8; 4096];
                    let _ = conn.read(&mut buf).await;
                    if !answering {
                        tokio::time::sleep(Duration::from_secs(60)).await;
                        return;
                    }
                    let body = "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"OK\"},\
                                \"finish_reason\":\"length\"}]}\n\ndata: [DONE]\n\n";
                    let head = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\
                                connection: close\r\n\r\n";
                    let _ = conn.write_all(format!("{head}{body}").as_bytes()).await;
                });
            }
        });
        Client::new(&url, None, None).unwrap()
    }

    fn quick() -> Timing {
        Timing {
            timeout: Duration::from_millis(200),
            interval: Duration::from_millis(50),
            retry: Duration::from_millis(20),
        }
    }

    #[tokio::test]
    async fn a_server_that_stops_answering_is_found_down_and_then_up_again() {
        let answering = Arc::new(AtomicBool::new(true));
        let client = server(Arc::clone(&answering)).await;
        assert_eq!(probe(&client, "m", Duration::from_secs(5)).await, Ok(()));

        let health = Health::watch(client, "m".into(), quick());
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(health.is_up());

        answering.store(false, Ordering::SeqCst);
        let found = tokio::time::timeout(Duration::from_secs(5), health.down()).await;
        assert!(found.is_ok(), "never found down");
        assert!(!health.is_up());
        assert!(!health.up_within(Duration::from_millis(100)).await);

        answering.store(true, Ordering::SeqCst);
        assert!(health.up_within(Duration::from_secs(5)).await);
    }

    #[tokio::test]
    async fn a_server_with_nothing_listening_does_not_answer() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/v1", listener.local_addr().unwrap());
        drop(listener);
        let client = Client::new(&url, None, None).unwrap();
        let error = probe(&client, "m", Duration::from_secs(5)).await.unwrap_err();
        assert!(error.starts_with(&format!("request to {url}/chat/completions failed")), "{error}");
    }
}
