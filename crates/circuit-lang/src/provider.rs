//! Symbol metadata injection point. Implemented by kicad-bridge
//! against real .kicad_sym libraries; mocked here for tests.

use std::collections::HashMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PinType {
    PowerInput,
    PowerOutput,
    Passive,
    Other,
}

#[derive(Debug, Clone)]
pub struct PinMeta {
    pub number: String,
    pub name: String,
    pub etype: PinType,
    pub unit: u8, // 1-based; 1 for single-unit symbols
}

#[derive(Debug, Clone, Default)]
pub struct SymbolMeta {
    pub pins: Vec<PinMeta>,
}

/// Resolve a pin reference (`id`) within a pin list, matching by **number
/// first, then by name**. This is `circuit-lang`'s canonical pin-resolution
/// order; reuse it instead of hand-rolling the same `find().or_else(find())`.
pub fn find_pin<'a>(pins: &'a [PinMeta], id: &str) -> Option<&'a PinMeta> {
    pins.iter()
        .find(|p| p.number == id)
        .or_else(|| pins.iter().find(|p| p.name == id))
}

pub trait SymbolProvider {
    fn symbol(&self, lib_id: &str) -> Option<&SymbolMeta>;
    /// Closest known lib_ids for an unknown one (for diagnostics).
    fn suggest(&self, lib_id: &str) -> Vec<String>;
}

#[derive(Default)]
pub struct MockSymbolProvider {
    symbols: HashMap<String, SymbolMeta>,
}

impl MockSymbolProvider {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add(&mut self, lib_id: &str, pins: Vec<(&str, &str, PinType, u8)>) -> &mut Self {
        let pins = pins
            .into_iter()
            .map(|(number, name, etype, unit)| PinMeta {
                number: number.into(),
                name: name.into(),
                etype,
                unit,
            })
            .collect();
        self.symbols.insert(lib_id.into(), SymbolMeta { pins });
        self
    }

    /// Device:R / Device:C / Device:D / Device:LED — enough for most tests.
    pub fn with_basics() -> Self {
        use PinType::*;
        let mut p = Self::new();
        for id in ["Device:R", "Device:C", "Device:L"] {
            p.add(id, vec![("1", "~", Passive, 1), ("2", "~", Passive, 1)]);
        }
        p.add(
            "Device:D",
            vec![("1", "K", Passive, 1), ("2", "A", Passive, 1)],
        );
        p.add(
            "Device:LED",
            vec![("1", "K", Passive, 1), ("2", "A", Passive, 1)],
        );
        // Power & ground symbols are ordinary single-pin components; their lone
        // power-input pin carries the rail name. Cover the common library names.
        for (id, net) in [
            ("power:GND", "GND"),
            ("power:VCC", "VCC"),
            ("power:+3V3", "+3V3"),
            ("power:+5V", "+5V"),
            ("power:+12V", "+12V"),
            ("power:VBUS", "VBUS"),
        ] {
            p.add(id, vec![("1", net, PowerInput, 1)]);
        }
        // Net-label marker (not a real KiCAD symbol — a synthetic single-pin part):
        // `label:global` marks its net a board I/O port (drawn as a global-label).
        p.add("label:global", vec![("1", "~", Passive, 1)]);
        p
    }
}

impl SymbolProvider for MockSymbolProvider {
    fn symbol(&self, lib_id: &str) -> Option<&SymbolMeta> {
        self.symbols.get(lib_id)
    }
    fn suggest(&self, lib_id: &str) -> Vec<String> {
        let mut hits: Vec<(usize, &String)> = self
            .symbols
            .keys()
            .map(|k| {
                (
                    strsim::levenshtein(&lib_id.to_lowercase(), &k.to_lowercase()),
                    k,
                )
            })
            .filter(|(d, _)| *d <= 3)
            .collect();
        hits.sort();
        // Only surface the closest tier (ties on the minimum distance): a query
        // that exactly matches one symbol must not pull in its near-neighbours.
        let best = match hits.first() {
            Some((d, _)) => *d,
            None => return Vec::new(),
        };
        hits.into_iter()
            .take_while(|(d, _)| *d == best)
            .take(3)
            .map(|(_, k)| k.clone())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mock_provider_serves_symbols_and_suggestions() {
        let p = MockSymbolProvider::with_basics(); // includes Device:R, Device:C
        let r = p.symbol("Device:R").unwrap();
        assert_eq!(r.pins.len(), 2);
        assert!(p.symbol("Device:Q").is_none());
        assert_eq!(p.suggest("Device:r"), vec!["Device:R".to_string()]);
    }
}
