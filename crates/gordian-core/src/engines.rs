//! The two deterministic engines the loop drives, behind one import point.
//!
//! `sch-engine` (layout + compile + checks) and `pcb-auto` (board + route + DRC)
//! land alongside this crate; the `real-sch` / `real-pcb` features switch each on
//! as it becomes usable. Without a feature the loop compiles against a stand-in
//! with the same signatures that refuses at runtime, so the prompt, the critic,
//! the CLI and the budget can be built and tested before the engine is ready.

#[cfg(feature = "real-sch")]
pub use sch_engine as sch;
#[cfg(not(feature = "real-sch"))]
pub use stub::sch;

#[cfg(feature = "real-pcb")]
pub use pcb_auto as pcb;
#[cfg(not(feature = "real-pcb"))]
pub use stub::pcb;

#[allow(dead_code)]
mod stub {
    //! Signature-compatible stand-ins for the two engines.

    pub mod sch {
        use std::collections::{BTreeMap, BTreeSet};
        use std::path::Path;

        use anyhow::{Result, bail};
        use serde_json::Value;

        pub struct SymbolHit {
            pub lib_id: String,
            pub description: String,
            pub footprint: String,
            pub pins: usize,
        }

        #[derive(Default)]
        pub struct Design {
            pub paper: String,
        }

        impl Design {
            pub fn to_json(&self, _with_ids: bool) -> Value {
                Value::Null
            }
        }

        pub mod check {
            use super::Design;
            use serde_json::Value;

            pub fn annotate_wires(_design: &mut Value, _base: Option<&Design>) {}
        }

        pub struct Library;

        impl Library {
            /// The stand-in opens: the loop, its tools and its transcript can be
            /// exercised end to end before the engine can lay anything out.
            pub fn load(_symbol_dir: &Path) -> Result<Library> {
                Ok(Library)
            }
            pub fn search(&self, _query: &str, _limit: usize) -> Vec<SymbolHit> {
                Vec::new()
            }
            pub fn info_text(&self, _lib_id: &str, _unit: Option<u32>) -> Result<String> {
                bail!("sch-engine is not linked into this build")
            }
        }

        #[derive(Default)]
        pub struct BuildReport {
            pub issues: Vec<String>,
            pub warnings: Vec<String>,
            pub notes: Vec<String>,
            pub netlist: BTreeMap<String, BTreeSet<String>>,
            pub paper: String,
            pub raw: Value,
        }

        impl BuildReport {
            pub fn netlist_text(&self, _limit: usize) -> String {
                String::new()
            }
            pub fn is_clean(&self) -> bool {
                self.issues.is_empty()
            }
        }

        pub fn build(_lib: &Library, _design: &Value, _out_sch: &Path) -> Result<BuildReport> {
            bail!("sch-engine is not linked into this build")
        }

        pub fn build_patch(
            _lib: &Library,
            _base: &Design,
            _patch: &Value,
            _out_sch: &Path,
        ) -> Result<BuildReport> {
            bail!("sch-engine is not linked into this build")
        }

        pub fn extract(_sch: &Path) -> Result<Design> {
            bail!("sch-engine is not linked into this build")
        }

        pub fn run_erc(_kicad_cli: &Path, _sch: &Path) -> Result<Vec<String>> {
            bail!("sch-engine is not linked into this build")
        }
    }

    pub mod pcb {
        use std::collections::BTreeMap;
        use std::path::Path;

        use anyhow::{Result, bail};
        use kicad::KicadInstallation;

        pub enum Outline {
            Suggest,
            Keep,
            Rect { w: f64, h: f64, radius: f64 },
        }

        pub struct AutoOptions {
            pub outline: Outline,
            pub holes: u32,
            pub layers: u32,
            pub edge_for: BTreeMap<String, String>,
            pub gnd_zone: bool,
            pub timeout_s: u64,
        }

        #[derive(Default)]
        pub struct AutoReport {
            pub ok: bool,
            pub completion: f64,
            pub unrouted: usize,
            pub drc_errors: usize,
            pub drc_warnings: usize,
            pub outline_mm: (f64, f64),
            pub notes: Vec<String>,
            pub seconds: f64,
        }

        #[derive(Default)]
        pub struct CheckReport {
            pub completion: f64,
            pub unrouted: usize,
            pub drc_errors: usize,
            pub drc_warnings: usize,
        }

        pub enum Side {
            Front,
            Back,
        }

        pub fn board_from_schematic(
            _kicad: &KicadInstallation,
            _sch: &Path,
            _out_pcb: &Path,
        ) -> Result<usize> {
            bail!("pcb-auto is not linked into this build")
        }

        pub fn auto_layout(
            _kicad: &KicadInstallation,
            _pcb: &Path,
            _opts: &AutoOptions,
        ) -> Result<AutoReport> {
            bail!("pcb-auto is not linked into this build")
        }

        pub fn check(_kicad: &KicadInstallation, _pcb: &Path) -> Result<CheckReport> {
            bail!("pcb-auto is not linked into this build")
        }

        pub fn render(
            _kicad: &KicadInstallation,
            _pcb: &Path,
            _out_png: &Path,
            _side: Side,
        ) -> Result<()> {
            bail!("pcb-auto is not linked into this build")
        }

        pub fn export_fab(
            _kicad: &KicadInstallation,
            _pcb: &Path,
            _sch: Option<&Path>,
            _out_dir: &Path,
        ) -> Result<Vec<std::path::PathBuf>> {
            bail!("pcb-auto is not linked into this build")
        }
    }
}
