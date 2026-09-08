//! Deterministic facts about KiCad files, for the quality harness.
//!
//! Self-contained: everything is read from the `.kicad_sch` / `.kicad_pcb`
//! itself, so measuring a case never depends on the engine that produced it.
//! The two binaries [`sch_facts`](../src/bin/sch_facts.rs) and
//! [`pcb_facts`](../src/bin/pcb_facts.rs) print the JSON `quality/run.py` reads.

pub mod geom;
pub mod net;
pub mod pcb;
pub mod sch;
pub mod sexp;
pub mod visual;
