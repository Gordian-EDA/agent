// Prove the full 4-layer power-plane scheme: F.Cu power pads stitched to inner
// planes (GND=In1, VCC=In2) via through-vias, a signal routed F<->B with a via,
// and both planes carving anti-pads around foreign vias. Target: 0 DRC, all connected.
use kicad_bridge::synth::plane_fill_rects;
use pcb_engine::problem::{Bounds, Point2};
use std::fmt::Write;

fn zone(o:&mut String, net:i32, name:&str, layer:&str, uuid:&str, rects:&[[f64;4]]){
    let _=writeln!(o," (zone (net {net}) (net_name \"{name}\") (layer \"{layer}\") (uuid \"{uuid}\") (hatch edge 0.5) (connect_pads (clearance 0.2)) (min_thickness 0.2) (fill yes)");
    o.push_str("  (polygon (pts (xy 0 0) (xy 20 0) (xy 20 20) (xy 0 20)))\n");
    for r in rects { let _=writeln!(o,"  (filled_polygon (layer \"{layer}\") (pts (xy {} {}) (xy {} {}) (xy {} {}) (xy {} {})))",r[0],r[1],r[2],r[1],r[2],r[3],r[0],r[3]); }
    o.push_str(" )\n");
}
fn smd(o:&mut String, uuid:&str, x:f64,y:f64,net:i32,name:&str){
    let _=writeln!(o," (footprint \"t:p\" (layer \"F.Cu\") (uuid \"{uuid}\") (at {x} {y}) (attr smd) (pad \"1\" smd rect (at 0 0) (size 1 1) (layers \"F.Cu\") (net {net} \"{name}\")))");
}
fn via(o:&mut String, uuid:&str, x:f64,y:f64,net:i32){
    let _=writeln!(o," (via (at {x} {y}) (size 0.6) (drill 0.3) (layers \"F.Cu\" \"B.Cu\") (net {net}) (uuid \"{uuid}\"))");
}
fn main(){
    let b=Bounds{min_x:0.0,max_x:20.0,min_y:0.0,max_y:20.0};
    let h=0.65; // via_radius+clearance+safety
    let gnd_vias=[(5.0,5.0),(5.0,15.0)];
    let vcc_vias=[(15.0,5.0),(15.0,15.0)];
    let sig_via=(10.0,10.0); let sig_via2=(10.0,17.0);
    // In1 GND plane: anti-pad around foreign vias (VCC + SIG), NOT GND.
    let in1_ko:Vec<(Point2,f64,f64)>=vcc_vias.iter().chain([sig_via,sig_via2].iter()).map(|&(x,y)|(Point2{x,y},h,h)).collect();
    // In2 VCC plane: anti-pad around foreign vias (GND + SIG), NOT VCC.
    let in2_ko:Vec<(Point2,f64,f64)>=gnd_vias.iter().chain([sig_via,sig_via2].iter()).map(|&(x,y)|(Point2{x,y},h,h)).collect();
    let in1=plane_fill_rects(&b,0.5,&in1_ko, None);
    let in2=plane_fill_rects(&b,0.5,&in2_ko, None);
    let mut o=String::new();
    o.push_str("(kicad_pcb (version 20241229) (generator \"autopcb\") (generator_version \"9.0\")\n (general (thickness 1.6) (legacy_teardrops no)) (paper \"A4\")\n (layers (0 \"F.Cu\" signal) (1 \"In1.Cu\" signal) (2 \"In2.Cu\" signal) (3 \"B.Cu\" signal) (44 \"Edge.Cuts\" user))\n (setup (pad_to_mask_clearance 0))\n (net 0 \"\") (net 1 \"GND\") (net 2 \"VCC\") (net 3 \"SIG\")\n");
    o.push_str(" (gr_rect (start 0 0) (end 20 20) (stroke (width 0.1) (type default)) (fill no) (layer \"Edge.Cuts\") (uuid \"10000000-0000-0000-0000-000000000099\"))\n");
    smd(&mut o,"20000000-0000-0000-0000-000000000001",5.0,5.0,1,"GND");
    smd(&mut o,"20000000-0000-0000-0000-000000000002",5.0,15.0,1,"GND");
    smd(&mut o,"20000000-0000-0000-0000-000000000003",15.0,5.0,2,"VCC");
    smd(&mut o,"20000000-0000-0000-0000-000000000004",15.0,15.0,2,"VCC");
    smd(&mut o,"20000000-0000-0000-0000-000000000005",10.0,3.0,3,"SIG");
    smd(&mut o,"20000000-0000-0000-0000-000000000006",10.0,17.0,3,"SIG");
    via(&mut o,"40000000-0000-0000-0000-000000000001",5.0,5.0,1);
    via(&mut o,"40000000-0000-0000-0000-000000000002",5.0,15.0,1);
    via(&mut o,"40000000-0000-0000-0000-000000000003",15.0,5.0,2);
    via(&mut o,"40000000-0000-0000-0000-000000000004",15.0,15.0,2);
    via(&mut o,"40000000-0000-0000-0000-000000000005",10.0,10.0,3);
    via(&mut o,"40000000-0000-0000-0000-000000000006",10.0,17.0,3);
    // SIG trace F.Cu 3->10, B.Cu 10->17
    o.push_str(" (segment (start 10 3) (end 10 10) (width 0.2) (layer \"F.Cu\") (net 3) (uuid \"50000000-0000-0000-0000-000000000001\"))\n");
    o.push_str(" (segment (start 10 10) (end 10 17) (width 0.2) (layer \"B.Cu\") (net 3) (uuid \"50000000-0000-0000-0000-000000000002\"))\n");
    zone(&mut o,1,"GND","In1.Cu","30000000-0000-0000-0000-000000000001",&in1);
    zone(&mut o,2,"VCC","In2.Cu","30000000-0000-0000-0000-000000000002",&in2);
    o.push_str(")\n");
    std::fs::write("/tmp/plane4_demo.kicad_pcb",o).unwrap();
    println!("wrote /tmp/plane4_demo.kicad_pcb (in1 {} rects, in2 {} rects)",in1.len(),in2.len());
}
