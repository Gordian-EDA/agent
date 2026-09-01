//! Schematic placement as one neutral problem: gather parts + connectivity.
//!
//! ```text
//! SchematicPlaceProblem
//! ├── items        Vec<Item>         (search mutates positions here)
//! ├── inc          Incidence         (net → pins)
//! ├── seed         u64
//! └── options      PlaceOptions
//! ```

use std::io;

use kicad::KicadInstallation;
use sch_check::model::Design;

use sch_place::item::{Incidence, Item};
use sch_place::place::PlaceOptions;

use super::emit::{gather, incidence};
use super::refine::SEARCH_SEED;

/// One block (or whole design): gathered parts, connectivity, and search knobs.
pub struct SchematicPlaceProblem {
    pub items: Vec<Item>,
    pub inc: Incidence,
    pub seed: u64,
    pub options: PlaceOptions,
}

impl SchematicPlaceProblem {
    /// Build the neutral placement problem from a compiled design.
    pub fn from_design(env: &KicadInstallation, design: &Design) -> io::Result<Self> {
        Self::from_design_with_options(env, design, PlaceOptions::default())
    }

    /// Build the placement problem with explicit caller-owned options.
    pub fn from_design_with_options(
        env: &KicadInstallation,
        design: &Design,
        options: PlaceOptions,
    ) -> io::Result<Self> {
        let items = gather(env, design)?;
        let inc = incidence(&items);
        Ok(Self {
            items,
            inc,
            seed: SEARCH_SEED,
            options,
        })
    }
}
