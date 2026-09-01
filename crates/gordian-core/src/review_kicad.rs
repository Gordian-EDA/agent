//! Independent visual review of a rendered PCB.

use crate::{Binary, Provider};
use anyhow::Result;
use gordian_runtime::config::ReviewConfig;

/// Diverse review lenses for PCB placement and routing quality.
pub const BOARD_LENSES: &[&str] = &[
    "",
    "PLACEMENT and grouping: related parts should be close, connectors should be near board edges, and the design should use the board area compactly",
    "ROUTING and reading: avoidable trace detours, congestion, excessive vias, and silkscreen text colliding with pads, copper, or other text",
];

const QUICK_BOARD_LENSES: &[&str] = &[
    "check the visible worst layout issues only: related-part grouping, connector placement, board use, routing directness, congestion, and silkscreen legibility",
];

const BOARD_REVIEW_SYSTEM: &str = r#"Review one rendered KiCAD PCB plot for placement/routing quality, not electrical correctness.

DRC ground truth says zero shorts, clearance violations, and unconnected items;
do not report those. Judge related-part grouping, connector edge placement,
board utilisation, routing directness/neatness, via economy, and silkscreen
legibility. Different copper colors are different layers. Minor issues alone
score >=8; a real major scores 5-7; critical unusable/broken-looking issues
score <=4.

Reason briefly from visible evidence and name a concrete better alternative for
each defect. Then emit strict JSON after `FINAL_JSON:` with:
{"score":0-10,"summary":"one sentence","defects":[{"severity":"critical|major|minor","confidence":"high|medium|low","category":"placement|board-utilisation|routing-directness|routing-neatness|via-economy|silkscreen|other","location":"refdes/region","description":"concrete observation","verification":"visible evidence"}]}"#;

/// Review a rendered board and return its actionable score and high-confidence defects.
pub async fn review_board(
    client: &dyn Provider,
    intent: &str,
    image: Binary,
    config: &ReviewConfig,
) -> Result<(f64, Vec<String>)> {
    let lenses = if config.ensemble {
        BOARD_LENSES
    } else {
        QUICK_BOARD_LENSES
    };
    let prompt = format!(
        "Audit this rendered PCB layout for visual quality. Intended circuit: {intent}. Reason first, then emit the FINAL_JSON verdict."
    );
    crate::review::review_image_with_retry(
        client,
        BOARD_REVIEW_SYSTEM,
        lenses,
        &prompt,
        image,
        config.retry_json,
    )
    .await
}
