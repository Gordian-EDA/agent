//! Actionable guidance shared with the agent-facing tool dispatcher.

use serde_json::{Value, json};

/// Add a positional pin suggestion to the standard unknown-pin refusal.
///
/// Some KiCad footprints use named pads such as `S SN T TN`. When a caller
/// supplies `2`, the second reported pad is the useful one-call correction even
/// though `2` is not itself an electrical pin name.
pub fn enrich_positional_pin_refusal(mut result: Value) -> Value {
    let Some(error) = result.get("error").and_then(Value::as_str) else {
        return result;
    };
    let Some((reference, rest)) = error
        .strip_prefix('`')
        .and_then(|error| error.split_once("` has no pin `"))
    else {
        return result;
    };
    let Some((requested, available)) = rest.split_once("`; it has ") else {
        return result;
    };
    let Some(position) = requested
        .parse::<usize>()
        .ok()
        .filter(|position| *position > 0)
    else {
        return result;
    };
    let Some(pin) = available.split_whitespace().nth(position - 1) else {
        return result;
    };
    let reference = reference.to_owned();
    let requested = requested.to_owned();
    let pin = pin.to_owned();
    let requested_pin = format!("{reference}.{requested}");
    let suggested_pin = format!("{reference}.{pin}");
    result["did_you_mean"] = Value::Object(serde_json::Map::from_iter([(
        requested_pin.clone(),
        json!(suggested_pin.clone()),
    )]));
    result["fix"] = json!(format!(
        "replace `{requested_pin}` with `{suggested_pin}`; `{requested}` means position \
         {position}, whose pad in the reported order is `{pin}`"
    ));
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numeric_unknown_pin_suggests_the_pad_at_that_position() {
        let refusal = json!({
            "error": "`J2` has no pin `2`; it has S SN T TN"
        });

        let enriched = enrich_positional_pin_refusal(refusal);

        assert_eq!(enriched["did_you_mean"]["J2.2"], json!("J2.SN"));
        assert!(enriched["fix"].as_str().unwrap().contains("position 2"));
    }

    #[test]
    fn named_unknown_pins_keep_the_original_refusal() {
        let refusal = json!({
            "error": "`J2` has no pin `shield`; it has S SN T TN"
        });

        assert_eq!(enrich_positional_pin_refusal(refusal.clone()), refusal);
    }
}
