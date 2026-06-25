#[test]
fn tui_setup_does_not_capture_mouse_so_transcript_text_is_selectable() {
    let source = include_str!("../src/tui/mod.rs");

    assert!(
        !source.contains("EnableMouseCapture"),
        "mouse capture prevents normal terminal text selection/copy in the transcript"
    );
}
