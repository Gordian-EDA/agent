//! Live smoke test against real AWS Bedrock.
//!
//! Ignored by default (costs money + needs network). Run explicitly with:
//!   set -a; source .env; set +a; \
//!   cargo test -p agent --test llm_smoke -- --ignored --nocapture
//!
//! Skips gracefully if `AWS_BEARER_TOKEN_BEDROCK` is not set.

use agent::llm::{LlmClient, Message};

#[tokio::test]
#[ignore = "hits real Bedrock; run with --ignored"]
async fn bedrock_responds_to_a_trivial_prompt() {
    if std::env::var("AWS_BEARER_TOKEN_BEDROCK").is_err() {
        eprintln!("SKIP: AWS_BEARER_TOKEN_BEDROCK not set");
        return;
    }

    let client = agent::llm::from_env().expect("config from env/.env");
    let reply = client
        .complete(
            "You are a calculator. Reply with only the number.",
            &[Message::user("What is 6 times 7?")],
            &[], // no tools
        )
        .await
        .unwrap();

    assert!(reply.text.contains("42"), "got: {}", reply.text);
}
