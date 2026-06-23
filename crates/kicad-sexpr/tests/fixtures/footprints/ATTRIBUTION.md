# Footprint fixtures — origin & license

The `.kicad_mod` files in this directory are verbatim copies of footprints
shipped with KiCAD 10.0.3 (`/usr/share/kicad/footprints/*.pretty/`). They are
vendored here so `footlib.rs` parsing tests run **without** a KiCAD install.

| File | Source library (`.pretty`) |
| --- | --- |
| `R_0603_1608Metric.kicad_mod` | `Resistor_SMD.pretty` |
| `SOT-23.kicad_mod` | `Package_TO_SOT_SMD.pretty` |
| `PinHeader_1x02_P2.54mm_Vertical.kicad_mod` | `Connector_PinHeader_2.54mm.pretty` |

The KiCAD footprint libraries are licensed **CC-BY-SA 4.0 with an exception**
that explicitly permits use on boards (and, by extension, copying individual
footprints as test fixtures) without the board/derivative inheriting the
license. See <https://www.kicad.org/libraries/license/>. These copies are
unmodified; attribution is provided above.
