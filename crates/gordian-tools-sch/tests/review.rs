//! `review_schematic` over a scripted vision model: the tool must read the
//! critic's verdict JSON, report the modal score of its samples, and hand the
//! defects back in the shape the agent acts on.

use async_trait::async_trait;
use gordian_llm::{ChatMessage, MessageContent, Provider, StreamEnd, Tool};
use gordian_tools_sch::render::SheetPngs;
use gordian_tools_sch::review::{SAMPLES, Subject, review, summary};
use sch_floorplan::visual::VisualFacts;
use std::sync::Mutex;

/// Replies with one scripted verdict per call, in order.
struct ScriptedCritic(Mutex<Vec<String>>);

impl ScriptedCritic {
    fn new(replies: [&str; SAMPLES]) -> Self {
        Self(Mutex::new(
            replies.iter().rev().map(ToString::to_string).collect(),
        ))
    }
}

#[async_trait]
impl Provider for ScriptedCritic {
    async fn complete(
        &self,
        _system: &str,
        _messages: &[ChatMessage],
        _tools: &[Tool],
    ) -> anyhow::Result<StreamEnd> {
        let reply = self.0.lock().expect("script").pop().expect("a reply left");
        Ok(StreamEnd {
            captured_content: Some(MessageContent::from_text(reply)),
            ..Default::default()
        })
    }
}

/// An engine measurement with nothing wrong in it — what the critic is told is
/// impossible on this sheet.
fn clean_geometry() -> VisualFacts {
    VisualFacts {
        sheet_extent: [0.0, 0.0, 210.0, 297.0],
        body_overlaps: Vec::new(),
        wires_through_bodies: Vec::new(),
        text_collisions: Vec::new(),
        off_grid_pins: Vec::new(),
        dangling_wire_ends: Vec::new(),
    }
}

fn subject() -> Subject {
    Subject::new(
        SheetPngs {
            clean: b"clean".to_vec(),
            annotated: b"annotated".to_vec(),
            annotated_path: "/tmp/render-001.png".into(),
            parts: "R1=10k, U1=STM32F103C8T6".into(),
            visual: clean_geometry(),
        },
        b"anchor".to_vec(),
    )
}

fn verdict(score: f64) -> String {
    format!(
        r#"I traced every wire.
FINAL_JSON:
{{"score": {score}, "summary": "reads well", "defects": [
  {{"severity": "major", "confidence": "high", "category": "spacing",
   "location": "C1", "description": "C1 sits three columns from U1's VDD pin",
   "at_mm": [131.5, 88.9], "refs": ["C1", "U1"],
   "fix": "move C1 against U1.VDD and drop its stub straight to GND"}}
]}}"#
    )
}

#[tokio::test]
async fn reports_the_modal_score_and_the_mapped_defects() {
    let critic = ScriptedCritic::new([&verdict(7.0), &verdict(9.0), &verdict(7.0)]);

    let result = review(&critic, &subject()).await.expect("review");

    assert_eq!(result["score"], 7.0, "modal of 7, 9, 7");
    assert_eq!(result["samples"], serde_json::json!([7.0, 9.0, 7.0]));
    assert_eq!(result["reference"], "human sheet rated 9");
    let defect = &result["defects"][0];
    assert_eq!(defect["kind"], "spacing");
    assert_eq!(defect["severity"], "major");
    assert_eq!(defect["at_mm"], serde_json::json!([131.5, 88.9]));
    assert_eq!(defect["refs"], serde_json::json!(["C1", "U1"]));
    assert!(defect["what"].as_str().expect("what").contains("C1 sits"));
    assert!(defect["fix"].as_str().expect("fix").contains("move C1"));
    assert_eq!(
        result["_image_path"], "/tmp/render-001.png",
        "the annotated render is what the model is shown"
    );
    assert_eq!(
        summary(&result),
        "review 7/10 vs the human sheet rated 9 (samples 7,9,7); 1 defect(s)"
    );
}

#[tokio::test]
async fn an_unparsable_grader_is_reported_not_scored() {
    let critic = ScriptedCritic::new(["the image did not load", "sorry", "n/a"]);

    let result = review(&critic, &subject()).await.expect("review");

    assert!(result.get("score").is_none(), "no score was ever parsed");
    assert!(
        result["error"]
            .as_str()
            .expect("error")
            .contains("no parsable verdict")
    );
}
