//! Reading a JSON verdict out of a reasoning-then-JSON reply.
//!
//! Every critic in the workspace is asked to think in prose first and commit to
//! strict JSON after a `FINAL_JSON:` marker, because tracing the evidence before
//! answering is what kills its false positives. One parser serves them all.

use serde_json::Value;

/// The verdict object in `text`: the block after the last `FINAL_JSON:` marker
/// if there is one, else the last balanced `{...}` object. `None` when the reply
/// carries no JSON at all.
pub fn verdict_json(text: &str) -> Option<Value> {
    let tail = text
        .rsplit_once("FINAL_JSON:")
        .map(|(_, after)| after)
        .unwrap_or(text);
    let cleaned = tail
        .trim()
        .trim_start_matches("```json")
        .trim_start_matches("```")
        .trim_end_matches("```")
        .trim();
    if let Ok(value) = serde_json::from_str::<Value>(cleaned) {
        return Some(value);
    }
    last_balanced_object(text).and_then(|s| serde_json::from_str(s).ok())
}

fn last_balanced_object(text: &str) -> Option<&str> {
    let (mut depth, mut start, mut last) = (0i32, None, None);
    for (i, byte) in text.bytes().enumerate() {
        if byte == b'{' {
            if depth == 0 {
                start = Some(i);
            }
            depth += 1;
        } else if byte == b'}' {
            depth -= 1;
            if depth == 0
                && let Some(s) = start
            {
                last = Some(&text[s..=i]);
            }
        }
    }
    last
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefers_the_block_after_the_marker() {
        let text = "{\"score\": 1}\nFINAL_JSON:\n{\"score\": 9}";
        assert_eq!(verdict_json(text).unwrap()["score"], 9);
    }

    #[test]
    fn strips_a_fenced_block() {
        let text = "FINAL_JSON:\n```json\n{\"score\": 7}\n```";
        assert_eq!(verdict_json(text).unwrap()["score"], 7);
    }

    #[test]
    fn falls_back_to_the_last_object() {
        let text = "prose {\"score\": 3} more prose {\"score\": 8} tail";
        assert_eq!(verdict_json(text).unwrap()["score"], 8);
    }

    #[test]
    fn no_json_at_all() {
        assert!(verdict_json("the image failed to load").is_none());
    }
}
