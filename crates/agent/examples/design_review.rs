//! Agent design with an INDEPENDENT design-review → fix loop (the netlist analog of a
//! Visual-Chain-of-Thought). Turn 1 drafts the design; then a FRESH LLM reviewer (unbiased —
//! the generating model rationalises its own slips) audits the committed netlist for
//! electrical-CORRECTNESS faults that pass ERC and look clean (pin-function mis-wires,
//! voltage-domain part-selection errors, topology errors); any HIGH-confidence critical/major
//! defects are fed back as a fix turn (the stateful agent edits its design), then re-reviewed.
//!
//! Usage: cargo run --release -p agent --example design_review -- <out.png> "<prompt>"

use agent::llm::Message;
use agent::{Agent, AutoApprove};
use kicad_bridge::cli::KicadCli;
use kicad_bridge::env::KicadEnv;
use serde_json::Value;

const REVIEW_SYSTEM: &str = r#"You are a senior electronics design engineer performing a NETLIST review (NOT a layout review).

You are given a circuit's intended function and its netlist in circuit-YAML (components with a refdes, a `part:` KiCAD lib_id, an optional `value:`, and pin->net maps; `between:[A,B]` = a 2-pin part across nets A,B; `positive/negative` = a polarized part; `power:NET` = a rail symbol; `label:global` = an exposed port).

Review ONLY for ELECTRICAL-DESIGN CORRECTNESS — faults a netlist can have while still passing ERC (connectivity) and looking clean:
1. PIN-FUNCTION mis-wires: a net wired to the WRONG pin for its function. Use your knowledge of the SPECIFIC part's pinout (e.g. SPI/ISP MISO/MOSI/SCK on the wrong MCU pin; a regulator FB pin not seeing the feedback divider; enable/boot/reset tied wrong).
2. WRONG VALUES: a resistor/cap value wrong by ~an order of magnitude for its role (1M I2C pull-up; 10nF "bulk" cap; a feedback divider whose ratio gives the wrong output voltage).
3. MISSING ESSENTIAL parts (cannot function without): crystal with no load caps; regulator with no output cap; an IC powered with NO decoupling at all.
4. VOLTAGE-DOMAIN / part-selection: a part operated outside its supply range (e.g. a 5V-only transceiver on a 3.3V rail).
5. TOPOLOGY errors: feedback/bias/reference wired wrong; reversed polarity; a missing return path.

Do NOT report layout, naming/style, nice-to-have protection, or anything you are not confident is a real electrical fault. A correct design SHOULD score 9-10 with few/no defects; do NOT invent defects.

REASON step by step FIRST (per IC, state its key pins from your knowledge of that exact part, then trace the critical nets), THEN emit, after a line `FINAL_JSON:`, a JSON object:
{"score": 0-10, "summary": "one line", "defects": [{"severity": "critical|major|minor", "confidence": "high|medium|low", "refdes": "U1", "issue": "short", "why": "the electrical reason"}]}"#;

/// Pull the verdict JSON out of the reasoning+JSON response (prefer the block after
/// FINAL_JSON:, else the last balanced object).
fn extract_json(text: &str) -> Option<Value> {
    let tail = text.rsplit_once("FINAL_JSON:").map(|(_, b)| b).unwrap_or(text);
    let cleaned = tail.trim().trim_start_matches("```json").trim_start_matches("```").trim_end_matches("```").trim();
    if let Ok(v) = serde_json::from_str::<Value>(cleaned) {
        return Some(v);
    }
    let (mut depth, mut start, mut last) = (0i32, None, None);
    for (i, b) in text.bytes().enumerate() {
        if b == b'{' {
            if depth == 0 {
                start = Some(i);
            }
            depth += 1;
        } else if b == b'}' {
            depth -= 1;
            if depth == 0 {
                if let Some(s) = start {
                    last = Some(&text[s..=i]);
                }
            }
        }
    }
    last.and_then(|s| serde_json::from_str(s).ok())
}

