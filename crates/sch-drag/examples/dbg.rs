use sch_drag::sheet::{Sheet, point_of};
fn main() {
    let doc = sch_doc::SchDoc::read(std::env::args().nth(1).unwrap()).unwrap();
    let s = Sheet::of(&doc);
    let mut n = 0;
    for (node, wires) in &s.incident {
        if wires.len() == 1
            && !s.fixtures.contains(node)
            && !s.pins_at.contains_key(node)
            && s.wires_through(point_of(node)).next().is_none()
        {
            n += 1;
            if n <= 8 {
                println!(
                    "dangling {:?} net {:?}",
                    point_of(node),
                    s.net_at(point_of(node))
                );
            }
        }
    }
    println!("dangling {n}");
    let unresolved: Vec<_> = doc
        .symbols()
        .filter(|sy| sch_doc::body_rect(&doc, sy).is_none())
        .map(|sy| sy.lib_id.clone())
        .collect();
    println!("unresolved {unresolved:?}");
}
