use std::collections::HashMap;
use std::io;

use quick_xml::Reader;
use quick_xml::events::Event;

/// A parsed `kicad-cli sch export netlist --format kicadxml` result: the
/// connectivity oracle for lifting a `.kicad_sch` back into a kernel design.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Netlist {
    /// One entry per placed component (`<comp>`), in document order.
    pub components: Vec<NetComp>,
    /// One entry per net (`<net>`), in document order. Includes single-node
    /// nets; unconnected pins do not appear.
    pub nets: Vec<Net>,
}

/// A component instance from the netlist (`<comp ref=...>`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NetComp {
    /// Schematic reference designator, e.g. `"R1"`.
    pub reference: String,
    /// Component value, e.g. `"10k"`.
    pub value: String,
    /// Library id reconstructed from `<libsource lib=".." part=".."/>` as
    /// `"lib:part"`, e.g. `"Device:R"`.
    pub lib_id: String,
    /// Field/property metadata keyed by name.
    pub properties: HashMap<String, String>,
}

/// A net and the pins it connects (`<net name=...>` with `<node>` children).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Net {
    /// Net name, e.g. `"/VIN"` or `"Net-(C1-Pad2)"`.
    pub name: String,
    /// `(reference, pin)` pairs for every pin on this net.
    pub nodes: Vec<(String, String)>,
}

fn attr(tag: &quick_xml::events::BytesStart, key: &[u8]) -> Option<String> {
    tag.attributes().flatten().find_map(|a| {
        if a.key.as_ref() == key {
            a.unescape_value().ok().map(|v| v.into_owned())
        } else {
            None
        }
    })
}

pub(crate) fn parse_netlist_xml(xml: &str) -> io::Result<Netlist> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);

    let mut netlist = Netlist::default();
    let mut in_components = false;
    let mut in_nets = false;
    let mut comp: Option<NetComp> = None;
    let mut pending_field: Option<String> = None;
    let mut net: Option<Net> = None;

    let mut buf = Vec::new();
    loop {
        let event = reader.read_event_into(&mut buf).map_err(|e| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("malformed netlist XML: {e}"),
            )
        })?;

        match event {
            Event::Start(tag) => match tag.name().as_ref() {
                b"components" => in_components = true,
                b"nets" => in_nets = true,
                b"comp" if in_components => {
                    let mut c = NetComp::default();
                    if let Some(r) = attr(&tag, b"ref") {
                        c.reference = r;
                    }
                    comp = Some(c);
                }
                b"value" if comp.is_some() => pending_field = Some("__value".into()),
                b"footprint" if comp.is_some() => pending_field = Some("Footprint".into()),
                b"field" if comp.is_some() => {
                    pending_field = attr(&tag, b"name");
                }
                b"net" if in_nets => {
                    let mut n = Net::default();
                    if let Some(name) = attr(&tag, b"name") {
                        n.name = name;
                    }
                    net = Some(n);
                }
                _ => {}
            },
            Event::Empty(tag) => match tag.name().as_ref() {
                b"libsource" if comp.is_some() => {
                    if let Some(c) = comp.as_mut() {
                        let lib = attr(&tag, b"lib").unwrap_or_default();
                        let part = attr(&tag, b"part").unwrap_or_default();
                        c.lib_id = format!("{lib}:{part}");
                    }
                }
                b"field" if comp.is_some() => {
                    if let (Some(c), Some(name)) = (comp.as_mut(), attr(&tag, b"name")) {
                        c.properties.entry(name).or_default();
                    }
                }
                b"property" if comp.is_some() => {
                    if let (Some(c), Some(name)) = (comp.as_mut(), attr(&tag, b"name")) {
                        let value = attr(&tag, b"value").unwrap_or_default();
                        c.properties.insert(name, value);
                    }
                }
                b"node" if in_nets => {
                    if let Some(n) = net.as_mut() {
                        let r = attr(&tag, b"ref").unwrap_or_default();
                        let pin = attr(&tag, b"pin").unwrap_or_default();
                        n.nodes.push((r, pin));
                    }
                }
                _ => {}
            },
            Event::Text(text) if pending_field.is_some() && comp.is_some() => {
                let value = text
                    .unescape()
                    .map_err(|e| {
                        io::Error::new(
                            io::ErrorKind::InvalidData,
                            format!("malformed netlist text: {e}"),
                        )
                    })?
                    .into_owned();
                if let (Some(c), Some(field)) = (comp.as_mut(), pending_field.as_deref()) {
                    if field == "__value" {
                        c.value = value;
                    } else {
                        c.properties.insert(field.to_string(), value);
                    }
                }
            }
            Event::End(tag) => match tag.name().as_ref() {
                b"components" => in_components = false,
                b"nets" => in_nets = false,
                b"comp" => {
                    if let Some(c) = comp.take() {
                        netlist.components.push(c);
                    }
                }
                b"value" | b"footprint" | b"field" => pending_field = None,
                b"net" => {
                    if let Some(n) = net.take() {
                        netlist.nets.push(n);
                    }
                }
                _ => {}
            },
            Event::Eof => break,
            _ => {}
        }
        buf.clear();
    }

    Ok(netlist)
}

