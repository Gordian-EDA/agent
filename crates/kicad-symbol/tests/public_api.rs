use kicad_symbol::geometry::SymbolGeometry;
use kicad_symbol::search::SymbolIndex;
#[allow(unused_imports)]
use kicad_symbol::symlib;
use kicad_symbol::{PinDir, PinMeta, PinType, SymbolMeta, SymbolTable, find_pin};

#[test]
fn root_exports_and_existing_modules_remain_importable() {
    let meta = SymbolMeta {
        pins: vec![PinMeta {
            number: "1".into(),
            name: "A".into(),
            etype: PinType::Passive,
            dir: PinDir::Passive,
            unit: 1,
        }],
    };

    assert_eq!(find_pin(&meta.pins, "1").unwrap().name, "A");
    assert!(SymbolTable::with_basics().symbol("Device:R").is_some());

    let _geometry_loader: fn(&kicad_env::KicadEnv, &str) -> std::io::Result<SymbolGeometry> =
        SymbolGeometry::load;
    let _search_builder: fn(&kicad_env::KicadEnv) -> std::io::Result<SymbolIndex> =
        SymbolIndex::build;
}
