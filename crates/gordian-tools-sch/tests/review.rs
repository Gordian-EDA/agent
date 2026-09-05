//! `review_schematic` over a scripted vision model: the tool must read the
//! critic's verdict JSON, report the mean of its samples, and hand the defects
//! back in the shape the agent acts on.

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
   "refs": ["C1", "U1"],
   "fix": "move C1 against U1.VDD and drop its stub straight to GND"}}
]}}"#
    )
}

#[tokio::test]
async fn reports_the_mean_score_and_the_mapped_defects() {
    let critic = ScriptedCritic::new([
        &verdict(4.0),
        &verdict(4.0),
        &verdict(4.0),
        &verdict(4.0),
        &verdict(8.0),
        &verdict(8.0),
        &verdict(8.0),
    ]);

    let result = review(&critic, &subject()).await.expect("review");

    assert_eq!(result["mean"], 5.71, "the mean of four 4s and three 8s");
    assert_eq!(result["score"], 6.0, "the mean, rounded");
    assert_eq!(
        result["samples"],
        serde_json::json!([4.0, 4.0, 4.0, 4.0, 8.0, 8.0, 8.0])
    );
    assert_eq!(result["reference"], "human sheet rated 9");
    let defect = &result["defects"][0];
    assert_eq!(defect["kind"], "spacing");
    assert_eq!(defect["severity"], "major");
    assert_eq!(defect["refs"], serde_json::json!(["C1", "U1"]));
    assert!(defect["what"].as_str().expect("what").contains("C1 sits"));
    assert!(defect["fix"].as_str().expect("fix").contains("move C1"));
    assert_eq!(
        result["_image_path"], "/tmp/render-001.png",
        "the annotated render is what the model is shown"
    );
    assert_eq!(
        summary(&result),
        "review mean 5.71 (score 6/10) vs the human sheet rated 9 \
         (samples 4,4,4,4,8,8,8); 1 defect(s)"
    );
}

#[tokio::test]
async fn an_unparsable_grader_is_reported_not_scored() {
    let critic = ScriptedCritic::new([
        "the image did not load",
        "sorry",
        "n/a",
        "no",
        "cannot",
        "unavailable",
        "?",
    ]);

    let result = review(&critic, &subject()).await.expect("review");

    assert!(result.get("score").is_none(), "no score was ever parsed");
    assert!(
        result["error"]
            .as_str()
            .expect("error")
            .contains("no parsable verdict")
    );
}
