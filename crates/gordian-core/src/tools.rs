//! The seven tools the designer model drives, exactly as in the reference agent.

use gordian_llm::Tool;
use serde_json::json;

/// Tool definitions for a fresh design; `edit` swaps `build`'s argument from a
/// whole design to a patch against the sheet already in the project.
pub fn tool_defs(edit: bool) -> Vec<Tool> {
    vec![
        Tool::new("search_symbols")
            .with_description(
                "Search the KiCad stock symbol libraries. Returns up to 12 matching lib ids with \
                 descriptions. Query by part number, function or lib prefix (e.g. 'STM32F103C8', \
                 'USB-C connector', 'Device:R').",
            )
            .with_schema(json!({
                "type": "object",
                "properties": {"query": {"type": "string"}},
                "required": ["query"]
            })),
        Tool::new("symbol_info")
            .with_description(
                "Pin table of a symbol (number, name, electrical type, side) per unit. Call it for \
                 every symbol you use before writing its pin map.",
            )
            .with_schema(json!({
                "type": "object",
                "properties": {"lib_id": {"type": "string"}, "unit": {"type": "integer"}},
                "required": ["lib_id"]
            })),
        if edit { build_patch_tool() } else { build_tool() },
        Tool::new("erc")
            .with_description(
                "Run KiCad ERC on the last build. Returns the violations (errors and warnings).",
            )
            .with_schema(json!({"type": "object", "properties": {}})),
        Tool::new("render")
            .with_description(
                "Render the last build and return the image (with a coordinate grid in grid units) \
                 so you can inspect it.",
            )
            .with_schema(json!({"type": "object", "properties": {}})),
        Tool::new("review")
            .with_description(
                "Independent visual review of the last build: score 1-10 against a professional \
                 reference sheet plus a list of defects with coordinates. Renders first if needed.",
            )
            .with_schema(json!({"type": "object", "properties": {}})),
        Tool::new("finish")
            .with_description(
                "Deliver the last build. Accepted when it has no ISSUES, ERC ran without errors and \
                 the review scored 8 or better; otherwise it tells you what is missing (force=true \
                 overrides after you judged the remaining points acceptable).",
            )
            .with_schema(json!({
                "type": "object",
                "properties": {"summary": {"type": "string"}, "force": {"type": "boolean"}},
                "required": ["summary"]
            })),
    ]
}

fn build_tool() -> Tool {
    Tool::new("build")
        .with_description(
            "Lay out and compile the design into a .kicad_sch and run the fast checks \
             (connectivity, geometry, text collisions, netlist). Returns ISSUES to fix, warnings \
             and the resulting netlist. Fast; no KiCad, no image.",
        )
        .with_schema(json!({
            "type": "object",
            "properties": {"design": {
                "type": "object",
                "description": "The full design JSON (title, parts[...] with pin maps, layout[...] blocks with row/col trees, flags[...])."
            }},
            "required": ["design"]
        }))
}

fn build_patch_tool() -> Tool {
    Tool::new("build")
        .with_description(
            "Apply a patch to the ORIGINAL design (remove/update/add), compile to .kicad_sch and \
             run the fast checks. Returns issues, warnings, the netlist and the net changes versus \
             the original.",
        )
        .with_schema(json!({
            "type": "object",
            "properties": {"patch": {
                "type": "object",
                "description": "{remove:[ids], update:{id:{fields}}, add:{circuit:{parts:[...],layout:[...],flags:[...]}, parts:[], wires:[], labels:[], power:[], nc:[], texts:[]}, title?, rev?, paper?}"
            }},
            "required": ["patch"]
        }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use genai::chat::ToolName;

    #[test]
    fn the_surface_is_the_seven_tools_of_the_reference_agent() {
        let names: Vec<String> = tool_defs(false)
            .iter()
            .map(|t| match &t.name {
                ToolName::Custom(name) => name.clone(),
                other => format!("{other:?}"),
            })
            .collect();
        assert_eq!(
            names,
            [
                "search_symbols",
                "symbol_info",
                "build",
                "erc",
                "render",
                "review",
                "finish"
            ]
        );
    }

    #[test]
    fn edit_mode_builds_from_a_patch() {
        let build = tool_defs(true).into_iter().nth(2).unwrap();
        assert!(build.schema.unwrap()["properties"]["patch"].is_object());
    }
}
