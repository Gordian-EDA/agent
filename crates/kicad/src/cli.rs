//! Command runner for the `kicad-cli` binary.
//!
//! This module owns subprocess invocation and KiCAD CLI exit semantics. The
//! normalized report and netlist contracts live in sibling modules.

use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::KicadInstallation;

use crate::export::{check_status, files_with_ext, with_trailing_sep};
use crate::netlist::{Netlist, parse_netlist_xml};
use crate::reports::{DrcReport, ErcReport};

impl KicadInstallation {
    /// Run `kicad-cli sch erc` on `schematic` and parse the JSON report.
    ///
    /// Returns `Ok(report)` whether or not violations were found. A nonzero
    /// "violations exist" exit code is expected and still yields a valid report.
    pub fn erc(&self, schematic: &Path) -> io::Result<ErcReport> {
        let out = tempfile::Builder::new()
            .prefix("gordian-erc-")
            .suffix(".json")
            .tempfile()?;

        let output = Command::new(self.cli_path())
            .args(["sch", "erc", "--format", "json"])
            .arg("--output")
            .arg(out.path())
            .arg("--severity-all")
            .arg("--exit-code-violations")
            .arg(schematic)
            .output()?;

        let json = std::fs::read_to_string(out.path())?;
        match serde_json::from_str::<ErcReport>(&json) {
            Ok(report) => Ok(report),
            Err(parse_err) => {
                let stderr = String::from_utf8_lossy(&output.stderr);
                let stderr = stderr.trim();
                let detail = if stderr.is_empty() {
                    format!("could not parse ERC report ({parse_err})")
                } else {
                    format!("kicad-cli sch erc failed: {stderr}")
                };
                Err(io::Error::new(io::ErrorKind::InvalidData, detail))
            }
        }
    }

    /// Run `kicad-cli pcb drc` on `pcb` and parse the JSON report.
    ///
    /// Mirrors [`erc`](Self::erc): violations are a normal report result, not a
    /// process failure, as long as the JSON report is parseable.
    pub fn drc(&self, pcb: &Path) -> io::Result<DrcReport> {
        let out = tempfile::Builder::new()
            .prefix("gordian-drc-")
            .suffix(".json")
            .tempfile()?;

        let board_has_zones = std::fs::read_to_string(pcb)
            .map(|text| text.contains("\n\t(zone") || text.contains("\n  (zone"))
            .unwrap_or(false);
        let mut cmd = Command::new(self.cli_path());
        cmd.args(["pcb", "drc", "--format", "json", "--all-track-errors"]);
        if board_has_zones && self.supports_pcb_drc_refill_zones() {
            cmd.arg("--refill-zones");
        }
        let output = cmd
            .arg("--exit-code-violations")
            .arg("--output")
            .arg(out.path())
            .arg(pcb)
            .output()?;

        let json = std::fs::read_to_string(out.path())?;
        match serde_json::from_str::<DrcReport>(&json) {
            Ok(report) => Ok(report),
            Err(parse_err) => {
                let stderr = String::from_utf8_lossy(&output.stderr);
                let stderr = stderr.trim();
                let detail = if stderr.is_empty() {
                    format!("could not parse DRC report ({parse_err})")
                } else {
                    format!("kicad-cli pcb drc failed: {stderr}")
                };
                Err(io::Error::new(io::ErrorKind::InvalidData, detail))
            }
        }
    }

    fn supports_pcb_drc_refill_zones(&self) -> bool {
        Command::new(self.cli_path())
            .args(["pcb", "drc", "--help"])
            .output()
            .ok()
            .map(|output| {
                String::from_utf8_lossy(&output.stdout).contains("--refill-zones")
                    || String::from_utf8_lossy(&output.stderr).contains("--refill-zones")
            })
            .unwrap_or(false)
    }

    /// Run `kicad-cli sch export netlist --format kicadxml` on `schematic`.
    pub fn netlist(&self, schematic: &Path) -> io::Result<Netlist> {
        let out = tempfile::Builder::new()
            .prefix("gordian-netlist-")
            .suffix(".xml")
            .tempfile()?;

        let output = Command::new(self.cli_path())
            .args(["sch", "export", "netlist", "--format", "kicadxml"])
            .arg("--output")
            .arg(out.path())
            .arg(schematic)
            .output()?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let stderr = stderr.trim();
            let detail = if stderr.is_empty() {
                "kicad-cli sch export netlist failed".to_string()
            } else {
                format!("kicad-cli sch export netlist failed: {stderr}")
            };
            return Err(io::Error::new(io::ErrorKind::InvalidData, detail));
        }

