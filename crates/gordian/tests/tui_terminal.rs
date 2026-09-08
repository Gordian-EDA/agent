#[test]
fn tui_setup_captures_the_mouse_so_the_wheel_never_masquerades_as_arrow_keys() {
    // Without mouse capture, most terminals translate wheel scroll into
    // synthetic Up/Down key events while in the alternate screen — identical
    // to the user's own key presses, so Up/Down could never be trusted to
    // mean prompt history alone. Capturing the mouse makes a real scroll
    // arrive as its own `Event::Mouse`, so the keyboard and the wheel can be
    // mapped independently (Up/Down = history, wheel = transcript scroll).
    // The cost — native click-drag selection in the terminal — is accepted;
    // most terminals still offer it behind a modifier (e.g. Shift-drag).
    let source = include_str!("../src/tui/mod.rs");

    assert!(
        source.contains("EnableMouseCapture"),
        "mouse capture must be enabled for the wheel and the keyboard to be distinguishable"
    );
}
