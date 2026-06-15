//! Smoke-test the configured LLM backend: print the selected provider/model and
//! run one trivial completion (and a one-tool round-trip). Verifies credentials
//! and the request/response mapping against the real endpoint.
//!
//! Usage: cargo run -p agent --example llm_smoke

use agent::llm::{self, Message, ToolDef};
use serde_json::json;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let (provider, model) = agent::config::provider_status();
    eprintln!("provider = {provider}\nmodel    = {model}\n");

    let client = llm::from_env()?;

    // 1. Plain text completion.
    let c = client
        .complete("You are a terse assistant.", &[Message::user("Reply with exactly: OK")], &[])
        .await?;
    println!("text       : {:?}", c.text.trim());
    println!("stop_reason: {}", c.stop_reason);
    println!("tokens     : in={} out={}", c.input_tokens, c.output_tokens);

    // 2. Tool-call round-trip (does the model emit a structured tool call?).
    let tool = ToolDef {
        name: "get_weather".to_string(),
        description: "Get the current weather for a city.".to_string(),
        input_schema: json!({
            "type": "object",
            "properties": { "city": { "type": "string" } },
            "required": ["city"],
        }),
    };
    let c2 = client
        .complete(
            "Use tools when relevant.",
            &[Message::user("What's the weather in Paris? Use the tool.")],
            std::slice::from_ref(&tool),
        )
        .await?;
    println!("\ntool_calls : {} ({:?})", c2.tool_calls.len(), c2.stop_reason);
    for call in &c2.tool_calls {
        println!("  -> {} {}", call.name, call.input);
    }
    Ok(())
}
