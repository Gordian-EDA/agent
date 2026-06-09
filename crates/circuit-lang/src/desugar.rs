//! Sugar -> kernel lowering (spec §5.5). The reconciler and lints see
//! only the output of this pass.

use crate::diag::Diagnostics;
use crate::model::*;
use crate::provider::SymbolProvider;
use crate::surface::*;
use indexmap::IndexMap;

/// Closed alias table (spec §5.5) — exactly these five.
fn alias(part: &str) -> String {
    match part {
        "R" => "Device:R".into(),
        "C" => "Device:C".into(),
        "L" => "Device:L".into(),
        "D" => "Device:D".into(),
        "LED" => "Device:LED".into(),
        other => other.into(),
    }
}

pub fn desugar(s: &SurfaceDesign, provider: &dyn SymbolProvider) -> (Design, Diagnostics) {
    let mut diags = Diagnostics::default();
    let mut d = Design {
        name: s.name.clone(),
        description: s.description.clone(),
        ..Default::default()
    };

    // rails -> power net attrs
    for (rail, _span) in &s.rails {
        d.nets.entry(rail.clone()).or_default().power = true;
    }
    for (net, attrs) in &s.nets {
        let e = d.nets.entry(net.clone()).or_default();
        e.power |= attrs.power;
        e.class = attrs.class.clone();
    }

    // surface components -> kernel components (pins still raw, resolved below)
    let mut raw_pins: Vec<RawPin> = Vec::new();
    for (bname, sb) in &s.blocks {
        let mut block = Block {
            note: sb.note.clone(),
            layout: sb.layout.clone(),
            components: IndexMap::new(),
        };
        for (refdes, sc) in &sb.components {
            let mut sc = sc.clone();
            apply_between(refdes, &mut sc, provider, &mut diags); // Task 7
            let comp = Component {
                part: alias(&sc.part),
                value: sc.value.clone(),
                footprint: sc.footprint.clone(),
                dnp: sc.dnp,
                props: sc.props.clone(),
                origin: Origin::Authored,
                ..Default::default()
            };
            for (pin, (target, span)) in &sc.pins {
                raw_pins.push(RawPin {
                    block: bname.clone(),
                    refdes: refdes.clone(),
                    unit: None,
                    pin: pin.clone(),
                    target: target.clone(),
                    span: *span,
                });
            }
            for (unit, pins) in &sc.units {
                for (pin, (target, span)) in pins {
                    raw_pins.push(RawPin {
                        block: bname.clone(),
                        refdes: refdes.clone(),
                        unit: Some(unit.clone()),
                        pin: pin.clone(),
                        target: target.clone(),
                        span: *span,
                    });
                }
            }
            block.components.insert(refdes.clone(), comp);
        }
        d.blocks.insert(bname.clone(), block);
    }

    resolve_pins(&mut d, raw_pins, &mut diags);
    synth_decouple(&mut d, s, &mut diags); // Task 8

    (d, diags)
}

struct RawPin {
    block: String,
    refdes: String,
    unit: Option<String>,
    pin: String,
    target: String,
    #[allow(dead_code)] // read in Task 8 (pin-ref resolution)
    span: crate::diag::Span,
}

/// Task 7 fills this in. No-op until then.
fn apply_between(
    _refdes: &str,
    _sc: &mut SurfaceComponent,
    _provider: &dyn SymbolProvider,
    _diags: &mut Diagnostics,
) {
}

/// Task 8 replaces this with pin-ref-aware resolution. For now:
/// `nc` -> NoConnect, anything else -> a net name.
fn resolve_pins(d: &mut Design, raw: Vec<RawPin>, _diags: &mut Diagnostics) {
    for rp in raw {
        let target = if rp.target.eq_ignore_ascii_case("nc") {
            PinTarget::NoConnect
        } else {
            PinTarget::Net(rp.target.clone())
        };
        write_pin(d, &rp, target);
    }
}

fn write_pin(d: &mut Design, rp: &RawPin, target: PinTarget) {
    let comp = d
        .blocks
        .get_mut(&rp.block)
        .and_then(|b| b.components.get_mut(&rp.refdes))
        .expect("raw pin refers to existing component");
    match &rp.unit {
        Some(u) => {
            comp.units
                .entry(u.clone())
                .or_default()
                .insert(rp.pin.clone(), target);
        }
        None => {
            comp.pins.insert(rp.pin.clone(), target);
        }
    }
}

/// Task 8 fills this in. No-op until then.
fn synth_decouple(_d: &mut Design, _s: &SurfaceDesign, _diags: &mut Diagnostics) {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::parse_str;
    use crate::provider::MockSymbolProvider;

    pub(crate) fn run(src: &str) -> (crate::model::Design, crate::diag::Diagnostics) {
        let (s, mut diags) = parse_str(src);
        let (d, ds) = desugar(
            &s.expect("parse failed"),
            &MockSymbolProvider::with_basics(),
        );
        diags.extend(ds);
        (d, diags)
    }

    #[test]
    fn aliases_rails_and_nc() {
        let (d, diags) = run("
version: 1
rails: [3V3, GND]
blocks:
  main:
    components:
      R1: {part: R, pins: {1: 3V3, 2: OUT}}
      U1: {part: M:X, pins: {EN: nc}}
");
        assert!(!diags.has_errors(), "{:?}", diags);
        assert!(d.nets["3V3"].power);
        assert!(d.nets["GND"].power);
        let main = &d.blocks["main"];
        assert_eq!(main.components["R1"].part, "Device:R");
        assert_eq!(main.components["U1"].part, "M:X"); // full lib_id passthrough
        assert_eq!(
            main.components["R1"].pins["1"],
            PinTarget::Net("3V3".into())
        );
        assert_eq!(main.components["U1"].pins["EN"], PinTarget::NoConnect);
    }
}