        let xml = std::fs::read_to_string(out.path())?;
        parse_netlist_xml(&xml)
    }

    /// Run `kicad-cli sch export svg` on `schematic`, writing into `out_dir`.
    pub fn export_svg(&self, schematic: &Path, out_dir: &Path) -> io::Result<PathBuf> {
        self.export_svg_opts(schematic, out_dir, false)
    }

    /// Like [`Self::export_svg`] but omits the drawing sheet when requested.
    pub fn export_svg_opts(
        &self,
        schematic: &Path,
        out_dir: &Path,
        exclude_sheet: bool,
    ) -> io::Result<PathBuf> {
        std::fs::create_dir_all(out_dir)?;
        let mut cmd = Command::new(self.cli_path());
        cmd.args(["sch", "export", "svg"])
            .arg("--output")
            .arg(out_dir);
        if exclude_sheet {
            cmd.arg("--exclude-drawing-sheet")
                .arg("--no-background-color");
        }
        let output = cmd.arg(schematic).output()?;
        check_status(&output, "kicad-cli sch export svg")?;

        let stem = schematic
            .file_stem()
            .and_then(|s| s.to_str())
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "schematic has no stem"))?;
        let svg = out_dir.join(format!("{stem}.svg"));
        if svg.is_file() {
            return Ok(svg);
        }

        let mut svgs = files_with_ext(out_dir, "svg")?;
        match svgs.drain(..).next() {
            Some(path) => Ok(path),
            None => Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("expected SVG not produced at {}", svg.display()),
            )),
        }
    }

    /// Run `kicad-cli pcb export svg` on `pcb`, writing one board-area SVG.
    pub fn export_pcb_svg(
        &self,
        pcb: &Path,
        out_file: &Path,
        layers: &str,
        mirror: bool,
    ) -> io::Result<PathBuf> {
        if let Some(parent) = out_file.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut cmd = Command::new(self.cli_path());
        cmd.args([
            "pcb",
            "export",
            "svg",
            "--mode-single",
            "--exclude-drawing-sheet",
            "--page-size-mode",
            "2",
            "--layers",
            layers,
            "--output",
        ])
        .arg(out_file);
        if mirror {
            cmd.arg("--mirror");
        }
        let output = cmd.arg(pcb).output()?;
        check_status(&output, "kicad-cli pcb export svg")?;
        if out_file.is_file() {
            Ok(out_file.to_path_buf())
        } else {
            Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("PCB SVG not produced at {}", out_file.display()),
            ))
        }
    }

    /// Run `kicad-cli pcb export gerbers` on `pcb`.
    pub fn export_gerbers(&self, pcb: &Path, out_dir: &Path) -> io::Result<Vec<PathBuf>> {
        std::fs::create_dir_all(out_dir)?;
        let output = Command::new(self.cli_path())
            .args(["pcb", "export", "gerbers", "--no-protel-ext"])
            .arg("--output")
            .arg(out_dir)
            .arg(pcb)
            .output()?;
        check_status(&output, "kicad-cli pcb export gerbers")?;
        let gerbers = files_with_ext(out_dir, "gbr")?;
        if gerbers.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("no Gerber files produced in {}", out_dir.display()),
            ));
        }
        Ok(gerbers)
    }

    /// Run `kicad-cli pcb export drill` on `pcb`.
    pub fn export_drill(&self, pcb: &Path, out_dir: &Path) -> io::Result<Vec<PathBuf>> {
        std::fs::create_dir_all(out_dir)?;
        let output = Command::new(self.cli_path())
            .args([
                "pcb",
                "export",
                "drill",
                "--format",
                "excellon",
                "--excellon-separate-th",
                "--generate-map",
            ])
            .arg("--output")
            .arg(with_trailing_sep(out_dir))
            .arg(pcb)
            .output()?;
        check_status(&output, "kicad-cli pcb export drill")?;
        let drills = files_with_ext(out_dir, "drl")?;
        if drills.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("no drill files produced in {}", out_dir.display()),
            ));
        }
        Ok(drills)
    }

    /// Run `kicad-cli pcb export pos` on `pcb`.
    pub fn export_pos(&self, pcb: &Path, out_file: &Path) -> io::Result<PathBuf> {
        if let Some(parent) = out_file.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let output = Command::new(self.cli_path())
            .args([
                "pcb", "export", "pos", "--format", "csv", "--side", "both", "--units", "mm",
            ])
            .arg("--output")
            .arg(out_file)
            .arg(pcb)
            .output()?;
        check_status(&output, "kicad-cli pcb export pos")?;
        if out_file.is_file() {
            Ok(out_file.to_path_buf())
        } else {
            Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("position file not produced at {}", out_file.display()),
            ))
        }
    }

    /// Run `kicad-cli sch export bom` on `schematic`.
    pub fn export_bom(&self, schematic: &Path, out_file: &Path) -> io::Result<PathBuf> {
        if let Some(parent) = out_file.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let output = Command::new(self.cli_path())
            .args([
                "sch",
                "export",
                "bom",
                "--group-by",
                "Value,Footprint",
                "--exclude-dnp",
            ])
            .arg("--output")
            .arg(out_file)
            .arg(schematic)
            .output()?;
        check_status(&output, "kicad-cli sch export bom")?;
        if out_file.is_file() {
            Ok(out_file.to_path_buf())
        } else {
            Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("BOM not produced at {}", out_file.display()),
            ))
        }
    }
}
