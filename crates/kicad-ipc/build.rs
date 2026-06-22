//! Generate Rust types from the vendored KiCAD IPC `.proto` files (KiCAD 9.0,
//! `api/proto/`). The `google.protobuf.*` well-known types are mapped onto
//! `prost-types` rather than regenerated.

use std::path::PathBuf;

fn main() {
    let root = PathBuf::from("proto");
    let protos = [
        "common/envelope.proto",
        "common/types/base_types.proto",
        "common/types/enums.proto",
        "common/types/project_settings.proto",
        "common/commands/base_commands.proto",
        "common/commands/editor_commands.proto",
        "common/commands/project_commands.proto",
        "board/board.proto",
        "board/board_types.proto",
        "board/board_commands.proto",
    ]
    .iter()
    .map(|p| root.join(p))
    .collect::<Vec<_>>();

    let mut cfg = prost_build::Config::new();
    // (prost-build already maps the google well-knowns onto prost-types.)
    // Generate `prost::Name` impls so commands/items pack into `google.protobuf.Any`
    // by their type URL. KiCAD strips a `type.googleapis.com/` prefix when matching
    // the Any's type, so the URL MUST carry that domain (prost defaults to none,
    // which yields `/kiapi...` and a decode failure).
    cfg.enable_type_names();
    cfg.type_name_domain(["."], "type.googleapis.com");
    // Emit one file with the full nested `kiapi::...` module tree.
    cfg.include_file("_proto.rs");
    cfg.compile_protos(&protos, &[root])
        .expect("compile KiCAD IPC protos");

    println!("cargo:rerun-if-changed=proto");
}