#[cfg(test)]
mod tests {
    use super::*;

    const RC_XML: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<export version="E">
  <design><source>x</source></design>
  <components>
    <comp ref="C1">
      <value>100nF</value>
      <footprint>Capacitor_SMD:C_0603_1608Metric</footprint>
      <fields>
        <field name="Footprint">Capacitor_SMD:C_0603_1608Metric</field>
        <field name="Datasheet"/>
        <field name="Description"/>
      </fields>
      <libsource lib="Device" part="C" description="Unpolarized capacitor"/>
      <property name="Sheetname" value=""/>
      <property name="ki_keywords" value="cap capacitor"/>
      <tstamps>33333333-0000-4000-8000-000000000002</tstamps>
    </comp>
    <comp ref="R1">
      <value>10k</value>
      <footprint>Resistor_SMD:R_0603_1608Metric</footprint>
      <fields>
        <field name="Footprint">Resistor_SMD:R_0603_1608Metric</field>
        <field name="Datasheet"/>
      </fields>
      <libsource lib="Device" part="R" description="Resistor"/>
      <property name="ki_keywords" value="R res resistor"/>
    </comp>
  </components>
  <libparts>
    <libpart lib="Device" part="C">
      <fields>
        <field name="Reference">C</field>
        <field name="Value">C</field>
      </fields>
    </libpart>
  </libparts>
  <nets>
    <net code="1" name="/VIN" class="Default">
      <node ref="R1" pin="1" pintype="passive"/>
    </net>
    <net code="2" name="/VOUT" class="Default">
      <node ref="C1" pin="1" pintype="passive"/>
    </net>
    <net code="3" name="Net-(C1-Pad2)" class="Default">
      <node ref="C1" pin="2" pintype="passive"/>
      <node ref="R1" pin="2" pintype="passive"/>
    </net>
  </nets>
</export>"#;

    #[test]
    fn parses_components_with_lib_id_value_and_properties() {
        let nl = parse_netlist_xml(RC_XML).unwrap();
        assert_eq!(nl.components.len(), 2);
        assert_eq!(nl.components[0].reference, "C1");
        assert_eq!(nl.components[1].reference, "R1");

        let c = &nl.components[0];
        assert_eq!(c.value, "100nF");
        assert_eq!(c.lib_id, "Device:C");
        assert_eq!(
            c.properties.get("Footprint").map(String::as_str),
            Some("Capacitor_SMD:C_0603_1608Metric")
        );
        assert_eq!(
            c.properties.get("ki_keywords").map(String::as_str),
            Some("cap capacitor")
        );
        assert_eq!(c.properties.get("Datasheet").map(String::as_str), Some(""));

        let r = &nl.components[1];
        assert_eq!(r.value, "10k");
        assert_eq!(r.lib_id, "Device:R");
    }

    #[test]
    fn parses_nets_preserving_nodes_and_order() {
        let nl = parse_netlist_xml(RC_XML).unwrap();
        assert_eq!(nl.nets.len(), 3);

        let vin = nl.nets.iter().find(|n| n.name == "/VIN").unwrap();
        assert_eq!(vin.nodes, vec![("R1".to_string(), "1".to_string())]);

        let shared = nl.nets.iter().find(|n| n.nodes.len() == 2).unwrap();
        assert_eq!(shared.name, "Net-(C1-Pad2)");
        assert_eq!(
            shared.nodes,
            vec![
                ("C1".to_string(), "2".to_string()),
                ("R1".to_string(), "2".to_string()),
            ]
        );
    }

    #[test]
    fn blank_netlist_has_no_components_or_nets() {
        let blank = r#"<?xml version="1.0" encoding="UTF-8"?>
<export version="E">
  <design><source>x</source></design>
  <components/>
  <libparts/>
  <libraries/>
  <nets/>
</export>"#;
        let nl = parse_netlist_xml(blank).unwrap();
        assert!(nl.components.is_empty());
        assert!(nl.nets.is_empty());
    }
}
