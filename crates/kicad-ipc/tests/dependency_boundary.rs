#[test]
fn bridge_does_not_depend_on_engine_models() {
    let manifest = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml"))
        .expect("read kicad-ipc Cargo.toml");

    for forbidden in ["pcb-model", "place-model"] {
        assert!(
            !manifest.lines().any(|line| {
                line.trim_start()
                    .strip_prefix(forbidden)
                    .is_some_and(|rest| rest.trim_start().starts_with('='))
            }),
            "kicad-ipc must expose bridge-owned DTOs instead of depending on {forbidden}"
        );
    }
}
