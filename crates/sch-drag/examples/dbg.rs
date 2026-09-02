use sch_doc::SchDoc;
use sch_drag::{Sheet, drag::partition};
fn main() {
    let mut a = std::env::args().skip(1);
    let (p, q) = (a.next().unwrap(), a.next().unwrap());
    let (da, db) = (SchDoc::read(&p).unwrap(), SchDoc::read(&q).unwrap());
    let (sa, sb) = (Sheet::of(&da), Sheet::of(&db));
    let (pa, pb) = (partition(&sa), partition(&sb));
    println!("ours equal: {}", pa == pb);
    for g in pa.iter().filter(|g| !pb.contains(g)).take(4) { println!("  before-only {g:?}"); }
    for g in pb.iter().filter(|g| !pa.contains(g)).take(4) { println!("  after-only  {g:?}"); }
    for pin in ["IC8.5", "IC8.4", "IC8.2"] {
        let (r, n) = pin.split_once('.').unwrap();
        for (tag, s) in [("before", &sa), ("after", &sb)] {
            if let Some(x) = s.pins.iter().find(|x| x.refdes == r && x.number == n) {
                println!("  {pin} {tag}: at {:?} net {:?} wires {} fixture {}", x.at, s.net_at(x.at),
                    s.incident.get(&sch_drag::sheet::key(x.at)).map_or(0, |v| v.len()),
                    s.fixtures.contains(&sch_drag::sheet::key(x.at)));
            }
        }
    }
}