/// (score, high-confidence critical/major defect lines).
fn parse_review(v: &Value) -> (f64, Vec<String>) {
    let score = v.get("score").and_then(Value::as_f64).unwrap_or(0.0);
    let mut defects = Vec::new();
    for d in v.get("defects").and_then(Value::as_array).into_iter().flatten() {
        let sev = d.get("severity").and_then(Value::as_str).unwrap_or("");
        let conf = d.get("confidence").and_then(Value::as_str).unwrap_or("");
        if matches!(sev, "critical" | "major") && conf == "high" {
            let rd = d.get("refdes").and_then(Value::as_str).unwrap_or("");
            let issue = d.get("issue").and_then(Value::as_str).unwrap_or("");
            let why = d.get("why").and_then(Value::as_str).unwrap_or("");
            defects.push(format!("- {rd}: {issue} ({why})"));
        }
    }
    (score, defects)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let out = args.next().expect("usage: design_review <out.png> <prompt>");
    let prompt = args.next().expect("usage: design_review <out.png> <prompt>");

    let env = KicadEnv::detect().expect("no KiCAD environment detected");
    let tmp = tempfile::tempdir()?;
    let ctx = agent::tools::ToolCtx::for_project(env.clone(), tmp.path().to_path_buf())?;
    let sch_path = ctx.sch_path().to_path_buf();

    let review_client = agent::llm::from_env()?; // FRESH reviewer, separate conversation
    let mut agent = Agent::new(agent::llm::from_env()?, ctx);
    let mut approvals = AutoApprove::yes();

    eprintln!("=== turn 1: design ===");
    agent.run_turn(&prompt, &mut approvals, None).await?;

    const MAX_FIX: usize = 2;
    let mut first_score = None;
    let mut last_score = 0.0;
    for round in 0..=MAX_FIX {
        if !sch_path.exists() {
            eprintln!("no schematic written");
            break;
        }
        let netlist = sch_layout::lift::lift(&env, &sch_path).unwrap_or_default();
        let user = format!("Intended circuit: {prompt}\n\nNetlist:\n{netlist}");
        let review = review_client.complete(REVIEW_SYSTEM, &[Message::user(user)], &[]).await?;
        let Some(v) = extract_json(&review.text) else {
            eprintln!("[review {round}] parse failed");
            break;
        };
        let (score, defects) = parse_review(&v);
        first_score.get_or_insert(score);
        last_score = score;
        eprintln!(
            "[review {round}] score={score} high-conf critical/major defects={}",
            defects.len()
        );
        for d in &defects {
            eprintln!("    {d}");
        }
        if defects.is_empty() || round == MAX_FIX {
            break;
        }
        eprintln!("=== fix turn {} ===", round + 1);
        let fix = format!(
            "An INDEPENDENT design review of the netlist you just committed found these \
             high-confidence functional defects (they pass ERC but are electrically wrong):\n{}\n\n\
             Fix each one — search for a correct part or value if needed (e.g. a 3.3V-capable \
             transceiver, the right MCU function pin) — and re-commit the corrected design.",
            defects.join("\n")
        );
        agent.run_turn(&fix, &mut approvals, None).await?;
    }
    eprintln!(
        "\n=== design-review result: {} -> {} ===",
        first_score.unwrap_or(0.0),
        last_score
    );

    if sch_path.exists() {
        let svg_dir = tempfile::tempdir()?;
        let svg_path = KicadCli::new(&env).export_svg_opts(&sch_path, svg_dir.path(), true)?;
        let svg = std::fs::read_to_string(&svg_path)?;
        let png = agent::render::svg_to_png(&svg, 1600)?;
        std::fs::write(&out, png)?;
        std::fs::copy(&sch_path, std::path::Path::new(&out).with_extension("kicad_sch")).ok();
        if let Ok(y) = sch_layout::lift::lift(&env, &sch_path) {
            std::fs::write(std::path::Path::new(&out).with_extension("circuit.yaml"), y).ok();
        }
    }
    println!("done: {out} (review {} -> {last_score})", first_score.unwrap_or(0.0));
    Ok(())
}
