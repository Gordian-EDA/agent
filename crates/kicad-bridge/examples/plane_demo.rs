use kicad_bridge::synth::plane_fill_rects;
use pcb_engine::problem::{Bounds, Point2};
use std::fmt::Write;
fn main(){
    let b = Bounds{min_x:0.0,max_x:20.0,min_y:0.0,max_y:20.0};
    // one foreign VCC via at (10,10), keepout half = via_radius(0.3)+clearance(0.2)=0.5
    let keepouts = vec![(Point2{x:10.0,y:10.0}, 0.65, 0.65)];
    let rects = plane_fill_rects(&b, 0.5, &keepouts);
    let mut o = String::new();
    o.push_str("(kicad_pcb (version 20241229) (generator \"autopcb\") (generator_version \"9.0\")\n");
    o.push_str(" (general (thickness 1.6) (legacy_teardrops no)) (paper \"A4\")\n");
    o.push_str(" (layers (0 \"F.Cu\" signal) (2 \"B.Cu\" signal) (44 \"Edge.Cuts\" user))\n");
    o.push_str(" (setup (pad_to_mask_clearance 0))\n (net 0 \"\") (net 1 \"GND\") (net 2 \"VCC\")\n");
    o.push_str(" (gr_rect (start 0 0) (end 20 20) (stroke (width 0.1) (type default)) (fill no) (layer \"Edge.Cuts\") (uuid \"10000000-0000-0000-0000-000000000001\"))\n");
    o.push_str(" (footprint \"t:p1\" (layer \"F.Cu\") (uuid \"20000000-0000-0000-0000-000000000001\") (at 3 3) (attr smd) (pad \"1\" smd rect (at 0 0) (size 1 1) (layers \"F.Cu\") (net 1 \"GND\")))\n");
    o.push_str(" (footprint \"t:p2\" (layer \"F.Cu\") (uuid \"20000000-0000-0000-0000-000000000002\") (at 17 17) (attr smd) (pad \"1\" smd rect (at 0 0) (size 1 1) (layers \"F.Cu\") (net 1 \"GND\")))\n");
    o.push_str(" (via (at 10 10) (size 0.6) (drill 0.3) (layers \"F.Cu\" \"B.Cu\") (net 2) (uuid \"40000000-0000-0000-0000-000000000001\"))\n");
    o.push_str(" (zone (net 1) (net_name \"GND\") (layer \"F.Cu\") (uuid \"30000000-0000-0000-0000-000000000001\") (hatch edge 0.5) (connect_pads (clearance 0.2)) (min_thickness 0.2) (fill yes)\n");
    o.push_str("  (polygon (pts (xy 0 0) (xy 20 0) (xy 20 20) (xy 0 20)))\n");
    for r in &rects {
        let _ = write!(o, "  (filled_polygon (layer \"F.Cu\") (pts (xy {} {}) (xy {} {}) (xy {} {}) (xy {} {})))\n",
            r[0],r[1], r[2],r[1], r[2],r[3], r[0],r[3]);
    }
    o.push_str(" )\n)\n");
    std::fs::write("/tmp/plane_demo.kicad_pcb", o).unwrap();
    println!("wrote /tmp/plane_demo.kicad_pcb with {} fill rects", rects.len());
}
