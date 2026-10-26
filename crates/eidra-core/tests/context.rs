//! The end of the context: a prompt the server cannot fit ends the session at its limit, rather
//! than with an error every later request would repeat.

mod common;

use common::{Quiet, Reply, agent, fake_server, scratch};
use eidra_core::Outcome;
use serde_json::json;

#[tokio::test]
async fn a_prompt_the_server_cannot_fit_ends_the_session_at_its_limit() {
    let dir = scratch("context");
    // As radiance refuses it (GUIDE.md §5.8).
    let refusal = json!({"error": {
        "code": "context_length_exceeded",
        "message": "This model's maximum context length is 225280 tokens: the request (226000 \
                    tokens) exceeds the available context size (225280 tokens) and leaves \
                    nothing to generate into; shorten the prompt by at least 721 tokens",
        "n_prompt_tokens": 226000,
        "n_ctx": 225280,
    }});
    let url = fake_server(vec![Reply::Refuse(400, refusal)]).await;
    let mut agent = agent(url, &dir);
    agent.push_user("go on").unwrap();
    assert_eq!(agent.run(&mut Quiet, 5).await.unwrap(), Outcome::ContextLimit(226_000));
    let log = std::fs::read_to_string(agent.session_log()).unwrap();
    assert!(log.lines().last().unwrap().contains("context_limit"), "{log}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn other_refusals_are_still_errors() {
    let dir = scratch("refusal");
    let refusal = json!({"error": {"code": "invalid_request", "message": "bad tool schema"}});
    let url = fake_server(vec![Reply::Refuse(400, refusal)]).await;
    let mut agent = agent(url, &dir);
    agent.push_user("go on").unwrap();
    let error = agent.run(&mut Quiet, 5).await.unwrap_err();
    assert!(format!("{error:#}").contains("bad tool schema"), "{error:#}");
    let _ = std::fs::remove_dir_all(&dir);
}
